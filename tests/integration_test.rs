//! End-to-end integration test suite verifying load orchestration, metrics, SLOs, and error taxonomy.

use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;
use wsblast::{
    AggregatedMetrics, ErrorCategory, LoadTestConfig, LoadTestConfigBuilder, OutputFormat,
    ReplyMatch, Runner, SloThresholds, Stage, TestMode, evaluate_slos, render_report,
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
            + metrics.total_connections_interrupted
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

    // The progress bar refreshes every 100 ms. A progress loop that only
    // noticed the end of the run at its next refresh, such as one with a
    // fixed sleep, would end a 25 ms run at the refresh 100 ms after the
    // start or later: at least 75 ms over the duration in every run, since
    // timers never fire early. A loaded machine (other jobs, Windows' 15.6 ms
    // timer resolution) can also wake the runner late, but only now and
    // then. So every run must last at least its duration, and the smallest
    // overshoot of a few runs must stay under half the refresh period: that
    // still fails on the defect every time while one slow wake-up cannot
    // fail the test.
    const REFRESH: Duration = Duration::from_millis(100);
    const RUNS: usize = 5;
    let duration = Duration::from_millis(25);
    for progress in [false, true] {
        let mut overshoots = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
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
            assert!(
                metrics.elapsed >= duration,
                "progress={progress}: elapsed {:?} for a {duration:?} run",
                metrics.elapsed
            );
            overshoots.push(metrics.elapsed - duration);
        }

        let best = overshoots.iter().min().expect("at least one run");
        assert!(
            *best < REFRESH / 2,
            "progress={progress}: every {duration:?} run overshot by {overshoots:?}"
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
    assert_eq!(metrics.total_messages_sent, 30);
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

/// Two echo phases: `calm` is paced and gates on response time with
/// budgets it meets, `strict` is unpaced and gates on an RTT p99 of 1 ns and
/// a throughput no echo server reaches, so only `strict` breaches.
#[cfg(feature = "scenario")]
fn two_phase_slo_scenario(target_url: &Url, top_slo: &str) -> ScenarioConfig {
    ScenarioConfig::from_yaml(&format!(
        r#"version: "1.0"
target: "{target_url}"
metadata:
  name: "phase slos"
{top_slo}phases:
  - name: "calm"
    duration: "400ms"
    connections: 2
    rate_per_conn: 50
    slo:
      max_p99: "10s"
      max_error_rate: 0.5
  - name: "strict"
    duration: "300ms"
    connections: 3
    slo:
      max_p99: "1ns"
      min_throughput: 1000000000
"#
    ))
    .unwrap()
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_phase_slos_gate_each_phase_on_its_own_metrics() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let scenario = two_phase_slo_scenario(&target_url, "");
    let outcome = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap()
    .with_signal_handling(false)
    .run()
    .await
    .unwrap();

    let names: Vec<&str> = outcome.phases.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["calm", "strict"]);
    let (calm, strict) = (&outcome.phases[0], &outcome.phases[1]);
    assert_eq!(calm.connections, 2);
    assert_eq!(strict.connections, 3);
    assert_eq!(calm.target_url, target_url);

    // The paced phase gates on response time, the unpaced one on RTT.
    assert!(calm.slo.passed, "{:?}", calm.slo);
    let calm_checks: Vec<&str> = calm.slo.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(calm_checks, ["Response time p99", "Error Rate"]);
    assert!(calm.metrics.response_time.count > 0, "{:?}", calm.metrics);
    assert!(!strict.slo.passed, "{:?}", strict.slo);
    let strict_checks: Vec<&str> = strict.slo.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(strict_checks, ["Latency p99", "Throughput (msg/s)"]);
    assert_eq!(strict.metrics.response_time.count, 0);

    // The run fails, with every phase check named after its phase.
    assert!(!outcome.passed());
    assert!(!outcome.slo.passed);
    let combined: Vec<(&str, bool)> = outcome
        .slo
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.passed))
        .collect();
    assert_eq!(
        combined,
        [
            ("calm: Response time p99", true),
            ("calm: Error Rate", true),
            ("strict: Latency p99", false),
            ("strict: Throughput (msg/s)", false),
        ]
    );

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_phase_metrics_add_up_to_the_merged_totals() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let scenario = two_phase_slo_scenario(&target_url, "");
    let outcome = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap()
    .with_signal_handling(false)
    .run()
    .await
    .unwrap();

    let total = &outcome.metrics;
    let sum = |field: fn(&AggregatedMetrics) -> u64| -> u64 {
        outcome.phases.iter().map(|p| field(&p.metrics)).sum()
    };
    assert_eq!(
        sum(|m| m.total_connections_attempted),
        total.total_connections_attempted
    );
    assert_eq!(sum(|m| m.total_connections_attempted), 5);
    assert_eq!(
        sum(|m| m.total_connections_established),
        total.total_connections_established
    );
    assert_eq!(
        sum(|m| m.total_connections_failed),
        total.total_connections_failed
    );
    assert_eq!(sum(|m| m.total_messages_sent), total.total_messages_sent);
    assert_eq!(sum(|m| m.total_messages_recv), total.total_messages_recv);
    assert_eq!(sum(|m| m.total_bytes_sent), total.total_bytes_sent);
    assert_eq!(sum(|m| m.total_bytes_recv), total.total_bytes_recv);
    assert_eq!(sum(|m| m.total_reconnects), total.total_reconnects);
    assert_eq!(sum(|m| m.message_rtt.count), total.message_rtt.count);
    assert_eq!(sum(|m| m.response_time.count), total.response_time.count);
    assert_eq!(
        sum(|m| m.handshake_latency.count),
        total.handshake_latency.count
    );
    let elapsed: Duration = outcome.phases.iter().map(|p| p.metrics.elapsed).sum();
    assert_eq!(elapsed, total.elapsed);
    assert!(total.total_messages_sent > 0, "{total:?}");
    for phase in &outcome.phases {
        // Each phase's time series covers its own run, from its own start.
        let first = phase.metrics.time_series.points.first().expect("points");
        assert!(first.start_secs < 0.05, "{:?}", phase.metrics.time_series);
        let messages: u64 = phase
            .metrics
            .time_series
            .points
            .iter()
            .map(|p| p.messages_sent)
            .sum();
        assert_eq!(
            messages, phase.metrics.total_messages_sent,
            "{}",
            phase.name
        );
    }

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_slo_applies_to_the_merged_metrics_and_the_base_config_wins() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // The scenario asks for an impossible throughput and a lenient error
    // budget; the base configuration lowers the throughput threshold.
    let scenario = two_phase_slo_scenario(
        &target_url,
        "slo:\n  min_throughput: 1000000000\n  max_error_rate: 0.5\n",
    );
    let base = scenario_base_config(&target_url)
        .slo(SloThresholds::new().with_min_throughput(1.0))
        .build()
        .unwrap();
    let outcome = ScenarioExecutor::new(&scenario, base, Some("calm"), None)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();

    let combined: Vec<(&str, &str, bool)> = outcome
        .slo
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.target.as_str(), c.passed))
        .collect();
    assert_eq!(
        combined,
        [
            ("Error Rate", "<= 50.00%", true),
            ("Throughput (msg/s)", ">= 1.0", true),
            ("calm: Response time p99", "<= 10s", true),
            ("calm: Error Rate", "<= 50.00%", true),
        ]
    );
    assert!(outcome.passed(), "{:?}", outcome.slo);

    // Without the override the scenario's own threshold applies and fails.
    let outcome = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        Some("calm"),
        None,
    )
    .unwrap()
    .with_signal_handling(false)
    .run()
    .await
    .unwrap();
    assert!(!outcome.passed());
    assert!(outcome.phases[0].slo.passed);
    let failed: Vec<&str> = outcome
        .slo
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, ["Throughput (msg/s)"]);

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_scenario_staged_phase_follows_its_profile() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let scenario = scenario(
        &target_url,
        &format!(
            r#"  - name: "warmup"
    duration: "200ms"
    connections: 2
  - name: "ramp"
    rate_per_conn: {STAGED_RATE}
    payload: "stage"
    stages:
      - duration: "1s"
        target: 10
      - duration: "2s"
        target: 10
      - duration: "1s"
        target: 0
"#
        ),
    );
    let outcome = ScenarioExecutor::new(
        &scenario,
        scenario_base_config(&target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap()
    .with_signal_handling(false)
    .run()
    .await
    .unwrap();

    let ramp = &outcome.phases[1];
    assert_eq!(ramp.stages, ramp_hold_ramp_down());
    assert_eq!(ramp.connections, 10);
    assert!(outcome.phases[0].stages.is_empty());
    let metrics = &ramp.metrics;
    assert_eq!(metrics.total_connections_attempted, 10, "{metrics:?}");
    assert_eq!(metrics.total_connections_established, 10, "{metrics:?}");
    assert_eq!(metrics.total_connections_not_started, 0, "{metrics:?}");
    assert!(metrics.elapsed >= Duration::from_secs(4), "{metrics:?}");
    assert!(
        metrics.elapsed < Duration::from_millis(4_600),
        "{metrics:?}"
    );
    assert_follows_ramp_hold_ramp_down(metrics);

    // The merged run counts the constant phase's two connections too.
    assert_eq!(outcome.metrics.total_connections_attempted, 12);
    cancel_server.cancel();
}

#[tokio::test]
async fn test_echo_ping_interval_does_not_throttle_unpaced_load() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let connections = 2;
    let duration = Duration::from_secs(1);
    let ping_interval = Duration::from_millis(200);
    let config = echo_test_config(&target_url)
        .connections(connections)
        .duration(duration);

    let baseline = run(config.clone()).await;
    let with_ping = run(config.ping_interval(ping_interval)).await;

    // A loop that waited for the next ping before each send manages one send
    // per ping interval and connection, plus one at the start. The bound is
    // ten times that, not a fixed rate, which a loaded machine may not
    // reach; the comparison with the baseline below catches a smaller cut.
    let pings = (duration.as_millis() / ping_interval.as_millis()) as u64;
    let throttled = connections as u64 * (pings + 1);
    assert!(
        with_ping.total_messages_sent > 10 * throttled,
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

    let stream_config = || {
        echo_test_config(&target_url)
            .mode(TestMode::Stream)
            .rate_per_connection(200)
            .duration(Duration::from_millis(500))
    };
    // The achievable paced rate depends on the platform's timer resolution
    // (about 15.6 ms on Windows), so compare against a run without pings on
    // the same machine instead of a fixed count.
    let baseline = run(stream_config()).await;
    let stream = run(stream_config().ping_interval(Duration::from_millis(50))).await;
    assert_eq!(stream.total_connections_established, 1);
    assert!(baseline.total_messages_sent > 10, "{baseline:?}");
    assert!(
        stream.total_messages_sent * 2 >= baseline.total_messages_sent,
        "keepalive pings slowed stream mode: {} sent with ping vs {} without",
        stream.total_messages_sent,
        baseline.total_messages_sent
    );
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

    // Linux and macOS refuse a closed local port at once, but Windows retries
    // the SYN for about two seconds first. A short connect timeout makes every
    // attempt fail inside the run on all three, as refused or as timed out.
    let metrics =
        run(single_session_echo_config(&dead_url).connect_timeout(Duration::from_millis(100)))
            .await;

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

    // A reply slower than the message timeout is a timeout too, so the
    // timeout has to be far longer than an echo can take on a loaded
    // machine, and the run long enough for the fourth request's timeout to
    // fire well before it ends.
    let metrics = run(single_session_echo_config(&target_url)
        .message_timeout(Duration::from_millis(500))
        .duration(Duration::from_millis(1_500)))
    .await;

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

/// Accepts TCP connections and never answers the WebSocket upgrade, so every
/// handshake stays in progress until the client gives up.
async fn spawn_unresponsive_server() -> (Url, tokio_util::sync::CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => held.push(stream),
                _ = cancel_server.cancelled() => break,
            }
        }
    });
    (url, cancel)
}

#[tokio::test]
async fn test_handshakes_cut_off_by_the_run_end_are_interrupted_not_failed() {
    let (target_url, cancel_server) = spawn_unresponsive_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(3)
        .duration(Duration::from_millis(300))
        .connect_timeout(Duration::from_secs(5)))
    .await;

    assert_eq!(metrics.total_connections_established, 0, "{metrics:?}");
    assert_eq!(metrics.total_connections_failed, 0, "{metrics:?}");
    assert_eq!(metrics.total_connections_interrupted, 3, "{metrics:?}");
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    cancel_server.cancel();
}

#[tokio::test]
async fn test_a_handshake_that_times_out_is_failed_not_interrupted() {
    let (target_url, cancel_server) = spawn_unresponsive_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_millis(400))
        .connect_timeout(Duration::from_millis(100))
        .reconnect(false))
    .await;

    assert_eq!(metrics.total_connections_failed, 2, "{metrics:?}");
    assert_eq!(metrics.total_connections_interrupted, 0, "{metrics:?}");
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::Timeout),
        Some(&2),
        "{metrics:?}"
    );
    cancel_server.cancel();
}

#[tokio::test]
async fn test_unresolvable_host_fails_every_slot_once_with_dns_error() {
    let metrics = run(LoadTestConfig::builder("ws://does-not-exist.invalid/")
        .connections(3)
        .duration(Duration::from_millis(500))
        .connect_timeout(Duration::from_secs(2)))
    .await;

    assert_eq!(metrics.total_connections_failed, 3, "{metrics:?}");
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::DnsResolution),
        Some(&3),
        "{metrics:?}"
    );
}

/// Echoes every data frame after `delay`, one at a time per connection.
async fn spawn_delayed_echo_server(delay: Duration) -> (Url, tokio_util::sync::CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws.split();
                        while let Some(Ok(msg)) = reader.next().await {
                            if msg.is_text() || msg.is_binary() {
                                tokio::time::sleep(delay).await;
                                if sink.send(msg).await.is_err() {
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
    (url, cancel)
}

#[tokio::test]
async fn test_paced_echo_response_time_counts_waiting_behind_a_slow_reply() {
    // Each reply takes 40 ms but requests are due every 10 ms, so every
    // request after the first waits longer than the one before it. RTT only
    // sees the 40 ms; response time sees the growing wait.
    let (target_url, cancel_server) = spawn_delayed_echo_server(Duration::from_millis(40)).await;

    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(100)
        .duration(Duration::from_millis(1200)))
    .await;

    let rtt = &metrics.message_rtt;
    let response = &metrics.response_time;
    assert!(rtt.count >= 10, "{metrics:?}");
    assert_eq!(response.count, rtt.count, "{metrics:?}");
    // RTT stays near the 40 ms delay. Its median is checked, not its p99:
    // with a few dozen samples p99 is the slowest reply, which one late
    // wake-up on a loaded machine can push past any fixed bound. Measured
    // from the schedule, RTT would grow with response time instead.
    assert!(rtt.p50_us < 100_000, "{metrics:?}");
    assert!(response.p99_us > 300_000, "{metrics:?}");
    assert!(response.p50_us > rtt.p50_us * 3, "{metrics:?}");
    cancel_server.cancel();
}

/// Runs a one-phase scenario against `target_url` and returns the outcome.
#[cfg(feature = "scenario")]
async fn run_one_phase(target_url: &Url, phase: &str) -> wsblast::ScenarioOutcome {
    ScenarioExecutor::new(
        &scenario(target_url, phase),
        scenario_base_config(target_url).build().unwrap(),
        None,
        None,
    )
    .unwrap()
    .with_signal_handling(false)
    .run()
    .await
    .unwrap()
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_paced_echo_script_records_response_time_and_gates_on_it() {
    // As in the paced echo loop: replies take 40 ms, sends are due every
    // 10 ms, so response time grows while RTT stays at 40 ms.
    let (target_url, cancel_server) = spawn_delayed_echo_server(Duration::from_millis(40)).await;

    let outcome = run_one_phase(
        &target_url,
        r#"  - name: "scripted"
    duration: "1200ms"
    connections: 1
    rate_per_conn: 100
    actions:
      - type: Send
        count: 3
    slo:
      max_p99: "60s"
"#,
    )
    .await;

    let metrics = &outcome.metrics;
    let (rtt, response) = (&metrics.message_rtt, &metrics.response_time);
    assert!(rtt.count >= 10, "{metrics:?}");
    assert_eq!(response.count, rtt.count, "{metrics:?}");
    // RTT stays near the 40 ms delay. Its median is checked, not its p99:
    // with a few dozen samples p99 is the slowest reply, which one late
    // wake-up on a loaded machine can push past any fixed bound. Measured
    // from the schedule, RTT would grow with response time instead.
    assert!(rtt.p50_us < 100_000, "{metrics:?}");
    assert!(response.p99_us > 300_000, "{metrics:?}");
    assert!(response.p50_us > rtt.p50_us * 3, "{metrics:?}");
    assert_eq!(metrics.time_series.latency_basis, "response_time");
    let checks: Vec<&str> = outcome.phases[0]
        .slo
        .checks
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(checks, ["Response time p99"]);
    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn test_paced_echo_script_restarts_its_schedule_after_a_pause() {
    // Every 40 ms reply is followed by a 60 ms Wait, far longer than the
    // 10 ms period. The schedule restarts after each Wait, so no request is
    // ever counted as late and response time stays close to RTT.
    let (target_url, cancel_server) = spawn_delayed_echo_server(Duration::from_millis(40)).await;

    let outcome = run_one_phase(
        &target_url,
        r#"  - name: "paused"
    duration: "1s"
    connections: 1
    rate_per_conn: 100
    think_time: "20ms"
    actions:
      - type: Send
      - type: Wait
        duration: "60ms"
      - type: Send
      - type: Ping
        interval: "30ms"
"#,
    )
    .await;

    let metrics = &outcome.metrics;
    let (rtt, response) = (&metrics.message_rtt, &metrics.response_time);
    assert!(rtt.count >= 4, "{metrics:?}");
    assert_eq!(response.count, rtt.count, "{metrics:?}");
    // Without the restart the n-th send would be about n * 140 ms late, so
    // the median response time would be hundreds of milliseconds above the
    // median RTT. The median ignores the odd late timer wake-up of a loaded
    // machine, which the maximum does not.
    assert!(response.p50_us < rtt.p50_us + 30_000, "{metrics:?}");
    cancel_server.cancel();
}

#[tokio::test]
async fn test_paced_echo_response_time_matches_rtt_when_the_server_keeps_up() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(50)
        .duration(Duration::from_millis(800)))
    .await;

    let response = &metrics.response_time;
    assert!(response.count >= 20, "{metrics:?}");
    assert_eq!(response.count, metrics.message_rtt.count, "{metrics:?}");
    // Every request is sent on time; the timer's own wake-up delay is within
    // the slack that response time leaves out.
    assert!(response.p50_us < 5_000, "{metrics:?}");
    cancel_server.cancel();
}

#[tokio::test]
async fn test_unpaced_echo_records_no_response_time() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url).duration(Duration::from_millis(300))).await;

    assert!(metrics.message_rtt.count > 0, "{metrics:?}");
    assert_eq!(metrics.response_time.count, 0, "{metrics:?}");
    cancel_server.cancel();
}

/// Echoes every data frame at once, except that after `stall_after` replies
/// on a connection it stops replying for `stall`.
async fn spawn_stalling_echo_server(
    stall_after: usize,
    stall: Duration,
) -> (Url, tokio_util::sync::CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws.split();
                        let mut replies = 0usize;
                        while let Some(Ok(msg)) = reader.next().await {
                            if msg.is_text() || msg.is_binary() {
                                if replies == stall_after {
                                    tokio::time::sleep(stall).await;
                                }
                                replies += 1;
                                if sink.send(msg).await.is_err() {
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
    (url, cancel)
}

#[tokio::test]
async fn test_time_series_places_a_server_stall_in_its_interval() {
    // At 50 msg/s the 76th request is due at 1.5 s and the server holds its
    // reply until about 2.1 s. Throughput drops in the interval where requests
    // stop being answered, and the stall's latency lands in the next one,
    // where the late replies arrive along with the catch-up sends.
    let (target_url, cancel_server) =
        spawn_stalling_echo_server(75, Duration::from_millis(600)).await;

    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(50)
        .duration(Duration::from_millis(3_500)))
    .await;

    let series = &metrics.time_series;
    assert_eq!(series.interval_secs, 1.0);
    assert_eq!(series.latency_basis, "response_time");
    assert!((3..=4).contains(&series.points.len()), "{series:?}");

    let sent: u64 = series.points.iter().map(|p| p.messages_sent).sum();
    let recv: u64 = series.points.iter().map(|p| p.messages_recv).sum();
    assert_eq!(sent, metrics.total_messages_sent, "{series:?}");
    assert_eq!(recv, metrics.total_messages_recv, "{series:?}");

    // Locate the spike instead of assuming its interval: on a busy machine
    // the schedule can shift by a few milliseconds and move a boundary.
    let p99 = |i: usize| series.points[i].latency.p99_us;
    let msgs = |i: usize| series.points[i].messages_sent;
    let spike = (0..series.points.len())
        .max_by_key(|&i| p99(i))
        .expect("points");
    assert!(p99(spike) > 400_000, "{series:?}");
    assert!(
        spike >= 1,
        "the stall started after the first interval: {series:?}"
    );
    assert!(p99(0) < 100_000, "{series:?}");
    // Requests stopped being answered in the interval before the late
    // replies arrived, and the catch-up sends land with them.
    assert!(msgs(spike - 1) < 45, "{series:?}");
    assert!(msgs(spike) > msgs(spike - 1), "{series:?}");
    cancel_server.cancel();
}

#[tokio::test]
async fn test_unpaced_time_series_reports_rtt_per_interval() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(echo_test_config(&target_url)
        .connections(2)
        .duration(Duration::from_millis(2_200)))
    .await;

    let series = &metrics.time_series;
    assert_eq!(series.latency_basis, "rtt");
    assert!((2..=3).contains(&series.points.len()), "{series:?}");
    let samples: u64 = series.points.iter().map(|p| p.latency.count).sum();
    assert_eq!(samples, metrics.message_rtt.count, "{series:?}");
    assert!(series.points.iter().all(|p| p.active_connections <= 2));
    cancel_server.cancel();
}

#[tokio::test]
async fn test_stages_ramp_up_down_and_up_again() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // Up to 4 connections, down to 1, up to 3: the last stage opens 2 new
    // connections, so 6 slots are opened in all.
    let metrics = run(LoadTestConfig::builder(target_url.as_str())
        .payload_text("stage")
        .stages(vec![
            Stage::new(Duration::from_millis(300), 4),
            Stage::new(Duration::from_millis(300), 1),
            Stage::new(Duration::from_millis(300), 3),
            Stage::new(Duration::from_millis(300), 3),
        ]))
    .await;

    assert_eq!(metrics.total_connections_attempted, 6, "{metrics:?}");
    assert_eq!(metrics.total_connections_established, 6, "{metrics:?}");
    assert_eq!(metrics.total_connections_failed, 0, "{metrics:?}");
    assert_eq!(metrics.total_connections_not_started, 0, "{metrics:?}");
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    assert!(metrics.elapsed >= Duration::from_millis(1_200));
    assert!(
        metrics.elapsed < Duration::from_millis(1_600),
        "{metrics:?}"
    );
    cancel_server.cancel();
}

/// Messages per second per connection in the runs of
/// [`ramp_hold_ramp_down`].
const STAGED_RATE: u64 = 20;

/// Up to 10 connections over 1 s, 10 for 2 s, down to 0 over 1 s.
fn ramp_hold_ramp_down() -> Vec<Stage> {
    vec![
        Stage::new(Duration::from_secs(1), 10),
        Stage::new(Duration::from_secs(2), 10),
        Stage::new(Duration::from_secs(1), 0),
    ]
}

/// Checks that the time series of a [`ramp_hold_ramp_down`] run at
/// [`STAGED_RATE`] follows the profile.
///
/// The counters are sampled on each 1 s boundary and when the run ends. On
/// a loaded machine (CI runners, Windows' 15.6 ms timers) a sample can be
/// taken late, connections can open a little after the stage asks for
/// them, and the run can end some tens of milliseconds after its duration.
/// So nothing here depends on what happens right at a boundary: connection
/// counts are read only where the profile is flat for a second either way
/// or where the result is certain, and the ramps are recognised by their
/// send rate relative to the hold, measured over each point's own duration
/// so that a late sample does not skew it. A run that ignored the profile,
/// with every connection open from the start or until the end, still
/// fails: its ramps would send at the full rate.
fn assert_follows_ramp_hold_ramp_down(metrics: &AggregatedMetrics) {
    let series = &metrics.time_series;
    let points = &series.points;
    // A fifth point is the sliver between the 4 s sample and the end of the
    // run, kept separate when the run ended 50 ms or more after that sample.
    assert!((4..=5).contains(&points.len()), "{series:?}");
    assert!(
        points.iter().all(|p| p.active_connections <= 10),
        "more connections than the target: {series:?}"
    );
    // Taken at 1 s or later: the first half of the ramp's connections were
    // opened at least half a second before.
    assert!(points[0].active_connections >= 5, "{series:?}");
    // Taken at 2 s, a second after the ramp and before the ramp-down.
    assert_eq!(points[1].active_connections, 10, "{series:?}");
    // Taken after every worker stopped.
    assert_eq!(points.last().map(|p| p.active_connections), Some(0));

    let rate = |p: &[wsblast::TimeSeriesPoint]| {
        let sent: u64 = p.iter().map(|p| p.messages_sent).sum();
        let secs: f64 = p.iter().map(|p| p.duration_secs).sum();
        sent as f64 / secs
    };
    // 10 connections at STAGED_RATE send 200 msg/s, and 4.5 connections on
    // average during each ramp about 100 msg/s.
    let hold = rate(&points[1..3]);
    assert!(
        hold > 0.5 * 10.0 * STAGED_RATE as f64,
        "hold at {hold} msg/s: {series:?}"
    );
    let (up, down) = (rate(&points[..1]), rate(&points[3..]));
    assert!(up < 0.75 * hold, "ramp-up at {up} msg/s: {series:?}");
    assert!(down < 0.75 * hold, "ramp-down at {down} msg/s: {series:?}");
}

#[tokio::test]
async fn test_stages_follow_the_profile_over_time() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let metrics = run(LoadTestConfig::builder(target_url.as_str())
        .payload_text("stage")
        .rate_per_connection(STAGED_RATE)
        .stages(ramp_hold_ramp_down()))
    .await;

    assert_follows_ramp_hold_ramp_down(&metrics);
    assert_eq!(metrics.total_connections_attempted, 10, "{metrics:?}");
    cancel_server.cancel();
}

#[tokio::test]
async fn test_request_limit_is_exact_with_many_connections() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    for mode in [TestMode::Echo, TestMode::Stream] {
        let metrics = run(echo_test_config(&target_url)
            .mode(mode)
            .connections(40)
            .duration(Duration::from_secs(10))
            .max_requests(101))
        .await;

        // Each send is claimed before it is made, so 40 connections racing
        // for the last messages cannot overshoot the limit.
        assert_eq!(metrics.total_messages_sent, 101, "{mode:?}: {metrics:?}");
        assert!(
            metrics.elapsed < Duration::from_secs(5),
            "{mode:?}: {metrics:?}"
        );
    }
    cancel_server.cancel();
}

#[tokio::test]
async fn test_live_counters_add_up_across_shards() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    // More workers than shards, so several workers share each shard.
    let runner = Runner::new(
        echo_test_config(&target_url)
            .connections(64)
            .duration(Duration::from_millis(500))
            .build()
            .unwrap(),
    );
    let metrics = runner.run().await.unwrap();
    let live = runner.live_snapshot();

    // On a loaded machine some of the 64 handshakes may not finish within
    // 500 ms, so the live counters are compared with the merged per-worker
    // metrics rather than with the connection count.
    assert_eq!(metrics.total_connections_attempted, 64);
    assert!(metrics.total_connections_established > 0, "{metrics:?}");
    assert_eq!(
        live.connections_established,
        metrics.total_connections_established
    );
    assert_eq!(live.messages_sent, metrics.total_messages_sent);
    assert_eq!(live.messages_received, metrics.total_messages_recv);
    assert_eq!(live.bytes_sent, metrics.total_bytes_sent);
    assert_eq!(live.active_connections, 0);
    cancel_server.cancel();
}

/// Drives the `wsblast` binary with live export enabled against the
/// in-process echo server.
#[cfg(any(feature = "prometheus", feature = "otel"))]
mod live_export {
    pub(super) use super::spawn_echo_server;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::process::{Child, Command};

    fn spawn_wsblast(args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_wsblast"))
            .args(args)
            .env("NO_COLOR", "1")
            .env_remove("OTEL_SERVICE_NAME")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    pub(super) struct Finished {
        pub(super) code: Option<i32>,
        pub(super) stdout: String,
        pub(super) stderr: String,
    }

    /// A running wsblast whose stderr is read in the background.
    pub(super) struct Running {
        child: Child,
        stderr: tokio::task::JoinHandle<String>,
    }

    /// Starts wsblast and reads stderr until a line containing `marker`,
    /// which is returned, if `marker` is given.
    pub(super) async fn start(args: &[&str], marker: Option<&str>) -> (Running, String) {
        let mut child = spawn_wsblast(args);
        let mut reader = BufReader::new(child.stderr.take().unwrap());
        let mut seen = String::new();
        let mut found = String::new();
        if let Some(marker) = marker {
            loop {
                let mut line = String::new();
                let n = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                    .await
                    .expect("no output from wsblast")
                    .unwrap();
                assert!(n > 0, "wsblast closed stderr before {marker:?}:\n{seen}");
                seen.push_str(&line);
                if line.contains(marker) {
                    found = line;
                    break;
                }
            }
        }
        // Keep draining stderr so the child never blocks on a full pipe.
        let stderr = tokio::spawn(async move {
            let _ = reader.read_to_string(&mut seen).await;
            seen
        });
        (Running { child, stderr }, found)
    }

    /// Waits for the child to exit, failing the test after `limit`.
    pub(super) async fn finish(running: Running, limit: Duration) -> Finished {
        let Running { mut child, stderr } = running;
        let mut stdout_pipe = child.stdout.take().unwrap();
        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stdout_pipe.read_to_string(&mut buf).await;
            buf
        });
        let stderr_task = stderr;
        let status = tokio::time::timeout(limit, child.wait())
            .await
            .expect("wsblast did not exit in time")
            .unwrap();
        Finished {
            code: status.code(),
            stdout: stdout_task.await.unwrap(),
            stderr: stderr_task.await.unwrap(),
        }
    }
}

#[cfg(feature = "prometheus")]
mod prometheus_export {
    use super::live_export::{Finished, Running, finish, spawn_echo_server, start};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Sends one scrape and returns the status line and the body.
    async fn get(addr: &str, path: &str) -> (String, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.lines().next().unwrap().to_string(), body.to_string())
    }

    /// The value of the sample line that starts with `series`.
    fn value(body: &str, series: &str) -> f64 {
        body.lines()
            .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("{series} missing from\n{body}"))
            .parse()
            .unwrap()
    }

    /// Starts wsblast with an endpoint on a free port and returns its address.
    async fn start_serving(args: &[&str]) -> (Running, String) {
        let mut all = args.to_vec();
        all.extend(["--no-progress", "--prometheus-listen", "127.0.0.1:0"]);
        let (running, line) = start(&all, Some("Serving Prometheus metrics")).await;
        let addr = line
            .split("http://")
            .nth(1)
            .and_then(|s| s.split("/metrics").next())
            .unwrap()
            .to_string();
        (running, addr)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_are_scraped_while_the_run_is_in_progress() {
        let (target_url, cancel_server) = spawn_echo_server().await;
        let (running, addr) =
            start_serving(&[target_url.as_str(), "-c", "2", "-r", "200", "-d", "2500ms"]).await;

        let (status, other) = get(&addr, "/").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found", "{other}");

        tokio::time::sleep(Duration::from_millis(300)).await;
        let (status, first) = get(&addr, "/metrics").await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let (_, second) = get(&addr, "/metrics").await;

        let sent = |body: &str| value(body, "wsblast_messages_sent_total");
        assert!(sent(&first) > 0.0, "{first}");
        assert!(sent(&second) > sent(&first), "{first}\n---\n{second}");
        assert!(
            value(&second, "wsblast_messages_received_total")
                > value(&first, "wsblast_messages_received_total")
        );
        assert_eq!(value(&second, "wsblast_active_connections"), 2.0);
        assert_eq!(value(&second, "wsblast_connections_established_total"), 2.0);
        assert!(
            value(&second, "wsblast_run_elapsed_seconds") >= 1.4,
            "{second}"
        );
        // After the first second, the completed interval reports latency.
        assert!(
            value(&second, "wsblast_latency_samples{basis=\"response_time\"}") > 0.0,
            "{second}"
        );
        assert!(
            value(
                &second,
                "wsblast_latency_seconds{basis=\"response_time\",quantile=\"0.99\"}"
            ) > 0.0
        );

        let Finished { code, stdout, .. } = finish(running, Duration::from_secs(10)).await;
        assert_eq!(code, Some(0), "{stdout}");
        // The endpoint closes with the run.
        assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
        cancel_server.cancel();
    }

    #[cfg(feature = "scenario")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scenario_phases_share_one_endpoint() {
        let (target_url, cancel_server) = spawn_echo_server().await;
        let phase = |name: &str| {
            format!(
                "  - name: \"{name}\"\n    target: \"{target_url}\"\n    duration: \"1200ms\"\n    connections: 1\n"
            )
        };
        let yaml = format!(
            "version: \"1.0\"\nmetadata:\n  name: \"export\"\nphases:\n{}{}",
            phase("one"),
            phase("two")
        );
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("prometheus_scenario_{}.yaml", std::process::id()));
        std::fs::write(&path, yaml).unwrap();

        let (running, addr) = start_serving(&["--scenario", path.to_str().unwrap()]).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (_, during_one) = get(&addr, "/metrics").await;
        // Phase two starts after 1.2 s plus the 500 ms pause.
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        let (_, during_two) = get(&addr, "/metrics").await;

        let sent = |body: &str| value(body, "wsblast_messages_sent_total");
        assert!(
            sent(&during_two) > sent(&during_one),
            "{during_one}\n---\n{during_two}"
        );
        assert_eq!(
            value(&during_two, "wsblast_connections_established_total"),
            2.0
        );
        assert!(value(&during_two, "wsblast_run_elapsed_seconds") > 2.0);

        let finished = finish(running, Duration::from_secs(10)).await;
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let _ = std::fs::remove_file(path);
        cancel_server.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_port_in_use_is_a_configuration_error() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = taken.local_addr().unwrap().to_string();
        let (running, _) = start(
            &[
                "ws://127.0.0.1:9",
                "--no-progress",
                "-d",
                "30s",
                "--prometheus-listen",
                &addr,
            ],
            None,
        )
        .await;
        let finished = finish(running, Duration::from_secs(10)).await;
        assert_eq!(finished.code, Some(1), "{}", finished.stderr);
        assert!(
            finished
                .stderr
                .contains("--prometheus-listen cannot listen on"),
            "{}",
            finished.stderr
        );
        assert!(finished.stdout.is_empty(), "{}", finished.stdout);
    }
}

#[cfg(feature = "otel")]
mod otel_export {
    use super::live_export::{finish, spawn_echo_server, start};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One request the collector received.
    #[derive(Debug, Clone)]
    struct Export {
        request_line: String,
        content_type: String,
        body: Vec<u8>,
    }

    /// A minimal OTLP/HTTP collector that records every request and answers
    /// 200 with an empty protobuf body.
    async fn spawn_collector() -> (String, Arc<Mutex<Vec<Export>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let header = |name: &str| {
                        head.lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case(name)
                                    .then(|| value.trim().to_string())
                            })
                            .unwrap_or_default()
                    };
                    let length: usize = header("content-length").parse().unwrap_or(0);
                    while buf.len() < head_end + length {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    log.lock().unwrap().push(Export {
                        request_line: head.lines().next().unwrap_or_default().to_string(),
                        content_type: header("content-type"),
                        body: buf[head_end..head_end + length].to_vec(),
                    });
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (base, received)
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_short_run_exports_to_the_collector() {
        let (target_url, cancel_server) = spawn_echo_server().await;
        let (collector, received) = spawn_collector().await;
        let (running, _) = start(
            &[
                target_url.as_str(),
                "-c",
                "2",
                "-d",
                "1200ms",
                "--no-progress",
                "--otel-endpoint",
                &collector,
                "--otel-interval",
                "300ms",
                "--otel-service-name",
                "wsblast-test",
            ],
            None,
        )
        .await;
        let finished = finish(running, Duration::from_secs(20)).await;
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert!(!finished.stderr.contains("Warning"), "{}", finished.stderr);

        let exports = received.lock().unwrap().clone();
        assert!(exports.len() >= 2, "{exports:?}");
        for export in &exports {
            assert_eq!(export.request_line, "POST /v1/metrics HTTP/1.1");
            assert_eq!(export.content_type, "application/x-protobuf");
            assert!(!export.body.is_empty());
        }
        // Protobuf keeps strings as UTF-8, so names and attributes are visible
        // without decoding the message.
        let last = &exports.last().unwrap().body;
        for needle in [
            "wsblast.messages.sent",
            "wsblast.latency",
            "wsblast-test",
            "service.name",
            "server.address",
            "127.0.0.1",
            "wsblast.mode",
            "echo",
            "quantile",
        ] {
            assert!(
                contains(last, needle),
                "{needle} missing from the final export"
            );
        }
        cancel_server.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_collector_warns_once_and_the_run_succeeds() {
        let (target_url, cancel_server) = spawn_echo_server().await;
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);

        let (running, _) = start(
            &[
                target_url.as_str(),
                "-c",
                "1",
                "-d",
                "800ms",
                "--no-progress",
                "--otel-endpoint",
                &endpoint,
                "--otel-interval",
                "200ms",
            ],
            None,
        )
        .await;
        let finished = finish(running, Duration::from_secs(20)).await;
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert_eq!(
            finished.stderr.matches("OpenTelemetry export to").count(),
            1,
            "{}",
            finished.stderr
        );
        assert!(finished.stdout.contains("WebSocket Benchmark Summary"));
        cancel_server.cancel();
    }

    #[tokio::test]
    async fn an_invalid_endpoint_is_a_configuration_error() {
        let (running, _) = start(
            &["ws://127.0.0.1:9", "--otel-endpoint", "localhost:4318"],
            None,
        )
        .await;
        let finished = finish(running, Duration::from_secs(10)).await;
        assert_eq!(finished.code, Some(1), "{}", finished.stderr);
        assert!(
            finished.stderr.contains("--otel-endpoint"),
            "{}",
            finished.stderr
        );
    }
}

#[tokio::test]
async fn test_time_series_counts_messages_sent_while_the_run_is_cancelled() {
    // Unpaced connections are almost always mid-send when the timer fires,
    // so a final snapshot taken at the moment of cancellation would miss the
    // last messages. The series must still add up to the totals.
    let (target_url, cancel_server) = spawn_echo_server().await;
    for _ in 0..5 {
        let metrics = run(echo_test_config(&target_url)
            .connections(20)
            .duration(Duration::from_millis(300)))
        .await;
        let points = &metrics.time_series.points;
        let sent: u64 = points.iter().map(|p| p.messages_sent).sum();
        let recv: u64 = points.iter().map(|p| p.messages_recv).sum();
        let bytes: u64 = points.iter().map(|p| p.bytes_sent).sum();
        assert_eq!(sent, metrics.total_messages_sent, "{metrics:?}");
        assert_eq!(recv, metrics.total_messages_recv, "{metrics:?}");
        assert_eq!(bytes, metrics.total_bytes_sent, "{metrics:?}");
        let last = points.last().expect("points");
        let end = last.start_secs + last.duration_secs;
        assert!(
            (end - metrics.elapsed.as_secs_f64()).abs() < 0.001,
            "the series ends where the run did: {end} vs {:?}",
            metrics.elapsed
        );
    }
    cancel_server.cancel();
}

/// How each connection to [`spawn_close_recording_echo_server`] ended: the
/// code of the client's Close frame, or `None` when the TCP connection
/// ended without one.
#[cfg(feature = "chaos")]
type CloseLog = std::sync::Arc<std::sync::Mutex<Vec<Option<u16>>>>;

/// Echoes data frames and logs how every connection ended.
#[cfg(feature = "chaos")]
async fn spawn_close_recording_echo_server() -> (Url, tokio_util::sync::CancellationToken, CloseLog)
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_server = cancel.clone();
    let log = CloseLog::default();
    let server_log = std::sync::Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    let log = std::sync::Arc::clone(&server_log);
                    tokio::spawn(async move {
                        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws.split();
                        let mut code = None;
                        while let Some(Ok(msg)) = reader.next().await {
                            match msg {
                                Message::Text(_) | Message::Binary(_) => {
                                    if sink.send(msg).await.is_err() {
                                        break;
                                    }
                                }
                                Message::Close(frame) => {
                                    code = Some(frame.map_or(1005, |f| u16::from(f.code)));
                                    break;
                                }
                                _ => {}
                            }
                        }
                        log.lock().unwrap().push(code);
                    });
                }
                _ = cancel_server.cancelled() => break,
            }
        }
    });
    (url, cancel, log)
}

/// The chaos counts of a run, which must be present.
#[cfg(feature = "chaos")]
fn chaos_of(metrics: &AggregatedMetrics) -> &wsblast::ChaosSummary {
    metrics
        .chaos
        .as_ref()
        .unwrap_or_else(|| panic!("no chaos summary: {metrics:?}"))
}

/// Asserts that a run recorded no error of any kind.
#[cfg(feature = "chaos")]
fn assert_no_errors(metrics: &AggregatedMetrics) {
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_off_reports_no_chaos() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_millis(200))
        .chaos(wsblast::ChaosConfig::new().with_seed(3)))
    .await;
    assert!(metrics.chaos.is_none(), "{metrics:?}");
    assert!(metrics.total_messages_recv > 0);
    cancel_server.cancel();
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_drop_skips_sends_without_counting_them_or_errors() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    let metrics = run(echo_test_config(&target_url)
        .duration(Duration::from_secs(10))
        .max_requests(400)
        .chaos(wsblast::ChaosConfig::new().with_drop(25.0).with_seed(11)))
    .await;

    let chaos = chaos_of(&metrics);
    assert_eq!(chaos.seed, 11);
    assert!((50..150).contains(&chaos.dropped), "{metrics:?}");
    assert_eq!((chaos.delayed, chaos.corrupted, chaos.closes), (0, 0, 0));
    // Every claim against -n was either sent or dropped; the last send may
    // be cut off by the end of the run.
    let claims = metrics.total_messages_sent + chaos.dropped;
    assert!((399..=400).contains(&claims), "{metrics:?}");
    assert_in_flight_at_most(&metrics, 1);
    assert_eq!(metrics.message_rtt.count, metrics.total_messages_recv);
    assert_eq!(metrics.total_unsolicited_frames, 0);
    assert_no_errors(&metrics);
    cancel_server.cancel();
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_corrupt_sends_counted_messages_whose_replies_are_matched() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    for reply_match in [
        ReplyMatch::First,
        ReplyMatch::Exact,
        "json:/seq".parse().unwrap(),
    ] {
        let metrics = run(echo_test_config(&target_url)
            .duration(Duration::from_secs(10))
            .max_requests(300)
            .payload_text(r#"{"seq":{{seq}},"pad":"abcdefgh"}"#)
            .reply_match(reply_match.clone())
            .chaos(wsblast::ChaosConfig::new().with_corrupt(50.0).with_seed(5)))
        .await;

        let chaos = chaos_of(&metrics);
        assert!(
            (100..200).contains(&chaos.corrupted),
            "{reply_match:?}: {metrics:?}"
        );
        assert_eq!(chaos.dropped, 0);
        assert!(metrics.total_messages_sent >= 299, "{metrics:?}");
        assert_in_flight_at_most(&metrics, 1);
        assert_eq!(metrics.message_rtt.count, metrics.total_messages_recv);
        assert_eq!(
            metrics.total_unsolicited_frames, 0,
            "{reply_match:?}: {metrics:?}"
        );
        assert_no_errors(&metrics);
    }
    cancel_server.cancel();
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_delay_is_kept_out_of_rtt_and_response_time() {
    // Requests are due every 20 ms and each one is held back 30 ms: the
    // schedule moves with the delay, so no request is late because of it.
    let (target_url, cancel_server) = spawn_echo_server().await;
    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(50)
        .duration(Duration::from_millis(1200))
        .chaos(
            wsblast::ChaosConfig::new()
                .with_delay(Duration::from_millis(30), Duration::ZERO)
                .with_seed(1),
        ))
    .await;

    let chaos = chaos_of(&metrics);
    assert!(metrics.total_messages_sent >= 10, "{metrics:?}");
    // Every send was delayed; the one in progress at the end may not have
    // been sent.
    assert!(
        (metrics.total_messages_sent..=metrics.total_messages_sent + 1).contains(&chaos.delayed),
        "{metrics:?}"
    );
    // At most one send per 50 ms instead of one per 20 ms.
    assert!(metrics.total_messages_sent <= 30, "{metrics:?}");
    assert_eq!(metrics.response_time.count, metrics.message_rtt.count);
    // Counted in RTT, the delay would add 30 ms to every sample; left in the
    // schedule, it would make the n-th request about n * 30 ms late. Either
    // way the median would be at least the delay. The median, unlike p99,
    // is not moved by the few late timer wake-ups of a loaded machine.
    assert!(metrics.message_rtt.p50_us < 30_000, "{metrics:?}");
    assert!(metrics.response_time.p50_us < 30_000, "{metrics:?}");
    assert_no_errors(&metrics);
    cancel_server.cancel();
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_close_reconnects_without_errors_in_both_modes() {
    for (mode, expected) in [
        (wsblast::ChaosCloseMode::Abrupt, None),
        (wsblast::ChaosCloseMode::Graceful, Some(1001)),
    ] {
        let (target_url, cancel_server, log) = spawn_close_recording_echo_server().await;
        let metrics = run(echo_test_config(&target_url)
            .connections(2)
            .duration(Duration::from_millis(1500))
            .chaos(
                wsblast::ChaosConfig::new()
                    .with_close(20.0, mode)
                    .with_seed(9),
            ))
        .await;

        let chaos = chaos_of(&metrics);
        assert!(chaos.closes >= 2, "{mode:?}: {metrics:?}");
        // Each close is followed by a reconnect after the 100 ms backoff,
        // unless the run ended first.
        assert!(
            metrics.total_reconnects + 2 >= chaos.closes
                && metrics.total_reconnects <= chaos.closes,
            "{mode:?}: {metrics:?}"
        );
        assert_eq!(metrics.total_connections_established, 2);
        assert_in_flight_at_most(&metrics, 2);
        assert_no_errors(&metrics);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let ends = log.lock().unwrap().clone();
        let chaos_ends = ends.iter().filter(|end| **end == expected).count() as u64;
        assert!(chaos_ends >= chaos.closes, "{mode:?}: {ends:?} {metrics:?}");
        cancel_server.cancel();
    }
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_close_in_stream_mode_follows_sends() {
    let (target_url, cancel_server, _log) = spawn_close_recording_echo_server().await;
    let metrics = run(echo_test_config(&target_url)
        .mode(TestMode::Stream)
        .rate_per_connection(200)
        .duration(Duration::from_millis(800))
        .chaos(
            wsblast::ChaosConfig::new()
                .with_close(10.0, wsblast::ChaosCloseMode::Abrupt)
                .with_seed(2),
        ))
    .await;
    let chaos = chaos_of(&metrics);
    assert!(chaos.closes >= 2, "{metrics:?}");
    assert!(metrics.total_reconnects >= 1, "{metrics:?}");
    assert_no_errors(&metrics);
    cancel_server.cancel();
}

#[cfg(feature = "chaos")]
#[tokio::test]
async fn test_chaos_seed_repeats_the_same_faults() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    let faults = |seed: u64| {
        let target_url = target_url.clone();
        async move {
            let metrics = run(echo_test_config(&target_url)
                .duration(Duration::from_secs(10))
                .max_requests(300)
                .chaos(
                    wsblast::ChaosConfig::new()
                        .with_delay(Duration::ZERO, Duration::from_micros(200))
                        .with_drop(20.0)
                        .with_corrupt(20.0)
                        .with_seed(seed),
                ))
            .await;
            let chaos = chaos_of(&metrics).clone();
            (chaos.dropped, chaos.corrupted)
        }
    };
    let first = faults(7).await;
    assert_eq!(first, faults(7).await);
    assert_ne!(first, faults(8).await);
    cancel_server.cancel();
}

#[cfg(all(feature = "chaos", feature = "scenario"))]
#[tokio::test]
async fn test_scenario_phase_chaos_replaces_the_base_chaos() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    let scenario = scenario(
        &target_url,
        r#"  - name: "faulty"
    duration: "300ms"
    connections: 1
    chaos:
      drop: 50
      seed: 4
  - name: "clean"
    duration: "300ms"
    connections: 1
    chaos: {}
"#,
    );
    let base = scenario_base_config(&target_url).chaos(
        wsblast::ChaosConfig::new()
            .with_corrupt(100.0)
            .with_seed(99),
    );
    let outcome = ScenarioExecutor::new(&scenario, base.build().unwrap(), None, None)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();

    let faulty = chaos_of(&outcome.phases[0].metrics);
    assert_eq!(faulty.seed, 4);
    assert!(faulty.dropped > 0 && faulty.corrupted == 0, "{faulty:?}");
    assert!(outcome.phases[1].metrics.chaos.is_none());
    assert_eq!(chaos_of(&outcome.metrics).dropped, faulty.dropped);
    assert_no_errors(&outcome.metrics);
    cancel_server.cancel();
}

#[tokio::test]
async fn test_generator_lag_is_small_on_an_idle_generator() {
    let (target_url, cancel_server) = spawn_echo_server().await;
    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(20)
        .duration(Duration::from_millis(800)))
    .await;

    // Other processes on a shared CI machine may delay a wake-up now and
    // then; the median stays at the timer's granularity.
    let lag = metrics.generator_lag.as_ref().expect("generator lag");
    assert!(lag.samples >= 20, "{lag:?}");
    assert!(lag.p50_us < 20_000, "{lag:?}");
    assert!(
        metrics
            .time_series
            .points
            .iter()
            .any(|point| point.generator_lag.is_some()),
        "{metrics:?}"
    );
    let gate = evaluate_slos(
        &SloThresholds::new().with_max_generator_lag(Duration::from_secs(1)),
        &metrics,
    );
    assert!(gate.passed, "{gate:?}");
    assert_eq!(gate.checks[0].name, "Generator lag p99");
    #[cfg(unix)]
    assert!(
        metrics
            .open_file_limit
            .as_ref()
            .is_some_and(|l| l.needed == 65),
        "{metrics:?}"
    );
    cancel_server.cancel();
}

#[tokio::test]
async fn test_generator_lag_flags_a_blocked_runtime() {
    // `#[tokio::test]` runs on one thread. A task that blocks it, as
    // CPU-bound work in a task would, delays every worker and the probe.
    let (target_url, cancel_server) = spawn_echo_server().await;
    let stop = tokio_util::sync::CancellationToken::new();
    let blocker = tokio::spawn({
        let stop = stop.clone();
        async move {
            while !stop.is_cancelled() {
                std::thread::sleep(Duration::from_millis(50));
                tokio::task::yield_now().await;
            }
        }
    });
    let metrics = run(echo_test_config(&target_url)
        .rate_per_connection(20)
        .duration(Duration::from_millis(1200)))
    .await;
    stop.cancel();
    blocker.await.unwrap();

    let lag = metrics.generator_lag.as_ref().expect("generator lag");
    assert!(lag.saturated, "{lag:?}");
    assert!(lag.p99_us >= 30_000, "{lag:?}");
    assert!(lag.intervals_over_threshold >= 1, "{lag:?}");
    let gate = evaluate_slos(
        &SloThresholds::new().with_max_generator_lag(Duration::from_millis(25)),
        &metrics,
    );
    assert!(!gate.passed, "{gate:?}");
    let rendered = render_report(
        &echo_test_config(&target_url).build().unwrap(),
        &metrics,
        &gate,
    );
    assert!(
        rendered.contains("SATURATED: latency includes client-side delay"),
        "{rendered}"
    );
    cancel_server.cancel();
}
