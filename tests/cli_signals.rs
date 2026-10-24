//! Process-level tests that drive the compiled `wsblast` binary: signal
//! handling and exit codes.
#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

const SUMMARY_HEADER: &str = "WebSocket Benchmark Summary";

/// Spawns an in-process WebSocket echo server on an ephemeral OS port.
async fn spawn_echo_server() -> (String, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let cancel = CancellationToken::new();
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
                _ = cancel_server.cancelled() => break,
            }
        }
    });

    (target_url, cancel)
}

fn spawn_wsblast(args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Delivers SIGINT, as an interactive Ctrl+C would.
fn interrupt(child: &Child) {
    let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill(2) only reads its integer arguments; pid belongs to a child
    // this test spawned and has not yet reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
}

struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Waits for the child to exit and collects its output, failing the test if
/// it outlives `limit`.
async fn wait_with_limit(mut child: Child, limit: Duration) -> Finished {
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    let status = tokio::time::timeout(limit, child.wait()).await;
    if status.is_err() {
        let _ = child.kill().await;
    }
    let stdout = stdout_task.await.unwrap();
    let stderr = stderr_task.await.unwrap();

    match status {
        Ok(status) => Finished {
            code: status.unwrap().code(),
            stdout,
            stderr,
        },
        Err(_) => panic!(
            "wsblast did not exit within {limit:?} of SIGINT\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_connection_ramp_still_reports() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let child = spawn_wsblast(&[
        &target_url,
        "-c",
        "20",
        "--ramp-rate",
        "5",
        "-d",
        "10s",
        "--no-progress",
    ]);

    tokio::time::sleep(Duration::from_secs(1)).await;
    interrupt(&child);
    let finished = wait_with_limit(child, Duration::from_secs(5)).await;

    assert!(
        matches!(finished.code, Some(0 | 2)),
        "expected a graceful exit after SIGINT, got {:?}\nstdout:\n{}\nstderr:\n{}",
        finished.code,
        finished.stdout,
        finished.stderr
    );
    assert!(
        finished.stdout.contains(SUMMARY_HEADER),
        "missing summary after SIGINT\nstdout:\n{}\nstderr:\n{}",
        finished.stdout,
        finished.stderr
    );

    cancel_server.cancel();
}

#[cfg(feature = "scenario")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_scenario_stops_remaining_phases() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let scenario_path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("cli_signals_two_phase_{}.yaml", std::process::id()));
    let phase = |name: &str| {
        format!(
            "  - name: \"{name}\"\n    duration: \"5s\"\n    connections: 2\n    ramp_rate: 4\n    rate_per_conn: 20\n"
        )
    };
    let scenario = format!(
        "version: \"1.0\"\ntarget: \"{target_url}\"\nmetadata:\n  name: \"signal\"\nphases:\n{}{}",
        phase("first"),
        phase("second")
    );
    std::fs::write(&scenario_path, scenario).unwrap();

    let child = spawn_wsblast(&[
        "--scenario",
        scenario_path.to_str().unwrap(),
        "--no-progress",
    ]);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    interrupt(&child);
    let finished = wait_with_limit(child, Duration::from_secs(4)).await;
    let _ = std::fs::remove_file(&scenario_path);

    assert!(
        finished.stderr.contains("phase 1/2"),
        "phase 1 never started\nstderr:\n{}",
        finished.stderr
    );
    assert!(
        !finished.stderr.contains("phase 2/2"),
        "SIGINT only stopped the current phase\nstderr:\n{}",
        finished.stderr
    );
    assert!(
        matches!(finished.code, Some(0 | 2)),
        "expected a graceful exit after SIGINT, got {:?}\nstdout:\n{}\nstderr:\n{}",
        finished.code,
        finished.stdout,
        finished.stderr
    );
    assert!(
        finished.stdout.contains(SUMMARY_HEADER),
        "missing summary after SIGINT\nstdout:\n{}\nstderr:\n{}",
        finished.stdout,
        finished.stderr
    );

    cancel_server.cancel();
}

/// Runs wsblast to completion and returns its exit code and output.
async fn run_wsblast(args: &[&str]) -> Finished {
    wait_with_limit(spawn_wsblast(args), Duration::from_secs(10)).await
}

#[tokio::test]
async fn unknown_flag_is_a_usage_error_with_exit_code_1() {
    let finished = run_wsblast(&["ws://127.0.0.1:9", "--no-such-flag"]).await;

    assert_eq!(
        finished.code,
        Some(1),
        "a usage error must not look like an SLO breach\nstderr:\n{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("--no-such-flag"),
        "clap's message is missing\nstderr:\n{}",
        finished.stderr
    );
}

#[tokio::test]
async fn conflicting_flags_exit_with_code_1() {
    let finished = run_wsblast(&["ws://127.0.0.1:9", "--url", "ws://127.0.0.1:10"]).await;
    assert_eq!(finished.code, Some(1), "stderr:\n{}", finished.stderr);
}

#[tokio::test]
async fn help_and_version_exit_with_code_0() {
    let help = run_wsblast(&["--help"]).await;
    assert_eq!(help.code, Some(0), "stderr:\n{}", help.stderr);
    assert!(help.stdout.contains("Usage:"), "stdout:\n{}", help.stdout);

    let version = run_wsblast(&["--version"]).await;
    assert_eq!(version.code, Some(0), "stderr:\n{}", version.stderr);
    assert!(
        version.stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout:\n{}",
        version.stdout
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn impossible_slo_exits_with_code_2() {
    let (target_url, cancel_server) = spawn_echo_server().await;

    let finished = run_wsblast(&[
        &target_url,
        "-c",
        "1",
        "-d",
        "300ms",
        "--no-progress",
        "--max-p50",
        "1ns",
    ])
    .await;

    assert_eq!(
        finished.code,
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        finished.stdout,
        finished.stderr
    );
    assert!(finished.stdout.contains(SUMMARY_HEADER));

    cancel_server.cancel();
}
