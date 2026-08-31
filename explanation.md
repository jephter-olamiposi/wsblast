# `wsblast` Architecture, System Design & Technical Glossary

This document is a comprehensive, senior-level guide to understanding every subsystem, data structure, design decision, and technical concept implemented in `wsblast`. 

---

## 1. Executive Summary & Mental Model

### What is `wsblast`?
`wsblast` is a Rust-native load-testing tool specifically designed for WebSocket endpoints (`ws://` and `wss://`). 

### Why is WebSocket load testing different from HTTP load testing?
- **HTTP (Stateless / Request-Response):** A client sends an HTTP request, receives an HTTP response, and may close or reuse the TCP connection for another discrete request.
- **WebSocket (Stateful / Bidirectional / Persistent):** A client establishes a long-lived TCP connection, upgrades it via an HTTP `101 Switching Protocols` handshake, and keeps the connection open. Both client and server can asynchronously send text, binary, or control frames (Ping/Pong) at any time.

Because connections persist, a WebSocket load tester must measure two distinct latency dimensions:
1. **Handshake Connection Latency:** Time to resolve DNS, establish TCP, complete TLS negotiation, and exchange HTTP Upgrade headers.
2. **Message Round-Trip Time (RTT):** Monotonic duration between dispatching a frame and receiving its corresponding response frame over the active stream.

---

## 2. Deep Glossary of Senior Backend Concepts & Terminology

### 1. RTT (Round-Trip Time)
The duration (measured in microseconds or milliseconds) from the instant a client writes a message to the socket until the client receives the server's reply.

### 2. Monotonic Clock vs. Wall Clock
- **Wall Clock (`SystemTime`):** Measures calendar time (e.g. `2026-08-31 01:30:00.123`). It can jump forward or backward due to NTP time synchronisation or leap seconds. **Never use wall clock for latency measurement.**
- **Monotonic Clock (`Instant`):** A monotonically increasing hardware timer provided by the operating system kernel (such as `mach_absolute_time` on macOS or `CLOCK_MONOTONIC_RAW` on Linux). It never jumps backward, making it the only accurate way to calculate nanosecond/microsecond elapsed time.

### 3. Percentiles (`p50`, `p75`, `p90`, `p95`, `p99`, `p99.9`, `p99.99`)
In high-throughput systems, averages (**mean**) hide severe degradation (the "flaw of averages"). Percentiles measure latency distribution across the population:
- **`p50` (Median):** 50% of requests were faster than this value. Represents typical user experience.
- **`p95`:** 95% of requests were faster than this value; 5% were slower.
- **`p99`:** 1 in 100 requests experienced latency worse than this threshold. Critical for real-time services.
- **`p99.9` (Three Nines) & `p99.99` (Four Nines):** Measures tail latency spikes caused by garbage collection, thread pool starvation, TCP head-of-line blocking, or kernel context switching.

### 4. HDR Histogram (High Dynamic Range Histogram)
`hdrhistogram` is a data structure created by Gil Tene. It tracks integer values across multiple orders of magnitude with a configurable, constant percentage of error (e.g. 3 significant digits) using fixed memory buffers.
- **Why we use it:** Instead of storing millions of raw `u64` latency numbers in a `Vec<u64>` (which causes massive memory allocation and expensive sorting), an HDR histogram groups values into logarithmic buckets in O(1) time with zero dynamic memory allocation on the hot path.

### 5. Lock-Free Atomic Telemetry vs. Mutex Contention
When hundreds of concurrent worker tasks update counters simultaneously:
- A shared `Mutex<Metrics>` causes heavy **lock contention**, forcing OS threads to sleep and context switch, which artificially distorts benchmark results.
- `wsblast` eliminates this by giving each worker its own **task-local `WorkerMetrics`** (local HDR histograms) and using lock-free **`AtomicU64`** with relaxed memory ordering (`Ordering::Relaxed`) for live progress meters.

### 6. Pacing & `MissedTickBehavior::Skip`
When load testing at a fixed rate (e.g. `-r 100` = 100 messages/sec per connection), `tokio::time::interval` is used.
- If the network experiences a temporary 200ms latency spike, multiple interval ticks are missed.
- `MissedTickBehavior::Burst` would fire all delayed messages immediately, causing an artificial "burst storm".
- `MissedTickBehavior::Skip` discards missed ticks and maintains the steady-state target rate.

### 7. WebSocket Handshake (RFC 6455)
A client initiates a WebSocket connection via standard HTTP GET with headers:
- `Upgrade: websocket`
- `Connection: Upgrade`
- `Sec-WebSocket-Key: <base64-random-key>`
- `Sec-WebSocket-Version: 13`
The server validates the key and replies with `HTTP/1.1 101 Switching Protocols` before raw WebSocket binary/text frames can flow.

### 8. Error Taxonomy
Rather than reporting generic "connection error", `wsblast` categorizes failures into actionable diagnostic buckets:
- `DnsResolution`: Host lookup failed.
- `TcpConnect`: TCP SYN timed out or connection refused.
- `TlsHandshake`: TLS certificate or cipher mismatch.
- `HttpUpgradeRejected`: Server rejected handshake with HTTP 401, 403, 404, or 500.
- `ProtocolError`: Framing, UTF-8, or opcode violation.
- `WriteError` / `ReadError`: Socket pipe reset or closed.
- `Timeout`: Per-message or handshake budget exceeded.
- `UnexpectedClose`: Remote closed connection without a clean close code.

### 9. Service Level Objective (SLO) Gating & Exit Codes
- **SLO (Service Level Objective):** Target reliability metric (e.g. "p99 RTT must be $\le 50\text{ms}$" and "Error rate must be $\le 0.1\%$").
- **Exit Code 0:** All tests finished and all SLO thresholds were met (CI pipeline passes).
- **Exit Code 1:** Runtime error (invalid URL, syntax error, fatal I/O failure).
- **Exit Code 2:** SLO threshold breached (CI pipeline fails automatically).

---

## 3. End-to-End Subsystem Architecture

```
                                  CLI INVOCATION
                                        │
                                        ▼
                   ┌──────────────────────────────────────────┐
                   │               src/main.rs                │
                   │  - Clap CLI Parser                       │
                   │  - Signal Trap (SIGINT / Ctrl+C)         │
                   └────────────────────┬─────────────────────┘
                                        │
                                        ▼
                   ┌──────────────────────────────────────────┐
                   │              src/config.rs               │
                   │  - Parse URL, Headers, Durations         │
                   │  - Validate Concurrency & SLO Budgets    │
                   └────────────────────┬─────────────────────┘
                                        │
                                        ▼
                   ┌──────────────────────────────────────────┐
                   │              src/runner.rs               │
                   │  - Spawns N Tokio Workers                │
                   │  - Manages Live Progress / TUI           │
                   │  - Coordinates CancellationToken         │
                   └───────┬──────────────────────────┬───────┘
                           │                          │
              Worker 0     │             Worker N-1   │
              ┌────────────▼─────────┐   ┌────────────▼─────────┐
              │    src/worker.rs     │   │    src/worker.rs     │
              │ - TCP/TLS/WS Connect │   │ - TCP/TLS/WS Connect │
              │ - Monotonic RTT loop │   │ - Monotonic RTT loop │
              │ - Task-local HDR Hist│   │ - Task-local HDR Hist│
              └────────────┬─────────┘   └────────────┬─────────┘
                           │                          │
                           └────────────┬─────────────┘
                                        │
                                        ▼
                   ┌──────────────────────────────────────────┐
                   │             src/metrics.rs               │
                   │  - Merge Task Histograms (p50/p95/p99)   │
                   │  - Aggregate Byte & Message Counters     │
                   │  - Compute Error Taxonomy Map            │
                   └────────────────────┬─────────────────────┘
                                        │
                                        ▼
                   ┌──────────────────────────────────────────┐
                   │              src/report.rs               │
                   │  - Evaluate SLO Pass/Fail Rules          │
                   │  - Emit Terminal Table / JSON / Markdown │
                   └────────────────────┬─────────────────────┘
                                        │
                                        ▼
                                 EXIT CODE (0 or 2)
```

---

## 4. Module-by-Module Code Breakdown

### `src/error.rs`
- **Purpose:** Centralized, strongly typed error hierarchy using `thiserror`.
- **Key Types:**
  - `WsBlastError`: Top-level enum (`Config`, `Worker`, `Io`, `Serialization`, `SloBreach`).
  - `ErrorCategory`: Normalized enum for taxonomy reporting.
  - `From<&WorkerError> for ErrorCategory`: Maps complex errors into taxonomy categories.

### `src/cli.rs`
- **Purpose:** Command-line argument parser built with `clap(derive)`.
- **Key Types:**
  - `Cli`: Struct mapping flags (`--url`, `-c`, `-d`, `-r`, `-p`, `--max-p99`, `--tui`, `--format`).
  - `TestMode`: Enum (`Echo`, `Stream`, `Listen`).
  - `OutputFormat`: Enum (`Text`, `Json`, `Markdown`).

### `src/config.rs`
- **Purpose:** Validates and normalizes raw CLI input into immutable configuration.
- **Key Functions:**
  - `LoadTestConfig::from_cli`: Validates scheme (`ws://`/`wss://`), concurrency $> 0$, headers, and SLO ranges.
  - `parse_human_duration`: Converts strings like `"500ms"`, `"10s"`, `"2m"` into `std::time::Duration`.

### `src/metrics.rs`
- **Purpose:** High-resolution telemetry and HDR histogram statistics.
- **Key Types:**
  - `LiveMetrics`: Thread-safe atomic counters (`active_connections`, `messages_sent`, `messages_received`, `total_errors`) for real-time meters.
  - `WorkerMetrics`: Task-local struct holding two `HdrHistogram` instances (one for connection handshakes, one for message RTT) bounded from $1\,\mu\text{s}$ to $60\,\text{s}$ with 3 significant figures.
  - `LatencyStats`: Computed percentile values (`min`, `mean`, `max`, `std_dev`, `p50`, `p75`, `p90`, `p95`, `p99`, `p99.9`, `p99.99`).
  - `AggregatedMetrics::merge`: Merges hundreds of worker histograms into a single consolidated report.

### `src/worker.rs`
- **Purpose:** Asynchronous WebSocket client lifecycle.
- **Key Execution Steps:**
  1. `build_handshake_request`: Uses `into_client_request()` to inject RFC 6455 headers, plus custom user headers and subprotocols.
  2. `tokio_tungstenite::connect_async`: Performs TCP + TLS + HTTP Upgrade within `connect_timeout`.
  3. `run_echo_loop`: Loops until test duration expires or cancellation token fires. Records nanosecond-resolution RTT with `start_time.elapsed()`.
  4. `render_payload`: Performs dynamic templating for `{{timestamp}}`, `{{worker_id}}`, and `{{seq}}`.
  5. `categorize_tungstenite_error`: Classifies I/O, TLS, and protocol errors into `ErrorCategory`.

### `src/runner.rs`
- **Purpose:** Worker pool coordinator and signal handler.
- **Key Execution Steps:**
  1. Spawns $N$ independent asynchronous worker tasks onto Tokio.
  2. Spawns duration timer and `tokio::signal::ctrl_c()` listener attached to a `CancellationToken`.
  3. Displays live `indicatif` progress bar in interactive text mode.
  4. Collects worker join handles and passes metric objects to `AggregatedMetrics::merge`.

### `src/report.rs`
- **Purpose:** Multi-format output generation and SLO gate validation.
- **Key Functions:**
  - `evaluate_slos`: Compares actual percentiles against configured target budgets.
  - `render_text_report`: Generates styled ASCII/Unicode tables via `comfy-table`.
  - `render_json_report`: Emits schema-versioned JSON (`schema_version: "1.0.0"`).
  - `render_markdown_report`: Formats markdown table for CI pull request summaries.

### `src/tui/` (`mod.rs`, `app.rs`, `widgets.rs`)
- **Purpose:** Interactive terminal UI powered by `ratatui` and `crossterm`.
- **Key Features:**
  - Live progress gauge.
  - Metric cards: Active Connections, Throughput (msg/s), Data Transferred (MB), Error Count.
  - Real-time rolling throughput sparkline (`history: VecDeque<u64>`).
  - Safe RAII terminal raw mode restoration on exit.

---

## 5. How to Run, Test & Validate Locally

### 1. Run the Built-In Local Echo Server
```bash
cargo run --example echo_server
```

### 2. Standard Benchmark Run (Text Output)
```bash
cargo run -- ws://127.0.0.1:9001 -c 50 -d 5s
```

### 3. CI Mode with SLO Gating (JSON Output)
```bash
# Returns exit code 0 if p99 <= 50ms and error rate <= 1%
cargo run -- ws://127.0.0.1:9001 -c 20 -d 5s --max-p99 50ms --max-error-rate 0.01 --format json -o report.json
echo "Exit code: $?"
```

### 4. Test SLO Failure Gating (Exit Code 2)
```bash
# Impossibly tight 1 nanosecond budget -> fails gate and exits with code 2
cargo run -- ws://127.0.0.1:9001 -c 10 -d 2s --max-p99 1ns
echo "Exit code: $?"  # Outputs 2
```

### 5. Interactive Live Dashboard (TUI)
```bash
cargo run -- ws://127.0.0.1:9001 -c 100 -d 30s --tui
```

### 6. Run the Automated Test Suite
```bash
cargo test
```
