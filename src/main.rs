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

#[cfg(feature = "distributed")]
use wsblast::distributed::{DistributedConfig, DistributedMode};

#[cfg(feature = "prometheus")]
use axum::response::IntoResponse;

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

    // Handle mock subcommand first (doesn't run load test)
    #[cfg(feature = "mock")]
    if let Some(mock_cmd) = &cli.mock {
        return handle_mock_command(mock_cmd).await;
    }

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

    // Handle distributed mode (coordinator or worker)
    #[cfg(feature = "distributed")]
    if let Some(mode) = cli.distributed_mode {
        if cli.scenario.is_some() {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::UnsupportedOption(
                    "scenario definitions cannot be distributed; run each phase as a separate load test"
                        .to_string(),
                ),
            ));
        }
        if cli.tui {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::UnsupportedOption(
                    "the interactive TUI is only available in standalone mode".to_string(),
                ),
            ));
        }
        #[cfg(feature = "capture")]
        if cli.capture {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::UnsupportedOption(
                    "frame capture is only available in standalone mode".to_string(),
                ),
            ));
        }
        #[cfg(feature = "otel")]
        if cli.enable_otel {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::UnsupportedOption(
                    "OpenTelemetry export is only available in standalone mode".to_string(),
                ),
            ));
        }
        #[cfg(feature = "prometheus")]
        if cli.enable_prometheus {
            return Err(wsblast::error::WsBlastError::Config(
                wsblast::error::ConfigError::UnsupportedOption(
                    "Prometheus export is only available in standalone mode".to_string(),
                ),
            ));
        }

        let coordinator_url = match cli.coordinator_url.as_deref() {
            Some(raw) => Some(url::Url::parse(raw).map_err(|e| {
                wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::InvalidUrl(
                    raw.to_string(),
                    e.to_string(),
                ))
            })?),
            None => None,
        };

        let distributed_config = DistributedConfig {
            mode,
            coordinator_url,
            worker_id: cli.worker_id.clone(),
            worker_count: cli.worker_count,
            bind_addr: cli.worker_bind.clone(),
            coordinator_bind_addr: cli.coordinator_bind.clone(),
        };

        return match mode {
            DistributedMode::Coordinator => {
                let load_config = LoadTestConfig::from_cli(cli.clone())?;
                #[cfg(feature = "auth")]
                let mut load_config = load_config;
                #[cfg(feature = "auth")]
                apply_auth(&mut load_config)?;
                let slo_passed =
                    wsblast::distributed::run_coordinator(distributed_config, load_config)
                        .await
                        .map_err(|e| wsblast::error::WsBlastError::Distributed(e.to_string()))?;
                if !slo_passed {
                    std::process::exit(2);
                }
                Ok(())
            }
            DistributedMode::Worker => wsblast::distributed::run_worker(distributed_config)
                .await
                .map_err(|e| wsblast::error::WsBlastError::Distributed(e.to_string())),
        };
    }

    // Extract values needed for observability before cli is potentially moved
    #[cfg(feature = "otel")]
    let otel_endpoint = cli.otel_endpoint.clone();
    #[cfg(feature = "otel")]
    let otel_service_name = cli.otel_service_name.clone();
    #[cfg(feature = "prometheus")]
    let prometheus_listen = cli.prometheus_listen.clone();
    #[cfg(not(feature = "otel"))]
    let _otel_endpoint = String::new();
    #[cfg(not(feature = "otel"))]
    let _otel_service_name = String::new();
    #[cfg(not(feature = "prometheus"))]
    let _prometheus_listen = String::new();

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

    // Observability exporters must start before load so Prometheus scrapes and
    // OTLP pushes cover the live run.
    let live_metrics = wsblast::metrics::LiveMetrics::new();

    #[cfg(feature = "otel")]
    let otel_exporter = if cli.enable_otel {
        Some(std::sync::Arc::new(
            wsblast::OtelExporter::new(&otel_endpoint, &otel_service_name).map_err(|e| {
                wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::InvalidThreshold(
                    e.to_string(),
                ))
            })?,
        ))
    } else {
        None
    };

    #[cfg(feature = "otel")]
    let otel_task = otel_exporter.as_ref().map(|exporter| {
        let exporter = std::sync::Arc::clone(exporter);
        let live = std::sync::Arc::clone(&live_metrics);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                ticker.tick().await;
                exporter.refresh_live(&live.snapshot());
            }
        })
    });

    #[cfg(feature = "prometheus")]
    let prometheus_exporter = if cli.enable_prometheus {
        let exporter = std::sync::Arc::new(wsblast::PrometheusExporter::new().map_err(|e| {
            wsblast::error::WsBlastError::Config(wsblast::error::ConfigError::InvalidThreshold(
                e.to_string(),
            ))
        })?);
        let registry = std::sync::Arc::new(exporter.build_registry());
        let listener = tokio::net::TcpListener::bind(&prometheus_listen)
            .await
            .map_err(|e| {
                wsblast::error::WsBlastError::Io(std::io::Error::other(format!(
                    "failed to bind Prometheus listener on {prometheus_listen}: {e}"
                )))
            })?;
        let exporter_for_server = std::sync::Arc::clone(&exporter);
        let live_for_server = std::sync::Arc::clone(&live_metrics);
        let server = tokio::spawn(async move {
            let router = axum::Router::new().route(
                "/metrics",
                axum::routing::get(move || {
                    let exporter = std::sync::Arc::clone(&exporter_for_server);
                    let live = std::sync::Arc::clone(&live_for_server);
                    let registry = std::sync::Arc::clone(&registry);
                    async move {
                        exporter.refresh_live(&live.snapshot());
                        let mut buffer = String::new();
                        match prometheus_client::encoding::text::encode(
                            &mut buffer,
                            registry.as_ref(),
                        ) {
                            Ok(()) => buffer.into_response(),
                            Err(err) => {
                                eprintln!("Prometheus encoding failed: {err}");
                                (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    "metrics encoding failed",
                                )
                                    .into_response()
                            }
                        }
                    }
                }),
            );
            if let Err(err) = axum::serve(listener, router).await {
                eprintln!("Prometheus metrics server failed: {err}");
            }
        });
        Some((exporter, server))
    } else {
        None
    };

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

    // Flush observability before any process exit so CI failures still export.
    #[cfg(feature = "otel")]
    {
        if let Some(task) = otel_task {
            task.abort();
        }
        if let Some(exporter) = &otel_exporter {
            exporter.refresh_live(&live_metrics.snapshot());
            exporter.export_final(&metrics);
            let _ = exporter.shutdown();
        }
    }

    #[cfg(feature = "prometheus")]
    if let Some((exporter, server)) = prometheus_exporter {
        exporter.refresh_live(&live_metrics.snapshot());
        exporter.export_final(&metrics);
        server.abort();
    }

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

#[cfg(feature = "mock")]
async fn handle_mock_command(mock_cmd: &wsblast::cli::MockSubcommand) -> Result<()> {
    match mock_cmd {
        wsblast::cli::MockSubcommand::Record {
            bind,
            output,
            delay,
        } => {
            let config = wsblast::mock::MockConfig {
                bind_addr: *bind,
                record_path: Some(output.clone()),
                replay_path: None,
                delay_ms: *delay,
                error_injection: None,
            };
            let server = wsblast::mock::MockServer::new(config);
            server.run().await?;
        }
        wsblast::cli::MockSubcommand::Replay {
            bind,
            input,
            delay,
            error_close_after,
            error_inject_after,
            error_message,
        } => {
            let error_injection = if error_close_after.is_some()
                || error_inject_after.is_some()
                || error_message.is_some()
            {
                Some(wsblast::mock::ErrorInjectionConfig {
                    close_after_messages: *error_close_after,
                    inject_error_after: *error_inject_after,
                    error_message: error_message.clone(),
                })
            } else {
                None
            };
            let config = wsblast::mock::MockConfig {
                bind_addr: *bind,
                record_path: None,
                replay_path: Some(input.clone()),
                delay_ms: *delay,
                error_injection,
            };
            let server = wsblast::mock::MockServer::new(config);
            server.run().await?;
        }
    }
    Ok(())
}
