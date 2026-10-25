//! A `GET /metrics` endpoint in the Prometheus text exposition format 0.0.4.
//!
//! The responder is a minimal HTTP/1.1 server on a Tokio listener: it reads
//! one request head, answers it and closes the connection. That is all a
//! Prometheus scrape needs, and it keeps the feature free of dependencies.

use super::{
    ACTIVE_CONNECTIONS, COUNTERS, ExportSnapshot, ExportSource, LATENCY, LATENCY_SAMPLES,
    MetricInfo, RUN_ELAPSED,
};
use crate::error::{ConfigError, Result};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Content type of the text exposition format.
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Largest request head accepted. A scrape request is a few hundred bytes.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long a client gets to send its request head and read the response.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections served at once; further ones are closed without a response.
const MAX_CONCURRENT: usize = 32;

/// Renders `snapshot` in the text exposition format.
pub(crate) fn render(snapshot: &ExportSnapshot) -> String {
    let mut out = String::with_capacity(2048);
    for counter in &COUNTERS {
        header(&mut out, &counter.info, "counter");
        let _ = writeln!(
            out,
            "{} {}",
            counter.info.prometheus,
            (counter.read)(&snapshot.counters)
        );
    }
    header(&mut out, &ACTIVE_CONNECTIONS, "gauge");
    let _ = writeln!(
        out,
        "{} {}",
        ACTIVE_CONNECTIONS.prometheus, snapshot.counters.active_connections
    );
    header(&mut out, &RUN_ELAPSED, "gauge");
    let _ = writeln!(
        out,
        "{} {}",
        RUN_ELAPSED.prometheus,
        snapshot.elapsed.as_secs_f64()
    );
    header(&mut out, &LATENCY, "gauge");
    if let Some(latency) = &snapshot.latency {
        for (quantile, secs) in latency.quantiles() {
            let _ = writeln!(
                out,
                "{}{{basis=\"{}\",quantile=\"{quantile}\"}} {secs}",
                LATENCY.prometheus, latency.basis
            );
        }
    }
    header(&mut out, &LATENCY_SAMPLES, "gauge");
    if let Some(latency) = &snapshot.latency {
        let _ = writeln!(
            out,
            "{}{{basis=\"{}\"}} {}",
            LATENCY_SAMPLES.prometheus, latency.basis, latency.count
        );
    }
    out
}

fn header(out: &mut String, info: &MetricInfo, kind: &str) {
    let _ = writeln!(out, "# HELP {} {}", info.prometheus, info.help);
    let _ = writeln!(out, "# TYPE {} {kind}", info.prometheus);
}

/// What a request head asks for.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Metrics,
    NotFound,
    BadRequest,
    TooLarge,
}

/// Routes a complete request head (everything up to the blank line).
fn route(head: &[u8]) -> Route {
    let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let Ok(line) = std::str::from_utf8(&head[..line_end]) else {
        return Route::BadRequest;
    };
    let mut parts = line.trim_end_matches('\r').split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Route::BadRequest;
    };
    if method.is_empty() || !target.starts_with('/') || !version.starts_with("HTTP/1.") {
        return Route::BadRequest;
    }
    let path = target.split('?').next().unwrap_or(target);
    if method == "GET" && path == "/metrics" {
        Route::Metrics
    } else {
        Route::NotFound
    }
}

/// Reads until the end of the request head. Returns `None` when the peer
/// closes or errors before sending a complete head.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head_end(&buf) {
            buf.truncate(end);
            return Ok(Some(buf));
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return Ok(Some(buf));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// End of the request head: after the first empty line.
fn head_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Serves one connection: reads one request head, answers it and returns.
async fn handle<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, source: &ExportSource) {
    let head = match tokio::time::timeout(IO_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(Some(head))) => head,
        // Closed, failed or timed out before a complete head: nothing to answer.
        _ => return,
    };
    let route = if head_end(&head).is_none() {
        Route::TooLarge
    } else {
        route(&head)
    };
    let (status, content_type, body) = match route {
        Route::Metrics => ("200 OK", CONTENT_TYPE, render(&source.snapshot())),
        Route::NotFound => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not Found\n".into(),
        ),
        Route::BadRequest => (
            "400 Bad Request",
            "text/plain; charset=utf-8",
            "Bad Request\n".into(),
        ),
        Route::TooLarge => (
            "431 Request Header Fields Too Large",
            "text/plain; charset=utf-8",
            "Request Header Fields Too Large\n".into(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(IO_TIMEOUT, async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    })
    .await;
}

/// The running endpoint. It stops when [`PrometheusServer::shutdown`] is
/// called or the value is dropped.
#[derive(Debug)]
pub(crate) struct PrometheusServer {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl PrometheusServer {
    /// Binds `addr` and starts serving `source`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] when the address cannot be
    /// bound, for example because the port is in use.
    pub(crate) async fn bind(addr: &str, source: Arc<ExportSource>) -> Result<Self> {
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            ConfigError::InvalidOption(format!(
                "--prometheus-listen cannot listen on '{addr}': {e}"
            ))
        })?;
        let addr = listener.local_addr().map_err(|e| {
            ConfigError::InvalidOption(format!(
                "--prometheus-listen cannot listen on '{addr}': {e}"
            ))
        })?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve(listener, source, cancel.clone()));
        Ok(Self { addr, cancel, task })
    }

    /// The address the endpoint listens on.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting connections and closes the listener.
    pub(crate) async fn shutdown(mut self) {
        self.cancel.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for PrometheusServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn serve(listener: TcpListener, source: Arc<ExportSource>, cancel: CancellationToken) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    loop {
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                // A failed accept (for example EMFILE) affects only that client.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            },
        };
        if in_flight.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT {
            in_flight.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let source = Arc::clone(&source);
        let in_flight = Arc::clone(&in_flight);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = handle(stream, &source) => {}
                _ = cancel.cancelled() => {}
            }
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::IntervalExport;
    use crate::metrics::{LiveMetrics, LiveSnapshot};
    use tokio::io::duplex;

    fn sample_snapshot() -> ExportSnapshot {
        ExportSnapshot {
            counters: LiveSnapshot {
                messages_sent: 12,
                messages_received: 11,
                bytes_sent: 1_200,
                bytes_received: 1_100,
                total_errors: 1,
                connections_established: 2,
                connections_failed: 3,
                reconnects: 4,
                active_connections: 2,
                ..LiveSnapshot::default()
            },
            elapsed: Duration::from_millis(1_250),
            latency: Some(IntervalExport {
                basis: "rtt",
                count: 9,
                p50_us: 1_500,
                p90_us: 2_000,
                p99_us: 12_000,
            }),
        }
    }

    #[test]
    fn exposition_has_help_type_and_a_value_for_every_metric() {
        let text = render(&sample_snapshot());
        let expected = [
            "# HELP wsblast_messages_sent_total Data frames written successfully.",
            "# TYPE wsblast_messages_sent_total counter",
            "wsblast_messages_sent_total 12",
            "wsblast_messages_received_total 11",
            "wsblast_bytes_sent_total 1200",
            "wsblast_bytes_received_total 1100",
            "wsblast_errors_total 1",
            "wsblast_connections_established_total 2",
            "wsblast_connections_failed_total 3",
            "wsblast_reconnects_total 4",
            "# TYPE wsblast_active_connections gauge",
            "wsblast_active_connections 2",
            "# TYPE wsblast_run_elapsed_seconds gauge",
            "wsblast_run_elapsed_seconds 1.25",
            "# TYPE wsblast_latency_seconds gauge",
            "wsblast_latency_seconds{basis=\"rtt\",quantile=\"0.5\"} 0.0015",
            "wsblast_latency_seconds{basis=\"rtt\",quantile=\"0.9\"} 0.002",
            "wsblast_latency_seconds{basis=\"rtt\",quantile=\"0.99\"} 0.012",
            "# TYPE wsblast_latency_samples gauge",
            "wsblast_latency_samples{basis=\"rtt\"} 9",
        ];
        let lines: Vec<&str> = text.lines().collect();
        for line in expected {
            assert!(lines.contains(&line), "missing {line:?} in\n{text}");
        }
        // Every family has exactly one HELP and one TYPE line, and every
        // sample line belongs to a declared family.
        let families: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(families.len(), 12);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("# HELP ")).count(),
            12
        );
        for sample in lines.iter().filter(|l| !l.starts_with('#')) {
            let name = sample.split(['{', ' ']).next().unwrap();
            assert!(families.contains(&name), "undeclared sample {sample:?}");
        }
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn latency_samples_are_omitted_until_an_interval_completes() {
        let mut snapshot = sample_snapshot();
        snapshot.latency = None;
        let text = render(&snapshot);
        assert!(text.contains("# TYPE wsblast_latency_seconds gauge"));
        assert!(!text.contains("wsblast_latency_seconds{"));
        assert!(!text.contains("wsblast_latency_samples{"));

        snapshot.latency = Some(IntervalExport {
            basis: "response_time",
            ..IntervalExport::default()
        });
        let text = render(&snapshot);
        assert!(!text.contains("wsblast_latency_seconds{"));
        assert!(text.contains("wsblast_latency_samples{basis=\"response_time\"} 0"));
    }

    #[test]
    fn requests_are_routed_by_method_and_path() {
        assert_eq!(route(b"GET /metrics HTTP/1.1\r\n\r\n"), Route::Metrics);
        assert_eq!(route(b"GET /metrics?x=1 HTTP/1.0\r\n\r\n"), Route::Metrics);
        assert_eq!(route(b"GET /metrics HTTP/1.1\n\n"), Route::Metrics);
        assert_eq!(route(b"GET / HTTP/1.1\r\n\r\n"), Route::NotFound);
        assert_eq!(route(b"POST /metrics HTTP/1.1\r\n\r\n"), Route::NotFound);
        assert_eq!(route(b"GET /metricsx HTTP/1.1\r\n\r\n"), Route::NotFound);
        assert_eq!(route(b"GET /metrics\r\n\r\n"), Route::BadRequest);
        assert_eq!(route(b"GET metrics HTTP/1.1\r\n\r\n"), Route::BadRequest);
        assert_eq!(route(b"GET /metrics SPDY/3\r\n\r\n"), Route::BadRequest);
        assert_eq!(route(b"GET  /metrics HTTP/1.1\r\n\r\n"), Route::BadRequest);
        assert_eq!(
            route(b"\xff\xfe /metrics HTTP/1.1\r\n\r\n"),
            Route::BadRequest
        );
        assert_eq!(route(b"\r\n\r\n"), Route::BadRequest);
    }

    /// Writes `chunks` to a handler one at a time and returns the response.
    async fn exchange(chunks: &[&[u8]]) -> String {
        let source = ExportSource::new(LiveMetrics::new());
        let (mut client, server) = duplex(64 * 1024);
        let handler = tokio::spawn(async move { handle(server, &source).await });
        for chunk in chunks {
            client.write_all(chunk).await.unwrap();
            client.flush().await.unwrap();
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handler.await.unwrap();
        response
    }

    #[tokio::test]
    async fn a_request_split_across_reads_is_answered() {
        let response =
            exchange(&[b"GE", b"T /met", b"rics HTTP/1.1\r\nHost: x\r", b"\n\r\n"]).await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains(&format!("Content-Type: {CONTENT_TYPE}\r\n")));
        assert!(response.contains("Connection: close\r\n"));
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(length, body.len());
        assert!(body.contains("wsblast_messages_sent_total 0\n"));
    }

    #[tokio::test]
    async fn unknown_paths_and_malformed_requests_get_error_statuses() {
        let response = exchange(&[b"GET /other HTTP/1.1\r\n\r\n"]).await;
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{response}"
        );

        let response = exchange(&[b"this is not http\r\n\r\n"]).await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );

        let huge = vec![b'a'; MAX_REQUEST_BYTES + 10];
        let response = exchange(&[b"GET /metrics HTTP/1.1\r\nX: ", &huge]).await;
        assert!(response.starts_with("HTTP/1.1 431 "), "{response}");
    }

    #[tokio::test]
    async fn a_client_that_closes_early_gets_no_response() {
        let source = ExportSource::new(LiveMetrics::new());
        let (mut client, server) = duplex(1024);
        client.write_all(b"GET /metr").await.unwrap();
        client.shutdown().await.unwrap();
        handle(server, &source).await;
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.is_empty());
    }

    #[tokio::test]
    async fn the_server_answers_over_tcp_and_stops_on_shutdown() {
        let source = ExportSource::new(LiveMetrics::new());
        let server = PrometheusServer::bind("127.0.0.1:0", source).await.unwrap();
        let addr = server.local_addr();

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));

        // The port is taken while the server runs.
        let err = PrometheusServer::bind(&addr.to_string(), ExportSource::new(LiveMetrics::new()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--prometheus-listen"), "{err}");

        server.shutdown().await;
        assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    }
}
