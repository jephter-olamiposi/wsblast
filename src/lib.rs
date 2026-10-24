//! The load-testing engine behind the `wsblast` command line.
//!
//! Build a [`LoadTestConfig`] with [`LoadTestConfig::builder`], run it with a
//! [`Runner`], and gate the resulting [`AggregatedMetrics`] with
//! [`evaluate_slos`].

#![warn(missing_docs)]
#![deny(unsafe_code)]

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
