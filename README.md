# lmstudio-monitor

A standalone Rust TUI that passively observes [LM Studio](https://lmstudio.ai) inference activity on `localhost` and surfaces:

- **Live request feed** — last 30 completed inferences (timestamp, model, prompt/gen tokens, TTFT, tok/s, stop reason)
- **Rolling throughput** — 1m / 5m / 15m / session-lifetime windows, per-model
- **Hypothetical frontier cost** — Claude Fable 5, Opus 4.8, Gemini 3.1 Pro priced against the local token counts
- **Hardware** — system + LM Studio process tree CPU%, memory, GPU active residency, ANE power

No real frontier API calls — costs come from a baked-in pricing table. No daemon. Single binary. Local SQLite for cross-session totals.

## Status

v0.1.0. Implements [`LMS-MONITOR-01`](./LMS-MONITOR-01.md). Step 0 verification + the architectural pivot from `lms log stream -s runtime` to `-s model --stats --json` are documented in [`STEP-0-FINDINGS.md`](./STEP-0-FINDINGS.md).

## Requirements

- macOS (Apple Silicon recommended for GPU/ANE telemetry)
- LM Studio installed; embedded HTTP server enabled (Developer tab → "Start Server")
- `lms` CLI bundled with LM Studio at `~/.lmstudio/bin/lms` (run `~/.lmstudio/bin/lms bootstrap` once if you want it on PATH; not required by this app)
- Rust 1.94+ (2024 edition)
- `sudo` access for the GPU/ANE row (`powermetrics` requires elevation; you'll be prompted once per launch)

## Build

```sh
cargo build --release
# Produces target/release/lmstudio-monitor (~8.3 MB)
```

## Run

```sh
target/release/lmstudio-monitor
```

You'll be prompted for your sudo password — used solely to spawn `powermetrics --samplers cpu_power,gpu_power,ane_power -i 2000`. The TUI then opens.

If you'd rather skip GPU/ANE and avoid the prompt, use headless mode (which doesn't render the panel):

```sh
target/release/lmstudio-monitor --no-tui
```

### Flags

| flag | default | purpose |
|---|---|---|
| `--base-url <URL>` | `http://localhost:31337` | LM Studio HTTP server |
| `--config <PATH>` | XDG / macOS app-support dir | optional user config TOML |
| `--db <PATH>` | XDG / macOS app-support dir | SQLite usage DB |
| `--lms-bin <PATH>` | `~/.lmstudio/bin/lms` (env `LMS_BIN`) | path to the `lms` CLI binary |
| `--no-tui` | off | headless: print one summary line per completed inference to stderr |

### Keys (TUI)

| key | action |
|---|---|
| `q` / `Ctrl-C` | quit (terminal restored, session closed in DB) |
| `r` | reset session counters (records remain in DB) |
| `p` | pause UI updates (records still persist) |

## File locations (macOS)

- DB: `~/Library/Application Support/lmstudio-monitor/usage.db`
- App log: `~/Library/Application Support/lmstudio-monitor/lmstudio-monitor.log`
- User config (optional): `~/Library/Application Support/lmstudio-monitor/config.toml`

Set `LMS_LOG=debug` (or `trace`) to crank tracing detail in the log file. `trace` includes raw `powermetrics` lines, useful for diagnosing GPU/ANE parsing issues.

## Override pricing

Defaults are baked in from the values in [`pricing.toml`](./pricing.toml) (rates updated 2026-07-09; the original snapshot is [STEP-0-FINDINGS §0.5](./STEP-0-FINDINGS.md#05--frontier-pricing-snapshot-per-1m-tokens-usd)). To override, drop a TOML file at `~/Library/Application Support/lmstudio-monitor/config.toml`:

```toml
[pricing.providers.anthropic.models.claude-opus-4-8]
input_per_mtok_usd  = 5.00
output_per_mtok_usd = 25.00
```

## Troubleshooting

| symptom | check |
|---|---|
| "server: ●unreachable" | LM Studio's "Start Server" toggle (Developer tab); `~/.lmstudio/bin/lms server status` |
| GPU or ANE shows `n/a` | `LMS_LOG=trace` then `grep powermetrics ~/Library/Application\ Support/lmstudio-monitor/lmstudio-monitor.log` — the parser tolerates label variants but isn't psychic |
| no records appear despite traffic | the model source needs `lms log stream -s model --stats --json`; verify with `~/.lmstudio/bin/lms --version` |
| sudo prompt fails / app exits | sudo cache may be stale; run `sudo -v` once before launching, or use `--no-tui` to skip the powermetrics path |

## Development

```sh
cargo test          # 44 tests (parser, aggregator, pricing, db, hardware sampler, TUI layout)
cargo run -- --help
```

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for module layout, data flow, and design decisions.
