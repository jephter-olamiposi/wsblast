//! The `wsblast` command: argument parsing, the run, the report and the exit code.

use crate::cli::Cli;
use crate::config::LoadTestConfig;
#[cfg(feature = "scenario")]
use crate::config::ScenarioConfig;
use crate::error::{ConfigError, Result, WsBlastError};
use crate::metrics::LiveMetrics;
use crate::report;
use crate::runner::Runner;
#[cfg(feature = "scenario")]
use crate::runner::ScenarioExecutor;
use crate::tui::TuiApp;
use clap::{CommandFactory, Parser};
use colored::Colorize;
use std::sync::Arc;

/// Runs the command line and exits the process with its exit code.
///
/// Exit code 0 means the run completed and every SLO passed, 1 a
/// configuration or runtime error, and 2 an SLO breach or, with
/// `--fail-on-regression`, a regression.
pub async fn run_cli() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Err(err) = run_app().await {
        eprintln!("\n{} {}", "Error:".bold().red(), err);
        std::process::exit(1);
    }
}

/// Resolves configured authentication before the run, running
/// `--auth-token-cmd` if set. The returned guard keeps a token refresh
/// running and must live until the run ends.
#[cfg(feature = "auth")]
fn apply_auth(config: &mut LoadTestConfig) -> Result<Option<crate::auth::RefreshGuard>> {
    let Some(auth) = config.auth.take() else {
        return Ok(None);
    };
    Ok(
        crate::auth::prepare_auth(&auth, &mut config.headers)?.map(|(live, guard)| {
            config.live_auth = Some(live);
            guard
        }),
    )
}

/// Hidden commands that print packaging assets generated from the CLI
/// definition. They are recognised before the load-test parser runs, so the
/// normal command line and its `--help` output are unchanged.
#[derive(Debug, Parser)]
#[command(name = "wsblast", disable_version_flag = true)]
enum AssetCommand {
    /// Print a shell completion script to stdout
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the wsblast(1) man page, or a subcommand's wsblast-NAME(1)
    /// page, in roff format to stdout
    Man {
        /// Subcommand whose page to print
        subcommand: Option<String>,
    },
}

impl AssetCommand {
    /// Parses an asset command when the first argument names one.
    fn from_args() -> Option<Self> {
        let first = std::env::args_os().nth(1)?;
        if !matches!(first.to_str(), Some("completions" | "man")) {
            return None;
        }
        match Self::try_parse() {
            Ok(command) => Some(command),
            Err(err) => {
                let _ = err.print();
                std::process::exit(if err.use_stderr() { 1 } else { 0 });
            }
        }
    }

    /// Renders into memory first: the generators cannot report a failed
    /// write, and stdout may be a closed pipe.
    fn print(self) -> Result<()> {
        use std::io::Write;

        let mut rendered = Vec::new();
        match self {
            Self::Completions { shell } => {
                clap_complete::generate(shell, &mut Cli::command(), "wsblast", &mut rendered);
            }
            Self::Man { subcommand } => {
                let mut command = Cli::command();
                let page = match subcommand {
                    None => command,
                    Some(name) => {
                        command.build();
                        // clap names are static strings, so each page's
                        // name is listed here.
                        let page_name = match name.as_str() {
                            "mock" => "wsblast-mock",
                            "agent" => "wsblast-agent",
                            "render" => "wsblast-render",
                            _ => "",
                        };
                        let sub = command
                            .find_subcommand(&name)
                            .filter(|_| !page_name.is_empty())
                            .ok_or_else(|| {
                                WsBlastError::Config(ConfigError::InvalidOption(format!(
                                    "no subcommand named '{name}'"
                                )))
                            })?;
                        sub.clone()
                            .name(page_name)
                            .version(env!("CARGO_PKG_VERSION"))
                    }
                };
                clap_mangen::Man::new(page).render(&mut rendered)?;
            }
        }
        let mut stdout = std::io::stdout().lock();
        match stdout.write_all(&rendered).and_then(|()| stdout.flush()) {
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            result => Ok(result?),
        }
    }
}

/// Parses the command line, keeping exit code 2 reserved for SLO breaches.
///
/// `--help` and `--version` print to stdout and exit with 0 as clap does.
/// Every other parse error prints clap's message to stderr and exits with 1,
/// the code wsblast uses for configuration errors.
fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            std::process::exit(if err.use_stderr() { 1 } else { 0 });
        }
    }
}

#[cfg(feature = "compare")]
fn run_compare(cli: &Cli) -> Result<()> {
    if cli.compare.len() != 2 {
        return Err(WsBlastError::Config(ConfigError::InvalidOption(
            "Compare requires exactly 2 files: --compare <baseline.json> <current.json>".into(),
        )));
    }

    let baseline_path = &cli.compare[0];
    let current_path = &cli.compare[1];

    let baseline_json = std::fs::read_to_string(baseline_path).map_err(|e| {
        WsBlastError::Config(ConfigError::PayloadFile {
            path: baseline_path.display().to_string(),
            source: e,
        })
    })?;

    let current_json = std::fs::read_to_string(current_path).map_err(|e| {
        WsBlastError::Config(ConfigError::PayloadFile {
            path: current_path.display().to_string(),
            source: e,
        })
    })?;

    if cli.format == crate::cli::OutputFormat::Html {
        return Err(WsBlastError::Config(ConfigError::UnsupportedOption(
            "--compare writes text, json or markdown; --format html is for run reports".into(),
        )));
    }

    if !cli.compare_threshold.is_finite() || cli.compare_threshold < 0.0 {
        return Err(WsBlastError::Config(ConfigError::InvalidThreshold(
            format!(
                "--compare-threshold must be a finite non-negative percentage, received {}",
                cli.compare_threshold
            ),
        )));
    }

    let baseline =
        report::parse_report_metrics(&baseline_json).map_err(WsBlastError::Serialization)?;
    let current =
        report::parse_report_metrics(&current_json).map_err(WsBlastError::Serialization)?;

    let comparison = report::compare_metrics(&baseline, &current, cli.compare_threshold);
    let rendered = report::render_comparison_report(&comparison, cli.format, cli.compare_threshold);

    println!("{rendered}");

    if let Some(output_path) = &cli.output_file {
        std::fs::write(output_path, &rendered).map_err(WsBlastError::Io)?;
    }

    if cli.fail_on_regression && !comparison.regressions.is_empty() {
        std::process::exit(2);
    }

    Ok(())
}

/// `wsblast render`: reads a saved JSON report and renders it without
/// running a test. The exit code does not reflect the report's SLO result.
fn run_render(args: &crate::cli::RenderArgs) -> Result<()> {
    let path = args.report.display().to_string();
    let json = std::fs::read_to_string(&args.report).map_err(|e| {
        WsBlastError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read report '{path}': {e}"),
        ))
    })?;
    let saved = report::SavedReport::parse(&json).map_err(|reason| {
        WsBlastError::Config(ConfigError::InvalidOption(format!("'{path}': {reason}")))
    })?;
    // The report does not record when the run happened; the file's
    // modification time is when it was written.
    let saved_at = std::fs::metadata(&args.report)
        .and_then(|meta| meta.modified())
        .ok()
        .map(|time| ("Report saved", crate::clock::rfc3339_utc(time)));
    let rendered = report::render_view(&saved.view(saved_at), args.format.into());

    match &args.output {
        Some(output) => {
            std::fs::write(output, &rendered).map_err(|e| {
                WsBlastError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot write '{}': {e}", output.display()),
                ))
            })?;
            eprintln!("Wrote {}", output.display());
        }
        None => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            match writeln!(stdout, "{rendered}").and_then(|()| stdout.flush()) {
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
                result => result?,
            }
        }
    }
    Ok(())
}

/// The live exporters a run was asked for. They start before the run, so a
/// port in use or an invalid endpoint is a configuration error before any
/// connection is made, and they stop as soon as the run ends.
#[derive(Default)]
struct LiveExport {
    #[cfg(any(feature = "prometheus", feature = "otel"))]
    source: Option<Arc<crate::export::ExportSource>>,
    #[cfg(feature = "prometheus")]
    prometheus: Option<crate::export::prometheus::PrometheusServer>,
    #[cfg(feature = "otel")]
    otel: Option<crate::export::otel::OtelExporter>,
}

impl LiveExport {
    async fn start(cli: &Cli, config: &LoadTestConfig, live: &Arc<LiveMetrics>) -> Result<Self> {
        #[cfg_attr(not(any(feature = "prometheus", feature = "otel")), allow(unused_mut))]
        let mut export = Self::default();

        #[cfg(not(feature = "prometheus"))]
        if cli.prometheus_listen.is_some() {
            return Err(not_compiled_in("--prometheus-listen", "prometheus"));
        }
        #[cfg(not(feature = "otel"))]
        if cli.otel_endpoint.is_some() {
            return Err(not_compiled_in("--otel-endpoint", "otel"));
        }
        #[cfg(not(any(feature = "prometheus", feature = "otel")))]
        let _ = (config, live);

        #[cfg(feature = "otel")]
        let otel = match &cli.otel_endpoint {
            Some(endpoint) => Some(otel_settings(cli, endpoint, config)?),
            None => None,
        };
        #[cfg(all(feature = "prometheus", not(feature = "otel")))]
        let _ = config;

        #[cfg(feature = "prometheus")]
        if let Some(addr) = &cli.prometheus_listen {
            let server =
                crate::export::prometheus::PrometheusServer::bind(addr, export.source_for(live))
                    .await?;
            eprintln!(
                "Serving Prometheus metrics at http://{}/metrics",
                server.local_addr()
            );
            export.prometheus = Some(server);
        }

        #[cfg(feature = "otel")]
        if let Some(settings) = otel {
            export.otel = Some(crate::export::otel::OtelExporter::start(
                settings,
                export.source_for(live),
            )?);
        }

        Ok(export)
    }

    #[cfg(any(feature = "prometheus", feature = "otel"))]
    fn source_for(&mut self, live: &Arc<LiveMetrics>) -> Arc<crate::export::ExportSource> {
        Arc::clone(
            self.source
                .get_or_insert_with(|| crate::export::ExportSource::new(Arc::clone(live))),
        )
    }

    /// The source runners publish their time series to, when any exporter runs.
    #[cfg(any(feature = "prometheus", feature = "otel"))]
    fn source(&self) -> Option<Arc<crate::export::ExportSource>> {
        self.source.clone()
    }

    /// Freezes the exported run time, stops the Prometheus endpoint and sends
    /// the final OTLP export.
    async fn finish(self) {
        #[cfg(any(feature = "prometheus", feature = "otel"))]
        if let Some(source) = &self.source {
            source.finish();
        }
        #[cfg(feature = "prometheus")]
        if let Some(server) = self.prometheus {
            server.shutdown().await;
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel {
            otel.finish().await;
        }
    }
}

#[cfg(not(all(feature = "prometheus", feature = "otel")))]
fn not_compiled_in(flag: &str, feature: &str) -> WsBlastError {
    WsBlastError::Config(ConfigError::UnsupportedOption(format!(
        "{flag} needs the '{feature}' feature, which this build of wsblast does not include; rebuild with --features {feature}"
    )))
}

/// Validates the `--otel-*` options.
#[cfg(feature = "otel")]
fn otel_settings(
    cli: &Cli,
    endpoint: &str,
    config: &LoadTestConfig,
) -> Result<crate::export::otel::OtelSettings> {
    use clap::ValueEnum;

    let interval = crate::config::parse_human_duration(&cli.otel_interval)?;
    if interval.is_zero() {
        return Err(WsBlastError::Config(ConfigError::InvalidOption(
            "--otel-interval must be greater than zero".into(),
        )));
    }
    let service_name = cli
        .otel_service_name
        .clone()
        .or_else(|| std::env::var("OTEL_SERVICE_NAME").ok())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "wsblast".to_string());
    Ok(crate::export::otel::OtelSettings {
        metrics_url: crate::export::otel::metrics_url(endpoint)?,
        interval,
        service_name,
        target_host: config.target_url.host_str().unwrap_or_default().to_string(),
        mode: config
            .mode
            .to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_default(),
        defer_warning: config.tui,
    })
}

/// Rejects the options a distributed run does not support in this version.
#[cfg(feature = "distributed")]
fn check_distributed_options(cli: &Cli) -> Result<()> {
    let mut unsupported: Vec<&str> = Vec::new();
    if cli.scenario.is_some() {
        unsupported.push("--scenario");
    }
    #[cfg(feature = "capture")]
    if cli.capture.is_some() {
        unsupported.push("--capture");
    }
    if cli.tui {
        unsupported.push("--tui");
    }
    #[cfg(feature = "auth")]
    if cli.auth_token_refresh.is_some() {
        unsupported.push("--auth-token-refresh");
    }
    if cli.prometheus_listen.is_some() {
        unsupported.push("--prometheus-listen");
    }
    if cli.otel_endpoint.is_some() {
        unsupported.push("--otel-endpoint");
    }
    match unsupported.as_slice() {
        [] => Ok(()),
        flags => Err(WsBlastError::Config(ConfigError::UnsupportedOption(
            format!(
                "{} cannot be used with --agents in this version of wsblast",
                flags.join(", ")
            ),
        ))),
    }
}

/// `wsblast URL --agents ...`: runs the test on the agents, prints the
/// merged report and exits with 2 on an SLO breach, else 1 when an agent was
/// lost.
#[cfg(feature = "distributed")]
async fn run_distributed(cli: &Cli) -> Result<()> {
    use crate::distributed::{Coordinator, TOKEN_ENV};

    check_distributed_options(cli)?;
    let token = cli
        .agent_token
        .clone()
        .or_else(|| std::env::var(TOKEN_ENV).ok())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            WsBlastError::Config(ConfigError::InvalidOption(format!(
                "--agents needs the agents' token: pass --agent-token or set {TOKEN_ENV}"
            )))
        })?;
    let agent_timeout = crate::config::parse_human_duration(&cli.agent_timeout)?;
    if agent_timeout.is_zero() {
        return Err(WsBlastError::Config(ConfigError::InvalidOption(
            "--agent-timeout must be greater than zero".into(),
        )));
    }

    let config = LoadTestConfig::from_cli(cli.clone())?;
    // Credentials are resolved here, once, and sent to the agents as
    // handshake headers; a refreshed token was rejected above.
    #[cfg(feature = "auth")]
    let mut config = config;
    #[cfg(feature = "auth")]
    let _token_refresh = apply_auth(&mut config)?;

    let mut coordinator =
        Coordinator::new(config.clone(), &cli.agents, token)?.agent_timeout(agent_timeout);
    if let Some(ca) = &cli.agent_ca {
        coordinator = coordinator.agent_ca(ca);
    }
    let outcome = coordinator.run().await?;

    for agent in outcome.lost_agents() {
        eprintln!(
            "\n{} agent {} was lost: {}. Its connections and messages are not in this report.",
            "WARNING:".bold().red(),
            agent.address,
            agent.error.as_deref().unwrap_or("no result")
        );
    }
    let lost = outcome.lost_agents().count();
    let metrics = outcome.metrics;
    let slo_result = report::evaluate_slos(&config.slo, &metrics);
    report::emit_scenario_report(&config, &metrics, &slo_result, &[]).map_err(WsBlastError::Io)?;
    if lost > 0 {
        eprintln!(
            "{} {lost} of {} agents were lost; the report covers the other {}.",
            "WARNING:".bold().red(),
            cli.agents.len(),
            cli.agents.len() - lost
        );
    }

    if !slo_result.passed {
        std::process::exit(2);
    }
    if lost > 0 {
        std::process::exit(1);
    }
    exit_if_nothing_connected(&metrics);
    Ok(())
}

/// Exits with code 1 when no connection completed its handshake.
fn exit_if_nothing_connected(metrics: &crate::metrics::AggregatedMetrics) {
    if metrics.total_connections_attempted > 0 && metrics.total_connections_established == 0 {
        if metrics.total_connections_failed == 0 && metrics.total_connections_interrupted > 0 {
            eprintln!(
                "{} No connection finished its handshake before the run ended. Increase --duration or check the target.",
                "Failure:".bold().red()
            );
        } else {
            eprintln!(
                "{} All connection attempts failed. Target may be unreachable.",
                "Failure:".bold().red()
            );
        }
        std::process::exit(1);
    }
}

async fn run_app() -> Result<()> {
    if let Some(command) = AssetCommand::from_args() {
        return command.print();
    }

    let cli = parse_cli();

    #[cfg(feature = "mock")]
    if let Some(crate::cli::Command::Mock(args)) = &cli.command {
        return crate::mock::run_command(args).await;
    }

    #[cfg(feature = "distributed")]
    if let Some(crate::cli::Command::Agent(args)) = &cli.command {
        return crate::distributed::run_agent_command(args).await;
    }

    if let Some(crate::cli::Command::Render(args)) = &cli.command {
        return run_render(args);
    }

    // Handle compare command first (doesn't run load test)
    #[cfg(feature = "compare")]
    if !cli.compare.is_empty() {
        return run_compare(&cli);
    }

    #[cfg(feature = "distributed")]
    if !cli.agents.is_empty() {
        return run_distributed(&cli).await;
    }

    let scenario_mode = cli.is_scenario_mode();
    let cli_for_config = cli.clone();

    #[cfg(feature = "scenario")]
    let mut scenario: Option<ScenarioConfig> = None;

    let config = if scenario_mode {
        #[cfg(feature = "scenario")]
        {
            let scenario_path = cli_for_config.scenario.as_ref().ok_or_else(|| {
                WsBlastError::Config(ConfigError::InvalidScenario(
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
            return Err(WsBlastError::Config(ConfigError::InvalidScenario(
                "Scenario support not compiled in. Build with --features scenario".to_string(),
            )));
        }
    } else {
        LoadTestConfig::from_cli(cli_for_config)?
    };

    #[cfg(feature = "auth")]
    let mut config = config;

    #[cfg(feature = "auth")]
    let _token_refresh = apply_auth(&mut config)?;

    let live_metrics = LiveMetrics::new();
    let live_export = LiveExport::start(&cli, &config, &live_metrics).await?;

    // For scenario mode, run the ScenarioExecutor instead of single Runner.
    // The block's result is held until the live exporters have stopped, so
    // they shut down on errors too.
    let outcome: Result<_> = async {
        if scenario_mode {
            #[cfg(feature = "scenario")]
            {
                let scenario = scenario.as_ref().ok_or_else(|| {
                    WsBlastError::Config(ConfigError::InvalidScenario(
                        "Scenario mode requires --scenario <FILE>".to_string(),
                    ))
                })?;
                let executor = ScenarioExecutor::new(
                    scenario,
                    config.clone(),
                    cli.phase.as_deref(),
                    cli.resolve_url(),
                )?
                .with_live_metrics(Arc::clone(&live_metrics));
                #[cfg(any(feature = "prometheus", feature = "otel"))]
                let executor = executor.with_export(live_export.source());
                let outcome = executor.run().await?;
                Ok((outcome.metrics, outcome.phase_targets))
            }
            #[cfg(not(feature = "scenario"))]
            {
                Err(WsBlastError::Config(ConfigError::InvalidScenario(
                    "Scenario support not compiled in. Build with --features scenario".to_string(),
                )))
            }
        } else {
            // Normal single-phase run
            let runner = Runner::new(config.clone()).with_live_metrics(Arc::clone(&live_metrics));
            #[cfg(any(feature = "prometheus", feature = "otel"))]
            let runner = runner.with_export(live_export.source());

            if config.tui {
                let config_arc = Arc::new(config.clone());
                let cancel_token = runner.cancel_token();

                let mut tui_app = TuiApp::new(config_arc, Arc::clone(&live_metrics), cancel_token);

                let runner_handle = tokio::spawn(async move { runner.run().await });

                if let Err(e) = tui_app.run().await {
                    eprintln!("TUI error: {e}");
                }

                let metrics = runner_handle
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))??;
                Ok((metrics, Vec::new()))
            } else {
                Ok((runner.run().await?, Vec::new()))
            }
        }
    }
    .await;
    live_export.finish().await;
    let (metrics, phase_targets) = outcome?;

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

    report::emit_scenario_report(&config, &metrics, &slo_result, &phase_targets)
        .map_err(WsBlastError::Io)?;

    if !slo_result.passed {
        std::process::exit(2);
    }

    exit_if_nothing_connected(&metrics);

    Ok(())
}
