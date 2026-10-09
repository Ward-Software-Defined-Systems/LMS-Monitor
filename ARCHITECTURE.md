# Architecture

## Goal

A single Rust binary that passively observes a local LM Studio instance and shows inference metrics and hypothetical frontier-API costs in a terminal UI. No daemon, nothing in the request path, and no network traffic except to the LM Studio server.

## Process model

One OS process running a multi-thread `tokio` runtime, plus two child processes and one plain thread for keyboard input. Data moves over bounded `tokio::sync::mpsc` channels (capacities in parentheses below). The one piece of shared state is the latest powermetrics reading, behind an `Arc<tokio::sync::Mutex<PowermetricsState>>`. The UI loop runs inline in `main`, not as a spawned task.

```
 lms log stream -s model --stats --json           sudo -n /usr/bin/powermetrics …
 (child, restarted with backoff)                  (child, own process group, root)
          │ stdout                                         │ stdout
          ▼                                                ▼
 log_stream::run                                  powermetrics reader task
          │ lines (256)                                    │ writes
          ▼                                                ▼
 parser::parser_task                              Arc<Mutex<PowermetricsState>>
          │ records (64)                                   │ read on each tick
          │                                                ▼
          │     api::poll_models                  hardware::run_sampler
          │     GET /api/v0/models, 2 s           sysinfo, 2 s
          │           │ models (8)                         │ hardware (4)
          │           │   server_log::run                  │
          │           │   tails server-logs, 1 s           │
          │           │     │ rejections (16)              │
          ▼           ▼     ▼                              ▼
 ┌──────────────────────────────────────────────────────────────────────┐
 │ UI loop, inline in main: tui::run, or run_headless with --no-tui     │
 │ redraws on a 250 ms tick and after every message                     │
 └──────────────────────────────────────────────────────────────────────┘
          │ DbHandle (256)     ▲ keys (64)                 ▲ lifetime (4)
          ▼                    │                           │
 db writer task           input thread            lifetime poller, 2 s
          │ inserts       crossterm::event::read  own SQLite connection
          ▼                                                ▲ reads
 usage.db (SQLite) ────────────────────────────────────────┘

 signal task: SIGINT / SIGTERM ──▶ shutdown watch<bool>
```

In `--no-tui` mode, `run_headless` consumes only the records and rejections; `main` drops the hardware and lifetime receivers so those tasks stop at their first send, and powermetrics isn't started.

### Shutdown

`q` or Ctrl-C in the TUI (raw mode delivers Ctrl-C as a key, not SIGINT), or SIGINT or SIGTERM, ends the UI loop. Then `main`:

1. sets the shutdown `watch`, which `log_stream` (it kills `lms`), the server-log tailer, the hardware sampler and the lifetime poller listen to;
2. queues `DbCommand::Shutdown` behind any pending inserts; the writer stamps `sessions.ended_at` and exits;
3. sends SIGTERM to the powermetrics `sudo`, which relays it, and waits up to 2 s;
4. sleeps 50 ms so the writer can finish, then exits.

The API poller and the parser stop when their channels close, the powermetrics reader when its stdout closes, and the input thread stays blocked until the process exits. SIGHUP (closing the terminal window), SIGKILL and panics skip this sequence and leave `ended_at` NULL.

## Modules

| file | role |
|---|---|
| `src/main.rs` | CLI, tracing setup, sudo priming, channel and task wiring, the lifetime poller, signal handling, TUI vs headless dispatch, shutdown |
| `src/api.rs` | `GET /api/v0/models` client (1 s connect, 3 s total timeout); `poll_models` task; `ModelsSnapshot::{Loaded, Unreachable}` |
| `src/log_stream.rs` | runs `lms log stream -s model --stats --json`; restarts it with backoff 1 → 2 → 4 → … → 30 s |
| `src/parser.rs` | JSON-Lines event parser; `RecordBuilder` pairs start and stats events per model and evicts stale starts |
| `src/server_log.rs` | follows LM Studio's server log for context-overflow rejections: `parse_rejection`, `Tailer`, the `run` task |
| `src/aggregate.rs` | 1m / 5m / 15m / session windows: request count, token sums, mean and p95 tok/s, mean TTFT; also p50 and a per-model breakdown, which nothing renders |
| `src/pricing.rs` | pricing table baked from `pricing.toml`; per-model `merge` of user overrides; `hypothetical_cost` |
| `src/db.rs` | SQLite schema and its v1 → v2 migration, sessions, the writer task (`spawn_writer`, `DbHandle`), `lifetime_totals` |
| `src/config.rs` | optional config file; default db, config and log paths via `directories` |
| `src/hardware.rs` | `sysinfo` sampler; `prime_sudo` and `spawn_powermetrics`; powermetrics line parsers |
| `src/tui/mod.rs` | `AppState`, event loop, key handling, terminal setup and panic-safe restore |
| `src/tui/layout.rs` | top-to-bottom panel layout |
| `src/tui/widgets.rs` | per-panel renderers: header, models, hardware, feed, rolling, costs, footer |

## Event source: the design pivot

The original design read `lms log stream -s runtime`. Captured against real traffic, that source emitted only diagnostics for 3 chat completions (a header line, a blank line and 6 `[DEBUG]` lines), with no timings, token counts or task IDs; see `fixtures/lms-log-source-runtime-mlx.txt`.

The useful source is `lms log stream -s model --stats --json`, which emits one JSON envelope per line. Abridged:

```json
{"timestamp":1777939641468,"data":{"type":"llm.prediction.input","modelIdentifier":"…","modelPath":"…","input":"…"}}
{"timestamp":1777939642794,"data":{"type":"llm.prediction.output","modelIdentifier":"…","output":"…",
  "stats":{"stopReason":"…","tokensPerSecond":96.48,"numGpuLayers":-1,"timeToFirstTokenSec":0.33,
           "totalTimeSec":0.995,"promptTokensCount":20,"predictedTokensCount":99,"totalTokensCount":119}}}
```

`parser::parse_line` deserializes only what it needs: `timestamp`, `data.type`, `data.modelIdentifier` and, for output events, `stats`. `input` and `output` (the full rendered prompt and the response) are never deserialized, so that text goes no further than the line buffer. Every `stats` field except `stopReason` and `numGpuLayers` is required: if one is missing, the line fails to deserialize and becomes `LogEvent::Other`, which is ignored, so that request isn't recorded. Non-JSON lines, like the stream's header and blank lines, are `Other` too.

The pivot made the parser one `serde_json::from_str` per line instead of regexes over free-form text, put the model ID in every event (no cross-referencing against `/api/v0/models`), and removed a state machine over task IDs, which this format doesn't have.

## Input ↔ output correlation

Nothing links an output event to its input event. `RecordBuilder` pairs them per `modelIdentifier`, first in first out, with a wall-clock sanity check:

- On `PredictionInput`: push the timestamp onto that model's `VecDeque`.
- On `PredictionOutput`: take `gen` as predicted tokens ÷ tokens per second (or `totalTimeSec` when tok/s is 0), and a tolerance of `max(2·(ttft + gen), 5 s) + 1 s`. Pop inputs from the front until one is within the tolerance of the output's timestamp; older ones are orphans from requests that never produced stats, and are dropped. The match becomes `started_at`; with no match, the output's own timestamp is used.
- Every 30 s, `parser_task` evicts pending inputs older than an hour (`stale_after`), well past the slowest plausible request.

This keeps one orphan input from mis-pairing every later output. `record_builder_pairs_correctly_skipping_orphan` checks it against `fixtures/lms-log-source-model-stats-mlx.jsonl`, whose first input is such an orphan, and `long_request_keeps_its_start_after_an_eviction_sweep` covers a 5-minute request.

Only `started_at` depends on the pairing; token counts and timings come from the output event itself. With several requests in flight on one model, first-in-first-out order can give a request another one's start time.

## Second source: the server log

A request whose prompt doesn't fit the model's loaded context never produces stats: LM Studio refuses it before generating, so the stream above has nothing to record. Its only trace is one line in LM Studio's server log. From the MLX engine, verbatim:

```
[2026-10-08 15:37:38][ERROR][qwen/qwen3.8-27b] Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.. Error Data: n/a, Additional Data: n/a
```

`server_log::parse_rejection` takes a line only if it starts with `[YYYY-MM-DD HH:MM:SS][ERROR][<tag>] `. The log also holds every request body as pretty-printed, multi-line JSON, and a body can quote this very message; LM Studio also logs a DEBUG `[transformers]` line with the same numbers in the same second. The message after the tag must be one of the engines' overflow errors:

- MLX: `Input does not fit in context length…`, with the input and context sizes. This is the only one seen in a real log.
- llama.cpp: `request (N tokens) exceeds the available context size (M tokens)…`, from the engine's message template.
- Both: `The number of tokens to keep from the initial prompt is greater than the context length…`, without sizes.

A size counts only if ` tokens` follows it, so a line cut off mid-number gives no size rather than a wrong one. The timestamp is local time, to the second; `earliest()` resolves a DST overlap, and a time that doesn't exist (the spring-forward gap) falls back to now.

The files are `<server-logs>/YYYY-MM/YYYY-MM-DD.N.log` (default `~/.lmstudio/server-logs`, `--server-log-dir` to change). LM Studio starts each day at `.1`, moves to the next number at about 10 MiB, and appends to the current file across restarts. N has passed 9, so files sort by date, then N as a number. `Tailer::poll`, called every second by `server_log::run`:

1. lists the files, in month folders from the current file's month on;
2. on the first successful listing, starts at the end of the newest file, skipping the rest of a line it lands in the middle of; with no files yet, every file that appears later is read from its start;
3. drains the current file, then, while a later file is listed, finishes the current one and drains that next one from offset 0. The listing comes before the drain, so a file is only left once all of it has been read;
4. reopens the file by path each time and compares its device and inode with the last read's: a replaced file, or one shorter than the offset, is read again from 0. If the current file disappears, it moves to the next file, or the newest.

Lines are matched on their first 4 KiB, so a multi-megabyte body line is never held in memory; a partial last line waits for the next poll. Rejections stamped more than 60 s before the tailer started are dropped, so a file read again from 0 doesn't bring back history. The read is synchronous, inside its task, like `hardware::run_sampler`'s; at idle the log grows by under 1 KB/s. Line text never reaches `tracing`: only file names, offsets and the rejections themselves.

Rejections reach the UI loop over their own channel, are saved by the writer task, and go into the feed (subject to pause, like records), but not into the aggregator: nothing ran.

## Aggregator

The session's records live in a `Vec<InferenceRecord>`, cleared by `r`. A snapshot is recomputed on every redraw:

- **1m / 5m / 15m**: records whose `started_at` falls within the window, measured from now. The windows keep sliding while paused, and a request that ran longer than a window never counts in it.
- **session**: every record since launch or the last `r`.
- Metrics: request count, prompt and generated token sums, mean and p95 tok/s (nearest rank after a sort), mean TTFT. p50 and a per-model breakdown are computed too, but nothing renders them.

Cross-session totals come from `db::lifetime_totals`, which the lifetime poller in `main.rs` runs every 2 s and pushes to the header, so the aggregator never reads SQLite.

## Persistence

SQLite via `rusqlite` (bundled). On every open, `open_or_create` applies the v1 schema (`SCHEMA_SQL`, `CREATE … IF NOT EXISTS`, frozen), then `migrate` brings the file to v2:

| table | columns | notes |
|---|---|---|
| `sessions` | `id`, `started_at`, `ended_at` | one row per run; `ended_at` stays NULL if the run didn't shut down cleanly |
| `inference_records` | `id`, `session_id`, `started_at`, `model_id`, `prompt_tokens`, `gen_tokens`, `ttft_ms`, `gen_ms`, `total_ms`, `tokens_per_second`, `raw_json`, `stop_reason` | indexed on `session_id` and `started_at`; `stop_reason` added in v2 |
| `context_rejections` | `id`, `session_id`, `occurred_at`, `model_id`, `input_tokens`, `context_length` | v2; indexed on `occurred_at`; the two sizes are NULL when the message doesn't give them |
| `schema_version` | `version` | one row per applied version: 1 and 2 |

Timestamps are RFC 3339 strings in UTC. `gen_ms` is derived as described above. `total_ms` is LM Studio's `totalTimeSec` as reported; in the captured fixture it's less than TTFT plus generation time, so don't read it as wall time. `raw_json` is always NULL, and the GPU layer count isn't stored.

The migration runs only while `MAX(version)` is below 2, in one `IMMEDIATE` transaction that checks the version again once it holds the lock. It adds `stop_reason` (unless `pragma_table_info` already lists it), creates `context_rejections` and records version 2. A monitor built before v2 may be writing to the same file; taking the write lock up front makes SQLite wait out the busy timeout, where a deferred transaction that read first would fail its lock upgrade at once. That older monitor keeps working on a v2 file: its schema SQL is all no-ops there, and its insert names its columns, leaving `stop_reason` NULL. So a NULL `stop_reason` means the row predates v2 or came from an older binary.

Connections:

- `main` opens one, which runs the migration, inserts the session row, and hands it to `db::spawn_writer`. From then on every insert, record or rejection, goes through that task; `DbHandle` is a cloneable sender for its channel.
- The lifetime poller opens its own connection with the same `open_or_create` after that, so it finds v2 already in place and afterwards only reads.
- The journal mode is SQLite's default rollback journal, not WAL. rusqlite's default 5 s busy timeout covers the brief overlap between the writer and the poller.

## Pricing

[`pricing.toml`](./pricing.toml) is baked in with `include_str!` (list prices, updated 2026-10-04; Gemini at its ≤200K-token prompt rate). The cost panel's columns are fixed at compile time:

```rust
pub const FRONTIER_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "gemini-3-1-pro",
];
```

A `[pricing]` section in the user config is merged onto the baked table one model at a time (`PricingTable::merge`, called by `Config::effective_pricing`). Each override replaces that model's rates and removes the key from whichever provider held it, so `lookup`, which searches every provider, never finds two rates. Keys outside `FRONTIER_MODELS` are kept but never shown. The config file is `~/Library/Application Support/lmstudio-monitor/config.toml` on macOS (`$XDG_CONFIG_HOME/lmstudio-monitor/config.toml` on Linux, via `directories`), or wherever `--config` points.

The panel prices the session's total prompt and generated tokens at flat rates; long-context tiers (Gemini above 200K prompt tokens) and caching discounts aren't modelled. Ollama-Monitor keeps an identical rate table in its `pricing.toml`; change both together.

## Hardware sampling

Two sources feed one snapshot every 2 s:

- **CPU, memory and LM Studio's processes** via `sysinfo` 0.38, with no privileges. A process counts as LM Studio's if its executable path contains `/LM Studio.app/` or `/.lmstudio/`, or its name contains `mlx-llm` or `llama-server`. That catches the GUI app, its Electron helpers, the bundled `~/.lmstudio/.internal/utils/node` inference worker (which holds the loaded model, typically 20+ GB RSS), the `lms` CLI (including the stream the monitor itself runs) and any standalone `llama-server`. Matching on names alone missed the worker, whose `argv[0]` is just `node`. LM Studio's CPU is the sum of per-process figures (100% = one core), while system CPU is 0–100 across all cores. "free" is sysinfo's available memory.
- **GPU active residency and ANE power** via `powermetrics --samplers cpu_power,gpu_power,ane_power -i 2000`, which needs root. A reader task parses `GPU HW active residency:` (or the older `GPU active residency:`) and `ANE Power:` (or `ANE power:`, `ANE:`) lines into the shared state. The `cpu_power` sampler is there because the `ANE Power:` line is part of the unified power summary it emits: on an M4 Max with a recent macOS, `ane_power` alone printed nothing while idle.

Getting root without breaking the TUI takes three steps:

1. `hardware::prime_sudo` runs `sudo -v` synchronously in `main`, after the config loads and before the database opens or the TUI takes the terminal. A password prompt therefore reads from a normal cooked terminal, and Ctrl-C there exits before anything is written to the database (no signal handler is installed yet).
2. `spawn_powermetrics` runs `sudo -n /usr/bin/powermetrics …`. `-n` never prompts: it uses the credential `sudo -v` cached, or a NOPASSWD rule, and otherwise fails at once. It's spawned even when priming failed, so a NOPASSWD rule for powermetrics alone still works.
3. The child gets its own process group (`process_group(0)`) and no stdin. Since sudo 1.9.14, `use_pty` is on by default, and a sudo in the terminal's foreground process group may read keystrokes to relay to its command, competing with the TUI. In a background group it never reads from or reconfigures the terminal. That's only safe because `-n` keeps it from prompting. Don't use `setsid`: sudo ties its cached credential to the terminal session.

If any step fails, a warning goes to the log and GPU/ANE show `n/a`; the TUI still runs. `--no-tui` skips all of it. On shutdown, `libc::kill` sends SIGTERM to the sudo process, which relays it so powermetrics doesn't linger as root. powermetrics is never respawned. When its output ends, the reader clears the readings, so GPU/ANE drop back to `n/a` rather than freezing at their last values; it logs `powermetrics exited` as a warning mid-run, or as info at shutdown.

## TUI

`ratatui` 0.30 + `crossterm` 0.29. Top to bottom:

| panel | rows | content |
|---|---|---|
| header | 3 | name · server status and URL · `err:` text when unreachable · `[PAUSED]` · lifetime reqs / sessions / prompt tok / gen tok · local clock |
| loaded models | 6 | id · type · compat · quant · max ctx · state, with `▸` on the most recent request's model. Lists every downloaded model, but only 3 rows fit |
| hardware | 3 | one line: system CPU/MEM │ LM Studio CPU/RSS/process count │ GPU/ANE |
| live feed | `Min(7)` | last 30 entries, newest first in arrival order: completed requests (by start time) and refusals (by refusal time). Red marks context overflow: a `contextLengthReached` stop cell, or a whole `rejected (ctx N)` row with the refused prompt's size. Shows terminal height − 32 of them, no scrolling |
| rolling metrics | 9 | columns 1m / 5m / 15m / session; rows: requests, prompt tok, gen tok, mean tok/s, p95 tok/s, mean TTFT |
| hypothetical cost | 7 | per frontier model: session input / output / total USD |
| footer | 1 | `q quit · r reset session · p pause` |

The fixed panels take 29 rows and the feed at least 7, so 36 rows is the minimum (`hardware_row_survives_at_minimum_height` renders 120×36). Every panel fits in 120 columns except the header, which needs about 145 even with zero totals; its clock is the first thing cut off.

[Ollama-Monitor](https://github.com/Ward-Software-Defined-Systems/Ollama-Monitor), the sibling project for Ollama, ports this TUI panel for panel: its `tui/layout.rs` is a byte-identical copy and its `tui/widgets.rs` differs only in data mapping plus a few Ollama-only extras. Change both together. The exception for now is the overflow rows (`FeedEntry::Rejected`, the red stop cell), which have no Ollama data source yet; `rejected_row` and `stop_cell` are written to port as they are.

The loop draws, then waits in `tokio::select!` for the first of: the 250 ms tick, a key, or a message on any channel. Channel arms match `Some(x) = rx.recv()`, so a closed channel disables its arm instead of spinning the loop. Keys come from a plain thread blocked in `crossterm::event::read`, which forwards key presses over a channel.

Pause (`p`) only gates `AppState::ingest_record` and `ingest_rejection`. The loop still writes each record and rejection to the database, but while paused they skip the feed (and records the aggregator) for good. The header, models, hardware and lifetime totals keep updating, and the rolling windows keep sliding.

Terminal handling:

1. The panic hook is installed first. It restores the terminal, then runs the original hook.
2. `enter_terminal()`: `enable_raw_mode`, then `EnterAlternateScreen`.
3. `leave_terminal()`: `LeaveAlternateScreen`, then `disable_raw_mode`.

`q`, Ctrl-C, SIGINT, SIGTERM, draw errors and panics all end in `leave_terminal()`.

## Notable invariants

- Every record the parser emits, and every rejection the tailer emits, is written to the database exactly once, by the UI loop: `tui::run` through its `record_sink`, or `run_headless`. While not paused, the TUI also adds it to the feed, and a record to the aggregator.
- Pricing keys are hyphenated (`gemini-3-1-pro`, not `gemini-3.1-pro`) so they work as bare TOML keys. A key that's in `FRONTIER_MODELS` but missing from the table shows as `(no rate)`.
- `tracing` writes only to the log file. stderr is used only before the TUI starts (the line explaining sudo, `sudo -v failed` if priming fails, sudo's own prompt on the terminal, and `powermetrics unavailable` if sudo can't be spawned) and in `--no-tui` mode (a banner and one line per request).
- `lms log stream` restarts with backoff 1 → 2 → 4 → 8 → 16 → 30 s, which never resets (see Known gaps).
- After startup the writer task is the only writer; the lifetime poller's connection only reads.
- `record_sink: Option<DbHandle>` is always `Some` today; `None` would show records without saving them.

## Testing

`cargo test` needs no LM Studio, sudo, powermetrics or terminal; CI runs it, along with fmt and clippy, on Linux.

| module | covered |
|---|---|
| `parser` | line classification and pairing against the captured stream, orphan skipping, eviction, a 5-minute request, unmatched outputs |
| `aggregate` | windows, percentiles and per-model sums on synthetic records |
| `server_log` | rejection parsing against verbatim server-log lines, including real ones that must not match; the tailer in temp folders: starting at the end, partial and overlong lines, rotation across numbers, days and months, truncation, replacement and deletion, the stale-line guard, the `run` task |
| `db` | the v1 → v2 migration (fresh, idempotent, beside an open v1 connection, concurrent opens), stop reasons, rejections, lifetime totals across sessions, the writer task |
| `pricing`, `config` | baked rates, cost arithmetic, per-model override merging |
| `api` | `/api/v0/models` parsing against captured responses |
| `hardware` | powermetrics line parsers, readings clearing when powermetrics exits, byte formatting, a live `sysinfo` sample |
| `tui` | rendering into ratatui's `TestBackend`: every panel at 120×36, the one-line hardware row, a partial pricing override, red overflow cells and rows, feed order, rejections under pause |

The fixtures are real LM Studio captures. The `api` and `parser` tests load them; `lms-log-source-runtime-mlx.txt` is kept only as evidence for the event-source pivot. The `server_log` tests use verbatim server-log lines inline instead of a captured file, because a raw server log holds private request bodies.

CI builds with `rust:1.97`, so the declared minimum, Rust 1.94, isn't exercised. The `log_stream` tilde test sets `HOME` for the whole test process; don't add tests that read it.

## Known gaps

- The models panel fits three models; more are cut off, possibly including the one marked `▸`. `layout.rs` is shared with Ollama-Monitor, so resize it in both.
- Nothing in the UI shows whether `lms log stream` is running, and its stderr is discarded, so a failure shows up only as `lms log stream error` in the log.
- The stream's restart backoff never resets, so after a few LM Studio restarts every reconnect waits 30 s, and requests that finish in the gap are lost.
- An output event missing a required stat (including the unused `totalTokensCount`) is dropped silently.
- powermetrics isn't respawned, so if it exits mid-run, GPU/ANE stay `n/a` until the monitor restarts.
- In `--no-tui` mode nothing reads the models channel, so the API poller blocks after 8 snapshots. That's harmless.
- Concurrent requests on one model can get each other's start times.
- p50, the per-model breakdown and `raw_json` exist but nothing uses them.
- Pricing ignores long-context tiers and caching.
- If `Terminal::new` fails after raw mode is on, `tui::run` returns without restoring the terminal.
- Refusals are recognized from the MLX engine's message, the only one seen in a real log. The llama.cpp messages are matched from the engine's templates, unverified; if llama.cpp errors are logged as multi-line JSON rather than one line, they're missed.
- Nothing in the UI shows whether the server-log folder was found; the log says `server log folder …` when it can't be read. Refusals appear up to a second late, stamped to the second in local time.
- `stop_reason` is NULL on rows recorded before v2 or by an older binary sharing the database.
- The 2 s `/api/v0/models` poll is most of what LM Studio writes to its server log, since it logs every request and the full JSON response: about 48 MiB a day while the monitor runs.

## Reference files

- [`README.md`](./README.md): usage, install, troubleshooting
- [`pricing.toml`](./pricing.toml): baked-in frontier rates
- [`fixtures/`](./fixtures): captured `/api/v0/models` responses and `lms log stream` output (the server-log lines live inline in `src/server_log.rs`'s tests)
- [`.gitlab-ci.yml`](./.gitlab-ci.yml): the fmt, clippy and test jobs
