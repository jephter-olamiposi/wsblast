//! Process-level signal handling tests that drive the compiled `wsblast` binary.
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

    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            let _ = child.kill().await;
            panic!("wsblast did not exit within {limit:?} of SIGINT");
        }
    };

    Finished {
        code: status.code(),
        stdout: stdout_task.await.unwrap(),
        stderr: stderr_task.await.unwrap(),
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
