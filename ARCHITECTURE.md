# Architecture

## Goal

Single Rust binary that passively observes a local LM Studio instance and surfaces inference metrics + hypothetical frontier-API costs in a terminal UI. No daemon, no remote, no real frontier API calls outside `localhost`.

## Process model

One OS process, one `tokio` multi-thread runtime. Tasks communicate through bounded `tokio::sync::mpsc` channels; shutdown propagates through one `tokio::sync::watch::channel<bool>`.

```
                     ┌──────────────────────────────────┐
                     │            main()                │
                     │  parse CLI · open DB · spawn     │
                     │  workers · run TUI or headless · │
                     │  SIGINT / SIGTERM cleanup        │
                     └────────────────┬─────────────────┘
                                      │
   ┌──────────┬─────────────┬─────────┼──────────────┬─────────────┬────────────┐
   ▼          ▼             ▼         ▼              ▼             ▼            ▼
┌──────┐  ┌──────────┐  ┌─────────┐ ┌──────────────┐ ┌─────────┐ ┌────────────┐
│api   │  │log_stream│  │parser   │ │hardware      │ │db_writer│ │ TUI render │
│poll  │  │subproc   │  │task     │ │sampler       │ │task     │ │ task       │
│2 s   │  │          │  │         │ │ + powermetr. │ │         │ │            │
└──┬───┘  └────┬─────┘  └────┬────┘ └──────┬───────┘ └────┬────┘ └─────┬──────┘
   │           │             │             │              │            │
   │       lines │           │records      │ hw snaps     │            │
   │           ├──parser_task│             │              │            │
   │           │            ┌▼ records_tx  │              │            │
   │ models    │            │              │              │            │
   │ snapshots │            │              │              │            │
   ├───────────┴────────────┼──────────────┼──────────────┼────────────►│
   │                        │              │              │            │   render
   │  (lifetime poller:     │  records_rx  │  hardware_rx │  records   │   loop
   │   reads SQLite every   │  (TUI tees   │              │  via       │   250 ms
   │   2 s, sends totals    │   to DB)     │              │  DbHandle  │
   │   to TUI header)       │              │              │            │
   └────────────────────────┴──────────────┴──────────────┴────────────┘
```

## Modules

| file | role |
|---|---|
| `src/main.rs` | CLI parse, runtime bootstrap, channel + task wiring, signal handling, TUI vs headless dispatch |
| `src/api.rs` | `GET /api/v0/models` client; `poll_models` task; `ModelsSnapshot::{Loaded,Unreachable}` |
| `src/log_stream.rs` | `lms log stream -s model --stats --json` subprocess wrapper; exp-backoff restart 1→2→4→…→30 s |
| `src/parser.rs` | JSON-Lines event parser; `RecordBuilder` state machine; per-`modelIdentifier` FIFO with wall-clock sanity |
| `src/aggregate.rs` | rolling 1m / 5m / 15m / session-lifetime windows; per-model breakdown; mean / p50 / p95 |
| `src/pricing.rs` | TOML-loaded pricing table; `hypothetical_cost`; baked-in defaults from `pricing.toml` |
| `src/db.rs` | SQLite schema + sessions + dedicated tokio writer task; `lifetime_totals` query |
| `src/config.rs` | optional user TOML loader; XDG / macOS app-support path resolution for db / config / log |
| `src/hardware.rs` | `sysinfo` CPU/MEM sampler + `sudo powermetrics` GPU/ANE parser |
| `src/tui/mod.rs` | event loop; `AppState`; render dispatcher; panic-safe terminal restore |
| `src/tui/layout.rs` | top-to-bottom panel layout |
| `src/tui/widgets.rs` | per-panel render fns (header, models, hardware, feed, rolling, costs, footer) |

## Event source — the design pivot

The original design read `lms log stream -s runtime`. Captured against real traffic, that source emitted **8 lines of pure diagnostics for 3 chat completions** — no timings, no token counts, no task IDs (see `fixtures/lms-log-source-runtime-mlx.txt`).

The actually-useful source is `lms log stream -s model --stats --json`, which emits JSON-Lines envelopes:

```json
{"timestamp":1777939641468,"data":{"type":"llm.prediction.input","modelIdentifier":"…","input":"…"}}
{"timestamp":1777939642794,"data":{"type":"llm.prediction.output","modelIdentifier":"…","output":"…",
  "stats":{"tokensPerSecond":96.48,"timeToFirstTokenSec":0.33,"totalTimeSec":0.995,
           "promptTokensCount":20,"predictedTokensCount":99,"stopReason":"…"}}}
```

This pivot collapsed the parser (one `serde_json::from_str` per line vs. regex over free-form text), eliminated cross-referencing against `/api/v0/models` for model-id attribution (it's now in every event), and removed a state machine over `task_id` (which doesn't exist in the new format).

## Input ↔ output correlation

Output events arrive separately from input events; there's no `task_id` linking them. `RecordBuilder` pairs them by **per-`modelIdentifier` FIFO with a wall-clock sanity check**:

- On `PredictionInput`: push timestamp into the model's `VecDeque`.
- On `PredictionOutput`: pop the front, but only if `(output_ts − front_input_ts) ≤ 2·(ttft + gen_time) + 1 s`. If the gap is way too large (front input is an orphan from a failed request that never produced output), drop and try the next.

This prevents one orphan input from silently mis-pairing every subsequent output. Verified against the captured fixture `fixtures/lms-log-source-model-stats-mlx.jsonl`, which contains exactly such an orphan in position 0.

## Aggregator

`Vec<InferenceRecord>` in memory for the current session. Snapshots compute per-window metrics on-demand:

- **1m / 5m / 15m**: filter records younger than the cutoff
- **session_lifetime**: all session records
- Per-model breakdown groups by `model_id`
- Percentiles via sort + index; cheap at session-realistic record counts

Cross-session totals come from a separate `db::lifetime_totals` query. A poller task runs it every 2 s and pushes to the TUI header — so the aggregator stays focused on the live session and doesn't need to bootstrap from SQLite at startup.

## Persistence

SQLite via bundled `rusqlite`. Two tables (`sessions`, `inference_records`) plus a `schema_version` row. Schema is applied idempotently on every open.

All writes go through one dedicated tokio task (`db::spawn_writer`); `DbHandle` is a clone-able sender wrapping its mpsc. No `Arc<Mutex<Connection>>` racing the UI. The lifetime poller opens its own read-only connection — SQLite handles concurrent readers natively.

Sessions are stamped on app start (`started_at`); `ended_at` is filled on graceful shutdown — `q`, `Ctrl-C` (SIGINT), or SIGTERM all route through the same shutdown broadcast.

## Pricing

[`pricing.toml`](./pricing.toml) is `include_str!`-baked at compile time (list prices, updated 2026-10-03). The six frontier model keys are constant:

```rust
pub const FRONTIER_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "gemini-3-1-pro",   // hyphenated for TOML
];
```

Pricing is flat-rate; long-context tiers (e.g. Gemini above 200K prompt tokens) aren't modelled. User config at `$XDG_CONFIG_HOME/lmstudio-monitor/config.toml` (macOS: `~/Library/Application Support/lmstudio-monitor/config.toml`) overrides.

## Hardware sampling

Two paths:

- **CPU + memory + LM Studio process tree** via `sysinfo` — no privilege required. Detection is **path-based**: `/Applications/LM Studio.app/` and `/.lmstudio/` substrings on the executable path. This catches the GUI app, Electron helpers, the bundled `~/.lmstudio/.internal/utils/node` inference worker (where the loaded model actually lives — typically 20+ GB RSS), and the `lms` CLI itself. Pure name-based matching missed the worker because its `argv[0]` is just `node`.
- **GPU active residency + ANE power** via `sudo powermetrics --samplers cpu_power,gpu_power,ane_power -i 2000`. Critically, sudo is invoked **before** TUI raw-mode entry so the password prompt works against a normal cooked terminal. The output is text-parsed for `GPU HW active residency:` and `ANE Power:` (Apple Silicon labels — see hardware.rs tests).

If `sudo` or `powermetrics` fails for any reason, the failure logs a warning and GPU/ANE fall back to `n/a` in the panel. The TUI still launches.

On shutdown, the sudo child receives `SIGTERM` via `libc::kill` so `powermetrics` exits cleanly rather than orphaning as root.

The cpu_power sampler is deliberately included alongside ane_power — on M4 Max with a recent macOS, `ane_power` alone produces nothing in idle; cpu_power emits the unified power summary that includes the `ANE Power:` line.

## TUI

`ratatui` 0.30 + `crossterm` 0.29. Top-to-bottom layout (heights chosen to fit 120×40):

| panel | rows | widget |
|---|---|---|
| header | 3 | server / lms status, lifetime totals, paused flag, clock |
| loaded models | 6 | id · type · compat · quant · ctx · state, with arrow on the most-recent inference target |
| hardware | 3 | one line: system CPU/MEM · LM Studio CPU/RSS/proc count · GPU/ANE |
| live feed | `Min(7)` | last 30 records, newest at top — table grows on tall terminals |
| rolling metrics | 9 | `1m / 5m / 15m / session` columns; rows = req count, prompt tok, gen tok, mean tps, p95 tps, mean TTFT |
| hypothetical cost | 7 | per-frontier-model session-cumulative input / output / total USD |
| footer | 1 | `q quit · r reset session · p pause` |

Ollama-Monitor, the sibling project for Ollama, ports this TUI panel for panel: its `tui/layout.rs` is a byte-identical copy and its `tui/widgets.rs` differs only in data mapping plus a few Ollama-only extras. Change both together.

Render tick: 250 ms. Channel reads non-blocking via `tokio::select!`. Input events arrive from a dedicated blocking thread (sync `crossterm::event::read` → mpsc → main loop).

Terminal restore is bracketed by:

1. `enter_terminal()` — `enable_raw_mode` + `EnterAlternateScreen`
2. **Panic hook installed before** `enter_terminal()` — calls `leave_terminal()` first, then chains the original hook
3. `leave_terminal()` — `disable_raw_mode` + `LeaveAlternateScreen`

`q`, `Ctrl-C`, SIGINT, SIGTERM, and panics all route to a clean restore.

## Notable invariants

- Every record in `inference_records` ⇔ one TUI feed entry ⇔ one aggregator ingest. The TUI is the sole tee point — no double-counting.
- Pricing keys are TOML-friendly (hyphenated, no dots): `gemini-3-1-pro`, not `gemini-3.1-pro`. Mismatch = silent miss.
- `tracing` writes to a file *only* — never stdout/stderr while the TUI is active. The two stderr exceptions are the pre-TUI sudo prompt message and `--no-tui` mode (one line per completed inference).
- Subprocess respawn uses exp backoff 1 → 2 → 4 → 8 → 16 → 30 s.
- DB writer is the only writer; the lifetime poller has its own read-only connection.
- `record_sink: Option<DbHandle>` is `Some` in TUI mode, `None` would mean records reach the TUI but skip persistence (currently always `Some`).

## Reference files

- [`README.md`](./README.md) — usage, install, troubleshooting
- [`pricing.toml`](./pricing.toml) — baked-in frontier defaults
- [`fixtures/`](./fixtures) — captured API + log samples used by parser/aggregator tests
