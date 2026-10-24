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
//! if the 99th percentile round-trip time exceeds 25 ms:
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
//!   and [`LatencyStats`] for handshakes and echo round trips.
//! - **Gate**: [`evaluate_slos`] checks [`SloThresholds`] and returns an
//!   [`SloEvaluation`]; [`render_report`] renders the same text, JSON or
//!   Markdown report the CLI prints.
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
//! - `auth` and `compare` add the `--auth-*` and `--compare` command-line
//!   options and have no library API. Send credentials with
//!   [`header`](LoadTestConfigBuilder::header).
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
#![warn(missing_docs)]
#![deny(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod app;
mod auth;
mod cli;
mod config;
mod error;
mod metrics;
mod report;
mod runner;
mod tui;
mod worker;

pub use cli::{OutputFormat, TestMode};
#[cfg(feature = "scenario")]
pub use config::ScenarioConfig;
pub use config::{LoadTestConfig, LoadTestConfigBuilder, ReplyMatch, SloThresholds};
pub use error::{ConfigError, ErrorCategory, Result, WorkerError, WsBlastError};
pub use metrics::{AggregatedMetrics, LatencyStats, LiveSnapshot};
#[cfg(feature = "scenario")]
pub use report::PhaseTarget;
pub use report::{SloCheck, SloEvaluation, evaluate_slos, render_report};
pub use runner::Runner;
#[cfg(feature = "scenario")]
pub use runner::{ScenarioExecutor, ScenarioOutcome};

/// Support for the `wsblast` binary and the benchmarks. Not part of the
/// public API: anything here may change or disappear in any release.
#[doc(hidden)]
pub mod __private {
    pub use crate::app::run_cli;
    pub use crate::config::parse_human_duration;
    pub use crate::metrics::{LiveMetrics, WorkerMetrics};

    /// Merges per-worker metrics as the runner does at the end of a run.
    pub fn merge_worker_metrics(
        workers: Vec<WorkerMetrics>,
        elapsed: std::time::Duration,
        attempted_connections: u64,
    ) -> crate::Result<crate::AggregatedMetrics> {
        crate::AggregatedMetrics::merge(workers, elapsed, attempted_connections)
    }
}
