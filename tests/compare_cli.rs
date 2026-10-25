//! `wsblast --compare` run as a process.
//!
//! The reports under `tests/fixtures/compare_legacy/` have the shape wsblast
//! 0.2.0 wrote: no time series and no response time. The `forward.*` and
//! `reverse.*` files are the comparison output of the code before
//! time-series metrics were added, so a comparison of two such reports must
//! stay byte for byte what it was.
#![cfg(feature = "compare")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn wsblast(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compare_legacy")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .replace("\r\n", "\n")
}

#[test]
fn comparison_of_reports_without_a_time_series_is_unchanged() {
    let baseline = fixture("baseline.json");
    let current = fixture("current.json");
    let (baseline, current) = (baseline.to_str().unwrap(), current.to_str().unwrap());

    for (format, ext) in [("text", "txt"), ("json", "json"), ("markdown", "md")] {
        for (direction, first, second) in [
            ("forward", baseline, current),
            ("reverse", current, baseline),
        ] {
            let output = wsblast(&[
                "--compare",
                first,
                second,
                "--compare-threshold",
                "3",
                "--format",
                format,
            ]);
            assert_eq!(output.status.code(), Some(0), "{direction} {format}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                read_fixture(&format!("{direction}.{ext}")),
                stdout,
                "{direction} {format}"
            );
        }
    }

    let gated = wsblast(&[
        "--compare",
        baseline,
        current,
        "--compare-threshold",
        "3",
        "--fail-on-regression",
    ]);
    assert_eq!(gated.status.code(), Some(2));
}

/// A 60-second echo report at 1000 msg/s with a 1-second time series. With
/// `stall`, the server stops answering during interval 30 and the late
/// replies arrive in interval 31: the run loses about 1.7% of its messages,
/// so the totals and whole-run percentiles barely move, but the worst
/// interval's p99 is two seconds.
fn report_with_series(stall: bool) -> serde_json::Value {
    let latency = |p999: u64| {
        serde_json::json!({
            "count": 60_000, "min_us": 300, "mean_us": 1_100.0, "max_us": 2_000_000,
            "std_dev_us": 400.0, "p50_us": 1_000, "p75_us": 1_200, "p90_us": 1_600,
            "p95_us": 1_900, "p99_us": 2_400, "p999_us": p999, "p9999_us": 9_000,
        })
    };
    let points: Vec<serde_json::Value> = (0..60)
        .map(|i| {
            let messages = if stall && i == 30 { 0 } else { 1_000 };
            let p99 = if stall && i == 31 { 2_000_000 } else { 2_400 };
            serde_json::json!({
                // Interval boundaries are sampled a little after the tick.
                "start_secs": if i == 0 { 0.0 } else { f64::from(i) + 0.000_7 },
                "duration_secs": 1.0,
                "active_connections": 10,
                "messages_sent": messages,
                "messages_recv": messages,
                "bytes_sent": messages * 64,
                "bytes_recv": messages * 64,
                "errors": 0,
                "throughput_msg_per_sec": f64::from(messages),
                "latency": {
                    "count": if messages == 0 { 0 } else { 1_000 },
                    "p50_us": if messages == 0 { 0 } else { 1_000 },
                    "p90_us": if messages == 0 { 0 } else { 1_600 },
                    "p99_us": if messages == 0 { 0 } else { p99 },
                    "max_us": if messages == 0 { 0 } else { p99 },
                },
            })
        })
        .collect();
    let total: u64 = if stall { 59_000 } else { 60_000 };
    serde_json::json!({
        "schema_version": "1.0.0",
        "target_url": "ws://127.0.0.1:9001/",
        "connections": 10,
        "duration_secs": 60.0,
        "mode": "echo",
        "metrics": {
            "total_connections_attempted": 10,
            "total_connections_established": 10,
            "total_connections_failed": 0,
            "total_messages_sent": total,
            "total_messages_recv": total,
            "total_bytes_sent": total * 64,
            "total_bytes_recv": total * 64,
            "elapsed": { "secs": 60, "nanos": 0 },
            "throughput_msg_per_sec": total as f64 / 60.0,
            "throughput_bytes_per_sec": (total * 128) as f64 / 60.0,
            "error_rate": 0.0,
            "handshake_latency": latency(9_000),
            "message_rtt": latency(if stall { 9_300 } else { 9_000 }),
            "time_series": { "interval_secs": 1.0, "latency_basis": "rtt", "points": points },
            "error_breakdown": {},
        },
        "slo": { "passed": true, "checks": [] },
    })
}

fn write_report(name: &str, report: &serde_json::Value) -> String {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("compare_{}_{name}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string_pretty(report).unwrap()).unwrap();
    path.to_str().unwrap().to_string()
}

fn metrics_of(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["metric"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_stall_hidden_in_the_totals_regresses_the_worst_interval() {
    let baseline = write_report("steady", &report_with_series(false));
    let current = write_report("stalled", &report_with_series(true));

    let output = wsblast(&["--compare", &baseline, &current, "--format", "json"]);
    assert_eq!(output.status.code(), Some(0));
    let comparison: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(comparison["schema_version"], "1.2.0");
    let regressed = metrics_of(&comparison["regressions"]);
    assert!(
        regressed.contains(&"series.p99_worst_us".to_string()),
        "{regressed:?}"
    );
    // Every whole-run metric stayed within the 10% threshold.
    assert!(
        regressed.iter().all(|metric| metric.starts_with("series.")),
        "{regressed:?}"
    );
    assert!(
        !regressed.contains(&"series.throughput_mean".to_string()),
        "{regressed:?}"
    );

    let text = wsblast(&["--compare", &baseline, &current]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("series.p99_worst_us"), "{text}");

    let gated = wsblast(&["--compare", &baseline, &current, "--fail-on-regression"]);
    assert_eq!(gated.status.code(), Some(2));

    // Outside the stall the two runs are the same.
    let windowed = wsblast(&[
        "--compare",
        &baseline,
        &current,
        "--compare-window",
        "35s..",
        "--fail-on-regression",
        "--format",
        "json",
    ]);
    assert_eq!(windowed.status.code(), Some(0));
    let comparison: serde_json::Value = serde_json::from_slice(&windowed.stdout).unwrap();
    assert_eq!(
        comparison["window"],
        serde_json::json!({ "start_secs": 35.0 })
    );
    assert!(metrics_of(&comparison["regressions"]).is_empty());
}

#[test]
fn invalid_or_unusable_windows_exit_with_1() {
    let with_series = write_report("window", &report_with_series(false));
    let legacy = fixture("baseline.json");
    let legacy = legacy.to_str().unwrap();

    let cases: [(&[&str], &str); 4] = [
        (
            &[
                "--compare",
                &with_series,
                legacy,
                "--compare-window",
                "10s..",
            ],
            "has no time series",
        ),
        (
            &[
                "--compare",
                &with_series,
                &with_series,
                "--compare-window",
                "100s..",
            ],
            "has no interval inside the window",
        ),
        (
            &[
                "--compare",
                &with_series,
                &with_series,
                "--compare-window",
                "50s..10s",
            ],
            "the start must be before the end",
        ),
        (
            &[
                "--compare",
                &with_series,
                &with_series,
                "--compare-window",
                "10s",
            ],
            "expected START..END",
        ),
    ];
    for (args, message) in cases {
        let output = wsblast(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }

    let without_compare = wsblast(&["ws://127.0.0.1:1", "--compare-window", "10s.."]);
    assert_eq!(without_compare.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&without_compare.stderr);
    assert!(stderr.contains("--compare <FILE> <FILE>"), "{stderr}");
}
