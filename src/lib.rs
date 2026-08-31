//! wsblast library definitions for programmatic WebSocket load testing.

pub mod cli;
pub mod config;
pub mod error;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod tui;
pub mod worker;

pub use cli::{Cli, OutputFormat, TestMode};
pub use config::{LoadTestConfig, PayloadConfig, SloThresholds};
pub use error::{ConfigError, ErrorCategory, Result, WorkerError, WsBlastError};
pub use metrics::{AggregatedMetrics, LatencyStats, LiveMetrics, LiveSnapshot, WorkerMetrics};
pub use report::{SloCheck, SloEvaluation, evaluate_slos, render_report};
pub use runner::Runner;
pub use tui::TuiApp;
pub use worker::WorkerSession;
