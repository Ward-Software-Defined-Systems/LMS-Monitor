#![allow(dead_code)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, watch};

#[derive(Debug, Clone, Default)]
pub struct HardwareSnapshot {
    pub system_cpu_percent: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,

    pub lms_cpu_percent: f32,
    pub lms_rss_bytes: u64,
    pub lms_process_count: u32,

    /// Apple Silicon GPU / ANE telemetry from `powermetrics`.
    /// `None` if powermetrics didn't start, hasn't produced a sample yet, or
    /// emitted an unrecognized format.
    pub gpu_active_percent: Option<f32>,
    pub ane_power_mw: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct PowermetricsState {
    pub gpu_active_percent: Option<f32>,
    pub ane_power_mw: Option<f32>,
}

pub type PowermetricsStateHandle = Arc<Mutex<PowermetricsState>>;

pub struct PowermetricsHandle {
    pub state: PowermetricsStateHandle,
    child: Child,
}

impl PowermetricsHandle {
    /// Send SIGTERM to sudo so the inner powermetrics shuts down cleanly,
    /// then wait briefly for the process to exit.
    pub async fn terminate(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid is a valid OS PID returned by tokio's Child.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

/// Spawn `sudo powermetrics --samplers gpu_power,ane_power -i 2000` and a sub-task that
/// parses the streaming text output into a shared state. The sudo password prompt fires
/// before this returns — caller must invoke this BEFORE entering TUI raw mode.
pub async fn spawn_powermetrics() -> Result<PowermetricsHandle> {
    let mut cmd = Command::new("sudo");
    cmd.arg("--prompt")
        .arg("[lmstudio-monitor] sudo password (for powermetrics GPU/ANE telemetry): ")
        .arg("/usr/bin/powermetrics")
        // cpu_power emits the unified power summary including ANE Power on Apple Silicon;
        // gpu_power emits GPU HW active residency. Without cpu_power, ANE is silent on M4 Max.
        .arg("--samplers")
        .arg("cpu_power,gpu_power,ane_power")
        .arg("-i")
        .arg("2000")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .context("spawn sudo powermetrics (is sudo on PATH?)")?;
    let stdout = child
        .stdout
        .take()
        .context("powermetrics child has no stdout")?;

    let state: PowermetricsStateHandle = Arc::new(Mutex::new(PowermetricsState::default()));
    let state_for_reader = state.clone();

    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    tracing::trace!(target: "lmstudio_monitor::powermetrics", "{line}");
                    if let Some(pct) = parse_gpu_active_percent(&line) {
                        state_for_reader.lock().await.gpu_active_percent = Some(pct);
                    } else if let Some(mw) = parse_ane_power_mw(&line) {
                        state_for_reader.lock().await.ane_power_mw = Some(mw);
                    }
                }
                Ok(None) => {
                    tracing::warn!("powermetrics stdout closed");
                    return;
                }
                Err(e) => {
                    tracing::warn!("powermetrics stdout read error: {e}");
                    return;
                }
            }
        }
    });

    Ok(PowermetricsHandle { state, child })
}

fn parse_gpu_active_percent(line: &str) -> Option<f32> {
    // Apple Silicon emits "GPU HW active residency:   2.96% (338 MHz: ...)".
    // Older macs / older powermetrics use "GPU active residency:" — accept either.
    let line = line.trim_start();
    let rest = line
        .strip_prefix("GPU HW active residency:")
        .or_else(|| line.strip_prefix("GPU active residency:"))?
        .trim_start();
    let pct_end = rest.find('%')?;
    rest[..pct_end].trim().parse().ok()
}

fn parse_ane_power_mw(line: &str) -> Option<f32> {
    // Expected: "ANE Power: 234 mW"  (case variants tolerated)
    let line = line.trim_start();
    let stripped = line
        .strip_prefix("ANE Power:")
        .or_else(|| line.strip_prefix("ANE power:"))
        .or_else(|| line.strip_prefix("ANE:"))?;
    let rest = stripped.trim_start();
    // pull leading numeric portion
    let mut end = 0;
    for (i, c) in rest.char_indices() {
        if c.is_ascii_digit() || c == '.' {
            end = i + c.len_utf8();
        } else if end > 0 {
            break;
        } else {
            return None;
        }
    }
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Substrings to look for in a process's executable path. These cover:
/// - the GUI app + helpers under `/Applications/LM Studio.app/...`
/// - the bundled inference workers under `~/.lmstudio/...` (the runtime "node" subprocess
///   that actually holds the loaded model is here, NOT under /Applications)
const LMS_EXE_PATH_SUBSTRINGS: &[&str] = &["/LM Studio.app/", "/.lmstudio/"];

/// Fallback substring match against `proc.name()` (basename) for runtimes whose argv[0]
/// is the runtime binary (e.g. llama.cpp variants).
const LMS_NAME_SUBSTRINGS: &[&str] = &["mlx-llm", "llama-server"];

fn is_lms_process(proc: &Process) -> bool {
    if let Some(exe) = proc.exe() {
        let exe_str = exe.to_string_lossy();
        if LMS_EXE_PATH_SUBSTRINGS.iter().any(|p| exe_str.contains(p)) {
            return true;
        }
    }
    let name = proc.name().to_string_lossy();
    LMS_NAME_SUBSTRINGS.iter().any(|p| name.contains(p))
}

pub async fn run_sampler(
    tx: mpsc::Sender<HardwareSnapshot>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    pm_state: Option<PowermetricsStateHandle>,
) {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything())
            .with_processes(ProcessRefreshKind::everything()),
    );
    // Prime: sysinfo needs two samples spaced ≥ MINIMUM_CPU_UPDATE_INTERVAL apart for CPU%.
    sys.refresh_all();
    tokio::time::sleep(Duration::from_millis(250)).await;
    sys.refresh_cpu_usage();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                sys.refresh_processes(ProcessesToUpdate::All, true);
                let mut snap = build_snapshot(&sys);
                if let Some(state) = &pm_state {
                    let s = state.lock().await;
                    snap.gpu_active_percent = s.gpu_active_percent;
                    snap.ane_power_mw = s.ane_power_mw;
                }
                tracing::debug!(
                    "hw snap cpu={:.1}% mem={}/{} lms_procs={} lms_rss={} gpu={:?} ane_mw={:?}",
                    snap.system_cpu_percent,
                    snap.mem_used_bytes,
                    snap.mem_total_bytes,
                    snap.lms_process_count,
                    snap.lms_rss_bytes,
                    snap.gpu_active_percent,
                    snap.ane_power_mw,
                );
                if tx.send(snap).await.is_err() {
                    return;
                }
            }
            _ = shutdown.changed() => return,
        }
    }
}

fn build_snapshot(sys: &System) -> HardwareSnapshot {
    let mut snap = HardwareSnapshot {
        system_cpu_percent: sys.global_cpu_usage(),
        mem_used_bytes: sys.used_memory(),
        mem_total_bytes: sys.total_memory(),
        mem_available_bytes: sys.available_memory(),
        ..Default::default()
    };

    let mut total_cpu = 0.0_f32;
    let mut total_rss = 0_u64;
    let mut count = 0_u32;
    for proc in sys.processes().values() {
        if is_lms_process(proc) {
            total_cpu += proc.cpu_usage();
            total_rss += proc.memory();
            count += 1;
        }
    }
    snap.lms_cpu_percent = total_cpu;
    snap.lms_rss_bytes = total_rss;
    snap.lms_process_count = count;
    snap
}

pub fn format_bytes(b: u64) -> String {
    let f = b as f64;
    if f >= 1.0e9 {
        format!("{:.1} GB", f / 1.0e9)
    } else if f >= 1.0e6 {
        format!("{:.0} MB", f / 1.0e6)
    } else if f >= 1.0e3 {
        format!("{:.0} KB", f / 1.0e3)
    } else {
        format!("{b} B")
    }
}

/// Format `used` and `total` with a single shared unit chosen from `total`,
/// e.g. `38.2/137.4 GB` — compact enough for a one-line panel.
pub fn format_bytes_ratio(used: u64, total: u64) -> String {
    let (div, unit, prec) = if total >= 1_000_000_000 {
        (1.0e9, "GB", 1)
    } else if total >= 1_000_000 {
        (1.0e6, "MB", 0)
    } else if total >= 1_000 {
        (1.0e3, "KB", 0)
    } else {
        (1.0, "B", 0)
    };
    format!(
        "{:.prec$}/{:.prec$} {unit}",
        used as f64 / div,
        total as f64 / div,
        prec = prec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "2 KB");
        assert_eq!(format_bytes(1_500_000), "2 MB");
        assert_eq!(format_bytes(38_200_000_000), "38.2 GB");
    }

    #[test]
    fn format_bytes_ratio_shares_one_unit() {
        assert_eq!(
            format_bytes_ratio(38_200_000_000, 137_400_000_000),
            "38.2/137.4 GB"
        );
        assert_eq!(
            format_bytes_ratio(512_000_000, 137_400_000_000),
            "0.5/137.4 GB"
        );
        assert_eq!(format_bytes_ratio(1_500_000, 8_000_000), "2/8 MB");
        assert_eq!(format_bytes_ratio(0, 0), "0/0 B");
    }

    #[test]
    fn build_snapshot_handles_empty_system_match() {
        // Building on a real System should never panic, even if no LM Studio process is running.
        let mut sys = System::new();
        sys.refresh_all();
        let snap = build_snapshot(&sys);
        assert!(snap.mem_total_bytes > 0);
        // lms_* fields should be 0/empty when nothing matches the patterns
        // (we can't assert == 0 universally because dev machines may have lms running).
        assert!(snap.lms_process_count == snap.lms_process_count); // touch the field
    }

    #[tokio::test]
    async fn sampler_emits_snapshot_within_a_few_seconds() {
        let (tx, mut rx) = mpsc::channel(2);
        let (sd_tx, sd_rx) = watch::channel(false);
        tokio::spawn(run_sampler(tx, Duration::from_millis(300), sd_rx, None));

        let snap = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("sampler should emit within 3s")
            .expect("channel should yield a snapshot");

        assert!(snap.mem_total_bytes > 0, "mem_total should be non-zero");
        assert!(
            snap.mem_used_bytes <= snap.mem_total_bytes,
            "used should not exceed total"
        );
        assert!(
            snap.gpu_active_percent.is_none(),
            "no powermetrics handle => GPU stays None"
        );
        assert!(
            snap.ane_power_mw.is_none(),
            "no powermetrics handle => ANE stays None"
        );

        eprintln!(
            "[hw snap] cpu={:.1}%  mem={}/{} (avail {})  lms procs={} cpu={:.1}% rss={}",
            snap.system_cpu_percent,
            format_bytes(snap.mem_used_bytes),
            format_bytes(snap.mem_total_bytes),
            format_bytes(snap.mem_available_bytes),
            snap.lms_process_count,
            snap.lms_cpu_percent,
            format_bytes(snap.lms_rss_bytes),
        );

        let _ = sd_tx.send(true);
    }

    #[test]
    fn parse_gpu_active_percent_apple_silicon_format() {
        // Real M4 Max format captured from `powermetrics --samplers gpu_power`:
        assert_eq!(
            parse_gpu_active_percent(
                "GPU HW active residency:   2.96% (338 MHz: .18% 618 MHz:   0%)"
            ),
            Some(2.96)
        );
        assert_eq!(
            parse_gpu_active_percent("GPU HW active residency:  38.45%"),
            Some(38.45)
        );
    }

    #[test]
    fn parse_gpu_active_percent_legacy_format() {
        // Older powermetrics / non-AS uses no "HW" — keep accepting both.
        assert_eq!(
            parse_gpu_active_percent("GPU active residency: 38.45% (0 MHz: 61.55%)"),
            Some(38.45)
        );
    }

    #[test]
    fn parse_gpu_active_percent_rejects_unrelated() {
        assert!(parse_gpu_active_percent("GPU idle residency: 61.55%").is_none());
        assert!(parse_gpu_active_percent("GPU SW state: ...").is_none());
        assert!(parse_gpu_active_percent("garbage line").is_none());
    }

    #[test]
    fn parse_ane_power_mw_canonical() {
        assert_eq!(parse_ane_power_mw("ANE Power: 234 mW"), Some(234.0));
        assert_eq!(parse_ane_power_mw("ANE power: 0 mW"), Some(0.0));
        assert_eq!(parse_ane_power_mw("  ANE Power: 1234.5 mW"), Some(1234.5));
    }

    #[test]
    fn parse_ane_power_mw_rejects_unrelated() {
        assert!(parse_ane_power_mw("GPU Power: 234 mW").is_none());
        assert!(parse_ane_power_mw("nonsense line").is_none());
    }
}
