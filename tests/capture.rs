//! `--capture`: the JSONL file matches the report, honours the record limit
//! and round-trips binary payloads.
#![cfg(feature = "capture")]

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;
use wsblast::{AggregatedMetrics, LoadTestConfig, LoadTestConfigBuilder, Runner};

/// Spawns an in-process WebSocket echo server on an ephemeral OS port.
async fn spawn_echo_server() -> (String, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let cancel = CancellationToken::new();
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
                            match msg {
                                Message::Text(_) | Message::Binary(_) => {
                                    let _ = sink.send(msg).await;
                                }
                                Message::Close(_) => break,
                                _ => {}
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

fn capture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("capture_{name}_{}.jsonl", std::process::id()))
}

fn config(url: &str, path: &PathBuf) -> LoadTestConfigBuilder {
    LoadTestConfig::builder(url)
        .connections(2)
        .duration(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(2))
        .message_timeout(Duration::from_secs(2))
        .capture(path)
}

async fn run(config: LoadTestConfigBuilder) -> AggregatedMetrics {
    Runner::new(config.build().unwrap())
        .with_signal_handling(false)
        .run()
        .await
        .unwrap()
}

/// Reads the capture file: the header and the records.
fn read_capture(path: &PathBuf) -> (serde_json::Value, Vec<serde_json::Value>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines().map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("invalid JSON line {line:?}: {err}"))
    });
    let header = lines.next().expect("header line");
    (header, lines.collect())
}

fn count(records: &[serde_json::Value], dir: &str, kind: &str) -> u64 {
    records
        .iter()
        .filter(|r| r["dir"] == dir && r["kind"] == kind)
        .count() as u64
}

/// Minimal standard base64 decoder, independent of the crate's encoder.
fn decode_base64(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut nbits = 0;
    let mut out = Vec::new();
    for c in input.bytes().filter(|&c| c != b'=') {
        let value = ALPHABET.iter().position(|&a| a == c).expect("base64 digit") as u32;
        bits = (bits << 6) | value;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    out
}

#[tokio::test]
async fn capture_of_an_echo_run_matches_the_report() {
    let (url, server) = spawn_echo_server().await;
    let path = capture_path("echo");

    let metrics = run(config(&url, &path)
        .rate_per_connection(50)
        .max_requests(40)
        .payload_text(r#"{"seq":{{seq}}}"#))
    .await;
    server.cancel();

    let (header, records) = read_capture(&path);
    assert_eq!(header["wsblast_capture"], 1);
    assert_eq!(header["target"], format!("{url}/"));
    assert!(header["started"].as_str().unwrap().ends_with('Z'));

    assert_eq!(metrics.total_messages_sent, 40);
    assert_eq!(count(&records, "sent", "text"), metrics.total_messages_sent);
    assert_eq!(count(&records, "recv", "text"), metrics.total_messages_recv);
    assert_eq!(
        count(&records, "recv", "open"),
        metrics.total_connections_established + metrics.total_reconnects
    );

    let mut last = [0u64; 2];
    for record in &records {
        let t_us = record["t_us"].as_u64().unwrap();
        let worker = record["worker"].as_u64().unwrap() as usize;
        assert!(t_us >= last[worker], "a worker's records are in time order");
        last[worker] = t_us;
        if record["kind"] == "text" {
            let data = record["data"].as_str().unwrap();
            assert_eq!(record["len"].as_u64().unwrap(), data.len() as u64);
            assert!(data.starts_with(r#"{"seq":"#), "{data}");
        }
    }

    let summary = metrics.capture.expect("capture summary");
    assert_eq!(summary.frames_written, records.len() as u64);
    assert_eq!(summary.frames_dropped, 0);
    assert_eq!(summary.limit, 100_000);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capture_limit_drops_and_counts_the_rest() {
    let (url, server) = spawn_echo_server().await;
    let path = capture_path("limit");

    let metrics = run(config(&url, &path)
        .connections(1)
        .max_requests(200)
        .capture_limit(10))
    .await;
    server.cancel();

    let (_, records) = read_capture(&path);
    let summary = metrics.capture.expect("capture summary");
    assert_eq!(records.len(), 10);
    assert_eq!(summary.frames_written, 10);
    assert_eq!(summary.limit, 10);
    // One open record plus every data frame; Close frames add more.
    let produced = 1 + metrics.total_messages_sent + metrics.total_messages_recv;
    assert!(
        summary.frames_written + summary.frames_dropped >= produced,
        "{summary:?} for {produced} records"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn binary_frames_round_trip_through_base64() {
    let (url, server) = spawn_echo_server().await;
    let path = capture_path("binary");
    let payload: Vec<u8> = (0..=255u8).rev().chain([0, 0x80, 0xff]).collect();

    let metrics = run(config(&url, &path)
        .connections(1)
        .max_requests(5)
        .payload_binary(payload.clone()))
    .await;
    server.cancel();

    let (_, records) = read_capture(&path);
    let binary: Vec<_> = records.iter().filter(|r| r["kind"] == "binary").collect();
    assert_eq!(
        binary.len() as u64,
        metrics.total_messages_sent + metrics.total_messages_recv
    );
    assert!(!binary.is_empty());
    for record in binary {
        assert_eq!(record["len"].as_u64().unwrap(), payload.len() as u64);
        assert_eq!(decode_base64(record["data"].as_str().unwrap()), payload);
    }
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cli_reports_capture_totals() {
    let (url, server) = spawn_echo_server().await;
    let path = capture_path("cli");

    let run_cli = |format: &'static str| {
        let url = url.clone();
        let path = path.clone();
        async move {
            let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
                .args([
                    url.as_str(),
                    "-c",
                    "1",
                    "-n",
                    "5",
                    "-d",
                    "5s",
                    "--no-progress",
                ])
                .args(["--format", format, "--capture"])
                .arg(&path)
                .env("NO_COLOR", "1")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        }
    };

    let text = run_cli("text").await;
    let (_, records) = read_capture(&path);
    assert!(
        text.contains(&format!(
            "Capture: {} frames written, 0 dropped",
            records.len()
        )),
        "{text}"
    );

    let json: serde_json::Value = serde_json::from_str(&run_cli("json").await).unwrap();
    let capture = &json["metrics"]["capture"];
    assert_eq!(capture["frames_dropped"], 0);
    assert_eq!(capture["limit"], 100_000);
    assert_eq!(capture["path"], path.display().to_string());
    let (_, records) = read_capture(&path);
    assert_eq!(capture["frames_written"], records.len() as u64);

    let markdown = run_cli("markdown").await;
    assert!(markdown.contains("- **Capture:** "), "{markdown}");

    server.cancel();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unwritable_capture_path_is_an_error() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("no_such_directory")
        .join("capture.jsonl");
    let result = Runner::new(
        LoadTestConfig::builder("ws://127.0.0.1:9")
            .connections(1)
            .duration(Duration::from_millis(100))
            .capture(&path)
            .build()
            .unwrap(),
    )
    .with_signal_handling(false)
    .run()
    .await;
    let err = result.expect_err("capture file cannot be created");
    assert!(err.to_string().contains("capture file"), "{err}");
}

#[tokio::test]
async fn runs_without_capture_report_none() {
    let (url, server) = spawn_echo_server().await;
    let metrics = run(LoadTestConfig::builder(url.as_str())
        .connections(1)
        .max_requests(3)
        .duration(Duration::from_secs(5)))
    .await;
    server.cancel();
    assert!(metrics.capture.is_none());
    let json = serde_json::to_value(&metrics).unwrap();
    assert!(json.get("capture").is_none());
}

#[cfg(feature = "scenario")]
#[tokio::test]
async fn scenario_phases_share_one_capture_file() {
    let (url, server) = spawn_echo_server().await;
    let path = capture_path("scenario");
    let phase = |name: &str, connections: usize| {
        format!(
            "  - name: \"{name}\"\n    duration: \"200ms\"\n    connections: {connections}\n    actions:\n      - type: Send\n        count: 2\n      - type: Close\n        code: 1000\n        reason: \"done\"\n"
        )
    };
    let yaml = format!(
        "version: \"1.0\"\ntarget: \"{url}\"\nmetadata:\n  name: \"capture\"\nphases:\n{}{}",
        phase("first", 2),
        phase("second", 3)
    );
    let scenario = wsblast::ScenarioConfig::from_yaml(&yaml).unwrap();
    let base = config(&url, &path).build().unwrap();
    let outcome = wsblast::ScenarioExecutor::new(&scenario, base, None, None)
        .unwrap()
        .with_signal_handling(false)
        .run()
        .await
        .unwrap();
    server.cancel();
    let metrics = outcome.metrics;

    let (_, records) = read_capture(&path);
    assert_eq!(count(&records, "recv", "open"), 5);
    assert_eq!(count(&records, "sent", "text"), metrics.total_messages_sent);
    assert_eq!(count(&records, "recv", "text"), metrics.total_messages_recv);
    let closes: Vec<_> = records
        .iter()
        .filter(|r| r["dir"] == "sent" && r["kind"] == "close")
        .collect();
    assert_eq!(closes.len(), 5);
    for close in closes {
        assert_eq!(close["code"], 1000);
        assert_eq!(close["data"], "done");
        assert_eq!(close["len"], 6);
    }
    assert_eq!(
        metrics.capture.expect("capture summary").frames_written,
        records.len() as u64
    );
    let _ = std::fs::remove_file(&path);
}
