#![allow(dead_code)]

mod layout;
mod widgets;

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};

use crate::aggregate::Aggregator;
use crate::api::{ModelInfo, ModelsSnapshot};
use crate::db::{DbHandle, LifetimeTotals};
use crate::hardware::HardwareSnapshot;
use crate::parser::InferenceRecord;
use crate::pricing::PricingTable;

use widgets::ServerStatus;

const FEED_CAPACITY: usize = 30;

pub struct AppState {
    pub base_url: String,
    pub aggregator: Aggregator,
    pub pricing: PricingTable,
    pub recent: VecDeque<InferenceRecord>,
    pub models: Vec<ModelInfo>,
    pub server_status: ServerStatus,
    pub server_error: Option<String>,
    pub last_inference_model_id: Option<String>,
    pub paused: bool,
    pub session_started_at: DateTime<Utc>,
    pub lifetime: LifetimeTotals,
    pub hardware: HardwareSnapshot,
}

impl AppState {
    pub fn new(base_url: String, pricing: PricingTable) -> Self {
        Self {
            base_url,
            aggregator: Aggregator::new(),
            pricing,
            recent: VecDeque::with_capacity(FEED_CAPACITY),
            models: Vec::new(),
            server_status: ServerStatus::Unknown,
            server_error: None,
            last_inference_model_id: None,
            paused: false,
            session_started_at: Utc::now(),
            lifetime: LifetimeTotals::default(),
            hardware: HardwareSnapshot::default(),
        }
    }

    pub fn ingest_models(&mut self, snap: ModelsSnapshot) {
        match snap {
            ModelsSnapshot::Loaded(models) => {
                self.server_status = ServerStatus::Reachable;
                self.server_error = None;
                self.models = models;
            }
            ModelsSnapshot::Unreachable { error } => {
                self.server_status = ServerStatus::Unreachable;
                self.server_error = Some(error);
            }
        }
    }

    pub fn ingest_record(&mut self, rec: InferenceRecord) {
        if self.paused {
            return;
        }
        self.last_inference_model_id = Some(rec.model_id.clone());
        if self.recent.len() == FEED_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back(rec.clone());
        self.aggregator.ingest(rec);
    }

    pub fn reset_session(&mut self) {
        self.recent.clear();
        self.aggregator.reset_session();
        self.session_started_at = Utc::now();
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }
}

fn render(f: &mut ratatui::Frame, state: &AppState) {
    let l = layout::compute(f.area());
    widgets::render_header(
        f,
        l.header,
        widgets::HeaderInfo {
            server_status: state.server_status,
            server_error: state.server_error.as_deref(),
            base_url: &state.base_url,
            paused: state.paused,
            lifetime: &state.lifetime,
        },
    );
    widgets::render_models(
        f,
        l.models,
        &state.models,
        state.last_inference_model_id.as_deref(),
    );
    widgets::render_hardware(f, l.hardware, &state.hardware);
    let recent: Vec<InferenceRecord> = state.recent.iter().cloned().collect();
    widgets::render_feed(f, l.feed, &recent);
    let snap = state.aggregator.snapshot();
    widgets::render_rolling(f, l.rolling, &snap);
    widgets::render_costs(f, l.costs, &snap, &state.pricing);
    widgets::render_footer(f, l.footer);
}

fn enter_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    Ok(())
}

fn leave_terminal() -> io::Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_terminal();
        original(info);
    }));
}

fn spawn_input_thread(tx: mpsc::Sender<KeyEvent>) {
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(Event::Key(k)) => {
                    if k.kind == KeyEventKind::Press
                        && tx.blocking_send(k).is_err()
                    {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
}

pub enum InputControl {
    Continue,
    Quit,
}

fn handle_key(state: &mut AppState, key: KeyEvent) -> InputControl {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => InputControl::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => InputControl::Quit,
        KeyCode::Char('r') => {
            state.reset_session();
            InputControl::Continue
        }
        KeyCode::Char('p') => {
            state.toggle_pause();
            InputControl::Continue
        }
        _ => InputControl::Continue,
    }
}

pub async fn run(
    base_url: String,
    pricing: PricingTable,
    mut models_rx: mpsc::Receiver<ModelsSnapshot>,
    mut record_rx: mpsc::Receiver<InferenceRecord>,
    mut lifetime_rx: mpsc::Receiver<LifetimeTotals>,
    mut hardware_rx: mpsc::Receiver<HardwareSnapshot>,
    mut shutdown: watch::Receiver<bool>,
    record_sink: Option<DbHandle>,
) -> Result<()> {
    install_panic_hook();
    enter_terminal()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let (input_tx, mut input_rx) = mpsc::channel::<KeyEvent>(64);
    spawn_input_thread(input_tx);

    let mut state = AppState::new(base_url, pricing);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result: Result<()> = loop {
        if let Err(e) = terminal.draw(|f| render(f, &state)) {
            break Err(e.into());
        }

        tokio::select! {
            _ = tick.tick() => {}
            Some(key) = input_rx.recv() => {
                if let InputControl::Quit = handle_key(&mut state, key) {
                    break Ok(());
                }
            }
            snap = models_rx.recv() => {
                if let Some(s) = snap {
                    state.ingest_models(s);
                }
            }
            rec = record_rx.recv() => {
                if let Some(r) = rec {
                    if let Some(sink) = &record_sink {
                        sink.insert(r.clone()).await;
                    }
                    state.ingest_record(r);
                }
            }
            lt = lifetime_rx.recv() => {
                if let Some(t) = lt {
                    state.lifetime = t;
                }
            }
            hw = hardware_rx.recv() => {
                if let Some(h) = hw {
                    state.hardware = h;
                }
            }
            _ = shutdown.changed() => {
                break Ok(());
            }
        }
    };

    leave_terminal()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn sample_state() -> AppState {
        let mut state = AppState::new("http://localhost:31337".into(), PricingTable::defaults());
        state.hardware = HardwareSnapshot {
            system_cpu_percent: 23.4,
            mem_used_bytes: 38_200_000_000,
            mem_total_bytes: 128_000_000_000,
            mem_available_bytes: 89_800_000_000,
            lms_cpu_percent: 12.3,
            lms_rss_bytes: 20_100_000_000,
            lms_process_count: 3,
            gpu_active_percent: Some(38.5),
            ane_power_mw: Some(234.0),
        };
        state
    }

    fn render_to_lines(width: u16, height: u16, state: &AppState) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// The layout's fixed panels sum to 29 rows and the feed needs 7, so 36 rows is the
    /// shortest terminal that fits everything. The hardware row used to be the first
    /// casualty on short terminals; make sure it survives at the minimum height.
    #[test]
    fn hardware_row_survives_at_minimum_height() {
        let state = sample_state();
        let lines = render_to_lines(120, 36, &state);
        for title in ["status", "loaded models", "hardware", "live request feed", "rolling metrics", "hypothetical session cost"] {
            assert!(lines.iter().any(|l| l.contains(title)), "panel {title:?} missing");
        }
        let hw_row = lines
            .iter()
            .find(|l| l.contains("lms cpu"))
            .expect("hardware row rendered");
        assert!(hw_row.contains("ane  234 mW"), "hardware row clipped: {hw_row}");
        assert!(lines.last().unwrap().contains("q quit"), "footer missing");
    }
}
