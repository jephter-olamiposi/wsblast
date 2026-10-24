//! End-to-end integration test suite verifying load orchestration, metrics, SLOs, and error taxonomy.

use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;
use wsblast::{
    AggregatedMetrics, ErrorCategory, LoadTestConfig, LoadTestConfigBuilder, OutputFormat,
    ReplyMatch, Runner, SloThresholds, TestMode, evaluate_slos, render_report,
};
#[cfg(feature = "scenario")]
use wsblast::{ScenarioConfig, ScenarioExecutor};

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

/// Asserts every reply was matched to a request, allowing one unanswered
/// request per connection that was still in flight when the run was cancelled.
fn assert_in_flight_at_most(metrics: &AggregatedMetrics, connections: u64) {
    assert!(
        metrics.total_messages_recv <= metrics.total_messages_sent
            && metrics.total_messages_sent - metrics.total_messages_recv <= connections,
        "sent {} / received {} exceeds {connections} in-flight requests",
        metrics.total_messages_sent,
        metrics.total_messages_recv
    );
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
/// emits one result, and completes it when `send_complete` is set.
#[cfg(feature = "graphql-ws")]
async fn spawn_graphql_ws_server(
    send_complete: bool,
) -> (Url, tokio_util::sync::CancellationToken) {
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
                                    if !send_complete {
                                        continue;
                                    }
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

/// Echo configuration shared by every test: one connection for one second
/// with 2 second timeouts and no progress bar. Tests override what they vary.
fn echo_test_config(target_url: &Url) -> LoadTestConfigBuilder {
    LoadTestConfig::builder(target_url.as_str())
        .connections(1)
        .duration(Duration::from_secs(1))
        .payload_text("keepalive-test")
        .connect_timeout(Duration::from_secs(2))
        .message_timeout(Duration::from_secs(2))
}

async fn run(config: LoadTestConfigBuilder) -> AggregatedMetrics {
    Runner::new(config.build().unwrap()).run().await.unwrap()
}

#[tokio::test]
async fn test_echo_mode_load_run() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(10)
        .duration(Duration::from_millis(800))
        .payload_text(r#"{"msg":"test","seq":{{seq}}}"#))
    .await;

    assert_eq!(metrics.total_connections_attempted, 10);
    assert_eq!(metrics.total_connections_established, 10);
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(metrics.total_messages_sent > 50);
    assert_in_flight_at_most(&metrics, 10);
    assert_eq!(metrics.error_rate, 0.0);
    assert!(metrics.throughput_msg_per_sec > 10.0);
    assert!(metrics.message_rtt.p50_us > 0);
    assert!(metrics.handshake_latency.p50_us > 0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_slo_evaluation_pass_and_fail() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = echo_test_config(&target_url)
        .connections(5)
        .duration(Duration::from_millis(500))
        .payload_text("ping")
        .slo(
            SloThresholds::new()
                .with_max_p50(Duration::from_secs(1))
                .with_max_p95(Duration::from_secs(2))
                .with_max_p99(Duration::from_secs(3))
                .with_max_error_rate(0.01)
                .with_min_throughput(1.0),
        )
        .output_format(OutputFormat::Json)
        .build()
        .unwrap();

    let metrics = Runner::new(config.clone()).run().await.unwrap();

    let pass_eval = evaluate_slos(config.slo(), &metrics);
    assert!(pass_eval.passed);
    assert_eq!(pass_eval.checks.len(), 5);

    let failing_slo = SloThresholds::new().with_max_p50(Duration::from_nanos(1));
    let fail_eval = evaluate_slos(&failing_slo, &metrics);
    assert!(!fail_eval.passed);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_json_and_markdown_report_formatting() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let builder = echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_millis(300))
        .payload_text("hello")
        .connect_timeout(Duration::from_secs(1))
        .message_timeout(Duration::from_secs(1));
    let config = builder
        .clone()
        .output_format(OutputFormat::Json)
        .build()
        .unwrap();

    let metrics = Runner::new(config.clone()).run().await.unwrap();
    let slo = evaluate_slos(config.slo(), &metrics);

    let json_str = render_report(&config, &metrics, &slo);
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("Valid JSON report");
    assert_eq!(parsed["schema_version"], "1.0.0");
    assert!(
        parsed["metrics"]["throughput_msg_per_sec"]
            .as_f64()
            .unwrap()
            > 0.0
    );

    let config = builder
        .output_format(OutputFormat::Markdown)
        .build()
        .unwrap();
    let md_str = render_report(&config, &metrics, &slo);
    assert!(md_str.contains("# `wsblast` WebSocket Benchmark Report"));
    assert!(md_str.contains("Latency Distribution"));

    cancel_server.cancel();
}

#[tokio::test]
async fn test_error_taxonomy_when_target_offline() {
    let dead_url = Url::parse("ws://127.0.0.1:59998").unwrap();

    let config = echo_test_config(&dead_url)
        .connections(4)
        .duration(Duration::from_millis(300))
        .payload_text("test")
        .connect_timeout(Duration::from_millis(200))
        .message_timeout(Duration::from_millis(200));

    let connect_errors = |metrics: &AggregatedMetrics| {
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
        let total_errs: u64 = metrics.error_breakdown.values().sum();
        assert_eq!(tcp_errs + timeout_errs, total_errs);
        total_errs
    };

    // Workers retry with backoff until the duration ends, so every slot logs
    // at least one connect failure and the run still stops on schedule.
    let start = std::time::Instant::now();
    let metrics = run(config.clone()).await;
    assert!(start.elapsed() < Duration::from_secs(2));

    assert_eq!(metrics.total_connections_attempted, 4);
    assert_eq!(metrics.total_connections_established, 0);
    assert_eq!(metrics.total_connections_failed, 4);
    assert_eq!(metrics.total_reconnects, 0);
    assert_eq!(metrics.total_messages_sent, 0);
    assert!(connect_errors(&metrics) >= 4);

    // Without reconnects each worker makes exactly one attempt.
    let metrics = run(config.reconnect(false)).await;
    assert_eq!(metrics.total_connections_failed, 4);
    assert_eq!(connect_errors(&metrics), 4);
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

    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_millis(500))
        .payload_text("auth-test")
        .connect_timeout(Duration::from_secs(1))
        .message_timeout(Duration::from_secs(1))
        .header("Authorization", "Bearer secret-token-xyz"))
    .await;

    assert_eq!(metrics.total_connections_established, 1);
    assert_eq!(metrics.total_connections_failed, 0);
    assert!(metrics.total_messages_recv > 0);

    cancel.cancel();
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn test_repeated_headers_reach_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<(Vec<String>, Vec<String>)>();

    tokio::spawn(async move {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                let callback = move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    let values = |name: &str| {
                        req.headers()
                            .get_all(name)
                            .iter()
                            .map(|v| v.to_str().unwrap_or("").to_string())
                            .collect()
                    };
                    let _ = seen_tx.send((values("x-test"), values("host")));
                    Ok(resp)
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

    let target = format!("ws://127.0.0.1:{}", addr.port());
    let config = LoadTestConfig::builder(target)
        .connections(1)
        .duration(Duration::from_millis(300))
        .header("X-Test", "a")
        .header("X-Test", "b")
        .header("Host", "wsblast.test");

    let metrics = run(config).await;
    assert_eq!(metrics.total_connections_established, 1);

    let (x_test, host) = tokio::time::timeout(Duration::from_secs(1), seen_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(x_test, ["a", "b"]);
    assert_eq!(host, ["wsblast.test"]);

    cancel.cancel();
}

#[test]
fn test_builder_rejects_invalid_settings() {
    let target = "ws://127.0.0.1:9001";
    let message = |builder: LoadTestConfigBuilder| builder.build().unwrap_err().to_string();

    assert!(message(LoadTestConfig::builder("http://127.0.0.1:9001")).contains("scheme"));
    assert!(message(LoadTestConfig::builder("not a url")).contains("Invalid target URL"));
    assert!(message(LoadTestConfig::builder(target).connections(0)).contains("greater than zero"));
    assert!(message(LoadTestConfig::builder(target).max_requests(0)).contains("requests limit"));
    assert!(message(LoadTestConfig::builder(target).header("bad header", "x")).contains("header"));
    assert!(
        message(LoadTestConfig::builder(target).slo(SloThresholds::new().with_max_error_rate(1.5)))
            .contains("max_error_rate")
    );
    assert!(
        message(
            LoadTestConfig::builder(target)
                .mode(TestMode::Stream)
                .slo(SloThresholds::new().with_max_p99(Duration::from_millis(5)))
        )
        .contains("require --mode echo")
    );
    assert!(
        message(
            LoadTestConfig::builder(target)
                .payload_text("ping")
                .reply_match("json:/seq".parse().unwrap())
        )
        .contains("{{seq}}")
    );
    assert!(
        message(
            LoadTestConfig::builder(target)
                .reply_match(ReplyMatch::Exact)
                .mode(TestMode::Listen)
        )
        .contains("--mode echo")
    );
}

#[test]
fn test_builder_defaults_match_the_command_line() {
    let config = LoadTestConfig::builder("ws://127.0.0.1:9001")
        .build()
        .unwrap();

    assert_eq!(config.target_url().as_str(), "ws://127.0.0.1:9001/");
    assert_eq!(config.connections(), 50);
    assert_eq!(config.duration(), Duration::from_secs(10));
    assert_eq!(config.ramp_rate(), 0);
    assert_eq!(config.rate_per_connection(), 0);
    assert_eq!(config.max_requests(), None);
    assert_eq!(config.mode(), TestMode::Echo);
    assert_eq!(config.connect_timeout(), Duration::from_secs(5));
    assert_eq!(config.message_timeout(), Duration::from_secs(5));
    assert_eq!(config.ping_interval(), Duration::ZERO);
    assert_eq!(config.reply_match(), &ReplyMatch::First);
    assert_eq!(config.output_format(), OutputFormat::Text);
    assert_eq!(config.slo(), &SloThresholds::default());
    assert!(config.reconnect());
    assert!(!config.progress());
    assert!(config.headers().is_empty());
    assert_eq!(config.subprotocol(), None);
}

#[cfg(feature = "graphql-ws")]
#[tokio::test]
async fn test_graphql_ws_subscription_lifecycle() {
    let (target_url, cancel) = spawn_graphql_ws_server(true).await;

    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_millis(500))
        .rate_per_connection(10)
        .payload_text("{}")
        .subprotocol("graphql-transport-ws")
        .mode(TestMode::GraphQLWS)
        .graphql_operation_name("Price")
        .graphql_operation("subscription Price { price }"))
    .await;

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

#[cfg(feature = "graphql-ws")]
#[test]
fn test_builder_validates_graphql_options() {
    let target = "ws://127.0.0.1:9001";
    let message = |builder: LoadTestConfigBuilder| builder.build().unwrap_err().to_string();

    assert!(
        message(LoadTestConfig::builder(target).graphql_operation("subscription { a }"))
            .contains("--mode graphql-ws")
    );
    assert!(
        message(LoadTestConfig::builder(target).mode(TestMode::GraphQLWS))
            .contains("--graphql-ws-operation")
    );
    assert!(
        message(
            LoadTestConfig::builder(target)
                .mode(TestMode::GraphQLWS)
                .graphql_operation("subscription { a }")
                .graphql_variables(serde_json::json!([1, 2]))
        )
        .contains("JSON object")
    );
}

#[tokio::test]
async fn test_connection_ramp_rate() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // 20 conns/sec -> 50ms delay between worker spawns
    let metrics = run(echo_test_config(&target_url)
        .connections(5)
        .ramp_rate(20)
        .duration(Duration::from_millis(600))
        .payload_text("static-frame"))
    .await;

    assert_eq!(metrics.total_connections_established, 5);
    assert!(metrics.total_messages_sent > 0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_duration_includes_connection_ramp() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = echo_test_config(&target_url)
        .connections(5)
        .ramp_rate(5)
        .duration(Duration::from_millis(600))
        .payload_text("static-frame");

    let started = std::time::Instant::now();
    let metrics = run(config).await;
    let wall = started.elapsed();

    assert!(
        wall < Duration::from_millis(900),
        "run took {wall:?}, expected the 600ms duration to include the ramp"
    );
    assert!(
        metrics.elapsed >= Duration::from_millis(550)
            && metrics.elapsed < Duration::from_millis(800),
        "elapsed {:?} should track the 600ms duration",
        metrics.elapsed
    );
    assert!(metrics.total_connections_established >= 1);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_slots_the_ramp_never_reaches_are_not_failures() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(10)
        .ramp_rate(5)
        .duration(Duration::from_millis(600)))
    .await;

    assert_eq!(metrics.total_connections_attempted, 10);
    assert!(
        (1..=4).contains(&metrics.total_connections_established),
        "established {}",
        metrics.total_connections_established
    );
    assert_eq!(
        metrics.total_connections_failed, 0,
        "slots the ramp never reached were reported as failed"
    );
    assert_eq!(
        metrics.total_connections_established
            + metrics.total_connections_failed
            + metrics.total_connections_not_started,
        metrics.total_connections_attempted
    );
    assert!(metrics.total_connections_not_started >= 6);
    assert!(metrics.error_breakdown.is_empty());

    cancel_server.cancel();
}

#[tokio::test]
async fn test_progress_bar_does_not_extend_elapsed() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // 250ms sits between two progress refreshes, where a fixed sleep in the
    // progress loop would overshoot by about 50ms.
    let duration = Duration::from_millis(250);
    for progress in [false, true] {
        let config = echo_test_config(&target_url)
            .duration(duration)
            .progress(progress)
            .build()
            .unwrap();
        let metrics = Runner::new(config)
            .with_signal_handling(false)
            .run()
            .await
            .unwrap();

        let overshoot = metrics.elapsed.saturating_sub(duration);
        assert!(
            metrics.elapsed >= duration && overshoot < Duration::from_millis(20),
            "progress={progress}: elapsed {:?} for a {duration:?} run",
            metrics.elapsed
        );
    }

    cancel_server.cancel();
}

#[tokio::test]
async fn test_requests_limit_enforcement() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // Set duration to 30 seconds, but cap requests at 30
    let config = echo_test_config(&target_url)
        .connections(3)
        .duration(Duration::from_secs(30))
        .max_requests(30)
        .payload_text("req-limit-test");

    let start = std::time::Instant::now();
    let metrics = run(config).await;
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

    let metrics = run(echo_test_config(&target_url)
        .connections(4)
        .duration(Duration::from_millis(500))
        .rate_per_connection(200)
        .payload_text("stream-data")
        .mode(TestMode::Stream))
    .await;

    assert_eq!(metrics.total_connections_established, 4);
    assert!(metrics.total_messages_sent > 10);
    assert_eq!(metrics.total_connections_failed, 0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_binary_payload_dispatch() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_millis(400))
        .payload_binary(bytes::Bytes::from_static(b"\x00\x01\x02\x03\x04\x05")))
    .await;

    assert_eq!(metrics.total_connections_established, 2);
    assert!(metrics.total_messages_sent > 10);
    assert_in_flight_at_most(&metrics, 2);
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

    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_millis(500))
        .payload_text("unused")
        .mode(TestMode::Listen))
    .await;

    assert_eq!(metrics.total_connections_established, 1);
    assert!(metrics.total_messages_recv >= 3);
    assert_eq!(metrics.total_messages_sent, 0);

    cancel.cancel();
}

/// Base configuration for scenario runs; each phase overrides sizing,
/// pacing and payload.
#[cfg(feature = "scenario")]
fn scenario_base_config(target_url: &Url) -> LoadTestConfigBuilder {
    echo_test_config(target_url)
        .duration(Duration::from_millis(300))
        .payload_text("default")
}

/// Parses a scenario whose phases all run against `target_url`. `phases` is
/// the YAML list under `phases:`, indented by two spaces.
#[cfg(feature = "scenario")]
fn scenario(target_url: &Url, phases: &str) -> ScenarioConfig {
    let yaml = format!(
        "version: \"1.0\"\ntarget: \"{target_url}\"\nmetadata:\n  name: \"test\"\nphases:\n{phases}"
    );
    ScenarioConfig::from_yaml(&yaml).unwrap()
}

#[cfg(feature = "scenario")]
async fn run_scenario(scenario: &ScenarioConfig, base: LoadTestConfigBuilder) -> AggregatedMetrics {
    ScenarioExecutor::new(scenario, base.build().unwrap(), None, None)
        .unwrap()
        .run()
        .await
        .unwrap()
        .metrics
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_action_script_echoes_and_closes() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let scenario = scenario(
        &target_url,
        r#"  - name: "scripted"
    duration: "300ms"
    connections: 3
    rate_per_conn: 50
    think_time: "5ms"
    actions:
      - type: Send
        count: 2
      - type: Wait
        duration: "10ms"
      - type: Ping
        interval: "20ms"
      - type: Close
        code: 1000
        reason: "finished"
"#,
    );

    let metrics = run_scenario(&scenario, scenario_base_config(&target_url)).await;

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

    let phase = |name: &str, connections: usize| {
        format!(
            r#"  - name: "{name}"
    target: "{target_url}"
    duration: "80ms"
    connections: {connections}
    actions:
      - type: Send
        count: 1
      - type: Close
        code: 1000
"#
        )
    };
    let yaml = format!(
        "version: \"1.0\"\nmetadata:\n  name: \"two-phase\"\nphases:\n{}{}",
        phase("warmup", 2),
        phase("sustain", 3)
    );
    let scenario = ScenarioConfig::from_yaml(&yaml).unwrap();
    assert_eq!(scenario.name(), "two-phase");
    assert_eq!(
        scenario.phase_names().collect::<Vec<_>>(),
        ["warmup", "sustain"]
    );

    let executor = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap();
    let outcome = executor.run().await.unwrap();
    let metrics = &outcome.metrics;

    assert_eq!(metrics.total_connections_attempted, 5);
    assert_eq!(metrics.total_connections_established, 5);
    assert!(metrics.elapsed >= Duration::from_millis(160));
    // Every worker sends exactly one message before its Close action.
    assert_eq!(metrics.total_messages_sent, 5, "{metrics:?}");
    let phases: Vec<&str> = outcome
        .phase_targets
        .iter()
        .map(|target| target.phase.as_str())
        .collect();
    assert_eq!(phases, ["warmup", "sustain"]);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_echo_ping_interval_does_not_throttle_unpaced_load() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let config = echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_secs(1));

    let baseline = run(config.clone()).await;
    let with_ping = run(config.ping_interval(Duration::from_millis(200))).await;

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
    assert_in_flight_at_most(&with_ping, 2);
    assert_eq!(with_ping.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_echo_ping_tick_does_not_add_paced_data_sends() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(20)
        .ping_interval(Duration::from_millis(25))
        .duration(Duration::from_secs(1)))
    .await;

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

    let stream = run(echo_test_config(&target_url)
        .mode(TestMode::Stream)
        .rate_per_connection(200)
        .ping_interval(Duration::from_millis(50))
        .duration(Duration::from_millis(500)))
    .await;
    assert_eq!(stream.total_connections_established, 1);
    assert!(stream.total_messages_sent > 50);
    assert_eq!(stream.error_rate, 0.0);

    let listen = run(echo_test_config(&target_url)
        .mode(TestMode::Listen)
        .ping_interval(Duration::from_millis(50))
        .duration(Duration::from_millis(300)))
    .await;
    assert_eq!(listen.total_connections_established, 1);
    assert_eq!(listen.total_messages_sent, 0);
    assert_eq!(listen.error_rate, 0.0);

    cancel_server.cancel();
}

/// Spawns a WebSocket server that completes handshakes and then reads frames
/// without ever replying, so echo requests stay in flight.
async fn spawn_silent_server() -> (Url, tokio_util::sync::CancellationToken) {
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
                        if let Ok(mut ws_stream) = tokio_tungstenite::accept_async(stream).await {
                            while let Some(Ok(_)) = ws_stream.next().await {}
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
async fn test_echo_cancellation_interrupts_pending_reply() {
    let (target_url, cancel_server) = spawn_silent_server().await;

    let config = echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_millis(300))
        .message_timeout(Duration::from_secs(10));

    let start = std::time::Instant::now();
    let metrics = tokio::time::timeout(Duration::from_secs(15), run(config))
        .await
        .expect("runner hung past the message timeout");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown waited for the message timeout: run took {elapsed:?}"
    );
    assert_eq!(metrics.total_connections_established, 2);
    assert_eq!(metrics.total_messages_sent, 2);
    assert_eq!(metrics.total_messages_recv, 0);
    assert!(
        metrics.error_breakdown.is_empty(),
        "cancelled in-flight requests must not count as errors: {:?}",
        metrics.error_breakdown
    );

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_echo_cancellation_interrupts_pending_reply() {
    let (target_url, cancel_server) = spawn_silent_server().await;

    let scenario = scenario(
        &target_url,
        r#"  - name: "silent"
    duration: "300ms"
    connections: 2
    actions:
      - type: Send
"#,
    );
    let base = scenario_base_config(&target_url).message_timeout(Duration::from_secs(10));

    let start = std::time::Instant::now();
    let metrics = tokio::time::timeout(Duration::from_secs(15), run_scenario(&scenario, base))
        .await
        .expect("runner hung past the message timeout");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown waited for the message timeout: run took {elapsed:?}"
    );
    assert_eq!(metrics.total_messages_sent, 2);
    assert_eq!(metrics.total_messages_recv, 0);
    assert!(
        metrics.error_breakdown.is_empty(),
        "cancelled in-flight requests must not count as errors: {:?}",
        metrics.error_breakdown
    );

    cancel_server.cancel();
}

/// Spawns an echo server that closes every connection after `replies` echoes.
async fn spawn_closing_echo_server(replies: usize) -> (Url, tokio_util::sync::CancellationToken) {
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
                        let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws_stream.split();
                        let mut echoed = 0;
                        while let Some(Ok(msg)) = reader.next().await {
                            if let Message::Text(t) = msg {
                                let _ = sink.send(Message::Text(t)).await;
                                echoed += 1;
                                if echoed == replies {
                                    let _ = sink.send(Message::Close(None)).await;
                                    break;
                                }
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
async fn test_worker_reconnects_after_server_close() {
    let (target_url, cancel_server) = spawn_closing_echo_server(3).await;

    let config = echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_secs(1))
        .build()
        .unwrap();

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

    assert!(
        metrics.total_messages_sent > 2 * 3 * 3,
        "workers stopped after the first server close: {} messages sent by 2 workers",
        metrics.total_messages_sent
    );
    assert!(
        metrics.total_reconnects > 0,
        "unexpected metrics: {metrics:?}"
    );
    assert_eq!(metrics.total_connections_attempted, 2);
    assert_eq!(metrics.total_connections_established, 2);
    assert_eq!(metrics.total_connections_failed, 0);
    assert_eq!(
        metrics.handshake_latency.count,
        metrics.total_connections_established + metrics.total_reconnects
    );

    let snapshot = runner.live_snapshot();
    assert_eq!(snapshot.active_connections, 0);
    assert_eq!(snapshot.connections_established, 2);
    assert_eq!(snapshot.reconnects, metrics.total_reconnects);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_no_reconnect_keeps_single_session_per_worker() {
    let (target_url, cancel_server) = spawn_closing_echo_server(3).await;

    let config = echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_secs(1))
        .reconnect(false)
        .build()
        .unwrap();

    let runner = Runner::new(config);
    let metrics = runner.run().await.unwrap();

    // Each worker gets 3 replies, then sends a fourth request and sees the close.
    assert!(
        metrics.total_messages_sent <= 2 * 4,
        "workers kept running with no_reconnect: {metrics:?}"
    );
    assert_eq!(metrics.total_reconnects, 0);
    assert_eq!(metrics.total_connections_established, 2);
    assert_eq!(metrics.handshake_latency.count, 2);
    assert_eq!(runner.live_snapshot().active_connections, 0);

    cancel_server.cancel();
}

/// Spawns a server that echoes the first `replies` text frames of every
/// connection and then keeps reading without answering.
async fn spawn_echo_then_silent_server(
    replies: usize,
) -> (Url, tokio_util::sync::CancellationToken) {
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
                        let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws_stream.split();
                        let mut echoed = 0;
                        while let Some(Ok(msg)) = reader.next().await {
                            if let Message::Text(t) = msg
                                && echoed < replies
                            {
                                let _ = sink.send(Message::Text(t)).await;
                                echoed += 1;
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

/// Echo configuration where every worker makes one session and gives up on
/// the first reply that does not arrive within 50ms.
fn single_session_echo_config(target_url: &Url) -> LoadTestConfigBuilder {
    echo_test_config(target_url)
        .connections(3)
        .duration(Duration::from_millis(500))
        .message_timeout(Duration::from_millis(50))
        .reconnect(false)
}

#[tokio::test]
async fn test_error_rate_is_one_when_every_request_times_out() {
    let (target_url, cancel_server) = spawn_silent_server().await;

    let metrics = run(single_session_echo_config(&target_url)).await;

    assert_eq!(metrics.total_messages_sent, 3, "{metrics:?}");
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::Timeout),
        Some(&3)
    );
    assert_eq!(metrics.error_rate, 1.0, "{metrics:?}");

    cancel_server.cancel();
}

#[tokio::test]
async fn test_error_rate_is_one_when_the_target_refuses_connections() {
    let dead_url = Url::parse("ws://127.0.0.1:59997").unwrap();

    let metrics = run(single_session_echo_config(&dead_url)).await;

    assert_eq!(metrics.total_messages_sent, 0);
    assert_eq!(metrics.error_breakdown.values().sum::<u64>(), 3);
    assert_eq!(metrics.error_rate, 1.0, "{metrics:?}");
}

#[tokio::test]
async fn test_error_rate_is_zero_on_a_clean_run() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(single_session_echo_config(&target_url)
        .duration(Duration::from_millis(200))
        .message_timeout(Duration::from_secs(2)))
    .await;

    assert!(metrics.total_messages_sent > 0);
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    assert_eq!(metrics.error_rate, 0.0);

    cancel_server.cancel();
}

#[tokio::test]
async fn test_error_rate_counts_a_timed_out_request_once() {
    let (target_url, cancel_server) = spawn_echo_then_silent_server(3).await;

    let metrics = run(single_session_echo_config(&target_url)).await;

    // Each of the 3 workers gets 3 replies, then its fourth request times out:
    // 3 failed requests out of 12 sent.
    assert_eq!(metrics.total_messages_sent, 12, "{metrics:?}");
    assert_eq!(metrics.total_messages_recv, 9, "{metrics:?}");
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::Timeout),
        Some(&3)
    );
    assert_eq!(metrics.error_rate, 0.25, "{metrics:?}");

    cancel_server.cancel();
}

#[tokio::test]
async fn test_echo_server_close_is_not_counted_as_reply() {
    let (target_url, cancel_server) = spawn_closing_echo_server(3).await;

    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_millis(500))
        .reconnect(false))
    .await;

    // Three echoes, then the fourth request is answered by a Close frame.
    assert_eq!(
        metrics.total_messages_sent, 4,
        "unexpected metrics: {metrics:?}"
    );
    assert_eq!(
        metrics.total_messages_recv, 3,
        "a Close frame was counted as a reply: {metrics:?}"
    );
    assert_eq!(metrics.message_rtt.count, 3);

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_echo_server_close_is_not_counted_as_reply() {
    let (target_url, cancel_server) = spawn_closing_echo_server(3).await;

    let scenario = scenario(
        &target_url,
        r#"  - name: "closing"
    duration: "500ms"
    connections: 1
    actions:
      - type: Send
        count: 10
"#,
    );

    let metrics = run_scenario(
        &scenario,
        scenario_base_config(&target_url).reconnect(false),
    )
    .await;

    assert_eq!(
        metrics.total_messages_sent, 4,
        "unexpected metrics: {metrics:?}"
    );
    assert_eq!(
        metrics.total_messages_recv, 3,
        "a Close frame was counted as a reply: {metrics:?}"
    );
    assert_eq!(metrics.message_rtt.count, 3);

    cancel_server.cancel();
}

#[cfg(feature = "graphql-ws")]
#[tokio::test]
async fn test_graphql_ws_cancellation_is_not_an_error() {
    let (target_url, cancel) = spawn_graphql_ws_server(false).await;

    let metrics = run(echo_test_config(&target_url)
        .mode(TestMode::GraphQLWS)
        .subprotocol("graphql-transport-ws")
        .duration(Duration::from_millis(300))
        .graphql_operation_name("Price")
        .graphql_operation("subscription Price { price }"))
    .await;

    assert_eq!(metrics.total_connections_established, 1);
    assert!(
        metrics.total_messages_recv >= 1,
        "unexpected metrics: {metrics:?}"
    );
    assert_eq!(
        metrics.error_rate, 0.0,
        "cancelling an open subscription was recorded as an error: {:?}",
        metrics.error_breakdown
    );

    cancel.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_executor_cancel_token_skips_remaining_phases() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let phase = |name: &str| {
        format!(
            r#"  - name: "{name}"
    duration: "5s"
    connections: 2
    rate_per_conn: 20
"#
        )
    };
    let scenario = scenario(&target_url, &(phase("first") + &phase("second")));

    let executor = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap()
    .with_signal_handling(false);
    let cancel = executor.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });

    let started = std::time::Instant::now();
    let outcome = executor.run().await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(outcome.metrics.total_connections_attempted, 2);
    assert_eq!(outcome.metrics.total_connections_established, 2);
    assert_eq!(
        outcome.phase_targets.len(),
        1,
        "skipped phases are not listed"
    );

    cancel_server.cancel();
}

/// Delay between the unsolicited push and the real echo, so a misattributed
/// reply shows up as an RTT well below it.
const CHATTY_ECHO_DELAY: Duration = Duration::from_millis(25);
const CHATTY_ECHO_DELAY_FLOOR_US: u64 = 24_000;
const CHATTY_RUN: Duration = Duration::from_millis(300);
const CHATTY_MIN_REPLIES: u64 = 3;

/// Spawns an echo server that pushes one unsolicited heartbeat frame before
/// each echo and holds the echo back for [`CHATTY_ECHO_DELAY`].
async fn spawn_chatty_echo_server() -> (Url, tokio_util::sync::CancellationToken) {
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
                        let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws_stream.split();
                        while let Some(Ok(msg)) = reader.next().await {
                            let echo = match msg {
                                Message::Text(_) | Message::Binary(_) => msg,
                                Message::Close(_) => break,
                                _ => continue,
                            };
                            let heartbeat = Message::Text(r#"{"event":"heartbeat"}"#.into());
                            let _ = sink.send(heartbeat).await;
                            tokio::time::sleep(CHATTY_ECHO_DELAY).await;
                            let _ = sink.send(echo).await;
                        }
                    });
                }
                _ = cancel_server.cancelled() => break,
            }
        }
    });

    (target_url, cancel)
}

fn chatty_config(target_url: &Url, reply_match: ReplyMatch) -> LoadTestConfigBuilder {
    echo_test_config(target_url)
        .duration(CHATTY_RUN)
        .payload_text(r#"{"seq":{{seq}}}"#)
        .reply_match(reply_match)
}

/// Documents the default: the first data frame after a send is the reply,
/// even when it is a server push unrelated to the request.
#[tokio::test]
async fn test_first_match_takes_unsolicited_frame_as_reply() {
    let (target_url, cancel_server) = spawn_chatty_echo_server().await;

    let metrics = run(chatty_config(&target_url, ReplyMatch::First)).await;

    assert!(
        metrics.total_messages_recv >= CHATTY_MIN_REPLIES,
        "{metrics:?}"
    );
    assert_eq!(metrics.total_unsolicited_frames, 0);
    assert!(
        metrics.message_rtt.min_us < CHATTY_ECHO_DELAY_FLOOR_US,
        "the heartbeat should have been taken as a reply: {:?}",
        metrics.message_rtt
    );

    cancel_server.cancel();
}

async fn assert_unsolicited_frames_are_skipped(reply_match: ReplyMatch) {
    let (target_url, cancel_server) = spawn_chatty_echo_server().await;

    let metrics = run(chatty_config(&target_url, reply_match.clone())).await;

    assert_eq!(metrics.error_rate, 0.0, "{reply_match:?}: {metrics:?}");
    assert!(
        metrics.total_messages_recv >= CHATTY_MIN_REPLIES,
        "{reply_match:?}: {metrics:?}"
    );
    assert_eq!(metrics.message_rtt.count, metrics.total_messages_recv);
    // Each answered request was preceded by one heartbeat; the request still in
    // flight when the run ended may have seen its heartbeat too.
    let unsolicited = metrics.total_unsolicited_frames;
    assert!(
        (metrics.total_messages_recv..=metrics.total_messages_recv + 1).contains(&unsolicited),
        "{reply_match:?}: every heartbeat must be counted as unsolicited: {metrics:?}"
    );
    assert!(
        metrics.message_rtt.min_us >= CHATTY_ECHO_DELAY_FLOOR_US,
        "{reply_match:?}: RTT must come from the delayed echo: {:?}",
        metrics.message_rtt
    );

    cancel_server.cancel();
}

#[tokio::test]
async fn test_exact_match_skips_unsolicited_frames() {
    assert_unsolicited_frames_are_skipped(ReplyMatch::Exact).await;
}

#[tokio::test]
async fn test_json_pointer_match_skips_unsolicited_frames() {
    assert_unsolicited_frames_are_skipped("json:/seq".parse().unwrap()).await;
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_echo_match_skips_unsolicited_frames() {
    const SENDS: u64 = 5;
    let (target_url, cancel_server) = spawn_chatty_echo_server().await;

    let scenario = scenario(
        &target_url,
        r#"  - name: "chatty"
    duration: "5s"
    connections: 1
    payload: '{"seq":{{seq}}}'
    actions:
      - type: Send
        count: 5
      - type: Close
        code: 1000
        reason: "done"
"#,
    );

    let base = scenario_base_config(&target_url)
        .payload_text(r#"{"seq":{{seq}}}"#)
        .reply_match("json:/seq".parse().unwrap());
    let metrics = run_scenario(&scenario, base).await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert_eq!(metrics.total_messages_recv, SENDS);
    assert_eq!(metrics.total_unsolicited_frames, SENDS);
    assert!(
        metrics.message_rtt.min_us >= CHATTY_ECHO_DELAY_FLOOR_US,
        "RTT must come from the delayed echo: {:?}",
        metrics.message_rtt
    );

    cancel_server.cancel();
}
