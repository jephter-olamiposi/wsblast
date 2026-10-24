//! End-to-end integration test suite verifying load orchestration, metrics, SLOs, and error taxonomy.

use futures_util::{SinkExt, StreamExt};
use http::header::HeaderMap;
#[cfg(not(feature = "auth"))]
use http::header::{HeaderName, HeaderValue};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;
use wsblast::cli::{OutputFormat, TestMode};
#[cfg(feature = "graphql-ws")]
use wsblast::config::GraphQLWSConfig;
#[cfg(feature = "scenario")]
use wsblast::config::{
    ActionConfig, PhaseConfig, ScenarioAction, ScenarioConfig, ScenarioMeta, ScenarioPhase,
};
use wsblast::config::{LoadTestConfig, PayloadConfig, SloThresholds};
use wsblast::error::ErrorCategory;
use wsblast::report::{evaluate_slos, render_report};
use wsblast::runner::Runner;
#[cfg(feature = "scenario")]
use wsblast::runner::ScenarioExecutor;

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

/// Echoes the client's requested WebSocket subprotocol during the handshake.
#[cfg(feature = "graphql-ws")]
#[allow(clippy::result_large_err)]
fn echo_graphql_subprotocol(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> std::result::Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    if let Some(protocol) = request.headers().get("sec-websocket-protocol") {
        response
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.clone());
    }
    Ok(response)
}

/// Spawns an in-process GraphQL WS server that acknowledges one subscription,
/// emits one result, and completes.
#[cfg(feature = "graphql-ws")]
async fn spawn_graphql_ws_server() -> (Url, tokio_util::sync::CancellationToken) {
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
                        let ws_stream = match tokio_tungstenite::accept_hdr_async(
                            stream,
                            echo_graphql_subprotocol,
                        )
                        .await
                        {
                            Ok(stream) => stream,
                            Err(_) => return,
                        };
                        let (mut sink, mut reader) = ws_stream.split();
                        while let Some(Ok(msg)) = reader.next().await {
                            let Message::Text(text) = msg else {
                                continue;
                            };
                            let Ok(value) =
                                serde_json::from_str::<serde_json::Value>(&text)
                            else {
                                continue;
                            };
                            match value.get("type").and_then(|kind| kind.as_str()) {
                                Some("connection_init") => {
                                    let ack = serde_json::json!({"type": "connection_ack"});
                                    let _ = sink.send(Message::Text(ack.to_string().into())).await;
                                }
                                Some("subscribe") | Some("start") => {
                                    let id = value
                                        .get("id")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    let next = serde_json::json!({
                                        "type": "next",
                                        "id": id,
                                        "payload": {"data": {"price": 42}}
                                    });
                                    let _ = sink.send(Message::Text(next.to_string().into())).await;
                                    let complete = serde_json::json!({
                                        "type": "complete",
                                        "id": id
                                    });
                                    let _ =
                                        sink.send(Message::Text(complete.to_string().into())).await;
                                }
                                _ => {}
                            }
                        }
                    });
                }
                _ = cancel_server.cancelled() => break,
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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config.clone());
    let metrics = runner.run().await.unwrap();

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
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
    let metrics = runner.run().await.unwrap();

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Json,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config.clone());
    let metrics = runner.run().await.unwrap();
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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

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
    #[cfg(feature = "auth")]
    wsblast::auth::apply_auth_header(
        &mut headers,
        &wsblast::config::AuthConfig::Static {
            token: "secret-token-xyz".to_string(),
            header: "Authorization".to_string(),
        },
    )
    .unwrap();
    #[cfg(not(feature = "auth"))]
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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

    assert_eq!(metrics.total_connections_established, 1);
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(metrics.total_messages_recv > 0);

    cancel.cancel();
}

#[cfg(feature = "graphql-ws")]
#[tokio::test]
async fn test_graphql_ws_subscription_lifecycle() {
    let (target_url, cancel) = spawn_graphql_ws_server().await;

    let config = LoadTestConfig {
        target_url,
        connections: 1,
        ramp_rate: 0,
        duration: Duration::from_millis(500),
        max_requests: None,
        rate_per_conn: 10,
        payload: PayloadConfig::from_text("{}".to_string()),
        headers: HeaderMap::new(),
        subprotocol: Some("graphql-transport-ws".to_string()),
        mode: TestMode::GraphQLWS,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        graphql_ws: Some(GraphQLWSConfig {
            operation_name: Some("Price".to_string()),
            operation: "subscription Price { price }".to_string(),
            variables: None,
            extensions: None,
        }),
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let metrics = Runner::new(config).run().await.unwrap();

    assert_eq!(
        metrics.total_connections_established, 1,
        "unexpected metrics: {metrics:?}"
    );
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(
        metrics.total_messages_sent >= 2,
        "unexpected metrics: {metrics:?}"
    );
    assert!(metrics.total_messages_recv >= 1);
    assert_eq!(metrics.error_rate, 0.0);

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let start = std::time::Instant::now();
    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();
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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

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
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    };

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

    assert_eq!(metrics.total_connections_established, 1);
    assert!(metrics.total_messages_recv >= 3);
    assert_eq!(metrics.total_messages_sent, 0);

    cancel.cancel();
}

#[cfg(feature = "scenario")]
fn scenario_base_config(target_url: Url) -> LoadTestConfig {
    LoadTestConfig {
        target_url,
        connections: 1,
        ramp_rate: 0,
        duration: Duration::from_millis(300),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("default".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    }
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_action_script_echoes_and_closes() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let phase = ScenarioPhase {
        name: "scripted".to_string(),
        target: Some(target_url.clone()),
        duration: Duration::from_millis(300),
        connections: 3,
        ramp_rate: 0,
        rate_per_conn: 50,
        payload: None,
        think_time: Duration::from_millis(5),
        actions: vec![
            ScenarioAction::Send {
                payload: None,
                count: 2,
            },
            ScenarioAction::Wait(Duration::from_millis(10)),
            ScenarioAction::Ping(Duration::from_millis(20)),
            ScenarioAction::Close {
                code: 1000,
                reason: "finished".to_string(),
            },
        ],
    };

    let runner = Runner::with_scenario(scenario_base_config(target_url), phase);
    let metrics = runner.run().await.unwrap();

    assert_eq!(metrics.total_connections_attempted, 3);
    assert_eq!(metrics.total_connections_established, 3);
    assert_eq!(metrics.total_connections_failed, 0);
    assert_eq!(metrics.total_messages_sent, 6);
    assert_eq!(metrics.total_messages_recv, 6);
    assert_eq!(metrics.error_rate, 0.0);
    assert!(metrics.message_rtt.p50_us > 0);

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_executor_runs_every_phase() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let phase = |name: &str, connections: usize| PhaseConfig {
        name: name.to_string(),
        target: Some(target_url.to_string()),
        duration: "80ms".to_string(),
        connections,
        ramp_rate: 0,
        rate_per_conn: 0,
        payload: None,
        think_time: None,
        actions: vec![
            ActionConfig::Send {
                payload: None,
                count: Some(1),
            },
            ActionConfig::Close {
                code: Some(1000),
                reason: None,
            },
        ],
    };

    let scenario = ScenarioConfig {
        version: "1.0".to_string(),
        target: None,
        metadata: ScenarioMeta {
            name: "two-phase".to_string(),
            description: None,
            tags: None,
        },
        phases: vec![phase("warmup", 2), phase("sustain", 3)],
    };

    let executor =
        ScenarioExecutor::new(&scenario, scenario_base_config(target_url), None, None).unwrap();
    let (worker_metrics, elapsed, connections) = executor.run().await.unwrap();

    assert_eq!(connections, 5);
    assert_eq!(worker_metrics.len(), 5);
    assert!(elapsed >= Duration::from_millis(160));
    assert!(
        worker_metrics
            .iter()
            .all(|metrics| metrics.messages_sent > 0)
    );

    cancel_server.cancel();
}

/// Baseline echo configuration for regression tests that only vary a few fields.
fn echo_test_config(target_url: Url) -> LoadTestConfig {
    LoadTestConfig {
        target_url,
        connections: 1,
        ramp_rate: 0,
        duration: Duration::from_secs(1),
        max_requests: None,
        rate_per_conn: 0,
        payload: PayloadConfig::from_text("keepalive-test".to_string()),
        headers: HeaderMap::new(),
        subprotocol: None,
        mode: TestMode::Echo,
        connect_timeout: Duration::from_secs(2),
        message_timeout: Duration::from_secs(2),
        ping_interval: Duration::ZERO,
        #[cfg(feature = "compression")]
        compression: wsblast::config::CompressionConfig {
            enabled: false,
            level: 6,
        },
        #[cfg(feature = "auth")]
        auth: None,
        #[cfg(feature = "graphql-ws")]
        graphql_ws: None,
        #[cfg(feature = "capture")]
        capture: wsblast::config::CaptureConfig {
            enabled: false,
            output_path: std::path::PathBuf::from("capture.pcapng"),
            format: wsblast::config::CaptureFormat::PcapNg,
            max_frames: 0,
        },
        #[cfg(feature = "chaos")]
        chaos: wsblast::config::ChaosConfig {
            enabled: false,
            latency_ms: None,
            loss_percent: None,
            corrupt_percent: None,
            close_percent: None,
        },
        slo: SloThresholds::default(),
        output_format: OutputFormat::Text,
        output_path: None,
        tui: false,
        no_progress: true,
    }
}

#[tokio::test]
async fn test_echo_ping_interval_does_not_throttle_unpaced_load() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let mut config = echo_test_config(target_url);
    config.connections = 2;
    config.duration = Duration::from_secs(1);

    let baseline = Runner::new(config.clone()).run().await.unwrap();

    config.ping_interval = Duration::from_millis(200);
    let with_ping = Runner::new(config).run().await.unwrap();

    assert!(
        with_ping.total_messages_sent > 500,
        "keepalive pings throttled unpaced echo load: {} sent with ping vs {} without",
        with_ping.total_messages_sent,
        baseline.total_messages_sent
    );
    assert!(
        with_ping.total_messages_sent * 4 > baseline.total_messages_sent,
        "keepalive pings cut throughput by more than 4x: {} sent with ping vs {} without",
        with_ping.total_messages_sent,
        baseline.total_messages_sent
    );
    assert_eq!(with_ping.total_messages_recv, with_ping.total_messages_sent);
    assert_eq!(with_ping.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_echo_ping_tick_does_not_add_paced_data_sends() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let mut config = echo_test_config(target_url);
    config.rate_per_conn = 20;
    config.ping_interval = Duration::from_millis(25);
    config.duration = Duration::from_secs(1);

    let metrics = Runner::new(config).run().await.unwrap();

    // 20 msg/s for 1s plus the interval's immediate first tick.
    assert!(
        metrics.total_messages_sent <= 23,
        "ping ticks produced extra data sends: {} sent at 20 msg/s over 1s",
        metrics.total_messages_sent
    );
    assert!(metrics.total_messages_sent >= 15);
    assert_eq!(metrics.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_stream_and_listen_modes_with_ping_interval() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let mut config = echo_test_config(target_url.clone());
    config.mode = TestMode::Stream;
    config.rate_per_conn = 200;
    config.ping_interval = Duration::from_millis(50);
    config.duration = Duration::from_millis(500);
    let stream = Runner::new(config).run().await.unwrap();
    assert_eq!(stream.total_connections_established, 1);
    assert!(stream.total_messages_sent > 50);
    assert_eq!(stream.error_rate, 0.0);

    let mut config = echo_test_config(target_url);
    config.mode = TestMode::Listen;
    config.ping_interval = Duration::from_millis(50);
    config.duration = Duration::from_millis(300);
    let listen = Runner::new(config).run().await.unwrap();
    assert_eq!(listen.total_connections_established, 1);
    assert_eq!(listen.total_messages_sent, 0);
    assert_eq!(listen.error_rate, 0.0);

    cancel_server.cancel();
}
