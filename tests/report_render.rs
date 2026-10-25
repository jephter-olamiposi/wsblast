//! Report rendering through the public API.
//!
//! The text, JSON and Markdown reports of a live run are compared byte for
//! byte with the files under `tests/fixtures/live_report/`, which were
//! written by the renderers before the HTML report and `wsblast render`
//! were added. Regenerate them only on purpose, with
//! `WSBLAST_BLESS=1 cargo test --test report_render`.

use std::path::Path;
use std::time::Duration;
use wsblast::{
    AggregatedMetrics, LoadTestConfig, OutputFormat, SloThresholds, Stage, evaluate_slos,
    render_report,
};

/// Metrics with every optional part present: response time, a 25-point
/// time series, one error category (a map with several would print in hash
/// order) and capture totals.
fn rich_metrics() -> AggregatedMetrics {
    let latency = |base: u64| {
        serde_json::json!({
            "count": 1000, "min_us": base / 2, "mean_us": base as f64 * 1.1,
            "max_us": base * 20, "std_dev_us": base as f64 / 3.0,
            "p50_us": base, "p75_us": base * 2, "p90_us": base * 3,
            "p95_us": base * 4, "p99_us": base * 8, "p999_us": base * 15,
            "p9999_us": base * 19,
        })
    };
    let points: Vec<serde_json::Value> = (0..25)
        .map(|i| {
            serde_json::json!({
                "start_secs": f64::from(i),
                "duration_secs": 1.0,
                "active_connections": 20,
                "messages_sent": 400,
                "messages_recv": 398,
                "bytes_sent": 4000,
                "bytes_recv": 3980,
                "errors": if i == 12 { 2 } else { 0 },
                "throughput_msg_per_sec": 400.0,
                "latency": {
                    "count": if i == 7 { 0 } else { 398 },
                    "p50_us": 900, "p90_us": 1500,
                    "p99_us": if i == 12 { 310_000 } else { 2_400 },
                    "max_us": 3_000,
                },
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "total_connections_attempted": 20,
        "total_connections_established": 18,
        "total_connections_failed": 1,
        "total_connections_interrupted": 1,
        "total_connections_not_started": 0,
        "total_reconnects": 3,
        "total_unsolicited_frames": 5,
        "total_messages_sent": 10_000,
        "total_messages_recv": 9_950,
        "total_bytes_sent": 100_000,
        "total_bytes_recv": 99_500,
        "elapsed": { "secs": 25, "nanos": 4_000_000 },
        "throughput_msg_per_sec": 399.84,
        "throughput_bytes_per_sec": 7_978.2,
        "error_rate": 0.0049,
        "handshake_latency": latency(1_200),
        "message_rtt": latency(800),
        "response_time": latency(950),
        "time_series": {
            "interval_secs": 1.0,
            "latency_basis": "response_time",
            "points": points,
        },
        "error_breakdown": { "timeout": 49 },
        "capture": {
            "path": "run.jsonl",
            "frames_written": 20_000,
            "frames_dropped": 7,
            "limit": 100_000,
        },
    }))
    .expect("fixture metrics parse")
}

fn slo() -> SloThresholds {
    SloThresholds::new()
        .with_max_p99(Duration::from_millis(5))
        .with_max_error_rate(0.01)
        .with_min_throughput(500.0)
}

/// Builds a configuration that writes `format`.
type ConfigFn = fn(OutputFormat) -> LoadTestConfig;

fn constant_config(format: OutputFormat) -> LoadTestConfig {
    LoadTestConfig::builder("ws://127.0.0.1:9001/ws")
        .connections(20)
        .duration(Duration::from_secs(25))
        .rate_per_connection(20)
        .slo(slo())
        .output_format(format)
        .build()
        .expect("valid config")
}

fn staged_config(format: OutputFormat) -> LoadTestConfig {
    LoadTestConfig::builder("wss://staging.example.test/socket?room=a&b=1")
        .stages(vec![
            Stage::new(Duration::from_secs(5), 20),
            Stage::new(Duration::from_secs(15), 20),
            Stage::new(Duration::from_secs(5), 0),
        ])
        .output_format(format)
        .build()
        .expect("valid config")
}

/// Removes ANSI SGR sequences, so the comparison does not depend on whether
/// the terminal crates decided to colour the text report.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[test]
fn live_text_json_and_markdown_reports_are_unchanged() {
    let bless = std::env::var_os("WSBLAST_BLESS").is_some();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live_report");
    let metrics = rich_metrics();
    let mut stale = Vec::new();

    let configs: [(&str, ConfigFn); 2] = [("constant", constant_config), ("staged", staged_config)];
    for (name, config) in configs {
        for (format, extension) in [
            (OutputFormat::Text, "txt"),
            (OutputFormat::Json, "json"),
            (OutputFormat::Markdown, "md"),
        ] {
            let config = config(format);
            let evaluation = evaluate_slos(config.slo(), &metrics);
            let rendered = strip_ansi(&render_report(&config, &metrics, &evaluation));
            let path = dir.join(format!("{name}.{extension}"));
            if bless {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &rendered).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .replace("\r\n", "\n");
            if expected != rendered {
                stale.push(format!("{}\n--- rendered:\n{rendered}", path.display()));
            }
        }
    }

    assert!(stale.is_empty(), "reports changed:\n{}", stale.join("\n"));
}

#[test]
fn live_html_report_is_one_self_contained_page() {
    let metrics = rich_metrics();
    for config in [
        constant_config(OutputFormat::Html),
        staged_config(OutputFormat::Html),
    ] {
        let evaluation = evaluate_slos(config.slo(), &metrics);
        let html = render_report(&config, &metrics, &evaluation);
        assert!(html.starts_with("<!doctype html>"), "{html}");
        assert!(html.trim_end().ends_with("</html>"));
        assert_eq!(html.matches("<svg class=\"chart\"").count(), 4);
        assert!(!html.contains("<script"));
        // The staged target's query string is escaped like any report text.
        assert!(!html.contains("room=a&b=1"));
        let without_data = html.replace("wss://staging.example.test/socket?room=a&amp;b=1", "");
        assert!(!without_data.contains("http://") && !without_data.contains("https://"));
        assert!(html.contains("Response Time (from schedule)"));
        assert!(html.contains("Operation Timeout"));
        assert!(html.contains("20000 frames written, 7 dropped (run.jsonl)"));
    }
    let constant = constant_config(OutputFormat::Html);
    let html = render_report(
        &constant,
        &metrics,
        &evaluate_slos(constant.slo(), &metrics),
    );
    assert!(html.contains("FAIL: 2 of 3 SLO checks failed"));
    assert!(html.contains("20 msg/s per connection"));
    let staged = staged_config(OutputFormat::Html);
    let html = render_report(&staged, &metrics, &evaluate_slos(staged.slo(), &metrics));
    assert!(html.contains("No SLO thresholds were set"));
    assert!(html.contains("5s:20, 15s:20, 5s:0"));
}

// The compiled binary.

#[cfg(feature = "compare")]
fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/live_report")
        .join(name)
}

#[cfg(feature = "compare")]
struct Output {
    code: Option<i32>,
    stderr: String,
}

#[cfg(feature = "compare")]
fn wsblast(args: &[&std::ffi::OsStr]) -> Output {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run wsblast");
    Output {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[cfg(feature = "compare")]
fn os(text: &str) -> &std::ffi::OsStr {
    text.as_ref()
}

#[cfg(feature = "compare")]
#[test]
fn compare_rejects_the_html_format() {
    let report = fixture("constant.json");
    let result = wsblast(&[
        os("--compare"),
        report.as_os_str(),
        report.as_os_str(),
        os("--format"),
        os("html"),
    ]);
    assert_eq!(result.code, Some(1));
    assert!(result.stderr.contains("--format html"), "{}", result.stderr);
}
