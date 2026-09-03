# Codex Instructions - wsblast

`wsblast` is a high-performance, low-contention WebSocket load testing CLI written in Rust (2024 edition, Rust 1.85+). It delivers microsecond-accurate percentile latency distributions (`p50`, `p90`, `p95`, `p99`, `p99.9`), zero-allocation hot-path payload dispatch, granular failure taxonomy, live terminal dashboards (TUI), and automated Service Level Objective (SLO) gating.

## Skill Folder

Reusable agent skills live in [.codex/skills](.codex/skills).

Before planning and implementing changes, read and follow relevant skills:
- Senior Rust Workflow: `.codex/skills/senior-backend-workflow/SKILL.md`
- Production Code Only: `.codex/skills/production-code-only/SKILL.md`
- Comment Cleanup & Invariant Hygiene: `.codex/skills/comment-cleanup/SKILL.md`
- Performance & Concurrency: `.codex/skills/performance-concurrency/SKILL.md`
- Rust Code Quality: `.codex/skills/rust-code-quality/SKILL.md`

## Architecture & Module Boundaries

- `src/cli.rs`: Command-line interface definitions and flag declarations via `clap(derive)`. Request/response orchestration only.
- `src/config.rs`: Validated immutable configuration (`LoadTestConfig`), human-readable duration parser (`parse_human_duration`), and `PayloadConfig` classification (`StaticText`, `DynamicText`, `Binary`).
- `src/error.rs`: Strongly-typed error hierarchy (`WsBlastError`) and normalized `ErrorCategory` taxonomy (DNS, TCP Connect, TLS Handshake, HTTP 101 Upgrade Rejection, Protocol, Timeout, Socket Reset).
- `src/metrics.rs`: Task-local `HdrHistogram` instances pre-allocated for 6 orders of magnitude ($1\,\mu\text{s}$ to $60\,\text{s}$) at 3 significant figures, and lock-free atomic telemetry counters (`AtomicU64` with `Ordering::Relaxed`).
- `src/worker.rs`: Asynchronous WebSocket worker loop using Tokio, rate-limiting pacing (`MissedTickBehavior::Skip`), nanosecond RTT timing (`Instant`), zero-allocation frame dispatch, and RFC 6455 close handshakes.
- `src/runner.rs`: Worker pool orchestration, connection ramp pacing (`--ramp-rate`), OS file descriptor (`RLIMIT_NOFILE`) checks, and histogram merges.
- `src/report.rs`: Output formatting (styled terminal tables, schema v1.0 JSON, Markdown) and automated SLO evaluation.
- `src/tui/`: Non-blocking terminal dashboard powered by `ratatui` and `crossterm`.
- `examples/echo_server.rs`: High-concurrency local benchmark echo server with connection and telemetry logging.
- `benches/metrics_benchmark.rs`: Criterion microbenchmarks for latency recording, atomics, and duration parsing.
- `tests/integration_test.rs`: End-to-end integration test suite.

## Strict Engineering Standards

1. **Zero Allocations on Hot Paths:** Static payloads must be pre-allocated as `Utf8Bytes` or `Bytes` at startup. Never perform heap allocations, string formatting, or clones inside worker send/recv loops unless dynamic macro templates (`{{timestamp}}`, `{{seq}}`) are explicitly configured.
2. **Lock-Free Concurrency:** Never use `Mutex` or `RwLock` in the hot message path. Workers record into task-local histograms with zero mutex contention and merge once upon session teardown. Live UI metrics must use relaxed atomic counters.
3. **Strict Error Handling:** Never use `.unwrap()`, `.expect()`, `todo!`, or `panic!` in runtime paths. Use strongly-typed project errors and the `?` operator. Map all socket and handshake errors into `ErrorCategory`.
4. **Monotonic Hardware Timing:** Always use `tokio::time::Instant` or `std::time::Instant` for RTT and duration tracking (immune to NTP slewing and wall-clock adjustments).
5. **Crate & Documentation Research:** Always verify dependency versions in `Cargo.toml`. Look up official documentation on [crates.io](https://crates.io) or [docs.rs](https://docs.rs) for that exact version before writing code.
6. **Comment Hygiene:** Follow `.codex/skills/comment-cleanup/SKILL.md`. Zero syntax narration comments; preserve only business, concurrency, and `// SAFETY:` invariants.
7. **Quality Gate:** Before finishing, verify that `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` pass with zero warnings.
