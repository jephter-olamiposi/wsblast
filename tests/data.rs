//! Per-connection test data: `--data` rows reach payloads and handshake
//! headers, one row per connection.

use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;
use wsblast::{LoadTestConfig, Runner};

/// What one connection showed the server: its `x-user` header and the
/// first text frame it sent.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// An echo server that records, for every connection, the `x-user`
/// handshake header and the first frame.
async fn spawn_recording_server() -> (String, Seen, tokio_util::sync::CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (record, stop) = (Arc::clone(&seen), cancel.clone());
    tokio::spawn(async move {
        loop {
            let (stream, _) = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                () = stop.cancelled() => return,
            };
            let record = Arc::clone(&record);
            tokio::spawn(async move {
                let header = Arc::new(Mutex::new(String::new()));
                let keep = Arc::clone(&header);
                #[allow(clippy::result_large_err)]
                let callback = move |req: &Request, resp: Response| {
                    *keep.lock().unwrap() = req
                        .headers()
                        .get("x-user")
                        .map(|v| v.to_str().unwrap_or("").to_string())
                        .unwrap_or_default();
                    Ok(resp)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                let (mut sink, mut frames) = ws.split();
                let mut first = true;
                while let Some(Ok(msg)) = frames.next().await {
                    if let Message::Text(text) = &msg {
                        if first {
                            first = false;
                            let header = header.lock().unwrap().clone();
                            record.lock().unwrap().push((header, text.to_string()));
                        }
                        if sink.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (url, seen, cancel)
}

/// Writes `name` into a directory of this test process.
fn write_file(name: &str, content: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("data_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[tokio::test]
async fn every_connection_sends_its_own_row_in_payload_and_header() {
    let (url, seen, cancel) = spawn_recording_server().await;
    let data = write_file(
        "users.csv",
        "user,token\nada,t-ada\nbob,t-bob\n\"cy, jr\",t-cy\n",
    );
    let config = LoadTestConfig::builder(url)
        .connections(3)
        .max_requests(9)
        .duration(Duration::from_secs(3))
        .data_file(&data)
        .payload_text(r#"{"user":"{{data.user}}","token":"{{data.token}}","seq":{{seq}}}"#)
        .header("X-User", "{{data.user}}#{{worker_id}}")
        .build()
        .unwrap();
    assert!(config.headers().get("x-user").is_none());
    let metrics = Runner::new(config)
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(metrics.total_connections_established, 3);
    assert_eq!(metrics.error_rate, 0.0, "{:?}", metrics.error_breakdown);

    let seen: BTreeSet<(String, String)> = seen.lock().unwrap().iter().cloned().collect();
    let expected: BTreeSet<(String, String)> = [
        ("ada", "t-ada", 0),
        ("bob", "t-bob", 1),
        ("cy, jr", "t-cy", 2),
    ]
    .into_iter()
    .map(|(user, token, worker)| {
        (
            format!("{user}#{worker}"),
            format!(r#"{{"user":"{user}","token":"{token}","seq":0}}"#),
        )
    })
    .collect();
    assert_eq!(seen, expected);
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn a_scenario_reads_its_data_file_next_to_it() {
    let (url, seen, cancel) = spawn_recording_server().await;
    write_file("ids.jsonl", "{\"id\":41}\n{\"id\":42}\n");
    let scenario = write_file(
        "ids.yaml",
        r#"version: "1.0"
metadata:
  name: data
data: ids.jsonl
phases:
  - name: send
    connections: 2
    duration: 2s
    actions:
      - type: Send
        payload: '{"id":{{data.id}}}'
      - type: Close
"#,
    );
    // Run from another directory: the data file is found next to the
    // scenario, not in the working directory.
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .current_dir(std::env::temp_dir())
        .args([
            "--scenario",
            scenario.to_str().unwrap(),
            "--url",
            &url,
            "--no-progress",
            "--no-reconnect",
        ])
        .output()
        .await
        .unwrap();
    cancel.cancel();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let frames: BTreeSet<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, f)| f.clone())
        .collect();
    assert_eq!(
        frames,
        BTreeSet::from([r#"{"id":41}"#.to_string(), r#"{"id":42}"#.to_string()])
    );
}

#[test]
fn data_errors_name_the_file_column_and_row() {
    let message =
        |builder: wsblast::LoadTestConfigBuilder| builder.build().unwrap_err().to_string();
    let users = write_file("cols.csv", "user\nada\n");
    let base = || LoadTestConfig::builder("ws://127.0.0.1:9001");

    let unknown = message(base().data_file(&users).payload_text("{{data.email}}"));
    assert!(
        unknown.contains("has no column 'email' (columns: user)"),
        "{unknown}"
    );
    let no_file = message(base().payload_text("{{data.user}}"));
    assert!(no_file.contains("needs a data file"), "{no_file}");
    let missing = message(base().data_file("/nonexistent/users.csv"));
    assert!(
        missing.contains("Data file error: /nonexistent/users.csv"),
        "{missing}"
    );
    let bad = write_file("ctrl.csv", "user\nok\nbad\u{1}value\n");
    let header = message(base().data_file(&bad).header("X-User", "{{data.user}}"));
    assert!(
        header.contains("header x-user: the value is not a valid HTTP header value with row 2"),
        "{header}"
    );
    // Braces that are not a placeholder are sent as written.
    assert!(base().payload_text("{{name}}").build().is_ok());
}
