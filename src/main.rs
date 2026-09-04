//! wsblast - High-performance WebSocket load testing CLI with percentile latency & CI gating.

use clap::Parser;
use colored::Colorize;
use std::sync::Arc;
use wsblast::cli::Cli;
use wsblast::config::LoadTestConfig;
use wsblast::error::Result;
use wsblast::report;
use wsblast::runner::Runner;
use wsblast::tui::TuiApp;

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Err(err) = run_app().await {
        eprintln!("\n{} {}", colored::Colorize::bold("Error:").red(), err);
        std::process::exit(1);
    }
}

async fn run_app() -> Result<()> {
    let cli = Cli::parse();
    let config = LoadTestConfig::from_cli(cli)?;
    let runner = Runner::new(config.clone());

    let metrics = if config.tui {
        let config_arc = Arc::new(config.clone());
        let live_metrics = runner.live_metrics();
        let cancel_token = runner.cancel_token();

        let mut tui_app = TuiApp::new(config_arc, live_metrics, cancel_token);

        let runner_handle = tokio::spawn(async move { runner.run().await });

        if let Err(e) = tui_app.run().await {
            eprintln!("TUI error: {e}");
        }

        runner_handle
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
    } else {
        runner.run().await
    };

    let slo_result = report::evaluate_slos(&config.slo, &metrics);

    report::emit_report(&config, &metrics, &slo_result)
        .map_err(wsblast::error::WsBlastError::Io)?;

    if !slo_result.passed {
        std::process::exit(2);
    }

    if metrics.total_connections_attempted > 0 && metrics.total_connections_established == 0 {
        eprintln!(
            "{} All connection attempts failed. Target may be unreachable.",
            colored::Colorize::bold("Failure:").red()
        );
        std::process::exit(1);
    }

    Ok(())
}
