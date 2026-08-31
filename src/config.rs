//! Validated runtime configuration model for load test execution.

use crate::cli::{Cli, OutputFormat, TestMode};
use crate::error::{ConfigError, Result, WsBlastError};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

/// Fully validated, immutable configuration for load test execution.
#[derive(Debug, Clone)]
pub struct LoadTestConfig {
    pub target_url: Url,
    pub connections: usize,
    pub duration: Duration,
    pub max_requests: Option<u64>,
    pub rate_per_conn: u64,
    pub payload: PayloadConfig,
    pub headers: HeaderMap,
    pub subprotocol: Option<String>,
    pub mode: TestMode,
    pub connect_timeout: Duration,
    pub message_timeout: Duration,
    pub ping_interval: Duration,
    pub slo: SloThresholds,
    pub output_format: OutputFormat,
    pub output_path: Option<PathBuf>,
    pub tui: bool,
    pub no_progress: bool,
}

/// Payload specification supporting dynamic templating or raw binary streams.
#[derive(Debug, Clone)]
pub enum PayloadConfig {
    /// UTF-8 text template supporting `{{timestamp}}`, `{{worker_id}}`, and `{{seq}}` substitutions.
    Text(String),
    /// Raw binary frame payload.
    Binary(Vec<u8>),
}

/// Service Level Objective thresholds for automated CI/CD pass/fail gating.
#[derive(Debug, Clone, Default)]
pub struct SloThresholds {
    pub max_p50: Option<Duration>,
    pub max_p95: Option<Duration>,
    pub max_p99: Option<Duration>,
    pub max_p999: Option<Duration>,
    pub max_error_rate: Option<f64>,
    pub min_throughput: Option<f64>,
}

impl LoadTestConfig {
    /// Constructs and validates a `LoadTestConfig` from parsed CLI arguments.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Config`] if:
    /// - No target URL is provided or URL scheme is not `ws://` or `wss://`.
    /// - Concurrency is zero.
    /// - Any duration string fails parsing.
    /// - Custom headers violate HTTP specification.
    /// - Error rate threshold is not within the `[0.0, 1.0]` range.
    /// - Payload file cannot be read or contains non-UTF-8 data for text mode.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let raw_url = cli.resolve_url().ok_or_else(|| {
            ConfigError::InvalidUrl(
                "".into(),
                "No target URL provided. Specify target as argument or --url <URL>".into(),
            )
        })?;

        let target_url = Url::parse(raw_url)
            .map_err(|e| ConfigError::InvalidUrl(raw_url.to_string(), e.to_string()))?;

        match target_url.scheme() {
            "ws" | "wss" => {}
            other => return Err(ConfigError::UnsupportedScheme(other.to_string()).into()),
        }

        if cli.connections == 0 {
            return Err(ConfigError::ZeroConcurrency(0).into());
        }

        let duration = parse_human_duration(&cli.duration)?;
        let connect_timeout = parse_human_duration(&cli.connect_timeout)?;
        let message_timeout = parse_human_duration(&cli.message_timeout)?;
        let ping_interval = parse_human_duration(&cli.ping_interval)?;

        let payload = if let Some(path) = &cli.payload_file {
            let bytes = fs::read(path).map_err(|e| ConfigError::PayloadFile {
                path: path.display().to_string(),
                source: e,
            })?;
            if cli.binary {
                PayloadConfig::Binary(bytes)
            } else {
                let text = String::from_utf8(bytes).map_err(|e| ConfigError::PayloadFile {
                    path: path.display().to_string(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                })?;
                PayloadConfig::Text(text)
            }
        } else if let Some(text) = cli.payload {
            if cli.binary {
                PayloadConfig::Binary(text.into_bytes())
            } else {
                PayloadConfig::Text(text)
            }
        } else if cli.binary {
            PayloadConfig::Binary(b"wsblast-payload".to_vec())
        } else {
            PayloadConfig::Text(
                r#"{"source":"wsblast","ts":"{{timestamp}}","seq":{{seq}}}"#.to_string(),
            )
        };

        let mut headers = HeaderMap::new();
        for h in cli.headers {
            let (name, value) = parse_header_pair(&h)?;
            headers.insert(name, value);
        }

        let slo = SloThresholds {
            max_p50: cli
                .max_p50
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p95: cli
                .max_p95
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p99: cli
                .max_p99
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p999: cli
                .max_p999
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_error_rate: cli.max_error_rate,
            min_throughput: cli.min_throughput,
        };

        if let Some(err_rate) = slo.max_error_rate {
            if !(0.0..=1.0).contains(&err_rate) {
                return Err(ConfigError::InvalidThreshold(format!(
                    "max_error_rate must be between 0.0 and 1.0, received {err_rate}"
                ))
                .into());
            }
        }

        Ok(Self {
            target_url,
            connections: cli.connections,
            duration,
            max_requests: cli.requests,
            rate_per_conn: cli.rate,
            payload,
            headers,
            subprotocol: cli.subprotocol,
            mode: cli.mode,
            connect_timeout,
            message_timeout,
            ping_interval,
            slo,
            output_format: cli.format,
            output_path: cli.output_file,
            tui: cli.tui,
            no_progress: cli.no_progress,
        })
    }
}

/// Parses concise human duration strings into standard [`Duration`].
///
/// Supported suffix multipliers: `us`/`µs` (microseconds), `ms` (milliseconds),
/// `s` (seconds), `m` (minutes), `h` (hours). Defaults to integer seconds if no suffix is present.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidDuration`] if the string is empty or contains non-numeric components.
pub fn parse_human_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::InvalidDuration("empty duration string".into()).into());
    }

    if let Some(val) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        let num: u64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_micros(num));
    }

    if let Some(val) = s.strip_suffix("ms") {
        let num: u64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_millis(num));
    }

    if let Some(val) = s.strip_suffix('s') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num));
    }

    if let Some(val) = s.strip_suffix('m') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num * 60.0));
    }

    if let Some(val) = s.strip_suffix('h') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num * 3600.0));
    }

    if let Ok(num) = s.parse::<u64>() {
        return Ok(Duration::from_secs(num));
    }

    Err(WsBlastError::Config(ConfigError::InvalidDuration(
        s.to_string(),
    )))
}

fn parse_header_pair(header: &str) -> Result<(HeaderName, HeaderValue)> {
    let parts: Vec<&str> = header.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(ConfigError::InvalidHeader(header.to_string()).into());
    }

    let name = HeaderName::from_str(parts[0].trim())
        .map_err(|_| ConfigError::InvalidHeader(format!("Invalid header name '{}'", parts[0])))?;

    let value = HeaderValue::from_str(parts[1].trim())
        .map_err(|_| ConfigError::InvalidHeader(format!("Invalid header value '{}'", parts[1])))?;

    Ok((name, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_human_durations() {
        assert_eq!(
            parse_human_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_human_duration("10s").unwrap(),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_human_duration("2m").unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_human_duration("100us").unwrap(),
            Duration::from_micros(100)
        );
        assert_eq!(parse_human_duration("0s").unwrap(), Duration::ZERO);
    }

    #[test]
    fn test_parse_header_pair() {
        let (name, value) = parse_header_pair("Authorization: Bearer mytoken123").unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer mytoken123");
    }
}
