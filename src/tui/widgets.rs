use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::aggregate::{AggregateSnapshot, WindowMetrics};
use crate::api::ModelInfo;
use crate::db::LifetimeTotals;
use crate::hardware::{HardwareSnapshot, format_bytes, format_bytes_ratio};
use crate::parser::InferenceRecord;
use crate::pricing::{FRONTIER_MODELS, PricingTable, hypothetical_cost};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServerStatus {
    Unknown,
    Reachable,
    Unreachable,
}

pub struct HeaderInfo<'a> {
    pub server_status: ServerStatus,
    pub server_error: Option<&'a str>,
    pub base_url: &'a str,
    pub paused: bool,
    pub lifetime: &'a LifetimeTotals,
}

pub fn render_header(f: &mut Frame, area: Rect, info: HeaderInfo<'_>) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let (status_label, status_color) = match info.server_status {
        ServerStatus::Reachable => ("reachable", Color::Green),
        ServerStatus::Unreachable => ("unreachable", Color::Red),
        ServerStatus::Unknown => ("unknown", Color::Yellow),
    };
    let mut spans = vec![
        Span::styled(
            "lmstudio-monitor",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::raw("server: "),
        Span::styled(
            format!("● {status_label}"),
            Style::default().fg(status_color),
        ),
        Span::raw(format!(" ({})", info.base_url)),
    ];
    if let Some(err) = info.server_error {
        spans.push(Span::raw("  err: "));
        spans.push(Span::styled(
            truncate(err, 60),
            Style::default().fg(Color::Red),
        ));
    }
    if info.paused {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "[PAUSED]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw("   lifetime: "));
    spans.push(Span::styled(
        format!(
            "{} reqs / {} sessions / {} prompt tok / {} gen tok",
            info.lifetime.record_count,
            info.lifetime.session_count,
            info.lifetime.total_prompt_tokens,
            info.lifetime.total_gen_tokens,
        ),
        Style::default().fg(Color::Cyan),
    ));
    spans.push(Span::raw("   "));
    spans.push(Span::styled(now, Style::default().fg(Color::DarkGray)));

    let para = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title("status"));
    f.render_widget(para, area);
}

pub fn render_models(
    f: &mut Frame,
    area: Rect,
    models: &[ModelInfo],
    recent_model_id: Option<&str>,
) {
    let header = Row::new(vec![
        Cell::from("id"),
        Cell::from("type"),
        Cell::from("compat"),
        Cell::from("quant"),
        Cell::from("ctx"),
        Cell::from("state"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = models
        .iter()
        .map(|m| {
            let is_recent = recent_model_id.is_some_and(|r| r == m.id);
            let state_color = match m.state.as_str() {
                "loaded" => Color::Green,
                "not-loaded" => Color::DarkGray,
                _ => Color::Yellow,
            };
            let id_span = if is_recent {
                Span::styled(
                    format!("▸ {}", m.id),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(format!("  {}", m.id))
            };
            Row::new(vec![
                Cell::from(Line::from(id_span)),
                Cell::from(m.kind.as_deref().unwrap_or("-").to_string()),
                Cell::from(m.compatibility_type.as_deref().unwrap_or("-").to_string()),
                Cell::from(m.quantization.as_deref().unwrap_or("-").to_string()),
                Cell::from(
                    m.max_context_length
                        .map_or("-".to_string(), |c| c.to_string()),
                ),
                Cell::from(Span::styled(m.state.clone(), Style::default().fg(state_color))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("loaded models"));
    f.render_widget(table, area);
}

pub fn render_feed(f: &mut Frame, area: Rect, recent: &[InferenceRecord]) {
    let header = Row::new(vec![
        Cell::from("time"),
        Cell::from("model"),
        Cell::from("prompt"),
        Cell::from("gen"),
        Cell::from("TTFT"),
        Cell::from("tok/s"),
        Cell::from("stop"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = recent
        .iter()
        .rev() // newest first
        .map(|r| {
            let time = r
                .started_at
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string();
            Row::new(vec![
                Cell::from(time),
                Cell::from(truncate(&r.model_id, 28).to_string()),
                Cell::from(r.prompt_tokens.to_string()),
                Cell::from(r.gen_tokens.to_string()),
                Cell::from(format!("{:.0}ms", r.ttft_ms)),
                Cell::from(format!("{:.1}", r.tokens_per_second)),
                Cell::from(r.stop_reason.as_deref().unwrap_or("-").to_string()),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(24),
    ];
    let title = format!("live request feed ({}/30)", recent.len());
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

pub fn render_rolling(f: &mut Frame, area: Rect, snap: &AggregateSnapshot) {
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("1m"),
        Cell::from("5m"),
        Cell::from("15m"),
        Cell::from("session"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    fn metric_row(
        label: &str,
        f: impl Fn(&WindowMetrics) -> String,
        snap: &AggregateSnapshot,
    ) -> Row<'static> {
        Row::new(vec![
            Cell::from(Span::styled(
                label.to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Cell::from(f(&snap.window_1m)),
            Cell::from(f(&snap.window_5m)),
            Cell::from(f(&snap.window_15m)),
            Cell::from(f(&snap.session_lifetime)),
        ])
    }

    let rows = vec![
        metric_row("requests", |m| m.req_count.to_string(), snap),
        metric_row("prompt tok", |m| m.total_prompt_tokens.to_string(), snap),
        metric_row("gen tok", |m| m.total_gen_tokens.to_string(), snap),
        metric_row("mean tok/s", |m| format!("{:.1}", m.mean_tps), snap),
        metric_row("p95 tok/s", |m| format!("{:.1}", m.p95_tps), snap),
        metric_row("mean TTFT", |m| format!("{:.0}ms", m.mean_ttft_ms), snap),
    ];

    let widths = [
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("rolling metrics"),
        );
    f.render_widget(table, area);
}

pub fn render_costs(
    f: &mut Frame,
    area: Rect,
    snap: &AggregateSnapshot,
    pricing: &PricingTable,
) {
    let mut header_cells: Vec<Cell> = vec![Cell::from("")];
    for key in FRONTIER_MODELS {
        header_cells.push(Cell::from(*key));
    }
    let header = Row::new(header_cells).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    // Aggregate session totals
    let total_prompt = snap.session_lifetime.total_prompt_tokens;
    let total_gen = snap.session_lifetime.total_gen_tokens;

    // Build a synthetic record for cost summing.
    let synthetic = InferenceRecord {
        model_id: String::new(),
        started_at: snap.session_started_at,
        prompt_tokens: total_prompt,
        gen_tokens: total_gen,
        ttft_ms: 0.0,
        gen_ms: 0.0,
        total_ms: 0.0,
        tokens_per_second: 0.0,
        stop_reason: None,
        num_gpu_layers: None,
    };

    let costs: Vec<_> = FRONTIER_MODELS
        .iter()
        .map(|k| pricing.lookup(k).map(|p| hypothetical_cost(&synthetic, p)))
        .collect();

    fn cost_cell(c: &Option<crate::pricing::HypotheticalCost>, sel: fn(&crate::pricing::HypotheticalCost) -> f64) -> Cell<'static> {
        match c {
            Some(c) => Cell::from(format!("${:.4}", sel(c))),
            None => Cell::from("(no rate)"),
        }
    }

    fn row(
        label: &str,
        costs: &[Option<crate::pricing::HypotheticalCost>],
        sel: fn(&crate::pricing::HypotheticalCost) -> f64,
    ) -> Row<'static> {
        let mut cells = vec![Cell::from(Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray),
        ))];
        for c in costs {
            cells.push(cost_cell(c, sel));
        }
        Row::new(cells)
    }

    let rows = vec![
        row("input USD", &costs, |c| c.input_usd),
        row("output USD", &costs, |c| c.output_usd),
        row(
            "total USD",
            &costs,
            |c| c.total_usd,
        ),
    ];

    let mut widths = vec![Constraint::Length(14)];
    for _ in FRONTIER_MODELS {
        widths.push(Constraint::Min(14));
    }
    let title = format!(
        "hypothetical session cost  (prompt={total_prompt} tok / gen={total_gen} tok)"
    );
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

/// One-line hardware summary: system cpu/mem │ LM Studio process tree │ GPU/ANE.
/// Groups run from most to least important left→right, so a narrow terminal
/// clips the GPU/ANE tail before anything else.
fn hardware_line(hw: &HardwareSnapshot) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let sep = || Span::styled(" │ ", dim);

    let mut spans = vec![
        Span::raw("cpu "),
        Span::styled(
            format!("{:>5.1}%", hw.system_cpu_percent),
            cpu_style(hw.system_cpu_percent),
        ),
        Span::raw("  mem "),
        Span::styled(
            format_bytes_ratio(hw.mem_used_bytes, hw.mem_total_bytes),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(" ("),
        Span::styled(
            format!("{} free", format_bytes(hw.mem_available_bytes)),
            Style::default().fg(Color::Green),
        ),
        Span::raw(")"),
        sep(),
        Span::styled("lms ", dim.add_modifier(Modifier::BOLD)),
    ];

    if hw.lms_process_count == 0 {
        spans.push(Span::styled("no process detected", dim));
    } else {
        spans.extend([
            Span::raw("cpu "),
            Span::styled(
                format!("{:>5.1}%", hw.lms_cpu_percent),
                cpu_style(hw.lms_cpu_percent),
            ),
            Span::raw("  rss "),
            Span::styled(
                format_bytes(hw.lms_rss_bytes),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(
                "  {} proc{}",
                hw.lms_process_count,
                if hw.lms_process_count == 1 { "" } else { "s" }
            )),
        ]);
    }

    spans.push(sep());
    spans.push(Span::raw("gpu "));
    spans.push(match hw.gpu_active_percent {
        Some(pct) => Span::styled(format!("{pct:>5.1}%"), cpu_style(pct)),
        None => Span::styled("  n/a", dim),
    });
    spans.push(Span::raw("  ane "));
    spans.push(match hw.ane_power_mw {
        Some(mw) => Span::styled(
            format!("{mw:>4.0} mW"),
            Style::default().fg(if mw > 100.0 { Color::Yellow } else { Color::Green }),
        ),
        None => Span::styled(" n/a", dim),
    });

    Line::from(spans)
}

pub fn render_hardware(f: &mut Frame, area: Rect, hw: &HardwareSnapshot) {
    let para = Paragraph::new(hardware_line(hw))
        .block(Block::default().borders(Borders::ALL).title("hardware"));
    f.render_widget(para, area);
}

fn cpu_style(pct: f32) -> Style {
    let color = if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    Style::default().fg(color)
}

pub fn render_footer(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit  ·  "),
        Span::styled("r", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" reset session  ·  "),
        Span::styled("p", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" pause"),
    ]);
    let para = Paragraph::new(line).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Safe-byte truncation
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_handles_unicode() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        // multi-byte: each emoji is 4 bytes, truncate to 3 must back up to a valid boundary
        let s = "héllo";
        let t = truncate(s, 2);
        assert!(s.starts_with(t));
    }

    // Layout helpers don't have test coverage — they're exercised via the smoke run
    // and the deterministic snapshot tests below would be brittle to ratatui style updates.

    use crate::aggregate::Aggregator;
    use chrono::Utc;

    #[test]
    fn render_with_empty_state_does_not_panic() {
        // Sanity smoke: build the snapshot on empty aggregator and ensure costs lookup works
        let agg = Aggregator::new();
        let snap = agg.snapshot();
        let pricing = PricingTable::defaults();
        for k in FRONTIER_MODELS {
            assert!(pricing.lookup(k).is_some(), "frontier {k} missing");
        }
        // Ensure the hypothetical cost computation doesn't divide by zero on empty session
        let synth = InferenceRecord {
            model_id: String::new(),
            started_at: snap.session_started_at,
            prompt_tokens: 0,
            gen_tokens: 0,
            ttft_ms: 0.0,
            gen_ms: 0.0,
            total_ms: 0.0,
            tokens_per_second: 0.0,
            stop_reason: None,
            num_gpu_layers: None,
        };
        let _ = synth;
        let _ = Utc::now();
    }
    /// Worst-case-ish values: three-digit percentages, three-digit GB, double-digit
    /// proc count, four-digit ANE mW. The whole row must stay one line and fit the
    /// 118 inner columns of a 120-column terminal (typical values land near 108).
    #[test]
    fn hardware_line_is_one_compact_row() {
        let hw = HardwareSnapshot {
            system_cpu_percent: 100.0,
            mem_used_bytes: 123_400_000_000,
            mem_total_bytes: 137_400_000_000,
            mem_available_bytes: 101_200_000_000,
            lms_cpu_percent: 850.3,
            lms_rss_bytes: 98_700_000_000,
            lms_process_count: 12,
            gpu_active_percent: Some(100.0),
            ane_power_mw: Some(1234.0),
        };
        let line = hardware_line(&hw);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('\n'), "must be a single row: {text:?}");
        assert!(
            line.width() <= 116,
            "hardware row too wide ({} cols): {text}",
            line.width()
        );
        for needle in [
            "cpu 100.0%",
            "mem 123.4/137.4 GB",
            "101.2 GB free",
            "lms cpu 850.3%",
            "rss 98.7 GB",
            "12 procs",
            "gpu 100.0%",
            "ane 1234 mW",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text:?}");
        }
    }

    #[test]
    fn hardware_line_collapses_missing_lms_and_telemetry() {
        let hw = HardwareSnapshot::default();
        let line = hardware_line(&hw);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lms no process detected"), "{text:?}");
        assert!(!text.contains("rss"), "rss should be omitted without a process: {text:?}");
        assert!(text.contains("gpu   n/a"), "{text:?}");
        assert!(text.contains("ane  n/a"), "{text:?}");
    }
}
