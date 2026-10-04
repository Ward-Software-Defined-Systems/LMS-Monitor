# LMS-Monitor

A terminal dashboard for [LM Studio](https://lmstudio.ai) on macOS. It watches your local inference as it happens (every request's tokens, time to first token and throughput), shows what the same tokens would have cost on frontier APIs, and keeps live Apple Silicon hardware telemetry alongside.

![LMS-Monitor running in a terminal](assets/LMS-Monitor.png)

It's passive: it reads LM Studio's own stats stream and never sits in the request path, so nothing about how you call LM Studio changes.

## Features

- **Live request feed**: the last 30 completed requests with time, model, prompt and generated tokens, time to first token, tokens per second and stop reason.
- **Models**: every downloaded model with its type, format, quantization, context length and load state; `▸` marks the model the last request went to.
- **Rolling metrics**: requests, tokens, mean and p95 tokens per second, and mean time to first token over the last 1, 5 and 15 minutes and the whole session.
- **Hypothetical cost**: your session's token counts priced at list rates for Claude Fable 5.1, Fable 5, Opus 5.5, Opus 5, Opus 4.8 and Gemini 3.1 Pro.
- **Hardware**: system CPU and memory, CPU and memory of the LM Studio process tree, GPU active residency and Neural Engine power.
- **History**: lifetime request and token totals across sessions, kept in a local SQLite database.

## How it works

LM Studio publishes per-prediction stats on its log stream. LMS-Monitor runs `lms log stream -s model --stats --json` as a subprocess and reads it line by line, and polls the server's `/api/v0/models` endpoint every 2 seconds for the model list. Hardware figures come from [`sysinfo`](https://crates.io/crates/sysinfo) and from macOS's `powermetrics`, which needs sudo.

**Privacy:** the database stores only per-request metadata (model, token counts, timings), never prompt or response text. Nothing leaves your machine; the cost figures come from a pricing table compiled into the binary.

## Requirements

- macOS. Apple Silicon is recommended: the GPU and Neural Engine figures come from `powermetrics`.
- LM Studio with its local server running (Developer tab → Start Server). The `lms` CLI ships with LM Studio at `~/.lmstudio/bin/lms` and doesn't need to be on your `PATH`.
- Rust 1.94 or newer, to build.
- sudo, for the GPU and Neural Engine figures only.

## Install

Clone the repository, then from its directory:

```sh
cargo build --release          # binary at target/release/lmstudio-monitor
cargo install --path .         # or: put lmstudio-monitor on your PATH via ~/.cargo/bin
```

## Usage

```sh
lmstudio-monitor
```

It connects to LM Studio's default address, `http://localhost:1234`. If your server listens elsewhere, pass `--base-url` or set `LMS_BASE_URL`.

At launch it asks for your sudo password, used only to start `powermetrics` for the GPU and Neural Engine figures. Run it as your normal user, not under `sudo`. If sudo fails, those two figures show `n/a` and everything else still works.

`--no-tui` runs headless instead: one summary line per request on stderr, still recorded to the database, and no sudo prompt.

### Flags

| flag | default | purpose |
|---|---|---|
| `--base-url <URL>` | `http://localhost:1234` (env `LMS_BASE_URL`) | LM Studio server |
| `--lms-bin <PATH>` | `~/.lmstudio/bin/lms` (env `LMS_BIN`) | the `lms` CLI |
| `--config <PATH>` | `~/Library/Application Support/lmstudio-monitor/config.toml` | optional config file |
| `--db <PATH>` | `~/Library/Application Support/lmstudio-monitor/usage.db` | SQLite database |
| `--no-tui` | off | headless mode |

### Keys

| key | action |
|---|---|
| `q` or `Ctrl-C` | quit |
| `r` | reset the session counters (the database keeps everything) |
| `p` | pause the display (requests are still recorded) |

## Configuration

Prices live in [`pricing.toml`](pricing.toml) and are compiled in. To change one, add an override to `~/Library/Application Support/lmstudio-monitor/config.toml`:

```toml
[pricing.providers.anthropic.models.claude-opus-4-8]
input_per_mtok_usd  = 5.00
output_per_mtok_usd = 25.00
```

The cost panel is a rough comparison, not a quote. It applies list prices to your local model's token counts (a frontier model would tokenize the same text differently), and it ignores prompt caching, batch discounts and long-context pricing tiers.

## Files

| file | location |
|---|---|
| database | `~/Library/Application Support/lmstudio-monitor/usage.db` |
| log | `~/Library/Application Support/lmstudio-monitor/lmstudio-monitor.log` |
| config (optional) | `~/Library/Application Support/lmstudio-monitor/config.toml` |

Set `LMS_LOG=debug` or `LMS_LOG=trace` for more detail in the log; `trace` includes the raw `powermetrics` output.

## Troubleshooting

| symptom | check |
|---|---|
| header shows `server: ● unreachable` | Is the LM Studio server running (`~/.lmstudio/bin/lms server status`)? Is `--base-url` pointing at its port? |
| no requests appear | `~/.lmstudio/bin/lms log stream -s model --stats --json` should print a JSON line per prediction; if it doesn't, update LM Studio. |
| GPU or ANE shows `n/a` | Run with `LMS_LOG=trace` and search the log for `powermetrics`. |
| sudo prompt fails | Run `sudo -v` first, or use `--no-tui`. |

## Development

```sh
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

CI ([`.gitlab-ci.yml`](.gitlab-ci.yml)) runs the same checks on Linux. [ARCHITECTURE.md](ARCHITECTURE.md) covers the module layout, data flow and design decisions.

[Ollama-Monitor](https://github.com/Ward-Software-Defined-Systems/Ollama-Monitor) is a sibling project that shows the same dashboard for Ollama.

## License

MIT, see [LICENSE](LICENSE).

LMS-Monitor is an independent project, not affiliated with or endorsed by LM Studio, Anthropic or Google.
