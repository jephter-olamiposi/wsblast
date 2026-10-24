# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Scenario DSL with YAML multi-phase definitions, per-phase targets, actions (`Send`, `Wait`, `Ping`, `Close`), think time, and CLI URL precedence.
- OpenTelemetry OTLP export and Prometheus `/metrics` endpoint, opt-in via `--otel` and `--prometheus`.
- Handshake authentication helpers for static bearer tokens, API keys, HS256 JWTs, and OAuth2 client-credentials exchanges.
- Apollo GraphQL WS subscription mode (`--mode graphql-ws`) with `connection_init` / `subscribe` lifecycle handling and legacy `graphql-ws` (`start` / `data`) support.
- Frame capture to PCAP-NG and JSON Lines, shared across workers and scenario phases.
- Non-blocking chaos injection for latency, frame loss, corruption, and forced connection closes.
- Distributed coordinator/worker execution over gRPC with work splitting, failed-range requeueing, staged metrics, histogram aggregation, and SLO gating.
- Baseline comparison (`--compare`) with regression thresholds and `--fail-on-regression`.
- Record/replay mock server subcommands (`wsblast mock record`, `wsblast mock replay`) with delay and error injection.
- Wire-config propagation of GraphQL and chaos settings in distributed mode.

### Changed
- Metric constructors and aggregation are fallible (`WorkerMetrics::new`, `AggregatedMetrics::merge`); runner and executor APIs propagate metrics errors.
- `--compression` now fails fast with `CompressionUnsupported` instead of silently running uncompressed.
- README documents feature flags, scenario targets, auth lifetime behavior, observability, chaos, capture, GraphQL, distributed limits, and mock/comparison workflows.

### Fixed
- GraphQL WS mode no longer records an `UnexpectedClose` error when the run ends before the server completes the subscription, and now sends the stop/complete message before the Close frame.
- Echo mode no longer counts a server Close frame as a received reply with an RTT sample; the Close ends the session (and triggers a reconnect unless `--no-reconnect` is set).
- A message timeout, I/O error or server close no longer ends a worker for the rest of the run: workers reconnect with exponential backoff (100ms doubling to 5s) and report `total_reconnects`; `--no-reconnect` restores the old behaviour.
- Echo-mode shutdown no longer waits up to `--message-timeout` for an in-flight reply: the receive wait races cancellation, and replies arriving after the test window are no longer recorded.
- Echo mode with `--ping-interval` no longer throttles unpaced load to one message per keepalive tick, and keepalive ticks no longer add extra data sends when `--rate` is set.
- `--duration` is now the total wall-clock run including the `--ramp-rate` ramp, so runs no longer last ramp plus duration and throughput is no longer diluted by ramp time; a warning is printed when the ramp cannot finish within the duration.
- Ctrl+C or SIGTERM during a scenario now stops the whole scenario and prints the report instead of only ending the current phase and starting the next one.
- Ctrl+C and SIGTERM during a `--ramp-rate` connection ramp now stop the run and print the report instead of killing the process; `Runner::run` and `Runner::run_and_collect_worker_metrics` share one code path.
- Latency SLOs (`--max-p50/p95/p99/p999`) no longer pass when no round trip was measured: the check fails with "no RTT samples", and latency SLOs are rejected at startup outside `--mode echo`.
- Single-phase runs no longer execute the load test twice.
- Frame capture now writes output at the end of a run and supports unlimited capture (`--capture-max-frames 0`).
- Capture is shared across scenario phases instead of creating per-phase buffers.
- Chaos latency no longer blocks the Tokio runtime; loss percentages use the documented 0–100 scale.
- Mock replay consumes only recorded server-to-client frames.
- Distributed mode rejects unsupported scenario/capture/TUI/observability combinations instead of ignoring them.
- Remaining runtime `.unwrap()`/`.expect()` calls in metric initialization were replaced with typed errors.
- GraphQL WS now emits protocol-correct wire names (`connection_init`, `subscribe`, `next`, `complete`) instead of Rust variant names, and handles modern keepalives.
- Prometheus counters now export with a single `_total` suffix instead of `_total_total`.
- Duration flags no longer panic on negative, NaN, infinite or overflowing values (`-d infs`, `--duration=-1s`); they fail with `InvalidDuration`, and the parser now accepts `ns` and fractional values for every unit (`1.5ms`, `0.5us`).
- Repeated `-H` flags with the same header name now send every value instead of keeping only the last one; a `-H` header still replaces a default handshake header of the same name (such as `Host`).
- Comparison reports print improvements with their real sign (throughput 100 to 150 shows `+50.00%`, not `-50.00%`); `improvements[].pct_change` in comparison JSON is now the signed change instead of its absolute value.
- `cargo clippy --all-targets --features full -- -D warnings` passes again: the distributed worker boxes the `LoadTestConfig` carried by a work claim (`clippy::large_enum_variant`).
- `install.sh` downloads the release assets that are actually published (`wsblast-<target>.tar.gz`), verifies their `.sha256` checksum before installing, and fails with a clear message when the GitHub API lookup fails instead of assuming `v0.1.0`; releases now build `x86_64-apple-darwin` on `macos-latest` and add `aarch64-unknown-linux-gnu`.
- The Docker image builds again: the Dockerfile copies the whole crate (the manifest's `[[bench]]` target and build script were missing), installs the OpenSSL headers `native-tls` needs, and builds only `--bin wsblast`; `.dockerignore` now excludes build output, VCS, CI, assets and local agent files.

## [0.1.1] - 2026-09-04

### Changed
- Streamlined documentation to adopt concise systems engineering documentation style.
- Updated crate packaging manifest with explicit asset exclusions.

## [0.1.0] - 2026-09-04

### Added
- **Core Engine & Architecture:**
  - High-performance, low-contention WebSocket load generation engine built with Tokio and `tokio-tungstenite`.
  - Multi-mode support: `echo` (request-response with microsecond RTT latency tracking), `stream` (unidirectional high-throughput pipe), and `listen` (passive subscriber).
  - Pacing and rate limiting per connection using `tokio::time::interval` with `MissedTickBehavior::Skip` to prevent burst storms.
  - Connection ramp pacing (`--ramp-rate`) to avoid kernel SYN queue saturation and thundering-herd issues during high-concurrency connection ramp-up.
  - Request limit enforcement (`-n, --requests`) allowing benchmarking by message count in addition to or instead of time-based duration.
  - Granular failure taxonomy mapping socket drops into DNS resolution, TCP connect, TLS handshake, HTTP 101 upgrade rejection (`4xx`/`5xx`), protocol violation, timeout, and unexpected close.
- **Latency & Telemetry:**
  - Task-local `HdrHistogram` recording (1 µs to 60s at 3 significant figures) ensuring zero mutex contention across concurrent workers.
  - Lock-free live atomic counters (`LiveMetrics`) providing real-time telemetry updates.
  - Microsecond-accurate monotonic timing via hardware `Instant` clocking.
- **Terminal UI & Reporting:**
  - Interactive live terminal dashboard (`--tui`) powered by `ratatui` with real-time throughput sparklines, connection meters, and active error monitors.
  - Automated Service Level Objective (SLO) gating for latency percentiles (`p50`, `p95`, `p99`, `p99.9`), maximum error rate, and minimum throughput.
  - Multi-format report emission: human-readable UTF-8 terminal tables, schema v1.0 JSON (`--format json`), and GitHub Flavored Markdown (`--format markdown`) for PR summaries and CI artifacts.
- **Transport & Security:**
  - Secure TLS WebSocket (`wss://`) support backed by `rustls` 0.23 with pre-installed `ring` crypto provider and system native root certificates.
  - Custom HTTP handshake headers (`-H, --header`) and subprotocol negotiation (`--subprotocol`).
  - RFC 6455 Ping/Pong keepalive handling across all worker execution modes.
- **Deployment & Tooling:**
  - Multi-stage minimal production `Dockerfile` with non-privileged execution (`USER 1000:1000`).
  - POSIX-compliant one-line installer script (`install.sh`) supporting macOS and Linux across x86_64 and arm64.
  - Automated cross-platform GitHub Actions release workflow producing standalone binaries for Linux, macOS (Apple Silicon & Intel), and Windows.
  - Standalone high-concurrency echo benchmark server (`examples/echo_server.rs`) with real-time throughput telemetry logging.
  - Comprehensive Criterion microbenchmark suite (`benches/metrics_benchmark.rs`).
