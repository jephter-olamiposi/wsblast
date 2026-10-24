//! Multi-format report generation (Terminal Text, Schema v1.0 JSON, Markdown) and SLO gating.

use crate::cli::OutputFormat;
use crate::config::{LoadTestConfig, SloThresholds};
use crate::metrics::AggregatedMetrics;
use colored::Colorize;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

/// Result of evaluating configured SLO thresholds against aggregated test metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloEvaluation {
    pub passed: bool,
    pub checks: Vec<SloCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloCheck {
    pub name: String,
    pub target: String,
    pub actual: String,
    pub passed: bool,
}

/// Baseline comparison for regression detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineComparison {
    pub baseline: AggregatedMetrics,
    pub current: AggregatedMetrics,
    pub regressions: Vec<Regression>,
    pub improvements: Vec<Improvement>,
}

/// `pct_change` is the signed relative change from baseline to current, in percent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Regression {
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub pct_change: f64,
}

/// `pct_change` is the signed relative change from baseline to current, in
/// percent: positive for a throughput gain, negative for a latency drop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Improvement {
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub pct_change: f64,
}

/// Report payloads are either the schema envelope (`{ "metrics": ... }`) or a
/// bare aggregated-metrics object produced by earlier versions.
#[derive(Deserialize)]
#[serde(untagged)]
enum ReportDocument {
    Envelope { metrics: AggregatedMetrics },
    Bare(AggregatedMetrics),
}

/// Extracts aggregated metrics from a schema v1.0 report or a bare metrics object.
///
/// # Errors
///
/// Returns a [`serde_json::Error`] when the document matches neither shape.
pub fn parse_report_metrics(json: &str) -> serde_json::Result<AggregatedMetrics> {
    match serde_json::from_str::<ReportDocument>(json)? {
        ReportDocument::Envelope { metrics } | ReportDocument::Bare(metrics) => Ok(metrics),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
    Neutral,
}

/// Classifies a metric by the direction that constitutes an improvement.
fn metric_direction(metric: &str) -> MetricDirection {
    match metric {
        // Input sizing, not an outcome: comparing it would flag intentional reconfiguration.
        "total_connections_attempted" => MetricDirection::Neutral,
        "total_connections_failed" | "error_rate" => MetricDirection::LowerIsBetter,
        _ if metric.starts_with("errors.") => MetricDirection::LowerIsBetter,
        _ if metric.starts_with("handshake_latency.") || metric.starts_with("message_rtt.") => {
            MetricDirection::LowerIsBetter
        }
        _ => MetricDirection::HigherIsBetter,
    }
}

/// Compares two metric snapshots and detects regressions/improvements.
///
/// Each metric is scored by its own direction: throughput and delivery counts
/// higher is better, while errors and latency percentiles lower is better. A
/// metric that appears from a zero baseline is reported as a full 100% move so
/// new error categories cannot be silently skipped.
pub fn compare_metrics(
    baseline: &AggregatedMetrics,
    current: &AggregatedMetrics,
    threshold: f64,
) -> BaselineComparison {
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();

    let baseline_map = metrics_to_map(baseline);
    let current_map = metrics_to_map(current);
    let metric_names: std::collections::BTreeSet<&String> =
        baseline_map.keys().chain(current_map.keys()).collect();

    for metric_name in metric_names {
        let direction = metric_direction(metric_name);
        if direction == MetricDirection::Neutral {
            continue;
        }

        let baseline_val = baseline_map.get(metric_name).copied().unwrap_or(0.0);
        let current_val = current_map.get(metric_name).copied().unwrap_or(0.0);
        if baseline_val == 0.0 && current_val == 0.0 {
            continue;
        }

        // A non-zero value against a zero baseline has no finite relative change;
        // use a full move so the direction still classifies it.
        let pct_change = if baseline_val == 0.0 {
            100.0
        } else {
            (current_val - baseline_val) / baseline_val * 100.0
        };

        let (regressed, improved) = match direction {
            MetricDirection::HigherIsBetter => (pct_change < -threshold, pct_change > threshold),
            MetricDirection::LowerIsBetter => (pct_change > threshold, pct_change < -threshold),
            MetricDirection::Neutral => (false, false),
        };

        if regressed {
            regressions.push(Regression {
                metric: metric_name.clone(),
                baseline: baseline_val,
                current: current_val,
                pct_change,
            });
        } else if improved {
            improvements.push(Improvement {
                metric: metric_name.clone(),
                baseline: baseline_val,
                current: current_val,
                pct_change,
            });
        }
    }

    BaselineComparison {
        baseline: baseline.clone(),
        current: current.clone(),
        regressions,
        improvements,
    }
}

fn metrics_to_map(metrics: &AggregatedMetrics) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();

    map.insert(
        "total_connections_attempted".into(),
        metrics.total_connections_attempted as f64,
    );
    map.insert(
        "total_connections_established".into(),
        metrics.total_connections_established as f64,
    );
    map.insert(
        "total_connections_failed".into(),
        metrics.total_connections_failed as f64,
    );
    map.insert(
        "total_messages_sent".into(),
        metrics.total_messages_sent as f64,
    );
    map.insert(
        "total_messages_recv".into(),
        metrics.total_messages_recv as f64,
    );
    map.insert("total_bytes_sent".into(), metrics.total_bytes_sent as f64);
    map.insert("total_bytes_recv".into(), metrics.total_bytes_recv as f64);
    map.insert(
        "throughput_msg_per_sec".into(),
        metrics.throughput_msg_per_sec,
    );
    map.insert(
        "throughput_bytes_per_sec".into(),
        metrics.throughput_bytes_per_sec,
    );
    map.insert("error_rate".into(), metrics.error_rate);
    for (category, count) in &metrics.error_breakdown {
        let name = serde_json::to_string(category)
            .unwrap_or_else(|_| "\"other\"".to_string())
            .trim_matches('"')
            .to_string();
        map.insert(format!("errors.{name}"), *count as f64);
    }
    map.insert(
        "handshake_latency.p50_us".into(),
        metrics.handshake_latency.p50_us as f64,
    );
    map.insert(
        "handshake_latency.p95_us".into(),
        metrics.handshake_latency.p95_us as f64,
    );
    map.insert(
        "handshake_latency.p99_us".into(),
        metrics.handshake_latency.p99_us as f64,
    );
    map.insert(
        "handshake_latency.p999_us".into(),
        metrics.handshake_latency.p999_us as f64,
    );
    map.insert(
        "message_rtt.p50_us".into(),
        metrics.message_rtt.p50_us as f64,
    );
    map.insert(
        "message_rtt.p95_us".into(),
        metrics.message_rtt.p95_us as f64,
    );
    map.insert(
        "message_rtt.p99_us".into(),
        metrics.message_rtt.p99_us as f64,
    );
    map.insert(
        "message_rtt.p999_us".into(),
        metrics.message_rtt.p999_us as f64,
    );

    map
}

/// Renders a comparison report in the specified format.
pub fn render_comparison_report(
    comparison: &BaselineComparison,
    format: OutputFormat,
    threshold: f64,
) -> String {
    match format {
        OutputFormat::Text => render_text_comparison(comparison, threshold),
        OutputFormat::Json => render_json_comparison(comparison, threshold),
        OutputFormat::Markdown => render_markdown_comparison(comparison, threshold),
    }
}

fn render_text_comparison(comparison: &BaselineComparison, threshold: f64) -> String {
    let mut out = String::new();

    out.push('\n');
    out.push_str(&format!(
        "{} {}\n\n",
        "wsblast".bold().cyan(),
        "Baseline Comparison Report".bold()
    ));

    out.push_str(&format!("Threshold: ±{:.1}%\n\n", threshold));

    if !comparison.regressions.is_empty() {
        let mut reg_table = Table::new();
        reg_table.load_preset(UTF8_FULL);
        reg_table.set_header(vec!["Metric", "Baseline", "Current", "Change", "Status"]);

        for r in &comparison.regressions {
            reg_table.add_row(vec![
                &r.metric,
                &format!("{:.3}", r.baseline),
                &format!("{:.3}", r.current),
                &format!("{:+.2}%", r.pct_change),
                "⚠ REGRESSION",
            ]);
        }
        out.push_str(&reg_table.to_string());
        out.push_str("\n\n");
    }

    if !comparison.improvements.is_empty() {
        let mut imp_table = Table::new();
        imp_table.load_preset(UTF8_FULL);
        imp_table.set_header(vec!["Metric", "Baseline", "Current", "Change", "Status"]);

        for i in &comparison.improvements {
            imp_table.add_row(vec![
                &i.metric,
                &format!("{:.3}", i.baseline),
                &format!("{:.3}", i.current),
                &format!("{:+.2}%", i.pct_change),
                "✓ IMPROVEMENT",
            ]);
        }
        out.push_str(&imp_table.to_string());
        out.push_str("\n\n");
    }

    if comparison.regressions.is_empty() && comparison.improvements.is_empty() {
        out.push_str("No significant changes detected beyond threshold.\n\n");
    }

    out
}

fn render_json_comparison(comparison: &BaselineComparison, threshold: f64) -> String {
    #[derive(serde::Serialize)]
    struct JsonComparison<'a> {
        schema_version: &'static str,
        threshold: f64,
        regressions: &'a Vec<Regression>,
        improvements: &'a Vec<Improvement>,
    }

    let report = JsonComparison {
        schema_version: "1.1.0",
        threshold,
        regressions: &comparison.regressions,
        improvements: &comparison.improvements,
    };

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

fn render_markdown_comparison(comparison: &BaselineComparison, threshold: f64) -> String {
    let mut md = String::new();
    md.push_str("# `wsblast` Baseline Comparison Report\n\n");
    md.push_str(&format!("- **Threshold:** `±{:.1}%`\n\n", threshold));

    if !comparison.regressions.is_empty() {
        md.push_str("## ⚠ Regressions Detected\n\n");
        md.push_str("| Metric | Baseline | Current | Change |\n");
        md.push_str("| :--- | ---: | ---: | ---: |\n");
        for r in &comparison.regressions {
            md.push_str(&format!(
                "| {} | {:.3} | {:.3} | {:+.2}% |\n",
                r.metric, r.baseline, r.current, r.pct_change
            ));
        }
        md.push('\n');
    }

    if !comparison.improvements.is_empty() {
        md.push_str("## ✓ Improvements Detected\n\n");
        md.push_str("| Metric | Baseline | Current | Change |\n");
        md.push_str("| :--- | ---: | ---: | ---: |\n");
        for i in &comparison.improvements {
            md.push_str(&format!(
                "| {} | {:.3} | {:.3} | {:+.2}% |\n",
                i.metric, i.baseline, i.current, i.pct_change
            ));
        }
        md.push('\n');
    }

    if comparison.regressions.is_empty() && comparison.improvements.is_empty() {
        md.push_str("No significant changes detected beyond threshold.\n");
    }

    md
}

/// Evaluates configured SLO criteria against aggregated test metrics.
pub fn evaluate_slos(slo: &SloThresholds, metrics: &AggregatedMetrics) -> SloEvaluation {
    let mut checks = Vec::new();
    let mut all_passed = true;

    let rtt = &metrics.message_rtt;
    let latency_targets = [
        ("Latency p50", slo.max_p50, rtt.p50_duration()),
        ("Latency p95", slo.max_p95, rtt.p95_duration()),
        ("Latency p99", slo.max_p99, rtt.p99_duration()),
        ("Latency p99.9", slo.max_p999, rtt.p999_duration()),
    ];
    for (name, target, actual) in latency_targets {
        let Some(target) = target else {
            continue;
        };
        let check = latency_check(name, target, actual, rtt.count);
        all_passed &= check.passed;
        checks.push(check);
    }

    if let Some(max_err) = slo.max_error_rate {
        let passed = metrics.error_rate <= max_err;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Error Rate".into(),
            target: format!("<= {:.2}%", max_err * 100.0),
            actual: format!("{:.2}%", metrics.error_rate * 100.0),
            passed,
        });
    }

    if let Some(min_tput) = slo.min_throughput {
        let passed = metrics.throughput_msg_per_sec >= min_tput;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Throughput (msg/s)".into(),
            target: format!(">= {:.1}", min_tput),
            actual: format!("{:.1}", metrics.throughput_msg_per_sec),
            passed,
        });
    }

    SloEvaluation {
        passed: all_passed,
        checks,
    }
}

/// Builds one latency SLO check.
///
/// A latency budget cannot pass without evidence: when no round trip was
/// measured (every request timed out, or the mode does not measure RTT) the
/// check fails instead of comparing the empty histogram's zero against the target.
fn latency_check(name: &str, target: Duration, actual: Duration, samples: u64) -> SloCheck {
    if samples == 0 {
        return SloCheck {
            name: name.into(),
            target: format!("<= {target:?}"),
            actual: "no RTT samples".into(),
            passed: false,
        };
    }
    SloCheck {
        name: name.into(),
        target: format!("<= {target:?}"),
        actual: format!("{actual:?}"),
        passed: actual <= target,
    }
}

/// Formats benchmark metrics into the requested output format string.
pub fn render_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    match config.output_format {
        OutputFormat::Text => render_text_report(config, metrics, slo),
        OutputFormat::Json => render_json_report(config, metrics, slo),
        OutputFormat::Markdown => render_markdown_report(config, metrics, slo),
    }
}

/// Emits the formatted report to stdout and persists to file if specified in configuration.
///
/// # Errors
///
/// Returns [`std::io::Error`] if writing to the specified `output_path` fails.
pub fn emit_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> std::io::Result<()> {
    let output = render_report(config, metrics, slo);
    println!("{output}");

    if let Some(path) = &config.output_path {
        fs::write(path, output)?;
    }

    Ok(())
}

fn render_text_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    let mut out = String::new();

    out.push('\n');
    out.push_str(&format!(
        "{} {}\n\n",
        "wsblast".bold().cyan(),
        "WebSocket Benchmark Summary".bold()
    ));

    let mut meta_table = Table::new();
    meta_table.load_preset(UTF8_FULL);
    meta_table.set_header(vec!["Configuration Parameter", "Value"]);
    meta_table.add_row(vec!["Target URL", config.target_url.as_str()]);
    meta_table.add_row(vec![
        "Connections (Concurrency)",
        &config.connections.to_string(),
    ]);
    meta_table.add_row(vec![
        "Elapsed Duration",
        &format!("{:.2?}", metrics.elapsed),
    ]);
    meta_table.add_row(vec![
        "Rate Control",
        &if config.rate_per_conn > 0 {
            format!("{} msg/sec per connection", config.rate_per_conn)
        } else {
            "Unthrottled".to_string()
        },
    ]);
    meta_table.add_row(vec!["Mode", &format!("{:?}", config.mode)]);
    out.push_str(&meta_table.to_string());
    out.push_str("\n\n");

    let mut tput_table = Table::new();
    tput_table.load_preset(UTF8_FULL);
    tput_table.set_header(vec!["Traffic & Throughput Metric", "Count / Rate"]);
    tput_table.add_row(vec![
        "Connections Established / Failed",
        &format!(
            "{}/{}",
            metrics.total_connections_established, metrics.total_connections_failed
        ),
    ]);
    tput_table.add_row(vec!["Reconnects", &metrics.total_reconnects.to_string()]);
    tput_table.add_row(vec![
        "Messages Sent / Received",
        &format!(
            "{} / {}",
            metrics.total_messages_sent, metrics.total_messages_recv
        ),
    ]);
    tput_table.add_row(vec![
        "Data Transferred (Sent / Recv)",
        &format!(
            "{:.2} MB / {:.2} MB",
            metrics.total_bytes_sent as f64 / (1024.0 * 1024.0),
            metrics.total_bytes_recv as f64 / (1024.0 * 1024.0)
        ),
    ]);
    tput_table.add_row(vec![
        "Throughput (Messages/sec)",
        &format!("{:.2} msg/sec", metrics.throughput_msg_per_sec),
    ]);
    tput_table.add_row(vec![
        "Bandwidth (Transfer Rate)",
        &format!(
            "{:.2} MB/sec",
            metrics.throughput_bytes_per_sec / (1024.0 * 1024.0)
        ),
    ]);
    tput_table.add_row(vec![
        "Error Rate",
        &format!("{:.2}%", metrics.error_rate * 100.0),
    ]);
    out.push_str(&tput_table.to_string());
    out.push_str("\n\n");

    let mut lat_table = Table::new();
    lat_table.load_preset(UTF8_FULL);
    lat_table.set_header(vec![
        "Percentile",
        "Handshake Latency",
        "Message RTT Latency",
    ]);

    lat_table.add_row(vec![
        "p50 (Median)",
        &format_us(metrics.handshake_latency.p50_us),
        &format_us(metrics.message_rtt.p50_us),
    ]);
    lat_table.add_row(vec![
        "p75",
        &format_us(metrics.handshake_latency.p75_us),
        &format_us(metrics.message_rtt.p75_us),
    ]);
    lat_table.add_row(vec![
        "p90",
        &format_us(metrics.handshake_latency.p90_us),
        &format_us(metrics.message_rtt.p90_us),
    ]);
    lat_table.add_row(vec![
        "p95",
        &format_us(metrics.handshake_latency.p95_us),
        &format_us(metrics.message_rtt.p95_us),
    ]);
    lat_table.add_row(vec![
        "p99",
        &format_us(metrics.handshake_latency.p99_us),
        &format_us(metrics.message_rtt.p99_us),
    ]);
    lat_table.add_row(vec![
        "p99.9",
        &format_us(metrics.handshake_latency.p999_us),
        &format_us(metrics.message_rtt.p999_us),
    ]);
    lat_table.add_row(vec![
        "Min / Mean / Max",
        &format!(
            "{} / {} / {}",
            format_us(metrics.handshake_latency.min_us),
            format_us(metrics.handshake_latency.mean_us as u64),
            format_us(metrics.handshake_latency.max_us)
        ),
        &format!(
            "{} / {} / {}",
            format_us(metrics.message_rtt.min_us),
            format_us(metrics.message_rtt.mean_us as u64),
            format_us(metrics.message_rtt.max_us)
        ),
    ]);
    out.push_str(&lat_table.to_string());
    out.push_str("\n\n");

    if !metrics.error_breakdown.is_empty() {
        let mut err_table = Table::new();
        err_table.load_preset(UTF8_FULL);
        err_table.set_header(vec!["Error Taxonomy Category", "Occurrences"]);
        for (cat, count) in &metrics.error_breakdown {
            err_table.add_row(vec![cat.to_string(), count.to_string()]);
        }
        out.push_str(&err_table.to_string());
        out.push_str("\n\n");
    }

    if !slo.checks.is_empty() {
        let mut slo_table = Table::new();
        slo_table.load_preset(UTF8_FULL);
        slo_table.set_header(vec![
            "SLO Objective",
            "Target Threshold",
            "Actual Value",
            "Gate Status",
        ]);

        for c in &slo.checks {
            let status_cell = if c.passed {
                Cell::new("PASSED").fg(Color::Green)
            } else {
                Cell::new("FAILED").fg(Color::Red)
            };
            slo_table.add_row(Row::from(vec![
                Cell::new(&c.name),
                Cell::new(&c.target),
                Cell::new(&c.actual),
                status_cell,
            ]));
        }
        out.push_str(&slo_table.to_string());
        out.push_str("\n\n");

        if slo.passed {
            out.push_str(&format!(
                "{} {}\n",
                "[PASS]".bold().green(),
                "All SLO thresholds satisfied."
            ));
        } else {
            out.push_str(&format!(
                "{} {}\n",
                "[FAIL]".bold().red(),
                "One or more SLO thresholds breached!"
            ));
        }
    }

    out
}

#[derive(Debug, serde::Serialize)]
struct JsonReport<'a> {
    schema_version: &'static str,
    target_url: &'a str,
    connections: usize,
    duration_secs: f64,
    mode: &'a str,
    metrics: &'a AggregatedMetrics,
    slo: &'a SloEvaluation,
}

fn render_json_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    let mode_str = format!("{:?}", config.mode);
    let report = JsonReport {
        schema_version: "1.0.0",
        target_url: config.target_url.as_str(),
        connections: config.connections,
        duration_secs: metrics.elapsed.as_secs_f64(),
        mode: &mode_str,
        metrics,
        slo,
    };

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

fn render_markdown_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    let mut md = String::new();
    md.push_str("# `wsblast` WebSocket Benchmark Report\n\n");
    md.push_str(&format!("- **Target:** `{}`\n", config.target_url));
    md.push_str(&format!(
        "- **Concurrency:** `{}` workers\n",
        config.connections
    ));
    md.push_str(&format!(
        "- **Connections:** `{}` established / `{}` failed / `{}` reconnects\n",
        metrics.total_connections_established,
        metrics.total_connections_failed,
        metrics.total_reconnects
    ));
    md.push_str(&format!("- **Duration:** `{:.2?}`\n", metrics.elapsed));
    md.push_str(&format!(
        "- **Throughput:** `{:.2}` msg/sec\n",
        metrics.throughput_msg_per_sec
    ));
    md.push_str(&format!(
        "- **Error Rate:** `{:.2}%`\n\n",
        metrics.error_rate * 100.0
    ));

    md.push_str("### Latency Distribution\n\n");
    md.push_str("| Percentile | Handshake | Message RTT |\n");
    md.push_str("| :--- | :--- | :--- |\n");
    md.push_str(&format!(
        "| **p50** | {} | {} |\n",
        format_us(metrics.handshake_latency.p50_us),
        format_us(metrics.message_rtt.p50_us)
    ));
    md.push_str(&format!(
        "| **p90** | {} | {} |\n",
        format_us(metrics.handshake_latency.p90_us),
        format_us(metrics.message_rtt.p90_us)
    ));
    md.push_str(&format!(
        "| **p95** | {} | {} |\n",
        format_us(metrics.handshake_latency.p95_us),
        format_us(metrics.message_rtt.p95_us)
    ));
    md.push_str(&format!(
        "| **p99** | {} | {} |\n",
        format_us(metrics.handshake_latency.p99_us),
        format_us(metrics.message_rtt.p99_us)
    ));
    md.push_str(&format!(
        "| **p99.9** | {} | {} |\n",
        format_us(metrics.handshake_latency.p999_us),
        format_us(metrics.message_rtt.p999_us)
    ));
    md.push_str(&format!(
        "| **Max** | {} | {} |\n\n",
        format_us(metrics.handshake_latency.max_us),
        format_us(metrics.message_rtt.max_us)
    ));

    if !slo.checks.is_empty() {
        md.push_str("### SLO Gate Evaluation\n\n");
        md.push_str("| Metric | Target | Actual | Status |\n");
        md.push_str("| :--- | :--- | :--- | :--- |\n");
        for c in &slo.checks {
            let icon = if c.passed { "✅ PASS" } else { "❌ FAIL" };
            md.push_str(&format!(
                "| {} | `{}` | `{}` | **{}** |\n",
                c.name, c.target, c.actual, icon
            ));
        }
    }

    md
}

fn format_us(us: u64) -> String {
    if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{:.2} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.3} s", us as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCategory;
    use crate::metrics::LatencyStats;
    use std::collections::HashMap;

    fn latency(value: u64) -> LatencyStats {
        LatencyStats {
            count: 1,
            min_us: value,
            mean_us: value as f64,
            max_us: value,
            std_dev_us: 0.0,
            p50_us: value,
            p75_us: value,
            p90_us: value,
            p95_us: value,
            p99_us: value,
            p999_us: value,
            p9999_us: value,
        }
    }

    fn sample_metrics() -> AggregatedMetrics {
        AggregatedMetrics {
            total_connections_attempted: 10,
            total_connections_established: 10,
            total_connections_failed: 0,
            total_reconnects: 0,
            total_messages_sent: 100,
            total_messages_recv: 100,
            total_bytes_sent: 1_000,
            total_bytes_recv: 1_000,
            elapsed: std::time::Duration::from_secs(1),
            throughput_msg_per_sec: 100.0,
            throughput_bytes_per_sec: 2_000.0,
            error_rate: 0.0,
            handshake_latency: latency(500),
            message_rtt: latency(250),
            error_breakdown: HashMap::new(),
        }
    }

    #[test]
    fn latency_slo_fails_when_no_rtt_was_measured() {
        let mut metrics = sample_metrics();
        metrics.message_rtt = LatencyStats::from_histogram(
            &hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
        );
        let slo = SloThresholds {
            max_p50: Some(Duration::from_secs(1)),
            max_p99: Some(Duration::from_secs(1)),
            ..Default::default()
        };

        let evaluation = evaluate_slos(&slo, &metrics);

        assert!(!evaluation.passed);
        assert_eq!(evaluation.checks.len(), 2);
        assert!(evaluation.checks.iter().all(|check| !check.passed));
        assert!(
            evaluation
                .checks
                .iter()
                .all(|check| check.actual == "no RTT samples")
        );
    }

    #[test]
    fn latency_slo_compares_measured_percentiles() {
        let metrics = sample_metrics();
        let within = SloThresholds {
            max_p99: Some(Duration::from_micros(250)),
            ..Default::default()
        };
        let breached = SloThresholds {
            max_p99: Some(Duration::from_micros(249)),
            ..Default::default()
        };

        assert!(evaluate_slos(&within, &metrics).passed);
        assert!(!evaluate_slos(&breached, &metrics).passed);
    }

    #[test]
    fn non_latency_slos_still_evaluate_without_rtt_samples() {
        let mut metrics = sample_metrics();
        metrics.message_rtt = LatencyStats::from_histogram(
            &hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
        );
        let slo = SloThresholds {
            max_error_rate: Some(0.01),
            min_throughput: Some(50.0),
            ..Default::default()
        };

        assert!(evaluate_slos(&slo, &metrics).passed);
    }

    #[test]
    fn higher_throughput_is_an_improvement_and_lower_is_a_regression() {
        let baseline = sample_metrics();

        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        assert!(comparison.regressions.is_empty());
        assert!(
            comparison
                .improvements
                .iter()
                .any(|item| item.metric == "throughput_msg_per_sec")
        );

        let mut slower = sample_metrics();
        slower.throughput_msg_per_sec = 50.0;
        let comparison = compare_metrics(&baseline, &slower, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "throughput_msg_per_sec")
        );
        assert!(
            comparison
                .improvements
                .iter()
                .all(|item| item.metric != "throughput_msg_per_sec")
        );
    }

    #[test]
    fn higher_latency_is_a_regression_and_lower_is_an_improvement() {
        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);

        let mut faster = sample_metrics();
        faster.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        assert!(
            comparison
                .improvements
                .iter()
                .any(|item| item.metric == "message_rtt.p50_us")
        );

        let mut slower = sample_metrics();
        slower.message_rtt = latency(2_000);
        let comparison = compare_metrics(&baseline, &slower, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "message_rtt.p50_us")
        );
    }

    #[test]
    fn new_errors_from_a_zero_baseline_are_regressions() {
        let baseline = sample_metrics();

        let mut current = sample_metrics();
        current.total_connections_failed = 2;
        current.error_rate = 0.2;
        current
            .error_breakdown
            .insert(ErrorCategory::DnsResolution, 2);

        let comparison = compare_metrics(&baseline, &current, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "total_connections_failed")
        );
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "error_rate")
        );
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "errors.dns_resolution")
        );
    }

    #[test]
    fn parses_schema_envelopes_and_bare_metrics() {
        let metrics = sample_metrics();
        let envelope = serde_json::json!({
            "schema_version": "1.0.0",
            "metrics": metrics,
        });

        let parsed = parse_report_metrics(&envelope.to_string()).expect("envelope must parse");
        assert_eq!(parsed.total_messages_sent, metrics.total_messages_sent);

        let parsed = parse_report_metrics(&serde_json::to_string(&metrics).expect("serialize"))
            .expect("bare metrics must parse");
        assert_eq!(
            parsed.throughput_msg_per_sec,
            metrics.throughput_msg_per_sec
        );
    }

    fn rows_for_metric(rendered: &str, metric: &str) -> Vec<String> {
        rendered
            .lines()
            .filter(|line| line.contains(metric))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn comparison_reports_print_signed_improvement_change() {
        let baseline = sample_metrics();
        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);

        for format in [OutputFormat::Text, OutputFormat::Markdown] {
            let rendered = render_comparison_report(&comparison, format, 10.0);
            let rows = rows_for_metric(&rendered, "throughput_msg_per_sec");
            assert_eq!(rows.len(), 1, "{rendered}");
            assert!(rows[0].contains("+50.00%"), "{}", rows[0]);
        }

        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);
        let mut lower_latency = sample_metrics();
        lower_latency.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &lower_latency, 10.0);

        for format in [OutputFormat::Text, OutputFormat::Markdown] {
            let rendered = render_comparison_report(&comparison, format, 10.0);
            let rows = rows_for_metric(&rendered, "message_rtt.p50_us");
            assert_eq!(rows.len(), 1, "{rendered}");
            assert!(rows[0].contains("-50.00%"), "{}", rows[0]);
            assert!(!rows[0].contains("+50.00%"), "{}", rows[0]);
        }
    }

    #[test]
    fn improvement_pct_change_keeps_its_sign() {
        let baseline = sample_metrics();
        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        let improvement = comparison
            .improvements
            .iter()
            .find(|item| item.metric == "throughput_msg_per_sec")
            .unwrap();
        assert_eq!(improvement.pct_change, 50.0);

        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);
        let mut lower_latency = sample_metrics();
        lower_latency.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &lower_latency, 10.0);
        let improvement = comparison
            .improvements
            .iter()
            .find(|item| item.metric == "message_rtt.p50_us")
            .unwrap();
        assert_eq!(improvement.pct_change, -50.0);
    }

    #[test]
    fn comparison_json_carries_schema_version() {
        let comparison = compare_metrics(&sample_metrics(), &sample_metrics(), 10.0);
        let rendered = render_comparison_report(&comparison, OutputFormat::Json, 10.0);
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("comparison JSON must parse");
        assert_eq!(parsed["schema_version"], "1.1.0");
    }
}
