# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
