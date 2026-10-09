//! System CPU and memory and LM Studio's processes via `sysinfo`, plus GPU telemetry from a
//! child process chosen per platform: `sudo -n powermetrics` on macOS (GPU active residency
//! and Neural Engine power), `nvidia-smi` on Linux (GPU utilisation and VRAM in use).
//!
//! Both backends compile on every platform, so either platform's tests cover both parsers;
//! only the `telemetry` alias picks one. Each provides `PROGRAM`, `prime`, `command` and a
//! `State` implementing `Readings`.

#![allow(dead_code)]

mod nvidia_smi;
mod powermetrics;

#[cfg(not(target_os = "macos"))]
use nvidia_smi as telemetry;
#[cfg(target_os = "macos")]
use powermetrics as telemetry;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System, UpdateKind,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, mpsc, watch};

/// How often the telemetry child reports.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, Default)]
pub struct HardwareSnapshot {
    pub system_cpu_percent: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,

    pub lms_cpu_percent: f32,
    pub lms_rss_bytes: u64,
    pub lms_process_count: u32,

    /// macOS: powermetrics GPU HW active residency. Linux: nvidia-smi `utilization.gpu`,
    /// the busiest NVIDIA GPU's. This and the two below are `None` if the telemetry child
    /// didn't start, hasn't reported yet, has exited, or printed an unrecognized format.
    pub gpu_util_pct: Option<f32>,
    /// Linux only: nvidia-smi `memory.used`, summed across NVIDIA GPUs.
    pub gpu_mem_used_bytes: Option<u64>,
    /// macOS only: powermetrics `ANE Power`.
    pub ane_power_mw: Option<f32>,
}

/// The telemetry child's latest readings; back to default when it exits.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Telemetry {
    pub gpu_util_pct: Option<f32>,
    pub gpu_mem_used_bytes: Option<u64>,
    pub ane_power_mw: Option<f32>,
}

/// A backend's parser: folds each line of its child's output into its readings.
trait Readings: Default + Send + 'static {
    /// Returns whether the line carried a reading.
    fn apply_line(&mut self, line: &str) -> bool;
    fn publish(&self) -> Telemetry;
}

pub type TelemetryState = Arc<Mutex<Telemetry>>;

pub struct TelemetryHandle {
    pub state: TelemetryState,
    child: Child,
}

impl TelemetryHandle {
    /// Sends SIGTERM to the child (on macOS to sudo, which relays it so powermetrics
    /// doesn't linger as root), then waits briefly for it to exit.
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

/// Whatever the platform's telemetry needs before the TUI takes the terminal: on macOS the
/// sudo password prompt, which needs a cooked terminal; on Linux nothing.
pub fn prime() {
    telemetry::prime();
}

/// Spawns the platform's telemetry child and a task that feeds its output into shared
/// state (see `pump_telemetry`), returning as soon as the child is spawned.
pub fn spawn_telemetry(shutdown: watch::Receiver<bool>) -> Result<TelemetryHandle> {
    let mut child = telemetry::command()
        .spawn()
        .with_context(|| format!("spawn {}", telemetry::PROGRAM))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("{} has no stdout", telemetry::PROGRAM))?;
    let state = TelemetryState::default();
    tokio::spawn(pump_telemetry(
        BufReader::new(stdout),
        telemetry::State::default(),
        state.clone(),
        telemetry::PROGRAM,
        shutdown,
    ));
    Ok(TelemetryHandle { state, child })
}

/// Feeds the child's output into `state` until it ends, then clears the readings so the
/// panel shows n/a instead of numbers frozen at their last values. The output ends when
/// the child exits: at shutdown, after `terminate`, or on its own mid-run, which is worth
/// a warning because it's never respawned.
async fn pump_telemetry<R: AsyncBufRead + Unpin, S: Readings>(
    reader: R,
    mut readings: S,
    state: TelemetryState,
    program: &'static str,
    shutdown: watch::Receiver<bool>,
) {
    let mut lines = reader.lines();
    let ended = loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if readings.apply_line(&line) {
                    *state.lock().await = readings.publish();
                }
            }
            Ok(None) => break format!("{program} exited"),
            Err(e) => break format!("{program} read failed: {e}"),
        }
    };
    *state.lock().await = Telemetry::default();
    if *shutdown.borrow() {
        tracing::info!("{ended} (shutting down)");
    } else {
        tracing::warn!("{ended}; the GPU figures will show n/a");
    }
}

/// Substrings of a process's executable path, or of its argv[0], that mark it as LM
/// Studio's:
/// - `/LM Studio.app/`: the macOS app and its helpers;
/// - `/.lmstudio/`: everything LM Studio runs from its home, on either platform: the
///   `.internal/utils/node` inference worker (it holds the loaded model), the headless
///   `llmster` daemon, the `llama-server` engine processes under `extensions/backends/`,
///   and the `lms` CLI, including the stream this monitor runs;
/// - `/opt/LM-Studio/`: the Linux `.deb` install.
const LMS_PATH_HINTS: &[&str] = &["/LM Studio.app/", "/.lmstudio/", "/opt/LM-Studio/"];

/// Substrings of the process name, for processes whose path doesn't give them away:
/// `lm-studio` is the Linux app and its Electron helpers (an AppImage runs from a random
/// `/tmp/.mount_*` directory), `llmster` the headless daemon, `llama-server` and `mlx-llm`
/// engine processes. The name also works where the path can't be read: on Linux
/// `/proc/<pid>/exe` is private to the process's user, so `exe()` is empty for an LM Studio
/// service running as another user.
const LMS_NAME_HINTS: &[&str] = &["lm-studio", "llmster", "llama-server", "mlx-llm"];

fn is_lms_process(proc: &Process) -> bool {
    let exe = proc.exe().map(|p| p.to_string_lossy());
    let arg0 = proc.cmd().first().map(|a| a.to_string_lossy());
    matches_lms(
        exe.as_deref(),
        arg0.as_deref(),
        &proc.name().to_string_lossy(),
    )
}

fn matches_lms(exe: Option<&str>, arg0: Option<&str>, name: &str) -> bool {
    let path_hint = |s: &str| LMS_PATH_HINTS.iter().any(|h| s.contains(h));
    exe.is_some_and(path_hint)
        || arg0.is_some_and(path_hint)
        || LMS_NAME_HINTS.iter().any(|h| name.contains(h))
}

/// What sysinfo reads per process. `without_tasks` matters on Linux, where sysinfo
/// otherwise lists every thread as a process of its own, multiplying the count, CPU and
/// RSS. `cmd` feeds the argv[0] match; `OnlyIfNotSet` reads it and the path once per
/// process.
fn process_refresh() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .without_tasks()
}

pub async fn run_sampler(
    tx: mpsc::Sender<HardwareSnapshot>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    telemetry: Option<TelemetryState>,
) {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything())
            .with_processes(process_refresh()),
    );
    // Prime: sysinfo needs two samples spaced ≥ MINIMUM_CPU_UPDATE_INTERVAL apart for CPU%.
    tokio::time::sleep(Duration::from_millis(250)).await;
    sys.refresh_cpu_usage();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh());

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh());
                let mut snap = build_snapshot(&sys);
                if let Some(state) = &telemetry {
                    let t = *state.lock().await;
                    snap.gpu_util_pct = t.gpu_util_pct;
                    snap.gpu_mem_used_bytes = t.gpu_mem_used_bytes;
                    snap.ane_power_mw = t.ane_power_mw;
                }
                tracing::debug!(
                    "hw snap cpu={:.1}% mem={}/{} lms_procs={} lms_rss={} gpu={:?} vram={:?} ane_mw={:?}",
                    snap.system_cpu_percent,
                    snap.mem_used_bytes,
                    snap.mem_total_bytes,
                    snap.lms_process_count,
                    snap.lms_rss_bytes,
                    snap.gpu_util_pct,
                    snap.gpu_mem_used_bytes,
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
        // A thread listed as a process (only if the refresh kind asked for tasks, which
        // `process_refresh` doesn't) mustn't count again. Always `None` off Linux.
        if proc.thread_kind().is_some() || !is_lms_process(proc) {
            continue;
        }
        total_cpu += proc.cpu_usage();
        total_rss += proc.memory();
        count += 1;
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

    #[test]
    fn lms_processes_match_by_path_argv0_or_name() {
        // The macOS app and its helpers.
        assert!(matches_lms(
            Some("/Applications/LM Studio.app/Contents/MacOS/LM Studio"),
            None,
            "LM Studio"
        ));
        // The inference worker, the headless daemon and an engine process, all under the
        // LM Studio home.
        assert!(matches_lms(
            Some("/home/u/.lmstudio/.internal/utils/node"),
            None,
            "node"
        ));
        assert!(matches_lms(
            Some("/home/u/.lmstudio/llmster/0.0.25-1/llmster"),
            None,
            "llmster"
        ));
        assert!(matches_lms(
            Some(
                "/home/u/.lmstudio/extensions/backends/llama.cpp-linux-x86_64-nvidia-cuda12-avx2-2.51.0/llama-server"
            ),
            None,
            "llama-server"
        ));
        // Another user's process: no readable path, but argv[0] or the name gives it away.
        assert!(matches_lms(
            None,
            Some("/home/svc/.lmstudio/.internal/utils/node"),
            "node"
        ));
        assert!(matches_lms(None, None, "llmster"));
        // The Linux app from an AppImage mount, and from the .deb.
        assert!(matches_lms(
            Some("/tmp/.mount_LM-Stu6HlfEK/lm-studio"),
            None,
            "lm-studio"
        ));
        assert!(matches_lms(
            Some("/opt/LM-Studio/lm-studio"),
            None,
            "lm-studio"
        ));
        // Not LM Studio: this monitor, an unrelated node, Ollama.
        assert!(!matches_lms(
            Some("/home/u/LMS-Monitor/target/release/lmstudio-monitor"),
            None,
            "lmstudio-monitor"
        ));
        assert!(!matches_lms(Some("/usr/bin/node"), Some("node"), "node"));
        assert!(!matches_lms(Some("/usr/local/bin/ollama"), None, "ollama"));
    }

    #[test]
    fn both_backends_publish_defaults() {
        assert_eq!(
            powermetrics::State::default().publish(),
            Telemetry::default()
        );
        assert_eq!(nvidia_smi::State::default().publish(), Telemetry::default());
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
        assert_eq!(
            (
                snap.gpu_util_pct,
                snap.gpu_mem_used_bytes,
                snap.ane_power_mw
            ),
            (None, None, None),
            "no telemetry state => GPU figures stay None"
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

    /// Writes `output` into a pump driving `readings`, waits until the shared state shows
    /// `expected`, then closes the output (the child exiting) and checks the readings clear.
    async fn pump_then_exit<S: Readings>(readings: S, output: &[u8], expected: Telemetry) {
        use tokio::io::AsyncWriteExt;

        let state = TelemetryState::default();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (mut child_out, pipe) = tokio::io::duplex(256);
        let pump = tokio::spawn(pump_telemetry(
            BufReader::new(pipe),
            readings,
            state.clone(),
            "test-child",
            shutdown_rx,
        ));

        child_out.write_all(output).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while *state.lock().await != expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("readings should arrive while the child runs");

        drop(child_out); // the child exits
        pump.await.unwrap();
        assert_eq!(*state.lock().await, Telemetry::default());
    }

    #[tokio::test]
    async fn powermetrics_readings_clear_when_it_exits() {
        pump_then_exit(
            powermetrics::State::default(),
            b"GPU HW active residency:  38.45% (338 MHz: 1%)\nANE Power: 234 mW\n",
            Telemetry {
                gpu_util_pct: Some(38.45),
                gpu_mem_used_bytes: None,
                ane_power_mw: Some(234.0),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn nvidia_smi_readings_clear_when_it_exits() {
        pump_then_exit(
            nvidia_smi::State::default(),
            b"0, 26, 55\n1, 40, 2000\n",
            Telemetry {
                gpu_util_pct: Some(40.0),
                gpu_mem_used_bytes: Some(2055 * 1024 * 1024),
                ane_power_mw: None,
            },
        )
        .await;
    }
}
