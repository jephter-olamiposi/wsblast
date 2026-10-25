# wsblast

WebSocket load testing from the command line, with latency percentiles, a failure breakdown and SLO gates for CI.

[![Crates.io](https://img.shields.io/crates/v/wsblast.svg)](https://crates.io/crates/wsblast)
[![docs.rs](https://img.shields.io/docsrs/wsblast)](https://docs.rs/wsblast)
[![CI](https://github.com/jephter-olamiposi/wsblast/actions/workflows/ci.yml/badge.svg)](https://github.com/jephter-olamiposi/wsblast/actions/workflows/ci.yml)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue.svg)](#msrv-policy)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

![wsblast summary report with latency percentiles and an SLO gate](https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/assets/wsblast-summary-report.png)

Each connection is a worker on a Tokio runtime. In the default echo mode a worker sends a frame, waits for the reply and records the round-trip time; stream mode sends without waiting and listen mode only receives. Every worker records into its own histograms and counters, which are merged once when the run ends. The merged metrics are checked against the SLO thresholds you set, printed as a text, JSON or Markdown report, and turned into the exit code.

## Why wsblast

- **Failure taxonomy.** Failures are counted by cause: DNS resolution, TCP connect, TLS handshake, HTTP upgrade rejected, protocol error, write error, read error, timeout and unexpected close.
- **CI exit codes.** `0` when the run completed and every SLO passed, `1` for configuration or runtime errors, `2` only for an SLO breach or a `--fail-on-regression` regression.
- **HDR percentiles.** Handshake latency and round-trip time are recorded in HDR histograms and reported from p50 to p99.9, merged from every connection rather than averaged.
- **Coordinated omission corrected.** With `--rate`, requests follow a fixed schedule and wsblast also records response time from when each request was due, so a server stall shows up in every request that waited behind it instead of in one slow sample. Latency SLOs gate on it. See [METHODOLOGY.md](https://github.com/jephter-olamiposi/wsblast/blob/main/METHODOLOGY.md#4-timing).
- **Time series.** Every report breaks the run down by interval: throughput, errors and p99 per second, so a stall or a slow ramp shows up where it happened instead of disappearing into the run's averages. The JSON report has every point.
- **Live export.** `--prometheus-listen` serves a `/metrics` endpoint while the run lasts, and `--otel-endpoint` pushes the same metrics to an OpenTelemetry collector, so a load test shows up on the dashboards you already watch.
- **Built-in mock server.** `wsblast mock` is a local test target that can add delay and jitter, stall, close connections or reject upgrades on purpose, or replay a `--capture` recording, so a CI gate can be tested against known failures.
- **Distributed load with exact merging.** `wsblast agent` on several machines and `--agents` on one: the agents start together and their latency histograms are merged, not averaged, so a p99 across machines is a real p99.
- **HTML report.** `--format html` writes one self-contained page with charts of the time series, ready to attach to a CI run; `wsblast render` turns a saved JSON report into one later.
- **Single binary.** One executable, pre-built for Linux, macOS and Windows, with no agents, controllers or scripting runtime to deploy next to it.

## Install

```bash
cargo install wsblast                 # from source
cargo binstall wsblast                # pre-built binary via cargo-binstall
curl -fsSL https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/install.sh | sh
```

The shell installer downloads the release archive for your platform, verifies its SHA-256 checksum and installs to `/usr/local/bin`, or to `~/.local/bin` when sudo is unavailable. Set `WSBLAST_INSTALL_DIR` to choose the directory. Archives for every platform are on the [releases page](https://github.com/jephter-olamiposi/wsblast/releases).

No Docker image is published. Build one from the repository:

```bash
docker build -t wsblast https://github.com/jephter-olamiposi/wsblast.git
docker run --rm wsblast wss://ws.example.com/socket -c 20 -d 10s
```

## Quick start

```bash
# 50 connections for 10 seconds against an echo endpoint
wsblast ws://127.0.0.1:9001 -c 50 -d 10s

# The same run with the live dashboard
wsblast ws://127.0.0.1:9001 -c 50 -d 10s --tui

# Exit with 2 if p99 exceeds 25 ms or more than 1% of operations fail
wsblast ws://127.0.0.1:9001 -c 50 -d 15s --max-p99 25ms --max-error-rate 0.01

# Ramp up to 200 connections over 30 s, hold for 2 minutes, ramp down over 30 s
wsblast ws://127.0.0.1:9001 --stages 30s:200,2m:200,30s:0 -r 10
```

No server to test yet? `wsblast mock` starts an echo server on `127.0.0.1:9001`; add `--delay 20ms --stall-every 5s --stall-for 300ms` to see how a slow, stalling server shows up in the report ([docs/mock.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/mock.md)).

![wsblast live terminal dashboard](https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/assets/wsblast-tui-dashboard.png)

## In CI

```yaml
- name: WebSocket SLO gate
  run: |
    curl -fsSL https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/install.sh | sh
    wsblast "$WS_URL" -c 50 -d 30s -r 20 \
      --max-p99 50ms --max-error-rate 0.01 \
      --no-progress --format markdown -o report.md
- name: Job summary
  if: always()
  run: cat report.md >> "$GITHUB_STEP_SUMMARY"
```

The report is written before wsblast exits, also when the gate fails. [docs/ci.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/ci.md) has full GitHub Actions, GitLab CI and shell recipes, including JSON artifacts and baseline comparison with `--compare`.

## How wsblast measures

- Round-trip time runs from a send to its reply, only in echo mode, on a monotonic clock. `--match` decides which frame is the reply: the first data frame (`first`, the default), a byte-identical copy (`exact`), or a JSON field equal to the request's `{{seq}}` (`json:/path`).
- `--duration` is wall-clock time including the `--ramp-rate` ramp.
- The error rate is failed operations over attempted operations, and a failed request is counted once.
- A latency threshold fails when no round trip was measured, so an unreachable target cannot pass a latency gate.
- Workers reconnect with backoff after a timeout, I/O error or server close unless `--no-reconnect` is set.

[METHODOLOGY.md](https://github.com/jephter-olamiposi/wsblast/blob/main/METHODOLOGY.md) defines every number in the report and lists the known limitations.

## Feature flags

| Feature | Default | Adds |
| :--- | :--- | :--- |
| `scenario` | on | `--scenario` and `--phase`: multi-phase YAML scenarios with per-phase targets, actions, think time, per-phase and scenario-level SLOs, and staged load per phase ([docs/scenarios.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/scenarios.md)) |
| `compare` | on | `--compare`, `--compare-threshold`, `--compare-window` and `--fail-on-regression` for two JSON reports, including time-series stability metrics |
| `auth` | on | `--auth-type static` (bearer token) and `--auth-type apikey`, with the credential given directly or printed by `--auth-token-cmd` (optionally refreshed with `--auth-token-refresh`) |
| `graphql-ws` | on | `--mode graphql-ws` for `graphql-transport-ws` and legacy `graphql-ws` subscriptions |
| `prometheus` | on | `--prometheus-listen`: a live `/metrics` endpoint ([docs/export.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/export.md)) |
| `capture` | on | `--capture` and `--capture-limit`: a JSONL record of every frame and connection event of a run |
| `mock` | on | `wsblast mock`: a local test server with delay, jitter, stalls, rejected upgrades, server closes and capture replay ([docs/mock.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/mock.md)) |
| `distributed` | on | `wsblast agent` and `--agents`: one run's load generated by several machines, with exactly merged results ([docs/distributed.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/distributed.md)) |
| `otel` | off | `--otel-endpoint`, `--otel-interval`, `--otel-service-name`: OTLP/HTTP export to an OpenTelemetry collector |

Every feature except `otel` is on by default, so `cargo install wsblast` and the release binaries include them; `--features full` is an alias for that set. `otel` pulls in the OpenTelemetry SDK and an HTTP client, so it is opt-in: `cargo install wsblast --features otel`. Library users can opt out with `default-features = false` and enable only what they need. Without `auth`, credentials can still be sent with `-H "Authorization: Bearer $TOKEN"`.

## Command-line reference

The most used options; `wsblast --help` lists them all.

| Option | Default | Description |
| :--- | :--- | :--- |
| `URL` or `-u, --url` | | Target, `ws://` or `wss://` |
| `-c, --connections` | `50` | Concurrent connections |
| `-d, --duration` | `10s` | Total run time, including the ramp |
| `--ramp-rate` | `0` | Connections opened per second (`0` opens all at once) |
| `--stages` | | Load profile instead of `-c`, `-d` and `--ramp-rate`: `DURATION:CONNECTIONS` stages such as `30s:100,2m:100,30s:0` |
| `-r, --rate` | `0` | Messages per second per connection (`0` is unthrottled) |
| `-n, --requests` | | Stop after this many messages in total |
| `-p, --payload` / `--payload-file` | JSON template | Payload; `{{seq}}`, `{{timestamp}}` and `{{worker_id}}` are filled in per frame |
| `--binary` | | Send binary frames |
| `-H, --header` | | Handshake header, repeatable |
| `--mode` | `echo` | `echo`, `stream` or `listen` |
| `--match` | `first` | Reply matching in echo mode: `first`, `exact` or `json:/path` |
| `--max-p50`, `--max-p95`, `--max-p99`, `--max-p999` | | Latency thresholds (echo mode only): response time with `--rate`, round-trip time without |
| `--max-error-rate` | | Error rate threshold, a fraction such as `0.01` |
| `--min-throughput` | | Messages per second threshold |
| `--format` / `-o, --output` | `text` | `text`, `json`, `markdown` or `html` (a self-contained page with charts), optionally written to a file |
| `--tui` / `--no-progress` | | Live dashboard, or no progress bar for CI logs |
| `--compare BASE CURRENT` | | Compare two JSON reports; `--fail-on-regression` exits with 2 on a regression beyond `--compare-threshold` |
| `--compare-window START..END` | whole run | Compute the `series.*` comparison metrics only over START..END of each run (`10s..50s`, `10s..`, `..50s`) |
| `--auth-token-cmd` | | Command whose trimmed stdout is the token (`--auth-type static`) or API key (`--auth-type apikey`), run before the run starts |
| `--auth-token-cmd-timeout` | `30s` | Time allowed for each run of `--auth-token-cmd` |
| `--auth-token-refresh` | | Re-run `--auth-token-cmd` this often during the run; new handshakes and reconnects use the latest token |
| `--capture FILE` | | Record every data frame and connection event to FILE as JSON lines |
| `--capture-limit N` | `100000` | Most records `--capture` writes; later ones are dropped and counted (`0` = unlimited) |
| `--prometheus-listen` | | Serve live metrics at `http://ADDR/metrics` for Prometheus while the run lasts, e.g. `127.0.0.1:9464` |
| `--otel-endpoint` | | Push live metrics over OTLP/HTTP to this collector base URL, e.g. `http://localhost:4318` (`otel` feature) |
| `--otel-interval` | `10s` | Time between OTLP exports; the final values are also exported when the run ends |
| `--otel-service-name` | `$OTEL_SERVICE_NAME`, else `wsblast` | `service.name` resource attribute of the OTLP export |
| `--agents LIST` | | Generate the load on these agents instead of locally: comma-separated `HOST:PORT`, `ws://HOST:PORT` or `wss://HOST:PORT` |
| `--agent-token TOKEN` | `$WSBLAST_AGENT_TOKEN` | Token the agents were started with |
| `--agent-timeout DURATION` | `30s` | Time the agents get to become ready, and to send their results after the run |
| `--agent-ca PEM` | | Extra CA certificates to trust for `wss://` agents |
| `wsblast agent --listen ADDR [--token T] [--tls-cert PEM --tls-key PEM]` | | Run a load agent for a coordinator, one test at a time |
| `wsblast render REPORT.json [--format html\|markdown\|text] [-o FILE]` | `html` | Render a saved JSON report without running a test ([docs/ci.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/ci.md#8-html-report)) |
| `wsblast mock [--listen ADDR]` | `127.0.0.1:9001` | Run the mock test server (`--delay`, `--jitter`, `--close-after`, `--reject-ratio`, `--stall-every`/`--stall-for`, `--replay`) |

## Exit codes

| Code | Meaning |
| :--- | :--- |
| `0` | The run completed, every SLO passed and at least one connection was established. |
| `1` | Invalid arguments, a configuration or runtime error, or every connection failed. |
| `2` | An SLO failed, or `--fail-on-regression` found a regression. |

## Library

The engine is also a library: build a configuration with `LoadTestConfig::builder`, run it with `Runner` and gate with `evaluate_slos`. See the [API documentation](https://docs.rs/wsblast).

## Features removed in 0.2.0

0.2.0 kept only features that worked end to end. 0.3.0 brings back Prometheus and OpenTelemetry export, frame capture (as JSON Lines) and the mock server, rebuilt and tested. Distributed execution is back in a new design without gRPC (see [docs/distributed.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/distributed.md)). Chaos injection, JWT and OAuth2 authentication (use `--auth-token-cmd` with a command that mints the token instead) and the `compression` placeholder are still out. Their last implementation is on the [`archive/v0.1-full-features`](https://github.com/jephter-olamiposi/wsblast/tree/archive/v0.1-full-features) branch, and the [changelog](https://github.com/jephter-olamiposi/wsblast/blob/main/CHANGELOG.md) has migration notes.

## MSRV policy

The minimum supported Rust version is 1.88. It is declared as `rust-version` in `Cargo.toml`, and CI checks that the library and the binary build with it, with the default features and with `full`. Raising it is done in a minor release and noted in the changelog, never in a patch release.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/jephter-olamiposi/wsblast/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/jephter-olamiposi/wsblast/blob/main/LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in wsblast by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
