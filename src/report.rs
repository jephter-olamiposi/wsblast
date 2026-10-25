//! SLO evaluation, report rendering (terminal text, JSON schema 1.0,
//! Markdown), and the baseline comparison behind `--compare`.

use crate::cli::OutputFormat;
use crate::config::{LoadTestConfig, SloThresholds};
use crate::metrics::{AggregatedMetrics, CaptureSummary, DistributedSummary, LatencyStats};
use colored::Colorize;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use serde::{Deserialize, Serialize};
#[cfg(feature = "compare")]
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

/// Outcome of [`evaluate_slos`]: one check per configured threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SloEvaluation {
    /// True when every check passed, including when no threshold was set.
    pub passed: bool,
    /// Checks in a fixed order: p50, p95, p99, p99.9, error rate, throughput.
    pub checks: Vec<SloCheck>,
}

/// One evaluated SLO threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SloCheck {
    /// Human-readable name, for example `Latency p99`.
    pub name: String,
    /// The threshold, formatted for display (`<= 25ms`).
    pub target: String,
    /// The measured value, formatted for display, or `no RTT samples`.
    pub actual: String,
    /// Whether the measured value satisfies the threshold.
    pub passed: bool,
}

/// Endpoint one scenario phase ran against, listed in scenario reports.
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PhaseTarget {
    /// Phase name.
    pub phase: String,
    /// Endpoint the phase connected to.
    pub target_url: url::Url,
}

/// Baseline comparison for regression detection.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BaselineComparison {
    pub(crate) baseline: AggregatedMetrics,
    pub(crate) current: AggregatedMetrics,
    pub(crate) regressions: Vec<Regression>,
    pub(crate) improvements: Vec<Improvement>,
}

/// `pct_change` is the signed relative change from baseline to current, in percent.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Regression {
    pub(crate) metric: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) pct_change: f64,
}

/// `pct_change` is the signed relative change from baseline to current, in
/// percent: positive for a throughput gain, negative for a latency drop.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Improvement {
    pub(crate) metric: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) pct_change: f64,
}

/// Report payloads are either the schema envelope (`{ "metrics": ... }`) or a
/// bare aggregated-metrics object produced by earlier versions.
#[cfg(feature = "compare")]
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
#[cfg(feature = "compare")]
pub(crate) fn parse_report_metrics(json: &str) -> serde_json::Result<AggregatedMetrics> {
    match serde_json::from_str::<ReportDocument>(json)? {
        ReportDocument::Envelope { metrics } | ReportDocument::Bare(metrics) => Ok(metrics),
    }
}

#[cfg(feature = "compare")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
    Neutral,
}

/// Classifies a metric by the direction that constitutes an improvement.
#[cfg(feature = "compare")]
fn metric_direction(metric: &str) -> MetricDirection {
    match metric {
        // Input sizing, not an outcome: comparing it would flag intentional reconfiguration.
        "total_connections_attempted" => MetricDirection::Neutral,
        "total_connections_failed" | "error_rate" => MetricDirection::LowerIsBetter,
        _ if metric.starts_with("errors.") => MetricDirection::LowerIsBetter,
        _ if metric.starts_with("handshake_latency.")
            || metric.starts_with("message_rtt.")
            || metric.starts_with("response_time.") =>
        {
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
#[cfg(feature = "compare")]
pub(crate) fn compare_metrics(
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

        // Response time exists only for paced runs and in reports from 0.3
        // on; comparing it against a report without it would read as a
        // regression from zero.
        if metric_name.starts_with("response_time.")
            && !(baseline_map.contains_key(metric_name) && current_map.contains_key(metric_name))
        {
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

#[cfg(feature = "compare")]
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
    if metrics.response_time.count > 0 {
        let rt = &metrics.response_time;
        map.insert("response_time.p50_us".into(), rt.p50_us as f64);
        map.insert("response_time.p95_us".into(), rt.p95_us as f64);
        map.insert("response_time.p99_us".into(), rt.p99_us as f64);
        map.insert("response_time.p999_us".into(), rt.p999_us as f64);
    }

    map
}

/// Renders a comparison report in the specified format.
#[cfg(feature = "compare")]
pub(crate) fn render_comparison_report(
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

#[cfg(feature = "compare")]
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

#[cfg(feature = "compare")]
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

#[cfg(feature = "compare")]
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

/// Checks every threshold set in `slo` against `metrics`.
///
/// A latency threshold fails when no round trip was measured, because an
/// empty histogram is no evidence that the budget was met. Thresholds are
/// inclusive: a p99 equal to `max_p99` passes.
///
/// # Examples
///
/// Nothing listens on port 1, so this run measures no round trip and the
/// latency check fails along with the error budget. The short connect
/// timeout makes the attempt fail inside the run on every platform; Windows
/// otherwise retries a refused connection for about two seconds.
///
/// ```
/// use std::time::Duration;
/// use wsblast::{LoadTestConfig, Runner, SloThresholds, evaluate_slos};
///
/// # #[tokio::main]
/// # async fn main() -> wsblast::Result<()> {
/// let config = LoadTestConfig::builder("ws://127.0.0.1:1")
///     .connections(1)
///     .duration(Duration::from_millis(200))
///     .connect_timeout(Duration::from_millis(100))
///     .build()?;
/// let metrics = Runner::new(config).with_signal_handling(false).run().await?;
///
/// let slo = SloThresholds::new()
///     .with_max_p99(Duration::from_millis(50))
///     .with_max_error_rate(0.01);
/// let evaluation = evaluate_slos(&slo, &metrics);
///
/// assert!(!evaluation.passed);
/// assert_eq!(evaluation.checks[0].name, "Latency p99");
/// assert_eq!(evaluation.checks[0].actual, "no RTT samples");
/// assert!(!evaluation.checks[1].passed);
/// # Ok(())
/// # }
/// ```
pub fn evaluate_slos(slo: &SloThresholds, metrics: &AggregatedMetrics) -> SloEvaluation {
    let mut checks = Vec::new();
    let mut all_passed = true;

    // A paced run gates on response time, which counts the time a request
    // waited behind a slow reply; RTT alone would hide that wait.
    let (rtt, names) = if metrics.response_time.count > 0 {
        (
            &metrics.response_time,
            [
                "Response time p50",
                "Response time p95",
                "Response time p99",
                "Response time p99.9",
            ],
        )
    } else {
        (
            &metrics.message_rtt,
            ["Latency p50", "Latency p95", "Latency p99", "Latency p99.9"],
        )
    };
    let latency_targets = [
        (names[0], slo.max_p50, rtt.p50_duration()),
        (names[1], slo.max_p95, rtt.p95_duration()),
        (names[2], slo.max_p99, rtt.p99_duration()),
        (names[3], slo.max_p999, rtt.p999_duration()),
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
/// The most rows the text and Markdown reports show for the time series.
const MAX_TIME_WINDOWS: usize = 10;

/// A group of consecutive time series points, as one report row.
struct TimeWindow {
    label: String,
    throughput: f64,
    errors: u64,
    /// The highest p99 of any interval in the window. Percentiles cannot be
    /// combined, so this is the worst interval, not the window's p99.
    worst_p99: Option<u64>,
}

/// Groups the run's time series into at most [`MAX_TIME_WINDOWS`] rows. A
/// series with fewer than two points adds nothing to the summary tables, so
/// it gives no rows.
fn time_windows(metrics: &AggregatedMetrics) -> Vec<TimeWindow> {
    let points = &metrics.time_series.points;
    if points.len() < 2 {
        return Vec::new();
    }
    let per_window = points.len().div_ceil(MAX_TIME_WINDOWS);
    points
        .chunks(per_window)
        .map(|chunk| {
            let start = chunk[0].start_secs;
            let last = &chunk[chunk.len() - 1];
            let end = last.start_secs + last.duration_secs;
            let secs: f64 = chunk.iter().map(|p| p.duration_secs).sum();
            let messages: u64 = chunk
                .iter()
                .map(|p| p.messages_sent.max(p.messages_recv))
                .sum();
            TimeWindow {
                label: format!("{} to {}", format_secs(start), format_secs(end)),
                throughput: messages as f64 / secs.max(0.000_1),
                errors: chunk.iter().map(|p| p.errors).sum(),
                worst_p99: chunk
                    .iter()
                    .filter(|p| p.latency.count > 0)
                    .map(|p| p.latency.p99_us)
                    .max(),
            }
        })
        .collect()
}

/// Describes a staged load profile as `DURATION:TARGET` pairs, the syntax of
/// `--stages`.
fn stages_label(config: &LoadTestConfig) -> String {
    config
        .stages
        .iter()
        .map(|stage| format!("{:?}:{}", stage.duration, stage.target))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Formats seconds since the run started, with one decimal only when the
/// value is not a whole number of seconds.
fn format_secs(secs: f64) -> String {
    if (secs - secs.round()).abs() < 0.05 {
        format!("{:.0}s", secs.round())
    } else {
        format!("{secs:.1}s")
    }
}

fn latency_basis_label(metrics: &AggregatedMetrics) -> &'static str {
    match metrics.time_series.latency_basis.as_str() {
        "response_time" => "Response Time",
        "mixed" => "Latency",
        _ => "RTT",
    }
}

/// A report row label and the percentile it reads from a distribution.
type PercentileRow = (&'static str, fn(&LatencyStats) -> u64);

/// The latency distributions a report shows, by column name. Response time
/// appears only when the run recorded it.
fn latency_columns(metrics: &AggregatedMetrics) -> Vec<(&'static str, &LatencyStats)> {
    let mut columns = vec![
        ("Handshake Latency", &metrics.handshake_latency),
        ("Message RTT Latency", &metrics.message_rtt),
    ];
    if metrics.response_time.count > 0 {
        columns.push(("Response Time (from schedule)", &metrics.response_time));
    }
    columns
}

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

/// Renders the run report in the configuration's
/// [`output_format`](LoadTestConfig::output_format).
///
/// This is the report the CLI prints: terminal tables for
/// [`OutputFormat::Text`], the schema 1.0 document for [`OutputFormat::Json`],
/// and a GitHub Flavored Markdown summary for [`OutputFormat::Markdown`]. The
/// JSON schema is published in the repository as
/// [`schema/report-v1.json`](https://github.com/jephter-olamiposi/wsblast/blob/main/schema/report-v1.json).
pub fn render_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    render_scenario_report(config, metrics, slo, &[])
}

/// Formats a report that also lists the target of every executed scenario
/// phase. With no phases it is identical to [`render_report`].
pub(crate) fn render_scenario_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
) -> String {
    match config.output_format {
        OutputFormat::Text => render_text_report(config, metrics, slo, phase_targets),
        OutputFormat::Json => render_json_report(config, metrics, slo, phase_targets),
        OutputFormat::Markdown => render_markdown_report(config, metrics, slo, phase_targets),
    }
}

/// Prints the report to stdout and writes it to the configured output path,
/// listing the target of every executed scenario phase.
///
/// # Errors
///
/// Returns [`std::io::Error`] if writing to the specified `output_path` fails.
pub(crate) fn emit_scenario_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
) -> std::io::Result<()> {
    let output = render_scenario_report(config, metrics, slo, phase_targets);
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
    phase_targets: &[PhaseTarget],
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
    if phase_targets.is_empty() {
        meta_table.add_row(vec!["Target URL", config.target_url.as_str()]);
    }
    for target in phase_targets {
        meta_table.add_row(vec![
            format!("Target URL (phase '{}')", target.phase),
            target.target_url.to_string(),
        ]);
    }
    meta_table.add_row(vec![
        "Connections (Concurrency)",
        &config.connections.to_string(),
    ]);
    if !config.stages.is_empty() {
        meta_table.add_row(vec!["Load Stages", &stages_label(config)]);
    }
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
    if let Some(distributed) = &metrics.distributed {
        meta_table.add_row(vec![
            "Agents",
            &agents_label(distributed, |a| a.to_string()),
        ]);
    }
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
    if metrics.total_connections_interrupted > 0 {
        tput_table.add_row(vec![
            "Connections Interrupted (still connecting at end of run)",
            &metrics.total_connections_interrupted.to_string(),
        ]);
    }
    if metrics.total_connections_not_started > 0 {
        tput_table.add_row(vec![
            "Connections Not Started (ramp cut short by --duration)",
            &metrics.total_connections_not_started.to_string(),
        ]);
    }
    tput_table.add_row(vec!["Reconnects", &metrics.total_reconnects.to_string()]);
    if metrics.total_unsolicited_frames > 0 {
        tput_table.add_row(vec![
            "Unsolicited Frames (skipped by --match)",
            &metrics.total_unsolicited_frames.to_string(),
        ]);
    }
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
    if let Some(capture) = &metrics.capture {
        out.push_str(&format!("{}\n\n", capture_line(capture)));
    }

    let columns = latency_columns(metrics);
    let mut lat_table = Table::new();
    lat_table.load_preset(UTF8_FULL);
    let mut header = vec!["Percentile"];
    header.extend(columns.iter().map(|(name, _)| *name));
    lat_table.set_header(header);
    let rows: [PercentileRow; 6] = [
        ("p50 (Median)", |s| s.p50_us),
        ("p75", |s| s.p75_us),
        ("p90", |s| s.p90_us),
        ("p95", |s| s.p95_us),
        ("p99", |s| s.p99_us),
        ("p99.9", |s| s.p999_us),
    ];
    for (label, value) in rows {
        let mut row = vec![label.to_string()];
        row.extend(columns.iter().map(|(_, stats)| format_us(value(stats))));
        lat_table.add_row(row);
    }
    let mut row = vec!["Min / Mean / Max".to_string()];
    row.extend(columns.iter().map(|(_, stats)| {
        format!(
            "{} / {} / {}",
            format_us(stats.min_us),
            format_us(stats.mean_us as u64),
            format_us(stats.max_us)
        )
    }));
    lat_table.add_row(row);
    out.push_str(&lat_table.to_string());
    out.push_str("\n\n");

    let windows = time_windows(metrics);
    if !windows.is_empty() {
        let mut ts_table = Table::new();
        ts_table.load_preset(UTF8_FULL);
        ts_table.set_header(vec![
            "Time Window".to_string(),
            "Throughput (msg/s)".to_string(),
            "Errors".to_string(),
            format!("Worst {} p99", latency_basis_label(metrics)),
        ]);
        for w in &windows {
            ts_table.add_row(vec![
                w.label.clone(),
                format!("{:.1}", w.throughput),
                w.errors.to_string(),
                w.worst_p99.map_or_else(|| "-".to_string(), format_us),
            ]);
        }
        out.push_str(&ts_table.to_string());
        out.push_str("\n\n");
    }

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
    /// Every executed scenario phase in run order; omitted outside scenarios.
    #[serde(skip_serializing_if = "<[PhaseTarget]>::is_empty")]
    phase_targets: &'a [PhaseTarget],
    metrics: &'a AggregatedMetrics,
    slo: &'a SloEvaluation,
}

fn render_json_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
) -> String {
    let mode_str = format!("{:?}", config.mode);
    let report = JsonReport {
        schema_version: "1.0.0",
        target_url: config.target_url.as_str(),
        connections: config.connections,
        duration_secs: metrics.elapsed.as_secs_f64(),
        mode: &mode_str,
        phase_targets,
        metrics,
        slo,
    };

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

fn render_markdown_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
) -> String {
    let mut md = String::new();
    md.push_str("# `wsblast` WebSocket Benchmark Report\n\n");
    if phase_targets.is_empty() {
        md.push_str(&format!("- **Target:** `{}`\n", config.target_url));
    } else {
        md.push_str("- **Targets:**\n");
        for target in phase_targets {
            md.push_str(&format!(
                "  - phase `{}`: `{}`\n",
                target.phase, target.target_url
            ));
        }
    }
    md.push_str(&format!(
        "- **Concurrency:** `{}` workers\n",
        config.connections
    ));
    if !config.stages.is_empty() {
        md.push_str(&format!("- **Load Stages:** `{}`\n", stages_label(config)));
    }
    if let Some(distributed) = &metrics.distributed {
        md.push_str(&format!(
            "- **Agents:** {}\n",
            agents_label(distributed, |a| format!("`{a}`"))
        ));
    }
    md.push_str(&format!(
        "- **Connections:** `{}` established / `{}` failed / `{}` reconnects\n",
        metrics.total_connections_established,
        metrics.total_connections_failed,
        metrics.total_reconnects
    ));
    if metrics.total_connections_interrupted > 0 {
        md.push_str(&format!(
            "- **Connections Interrupted:** `{}` (still connecting at end of run)\n",
            metrics.total_connections_interrupted
        ));
    }
    if metrics.total_connections_not_started > 0 {
        md.push_str(&format!(
            "- **Connections Not Started:** `{}` (ramp cut short by `--duration`)\n",
            metrics.total_connections_not_started
        ));
    }
    if metrics.total_unsolicited_frames > 0 {
        md.push_str(&format!(
            "- **Unsolicited Frames:** `{}` skipped by `--match`\n",
            metrics.total_unsolicited_frames
        ));
    }
    md.push_str(&format!("- **Duration:** `{:.2?}`\n", metrics.elapsed));
    md.push_str(&format!(
        "- **Throughput:** `{:.2}` msg/sec\n",
        metrics.throughput_msg_per_sec
    ));
    md.push_str(&format!(
        "- **Error Rate:** `{:.2}%`\n",
        metrics.error_rate * 100.0
    ));
    if let Some(capture) = &metrics.capture {
        md.push_str(&format!(
            "- **Capture:** {} frames written, {} dropped (`{}`)\n",
            capture.frames_written, capture.frames_dropped, capture.path
        ));
    }
    md.push('\n');

    md.push_str("### Latency Distribution\n\n");
    let columns = latency_columns(metrics);
    md.push_str("| Percentile |");
    for (name, _) in &columns {
        md.push_str(&format!(" {name} |"));
    }
    md.push_str("\n| :--- |");
    md.push_str(&" :--- |".repeat(columns.len()));
    md.push('\n');
    let rows: [PercentileRow; 6] = [
        ("p50", |s| s.p50_us),
        ("p90", |s| s.p90_us),
        ("p95", |s| s.p95_us),
        ("p99", |s| s.p99_us),
        ("p99.9", |s| s.p999_us),
        ("Max", |s| s.max_us),
    ];
    for (label, value) in rows {
        md.push_str(&format!("| **{label}** |"));
        for (_, stats) in &columns {
            md.push_str(&format!(" {} |", format_us(value(stats))));
        }
        md.push('\n');
    }
    md.push('\n');

    let windows = time_windows(metrics);
    if !windows.is_empty() {
        md.push_str("### Over Time\n\n");
        md.push_str(&format!(
            "| Time Window | Throughput (msg/s) | Errors | Worst {} p99 |\n",
            latency_basis_label(metrics)
        ));
        md.push_str("| :--- | :--- | :--- | :--- |\n");
        for w in &windows {
            md.push_str(&format!(
                "| {} | {:.1} | {} | {} |\n",
                w.label,
                w.throughput,
                w.errors,
                w.worst_p99.map_or_else(|| "-".to_string(), format_us)
            ));
        }
        md.push('\n');
    }

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

/// "N" or "N (lost: a, b)" for a distributed run, each lost agent rendered
/// by `name`.
fn agents_label(distributed: &DistributedSummary, name: impl Fn(&str) -> String) -> String {
    let count = distributed.agents.len();
    if distributed.lost_agents.is_empty() {
        return count.to_string();
    }
    let lost: Vec<String> = distributed.lost_agents.iter().map(|a| name(a)).collect();
    format!("{count} (lost: {})", lost.join(", "))
}

/// The capture totals line of the text report.
fn capture_line(capture: &CaptureSummary) -> String {
    format!(
        "Capture: {} frames written, {} dropped ({})",
        capture.frames_written, capture.frames_dropped, capture.path
    )
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
    #[cfg(feature = "compare")]
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
            total_connections_interrupted: 0,
            total_connections_not_started: 0,
            total_reconnects: 0,
            total_unsolicited_frames: 0,
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
            response_time: LatencyStats::default(),
            time_series: crate::timeseries::TimeSeries::default(),
            error_breakdown: HashMap::new(),
            capture: None,
            distributed: None,
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

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
    fn rows_for_metric(rendered: &str, metric: &str) -> Vec<String> {
        rendered
            .lines()
            .filter(|line| line.contains(metric))
            .map(str::to_owned)
            .collect()
    }

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
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

    #[cfg(feature = "compare")]
    #[test]
    fn comparison_json_carries_schema_version() {
        let comparison = compare_metrics(&sample_metrics(), &sample_metrics(), 10.0);
        let rendered = render_comparison_report(&comparison, OutputFormat::Json, 10.0);
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("comparison JSON must parse");
        assert_eq!(parsed["schema_version"], "1.1.0");
    }

    fn report_config(format: &str) -> LoadTestConfig {
        use clap::Parser;

        let cli =
            crate::cli::Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--format", format]);
        LoadTestConfig::from_cli(cli).expect("valid config")
    }

    #[test]
    fn capture_totals_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown", "json"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("apture"), "{format}: {rendered}");
        }

        metrics.capture = Some(CaptureSummary {
            path: "run.jsonl".into(),
            frames_written: 120,
            frames_dropped: 3,
            limit: 100_000,
        });
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(
            text.contains("Capture: 120 frames written, 3 dropped (run.jsonl)"),
            "{text}"
        );
        let markdown = render_report(&report_config("markdown"), &metrics, &slo);
        assert!(
            markdown.contains("- **Capture:** 120 frames written, 3 dropped (`run.jsonl`)"),
            "{markdown}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_report(&report_config("json"), &metrics, &slo)).unwrap();
        assert_eq!(json["metrics"]["capture"]["frames_dropped"], 3);

        let parsed: AggregatedMetrics =
            serde_json::from_value(json["metrics"].clone()).expect("report parses");
        assert_eq!(parsed.capture, metrics.capture);
    }

    #[test]
    fn distributed_runs_list_their_agents_and_lost_agents() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown", "json"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Agents"), "{format}: {rendered}");
            assert!(!rendered.contains("distributed"), "{format}");
        }
        metrics.distributed = Some(DistributedSummary {
            agents: vec![
                "10.0.0.1:7700".into(),
                "10.0.0.2:7700".into(),
                "10.0.0.3:7700".into(),
            ],
            lost_agents: vec!["10.0.0.2:7700".into()],
        });
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(text.contains("3 (lost: 10.0.0.2:7700)"), "{text}");
        let markdown = render_report(&report_config("markdown"), &metrics, &slo);
        assert!(
            markdown.contains("- **Agents:** 3 (lost: `10.0.0.2:7700`)"),
            "{markdown}"
        );
        let json = render_report(&report_config("json"), &metrics, &slo);
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            json["metrics"]["distributed"]["lost_agents"][0],
            "10.0.0.2:7700"
        );
        assert_eq!(
            json["metrics"]["distributed"]["agents"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let parsed: AggregatedMetrics = serde_json::from_value(json["metrics"].clone()).unwrap();
        assert_eq!(parsed.distributed, metrics.distributed);

        metrics.distributed.as_mut().unwrap().lost_agents.clear();
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(!text.contains("lost"), "{text}");
    }

    #[test]
    fn unsolicited_frames_are_reported_only_when_seen() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Unsolicited"), "{format}: {rendered}");
        }

        metrics.total_unsolicited_frames = 42;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Unsolicited"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains("42"), "{format}: {line}");
        }
    }

    #[test]
    fn not_started_connections_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Not Started"), "{format}: {rendered}");
        }

        metrics.total_connections_established = 3;
        metrics.total_connections_not_started = 7;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Not Started"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains('7'), "{format}: {line}");
        }
    }

    #[test]
    fn paced_runs_gate_on_response_time_and_show_it() {
        let slo = SloThresholds::new().with_max_p99(Duration::from_millis(1));
        let mut metrics = sample_metrics();

        let unpaced = evaluate_slos(&slo, &metrics);
        assert_eq!(unpaced.checks[0].name, "Latency p99");
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &unpaced);
            assert!(!rendered.contains("Response Time"), "{format}: {rendered}");
        }

        // RTT is within the 1 ms budget but response time is not.
        metrics.response_time = latency(5_000);
        let paced = evaluate_slos(&slo, &metrics);
        assert_eq!(paced.checks[0].name, "Response time p99");
        assert!(!paced.passed);
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &paced);
            assert!(rendered.contains("Response Time"), "{format}: {rendered}");
        }
    }

    #[cfg(feature = "compare")]
    #[test]
    fn response_time_is_compared_only_when_both_reports_have_it() {
        let baseline = sample_metrics();
        let mut current = sample_metrics();
        current.response_time = latency(9_000);
        let comparison = compare_metrics(&baseline, &current, 5.0);
        assert!(
            comparison
                .regressions
                .iter()
                .all(|item| !item.metric.starts_with("response_time.")),
            "{:?}",
            comparison.regressions
        );

        let mut baseline = current.clone();
        baseline.response_time = latency(1_000);
        let comparison = compare_metrics(&baseline, &current, 5.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "response_time.p99_us"),
            "{:?}",
            comparison.regressions
        );
    }

    #[test]
    fn time_series_is_summarised_in_at_most_ten_rows() {
        use crate::timeseries::{TimeSeries, TimeSeriesPoint};
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Time Window"), "{format}: {rendered}");
        }

        let points: Vec<TimeSeriesPoint> = (0..25)
            .map(|i| TimeSeriesPoint {
                start_secs: f64::from(i),
                duration_secs: 1.0,
                messages_sent: 100,
                latency: crate::timeseries::IntervalLatency {
                    count: 100,
                    p99_us: if i == 12 { 900_000 } else { 1_000 },
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        metrics.time_series = TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        };

        let rendered = render_report(&report_config("markdown"), &metrics, &slo);
        let rows: Vec<&str> = rendered
            .lines()
            .filter(|line| line.contains("s to "))
            .collect();
        assert_eq!(rows.len(), 9, "{rendered}");
        assert!(
            rows[0].starts_with("| 0s to 3s | 100.0 | 0 |"),
            "{rendered}"
        );
        assert!(rows[4].contains("900.00 ms"), "{rendered}");
        assert!(rows[8].starts_with("| 24s to 25s |"), "{rendered}");
        assert!(rendered.contains("Worst RTT p99"), "{rendered}");
    }

    #[test]
    fn interrupted_connections_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Interrupted"), "{format}: {rendered}");
        }

        metrics.total_connections_established = 6;
        metrics.total_connections_interrupted = 4;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Interrupted"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains('4'), "{format}: {line}");
        }
    }

    #[test]
    fn scenario_reports_list_every_phase_target() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let phases = [
            PhaseTarget {
                phase: "warmup".to_string(),
                target_url: url::Url::parse("ws://first.test/ws").expect("url"),
            },
            PhaseTarget {
                phase: "spike".to_string(),
                target_url: url::Url::parse("wss://second.test/ws").expect("url"),
            },
        ];

        for format in ["text", "markdown"] {
            let rendered =
                render_scenario_report(&report_config(format), &sample_metrics(), &slo, &phases);
            for phase in &phases {
                let line = rendered
                    .lines()
                    .find(|line| line.contains(phase.target_url.as_str()))
                    .unwrap_or_else(|| panic!("{format}: {rendered}"));
                assert!(line.contains(&phase.phase), "{format}: {line}");
            }
        }

        let rendered =
            render_scenario_report(&report_config("json"), &sample_metrics(), &slo, &phases);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(
            parsed["phase_targets"],
            serde_json::json!([
                {"phase": "warmup", "target_url": "ws://first.test/ws"},
                {"phase": "spike", "target_url": "wss://second.test/ws"},
            ])
        );

        let single = render_report(&report_config("json"), &sample_metrics(), &slo);
        let parsed: serde_json::Value = serde_json::from_str(&single).expect("json");
        assert!(parsed.get("phase_targets").is_none(), "{single}");
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_unsolicited_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_unsolicited_frames");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_unsolicited_frames, 0);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_not_started_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_connections_not_started");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_connections_not_started, 0);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_interrupted_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_connections_interrupted");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_connections_interrupted, 0);
    }
}
