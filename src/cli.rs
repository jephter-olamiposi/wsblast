//! Command-line argument parsing and flag specifications.

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// High-throughput, CI-friendly WebSocket load testing and SLO gating tool.
#[derive(Debug, Parser, Clone)]
#[command(
    name = "wsblast",
    author,
    version,
    about = "High-performance WebSocket load-testing CLI with percentile latency & CI gating",
    long_about = "wsblast stresses WebSocket targets, computes high-resolution latency percentiles \
                  (p50/p95/p99/p99.9), maps failure taxonomy, and gates CI pipelines via SLO thresholds."
)]
pub struct Cli {
    /// Target WebSocket endpoint (ws:// or wss://)
    #[arg(
        value_name = "URL",
        help = "Target WebSocket URL (e.g., ws://127.0.0.1:9001/ws or wss://api.example.com/stream)"
    )]
    pub url: Option<String>,

    /// Target WebSocket endpoint flag (alternative to positional argument)
    #[arg(short = 'u', long = "url", value_name = "URL", conflicts_with = "url")]
    pub url_flag: Option<String>,

    /// Number of concurrent WebSocket connection workers
    #[arg(
        short = 'c',
        long = "connections",
        default_value = "50",
        value_name = "NUM"
    )]
    pub connections: usize,

    /// Test duration (e.g., '10s', '30s', '2m', '500ms')
    #[arg(
        short = 'd',
        long = "duration",
        default_value = "10s",
        value_name = "DURATION"
    )]
    pub duration: String,

    /// Maximum total messages to send across all workers (optional limit)
    #[arg(short = 'n', long = "requests", value_name = "NUM")]
    pub requests: Option<u64>,

    /// Message rate per connection per second (0 = unthrottled / maximum possible)
    #[arg(
        short = 'r',
        long = "rate",
        default_value = "0",
        value_name = "MSG_PER_SEC"
    )]
    pub rate: u64,

    /// Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})
    #[arg(short = 'p', long = "payload", value_name = "TEXT")]
    pub payload: Option<String>,

    /// Path to a file containing the message payload
    #[arg(long = "payload-file", value_name = "FILE", conflicts_with = "payload")]
    pub payload_file: Option<PathBuf>,

    /// Send payload as WebSocket binary frames instead of text frames
    #[arg(long = "binary", default_value_t = false)]
    pub binary: bool,

    /// Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)
    #[arg(short = 'H', long = "header", value_name = "HEADER:VALUE")]
    pub headers: Vec<String>,

    /// Optional WebSocket subprotocol to request during handshake
    #[arg(long = "subprotocol", value_name = "NAME")]
    pub subprotocol: Option<String>,

    /// Load testing execution mode
    #[arg(long = "mode", value_enum, default_value_t = TestMode::Echo)]
    pub mode: TestMode,

    /// Connection handshake timeout (e.g., '5s', '2000ms')
    #[arg(
        long = "connect-timeout",
        default_value = "5s",
        value_name = "DURATION"
    )]
    pub connect_timeout: String,

    /// Per-message response timeout in echo mode (e.g., '5s', '1000ms')
    #[arg(
        long = "message-timeout",
        default_value = "5s",
        value_name = "DURATION"
    )]
    pub message_timeout: String,

    /// Periodic ping heartbeat interval (e.g., '10s', '0s' to disable)
    #[arg(long = "ping-interval", default_value = "0s", value_name = "DURATION")]
    pub ping_interval: String,

    /// SLO threshold: Maximum acceptable p50 round-trip latency (e.g., '10ms', '500us')
    #[arg(long = "max-p50", value_name = "DURATION")]
    pub max_p50: Option<String>,

    /// SLO threshold: Maximum acceptable p95 round-trip latency (e.g., '50ms', '20ms')
    #[arg(long = "max-p95", value_name = "DURATION")]
    pub max_p95: Option<String>,

    /// SLO threshold: Maximum acceptable p99 round-trip latency (e.g., '100ms', '50ms')
    #[arg(long = "max-p99", value_name = "DURATION")]
    pub max_p99: Option<String>,

    /// SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., '250ms')
    #[arg(long = "max-p999", value_name = "DURATION")]
    pub max_p999: Option<String>,

    /// SLO threshold: Maximum acceptable error rate fraction (e.g., '0.01' for 1%, '0.001' for 0.1%)
    #[arg(long = "max-error-rate", value_name = "FLOAT")]
    pub max_error_rate: Option<f64>,

    /// SLO threshold: Minimum required message throughput (messages/sec)
    #[arg(long = "min-throughput", value_name = "RPS")]
    pub min_throughput: Option<f64>,

    /// Output report format
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Write output report to the specified file path
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output_file: Option<PathBuf>,

    /// Launch interactive terminal dashboard (TUI)
    #[arg(long = "tui", default_value_t = false)]
    pub tui: bool,

    /// Suppress interactive progress bar (recommended for non-interactive CI/CD runners)
    #[arg(long = "no-progress", default_value_t = false)]
    pub no_progress: bool,
}

/// Supported execution patterns for WebSocket load generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TestMode {
    /// Send request frame, await response, and record round-trip latency (RTT).
    Echo,
    /// Continuously stream frames without awaiting per-frame acknowledgments.
    Stream,
    /// Connect workers and passively record incoming server-pushed broadcast messages.
    Listen,
}

/// Supported serialisation and presentation formats for test summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable styled terminal tables.
    Text,
    /// Schema-versioned JSON for CI parsing and metric archiving.
    Json,
    /// GitHub Flavored Markdown table for PR comments and job summaries.
    Markdown,
}

impl Cli {
    /// Resolves target URL from either positional or named argument syntax.
    pub fn resolve_url(&self) -> Option<&str> {
        self.url.as_deref().or(self.url_flag.as_deref())
    }
}
