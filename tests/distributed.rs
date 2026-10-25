//! Distributed runs end to end: agents on 127.0.0.1:0 in this process, a
//! coordinator, and the mock server as the target.

#![cfg(all(feature = "distributed", feature = "mock"))]

use futures_util::{SinkExt, StreamExt};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;
use wsblast::distributed::{
    AgentConfig, AgentServer, Coordinator, DistributedOutcome, PROTOCOL_VERSION,
};
use wsblast::mock::{MockConfig, MockServer};
use wsblast::{LoadTestConfig, LoadTestConfigBuilder, Stage};

const TOKEN: &str = "test-token-0123456789";

struct Harness {
    target: String,
    agents: Vec<String>,
    stop: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn spawn_target(stop: &CancellationToken) -> String {
    let server = MockServer::bind(MockConfig::builder().listen("127.0.0.1:0").build().unwrap())
        .await
        .unwrap();
    let url = server.url();
    tokio::spawn(server.run(stop.clone()));
    url
}

async fn spawn_agent(stop: &CancellationToken, config: AgentConfig) -> String {
    let agent = AgentServer::bind(config).await.unwrap();
    let addr = agent.local_addr().to_string();
    tokio::spawn(agent.run(stop.clone()));
    addr
}

fn agent_config() -> wsblast::distributed::AgentConfigBuilder {
    AgentConfig::builder().listen("127.0.0.1:0").token(TOKEN)
}

/// A target and `count` agents, the agent at `slow` delaying its ready reply.
async fn harness(count: usize, slow: Option<(usize, Duration)>) -> Harness {
    let stop = CancellationToken::new();
    let target = spawn_target(&stop).await;
    let mut agents = Vec::new();
    for i in 0..count {
        let mut builder = agent_config();
        if let Some((index, delay)) = slow
            && index == i
        {
            builder = builder.ready_delay(delay);
        }
        agents.push(spawn_agent(&stop, builder.build().unwrap()).await);
    }
    Harness {
        target,
        agents,
        stop,
    }
}

async fn run(
    harness: &Harness,
    configure: impl FnOnce(LoadTestConfigBuilder) -> LoadTestConfigBuilder,
) -> wsblast::Result<DistributedOutcome> {
    let config = configure(LoadTestConfig::builder(&harness.target))
        .build()
        .unwrap();
    Coordinator::new(config, &harness.agents, TOKEN)?
        .with_signal_handling(false)
        .agent_timeout(Duration::from_secs(10))
        .run()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merged_totals_equal_the_sum_of_agent_totals() {
    let harness = harness(3, None).await;
    let outcome = run(&harness, |b| {
        b.connections(7)
            .duration(Duration::from_millis(1_500))
            .rate_per_connection(40)
    })
    .await
    .unwrap();

    let merged = &outcome.metrics;
    let agents: Vec<_> = outcome
        .agents
        .iter()
        .map(|agent| agent.metrics.as_ref().expect("no agent is lost"))
        .collect();
    assert_eq!(agents.len(), 3);
    let attempted: Vec<u64> = agents
        .iter()
        .map(|m| m.total_connections_attempted)
        .collect();
    assert_eq!(attempted, [3, 2, 2]);

    let sum = |f: fn(&wsblast::AggregatedMetrics) -> u64| agents.iter().map(|m| f(m)).sum::<u64>();
    assert_eq!(merged.total_connections_attempted, 7);
    assert_eq!(merged.total_connections_established, 7);
    assert_eq!(merged.total_messages_sent, sum(|m| m.total_messages_sent));
    assert_eq!(merged.total_messages_recv, sum(|m| m.total_messages_recv));
    assert_eq!(merged.total_bytes_sent, sum(|m| m.total_bytes_sent));
    assert_eq!(merged.total_bytes_recv, sum(|m| m.total_bytes_recv));
    assert_eq!(merged.message_rtt.count, sum(|m| m.message_rtt.count));
    assert_eq!(merged.response_time.count, sum(|m| m.response_time.count));
    assert_eq!(merged.handshake_latency.count, 7);
    assert!(merged.total_messages_recv > 100, "{merged:?}");

    // Merged from the histograms, the extremes are those of the agents and
    // every percentile lies between the agents' values for it.
    let rtt: Vec<_> = agents.iter().map(|m| &m.message_rtt).collect();
    assert_eq!(
        merged.message_rtt.max_us,
        rtt.iter().map(|s| s.max_us).max().unwrap()
    );
    assert_eq!(
        merged.message_rtt.min_us,
        rtt.iter().map(|s| s.min_us).min().unwrap()
    );
    let picks: [fn(&wsblast::LatencyStats) -> u64; 3] = [|s| s.p50_us, |s| s.p99_us, |s| s.p999_us];
    for pick in picks {
        let values: Vec<u64> = rtt.iter().map(|s| pick(s)).collect();
        let merged_value = pick(&merged.message_rtt);
        assert!(
            *values.iter().min().unwrap() <= merged_value
                && merged_value <= *values.iter().max().unwrap(),
            "{merged_value} outside {values:?}"
        );
    }

    let series = &merged.time_series;
    assert_eq!(series.latency_basis, "response_time");
    let series_sent: u64 = series.points.iter().map(|p| p.messages_sent).sum();
    assert_eq!(series_sent, merged.total_messages_sent);
    let series_samples: u64 = series.points.iter().map(|p| p.latency.count).sum();
    assert_eq!(series_samples, merged.response_time.count);

    let summary = merged.distributed.as_ref().expect("distributed summary");
    assert_eq!(summary.agents, harness.agents);
    assert!(summary.lost_agents.is_empty());
    // Elapsed runs from the start to the last result, so it is at least the
    // duration and not padded by any grace period.
    assert!(
        merged.elapsed >= Duration::from_millis(1_490),
        "{:?}",
        merged.elapsed
    );
    assert!(
        merged.elapsed < Duration::from_secs(4),
        "{:?}",
        merged.elapsed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_limit_is_split_exactly() {
    let harness = harness(3, None).await;
    let outcome = run(&harness, |b| {
        b.connections(3)
            .duration(Duration::from_secs(20))
            .max_requests(10)
    })
    .await
    .unwrap();
    let sent: Vec<u64> = outcome
        .agents
        .iter()
        .map(|agent| agent.metrics.as_ref().unwrap().total_messages_sent)
        .collect();
    assert_eq!(sent, [4, 3, 3]);
    assert_eq!(outcome.metrics.total_messages_sent, 10);
    assert!(outcome.metrics.elapsed < Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stages_are_shared_between_agents() {
    let harness = harness(2, None).await;
    let outcome = run(&harness, |b| {
        b.stages(vec![
            Stage::new(Duration::from_millis(300), 5),
            Stage::new(Duration::from_millis(900), 5),
        ])
    })
    .await
    .unwrap();
    let opened: Vec<u64> = outcome
        .agents
        .iter()
        .map(|agent| {
            agent
                .metrics
                .as_ref()
                .unwrap()
                .total_connections_established
        })
        .collect();
    assert_eq!(opened, [3, 2]);
    assert_eq!(outcome.metrics.total_connections_established, 5);
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_agent_starts_before_every_agent_is_ready() {
    let delay = Duration::from_millis(1_500);
    let harness = harness(3, Some((1, delay))).await;
    let before = unix_ms();
    let outcome = run(&harness, |b| {
        b.connections(3).duration(Duration::from_millis(500))
    })
    .await
    .unwrap();
    let starts: Vec<u64> = outcome
        .agents
        .iter()
        .map(|agent| agent.started_at_unix_ms.unwrap())
        .collect();
    for start in &starts {
        assert!(
            *start >= before + 1_400,
            "an agent started {} ms after the run began, before the slow agent was ready",
            start - before
        );
    }
    let spread = starts.iter().max().unwrap() - starts.iter().min().unwrap();
    assert!(spread < 250, "agents started {spread} ms apart: {starts:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_token_is_rejected_with_401_and_nothing_runs() {
    let harness = harness(2, None).await;
    let config = LoadTestConfig::builder(&harness.target)
        .connections(2)
        .duration(Duration::from_millis(300))
        .build()
        .unwrap();
    let err = Coordinator::new(config.clone(), &harness.agents, "wrong-token-0123456789")
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 401"), "{err}");
    assert!(err.contains("nothing was run"), "{err}");

    // The rejected coordinator did not leave the agents busy.
    let outcome = Coordinator::new(config, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();
    assert_eq!(outcome.metrics.total_connections_established, 2);

    // A raw handshake without a token gets 401 as well.
    let url = format!("ws://{}/", harness.agents[0]);
    match tokio_tungstenite::connect_async(url).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected HTTP 401, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_agent_turns_a_second_coordinator_away() {
    let harness = harness(1, None).await;
    let config = LoadTestConfig::builder(&harness.target)
        .connections(1)
        .duration(Duration::from_millis(1_500))
        .build()
        .unwrap();
    let first = tokio::spawn({
        let config = config.clone();
        let agents = harness.agents.clone();
        async move {
            Coordinator::new(config, &agents, TOKEN)
                .unwrap()
                .with_signal_handling(false)
                .run()
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let err = Coordinator::new(config, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("busy"), "{err}");
    assert!(first.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_coordinator_stops_the_agents_and_keeps_their_results() {
    let harness = harness(2, None).await;
    let config = LoadTestConfig::builder(&harness.target)
        .connections(4)
        .duration(Duration::from_secs(60))
        .rate_per_connection(20)
        .build()
        .unwrap();
    let coordinator = Coordinator::new(config, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false);
    let cancel = coordinator.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        cancel.cancel();
    });
    let outcome = coordinator.run().await.unwrap();
    assert!(outcome.metrics.elapsed < Duration::from_secs(10));
    assert!(outcome.metrics.total_messages_recv > 0);
    assert_eq!(outcome.lost_agents().count(), 0);
}

/// What a scripted agent does after `start`.
#[derive(Clone, Copy)]
enum Script {
    /// Answers hello with another protocol version.
    WrongProtocol,
    /// Completes the setup, then disconnects shortly after `start`.
    DisconnectAfterStart,
}

/// A hand-written agent speaking the JSON protocol, to test what a real
/// agent never does.
async fn spawn_scripted_agent(stop: &CancellationToken, script: Script) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = stop.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                () = stop.cancelled() => return,
            };
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let message: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                    let reply = match (message["type"].as_str().unwrap(), script) {
                        ("hello", Script::WrongProtocol) => {
                            r#"{"type":"hello","protocol":999,"version":"9.9.9"}"#.to_string()
                        }
                        ("hello", _) => format!(
                            r#"{{"type":"hello","protocol":{PROTOCOL_VERSION},"version":"test"}}"#
                        ),
                        ("prepare", _) => r#"{"type":"ready","connections":1}"#.to_string(),
                        ("start", Script::DisconnectAfterStart) => {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            return;
                        }
                        _ => continue,
                    };
                    ws.send(Message::Text(reply.into())).await.unwrap();
                }
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protocol_mismatch_fails_before_anything_runs() {
    let mut harness = harness(1, None).await;
    let scripted = spawn_scripted_agent(&harness.stop, Script::WrongProtocol).await;
    harness.agents.push(scripted.clone());
    let err = run(&harness, |b| {
        b.connections(2).duration(Duration::from_millis(300))
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains(&format!("agent {scripted}")), "{err}");
    assert!(err.contains("protocol version mismatch"), "{err}");
    assert!(err.contains("nothing was run"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_agent_is_reported_and_the_others_still_count() {
    let mut harness = harness(2, None).await;
    let scripted = spawn_scripted_agent(&harness.stop, Script::DisconnectAfterStart).await;
    harness.agents.push(scripted.clone());
    let outcome = run(&harness, |b| {
        b.connections(3)
            .duration(Duration::from_millis(1_000))
            .rate_per_connection(20)
    })
    .await
    .unwrap();
    let lost: Vec<&str> = outcome
        .lost_agents()
        .map(|agent| agent.address.as_str())
        .collect();
    assert_eq!(lost, [scripted.as_str()]);
    let error = outcome.agents[2].error.as_deref().unwrap();
    assert!(error.contains("disconnected"), "{error}");
    let summary = outcome.metrics.distributed.as_ref().unwrap();
    assert_eq!(summary.lost_agents, [scripted]);
    // Only the two real agents' single connections are counted.
    assert_eq!(outcome.metrics.total_connections_attempted, 2);
    assert_eq!(outcome.metrics.total_connections_established, 2);
}

struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn wsblast(args: &[&str], token_env: Option<&str>) -> Finished {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"));
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("WSBLAST_AGENT_TOKEN")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(token) = token_env {
        command.env("WSBLAST_AGENT_TOKEN", token);
    }
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("wsblast did not exit in time")
        .unwrap();
    Finished {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_exits_with_1_on_a_wrong_token() {
    let harness = harness(1, None).await;
    let finished = wsblast(
        &[
            &harness.target,
            "-c",
            "1",
            "-d",
            "300ms",
            "--agents",
            &harness.agents[0],
            "--agent-token",
            "not-the-right-token-at-all",
        ],
        None,
    )
    .await;
    assert_eq!(finished.code, Some(1), "{}", finished.stderr);
    assert!(finished.stderr.contains("HTTP 401"), "{}", finished.stderr);
    assert!(
        !finished.stdout.contains("Benchmark"),
        "{}",
        finished.stdout
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_reports_a_lost_agent_and_exits_with_1() {
    let mut harness = harness(1, None).await;
    let scripted = spawn_scripted_agent(&harness.stop, Script::DisconnectAfterStart).await;
    harness.agents.push(scripted.clone());
    let agents = harness.agents.join(",");
    let finished = wsblast(
        &[
            &harness.target,
            "-c",
            "2",
            "-d",
            "800ms",
            "--no-progress",
            "--format",
            "json",
            "--agents",
            &agents,
        ],
        Some(TOKEN),
    )
    .await;
    assert_eq!(finished.code, Some(1), "{}", finished.stderr);
    assert!(
        finished
            .stderr
            .contains(&format!("agent {scripted} was lost")),
        "{}",
        finished.stderr
    );
    let report: serde_json::Value = serde_json::from_str(&finished.stdout).unwrap();
    assert_eq!(report["metrics"]["distributed"]["lost_agents"][0], scripted);
    assert_eq!(report["metrics"]["total_connections_established"], 1);

    // An SLO breach takes precedence: exit code 2.
    let finished = wsblast(
        &[
            &harness.target,
            "-c",
            "2",
            "-d",
            "800ms",
            "--no-progress",
            "--min-throughput",
            "1000000000",
            "--agents",
            &agents,
        ],
        Some(TOKEN),
    )
    .await;
    assert_eq!(finished.code, Some(2), "{}", finished.stderr);
    assert!(finished.stdout.contains("Agents"), "{}", finished.stdout);
    assert!(finished.stdout.contains("(lost: "), "{}", finished.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_rejects_unsupported_combinations_and_missing_tokens() {
    for extra in [&["--tui"][..]] {
        let mut args = vec!["ws://127.0.0.1:9/", "--agents", "127.0.0.1:9"];
        args.extend_from_slice(extra);
        let finished = wsblast(&args, Some(TOKEN)).await;
        assert_eq!(finished.code, Some(1), "{extra:?}: {}", finished.stderr);
        assert!(
            finished.stderr.contains("cannot be used with --agents"),
            "{extra:?}: {}",
            finished.stderr
        );
    }
    let finished = wsblast(&["ws://127.0.0.1:9/", "--agents", "127.0.0.1:9"], None).await;
    assert_eq!(finished.code, Some(1));
    assert!(
        finished.stderr.contains("WSBLAST_AGENT_TOKEN"),
        "{}",
        finished.stderr
    );

    let finished = wsblast(&["agent", "--listen", "127.0.0.1:0"], None).await;
    assert_eq!(finished.code, Some(1));
    assert!(
        finished.stderr.contains("needs a token"),
        "{}",
        finished.stderr
    );
    let finished = wsblast(
        &["agent", "--listen", "127.0.0.1:0", "--token", "short"],
        None,
    )
    .await;
    assert_eq!(finished.code, Some(1));
    assert!(
        finished.stderr.contains("at least 16"),
        "{}",
        finished.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_serve_the_control_channel_over_tls() {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir().join(format!("wsblast-agent-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();

    // Bind the address `localhost` resolves to first, which the coordinator
    // also tries first (see tests/tls.rs).
    let local = tokio::net::lookup_host("localhost:0")
        .await
        .unwrap()
        .next()
        .unwrap();
    let stop = CancellationToken::new();
    let target = spawn_target(&stop).await;
    let agent = AgentServer::bind(
        AgentConfig::builder()
            .listen(local.to_string())
            .token(TOKEN)
            .tls(&cert, &key)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let agent_url = format!("wss://localhost:{}", agent.local_addr().port());
    assert!(agent.url().starts_with("wss://"));
    tokio::spawn(agent.run(stop.clone()));

    let config = LoadTestConfig::builder(&target)
        .connections(2)
        .duration(Duration::from_millis(400))
        .build()
        .unwrap();
    let outcome = Coordinator::new(config.clone(), [&agent_url], TOKEN)
        .unwrap()
        .agent_ca(&cert)
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();
    assert_eq!(outcome.metrics.total_connections_established, 2);

    // Without the CA the certificate is not trusted.
    let err = Coordinator::new(config, [&agent_url], TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("TLS") || err.contains("certificate"), "{err}");
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `type` of the next control message.
async fn next_type<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(Ok(Message::Text(text))) = ws.next().await else {
        panic!("connection ended");
    };
    let message: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
    message["type"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_stops_its_run_when_the_coordinator_disconnects() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let harness = harness(1, None).await;
    let mut request = format!("ws://{}/", harness.agents[0])
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let send = |text: String| Message::Text(text.into());
    ws.send(send(format!(
        r#"{{"type":"hello","protocol":{PROTOCOL_VERSION},"version":"test"}}"#
    )))
    .await
    .unwrap();
    assert_eq!(next_type(&mut ws).await, "hello");
    let prepare = serde_json::json!({
        "type": "prepare",
        "start_timeout": {"secs": 5, "nanos": 0},
        "config": {
            "target": harness.target,
            "mode": "echo",
            "connections": 2,
            "ramp_rate": 0,
            "duration": {"secs": 3600, "nanos": 0},
            "stages": [],
            "stage_share": null,
            "max_requests": null,
            "rate_per_conn": 10,
            "payload": {"kind": "text", "text": "hi"},
            "headers": [],
            "subprotocol": null,
            "connect_timeout": {"secs": 5, "nanos": 0},
            "message_timeout": {"secs": 5, "nanos": 0},
            "ping_interval": {"secs": 0, "nanos": 0},
            "reply_match": "first",
            "reconnect": true,
            "graphql": null
        }
    });
    ws.send(send(prepare.to_string())).await.unwrap();
    assert_eq!(next_type(&mut ws).await, "ready");
    ws.send(send(r#"{"type":"start"}"#.into())).await.unwrap();
    assert_eq!(next_type(&mut ws).await, "snapshot");
    drop(ws);

    // The hour-long run stops, so the agent takes the next test.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match run(&harness, |b| {
            b.connections(1).duration(Duration::from_millis(200))
        })
        .await
        {
            Ok(outcome) => {
                assert_eq!(outcome.metrics.total_connections_established, 1);
                break;
            }
            Err(err) if std::time::Instant::now() < deadline => {
                assert!(err.to_string().contains("busy"), "{err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => panic!("the agent is still busy: {err}"),
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigint_on_the_coordinator_still_reports_the_agents_results() {
    let harness = harness(2, None).await;
    let agents = harness.agents.join(",");
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args([
            harness.target.as_str(),
            "-c",
            "2",
            "-d",
            "60s",
            "-r",
            "20",
            "--no-progress",
            "--format",
            "json",
            "--agents",
            &agents,
        ])
        .env("NO_COLOR", "1")
        .env("WSBLAST_AGENT_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill(2) only reads its integer arguments; pid belongs to a child
    // this test spawned and has not yet reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("the coordinator did not exit after SIGINT")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["metrics"]["total_connections_established"], 2);
    assert!(report["metrics"]["total_messages_recv"].as_u64().unwrap() > 0);
    assert!(report["duration_secs"].as_f64().unwrap() < 15.0);
}

/// A two-phase scenario against `target`: a constant phase, then a staged
/// one whose own SLO cannot pass.
fn two_phase_scenario(target: &str, warm: &str, spike_hold: &str) -> wsblast::ScenarioConfig {
    wsblast::ScenarioConfig::from_yaml(&format!(
        r#"
version: "1.0"
target: "{target}"
metadata:
  name: "distributed"
phases:
  - name: "warm"
    connections: 5
    duration: "{warm}"
    rate_per_conn: 20
  - name: "spike"
    rate_per_conn: 20
    stages:
      - duration: "300ms"
        target: 7
      - duration: "{spike_hold}"
        target: 7
    slo:
      min_throughput: 1000000000
"#
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scenario_runs_phase_by_phase_and_merges_every_phase() {
    let harness = harness(3, None).await;
    let scenario = two_phase_scenario(&harness.target, "1s", "900ms");
    let base = LoadTestConfig::builder(&harness.target).build().unwrap();
    let outcome = Coordinator::scenario(&scenario, base, None, None, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .agent_timeout(Duration::from_secs(10))
        .run_scenario()
        .await
        .unwrap();

    let scenario = &outcome.scenario;
    assert_eq!(scenario.phases.len(), 2);
    assert_eq!(outcome.phases.len(), 2);
    assert_eq!(outcome.lost_agents().count(), 0);
    // The constant phase's SLO-free gate passes, the staged phase's fails,
    // and so does the run: the CLI exits with 2.
    assert!(scenario.phases[0].slo.passed);
    assert!(!scenario.phases[1].slo.passed);
    assert!(!scenario.passed());
    assert!(
        scenario
            .slo
            .checks
            .iter()
            .any(|check| check.name.starts_with("spike: ") && !check.passed),
        "{:?}",
        scenario.slo
    );

    for (phase, agents) in scenario.phases.iter().zip(&outcome.phases) {
        let agents: Vec<&wsblast::AggregatedMetrics> = agents
            .iter()
            .map(|agent| agent.metrics.as_ref().expect("no agent is lost"))
            .collect();
        let sum =
            |f: fn(&wsblast::AggregatedMetrics) -> u64| agents.iter().map(|m| f(m)).sum::<u64>();
        let merged = &phase.metrics;
        assert_eq!(
            merged.total_connections_attempted,
            sum(|m| m.total_connections_attempted)
        );
        assert_eq!(
            merged.total_connections_established,
            sum(|m| m.total_connections_established)
        );
        assert_eq!(merged.total_messages_sent, sum(|m| m.total_messages_sent));
        assert_eq!(merged.total_messages_recv, sum(|m| m.total_messages_recv));
        assert_eq!(merged.response_time.count, sum(|m| m.response_time.count));
        assert!(merged.total_messages_recv > 0, "{}: {merged:?}", phase.name);
    }
    // Each phase is split as a single run: 5 over 3 agents, then 7 at every
    // moment of the staged phase.
    let established = |phase: usize| -> Vec<u64> {
        outcome.phases[phase]
            .iter()
            .map(|agent| {
                agent
                    .metrics
                    .as_ref()
                    .unwrap()
                    .total_connections_established
            })
            .collect()
    };
    assert_eq!(established(0), [2, 2, 1]);
    assert_eq!(established(1), [3, 2, 2]);
    assert_eq!(scenario.phases[1].stages.len(), 2);

    // The scenario's metrics add up the phases, and its series lays them
    // end to end.
    let merged = &scenario.metrics;
    let phases = &scenario.phases;
    assert_eq!(merged.total_connections_attempted, 12);
    assert_eq!(
        merged.total_messages_sent,
        phases
            .iter()
            .map(|p| p.metrics.total_messages_sent)
            .sum::<u64>()
    );
    assert_eq!(
        merged.elapsed,
        phases[0].metrics.elapsed + phases[1].metrics.elapsed
    );
    let series_sent: u64 = merged
        .time_series
        .points
        .iter()
        .map(|p| p.messages_sent)
        .sum();
    assert_eq!(series_sent, merged.total_messages_sent);
    let second_start = merged.time_series.points.len() - phases[1].metrics.time_series.points.len();
    assert!(
        merged.time_series.points[second_start].start_secs
            >= phases[0].metrics.elapsed.as_secs_f64(),
        "{:?}",
        merged.time_series.points
    );
    let summary = merged.distributed.as_ref().unwrap();
    assert_eq!(summary.agents, harness.agents);
    assert!(summary.lost_agents.is_empty());
    assert_eq!(scenario.phase_targets.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_scenario_stops_the_phase_and_skips_the_rest() {
    let harness = harness(2, None).await;
    let scenario = two_phase_scenario(&harness.target, "60s", "60s");
    let base = LoadTestConfig::builder(&harness.target).build().unwrap();
    let coordinator = Coordinator::scenario(&scenario, base, None, None, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .agent_timeout(Duration::from_secs(10));
    let cancel = coordinator.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        cancel.cancel();
    });
    let outcome = coordinator.run_scenario().await.unwrap();
    assert_eq!(outcome.scenario.phases.len(), 1);
    assert_eq!(outcome.scenario.phases[0].name, "warm");
    assert!(outcome.scenario.metrics.elapsed < Duration::from_secs(10));
    assert!(outcome.scenario.metrics.total_messages_recv > 0);
    assert_eq!(outcome.lost_agents().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scenario_phase_too_small_to_split_is_rejected_up_front() {
    let harness = harness(3, None).await;
    let scenario = wsblast::ScenarioConfig::from_yaml(&format!(
        r#"
version: "1.0"
target: "{}"
metadata:
  name: "small"
phases:
  - name: "big"
    connections: 6
    duration: "1s"
  - name: "tiny"
    connections: 2
    duration: "1s"
"#,
        harness.target
    ))
    .unwrap();
    let base = LoadTestConfig::builder(&harness.target).build().unwrap();
    let err = Coordinator::scenario(&scenario, base, None, None, &harness.agents, TOKEN)
        .unwrap_err()
        .to_string();
    assert!(err.contains("phase 'tiny'"), "{err}");
    assert!(err.contains("3 agents cannot share 2 connections"), "{err}");
}

fn read_capture(path: &std::path::Path) -> (serde_json::Value, Vec<serde_json::Value>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines().map(|line| serde_json::from_str(line).unwrap());
    let header = lines.next().expect("a header line");
    (header, lines.collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_stream_their_capture_into_one_file() {
    let harness = harness(3, None).await;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("distributed-capture");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("all.jsonl");
    let outcome = run(&harness, |b| {
        b.connections(6)
            .duration(Duration::from_millis(1_200))
            .rate_per_connection(20)
            .capture(&path)
            .capture_limit(0)
    })
    .await
    .unwrap();

    let (header, records) = read_capture(&path);
    assert_eq!(header["wsblast_capture"], 1);
    assert_eq!(header["target"], format!("{}/", harness.target));
    assert_eq!(header["agents"], serde_json::json!(harness.agents));
    let summary = outcome.metrics.capture.as_ref().expect("a capture summary");
    assert_eq!(summary.frames_written, records.len() as u64);
    assert_eq!(summary.frames_dropped, 0);
    assert_eq!(summary.limit, 0);
    for agent in 0..3u64 {
        let opens = records
            .iter()
            .filter(|r| r["agent"] == agent && r["kind"] == "open")
            .count();
        assert_eq!(opens, 2, "agent {agent} opened its 2 connections");
    }
    let count = |dir: &str| {
        records
            .iter()
            .filter(|r| r["dir"] == dir && r["kind"] == "text")
            .count() as u64
    };
    assert_eq!(count("sent"), outcome.metrics.total_messages_sent);
    assert_eq!(count("recv"), outcome.metrics.total_messages_recv);
    // Every agent's records are timed from the start of the run.
    assert!(
        records
            .iter()
            .all(|r| r["t_us"].as_u64().unwrap() < 5_000_000)
    );

    // `wsblast mock --replay` reads the merged file, and keeps the agents'
    // connections apart.
    MockConfig::builder()
        .listen("127.0.0.1:0")
        .replay(&path)
        .build()
        .unwrap();

    // The limit holds for the whole run, across agents.
    let limited = dir.join("limited.jsonl");
    let outcome = run(&harness, |b| {
        b.connections(3)
            .duration(Duration::from_millis(800))
            .rate_per_connection(50)
            .capture(&limited)
            .capture_limit(25)
    })
    .await
    .unwrap();
    let (_, records) = read_capture(&limited);
    let summary = outcome.metrics.capture.unwrap();
    assert_eq!(records.len(), 25);
    assert_eq!(summary.frames_written, 25);
    assert_eq!(summary.limit, 25);
    let produced = outcome.metrics.total_messages_sent + outcome.metrics.total_messages_recv;
    assert!(
        summary.frames_written + summary.frames_dropped >= produced,
        "{summary:?} for {produced} frames"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scenario_capture_spans_every_phase() {
    let harness = harness(2, None).await;
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("distributed-capture-scenario");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scenario.jsonl");
    let scenario = two_phase_scenario(&harness.target, "600ms", "300ms");
    let base = LoadTestConfig::builder(&harness.target)
        .capture(&path)
        .capture_limit(0)
        .build()
        .unwrap();
    let outcome = Coordinator::scenario(&scenario, base, None, None, &harness.agents, TOKEN)
        .unwrap()
        .with_signal_handling(false)
        .run_scenario()
        .await
        .unwrap();
    let (_, records) = read_capture(&path);
    let metrics = &outcome.scenario.metrics;
    let summary = metrics.capture.as_ref().unwrap();
    assert_eq!(summary.frames_written, records.len() as u64);
    let opens = records.iter().filter(|r| r["kind"] == "open").count() as u64;
    assert_eq!(opens, metrics.total_connections_established);
    // Records of the second phase come after the first phase's run time.
    let first = outcome.scenario.phases[0].metrics.elapsed.as_micros() as u64;
    assert!(records.iter().any(|r| r["t_us"].as_u64().unwrap() > first));
    let _ = std::fs::remove_dir_all(&dir);
}

/// An echo server that records the `Authorization` header of every
/// handshake and closes each connection after `replies` echoes, so workers
/// reconnect.
async fn spawn_recording_target(
    stop: &CancellationToken,
    replies: usize,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stop = stop.clone();
    let recorded = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (tcp, _) = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                () = stop.cancelled() => return,
            };
            let seen = std::sync::Arc::clone(&recorded);
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let record = |request: &Request, response: Response| {
                    let value = request
                        .headers()
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string());
                    seen.lock().unwrap().push(value);
                    Ok(response)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, record).await else {
                    return;
                };
                let (mut sink, mut reader) = ws.split();
                let mut echoed = 0;
                while let Some(Ok(message)) = reader.next().await {
                    if let Message::Text(text) = message {
                        let _ = sink.send(Message::Text(text)).await;
                        echoed += 1;
                        if echoed == replies {
                            let _ = sink.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
            });
        }
    });
    (url, seen)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refreshed_tokens_reach_every_agent() {
    let harness = harness(2, None).await;
    let (target, seen) = spawn_recording_target(&harness.stop, 15).await;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("distributed-refresh");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let counter = dir.join("count");
    let script = dir.join("token.sh");
    std::fs::write(
        &script,
        format!(
            "n=$(cat '{counter}' 2>/dev/null || echo 0)\nn=$((n + 1))\necho $n > '{counter}'\necho secret-token-$n\n",
            counter = counter.display()
        ),
    )
    .unwrap();
    let command = format!("sh '{}'", script.display());
    let agents = harness.agents.join(",");
    let finished = wsblast(
        &[
            &target,
            "-c",
            "2",
            "-d",
            "2500ms",
            "-r",
            "60",
            "--no-progress",
            "--auth-type",
            "static",
            "--auth-token-cmd",
            &command,
            "--auth-token-refresh",
            "300ms",
            "--agents",
            &agents,
        ],
        Some(TOKEN),
    )
    .await;
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    // Tokens never appear in the coordinator's output.
    assert!(
        !finished.stderr.contains("secret-token"),
        "{}",
        finished.stderr
    );
    assert!(
        !finished.stdout.contains("secret-token"),
        "{}",
        finished.stdout
    );

    let tokens: Vec<u32> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|value| {
            value
                .as_deref()
                .and_then(|v| v.strip_prefix("Bearer secret-token-"))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("unexpected header {value:?}"))
        })
        .collect();
    assert!(tokens.len() >= 6, "too few handshakes: {tokens:?}");
    // The first handshakes carry the token of the run's start, and both
    // agents' reconnects late in the run carry refreshed ones: an agent left
    // with the first token would show it there.
    assert!(tokens[..2].iter().all(|&n| n <= 2), "{tokens:?}");
    let late = &tokens[tokens.len() * 2 / 3..];
    assert!(late.iter().all(|&n| n >= 3), "{tokens:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One sample of a Prometheus scrape, without labels.
fn sample(body: &str, name: &str) -> Option<f64> {
    body.lines()
        .find(|line| line.starts_with(name) && line[name.len()..].starts_with(' '))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

async fn scrape(addr: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_coordinator_exports_cluster_totals_to_prometheus() {
    use tokio::io::AsyncBufReadExt;

    let harness = harness(2, None).await;
    let agents = harness.agents.join(",");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args([
            harness.target.as_str(),
            "-c",
            "5",
            "-d",
            "4s",
            "-r",
            "20",
            "--no-progress",
            "--format",
            "json",
            "--prometheus-listen",
            "127.0.0.1:0",
            "--agents",
            &agents,
        ])
        .env("NO_COLOR", "1")
        .env("WSBLAST_AGENT_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let addr = loop {
        let line = tokio::time::timeout(Duration::from_secs(10), stderr.next_line())
            .await
            .expect("no Prometheus address on stderr")
            .unwrap()
            .expect("stderr closed before the Prometheus address");
        if let Some(rest) = line.strip_prefix("Serving Prometheus metrics at http://") {
            break rest.trim_end_matches("/metrics").to_string();
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = stderr.next_line().await {} });

    // Every connection of both agents is open, and both agents' messages
    // and the merged interval latency are exported.
    let deadline = std::time::Instant::now() + Duration::from_millis(3_500);
    let mut body;
    loop {
        body = scrape(&addr).await;
        let active = sample(&body, "wsblast_active_connections");
        let samples = body
            .lines()
            .find(|line| line.starts_with("wsblast_latency_samples{"))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|value| value.parse::<f64>().ok());
        if active == Some(5.0) && samples.is_some_and(|n| n > 0.0) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        sample(&body, "wsblast_connections_established_total"),
        Some(5.0),
        "{body}"
    );
    let sent = sample(&body, "wsblast_messages_sent_total").unwrap();
    assert!(sent > 0.0, "{body}");
    assert!(body.contains("basis=\"response_time\""), "{body}");

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("the coordinator did not exit")
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["metrics"]["total_messages_sent"].as_f64().unwrap() >= sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_of_another_protocol_version_gets_a_clear_error() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let harness = harness(1, None).await;
    let mut request = format!("ws://{}/", harness.agents[0])
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    // A coordinator of the first protocol version.
    ws.send(Message::Text(
        r#"{"type":"hello","protocol":1,"version":"0.2.0"}"#.into(),
    ))
    .await
    .unwrap();
    let Some(Ok(Message::Text(text))) = ws.next().await else {
        panic!("no reply");
    };
    let reply: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(reply["type"], "error");
    let message = reply["message"].as_str().unwrap();
    assert!(message.contains("protocol version mismatch"), "{message}");
    assert!(message.contains("speaking protocol 1"), "{message}");
}
