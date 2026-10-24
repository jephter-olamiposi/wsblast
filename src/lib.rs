//! wsblast library definitions for programmatic WebSocket load testing.

pub mod auth;
pub mod cli;
pub mod config;
#[cfg(feature = "distributed")]
pub mod distributed;
pub mod error;
pub mod metrics;
#[cfg(feature = "mock")]
pub mod mock;
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
#[cfg(feature = "distributed")]
pub use distributed::{DistributedConfig, DistributedMode};
#[cfg(feature = "distributed")]
pub use distributed::{run_coordinator, run_worker};
pub use error::{ConfigError, ErrorCategory, Result, WorkerError, WsBlastError};
#[cfg(feature = "otel")]
pub use metrics::OtelExporter;
#[cfg(feature = "prometheus")]
pub use metrics::PrometheusExporter;
pub use metrics::{AggregatedMetrics, LatencyStats, LiveMetrics, LiveSnapshot, WorkerMetrics};
#[cfg(feature = "mock")]
pub use mock::{ErrorInjectionConfig, FrameDirection, MockConfig, MockServer, RecordedFrame};
#[cfg(feature = "compare")]
pub use report::{
    BaselineComparison, Improvement, Regression, compare_metrics, parse_report_metrics,
    render_comparison_report,
};
pub use report::{SloCheck, SloEvaluation, evaluate_slos, render_report};
pub use runner::Runner;
pub use tui::TuiApp;
pub use worker::WorkerSession;
