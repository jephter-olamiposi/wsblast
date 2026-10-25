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

// `wsblast render`, through the compiled binary.

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/live_report")
        .join(name)
}

fn scratch(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("render_{}_{name}", std::process::id()))
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn wsblast(args: &[&std::ffi::OsStr]) -> Output {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run wsblast");
    Output {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn os(text: &str) -> &std::ffi::OsStr {
    text.as_ref()
}

#[test]
fn render_writes_html_from_a_saved_report() {
    let report = fixture("constant.json");
    let out = scratch("constant.html");
    let result = wsblast(&[
        os("render"),
        report.as_os_str(),
        os("--format"),
        os("html"),
        os("-o"),
        out.as_os_str(),
    ]);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(result.stdout.is_empty(), "{}", result.stdout);
    let html = std::fs::read_to_string(&out).expect("html written");
    assert!(html.starts_with("<!doctype html>"));
    assert_eq!(html.matches("<svg class=\"chart\"").count(), 4);
    assert!(html.contains("FAIL: 2 of 3 SLO checks failed"), "{html}");
    assert!(html.contains("Response Time (from schedule)"));
    assert!(html.contains("Not recorded in the JSON report"));
    assert!(html.contains("Report saved"));
    assert!(!html.contains("http://") && !html.contains("https://"));
    let _ = std::fs::remove_file(out);
}

#[test]
fn render_defaults_to_html_on_stdout_and_markdown_matches_the_live_report() {
    let report = fixture("constant.json");
    let html = wsblast(&[os("render"), report.as_os_str()]);
    assert_eq!(html.code, Some(0), "{}", html.stderr);
    assert!(html.stdout.starts_with("<!doctype html>"));

    // Markdown does not show the rate, the one setting a JSON report lacks,
    // so a saved report renders exactly as the live run printed it.
    for (name, format) in [("constant", "markdown"), ("scenario", "markdown")] {
        let rendered = wsblast(&[
            os("render"),
            fixture(&format!("{name}.json")).as_os_str(),
            os("--format"),
            os(format),
        ]);
        assert_eq!(rendered.code, Some(0), "{}", rendered.stderr);
        let live = std::fs::read_to_string(fixture(&format!("{name}.md")))
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(rendered.stdout.replace("\r\n", "\n"), format!("{live}\n"));
    }

    let text = wsblast(&[
        os("render"),
        fixture("scenario.json").as_os_str(),
        os("--format"),
        os("text"),
    ]);
    assert_eq!(text.code, Some(0), "{}", text.stderr);
    assert!(text.stdout.contains("Target URL (phase 'spike')"));
    assert!(text.stdout.contains("not recorded in the JSON report"));
}

#[test]
fn render_reads_reports_from_wsblast_0_2_0() {
    // A 0.2.0 report: no time series, response time, interrupted counter or
    // capture totals.
    let latency = serde_json::json!({
        "count": 10, "min_us": 100, "mean_us": 150.0, "max_us": 400,
        "std_dev_us": 20.0, "p50_us": 140, "p75_us": 160, "p90_us": 200,
        "p95_us": 250, "p99_us": 390, "p999_us": 400, "p9999_us": 400,
    });
    let report = serde_json::json!({
        "schema_version": "1.0.0",
        "target_url": "ws://127.0.0.1:9001/",
        "connections": 2,
        "duration_secs": 1.5,
        "mode": "Echo",
        "metrics": {
            "total_connections_attempted": 2,
            "total_connections_established": 2,
            "total_connections_failed": 0,
            "total_reconnects": 0,
            "total_unsolicited_frames": 0,
            "total_messages_sent": 10,
            "total_messages_recv": 10,
            "total_bytes_sent": 50,
            "total_bytes_recv": 50,
            "elapsed": { "secs": 1, "nanos": 500_000_000 },
            "throughput_msg_per_sec": 6.67,
            "throughput_bytes_per_sec": 66.7,
            "error_rate": 0.0,
            "handshake_latency": latency,
            "message_rtt": latency,
            "error_breakdown": {},
        },
        "slo": { "passed": true, "checks": [] },
    });
    let path = scratch("v020.json");
    std::fs::write(&path, report.to_string()).unwrap();

    let html = wsblast(&[os("render"), path.as_os_str()]);
    assert_eq!(html.code, Some(0), "{}", html.stderr);
    assert!(html.stdout.contains("This report has no time series"));
    assert!(!html.stdout.contains("<svg"));
    assert!(!html.stdout.contains("Response Time"));
    assert!(html.stdout.contains("No SLO thresholds were set"));

    for format in ["text", "markdown"] {
        let rendered = wsblast(&[os("render"), path.as_os_str(), os("--format"), os(format)]);
        assert_eq!(rendered.code, Some(0), "{format}: {}", rendered.stderr);
        assert!(rendered.stdout.contains("ws://127.0.0.1:9001/"));
        assert!(!rendered.stdout.contains("Time Window"));
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn render_exits_with_1_for_missing_or_foreign_files() {
    let missing = scratch("missing.json");
    let result = wsblast(&[os("render"), missing.as_os_str()]);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("cannot read report"),
        "{}",
        result.stderr
    );
    assert!(result.stdout.is_empty());

    let cases = [
        ("not-json.json", "<html></html>", "not valid JSON"),
        (
            "other.json",
            r#"{"name": "package", "version": "1.0.0"}"#,
            "not a wsblast JSON report",
        ),
        (
            "future.json",
            r#"{"schema_version": "2.0.0"}"#,
            "unsupported report schema version '2.0.0'",
        ),
        (
            "partial.json",
            r#"{"schema_version": "1.0.0", "target_url": "ws://h/"}"#,
            "not a wsblast JSON report",
        ),
    ];
    for (name, content, message) in cases {
        let path = scratch(name);
        std::fs::write(&path, content).unwrap();
        let result = wsblast(&[os("render"), path.as_os_str()]);
        assert_eq!(result.code, Some(1), "{name}");
        assert!(result.stderr.contains(message), "{name}: {}", result.stderr);
        assert!(result.stdout.is_empty(), "{name}");
        let _ = std::fs::remove_file(path);
    }

    let result = wsblast(&[
        os("render"),
        fixture("constant.json").as_os_str(),
        os("--format"),
        os("json"),
    ]);
    assert_eq!(result.code, Some(1), "a saved report is already JSON");
    let result = wsblast(&[os("render")]);
    assert_eq!(result.code, Some(1), "the report path is required");
}

#[cfg(feature = "compare")]
#[test]
fn compare_writes_an_html_page() {
    let report = fixture("constant.json");
    let out = scratch("compare.html");
    let result = wsblast(&[
        os("--compare"),
        report.as_os_str(),
        report.as_os_str(),
        os("--format"),
        os("html"),
        os("-o"),
        out.as_os_str(),
    ]);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let html = std::fs::read_to_string(&out).expect("html written");
    assert_eq!(result.stdout, format!("{html}\n"), "printed and written");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("wsblast baseline comparison"));
    assert!(html.contains("No regressions: no metric changed by more than"));
    assert_eq!(html.matches("<svg class=\"chart\"").count(), 2);
    assert!(!html.contains("http://") && !html.contains("https://"));
    assert!(!html.contains("<script"));
    let _ = std::fs::remove_file(out);
}

/// A comparison JSON renders as the comparison it was written from: text
/// and Markdown byte for byte, HTML with the changes it records.
#[cfg(feature = "compare")]
#[test]
fn render_reads_comparison_json() {
    let legacy = |name: &str| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/compare_legacy")
            .join(name)
    };
    for direction in ["forward", "reverse"] {
        let saved = legacy(&format!("{direction}.json"));
        for (format, ext) in [("markdown", "md"), ("text", "txt")] {
            let rendered = wsblast(&[os("render"), saved.as_os_str(), os("--format"), os(format)]);
            assert_eq!(rendered.code, Some(0), "{}", rendered.stderr);
            let live = std::fs::read_to_string(legacy(&format!("{direction}.{ext}")))
                .unwrap()
                .replace("\r\n", "\n");
            assert_eq!(
                rendered.stdout.replace("\r\n", "\n"),
                live,
                "{direction} {format}"
            );
        }
    }

    let saved = legacy("forward.json");
    let comparison: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    let regressions = comparison["regressions"].as_array().unwrap().len();
    let improvements = comparison["improvements"].as_array().unwrap().len();
    let html = wsblast(&[os("render"), saved.as_os_str()]);
    assert_eq!(html.code, Some(0), "{}", html.stderr);
    assert!(html.stdout.starts_with("<!doctype html>"));
    assert!(html.stdout.contains("Comparison JSON, schema 1.1.0"));
    assert!(html.stdout.contains("Comparison saved"));
    assert!(
        html.stdout
            .contains(&format!("REGRESSION: {regressions} metrics regressed"))
    );
    assert_eq!(
        html.stdout.matches("<tr class=\"regressed\">").count(),
        regressions
    );
    assert_eq!(
        html.stdout.matches("<tr class=\"improved\">").count(),
        improvements
    );
    assert!(!html.stdout.contains("<svg"));

    let cases = [
        (
            r#"{"schema_version": "2.0.0", "regressions": [], "improvements": []}"#,
            "unsupported comparison schema version '2.0.0'",
        ),
        (
            r#"{"schema_version": "1.2.0", "regressions": [], "improvements": []}"#,
            "not a wsblast comparison JSON",
        ),
        (
            r#"{"schema_version": "1.2.0", "threshold": 10, "regressions": [{"metric": "x"}], "improvements": []}"#,
            "not a wsblast comparison JSON",
        ),
    ];
    for (content, message) in cases {
        let path = scratch("bad-comparison.json");
        std::fs::write(&path, content).unwrap();
        let result = wsblast(&[os("render"), path.as_os_str()]);
        assert_eq!(result.code, Some(1), "{content}");
        assert!(
            result.stderr.contains(message),
            "{content}: {}",
            result.stderr
        );
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(not(feature = "compare"))]
#[test]
fn render_names_the_feature_a_comparison_json_needs() {
    let saved =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compare_legacy/forward.json");
    let result = wsblast(&[os("render"), saved.as_os_str()]);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("'compare' feature"),
        "{}",
        result.stderr
    );
}
