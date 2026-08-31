//! Multi-format report generation (Terminal Text, Schema v1.0 JSON, Markdown) and SLO gating.

use crate::cli::OutputFormat;
use crate::config::{LoadTestConfig, SloThresholds};
use crate::metrics::AggregatedMetrics;
use colored::Colorize;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use std::fs;

/// Result of evaluating configured SLO thresholds against aggregated test metrics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SloEvaluation {
    pub passed: bool,
    pub checks: Vec<SloCheck>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SloCheck {
    pub name: String,
    pub target: String,
    pub actual: String,
    pub passed: bool,
}

/// Evaluates configured SLO criteria against aggregated test metrics.
pub fn evaluate_slos(slo: &SloThresholds, metrics: &AggregatedMetrics) -> SloEvaluation {
    let mut checks = Vec::new();
    let mut all_passed = true;

    if let Some(target_p50) = slo.max_p50 {
        let actual = metrics.message_rtt.p50_duration();
        let passed = actual <= target_p50;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Latency p50".into(),
            target: format!("<= {:?}", target_p50),
            actual: format!("{:?}", actual),
            passed,
        });
    }

    if let Some(target_p95) = slo.max_p95 {
        let actual = metrics.message_rtt.p95_duration();
        let passed = actual <= target_p95;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Latency p95".into(),
            target: format!("<= {:?}", target_p95),
            actual: format!("{:?}", actual),
            passed,
        });
    }

    if let Some(target_p99) = slo.max_p99 {
        let actual = metrics.message_rtt.p99_duration();
        let passed = actual <= target_p99;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Latency p99".into(),
            target: format!("<= {:?}", target_p99),
            actual: format!("{:?}", actual),
            passed,
        });
    }

    if let Some(target_p999) = slo.max_p999 {
        let actual = metrics.message_rtt.p999_duration();
        let passed = actual <= target_p999;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Latency p99.9".into(),
            target: format!("<= {:?}", target_p999),
            actual: format!("{:?}", actual),
            passed,
        });
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
