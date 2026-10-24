//! Command-line argument parsing and flag specifications.

#[cfg(feature = "mock")]
use clap::Subcommand;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
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

    /// Connection ramp rate in workers per second (0 = spawn all connections immediately)
    #[arg(
        long = "ramp-rate",
        default_value = "0",
        value_name = "WORKERS_PER_SEC"
    )]
    pub ramp_rate: u64,

    /// Total test duration including the connection ramp (e.g., '10s', '30s', '2m', '500ms')
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

    /// OpenTelemetry OTLP endpoint (e.g., http://localhost:4317)
    #[cfg(feature = "otel")]
    #[arg(
        long = "otel-endpoint",
        value_name = "URL",
        default_value = "http://localhost:4317"
    )]
    pub otel_endpoint: String,

    /// OpenTelemetry service name
    #[cfg(feature = "otel")]
    #[arg(
        long = "otel-service-name",
        value_name = "NAME",
        default_value = "wsblast"
    )]
    pub otel_service_name: String,

    /// Enable OpenTelemetry OTLP metric export
    #[cfg(feature = "otel")]
    #[arg(long = "otel", default_value_t = false)]
    pub enable_otel: bool,

    /// Serve Prometheus metrics over HTTP
    #[cfg(feature = "prometheus")]
    #[arg(long = "prometheus", default_value_t = false)]
    pub enable_prometheus: bool,

    /// Prometheus metrics listen address (e.g., 0.0.0.0:9090)
    #[cfg(feature = "prometheus")]
    #[arg(
        long = "prometheus-listen",
        value_name = "ADDR",
        default_value = "0.0.0.0:9090"
    )]
    pub prometheus_listen: String,

    /// Enable permessage-deflate compression (unavailable with tungstenite 0.26)
    #[cfg(feature = "compression")]
    #[arg(long = "compression", default_value_t = false)]
    pub compression: bool,

    /// Compression level (1-9)
    #[cfg(feature = "compression")]
    #[arg(long = "compression-level", value_name = "LEVEL", default_value_t = 6)]
    pub compression_level: u8,

    /// Authentication type (static, jwt, oauth2, apikey)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-type", value_name = "TYPE", value_enum, default_value_t = AuthType::None)]
    pub auth_type: AuthType,

    /// Static token for auth (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-token", value_name = "TOKEN")]
    pub auth_token: Option<String>,

    /// Header name for static token (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-header",
        value_name = "HEADER",
        default_value = "Authorization"
    )]
    pub auth_header: String,

    /// JWT token endpoint (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-endpoint", value_name = "URL")]
    pub auth_jwt_endpoint: Option<String>,

    /// JWT client ID (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-client-id", value_name = "ID")]
    pub auth_jwt_client_id: Option<String>,

    /// JWT secret (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-secret", value_name = "SECRET")]
    pub auth_jwt_secret: Option<String>,

    /// JWT scopes (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-scopes", value_name = "SCOPES", num_args = 1..)]
    pub auth_jwt_scopes: Vec<String>,

    /// OAuth2 token URL (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-token-url", value_name = "URL")]
    pub auth_oauth2_token_url: Option<String>,

    /// OAuth2 client ID (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-client-id", value_name = "ID")]
    pub auth_oauth2_client_id: Option<String>,

    /// OAuth2 client secret (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-client-secret", value_name = "SECRET")]
    pub auth_oauth2_client_secret: Option<String>,

    /// OAuth2 scopes (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-scopes", value_name = "SCOPES", num_args = 1..)]
    pub auth_oauth2_scopes: Vec<String>,

    /// API Key (used with --auth-type apikey)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-apikey", value_name = "KEY")]
    pub auth_apikey: Option<String>,

    /// API Key header name (used with --auth-type apikey)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-apikey-header",
        value_name = "HEADER",
        default_value = "X-API-Key"
    )]
    pub auth_apikey_header: String,

    /// GraphQL WS operation name
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-operation-name", value_name = "NAME")]
    pub graphql_ws_operation_name: Option<String>,

    /// GraphQL WS operation (query/mutation/subscription)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-operation", value_name = "OPERATION")]
    pub graphql_ws_operation: Option<String>,

    /// GraphQL WS variables (JSON)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-variables", value_name = "JSON")]
    pub graphql_ws_variables: Option<String>,

    /// GraphQL WS extensions (JSON)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-extensions", value_name = "JSON")]
    pub graphql_ws_extensions: Option<String>,

    /// Enable frame capture to file
    #[cfg(feature = "capture")]
    #[arg(long = "capture", default_value_t = false)]
    pub capture: bool,

    /// Frame capture output file path
    #[cfg(feature = "capture")]
    #[arg(long = "capture-output", value_name = "FILE")]
    pub capture_output: Option<PathBuf>,

    /// Frame capture format (pcapng or jsonl)
    #[cfg(feature = "capture")]
    #[arg(long = "capture-format", value_name = "FORMAT", value_enum, default_value_t = CaptureFormat::PcapNg)]
    pub capture_format: CaptureFormat,

    /// Maximum frames to capture (0 = unlimited)
    #[cfg(feature = "capture")]
    #[arg(long = "capture-max-frames", value_name = "NUM", default_value_t = 0)]
    pub capture_max_frames: usize,

    /// Enable chaos injection
    #[cfg(feature = "chaos")]
    #[arg(long = "chaos", default_value_t = false)]
    pub chaos: bool,

    /// Inject latency in milliseconds
    #[cfg(feature = "chaos")]
    #[arg(long = "chaos-latency", value_name = "MS")]
    pub chaos_latency: Option<u64>,

    /// Inject packet loss percentage (0.0-100.0)
    #[cfg(feature = "chaos")]
    #[arg(long = "chaos-loss", value_name = "PERCENT")]
    pub chaos_loss: Option<f64>,

    /// Inject corruption percentage (0.0-100.0)
    #[cfg(feature = "chaos")]
    #[arg(long = "chaos-corrupt", value_name = "PERCENT")]
    pub chaos_corrupt: Option<f64>,

    /// Inject random close percentage (0.0-100.0)
    #[cfg(feature = "chaos")]
    #[arg(long = "chaos-close", value_name = "PERCENT")]
    pub chaos_close: Option<f64>,

    /// Output report format
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Compare two JSON reports for regression detection
    #[cfg(feature = "compare")]
    #[arg(long = "compare", value_name = "FILE", num_args = 2)]
    pub compare: Vec<PathBuf>,

    /// Regression threshold percentage (default: 10%)
    #[cfg(feature = "compare")]
    #[arg(
        long = "compare-threshold",
        value_name = "PERCENT",
        default_value_t = 10.0
    )]
    pub compare_threshold: f64,

    /// Fail on regression (exit code 2)
    #[cfg(feature = "compare")]
    #[arg(long = "fail-on-regression", default_value_t = false)]
    pub fail_on_regression: bool,

    /// Distributed mode: coordinator or worker (omit for a standalone load test)
    #[cfg(feature = "distributed")]
    #[arg(long = "distributed-mode", value_name = "MODE", value_enum)]
    pub distributed_mode: Option<DistributedMode>,

    /// Coordinator URL (required for worker mode)
    #[cfg(feature = "distributed")]
    #[arg(long = "coordinator-url", value_name = "URL")]
    pub coordinator_url: Option<String>,

    /// Worker ID (auto-generated if not provided)
    #[cfg(feature = "distributed")]
    #[arg(long = "worker-id", value_name = "ID")]
    pub worker_id: Option<String>,

    /// Total worker count (coordinator mode)
    #[cfg(feature = "distributed")]
    #[arg(long = "worker-count", value_name = "NUM", default_value_t = 1)]
    pub worker_count: usize,

    /// Bind address for worker gRPC server
    #[cfg(feature = "distributed")]
    #[arg(
        long = "worker-bind",
        value_name = "ADDR",
        default_value = "127.0.0.1:50051"
    )]
    pub worker_bind: String,

    /// Bind address for coordinator gRPC server
    #[cfg(feature = "distributed")]
    #[arg(
        long = "coordinator-bind",
        value_name = "ADDR",
        default_value = "127.0.0.1:50052"
    )]
    pub coordinator_bind: String,

    /// Write output report to the specified file path
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output_file: Option<PathBuf>,

    /// Launch interactive terminal dashboard (TUI)
    #[arg(long = "tui", default_value_t = false)]
    pub tui: bool,

    /// Suppress interactive progress bar (recommended for non-interactive CI/CD runners)
    #[arg(long = "no-progress", default_value_t = false)]
    pub no_progress: bool,

    /// End a connection's worker after a timeout, I/O error or server close instead of reconnecting with backoff
    #[arg(long = "no-reconnect", default_value_t = false)]
    pub no_reconnect: bool,

    /// Path to scenario YAML file defining multi-phase test (conflicts with individual flags)
    #[arg(long = "scenario", value_name = "FILE", conflicts_with_all = ["connections", "duration", "rate", "payload", "payload_file"])]
    pub scenario: Option<PathBuf>,

    /// Run only a specific phase from the scenario by name
    #[arg(long = "phase", value_name = "NAME", requires = "scenario")]
    pub phase: Option<String>,

    /// Mock server subcommand
    #[cfg(feature = "mock")]
    #[command(subcommand)]
    pub mock: Option<MockSubcommand>,
}

#[cfg(feature = "mock")]
#[derive(Debug, Subcommand, Clone)]
pub enum MockSubcommand {
    /// Record WebSocket interactions to a file
    Record {
        /// Bind address for the mock server
        #[arg(long = "bind", default_value = "127.0.0.1:9001")]
        bind: std::net::SocketAddr,
        /// Output file for recorded frames
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: PathBuf,
        /// Delay before responding (milliseconds)
        #[arg(long = "delay", value_name = "MS")]
        delay: Option<u64>,
    },
    /// Replay recorded WebSocket interactions
    Replay {
        /// Bind address for the mock server
        #[arg(long = "bind", default_value = "127.0.0.1:9001")]
        bind: std::net::SocketAddr,
        /// Input file with recorded frames
        #[arg(short = 'i', long = "input", value_name = "FILE")]
        input: PathBuf,
        /// Delay before responding (milliseconds)
        #[arg(long = "delay", value_name = "MS")]
        delay: Option<u64>,
        /// Error injection: close after N messages
        #[arg(long = "error-close-after", value_name = "N")]
        error_close_after: Option<usize>,
        /// Error injection: inject error after N messages
        #[arg(long = "error-inject-after", value_name = "N")]
        error_inject_after: Option<usize>,
        /// Error injection: custom error message
        #[arg(long = "error-message", value_name = "MSG")]
        error_message: Option<String>,
    },
}

impl Cli {
    /// Resolves target URL from either positional or named argument syntax.
    pub fn resolve_url(&self) -> Option<&str> {
        self.url.as_deref().or(self.url_flag.as_deref())
    }

    /// Returns true if running in scenario mode
    pub fn is_scenario_mode(&self) -> bool {
        self.scenario.is_some()
    }
}

/// Authentication method for WebSocket handshake.
#[cfg(feature = "auth")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthType {
    /// No authentication
    None,
    /// Static bearer token
    Static,
    /// JWT with client credentials
    Jwt,
    /// OAuth2 client credentials flow
    #[value(name = "oauth2", alias = "o-auth2")]
    OAuth2,
    /// API Key
    #[value(name = "apikey", alias = "api-key")]
    ApiKey,
}

/// Distributed operation mode.
#[cfg(feature = "distributed")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistributedMode {
    /// Coordinator node that orchestrates workers
    Coordinator,
    /// Worker node that executes load test
    Worker,
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
    /// Apollo GraphQL WS protocol (subscriptions).
    #[cfg(feature = "graphql-ws")]
    #[value(name = "graphql-ws", alias = "graph-qlws")]
    GraphQLWS,
}

/// Frame capture output format.
#[cfg(feature = "capture")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureFormat {
    /// PCAP-NG format for Wireshark analysis
    PcapNg,
    /// JSON Lines format for structured logging
    JsonLines,
}

/// Supported serialisation and presentation formats for test summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable styled terminal tables.
    Text,
    /// Schema-versioned JSON for CI parsing and metric archiving.
    Json,
    /// GitHub Flavored Markdown table for PR comments and job summaries.
    Markdown,
}
