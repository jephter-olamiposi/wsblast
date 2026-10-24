//! wsblast - High-performance WebSocket load testing CLI with percentile latency & CI gating.

use clap::Parser;
use colored::Colorize;
use std::sync::Arc;
use wsblast::cli::Cli;
use wsblast::config::LoadTestConfig;
#[cfg(feature = "scenario")]
use wsblast::config::ScenarioConfig;
use wsblast::error::Result;
use wsblast::report;
use wsblast::runner::Runner;
#[cfg(feature = "scenario")]
use wsblast::runner::ScenarioExecutor;
use wsblast::tui::TuiApp;

/// Resolves configured authentication once and merges it into handshake headers.
#[cfg(feature = "auth")]
fn apply_auth(config: &mut LoadTestConfig) -> Result<()> {
    if let Some(auth) = config.auth.take() {
        wsblast::auth::apply_auth_header(&mut config.headers, &auth)
            .map_err(wsblast::error::WsBlastError::Worker)?;
    }
    Ok(())
}

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

    // Handle compare command first (doesn't run load test)
    #[cfg(feature = "compare")]
    if !cli.compare.is_empty() {
        if cli.compare.len() != 2 {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::InvalidThreshold(
                    "Compare requires exactly 2 files: --compare <baseline.json> <current.json>"
                        .into(),
                ),
            ));
        }

        let baseline_path = &cli.compare[0];
        let current_path = &cli.compare[1];

        let baseline_json = std::fs::read_to_string(baseline_path).map_err(|e| {
            wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::PayloadFile {
                path: baseline_path.display().to_string(),
                source: e,
            })
        })?;

        let current_json = std::fs::read_to_string(current_path).map_err(|e| {
            wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::PayloadFile {
                path: current_path.display().to_string(),
                source: e,
            })
        })?;

        if !cli.compare_threshold.is_finite() || cli.compare_threshold < 0.0 {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::InvalidThreshold(format!(
                    "--compare-threshold must be a finite non-negative percentage, received {}",
                    cli.compare_threshold
                )),
            ));
        }

        let baseline = wsblast::report::parse_report_metrics(&baseline_json)
            .map_err(wsblast::error::WsBlastError::Serialization)?;

        let current = wsblast::report::parse_report_metrics(&current_json)
            .map_err(wsblast::error::WsBlastError::Serialization)?;

        let comparison =
            wsblast::report::compare_metrics(&baseline, &current, cli.compare_threshold);
        let report = wsblast::report::render_comparison_report(
            &comparison,
            cli.format,
            cli.compare_threshold,
        );

        println!("{report}");

        if let Some(output_path) = &cli.output_file {
            std::fs::write(output_path, &report).map_err(wsblast::error::WsBlastError::Io)?;
        }

        if cli.fail_on_regression && !comparison.regressions.is_empty() {
            std::process::exit(2);
        }

        return Ok(());
    }

    let scenario_mode = cli.is_scenario_mode();
    let cli_for_config = cli.clone();

    #[cfg(feature = "scenario")]
    let mut scenario: Option<ScenarioConfig> = None;

    let config = if scenario_mode {
        #[cfg(feature = "scenario")]
        {
            let scenario_path = cli_for_config.scenario.as_ref().ok_or_else(|| {
                wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::InvalidScenario(
                    "Scenario mode requires --scenario <FILE>".to_string(),
                ))
            })?;
            let loaded = ScenarioConfig::load(scenario_path)?;
            let built = LoadTestConfig::from_scenario_config(&loaded, &cli_for_config)?;
            scenario = Some(loaded);
            built
        }
        #[cfg(not(feature = "scenario"))]
        {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::InvalidScenario(
                    "Scenario support not compiled in. Build with --features scenario".to_string(),
                ),
            ));
        }
    } else {
        LoadTestConfig::from_cli(cli_for_config)?
    };

    #[cfg(feature = "auth")]
    let mut config = config;

    #[cfg(feature = "auth")]
    apply_auth(&mut config)?;

    let live_metrics = wsblast::metrics::LiveMetrics::new();

    // For scenario mode, run the ScenarioExecutor instead of single Runner
    let metrics = if scenario_mode {
        #[cfg(feature = "scenario")]
        {
            let scenario = scenario.as_ref().ok_or_else(|| {
                wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::InvalidScenario(
                    "Scenario mode requires --scenario <FILE>".to_string(),
                ))
            })?;
            let executor = ScenarioExecutor::new(
                scenario,
                config.clone(),
                cli.phase.as_deref(),
                cli.resolve_url(),
            )?
            .with_live_metrics(std::sync::Arc::clone(&live_metrics));
            let (worker_metrics, total_elapsed, total_connections) = executor.run().await?;
            wsblast::metrics::AggregatedMetrics::merge(
                worker_metrics,
                total_elapsed,
                total_connections,
            )?
        }
        #[cfg(not(feature = "scenario"))]
        {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::InvalidScenario(
                    "Scenario support not compiled in. Build with --features scenario".to_string(),
                ),
            ));
        }
    } else {
        // Normal single-phase run
        let runner =
            Runner::new(config.clone()).with_live_metrics(std::sync::Arc::clone(&live_metrics));

        if config.tui {
            let config_arc = Arc::new(config.clone());
            let cancel_token = runner.cancel_token();

            let mut tui_app = TuiApp::new(
                config_arc,
                std::sync::Arc::clone(&live_metrics),
                cancel_token,
            );

            let runner_handle = tokio::spawn(async move { runner.run().await });

            if let Err(e) = tui_app.run().await {
                eprintln!("TUI error: {e}");
            }

            runner_handle
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))??
        } else {
            runner.run().await?
        }
    };

    // Multi-phase scenarios report aggregate sizing rather than the first phase.
    #[cfg(feature = "scenario")]
    let config = if scenario_mode {
        let mut aggregate_config = config;
        aggregate_config.connections =
            usize::try_from(metrics.total_connections_attempted).unwrap_or(usize::MAX);
        aggregate_config.duration = metrics.elapsed;
        aggregate_config
    } else {
        config
    };

    // Generate report and evaluate SLOs for both modes
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
