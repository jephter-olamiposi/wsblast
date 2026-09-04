//! End-to-end integration test suite verifying load orchestration, metrics, SLOs, and error taxonomy.

use futures_util::{SinkExt, StreamExt};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;
use wsblast::cli::{OutputFormat, TestMode};
use wsblast::config::{LoadTestConfig, PayloadConfig, SloThresholds};
use wsblast::error::ErrorCategory;
use wsblast::report::{evaluate_slos, render_report};
use wsblast::runner::Runner;

/// Spawns an in-process WebSocket echo server on an ephemeral OS port.
async fn spawn_echo_server() -> (Url, tokio_util::sync::CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let target_url = Url::parse(&format!("ws://127.0.0.1:{}", addr.port())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        if let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await {
                            let (mut sink, mut reader) = ws_stream.split();
                            while let Some(Ok(msg)) = reader.next().await {
                                match msg {
                                    Message::Text(t) => {
                                        let _ = sink.send(Message::Text(t)).await;
                                    }
                                    Message::Binary(b) => {
                                        let _ = sink.send(Message::Binary(b)).await;
                                    }
                                    Message::Ping(p) => {
                                        let _ = sink.send(Message::Pong(p)).await;
                                    }
                                    Message::Close(_) => {
                                        let _ = sink.send(Message::Close(None)).await;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    });
                }
                _ = cancel_server.cancelled() => {
                    break;
                }
            }
        }
    });

    (target_url, cancel)
}

#[tokio::test]
async fn test_echo_mode_load_run() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 10,
        ramp_rate: 0,
        duration: Duration::from_millis(800),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text(r#"{"msg":"test","seq":{{seq}}}"#.to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config.clone());
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_attempted, 10);
    assert_eq!(metrics.total_connections_established, 10);
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(metrics.total_messages_sent > 50);
    assert_eq!(metrics.total_messages_recv, metrics.total_messages_sent);
    assert_eq!(metrics.error_rate, 0.0);
    assert!(metrics.throughput_msg_per_sec > 10.0);
    assert!(metrics.message_rtt.p50_us > 0);
    assert!(metrics.handshake_latency.p50_us > 0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_slo_evaluation_pass_and_fail() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 5,
        ramp_rate: 0,
        duration: Duration::from_millis(500),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("ping".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds {
            max_p50: Some(Duration::from_secs(1)),
            max_p95: Some(Duration::from_secs(2)),
            max_p99: Some(Duration::from_secs(3)),
            max_p999: None,
            max_error_rate: Some(0.01),
            min_throughput: Some(1.0),
        },
        output_format: OutputFormat::Json,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config.clone());
    let metrics = runner.run().await;

    let pass_eval = evaluate_slos(&config.slo, &metrics);
    assert!(pass_eval.passed);
    assert_eq!(pass_eval.checks.len(), 5);

    let failing_slo = SloThresholds {
        max_p50: Some(Duration::from_nanos(1)),
        ..Default::default()
    };
    let fail_eval = evaluate_slos(&failing_slo, &metrics);
    assert!(!fail_eval.passed);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_json_and_markdown_report_formatting() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let mut config = LoadTestConfig {
        target_url,
        connections: 2,
        ramp_rate: 0,
        duration: Duration::from_millis(300),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("hello".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(1),
        message_timeout: Duration::from_secs(1),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Json,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config.clone());
    let metrics = runner.run().await;
    let slo = evaluate_slos(&config.slo, &metrics);

    let json_str = render_report(&config, &metrics, &slo);
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("Valid JSON report");
    assert_eq!(parsed["schema_version"], "1.0.0");
    assert!(
        parsed["metrics"]["throughput_msg_per_sec"]
            .as_f64()
            .unwrap()
            > 0.0
    );

    config.output_format = OutputFormat::Markdown;
    let md_str = render_report(&config, &metrics, &slo);
    assert!(md_str.contains("# `wsblast` WebSocket Benchmark Report"));
    assert!(md_str.contains("Latency Distribution"));

    cancel_server.cancel();
}

#[tokio::test]
async fn test_error_taxonomy_when_target_offline() {
    let dead_url = Url::parse("ws://127.0.0.1:59998").unwrap();

    let config = LoadTestConfig {
        target_url: dead_url,
        connections: 4,
        ramp_rate: 0,
        duration: Duration::from_millis(300),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("test".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_millis(200),
        message_timeout: Duration::from_millis(200),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_attempted, 4);
    assert_eq!(metrics.total_connections_established, 0);
    assert_eq!(metrics.total_connections_failed, 4);
    assert_eq!(metrics.total_messages_sent, 0);

    let tcp_errs = metrics
        .error_breakdown
        .get(&ErrorCategory::TcpConnect)
        .copied()
        .unwrap_or(0);
    let timeout_errs = metrics
        .error_breakdown
        .get(&ErrorCategory::Timeout)
        .copied()
        .unwrap_or(0);
    assert!(tcp_errs + timeout_errs == 4);
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn test_custom_headers_propagation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let target_url = Url::parse(&format!("ws://127.0.0.1:{}", addr.port())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();

    tokio::spawn(async move {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                let callback = |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    if req.headers().get("Authorization").map(|v| v.to_str().unwrap_or("")) == Some("Bearer secret-token-xyz") {
                        Ok(resp)
                    } else {
                        let mut error_resp = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some("Unauthorized".into()));
                        *error_resp.status_mut() = http::StatusCode::UNAUTHORIZED;
                        Err(error_resp)
                    }
                };

                if let Ok(ws_stream) = tokio_tungstenite::accept_hdr_async(stream, callback).await {
                    let (mut sink, mut reader) = ws_stream.split();
                    while let Some(Ok(msg)) = reader.next().await {
                        if let Message::Text(t) = msg {
                            let _ = sink.send(Message::Text(t)).await;
                        }
                    }
                }
            }
            _ = cancel_server.cancelled() => {}
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("Bearer secret-token-xyz"),
    );

    let config = LoadTestConfig {
        target_url,
        connections: 1,
        ramp_rate: 0,
        duration: Duration::from_millis(500),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("auth-test".to_string()),
        headers,
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(1),
        message_timeout: Duration::from_secs(1),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_established, 1);
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(metrics.total_messages_recv > 0);

    cancel.cancel();
}

#[tokio::test]
async fn test_connection_ramp_rate() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 5,
        ramp_rate: 20, // 20 conns/sec -> 50ms delay between worker spawns
        duration: Duration::from_millis(600),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("static-frame".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_established, 5);
    assert!(metrics.total_messages_sent > 0);

    cancel_server.cancel();
}

#[test]
fn test_payload_config_classification() {
    let static_p = PayloadConfig::from_text("plain text without macros".to_string());
    assert!(matches!(static_p, PayloadConfig::StaticText(_)));

    let dynamic_p = PayloadConfig::from_text(r#"{"seq": {{seq}}}"#.to_string());
    assert!(matches!(dynamic_p, PayloadConfig::DynamicText(_)));
}

#[tokio::test]
async fn test_requests_limit_enforcement() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // Set duration to 30 seconds, but cap requests at 30
    let config = LoadTestConfig {
        target_url,
        connections: 3,
        ramp_rate: 0,
        duration: Duration::from_secs(30),
        max_requests: Some(30),
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("req-limit-test".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let start = std::time::Instant::now();
    let runner = Runner::new(config);
    let metrics = runner.run().await;
    let elapsed = start.elapsed();

    // Must finish well under the 30s configured duration
    assert!(elapsed < Duration::from_secs(5));
    assert!(metrics.total_messages_sent >= 30);
    assert_eq!(metrics.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_stream_mode_load_run() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 4,
        ramp_rate: 0,
        duration: Duration::from_millis(500),
        max_requests: None,
        rate_per_conn: 200,
        payload: PayloadConfig::from_text("stream-data".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Stream,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_established, 4);
    assert!(metrics.total_messages_sent > 10);
    assert_eq!(metrics.total_connections_failed, 0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_binary_payload_dispatch() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 2,
        ramp_rate: 0,
        duration: Duration::from_millis(400),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::Binary(bytes::Bytes::from_static(b"\x00\x01\x02\x03\x04\x05")),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_established, 2);
    assert!(metrics.total_messages_sent > 10);
    assert_eq!(metrics.total_messages_recv, metrics.total_messages_sent);
    assert!(metrics.total_bytes_sent > 0);
    assert_eq!(metrics.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_listen_mode_with_server_broadcast_and_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let target_url = Url::parse(&format!("ws://127.0.0.1:{}", addr.port())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        if let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await {
                            let (mut sink, mut reader) = ws_stream.split();
                            // Push 3 broadcast messages
                            for i in 0..3 {
                                let _ = sink.send(Message::Text(format!("broadcast-{i}").into())).await;
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            // Send Ping to verify client responds with Pong per RFC 6455
                            let _ = sink.send(Message::Ping(bytes::Bytes::from_static(b"keepalive"))).await;
                            // Await Pong or client close
                            while let Some(Ok(msg)) = reader.next().await {
                                if let Message::Pong(_) = msg {
                                    break;
                                }
                            }
                        }
                    });
                }
                _ = cancel_server.cancelled() => {
                    break;
                }
            }
        }
    });

    let config = LoadTestConfig {
        target_url,
        connections: 1,
        ramp_rate: 0,
        duration: Duration::from_millis(500),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("unused".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Listen,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await;

    assert_eq!(metrics.total_connections_established, 1);
    assert!(metrics.total_messages_recv >= 3);
    assert_eq!(metrics.total_messages_sent, 0);

    cancel.cancel();
}
