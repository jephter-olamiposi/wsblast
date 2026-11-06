//! The load-testing engine behind the [`wsblast`] command line, as a library.
//!
//! Use it to drive WebSocket load from your own test harness or tool and to
//! gate on the result in code: the same engine, validation, metrics and SLO
//! checks the CLI uses, without spawning a process or parsing its report.
//!
//! [`wsblast`]: https://crates.io/crates/wsblast
//!
//! # Example
//!
//! Run 10 connections for 5 seconds at 20 messages per second each, then fail
//! if the 99th percentile response time exceeds 25 ms. Response time is
//! measured from when each request was due, so it includes any time a request
//! waited behind a slow reply:
//!
//! ```no_run
//! use std::time::Duration;
//! use wsblast::{LoadTestConfig, Runner, SloThresholds, evaluate_slos};
//!
//! #[tokio::main]
//! async fn main() -> wsblast::Result<()> {
//!     let config = LoadTestConfig::builder("ws://127.0.0.1:9001/ws")
//!         .connections(10)
//!         .duration(Duration::from_secs(5))
//!         .rate_per_connection(20)
//!         .slo(SloThresholds::new().with_max_p99(Duration::from_millis(25)))
//!         .build()?;
//!
//!     let metrics = Runner::new(config.clone())
//!         .with_signal_handling(false)
//!         .run()
//!         .await?;
//!     println!("p99 round trip: {:?}", metrics.message_rtt.p99_duration());
//!     println!("p99 response time: {:?}", metrics.response_time.p99_duration());
//!
//!     let gate = evaluate_slos(config.slo(), &metrics);
//!     for check in gate.checks.iter().filter(|check| !check.passed) {
//!         eprintln!("{}: {} (target {})", check.name, check.actual, check.target);
//!     }
//!     if !gate.passed {
//!         std::process::exit(2);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Overview
//!
//! - **Configure**: [`LoadTestConfig::builder`] returns a
//!   [`LoadTestConfigBuilder`] that starts from the command-line defaults.
//!   [`build`](LoadTestConfigBuilder::build) applies the same validation as
//!   the CLI and returns a [`ConfigError`] for anything the CLI would reject.
//! - **Run**: [`Runner::run`] opens the connections, drives them until the
//!   duration or request limit is reached or the run is cancelled, and merges
//!   every worker's metrics. [`Runner::live_snapshot`] reads progress while it
//!   runs.
//! - **Read the results**: [`AggregatedMetrics`] holds connection, message and
//!   byte counts, throughput, the error rate, an [`ErrorCategory`] breakdown,
//!   and [`LatencyStats`] for handshakes, echo round trips and, for paced
//!   runs, response time.
//! - **Gate**: [`evaluate_slos`] checks [`SloThresholds`] and returns an
//!   [`SloEvaluation`]; [`render_report`] renders the same text, JSON,
//!   Markdown or HTML report the CLI prints.
//! - **Scenarios** (`scenario` feature): [`ScenarioConfig`] loads a YAML
//!   multi-phase scenario and [`ScenarioExecutor`] runs its phases in order.
//!
//! How each number is measured is described in [METHODOLOGY.md].
//!
//! [METHODOLOGY.md]: https://github.com/jephter-olamiposi/wsblast/blob/main/METHODOLOGY.md
//!
//! # Behaviour to know when embedding
//!
//! - [`Runner::run`] listens for Ctrl+C and SIGTERM by default and ends the
//!   run when one arrives. Call
//!   [`with_signal_handling(false)`](Runner::with_signal_handling) when your
//!   program handles signals itself, and stop the run through
//!   [`Runner::cancel_token`] instead.
//! - The runner writes warnings to stderr when the connection ramp cannot
//!   finish within the duration or the connection count approaches the
//!   process's open-file limit. The progress bar is off unless
//!   [`progress(true)`](LoadTestConfigBuilder::progress) is set.
//! - Only [`TestMode::Echo`] measures round-trip time, so latency thresholds
//!   are rejected in other modes and fail when no round trip was measured.
//!
//! # Feature flags
//!
//! - `scenario`: [`ScenarioConfig`], [`ScenarioExecutor`], [`ScenarioOutcome`]
//!   and [`PhaseTarget`].
//! - `graphql-ws`: [`TestMode::GraphQLWS`] and the `graphql_*` builder
//!   setters.
//! - `capture`: the `--capture` and `--capture-limit` options and the
//!   [`capture`] and [`capture_limit`] builder setters, which record every
//!   frame of a run to a JSONL file. Their totals are in
//!   [`AggregatedMetrics::capture`].
//! - `mock`: the `wsblast mock` test server, over `ws://` or `wss://`, and
//!   the [`mock`] module, to run it in-process: echo with delays, jitter,
//!   stalls, rejected upgrades and server closes, or replay of a capture
//!   file.
//! - `distributed`: the `wsblast agent` command, the `--agents` options
//!   and the [`distributed`] module, to generate one run's load from several
//!   machines and merge their results exactly.
//! - `chaos`: the `--chaos-*` options and [`ChaosConfig`], which inject
//!   client-side faults (delayed, dropped and corrupted sends and closed
//!   connections) without counting them as errors or latency. Their counts
//!   are in [`AggregatedMetrics::chaos`].
//! - `auth` and `compare` add the `--auth-*` and `--compare` command-line
//!   options and have no library API. Send credentials with
//!   [`header`](LoadTestConfigBuilder::header). The opt-in `jwt` and
//!   `oauth2` features add `--auth-type jwt` and `--auth-type oauth2`, also
//!   without a library API.
//!
#![cfg_attr(
    not(feature = "scenario"),
    doc = "[`ScenarioConfig`]: https://docs.rs/wsblast/latest/wsblast/struct.ScenarioConfig.html\n\
           [`ScenarioExecutor`]: https://docs.rs/wsblast/latest/wsblast/struct.ScenarioExecutor.html\n\
           [`ScenarioOutcome`]: https://docs.rs/wsblast/latest/wsblast/struct.ScenarioOutcome.html\n\
           [`PhaseTarget`]: https://docs.rs/wsblast/latest/wsblast/struct.PhaseTarget.html"
)]
#![cfg_attr(
    not(feature = "graphql-ws"),
    doc = "[`TestMode::GraphQLWS`]: https://docs.rs/wsblast/latest/wsblast/enum.TestMode.html#variant.GraphQLWS"
)]
#![cfg_attr(
    not(feature = "mock"),
    doc = "[`mock`]: https://docs.rs/wsblast/latest/wsblast/mock/index.html"
)]
#![cfg_attr(
    not(feature = "chaos"),
    doc = "[`ChaosConfig`]: https://docs.rs/wsblast/latest/wsblast/struct.ChaosConfig.html"
)]
#![cfg_attr(
    not(feature = "distributed"),
    doc = "[`distributed`]: https://docs.rs/wsblast/latest/wsblast/distributed/index.html"
)]
#![cfg_attr(
    feature = "capture",
    doc = "[`capture`]: LoadTestConfigBuilder::capture\n\
           [`capture_limit`]: LoadTestConfigBuilder::capture_limit"
)]
#![cfg_attr(
    not(feature = "capture"),
    doc = "[`capture`]: https://docs.rs/wsblast/latest/wsblast/struct.LoadTestConfigBuilder.html#method.capture\n\
           [`capture_limit`]: https://docs.rs/wsblast/latest/wsblast/struct.LoadTestConfigBuilder.html#method.capture_limit"
)]
#![warn(missing_docs)]
#![deny(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod app;
#[cfg(feature = "auth")]
mod auth;
#[cfg(any(
    feature = "capture",
    feature = "mock",
    feature = "distributed",
    feature = "jwt"
))]
mod base64;
#[cfg(feature = "capture")]
mod capture;
#[cfg(feature = "chaos")]
mod chaos;
mod cli;
mod clock;
mod config;
mod connect;
#[cfg(feature = "distributed")]
#[cfg_attr(docsrs, doc(cfg(feature = "distributed")))]
pub mod distributed;
mod error;
#[cfg(any(feature = "prometheus", feature = "otel"))]
mod export;
mod histogram;
mod html;
mod lag;
mod metrics;
#[cfg(feature = "mock")]
#[cfg_attr(docsrs, doc(cfg(feature = "mock")))]
pub mod mock;
mod output;
mod report;
mod runner;
#[cfg(feature = "compare")]
mod series_compare;
mod timeseries;
#[cfg(any(feature = "mock", feature = "distributed"))]
mod tls;
mod tui;
mod worker;

#[cfg(feature = "chaos")]
pub use chaos::{ChaosCloseMode, ChaosConfig};
pub use cli::{OutputFormat, TestMode};
#[cfg(feature = "scenario")]
pub use config::ScenarioConfig;
pub use config::{LoadTestConfig, LoadTestConfigBuilder, ReplyMatch, SloThresholds, Stage};
pub use error::{ConfigError, ErrorCategory, Result, WorkerError, WsBlastError};
pub use metrics::{
    AgentGenerator, AggregatedMetrics, CaptureSummary, ChaosSummary, DistributedSummary,
    GeneratorLag, LatencyStats, LiveSnapshot, OpenFileLimit, SlowReplies,
};
#[cfg(feature = "scenario")]
pub use report::PhaseTarget;
pub use report::{SloCheck, SloEvaluation, evaluate_slos, render_report};
pub use runner::Runner;
#[cfg(feature = "scenario")]
pub use runner::{PhaseResult, ScenarioExecutor, ScenarioOutcome};
pub use timeseries::{IntervalLag, IntervalLatency, TimeSeries, TimeSeriesPoint};

/// Support for the `wsblast` binary and the benchmarks. Not part of the
/// public API: anything here may change or disappear in any release.
#[doc(hidden)]
pub mod __private {
    pub use crate::app::run_cli;
    pub use crate::config::parse_human_duration;
    pub use crate::metrics::{CounterShard, LiveMetrics, WorkerMetrics};

    /// Merges per-worker metrics as the runner does at the end of a run.
    pub fn merge_worker_metrics(
        workers: Vec<WorkerMetrics>,
        elapsed: std::time::Duration,
        attempted_connections: u64,
    ) -> crate::Result<crate::AggregatedMetrics> {
        crate::AggregatedMetrics::merge(workers, elapsed, attempted_connections)
    }
}

// Compile the Rust examples in README.md as doctests, so the crate's front
// page cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
