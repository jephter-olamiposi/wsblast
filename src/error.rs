//! Typed domain error hierarchy and error classification for WebSocket load testing.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Every error the library returns.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WsBlastError {
    /// The configuration was rejected before any connection was made.
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    /// A worker-level failure that aborted the operation, such as an invalid
    /// authentication header.
    #[error("Worker execution error: {0}")]
    Worker(#[from] WorkerError),

    /// Reading or writing a file or the terminal failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A report could not be serialized or parsed as JSON.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A latency histogram could not be created or merged.
    #[error("Metrics initialization failed: {0}")]
    Metrics(String),
}

/// Reasons a configuration is rejected before load generation starts.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The target URL is missing or does not parse: the raw value and the
    /// parser's message.
    #[error("Invalid target URL '{0}': {1}")]
    InvalidUrl(String, String),

    /// The target URL's scheme is not `ws` or `wss`.
    #[error("Unsupported WebSocket scheme '{0}': only 'ws://' and 'wss://' are supported")]
    UnsupportedScheme(String),

    /// A duration string does not parse.
    #[error(
        "Invalid duration '{0}': expected a non-negative number with a unit of ns, us, ms, s, m or h (e.g. '500ms', '1.5s', '2m'), or bare integer seconds"
    )]
    InvalidDuration(String),

    /// A handshake header has an invalid name or value.
    #[error("Invalid header format '{0}': expected 'Header-Name: value'")]
    InvalidHeader(String),

    /// The connection count is zero.
    #[error("Concurrency must be greater than zero, received {0}")]
    ZeroConcurrency(usize),

    /// A threshold is out of range, such as an error rate above 1.0.
    #[error("Invalid threshold specification: {0}")]
    InvalidThreshold(String),

    /// The authentication flags are incomplete or contradictory.
    #[error("Invalid authentication options: {0}")]
    InvalidAuth(String),

    /// An option has an invalid value.
    #[error("Invalid option: {0}")]
    InvalidOption(String),

    /// A scenario file does not parse or fails validation.
    #[error("Invalid scenario: {0}")]
    InvalidScenario(String),

    /// Two options cannot be used together, such as a latency SLO outside
    /// echo mode.
    #[error("Unsupported option combination: {0}")]
    UnsupportedOption(String),

    /// A payload, scenario or report file cannot be read.
    #[error("Payload file error at '{path}': {source}")]
    PayloadFile {
        /// Path of the file.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Failures of a single WebSocket session.
///
/// During a run these are counted by [`ErrorCategory`] rather than returned;
/// see [`AggregatedMetrics::error_breakdown`](crate::AggregatedMetrics::error_breakdown).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkerError {
    /// The host name did not resolve.
    #[error("DNS resolution failed: {0}")]
    DnsResolution(String),

    /// The TCP connection could not be opened.
    #[error("TCP connection failed to {address}: {source}")]
    TcpConnect {
        /// Address that was dialled.
        address: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The TLS handshake failed.
    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),

    /// The server answered the upgrade request with an HTTP error.
    #[error("HTTP upgrade rejected with status {status}: {reason}")]
    HttpUpgradeRejected {
        /// HTTP status code.
        status: u16,
        /// Reason phrase or body excerpt.
        reason: String,
    },

    /// The peer violated the WebSocket protocol, or a handshake header was invalid.
    #[error("WebSocket protocol error: {0}")]
    Protocol(String),

    /// Writing a frame failed.
    #[error("Frame write error: {0}")]
    Write(String),

    /// Reading a frame failed.
    #[error("Frame read error: {0}")]
    Read(String),

    /// An operation did not finish in time.
    #[error("Operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The connection closed while the session still needed it.
    #[error("Connection closed unexpectedly: code={code:?}, reason={reason}")]
    UnexpectedClose {
        /// Close code, if the peer sent one.
        code: Option<u16>,
        /// Close reason.
        reason: String,
    },
}

/// Category under which a failure is counted in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCategory {
    /// The host name did not resolve.
    DnsResolution,
    /// The TCP connection was refused or failed.
    TcpConnect,
    /// The TLS handshake failed.
    TlsHandshake,
    /// The server rejected the WebSocket upgrade with a 4xx or 5xx status.
    HttpUpgradeRejected,
    /// The peer violated the WebSocket or GraphQL WS protocol.
    ProtocolError,
    /// Writing a frame failed.
    WriteError,
    /// Reading a frame failed.
    ReadError,
    /// A handshake or a reply did not arrive in time.
    Timeout,
    /// The connection closed while a reply or subscription was pending.
    UnexpectedClose,
    /// Anything else.
    Other,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DnsResolution => write!(f, "DNS Resolution Failed"),
            Self::TcpConnect => write!(f, "TCP Connect Failed"),
            Self::TlsHandshake => write!(f, "TLS Handshake Failed"),
            Self::HttpUpgradeRejected => write!(f, "HTTP Upgrade Rejected"),
            Self::ProtocolError => write!(f, "Protocol Error"),
            Self::WriteError => write!(f, "Frame Write Error"),
            Self::ReadError => write!(f, "Frame Read Error"),
            Self::Timeout => write!(f, "Operation Timeout"),
            Self::UnexpectedClose => write!(f, "Unexpected Close"),
            Self::Other => write!(f, "Other Error"),
        }
    }
}

impl From<&WorkerError> for ErrorCategory {
    fn from(err: &WorkerError) -> Self {
        match err {
            WorkerError::DnsResolution(_) => Self::DnsResolution,
            WorkerError::TcpConnect { .. } => Self::TcpConnect,
            WorkerError::TlsHandshake(_) => Self::TlsHandshake,
            WorkerError::HttpUpgradeRejected { .. } => Self::HttpUpgradeRejected,
            WorkerError::Protocol(_) => Self::ProtocolError,
            WorkerError::Write(_) => Self::WriteError,
            WorkerError::Read(_) => Self::ReadError,
            WorkerError::Timeout(_) => Self::Timeout,
            WorkerError::UnexpectedClose { .. } => Self::UnexpectedClose,
        }
    }
}

/// Result type returned by the library.
pub type Result<T> = std::result::Result<T, WsBlastError>;
