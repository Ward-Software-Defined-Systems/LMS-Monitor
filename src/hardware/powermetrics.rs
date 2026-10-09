//! macOS telemetry backend: `sudo -n powermetrics`, parsed for GPU active residency and
//! Neural Engine power. Needs root, so `prime` takes the sudo password before the TUI owns
//! the terminal.

use std::process::Stdio;

use tokio::process::Command;

use super::{Readings, SAMPLE_INTERVAL, Telemetry};

pub(super) const PROGRAM: &str = "powermetrics";

const SUDO_PROMPT: &str = "[lmstudio-monitor] sudo password (for powermetrics GPU/ANE telemetry): ";

/// Runs `sudo -v` in the foreground so a password prompt reads from a normal cooked
/// terminal, caching the credential that `command`'s `sudo -n` relies on. Call it before
/// the TUI takes the terminal. Failure isn't fatal: GPU/ANE just end up n/a.
pub(super) fn prime() {
    eprintln!(
        "lmstudio-monitor: sudo runs powermetrics for the GPU/ANE figures (--no-tui skips it)"
    );
    if !prime_sudo() {
        eprintln!("sudo -v failed; GPU/ANE may show n/a");
    }
}

fn prime_sudo() -> bool {
    match std::process::Command::new("sudo")
        .args(["-v", "--prompt", SUDO_PROMPT])
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!("sudo -v exited with {status}; GPU/ANE may show n/a");
            false
        }
        Err(e) => {
            tracing::warn!("sudo -v could not run: {e}; GPU/ANE may show n/a");
            false
        }
    }
}

/// `sudo -n /usr/bin/powermetrics …`. `-n` never prompts: it uses the credential `prime`
/// cached (or a NOPASSWD rule) and otherwise fails at once, leaving GPU/ANE at n/a.
///
/// The child gets its own process group. With `use_pty` (on by default since sudo
/// 1.9.14), a sudo in the terminal's foreground group may read keystrokes to relay to
/// its command; in a background group it never reads from or reconfigures the TUI's
/// terminal. That is only safe because `-n` keeps it from prompting.
pub(super) fn command() -> Command {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n")
        .arg("/usr/bin/powermetrics")
        // cpu_power emits the unified power summary including ANE Power on Apple Silicon;
        // gpu_power emits GPU HW active residency. Without cpu_power, ANE is silent on M4 Max.
        .arg("--samplers")
        .arg("cpu_power,gpu_power,ane_power")
        .arg("-i")
        .arg(SAMPLE_INTERVAL.as_millis().to_string())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd
}

#[derive(Debug, Default)]
pub(super) struct State {
    gpu_util_pct: Option<f32>,
    ane_power_mw: Option<f32>,
}

impl Readings for State {
    fn apply_line(&mut self, line: &str) -> bool {
        tracing::trace!(target: "lmstudio_monitor::powermetrics", "{line}");
        if let Some(pct) = parse_gpu_active_percent(line) {
            self.gpu_util_pct = Some(pct);
            true
        } else if let Some(mw) = parse_ane_power_mw(line) {
            self.ane_power_mw = Some(mw);
            true
        } else {
            false
        }
    }

    fn publish(&self) -> Telemetry {
        Telemetry {
            gpu_util_pct: self.gpu_util_pct,
            gpu_mem_used_bytes: None,
            ane_power_mw: self.ane_power_mw,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_applies_gpu_and_ane_lines() {
        let mut state = State::default();
        assert!(state.apply_line("GPU HW active residency:  38.45% (338 MHz: 1%)"));
        assert!(state.apply_line("ANE Power: 234 mW"));
        assert!(!state.apply_line("CPU Power: 1200 mW"));
        assert_eq!(
            state.publish(),
            Telemetry {
                gpu_util_pct: Some(38.45),
                gpu_mem_used_bytes: None,
                ane_power_mw: Some(234.0),
            }
        );
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
