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
    let cases: [&[&str]; 5] = [
        &["--reject-ratio", "1.5"],
        &["--close-after", "0"],
        &["--stall-every", "1s"],
        &["--stall-every", "100ms", "--stall-for", "100ms"],
        &["--delay", "soon"],
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
