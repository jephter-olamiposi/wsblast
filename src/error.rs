//! Typed domain error hierarchy and error classification for WebSocket load testing.

use std::fmt;

/// Primary error type encompassing configuration, transport, I/O, and SLO evaluation failures.
#[derive(Debug, thiserror::Error)]
pub enum WsBlastError {
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("Worker execution error: {0}")]
    Worker(#[from] WorkerError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("SLO threshold breached: {0}")]
    SloBreach(String),
}

/// Configuration validation errors encountered before initiating load generation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Invalid target URL '{0}': {1}")]
    InvalidUrl(String, String),

    #[error("Unsupported WebSocket scheme '{0}': only 'ws://' and 'wss://' are supported")]
    UnsupportedScheme(String),

    #[error("Invalid duration '{0}': must be formatted like '10s', '2m', '500ms'")]
    InvalidDuration(String),

    #[error("Invalid header format '{0}': expected 'Header-Name: value'")]
    InvalidHeader(String),

    #[error("Concurrency must be greater than zero, received {0}")]
    ZeroConcurrency(usize),

    #[error("Rate must be greater than zero, received {0}")]
    ZeroRate(u64),

    #[error("Invalid threshold specification: {0}")]
    InvalidThreshold(String),

    #[error("Payload file error at '{path}': {source}")]
    PayloadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Granular worker-level session errors captured during connection establishment and framing.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("DNS resolution failed: {0}")]
    DnsResolution(String),

    #[error("TCP connection failed to {address}: {source}")]
    TcpConnect {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),

    #[error("HTTP upgrade rejected with status {status}: {reason}")]
    HttpUpgradeRejected { status: u16, reason: String },

    #[error("WebSocket protocol error: {0}")]
    Protocol(String),

    #[error("Frame write error: {0}")]
    Write(String),

    #[error("Frame read error: {0}")]
    Read(String),

    #[error("Operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Connection closed unexpectedly: code={code:?}, reason={reason}")]
    UnexpectedClose { code: Option<u16>, reason: String },

    #[error("Internal channel closed unexpectedly")]
    ChannelClosed,
}

/// Error category taxonomy for metric aggregation and CI/CD diagnostic reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    DnsResolution,
    TcpConnect,
    TlsHandshake,
    HttpUpgradeRejected,
    ProtocolError,
    WriteError,
    ReadError,
    Timeout,
    UnexpectedClose,
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
            WorkerError::ChannelClosed => Self::Other,
        }
    }
}

pub type Result<T> = std::result::Result<T, WsBlastError>;
