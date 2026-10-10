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
use crate::server_log::ContextRejection;

use widgets::{FeedEntry, ServerStatus};

const FEED_CAPACITY: usize = 30;

pub struct AppState {
    pub base_url: String,
    pub aggregator: Aggregator,
    pub pricing: PricingTable,
    /// Newest first.
    pub feed: VecDeque<FeedEntry>,
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
            feed: VecDeque::with_capacity(FEED_CAPACITY),
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
        // Taken now, while the model that just answered is still loaded; it may unload later.
        let loaded_context = self
            .models
            .iter()
            .find(|m| m.id == rec.model_id)
            .and_then(|m| m.loaded_context_length);
        self.push_feed(FeedEntry::Completed {
            record: rec.clone(),
            loaded_context,
        });
        self.aggregator.ingest(rec);
    }

    /// A request LM Studio refused before running it. It only goes in the feed: nothing
    /// ran, so there's nothing for the rolling metrics or the cost panel.
    pub fn ingest_rejection(&mut self, rej: ContextRejection) {
        if self.paused {
            return;
        }
        self.last_inference_model_id = Some(rej.model_id.clone());
        self.push_feed(FeedEntry::Rejected(rej));
    }

    fn push_feed(&mut self, entry: FeedEntry) {
        if self.feed.len() == FEED_CAPACITY {
            self.feed.pop_back();
        }
        self.feed.push_front(entry);
    }

    pub fn reset_session(&mut self) {
        self.feed.clear();
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
    widgets::render_feed(f, l.feed, &state.feed);
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
                    if k.kind == KeyEventKind::Press && tx.blocking_send(k).is_err() {
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    base_url: String,
    pricing: PricingTable,
    mut models_rx: mpsc::Receiver<ModelsSnapshot>,
    mut record_rx: mpsc::Receiver<InferenceRecord>,
    mut rejection_rx: mpsc::Receiver<ContextRejection>,
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
            // `Some(..)` patterns disable an arm once its channel closes; matching `None`
            // would complete instantly on every pass and spin the loop.
            Some(snap) = models_rx.recv() => {
                state.ingest_models(snap);
            }
            Some(rec) = record_rx.recv() => {
                if let Some(sink) = &record_sink {
                    sink.insert(rec.clone()).await;
                }
                state.ingest_record(rec);
            }
            Some(rej) = rejection_rx.recv() => {
                if let Some(sink) = &record_sink {
                    sink.insert_rejection(rej.clone()).await;
                }
                state.ingest_rejection(rej);
            }
            Some(totals) = lifetime_rx.recv() => {
                state.lifetime = totals;
            }
            Some(hw) = hardware_rx.recv() => {
                state.hardware = hw;
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
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    fn sample_state() -> AppState {
        let mut state = AppState::new("http://localhost:1234".into(), PricingTable::defaults());
        state.hardware = HardwareSnapshot {
            system_cpu_percent: 23.4,
            mem_used_bytes: 38_200_000_000,
            mem_total_bytes: 128_000_000_000,
            mem_available_bytes: 89_800_000_000,
            lms_cpu_percent: 12.3,
            lms_rss_bytes: 20_100_000_000,
            lms_process_count: 3,
            gpu_util_pct: Some(38.5),
            gpu_mem_used_bytes: Some(5_100_000_000),
            ane_power_mw: Some(234.0),
        };
        state
    }

    fn render_buffer(width: u16, height: u16, state: &AppState) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_to_lines(width: u16, height: u16, state: &AppState) -> Vec<String> {
        let buf = render_buffer(width, height, state);
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Column and row of the first rendered occurrence of `needle`. Border characters are
    /// several bytes long, so the byte offset is converted to a column count.
    fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if let Some(i) = row.find(needle) {
                return (row[..i].chars().count() as u16, y);
            }
        }
        panic!("{needle:?} not rendered");
    }

    /// Foreground colour of each cell `needle` covers.
    fn fg_of(buf: &Buffer, needle: &str) -> Vec<Color> {
        let (x0, y) = find(buf, needle);
        (x0..x0 + needle.chars().count() as u16)
            .map(|x| buf[(x, y)].fg)
            .collect()
    }

    fn record(model: &str, stop_reason: Option<&str>) -> InferenceRecord {
        InferenceRecord {
            model_id: model.into(),
            started_at: Utc::now(),
            prompt_tokens: 1_000,
            gen_tokens: 50,
            ttft_ms: 120.0,
            gen_ms: 500.0,
            total_ms: 620.0,
            tokens_per_second: 100.0,
            stop_reason: stop_reason.map(str::to_string),
            num_gpu_layers: None,
        }
    }

    fn loaded_model(id: &str, context: u64) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            object: "model".into(),
            kind: Some("llm".into()),
            publisher: None,
            arch: None,
            compatibility_type: Some("mlx".into()),
            quantization: None,
            state: "loaded".into(),
            max_context_length: Some(context),
            loaded_context_length: Some(context),
            capabilities: None,
        }
    }

    fn rejection(model: &str) -> ContextRejection {
        ContextRejection {
            model_id: model.into(),
            at: Utc::now(),
            input_tokens: Some(359_277),
            context_length: Some(262_144),
        }
    }

    /// The layout's fixed panels sum to 29 rows and the feed needs 7, so 36 rows is the
    /// shortest terminal that fits everything. The hardware row used to be the first
    /// casualty on short terminals; make sure it survives at the minimum height.
    #[test]
    fn hardware_row_survives_at_minimum_height() {
        let state = sample_state();
        let lines = render_to_lines(120, 36, &state);
        for title in [
            "status",
            "loaded models",
            "hardware",
            "live request feed",
            "rolling metrics",
            "hypothetical session cost",
        ] {
            assert!(
                lines.iter().any(|l| l.contains(title)),
                "panel {title:?} missing"
            );
        }
        let hw_row = lines
            .iter()
            .find(|l| l.contains("lms cpu"))
            .expect("hardware row rendered");
        let tail = if cfg!(target_os = "macos") {
            "ane  234 mW"
        } else {
            "vram 5.1 GB"
        };
        assert!(hw_row.contains(tail), "hardware row clipped: {hw_row}");
        assert!(lines.last().unwrap().contains("q quit"), "footer missing");
    }

    /// Re-pricing one model in the config must leave every other cost column priced.
    #[test]
    fn partial_pricing_override_keeps_every_cost_column() {
        let cfg: crate::config::Config = toml::from_str(
            "[pricing.providers.google.models.gemini-3-1-pro]\n\
             input_per_mtok_usd = 4.0\n\
             output_per_mtok_usd = 18.0\n",
        )
        .unwrap();
        let mut state = sample_state();
        state.pricing = cfg.effective_pricing();
        let lines = render_to_lines(120, 36, &state);
        assert!(
            lines.iter().any(|l| l.contains("gemini-3-1-pro")),
            "cost panel missing"
        );
        assert!(
            !lines.iter().any(|l| l.contains("(no rate)")),
            "a cost column lost its rate"
        );
    }

    #[test]
    fn context_overflow_is_red_in_feed() {
        let mut state = sample_state();
        state.ingest_record(record("model-full", Some("contextLengthReached")));
        state.ingest_record(record("model-done", Some("eosFound")));
        state.ingest_rejection(rejection("model-refused"));
        let buf = render_buffer(120, 36, &state);
        assert!(
            fg_of(&buf, "contextLengthReached")
                .iter()
                .all(|c| *c == Color::Red),
            "contextLengthReached isn't red"
        );
        assert!(
            fg_of(&buf, "model-full").iter().all(|c| *c != Color::Red),
            "only the stop cell of a context-full row should be red"
        );
        assert!(
            fg_of(&buf, "eosFound").iter().all(|c| *c != Color::Red),
            "a normal stop reason is red"
        );

        // A refused request's whole row is red, inside the panel's borders.
        let (_, y) = find(&buf, "rejected (ctx 262144)");
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        assert!(
            row.contains("model-refused") && row.contains("359277"),
            "{row}"
        );
        for x in 1..buf.area.width - 1 {
            let cell = &buf[(x, y)];
            if cell.symbol() != " " {
                assert_eq!(cell.fg, Color::Red, "column {x} of {row:?}");
            }
        }
    }

    /// Both context-full replies seen from LM Studio with MLX ended
    /// `maxPredictedTokensReached` at exactly the loaded context (259,773 + 2,371 =
    /// 262,144). One token short, the same stop reason is an ordinary output cap.
    #[test]
    fn token_cap_at_the_context_limit_is_red() {
        let models = || ModelsSnapshot::Loaded(vec![loaded_model("model-a", 262_144)]);
        let capped = |gen_tokens| InferenceRecord {
            prompt_tokens: 259_773,
            gen_tokens,
            ..record("model-a", Some("maxPredictedTokensReached"))
        };
        let stop_is_red = |state: &AppState| {
            let colors = fg_of(&render_buffer(120, 36, state), "maxPredictedTokensReached");
            assert!(
                colors.iter().all(|c| *c == Color::Red) || colors.iter().all(|c| *c != Color::Red),
                "partly red: {colors:?}"
            );
            colors[0] == Color::Red
        };

        let mut state = sample_state();
        state.ingest_models(models());
        state.ingest_record(capped(2_371));
        assert!(stop_is_red(&state), "a full context isn't red");
        // The context was taken when the record arrived, so the row stays red after
        // the model unloads.
        state.ingest_models(ModelsSnapshot::Loaded(Vec::new()));
        assert!(
            stop_is_red(&state),
            "the row lost its red when the model unloaded"
        );

        let mut state = sample_state();
        state.ingest_models(models());
        state.ingest_record(capped(2_370));
        assert!(!stop_is_red(&state), "an ordinary output cap is red");

        // With no loaded context to compare against, nothing is flagged.
        let mut state = sample_state();
        state.ingest_record(capped(2_371));
        assert!(!stop_is_red(&state), "red without a known context");
    }

    #[test]
    fn rejection_ingest_respects_pause_cap_and_aggregator() {
        let mut state = sample_state();
        state.toggle_pause();
        state.ingest_rejection(rejection("while-paused"));
        assert!(
            state.feed.is_empty(),
            "a rejection reached the feed while paused"
        );
        assert_eq!(state.last_inference_model_id, None);
        state.toggle_pause();
        for _ in 0..FEED_CAPACITY + 5 {
            state.ingest_rejection(rejection("m"));
        }
        assert_eq!(state.feed.len(), FEED_CAPACITY);
        assert_eq!(state.last_inference_model_id.as_deref(), Some("m"));
        assert_eq!(
            state.aggregator.snapshot().session_lifetime.req_count,
            0,
            "a rejection counted as a request"
        );
    }

    #[test]
    fn feed_is_newest_first() {
        let mut state = sample_state();
        state.ingest_record(record("older-model", Some("eosFound")));
        state.ingest_rejection(rejection("newer-model"));
        assert!(matches!(
            &state.feed[0],
            FeedEntry::Rejected(r) if r.model_id == "newer-model"
        ));
        let buf = render_buffer(120, 36, &state);
        assert!(find(&buf, "newer-model").1 < find(&buf, "older-model").1);
    }
}
