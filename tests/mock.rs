//! `wsblast mock`: each behaviour shows up in a load test's report as it
//! should, and the subcommand validates its flags.
#![cfg(feature = "mock")]

use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wsblast::mock::{MockConfig, MockConfigBuilder, MockServer, MockStats, MockStatsHandle};
use wsblast::{AggregatedMetrics, ErrorCategory, LoadTestConfig, LoadTestConfigBuilder, Runner};

struct Mock {
    url: String,
    stats: MockStatsHandle,
    cancel: CancellationToken,
    task: JoinHandle<wsblast::Result<MockStats>>,
}

impl Mock {
    async fn start(builder: MockConfigBuilder) -> Self {
        let server = MockServer::bind(builder.listen("127.0.0.1:0").build().unwrap())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        Self {
            url: server.url(),
            stats: server.stats(),
            task: tokio::spawn(server.run(cancel.clone())),
            cancel,
        }
    }

    async fn stop(self) -> MockStats {
        self.cancel.cancel();
        self.task.await.unwrap().unwrap()
    }
}

fn load(url: &str) -> LoadTestConfigBuilder {
    LoadTestConfig::builder(url)
        .connections(2)
        .duration(Duration::from_millis(1500))
        .connect_timeout(Duration::from_secs(3))
        .message_timeout(Duration::from_secs(3))
}

async fn run(config: LoadTestConfigBuilder) -> AggregatedMetrics {
    Runner::new(config.build().unwrap())
        .with_signal_handling(false)
        .run()
        .await
        .unwrap()
}

#[tokio::test]
async fn plain_mock_echoes() {
    let mock = Mock::start(MockConfig::builder()).await;
    let metrics = run(load(&mock.url).duration(Duration::from_millis(500))).await;
    let stats = mock.stop().await;

    assert_eq!(metrics.total_connections_established, 2);
    assert!(metrics.total_messages_recv > 10, "{metrics:?}");
    assert_eq!(metrics.error_rate, 0.0);
    assert_eq!(stats.connections_accepted, 2);
    assert!(stats.frames_sent >= metrics.total_messages_recv);
    assert_eq!(stats.connections_active, 0);
}

#[tokio::test]
async fn delay_is_a_floor_on_round_trip_time() {
    let mock = Mock::start(MockConfig::builder().delay(Duration::from_millis(25))).await;
    let metrics = run(load(&mock.url).duration(Duration::from_secs(1))).await;
    mock.stop().await;

    assert!(metrics.message_rtt.count > 0, "{metrics:?}");
    // The histogram's lowest equivalent of 25 ms is within 0.1 % of it.
    assert!(
        metrics.message_rtt.min_us >= 24_900,
        "min RTT {} us is below the 25 ms delay",
        metrics.message_rtt.min_us
    );
}

#[tokio::test]
async fn jitter_spreads_round_trip_times_above_the_delay() {
    let mock = Mock::start(
        MockConfig::builder()
            .delay(Duration::from_millis(5))
            .jitter(Duration::from_millis(40))
            .seed(42),
    )
    .await;
    let metrics = run(load(&mock.url).connections(4)).await;
    mock.stop().await;

    let rtt = &metrics.message_rtt;
    assert!(rtt.count >= 40, "{rtt:?}");
    assert!(rtt.min_us >= 4_990, "{rtt:?}");
    assert!(
        rtt.max_us - rtt.min_us >= 15_000,
        "40 ms of jitter left a spread of only {} us",
        rtt.max_us - rtt.min_us
    );
}

#[tokio::test]
async fn close_after_makes_workers_reconnect() {
    let mock = Mock::start(MockConfig::builder().close_after(5)).await;
    let metrics = run(load(&mock.url).connections(1)).await;
    let stats = mock.stop().await;

    assert!(metrics.total_reconnects >= 2, "{metrics:?}");
    assert_eq!(metrics.total_connections_established, 1);
    assert_eq!(
        stats.connections_accepted,
        metrics.total_connections_established + metrics.total_reconnects
    );
    // Every session but the last, which the end of the run cuts short,
    // gets exactly five replies before the server closes it.
    let sessions = stats.connections_accepted;
    assert!(
        (5 * (sessions - 1)..=5 * sessions).contains(&metrics.total_messages_recv),
        "{} replies over {sessions} sessions",
        metrics.total_messages_recv
    );
}

#[tokio::test]
async fn reject_ratio_one_rejects_every_upgrade() {
    let mock = Mock::start(MockConfig::builder().reject_ratio(1.0)).await;
    let metrics = run(load(&mock.url).duration(Duration::from_millis(800))).await;
    let stats = mock.stop().await;

    assert_eq!(metrics.total_connections_established, 0);
    let rejected = metrics
        .error_breakdown
        .get(&ErrorCategory::HttpUpgradeRejected)
        .copied()
        .unwrap_or(0);
    assert!(rejected >= 2, "{:?}", metrics.error_breakdown);
    assert_eq!(rejected, metrics.error_breakdown.values().sum::<u64>());
    assert_eq!(stats.connections_accepted, 0);
    assert_eq!(stats.connections_rejected, rejected);
}

#[tokio::test]
async fn reject_ratio_rejects_about_that_fraction() {
    let mock = Mock::start(MockConfig::builder().reject_ratio(0.5).seed(1)).await;
    // One handshake per slot: rejected slots do not retry.
    let metrics = run(load(&mock.url)
        .connections(200)
        .duration(Duration::from_secs(2))
        .reconnect(false)
        .rate_per_connection(1))
    .await;
    let stats = mock.stop().await;

    assert_eq!(stats.connections_accepted + stats.connections_rejected, 200);
    assert!(
        (60..=140).contains(&stats.connections_rejected),
        "{stats:?}"
    );
    assert_eq!(
        metrics.total_connections_established,
        stats.connections_accepted
    );
}

#[tokio::test]
async fn stalls_show_up_in_paced_response_time() {
    let mock = Mock::start(
        MockConfig::builder().stall(Duration::from_millis(400), Duration::from_millis(150)),
    )
    .await;
    let metrics = run(load(&mock.url)
        .connections(1)
        .duration(Duration::from_millis(1800))
        .rate_per_connection(100))
    .await;
    mock.stop().await;

    let response = &metrics.response_time;
    let rtt = &metrics.message_rtt;
    assert!(response.count > 50, "{response:?}");
    // A request sent as a stall starts waits for all of it.
    assert!(response.max_us >= 100_000, "{response:?}");
    // Requests that fall due during a stall queue behind the held one, so
    // response time sees many slow samples where RTT sees one per stall.
    assert!(
        response.p95_us >= 30_000 && response.p95_us > 3 * rtt.p95_us,
        "response time {response:?} vs RTT {rtt:?}"
    );
}

/// Without --rate stalls barely move the percentiles, because a connection
/// sends nothing while it waits; the run records the slow replies, so the
/// reports can say so. The stalls are long next to Windows' 15.6 ms timer
/// tick, so they stay above ten times p99 there too.
#[tokio::test]
async fn unpaced_runs_flag_the_slow_replies_a_stall_causes() {
    let mock = Mock::start(
        MockConfig::builder().stall(Duration::from_millis(600), Duration::from_millis(250)),
    )
    .await;
    let unpaced = run(load(&mock.url).duration(Duration::from_millis(1800))).await;
    let paced = run(load(&mock.url)
        .duration(Duration::from_millis(1800))
        .rate_per_connection(100))
    .await;
    mock.stop().await;

    // Two stalls of 250 ms in 1.8 s: over a quarter of the waiting time.
    let slow = unpaced.slow_replies.as_ref().expect("slow replies");
    assert!(slow.count >= 2, "{slow:?}");
    assert!(slow.wait_share > 0.1 && slow.hides_a_stall(), "{slow:?}");
    assert!(
        unpaced.message_rtt.p99_us < 100_000,
        "{:?}",
        unpaced.message_rtt
    );
    // Response time already counts the requests that waited.
    assert!(paced.slow_replies.is_none(), "{:?}", paced.slow_replies);
}

#[cfg(feature = "capture")]
#[tokio::test]
async fn replay_reproduces_the_recorded_frames() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let recorded = dir.join(format!("mock_replay_recorded_{}.jsonl", std::process::id()));
    let replayed = dir.join(format!("mock_replay_replayed_{}.jsonl", std::process::id()));

    // Record an echo run against the mock itself.
    let echo = Mock::start(MockConfig::builder()).await;
    let first = run(load(&echo.url)
        .rate_per_connection(40)
        .max_requests(20)
        .payload_text(r#"{"worker":{{worker_id}},"seq":{{seq}}}"#)
        .capture(&recorded))
    .await;
    echo.stop().await;
    let recorded_frames = recv_data_frames(&recorded);
    assert_eq!(recorded_frames.len() as u64, first.total_messages_recv);

    // Replay it: two listen connections get the two recorded workers.
    let replay = Mock::start(MockConfig::builder().replay(&recorded)).await;
    let second = run(load(&replay.url)
        .mode(wsblast::TestMode::Listen)
        .reconnect(false)
        .capture(&replayed))
    .await;
    let stats = replay.stop().await;

    assert_eq!(second.total_messages_recv, first.total_messages_recv);
    assert_eq!(stats.frames_sent, first.total_messages_recv);
    let mut sent: Vec<String> = recorded_frames.iter().map(|f| f.2.clone()).collect();
    let mut got: Vec<String> = recv_data_frames(&replayed)
        .iter()
        .map(|f| f.2.clone())
        .collect();
    sent.sort();
    got.sort();
    assert_eq!(got, sent, "the replayed payloads are the recorded ones");

    // Inter-frame timing is kept: compare each worker's first-to-last span.
    let span = |frames: &[(u64, u64, String)], worker_payloads: &[String]| {
        let times: Vec<u64> = frames
            .iter()
            .filter(|f| worker_payloads.contains(&f.2))
            .map(|f| f.1)
            .collect();
        times.iter().max().unwrap() - times.iter().min().unwrap()
    };
    let replayed_frames = recv_data_frames(&replayed);
    for worker in 0..2u64 {
        let payloads: Vec<String> = recorded_frames
            .iter()
            .filter(|f| f.0 == worker)
            .map(|f| f.2.clone())
            .collect();
        let before = span(&recorded_frames, &payloads);
        let after = span(&replayed_frames, &payloads);
        assert!(
            before.abs_diff(after) < 60_000,
            "worker {worker}: recorded span {before} us, replayed {after} us"
        );
    }

    let _ = std::fs::remove_file(&recorded);
    let _ = std::fs::remove_file(&replayed);
}

/// (worker, t_us, data) of every received text frame of a capture file.
#[cfg(feature = "capture")]
fn recv_data_frames(path: &std::path::Path) -> Vec<(u64, u64, String)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|r| r["dir"] == "recv" && r["kind"] == "text")
        .map(|r| {
            (
                r["worker"].as_u64().unwrap(),
                r["t_us"].as_u64().unwrap(),
                r["data"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn cancel_closes_open_connections() {
    let mock = Mock::start(MockConfig::builder()).await;
    let runner = Runner::new(
        load(&mock.url)
            .duration(Duration::from_secs(30))
            .reconnect(false)
            .build()
            .unwrap(),
    )
    .with_signal_handling(false);
    let stop_runner = runner.cancel_token();
    let task = tokio::spawn(async move { runner.run().await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.stats.snapshot().connections_active, 2);
    let started = std::time::Instant::now();
    let stats = mock.stop().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(stats.connections_active, 0);

    // The client sees the server's close as the end of its sessions.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop_runner.cancel();
    let metrics = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(metrics.total_reconnects, 0);
    assert_eq!(metrics.total_connections_established, 2);
    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);
}

#[tokio::test]
async fn invalid_mock_flags_exit_with_one() {
    let cases: [&[&str]; 7] = [
        &["--reject-ratio", "1.5"],
        &["--close-after", "0"],
        &["--stall-every", "1s"],
        &["--stall-every", "100ms", "--stall-for", "100ms"],
        &["--delay", "soon"],
        &["--rules", "does-not-exist.yaml"],
        &["--rules", "a.yaml", "--replay", "b.jsonl"],
    ];
    for args in cases {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
            .arg("mock")
            .args(["--listen", "127.0.0.1:0"])
            .args(args)
            .env("NO_COLOR", "1")
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(args[0].trim_start_matches('-')) || stderr.contains(args[1]),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn help_lists_the_mock_subcommand() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("wsblast [OPTIONS] [URL]"), "{help}");
    assert!(help.contains("  mock  "), "{help}");
}

/// Runs the binary as a CI job would: the mock in the background, a gate
/// that must fail on its delay, then Ctrl+C, which must exit with 0.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_subcommand_serves_a_gate_and_exits_zero_on_sigint() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(["mock", "--listen", "127.0.0.1:0", "--delay", "20ms"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(banner.contains("(echo, delay 20ms)"), "{banner}");
    let url = banner
        .split_whitespace()
        .find(|word| word.starts_with("ws://"))
        .unwrap()
        .to_string();

    let gate = |budget: &'static str| {
        let url = url.clone();
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
                .args([url.as_str(), "-c", "2", "-d", "1s", "--no-progress"])
                .args(["--max-p99", budget])
                .output()
                .await
                .unwrap()
                .status
                .code()
        }
    };
    assert_eq!(gate("5ms").await, Some(2), "a 5 ms budget breaches");
    assert_eq!(gate("2s").await, Some(0), "a 2 s budget passes");

    let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill(2) only reads its integer arguments; pid is a child this
    // test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let mut rest = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        rest.push(line);
    }
    assert!(
        rest.iter().any(|line| line.starts_with("[stats] ")),
        "{rest:?}"
    );
    assert!(
        rest.last().unwrap().starts_with("wsblast mock stopped: "),
        "{rest:?}"
    );
}

/// Writes a fresh self-signed certificate for `localhost` and its key as PEM
/// files, returning their paths.
fn localhost_certificate(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let cert = dir.join(format!(
        "wsblast-mock-{}-{name}-cert.pem",
        std::process::id()
    ));
    let key = dir.join(format!(
        "wsblast-mock-{}-{name}-key.pem",
        std::process::id()
    ));
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    (cert, key)
}

/// The address `localhost` resolves to first, which the client also tries
/// first (`::1` before `127.0.0.1` on Windows runners).
async fn localhost_addr() -> std::net::SocketAddr {
    tokio::net::lookup_host("localhost:0")
        .await
        .unwrap()
        .next()
        .expect("localhost does not resolve")
}

#[tokio::test]
async fn tls_mock_reports_a_wss_url() {
    let (cert, key) = localhost_certificate("url");
    let config = MockConfig::builder()
        .listen("127.0.0.1:0")
        .tls(&cert, &key)
        .build()
        .unwrap();
    assert!(config.is_tls());
    let server = MockServer::bind(config).await.unwrap();
    assert!(
        server.url().starts_with("wss://127.0.0.1:"),
        "{}",
        server.url()
    );
}

#[test]
fn tls_mock_rejects_unusable_certificate_files() {
    let (cert, key) = localhost_certificate("bad");
    let not_pem = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("wsblast-mock-{}-not-pem.txt", std::process::id()));
    std::fs::write(&not_pem, "not a certificate").unwrap();
    let cases = [
        (
            MockConfig::builder().tls(&not_pem, &key),
            "no PEM certificate",
        ),
        (
            MockConfig::builder().tls(&cert, &not_pem),
            "no PEM private key",
        ),
        (MockConfig::builder().tls(&key, &cert), "no PEM certificate"),
        (
            MockConfig::builder().tls("/nonexistent/cert.pem", &key),
            "/nonexistent/cert.pem",
        ),
    ];
    for (builder, expected) in cases {
        let err = builder.build().expect_err(expected).to_string();
        assert!(err.contains(expected), "{expected}: {err}");
    }
}

/// `wsblast mock --tls-cert --tls-key` serves wss://, and wsblast completes
/// echo round trips against it when the certificate is trusted through
/// SSL_CERT_FILE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_subcommand_serves_wss_with_a_trusted_certificate() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (cert, key) = localhost_certificate("cli");
    let listen = std::net::SocketAddr::new(localhost_addr().await.ip(), 0).to_string();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(["mock", "--listen", listen.as_str()])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let bound = banner
        .split_whitespace()
        .find_map(|word| word.strip_prefix("wss://"))
        .unwrap_or_else(|| panic!("no wss:// URL in {banner}"));
    let port = bound.rsplit(':').next().unwrap();
    let url = format!("wss://localhost:{port}");

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
            .args([url.as_str(), "-c", "2", "-d", "1s", "--no-reconnect"])
            .args(["--no-progress", "--format", "json"])
            .env("SSL_CERT_FILE", &cert)
            .env_remove("SSL_CERT_DIR")
            .env("NO_COLOR", "1")
            .output(),
    )
    .await
    .expect("wsblast did not exit within 20s")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let established = report
        .pointer("/metrics/total_connections_established")
        .and_then(serde_json::Value::as_u64);
    assert_eq!(established, Some(2), "{report:#}");
    let received = report
        .pointer("/metrics/total_messages_recv")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    assert!(received > 0, "{report:#}");
}

#[tokio::test]
async fn mock_tls_flags_need_each_other_and_valid_files() {
    let (cert, _key) = localhost_certificate("flags");
    let cert = cert.to_str().unwrap().to_string();
    let cases: [(&[&str], &str); 2] = [
        (&["--tls-cert", &cert], "--tls-key"),
        (
            &["--tls-cert", &cert, "--tls-key", &cert],
            "no PEM private key",
        ),
    ];
    for (args, expected) in cases {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
            .args(["mock", "--listen", "127.0.0.1:0"])
            .args(args)
            .env("NO_COLOR", "1")
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}

/// Writes a rules file under the test's target directory.
fn rules_file(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("mock-rules");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}-{}.yaml", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path
}

#[tokio::test]
async fn rules_reply_by_json_field_and_text_echo_the_rest_and_close() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let rules = rules_file(
        "contract",
        r#"
version: "1.0"
rules:
  - match: { json: /type, equals: subscribe }
    reply: ['{"type":"ack","id":{{json:/id}}}', '{"type":"data","n":{{seq}}}']
  - match: { text: ping }
    reply: pong
  - after: 4
    close: { code: 4000, reason: done }
"#,
    );
    let mock = Mock::start(MockConfig::builder().rules(&rules)).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(mock.url.as_str())
        .await
        .unwrap();
    ws.send(Message::text(r#"{"type":"subscribe","id":"s1"}"#))
        .await
        .unwrap();
    let mut texts = Vec::new();
    for _ in 0..2 {
        texts.push(recv_text(&mut ws).await);
    }
    assert_eq!(
        texts,
        [r#"{"type":"ack","id":"s1"}"#, r#"{"type":"data","n":1}"#]
    );

    ws.send(Message::text("ping")).await.unwrap();
    assert_eq!(recv_text(&mut ws).await, "pong");
    ws.send(Message::text("anything")).await.unwrap();
    assert_eq!(
        recv_text(&mut ws).await,
        "anything",
        "unmatched frames are echoed"
    );

    // The fourth frame reaches the closing rule.
    ws.send(Message::text("bye")).await.unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("a close in time");
    match close {
        Some(Ok(Message::Close(Some(frame)))) => {
            assert_eq!(u16::from(frame.code), 4000);
            assert_eq!(frame.reason.as_str(), "done");
        }
        other => panic!("expected a close frame, got {other:?}"),
    }
    drop(ws);
    let stats = mock.stop().await;
    assert_eq!(stats.frames_received, 4);
    assert_eq!(stats.frames_sent, 4);
}

async fn recv_text<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    match tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("a reply in time")
    {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => text.to_string(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// The use case rules exist for: a server that pushes other frames between
/// replies, answered by id. With `--match json:/id` wsblast skips the
/// pushes and times each request to the reply that carries its id.
#[tokio::test]
async fn load_test_matches_rule_replies_by_id_and_skips_pushed_frames() {
    let rules = rules_file(
        "correlated",
        r#"
version: "1.0"
rules:
  - match: { json: /id }
    reply: ['{"type":"tick"}', '{"type":"reply","id":{{json:/id}}}']
    delay: 5ms
"#,
    );
    let mock = Mock::start(MockConfig::builder().rules(&rules)).await;
    let metrics = run(load(&mock.url)
        .payload_text(r#"{"id":{{seq}}}"#)
        .reply_match(wsblast::ReplyMatch::JsonPointer("/id".to_string())))
    .await;
    mock.stop().await;

    assert!(metrics.total_messages_sent > 10, "{metrics:?}");
    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    // Every request was answered by its own reply; each tick was skipped.
    assert!(
        metrics.total_unsolicited_frames >= metrics.message_rtt.count - 2,
        "{metrics:?}"
    );
    assert!(metrics.message_rtt.min_us >= 5_000, "{metrics:?}");
}

#[tokio::test]
async fn rules_and_replay_cannot_be_combined_and_bad_files_name_the_rule() {
    let good = rules_file("ok", "version: \"1.0\"\nrules: [{reply: x}]\n");
    let err = MockConfig::builder()
        .rules(&good)
        .replay(&good)
        .build()
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot be combined"), "{err}");
    let bad = rules_file(
        "bad",
        "version: \"1.0\"\nrules: [{reply: x}, {close: 1005}]\n",
    );
    let err = MockConfig::builder()
        .rules(&bad)
        .build()
        .unwrap_err()
        .to_string();
    assert!(err.contains("rule 2") && err.contains("1005"), "{err}");
}

/// A mock that logs a user in, returning their session, and answers a
/// subscription with the session it carried.
fn session_rules() -> std::path::PathBuf {
    rules_file(
        "session",
        r#"
version: "1.0"
rules:
  - match: { json: /op, equals: login }
    reply: ['{"status":"ok","session":{{json:/user}}}']
  - match: { json: /op, equals: sub }
    reply: ['{"status":"ok","session":{{json:/session}}}']
"#,
    )
}

#[cfg(feature = "scenario")]
async fn run_scenario(yaml: &str, url: &str) -> AggregatedMetrics {
    let scenario = wsblast::ScenarioConfig::from_yaml(yaml).unwrap();
    let base = load(url).reconnect(false).build().unwrap();
    wsblast::ScenarioExecutor::new(&scenario, base, None, Some(url))
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap()
        .metrics
}

/// Each connection logs in, keeps the session its reply carries, and sends
/// it with every subscription: the mock echoes it back, and the check that
/// it is the connection's own passes for every reply.
#[cfg(feature = "scenario")]
#[tokio::test]
async fn captured_values_are_sent_back_and_checks_pass() {
    let mock = Mock::start(MockConfig::builder().rules(session_rules())).await;
    let metrics = run_scenario(
        r#"
version: "1.0"
metadata: { name: session }
phases:
  - name: session
    connections: 3
    duration: 3s
    actions:
      - type: Send
        payload: '{"op":"login","user":"u{{worker_id}}"}'
        expect: { json: /status, equals: ok }
        capture: { session: /session }
      - type: Send
        count: 4
        payload: '{"op":"sub","session":"{{session}}"}'
        expect: { json: /session }
      - type: Close
"#,
        &mock.url,
    )
    .await;
    mock.stop().await;
    assert_eq!(metrics.total_messages_recv, 15, "{metrics:?}");
    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);
}

/// A reply that fails its check still records its RTT, and counts one
/// check_failed error against its message; a capture that finds nothing
/// does too.
#[cfg(feature = "scenario")]
#[tokio::test]
async fn failed_checks_and_captures_count_once_per_reply() {
    let mock = Mock::start(MockConfig::builder().rules(session_rules())).await;
    let metrics = run_scenario(
        r#"
version: "1.0"
metadata: { name: failing }
phases:
  - name: failing
    connections: 2
    duration: 3s
    actions:
      - type: Send
        payload: '{"op":"login","user":"u"}'
        expect: { json: /status, equals: denied }
      - type: Send
        payload: '{"op":"login","user":"u"}'
        capture: { token: /token }
      - type: Send
        payload: '{"op":"login","user":"u"}'
        expect: { contains: '"ok"' }
      - type: Close
"#,
        &mock.url,
    )
    .await;
    mock.stop().await;
    assert_eq!(metrics.message_rtt.count, 6, "{metrics:?}");
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::CheckFailed),
        Some(&4),
        "{:?}",
        metrics.error_breakdown
    );
    // Four of six messages failed; the errors add no attempts.
    assert!((metrics.error_rate - 4.0 / 6.0).abs() < 1e-9, "{metrics:?}");
}

#[tokio::test]
async fn expect_checks_every_reply_of_a_plain_run() {
    let mock = Mock::start(MockConfig::builder().rules(session_rules())).await;
    let login = r#"{"op":"login","user":"u"}"#;
    let passing = run(load(&mock.url)
        .max_requests(20)
        .payload_text(login)
        .expect("json:/status=ok"))
    .await;
    let failing = run(load(&mock.url)
        .max_requests(20)
        .payload_text(login)
        .expect("json:/status=denied"))
    .await;
    mock.stop().await;
    assert_eq!(passing.error_rate, 0.0, "{passing:?}");
    // Every reply failed, and every reply still has its RTT. (The run ends
    // with the 20th send, when each of the two connections may still wait
    // for the reply to its last request, so up to two are not counted.)
    assert!(failing.message_rtt.count >= 18, "{failing:?}");
    assert_eq!(
        failing.error_breakdown.get(&ErrorCategory::CheckFailed),
        Some(&failing.message_rtt.count),
        "{failing:?}"
    );
}

#[test]
fn reply_checks_are_validated() {
    let message = |builder: LoadTestConfigBuilder| builder.build().unwrap_err().to_string();
    let base = || LoadTestConfig::builder("ws://127.0.0.1:9001");
    assert!(message(base().expect("status=ok")).contains("invalid --expect 'status=ok'"));
    assert!(message(base().expect("json:status")).contains("is not a JSON pointer"));
    assert!(
        message(base().expect("contains:x").mode(wsblast::TestMode::Stream))
            .contains("--expect requires --mode echo")
    );
}

#[cfg(feature = "scenario")]
#[test]
fn scenario_checks_are_validated() {
    let phase = |action: &str| {
        format!(
            "version: \"1.0\"\nmetadata: {{ name: v }}\nphases:\n  - name: p\n    connections: 1\n    duration: 1s\n    actions:\n      - {action}\n"
        )
    };
    let error = |action: &str| {
        wsblast::ScenarioConfig::from_yaml(&phase(action))
            .and_then(|scenario| {
                wsblast::ScenarioExecutor::new(
                    &scenario,
                    LoadTestConfig::builder("ws://127.0.0.1:9001")
                        .build()
                        .unwrap(),
                    None,
                    None,
                )
                .map(|_| ())
            })
            .unwrap_err()
            .to_string()
    };
    let cases = [
        ("{type: Send, expect: {equals: ok}}", "equals needs json"),
        ("{type: Send, expect: {}}", "either json"),
        (
            "{type: Send, expect: {json: status}}",
            "is not a JSON pointer",
        ),
        (
            "{type: Send, expect: {json: /a, regex: x}}",
            "unknown field `regex`",
        ),
        ("{type: Send, capture: {seq: /a}}", "built-in placeholder"),
        ("{type: Send, capture: {a-b: /a}}", "cannot name a variable"),
        (
            "{type: Send, capture: {s: a}}",
            "capture s: 'a' is not a JSON pointer",
        ),
    ];
    for (action, expected) in cases {
        let error = error(action);
        assert!(error.contains(expected), "{action}: {error}");
    }
    let stream =
        wsblast::ScenarioConfig::from_yaml(&phase("{type: Send, capture: {s: /a}}")).unwrap();
    let stream_error = wsblast::ScenarioExecutor::new(
        &stream,
        LoadTestConfig::builder("ws://127.0.0.1:9001")
            .mode(wsblast::TestMode::Stream)
            .build()
            .unwrap(),
        None,
        None,
    )
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(
        stream_error.contains("expect and capture need --mode echo"),
        "{stream_error}"
    );
}

/// A mock whose login takes 20 ms, and whose subscription answers at once.
#[cfg(feature = "scenario")]
fn slow_login_rules() -> std::path::PathBuf {
    rules_file(
        "slow-login",
        r#"
version: "1.0"
rules:
  - match: { json: /op, equals: login }
    reply: ['{"status":"ok","session":{{json:/user}}}']
    delay: 20ms
  - match: { json: /op, equals: sub }
    reply: ['{"status":"ok","session":{{json:/session}}}']
"#,
    )
}

#[cfg(feature = "scenario")]
async fn run_scenario_outcome(yaml: &str, url: &str) -> wsblast::ScenarioOutcome {
    let scenario = wsblast::ScenarioConfig::from_yaml(yaml).unwrap();
    let base = load(url).reconnect(false).build().unwrap();
    wsblast::ScenarioExecutor::new(&scenario, base, None, Some(url))
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap()
}

/// Named actions are reported apart, across the phases that share a name:
/// the slow login does not hide in the fast subscriptions' percentiles, a
/// failed check counts for its own action only, and an action SLO gates
/// on its own messages.
#[cfg(feature = "scenario")]
#[tokio::test]
async fn named_actions_are_reported_and_gated_apart() {
    let mock = Mock::start(MockConfig::builder().rules(slow_login_rules())).await;
    let outcome = run_scenario_outcome(
        r#"
version: "1.0"
metadata: { name: named }
phases:
  - name: first
    connections: 2
    duration: 3s
    actions:
      - type: Send
        name: login
        payload: '{"op":"login","user":"u{{worker_id}}"}'
        slo: { max_p50: 5ms }
      - type: Send
        name: subscribe
        count: 3
        payload: '{"op":"sub","session":"s"}'
        expect: { json: /missing }
        slo: { max_p99: 15ms }
      - type: Close
  - name: second
    connections: 1
    duration: 3s
    actions:
      - type: Send
        name: subscribe
        count: 2
        payload: '{"op":"sub","session":"s"}'
      - type: Close
"#,
        &mock.url,
    )
    .await;
    mock.stop().await;
    let actions = &outcome.metrics.actions;
    let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["login", "subscribe"]);
    let (login, subscribe) = (&actions[0], &actions[1]);
    assert_eq!(login.messages_sent, 2, "{login:?}");
    assert_eq!(login.errors, 0);
    assert!(login.message_rtt.min_us >= 20_000, "{login:?}");
    // 2 x 3 in the first phase and 2 in the second, merged by name; only
    // the first phase's 6 had a failing check.
    assert_eq!(subscribe.messages_sent, 8, "{subscribe:?}");
    assert_eq!(subscribe.message_rtt.count, 8);
    assert_eq!(subscribe.errors, 6, "{subscribe:?}");
    assert!((subscribe.error_rate - 0.75).abs() < 1e-9);
    assert!(subscribe.message_rtt.p99_us < 15_000, "{subscribe:?}");

    let check = |name: &str| {
        outcome
            .slo
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {:?}", outcome.slo.checks))
    };
    assert!(!check("Action login: latency p50").passed);
    assert!(check("Action subscribe: latency p99").passed);
    assert!(!outcome.slo.passed);
}

#[cfg(feature = "scenario")]
#[test]
fn action_names_and_slos_are_validated() {
    let scenario = |actions: &str| {
        format!(
            "version: \"1.0\"\nmetadata: {{ name: v }}\nphases:\n  - name: p\n    connections: 1\n    duration: 1s\n    actions:\n{actions}"
        )
    };
    let error = |actions: &str| {
        wsblast::ScenarioConfig::from_yaml(&scenario(actions))
            .and_then(|scenario| {
                wsblast::ScenarioExecutor::new(
                    &scenario,
                    LoadTestConfig::builder("ws://127.0.0.1:9001")
                        .build()
                        .unwrap(),
                    None,
                    None,
                )
                .map(|_| ())
            })
            .unwrap_err()
            .to_string()
    };
    let long = "x".repeat(65);
    let cases = [
        (
            "      - {type: Send, slo: {max_p99: 5ms}}\n".to_string(),
            "slo on an action needs a name",
        ),
        (
            "      - {type: Send, name: a, slo: {min_throughput: 5}}\n".to_string(),
            "an action SLO takes max_p50",
        ),
        (
            "      - {type: Send, name: a, slo: {max_p99: 5ms}}\n      - {type: Send, name: a, slo: {max_p50: 1ms}}\n"
                .to_string(),
            "action 'a' already has an slo",
        ),
        (format!("      - {{type: Send, name: {long}}}\n"), "must be 1 to 64 characters"),
        (
            "      - {type: Send, name: a, slo: {max_p99: soon}}\n".to_string(),
            "action 'a': slo",
        ),
    ];
    for (actions, expected) in cases {
        let error = error(&actions);
        assert!(error.contains(expected), "{actions}: {error}");
    }
}

#[tokio::test]
async fn arrivals_open_and_close_sessions_at_the_rate() {
    let mock = Mock::start(MockConfig::builder()).await;
    let config = load(&mock.url)
        .arrivals(wsblast::Arrivals::new(40.0, Duration::from_millis(250)))
        .rate_per_connection(20)
        .duration(Duration::from_millis(1500));
    let metrics = run(config).await;
    let stats = mock.stop().await;

    let arrivals = metrics
        .arrivals
        .expect("an arrival run reports its arrivals");
    // Arrivals at k / 40 s for every k before 1.5 s.
    assert!((57..=60).contains(&arrivals.started), "{arrivals:?}");
    assert_eq!(arrivals.skipped, 0);
    assert_eq!(metrics.total_connections_attempted, arrivals.started);
    assert!(
        metrics.total_connections_established + 2 >= arrivals.started,
        "{metrics:?}"
    );
    // Every handshake is counted, not only those of the sessions that
    // ended first.
    assert_eq!(
        metrics.handshake_latency.count,
        metrics.total_connections_established + metrics.total_reconnects
    );
    // A session lasts 250 ms, so about 10 are open at once.
    assert!((8..=13).contains(&arrivals.peak_open), "{arrivals:?}");
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    assert!(metrics.response_time.count > 0);
    assert_eq!(
        stats.connections_accepted,
        metrics.total_connections_established
    );
    assert_eq!(stats.connections_active, 0);
}

#[tokio::test]
async fn arrivals_beyond_the_cap_are_skipped() {
    let mock = Mock::start(MockConfig::builder()).await;
    let config = load(&mock.url)
        .arrivals(wsblast::Arrivals::new(100.0, Duration::from_secs(2)).with_max_connections(5))
        .duration(Duration::from_secs(1));
    let metrics = run(config).await;
    mock.stop().await;

    let arrivals = metrics.arrivals.expect("arrivals");
    // No session ends before the run does, so the first five hold the cap.
    assert_eq!(arrivals.started, 5, "{arrivals:?}");
    assert_eq!(arrivals.peak_open, 5);
    assert!(
        (95..=101).contains(&(arrivals.started + arrivals.skipped)),
        "{arrivals:?}"
    );
    assert_eq!(arrivals.max_connections, Some(5));
    let text = wsblast::render_report(
        &load("ws://127.0.0.1:9").build().unwrap(),
        &metrics,
        &wsblast::evaluate_slos(&wsblast::SloThresholds::default(), &metrics),
    );
    assert!(text.contains("capped at 5 open"), "{text}");
    assert!(text.contains("raise --max-connections"), "{text}");
}

#[cfg(feature = "socketio")]
fn socketio_load(url: &str) -> LoadTestConfigBuilder {
    load(url).mode(wsblast::TestMode::SocketIo)
}

/// Every message is an event whose acknowledgement is the reply: the RTT
/// is the time from event to ack, which the mock holds for its delay.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_events_are_timed_to_their_acks() {
    let mock = Mock::start(
        MockConfig::builder()
            .socketio()
            .delay(Duration::from_millis(5)),
    )
    .await;
    let metrics = run(socketio_load(&mock.url)
        .rate_per_connection(20)
        .duration(Duration::from_secs(1))
        .socketio_namespace("/chat")
        .socketio_event("chat")
        .expect("json:/0/seq"))
    .await;
    let stats = mock.stop().await;

    assert_eq!(metrics.total_connections_established, 2);
    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);
    assert!(metrics.total_messages_recv >= 30, "{metrics:?}");
    assert!(
        metrics.message_rtt.min_us >= 5_000,
        "{:?}",
        metrics.message_rtt
    );
    // Paced, like echo mode, so response time is recorded too.
    assert_eq!(metrics.response_time.count, metrics.message_rtt.count);
    // The mock counts events, not the namespace connects or pongs.
    assert_eq!(stats.frames_received, metrics.total_messages_sent);
    assert_eq!(metrics.total_unsolicited_frames, 0);
}

/// A binary payload is a binary event, its bytes an attachment; the mock
/// acknowledges it with the same attachment, which comes back as the reply.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_binary_events_are_acked_with_their_attachments() {
    let mock = Mock::start(MockConfig::builder().socketio()).await;
    let payload: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
    let metrics = run(socketio_load(&mock.url)
        .payload_binary(payload.clone())
        .socketio_namespace("/files")
        .rate_per_connection(50)
        .duration(Duration::from_secs(1)))
    .await;
    let stats = mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);
    assert!(metrics.total_messages_recv >= 50, "{metrics:?}");
    assert_eq!(
        metrics.total_bytes_recv,
        metrics.total_messages_recv * payload.len() as u64
    );
    assert_eq!(metrics.total_unsolicited_frames, 0);
    // One event per message, however many frames it took.
    assert_eq!(stats.frames_received, metrics.total_messages_sent);
}

/// A Socket.IO 2 session (Engine.IO 3) pings the server itself: the mock
/// speaks Engine.IO 3 to a client that asks for it and drops a client that
/// stops pinging, so a run longer than the ping timeout without reconnects
/// shows the pings arrive. Binary events work as in Engine.IO 4.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_2_sessions_ping_the_server() {
    let mock = Mock::start(
        MockConfig::builder().socketio_ping(Duration::from_millis(40), Duration::from_millis(60)),
    )
    .await;
    let config = socketio_load(&mock.url)
        .socketio_version(2)
        .build()
        .unwrap();
    assert!(config.target_url().as_str().contains("EIO=3"));
    let text = run(socketio_load(&mock.url)
        .socketio_version(2)
        .socketio_namespace("/chat")
        .rate_per_connection(5)
        .duration(Duration::from_millis(1200))
        .expect("json:/0/seq"))
    .await;
    let binary = run(socketio_load(&mock.url)
        .socketio_version(2)
        .payload_binary(vec![7u8; 500])
        .rate_per_connection(20)
        .duration(Duration::from_millis(800)))
    .await;
    mock.stop().await;

    assert_eq!(text.total_reconnects, 0, "{text:?}");
    assert_eq!(text.error_rate, 0.0, "{:?}", text.error_breakdown);
    assert!(text.total_messages_recv >= 8, "{text:?}");
    assert_eq!(binary.error_rate, 0.0, "{:?}", binary.error_breakdown);
    assert!(binary.total_messages_recv >= 20, "{binary:?}");
    assert_eq!(binary.total_bytes_recv, binary.total_messages_recv * 500);
}

/// A server that answers like Socket.IO 2: the open packet, then `40` for
/// the default namespace with no session id.
#[cfg(feature = "socketio")]
async fn spawn_socketio_2_lookalike() -> String {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let open = r#"0{"sid":"x","upgrades":[],"pingInterval":25000,"pingTimeout":20000}"#;
                let _ = ws.send(Message::Text(open.into())).await;
                let _ = ws.send(Message::Text("40".into())).await;
                while let Some(Ok(_)) = ws.next().await {}
            });
        }
    });
    url
}

/// A Socket.IO 2 server is named before the run, with the flag to use,
/// instead of every session being closed at the server's ping timeout.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_names_a_socket_io_2_server_before_the_run() {
    let url = spawn_socketio_2_lookalike().await;
    let err = Runner::new(socketio_load(&url).build().unwrap())
        .with_signal_handling(false)
        .run()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Socket.IO 2 server"), "{err}");
    assert!(err.contains("Add --sio-version 2"), "{err}");
}

/// The server pings far more often than the client sends: the pump answers
/// every ping while the loop waits for its next send, so the server never
/// drops the connection.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_pings_are_answered_between_sends() {
    let mock = Mock::start(
        MockConfig::builder().socketio_ping(Duration::from_millis(40), Duration::from_millis(60)),
    )
    .await;
    let metrics = run(socketio_load(&mock.url)
        .connections(1)
        .rate_per_connection(1)
        .duration(Duration::from_millis(1200)))
    .await;
    let stats = mock.stop().await;

    assert_eq!(metrics.total_reconnects, 0, "{metrics:?}");
    assert_eq!(stats.connections_accepted, 1);
    assert!(metrics.error_breakdown.is_empty(), "{metrics:?}");
    assert_eq!(metrics.total_messages_recv, 2);
}

/// A server that does not speak Engine.IO never opens a session, so each
/// attempt is one timeout before any message.
#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_against_a_plain_server_times_out_opening() {
    let mock = Mock::start(MockConfig::builder()).await;
    let metrics = run(socketio_load(&mock.url)
        .connections(1)
        .reconnect(false)
        .message_timeout(Duration::from_millis(200))
        .duration(Duration::from_millis(600)))
    .await;
    mock.stop().await;
    assert_eq!(metrics.total_messages_sent, 0);
    assert_eq!(
        metrics.error_breakdown.get(&ErrorCategory::Timeout),
        Some(&1),
        "{metrics:?}"
    );
    assert_eq!(metrics.error_rate, 1.0);
}

/// A scenario's actions run over Socket.IO: a named login and a capture of
/// a value from its ack, sent back in the next event.
#[cfg(all(feature = "socketio", feature = "scenario"))]
#[tokio::test]
async fn socketio_runs_scenario_actions() {
    let mock = Mock::start(MockConfig::builder().socketio()).await;
    let yaml = r#"
version: "1.0"
metadata: { name: sio }
phases:
  - name: session
    connections: 2
    duration: 800ms
    actions:
      - type: Send
        name: login
        payload: '{"op":"login","user":"{{worker_id}}"}'
        capture: { user: /0/user }
        expect: { json: /0/op, equals: login }
      - type: Send
        name: say
        payload: '{"op":"say","user":"{{user}}"}'
        expect: { json: /0/user }
      - type: Wait
        duration: 100ms
"#;
    let scenario = wsblast::ScenarioConfig::from_yaml(yaml).unwrap();
    let base = socketio_load(&mock.url).reconnect(false).build().unwrap();
    let outcome = wsblast::ScenarioExecutor::new(&scenario, base, None, Some(&mock.url))
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();
    mock.stop().await;
    let metrics = outcome.metrics;
    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);
    let names: Vec<(&str, u64)> = metrics
        .actions
        .iter()
        .map(|a| (a.name.as_str(), a.messages_sent))
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.iter().all(|(_, sent)| *sent >= 4), "{names:?}");
}

#[cfg(feature = "socketio")]
#[test]
fn socketio_settings_are_validated() {
    let message = |builder: LoadTestConfigBuilder| builder.build().unwrap_err().to_string();
    let base = || LoadTestConfig::builder("ws://127.0.0.1:9001");
    assert!(
        message(base().socketio_event("chat")).contains("require --mode socketio"),
        "outside the mode"
    );
    let sio = || base().mode(wsblast::TestMode::SocketIo);
    assert!(message(sio().socketio_namespace("chat")).contains("must start with '/'"));
    assert!(message(sio().socketio_event("disconnect")).contains("reserved"));
    assert!(message(sio().socketio_auth("token")).contains("JSON object"));
    // Binary payloads are binary events.
    assert!(sio().payload_binary(b"x".to_vec()).build().is_ok());
    let config = sio().build().unwrap();
    assert_eq!(
        config.target_url().as_str(),
        "ws://127.0.0.1:9001/socket.io/?EIO=4&transport=websocket"
    );
}
