mod aggregate;
mod api;
mod config;
mod db;
mod hardware;
mod log_stream;
mod parser;
mod pricing;
mod tui;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::{mpsc, watch};

/// LM Studio Usage Monitor — TUI.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Base URL of the LM Studio HTTP server.
    #[arg(long, default_value = "http://localhost:31337")]
    base_url: String,

    /// Path to user config TOML (defaults to XDG / macOS app-support dir).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to SQLite usage database (defaults to XDG / macOS app-support dir).
    #[arg(long)]
    db: Option<PathBuf>,

    /// Run without TUI; print one summary line per completed inference to stderr.
    #[arg(long)]
    no_tui: bool,

    /// Path to the `lms` CLI binary.
    #[arg(long, env = "LMS_BIN", default_value = "~/.lmstudio/bin/lms")]
    lms_bin: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.clone().or_else(config::default_config_path);
    let db_path = cli
        .db
        .clone()
        .or_else(config::default_db_path)
        .ok_or_else(|| anyhow::anyhow!("could not resolve default db path"))?;
    let log_path = config::default_log_path()
        .ok_or_else(|| anyhow::anyhow!("could not resolve default log path"))?;

    init_tracing(&log_path)?;
    tracing::info!(
        "starting lmstudio-monitor base_url={} db={} log={}",
        cli.base_url,
        db_path.display(),
        log_path.display()
    );

    let cfg = config::Config::load(config_path.as_deref())?;
    let pricing = cfg.pricing_or_defaults();

    let conn = db::open_or_create(&db_path)?;
    let session_id = db::start_session(&conn)?;
    let db_handle = db::spawn_writer(conn, session_id);

    let (models_tx, models_rx) = mpsc::channel(8);
    let (line_tx, line_rx) = mpsc::channel::<String>(256);
    let (records_tx, records_rx) = mpsc::channel::<parser::InferenceRecord>(64);
    let (lifetime_tx, lifetime_rx) = mpsc::channel::<db::LifetimeTotals>(4);
    let (hardware_tx, hardware_rx) = mpsc::channel::<hardware::HardwareSnapshot>(4);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn({
        let base = cli.base_url.clone();
        async move {
            api::poll_models(base, Duration::from_secs(2), models_tx).await;
        }
    });

    tokio::spawn({
        let bin = cli.lms_bin.clone();
        let sd = shutdown_rx.clone();
        async move {
            log_stream::run(bin, line_tx, sd).await;
        }
    });

    tokio::spawn(parser::parser_task(line_rx, records_tx));

    // Spawn `sudo powermetrics` BEFORE entering TUI raw mode so the password prompt
    // (if needed) appears in the cooked terminal. If it fails, we fall back to None
    // and GPU/ANE simply show "n/a" in the panel — TUI still launches.
    let pm_handle = if cli.no_tui {
        None
    } else {
        eprintln!("Starting powermetrics (GPU/ANE telemetry; may prompt for sudo password)…");
        match hardware::spawn_powermetrics().await {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("powermetrics unavailable: {e:#} — GPU/ANE will show n/a");
                tracing::warn!("powermetrics spawn failed: {e:#}");
                None
            }
        }
    };
    let pm_state = pm_handle.as_ref().map(|h| h.state.clone());

    tokio::spawn({
        let sd = shutdown_rx.clone();
        async move {
            hardware::run_sampler(hardware_tx, Duration::from_secs(2), sd, pm_state).await;
        }
    });

    tokio::spawn({
        let db_path = db_path.clone();
        let mut sd = shutdown_rx.clone();
        async move {
            let conn = match db::open_or_create(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("lifetime poller: open db failed: {e:#}");
                    return;
                }
            };
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Ok(t) = db::lifetime_totals(&conn)
                            && lifetime_tx.send(t).await.is_err()
                        {
                            return;
                        }
                    }
                    _ = sd.changed() => return,
                }
            }
        }
    });

    let shutdown_tx_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("install SIGTERM handler failed: {e}");
                    let _ = tokio::signal::ctrl_c().await;
                    let _ = shutdown_tx_signal.send(true);
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        let _ = shutdown_tx_signal.send(true);
    });

    let result = if cli.no_tui {
        // headless ignores the lifetime + hardware channels; drop receivers so poller sends fail fast.
        drop(lifetime_rx);
        drop(hardware_rx);
        run_headless(records_rx, db_handle.clone(), shutdown_rx.clone()).await
    } else {
        tui::run(
            cli.base_url.clone(),
            pricing,
            models_rx,
            records_rx,
            lifetime_rx,
            hardware_rx,
            shutdown_rx.clone(),
            Some(db_handle.clone()),
        )
        .await
    };

    let _ = shutdown_tx.send(true);
    db_handle.shutdown().await;
    if let Some(h) = pm_handle {
        h.terminate().await;
    }
    // Brief drain so the writer task can flush.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tracing::info!("shutdown complete");

    result
}

async fn run_headless(
    mut record_rx: mpsc::Receiver<parser::InferenceRecord>,
    db: db::DbHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    eprintln!("lmstudio-monitor: headless mode (one line per completed inference; Ctrl-C to quit)");
    loop {
        tokio::select! {
            rec = record_rx.recv() => {
                let Some(r) = rec else { return Ok(()) };
                eprintln!(
                    "{} model={} prompt={} gen={} ttft={:.0}ms tps={:.1} stop={}",
                    r.started_at.with_timezone(&chrono::Local).format("%H:%M:%S"),
                    r.model_id,
                    r.prompt_tokens,
                    r.gen_tokens,
                    r.ttft_ms,
                    r.tokens_per_second,
                    r.stop_reason.as_deref().unwrap_or("-"),
                );
                db.insert(r).await;
            }
            _ = shutdown.changed() => return Ok(()),
        }
    }
}

fn init_tracing(log_path: &PathBuf) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log parent {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open log file {}", log_path.display()))?;
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("LMS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_env_filter(env_filter)
        .with_ansi(false)
        .init();
    Ok(())
}
