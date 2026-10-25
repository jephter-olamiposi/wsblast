//! `--auth-token-cmd` end to end: the compiled binary runs the command and
//! an in-process WebSocket server checks the header each handshake carries.
#![cfg(feature = "auth")]

use futures_util::{SinkExt, StreamExt};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

/// An echo server that records `header` from every handshake and closes each
/// connection after `replies` echoes, so workers reconnect.
struct RecordingServer {
    url: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
    cancel: CancellationToken,
}

impl RecordingServer {
    // The handshake callback's error type is fixed by tungstenite.
    #[allow(clippy::result_large_err)]
    async fn spawn(header: &'static str, replies: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let (seen_server, cancel_server) = (Arc::clone(&seen), cancel.clone());

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Ok((stream, _)) = listener.accept() => {
                        let seen = Arc::clone(&seen_server);
                        tokio::spawn(async move {
                            let record = |req: &Request, resp: Response| {
                                let value = req
                                    .headers()
                                    .get(header)
                                    .map(|v| v.to_str().unwrap().to_string());
                                seen.lock().unwrap().push(value);
                                Ok(resp)
                            };
                            let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, record).await
                            else {
                                return;
                            };
                            let (mut sink, mut reader) = ws.split();
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

        Self { url, seen, cancel }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn wsblast(args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("wsblast did not exit")
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn static_token_from_a_command_is_sent_as_bearer() {
    let server = RecordingServer::spawn("authorization", 1000).await;
    let output = wsblast(&[
        &server.url,
        "-c",
        "2",
        "-d",
        "500ms",
        "--no-progress",
        "--auth-type",
        "static",
        "--auth-token-cmd",
        "echo token-abc",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let seen = server.seen();
    assert!(seen.len() >= 2, "{seen:?}");
    let expected = Some("Bearer token-abc");
    assert!(seen.iter().all(|v| v.as_deref() == expected), "{seen:?}");
}

#[tokio::test]
async fn api_key_from_a_command_uses_the_api_key_header() {
    let server = RecordingServer::spawn("x-tenant-key", 1000).await;
    let output = wsblast(&[
        &server.url,
        "-c",
        "1",
        "-d",
        "300ms",
        "--no-progress",
        "--auth-type",
        "apikey",
        "--auth-apikey-header",
        "X-Tenant-Key",
        "--auth-token-cmd",
        "echo key-123",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let seen = server.seen();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|v| v.as_deref() == Some("key-123")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_failing_command_is_a_configuration_error_before_the_run() {
    let server = RecordingServer::spawn("authorization", 1000).await;
    let output = wsblast(&[
        &server.url,
        "-d",
        "300ms",
        "--no-progress",
        "--auth-type",
        "static",
        "--auth-token-cmd",
        "exit 3",
    ])
    .await;

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("Authentication token command failed"),
        "{stderr}"
    );
    assert!(stderr.contains('3'), "{stderr}");
    assert!(server.seen().is_empty(), "the run started");
}

/// Writes a token script that counts its runs in a file and returns the
/// command that runs it.
#[cfg(unix)]
fn counter_script(name: &str, body: &str) -> String {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("token.sh");
    let counter = dir.join("count");
    std::fs::write(
        &script,
        format!(
            "n=$(cat '{counter}' 2>/dev/null || echo 0)\nn=$((n + 1))\necho $n > '{counter}'\n{body}\n",
            counter = counter.display()
        ),
    )
    .unwrap();
    format!("sh '{}'", script.display())
}

#[cfg(unix)]
fn token_number(value: &Option<String>) -> u32 {
    value
        .as_deref()
        .and_then(|v| v.strip_prefix("Bearer token-"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected header {value:?}"))
}

#[cfg(unix)]
#[tokio::test]
async fn reconnects_present_the_refreshed_token() {
    let command = counter_script("refresh-ok", "echo token-$n");
    // Each session ends after 20 replies, so the worker reconnects many
    // times during the run.
    let server = RecordingServer::spawn("authorization", 20).await;
    let output = wsblast(&[
        &server.url,
        "-c",
        "1",
        "-d",
        "2s",
        "-r",
        "100",
        "--no-progress",
        "--auth-type",
        "static",
        "--auth-token-cmd",
        &command,
        "--auth-token-refresh",
        "300ms",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let tokens: Vec<u32> = server.seen().iter().map(token_number).collect();
    assert!(tokens.len() >= 3, "too few handshakes: {tokens:?}");
    assert_eq!(tokens[0], 1, "the first handshake used {tokens:?}");
    assert!(tokens.windows(2).all(|w| w[0] <= w[1]), "{tokens:?}");
    assert!(
        *tokens.last().unwrap() >= 3,
        "reconnects did not pick up refreshed tokens: {tokens:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_refresh_keeps_the_previous_token_and_warns_once() {
    let command = counter_script(
        "refresh-fail",
        "if [ $n -gt 1 ]; then echo 'mint service down' >&2; exit 1; fi\necho token-$n",
    );
    let server = RecordingServer::spawn("authorization", 20).await;
    let output = wsblast(&[
        &server.url,
        "-c",
        "1",
        "-d",
        "1500ms",
        "-r",
        "100",
        "--no-progress",
        "--auth-type",
        "static",
        "--auth-token-cmd",
        &command,
        "--auth-token-refresh",
        "200ms",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let tokens: Vec<u32> = server.seen().iter().map(token_number).collect();
    assert!(tokens.len() >= 3, "too few handshakes: {tokens:?}");
    assert!(tokens.iter().all(|&t| t == 1), "{tokens:?}");
    let stderr = stderr(&output);
    assert_eq!(
        stderr.matches("--auth-token-cmd refresh failed").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("mint service down"), "{stderr}");
    assert!(stderr.contains("exit status: 1"), "{stderr}");
}
