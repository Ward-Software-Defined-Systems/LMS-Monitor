#![allow(dead_code)]

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

const ARGS: &[&str] = &["log", "stream", "-s", "model", "--stats", "--json"];

pub async fn run(
    lms_bin: String,
    line_tx: mpsc::Sender<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        if *shutdown.borrow() {
            return;
        }

        match run_once(&lms_bin, &line_tx, &mut shutdown).await {
            Ok(()) => {
                info!("lms log stream exited; restarting in {:?}", backoff);
            }
            Err(e) => {
                warn!("lms log stream error: {e:#}; retrying in {:?}", backoff);
            }
        }

        if *shutdown.borrow() {
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }

        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn run_once(
    lms_bin: &str,
    line_tx: &mpsc::Sender<String>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let bin = expand_tilde(lms_bin);
    let mut child = Command::new(&bin)
        .args(ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {bin}"))?;

    let stdout = child.stdout.take().context("child stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            next = lines.next_line() => {
                match next? {
                    Some(line) => {
                        if line_tx.send(line).await.is_err() {
                            let _ = child.kill().await;
                            return Ok(());
                        }
                    }
                    None => {
                        let _ = child.wait().await;
                        return Ok(());
                    }
                }
            }
            _ = shutdown.changed() => {
                let _ = child.kill().await;
                return Ok(());
            }
        }
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}/{}", home.to_string_lossy(), rest);
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_when_home_set() {
        // SAFETY: test process; setting HOME for the duration of this test.
        unsafe { std::env::set_var("HOME", "/tmp/fakehome") };
        assert_eq!(
            expand_tilde("~/.lmstudio/bin/lms"),
            "/tmp/fakehome/.lmstudio/bin/lms"
        );
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }
}
