# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

v0.2.0 ships a smaller crate in which every feature works end to end. Several features that were developed on `main` after 0.1.1 were removed before release; their code is preserved on the [`archive/v0.1-full-features`](https://github.com/jephter-olamiposi/wsblast/tree/archive/v0.1-full-features) branch, and several of them will return, rebuilt, in later releases.

### Migrating from 0.1

- Every 0.1.1 flag still works. Two behaviours changed: `--duration` now includes the `--ramp-rate` ramp, and workers reconnect after a timeout, I/O error or server close. Pass `--no-reconnect` to end a worker on its first connection loss, as 0.1 did.
- New capabilities are opt-in Cargo features (`scenario`, `auth`, `graphql-ws`, `compare`); `cargo install wsblast --features full` installs all of them.
- Library users: `WorkerMetrics::new` and `AggregatedMetrics::merge` now return `Result`, `LoadTestConfig` has new fields (`no_reconnect`, `reply_match`, plus `auth` and `graphql_ws` behind their features), `ConfigError::ZeroRate`, `WsBlastError::SloBreach` and `WorkerError::ChannelClosed` are gone, and a zero `--requests` limit is reported as `ConfigError::InvalidOption` instead of `ConfigError::InvalidThreshold`.
- If you built from `main` after 0.1.1 and enabled a removed feature (`distributed`, `otel`, `prometheus`, `capture`, `chaos`, `mock`, `compression`, `tracing-subscriber`), drop it from `--features` and remove its flags. Build the archive branch if you still need one of them.

### Added
- Scenario DSL with YAML multi-phase definitions, per-phase targets, actions (`Send`, `Wait`, `Ping`, `Close`), think time, and CLI URL precedence.
- Handshake authentication (`auth` feature) for static bearer tokens and API keys (`--auth-type static|apikey`).
- Apollo GraphQL WS subscription mode (`--mode graphql-ws`) with `connection_init` / `subscribe` lifecycle handling and legacy `graphql-ws` (`start` / `data`) support.
- Baseline comparison (`--compare`) with regression thresholds and `--fail-on-regression`.
- `--match <first|exact|json:POINTER>` selects how echo mode recognises the reply to a request so unsolicited server pushes no longer skew RTT; skipped frames are reported as `total_unsolicited_frames`.

### Changed
- Metric constructors and aggregation are fallible (`WorkerMetrics::new`, `AggregatedMetrics::merge`); runner and executor APIs propagate metrics errors.
- Configuration errors that are not thresholds have accurate variants: `ConfigError::InvalidAuth` for authentication options, and `ConfigError::InvalidOption` for a zero `--requests` limit, a missing `--graphql-ws-operation`, GraphQL WS variables or extensions that are not a JSON object, and `--compare` without exactly two files. `ConfigError::InvalidThreshold` now only covers `--max-error-rate` and `--compare-threshold`.
- README documents feature flags, scenario targets, authentication lifetime, GraphQL WS and comparison workflows.
- The minimum supported Rust version is now 1.88 (`rust-version` in `Cargo.toml`), for the default build and for `--features full`. The locked dependency graph needs it: `comfy-table` 7.2 uses let chains, which Rust 1.88 stabilised, so the previously declared 1.85 no longer builds the crate. The Docker image and the GitLab CI example now use `rust:1.88`.

### Removed
- Distributed coordinator/worker execution over gRPC (`distributed` feature, `--distributed-mode` and the coordinator and worker flags), with its protobuf definitions and build script. Building the crate no longer needs `protoc`.
- OpenTelemetry OTLP export (`otel` feature, `--otel*` flags) and the Prometheus `/metrics` endpoint (`prometheus` feature, `--prometheus*` flags).
- Frame capture to PCAP-NG and JSON Lines (`capture` feature, `--capture*` flags).
- Chaos injection of latency, frame loss, corruption and forced closes (`chaos` feature, `--chaos*` flags).
- The record/replay mock server (`mock` feature, `wsblast mock` subcommands).
- JWT and OAuth2 authentication (`--auth-type jwt|oauth2`, `--auth-jwt-*` and `--auth-oauth2-*` flags), the unused `Authenticator` trait and authenticator structs, and `AuthConfig::Custom`.
- The `compression` feature and `--compression` flags, which never compressed anything because the transport does not implement RFC 7692.
- The `tracing-subscriber` feature.
- `ConfigError::ZeroRate`, `WsBlastError::SloBreach` and `WorkerError::ChannelClosed`, which were never constructed.
- The unused `base64`, `rand` and `sha2` dependencies.
- `--compression` now fails fast with `CompressionUnsupported` instead of silently running uncompressed.
- README documents feature flags, scenario targets, auth lifetime behavior, observability, chaos, capture, GraphQL, distributed limits, and mock/comparison workflows.
- Command-line usage errors (unknown flag, missing value, conflicting flags) now exit with code 1 instead of clap's default 2, so exit code 2 always means an SLO breach or a `--fail-on-regression` regression; `--help` and `--version` still exit with 0.

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
- Remaining runtime `.unwrap()`/`.expect()` calls in metric initialization were replaced with typed errors.
- GraphQL WS now emits protocol-correct wire names (`connection_init`, `subscribe`, `next`, `complete`) instead of Rust variant names, and handles modern keepalives.
- Duration flags no longer panic on negative, NaN, infinite or overflowing values (`-d infs`, `--duration=-1s`); they fail with `InvalidDuration`, and the parser now accepts `ns` and fractional values for every unit (`1.5ms`, `0.5us`).
- Repeated `-H` flags with the same header name now send every value instead of keeping only the last one; a `-H` header still replaces a default handshake header of the same name (such as `Host`).
- Comparison reports print improvements with their real sign (throughput 100 to 150 shows `+50.00%`, not `-50.00%`); `improvements[].pct_change` in comparison JSON is now the signed change instead of its absolute value.
- `install.sh` downloads the release assets that are actually published (`wsblast-<target>.tar.gz`), verifies their `.sha256` checksum before installing, and fails with a clear message when the GitHub API lookup fails instead of assuming `v0.1.0`; releases now build `x86_64-apple-darwin` on `macos-latest` and add `aarch64-unknown-linux-gnu`.
- The Docker image builds again: the Dockerfile copies the whole crate (the manifest's `[[bench]]` target was missing), installs the OpenSSL headers `native-tls` needs, and builds only `--bin wsblast`; `.dockerignore` now excludes build output, VCS, CI, assets and local agent files.
- `error_rate` no longer counts a failed echo request twice: it is now failed operations over attempted operations, so a run where every request times out reports 1.0 instead of 0.5; errors that end an already-sent message (reply timeout, read error or close while the reply is awaited) are classified when recorded and add no second attempt.
- A scenario phase without a `target` no longer silently reuses the first phase's endpoint when neither `--url` nor a scenario-level `target` is set: validation rejects it and names the phase, the CLI > phase > scenario precedence is resolved for every phase, and text and Markdown reports list every executed phase's target (JSON adds an optional `phase_targets` array).
- Scenario files now reject unknown keys at the top level and in `metadata`, phases and actions, naming the misspelled key (`conections:` used to fall back silently to a default), and `Close` actions only accept the close codes an endpoint may send (1000-1003, 1007-1014, 3000-4999) instead of also accepting the reserved 1016-2999 range.
- Connection slots the ramp never reached before `--duration` ended are no longer reported as failed with no error recorded; they are counted in the new `total_connections_not_started` field, shown in text and Markdown reports when non-zero.

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
