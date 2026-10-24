//! wsblast library definitions for programmatic WebSocket load testing.

pub mod auth;
pub mod cli;
pub mod config;
pub mod error;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod tui;
pub mod worker;

#[cfg(feature = "auth")]
pub use auth::{
    ApiKeyAuthenticator, Authenticator, JwtAuthenticator, OAuth2Authenticator, StaticAuthenticator,
};
pub use cli::{Cli, OutputFormat, TestMode};
#[cfg(feature = "auth")]
pub use config::AuthConfig;
#[cfg(feature = "scenario")]
pub use config::{ActionConfig, PhaseConfig, ScenarioConfig, ScenarioMeta};
pub use config::{LoadTestConfig, PayloadConfig, SloThresholds};
pub use error::{ConfigError, ErrorCategory, Result, WorkerError, WsBlastError};
pub use metrics::{AggregatedMetrics, LatencyStats, LiveMetrics, LiveSnapshot, WorkerMetrics};
#[cfg(feature = "compare")]
pub use report::{
    BaselineComparison, Improvement, Regression, compare_metrics, parse_report_metrics,
    render_comparison_report,
};
pub use report::{SloCheck, SloEvaluation, evaluate_slos, render_report};
pub use runner::Runner;
pub use tui::TuiApp;
pub use worker::WorkerSession;
