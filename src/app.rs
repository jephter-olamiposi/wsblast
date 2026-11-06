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
use crate::timeseries::LiveIntervals;
use crate::tui::{DashboardSource, PhasePlan, RunFeed, RunInfo, RunProgress, TuiApp};
use clap::{CommandFactory, Parser};
use colored::Colorize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
///
/// clap only recognises a subcommand as the first argument: in
/// `wsblast -c 2 mock` the word `mock` is the target URL. Such a command
/// line fails with a note on where subcommands go, instead of an invalid
/// URL or an unexpected-argument error about the subcommand's own flags.
fn parse_cli() -> Result<Cli> {
    match Cli::try_parse() {
        Ok(cli) => {
            if let Some(name) = cli.url.as_deref().filter(|url| is_subcommand_name(url)) {
                return Err(WsBlastError::Config(ConfigError::InvalidOption(
                    misplaced_subcommand(name),
                )));
            }
            Ok(cli)
        }
        Err(err) => {
            let _ = err.print();
            if err.use_stderr() {
                // A misplaced subcommand followed by its own flags fails
                // to parse before the URL can be checked.
                if let Some(name) = std::env::args().skip(2).find(|arg| is_subcommand_name(arg)) {
                    eprintln!(
                        "\nNote: if '{name}' is meant as a subcommand, {}",
                        subcommand_usage(&name)
                    );
                }
            }
            std::process::exit(if err.use_stderr() { 1 } else { 0 });
        }
    }
}

/// Whether `word` names a subcommand of this build, the hidden asset
/// commands included.
fn is_subcommand_name(word: &str) -> bool {
    matches!(word, "completions" | "man")
        || Cli::command()
            .get_subcommands()
            .any(|sub| sub.get_name() == word)
}

fn subcommand_usage(name: &str) -> String {
    format!(
        "it must come first, followed only by its own flags: wsblast {name} [OPTIONS] (see wsblast {name} --help)"
    )
}

fn misplaced_subcommand(name: &str) -> String {
    format!(
        "'{name}' is a subcommand, not a target URL; {}",
        subcommand_usage(name)
    )
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

    if !cli.compare_threshold.is_finite() || cli.compare_threshold < 0.0 {
        return Err(WsBlastError::Config(ConfigError::InvalidThreshold(
            format!(
                "--compare-threshold must be a finite non-negative percentage, received {}",
                cli.compare_threshold
            ),
        )));
    }

    let window = cli
        .compare_window
        .as_deref()
        .map(crate::series_compare::CompareWindow::parse)
        .transpose()?;

    let baseline =
        report::parse_report_metrics(&baseline_json).map_err(WsBlastError::Serialization)?;
    let current =
        report::parse_report_metrics(&current_json).map_err(WsBlastError::Serialization)?;

    if let Some(window) = &window {
        for (path, metrics) in [(baseline_path, &baseline), (current_path, &current)] {
            check_window_covers(window, path, metrics)?;
        }
    }

    let comparison =
        report::compare_metrics_in(&baseline, &current, cli.compare_threshold, window.as_ref());
    let files = Some((
        baseline_path.display().to_string(),
        current_path.display().to_string(),
    ));
    let view = comparison.view(cli.compare_threshold, files);
    let rendered = report::render_comparison_view(&view, cli.format);

    // As for a run: with -o the file gets the chosen format and stdout the
    // text comparison.
    match &cli.output_file {
        Some(output_path) => {
            std::fs::write(output_path, &rendered).map_err(WsBlastError::Io)?;
            let summary = if cli.format == crate::cli::OutputFormat::Text {
                rendered
            } else {
                report::render_comparison_view(&view, crate::cli::OutputFormat::Text)
            };
            crate::output::outln!("{summary}");
        }
        None => crate::output::outln!("{rendered}"),
    }

    if cli.fail_on_regression && !comparison.regressions.is_empty() {
        std::process::exit(2);
    }

    Ok(())
}

/// A window can only be applied to a report with a time series, and must
/// hold at least one of its intervals: otherwise the time-series metrics
/// would silently drop out of the comparison and a gate on them would pass.
#[cfg(feature = "compare")]
fn check_window_covers(
    window: &crate::series_compare::CompareWindow,
    path: &std::path::Path,
    metrics: &crate::metrics::AggregatedMetrics,
) -> Result<()> {
    let series = &metrics.time_series;
    let reason = if series.points.is_empty() {
        "has no time series (reports from wsblast 0.2.0 and earlier have none)".to_string()
    } else if crate::series_compare::window_points(series, Some(window))
        .next()
        .is_none()
    {
        let end = series
            .points
            .last()
            .map_or(0.0, |point| point.start_secs + point.duration_secs);
        format!("has no interval inside the window (its time series covers 0s..{end:.3}s)")
    } else {
        return Ok(());
    };
    Err(WsBlastError::Config(ConfigError::InvalidOption(format!(
        "--compare-window {window}: {} {reason}",
        path.display()
    ))))
}

/// `wsblast render`: reads a saved JSON report, or a comparison JSON from
/// `--compare --format json`, and renders it without running a test. The
/// exit code does not reflect the report's SLO result or regressions.
fn run_render(args: &crate::cli::RenderArgs) -> Result<()> {
    let path = args.report.display().to_string();
    let json = std::fs::read_to_string(&args.report).map_err(|e| {
        WsBlastError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read report '{path}': {e}"),
        ))
    })?;
    let invalid = |reason: String| {
        WsBlastError::Config(ConfigError::InvalidOption(format!("'{path}': {reason}")))
    };
    // A report does not record when it was written; the file's
    // modification time is.
    let saved_at = |label: &'static str| {
        std::fs::metadata(&args.report)
            .and_then(|meta| meta.modified())
            .ok()
            .map(|time| (label, crate::clock::rfc3339_utc(time)))
    };
    let format = args.format.into();
    #[cfg(feature = "compare")]
    if let Some(saved) = report::SavedComparison::detect(&json) {
        let saved = saved.map_err(invalid)?;
        let rendered =
            report::render_comparison_view(&saved.view(saved_at("Comparison saved")), format);
        return write_rendered(args, &rendered);
    }
    let saved = report::SavedReport::parse(&json).map_err(invalid)?;
    let rendered = report::render_view(&saved.view(saved_at("Report saved")), format);
    write_rendered(args, &rendered)
}

/// Writes what `wsblast render` rendered to `--output` or stdout.
fn write_rendered(args: &crate::cli::RenderArgs, rendered: &str) -> Result<()> {
    match &args.output {
        Some(output) => {
            std::fs::write(output, rendered).map_err(|e| {
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
    /// Starts the exporters `cli` asks for, reading `live` and the time
    /// series the runner registers in `intervals`.
    async fn start(
        cli: &Cli,
        config: &LoadTestConfig,
        live: &Arc<LiveMetrics>,
        intervals: &Arc<LiveIntervals>,
    ) -> Result<Self> {
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
        let _ = (config, live, intervals);

        #[cfg(feature = "otel")]
        let otel = match &cli.otel_endpoint {
            Some(endpoint) => Some(otel_settings(cli, endpoint, config)?),
            None => None,
        };
        #[cfg(all(feature = "prometheus", not(feature = "otel")))]
        let _ = config;

        #[cfg(feature = "prometheus")]
        if let Some(addr) = &cli.prometheus_listen {
            let server = crate::export::prometheus::PrometheusServer::bind(
                addr,
                export.source_for(live, intervals),
            )
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
                export.source_for(live, intervals),
            )?);
        }

        Ok(export)
    }

    #[cfg(any(feature = "prometheus", feature = "otel"))]
    fn source_for(
        &mut self,
        live: &Arc<LiveMetrics>,
        intervals: &Arc<LiveIntervals>,
    ) -> Arc<crate::export::ExportSource> {
        Arc::clone(self.source.get_or_insert_with(|| {
            crate::export::ExportSource::with_intervals(Arc::clone(live), Arc::clone(intervals))
        }))
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
    // A token re-signed before expiry, or an OAuth2 token refreshed before
    // it expires, is pushed to the agents like a refreshed token command. A
    // token signed per connection attempt would need the signing key on
    // every agent, and the key stays on the coordinator.
    #[cfg(feature = "auth")]
    if cli.auth_jwt_per_connection {
        return Err(WsBlastError::Config(ConfigError::UnsupportedOption(
            "--auth-jwt-per-connection cannot be used with --agents: the signing key stays on the coordinator; without the flag the shared token is re-signed before expiry and pushed to the agents".to_string(),
        )));
    }
    #[cfg(not(feature = "auth"))]
    let _ = cli;
    Ok(())
}

/// The dashboard of a coordinator and the token that stops its run: the
/// keys that quit the dashboard stop every agent, as Ctrl+C does, and the
/// run still reports what the agents measured.
#[cfg(feature = "distributed")]
fn cluster_dashboard(
    coordinator: &crate::distributed::Coordinator,
) -> Result<(Arc<dyn DashboardSource>, CancellationToken)> {
    let feed = coordinator.dashboard().ok_or_else(|| {
        WsBlastError::Distributed("the dashboard has no live values to read".to_string())
    })?;
    Ok((Arc::new(feed), coordinator.cancel_token()))
}

/// What a distributed run, single or scenario, reports.
#[cfg(feature = "distributed")]
struct DistributedResult {
    run: RunResult,
    /// Addresses and reasons of the lost agents.
    lost: Vec<(String, String)>,
}

/// `wsblast URL --agents ...`: runs the test, or the scenario, on the
/// agents, prints the merged report and exits with 2 on an SLO breach, else
/// 1 when an agent was lost.
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

    let scenario_mode = cli.is_scenario_mode();
    #[cfg(feature = "scenario")]
    let scenario = match (&cli.scenario, scenario_mode) {
        (Some(path), true) => Some(ScenarioConfig::load(path)?),
        _ => None,
    };
    #[cfg(not(feature = "scenario"))]
    if scenario_mode {
        return Err(WsBlastError::Config(ConfigError::InvalidScenario(
            "Scenario support not compiled in. Build with --features scenario".to_string(),
        )));
    }
    #[cfg(feature = "scenario")]
    let config = match &scenario {
        Some(scenario) => LoadTestConfig::from_scenario_config(scenario, cli)?,
        None => LoadTestConfig::from_cli(cli.clone())?,
    };
    #[cfg(not(feature = "scenario"))]
    let config = LoadTestConfig::from_cli(cli.clone())?;
    // Credentials are resolved here, once, and sent to the agents as
    // handshake headers; a refreshed token is pushed to them as it changes,
    // for as long as the guard lives.
    #[cfg(feature = "auth")]
    let mut config = config;
    #[cfg(feature = "auth")]
    let _token_refresh = apply_auth(&mut config)?;

    let live_metrics = LiveMetrics::new();
    let live_intervals = LiveIntervals::new();
    let live_export = LiveExport::start(cli, &config, &live_metrics, &live_intervals).await?;
    let outcome: Result<DistributedResult> = async {
        #[cfg(feature = "scenario")]
        if let Some(scenario) = &scenario {
            let mut coordinator = Coordinator::scenario(
                scenario,
                config.clone(),
                cli.phase.as_deref(),
                cli.resolve_url(),
                &cli.agents,
                token,
            )?
            .agent_timeout(agent_timeout)
            .with_live_metrics(Arc::clone(&live_metrics));
            coordinator = coordinator.with_live_intervals(Some(Arc::clone(&live_intervals)));
            if let Some(ca) = &cli.agent_ca {
                coordinator = coordinator.agent_ca(ca);
            }
            let outcome = if config.tui {
                let coordinator = coordinator.with_progress(RunProgress::new());
                let (feed, cancel_token) = cluster_dashboard(&coordinator)?;
                run_with_tui(&config, feed, cancel_token, async move {
                    coordinator.run_scenario().await
                })
                .await?
            } else {
                coordinator.run_scenario().await?
            };
            let lost = outcome
                .lost_agents()
                .map(|agent| {
                    (
                        agent.address.clone(),
                        agent.error.clone().unwrap_or_default(),
                    )
                })
                .collect();
            let scenario = outcome.scenario;
            return Ok(DistributedResult {
                run: RunResult {
                    phases: scenario
                        .phases
                        .iter()
                        .map(report::PhaseReport::from)
                        .collect(),
                    metrics: scenario.metrics,
                    phase_targets: scenario.phase_targets,
                    slo: Some(scenario.slo),
                },
                lost,
            });
        }
        let mut coordinator = Coordinator::new(config.clone(), &cli.agents, token)?
            .agent_timeout(agent_timeout)
            .with_live_metrics(Arc::clone(&live_metrics));
        coordinator = coordinator.with_live_intervals(Some(Arc::clone(&live_intervals)));
        if let Some(ca) = &cli.agent_ca {
            coordinator = coordinator.agent_ca(ca);
        }
        let outcome = if config.tui {
            let coordinator = coordinator.with_progress(RunProgress::new());
            let (feed, cancel_token) = cluster_dashboard(&coordinator)?;
            run_with_tui(&config, feed, cancel_token, async move {
                coordinator.run().await
            })
            .await?
        } else {
            coordinator.run().await?
        };
        let lost = outcome
            .lost_agents()
            .map(|agent| {
                (
                    agent.address.clone(),
                    agent.error.clone().unwrap_or_default(),
                )
            })
            .collect();
        Ok(DistributedResult {
            run: RunResult::single(outcome.metrics),
            lost,
        })
    }
    .await;
    live_intervals.finish();
    live_export.finish().await;
    let DistributedResult { run, lost } = outcome?;
    let RunResult {
        metrics,
        phase_targets,
        phases,
        slo,
    } = run;

    for (address, reason) in &lost {
        eprintln!(
            "\n{} agent {address} was lost: {reason}. Its connections and messages are not in this report.",
            "WARNING:".bold().red(),
        );
    }
    let config = if scenario_mode {
        let mut aggregate_config = config;
        aggregate_config.connections =
            usize::try_from(metrics.total_connections_attempted).unwrap_or(usize::MAX);
        aggregate_config.duration = metrics.elapsed;
        aggregate_config
    } else {
        config
    };
    let slo_result = slo.unwrap_or_else(|| report::evaluate_slos(&config.slo, &metrics));
    report::emit_scenario_report(&config, &metrics, &slo_result, &phase_targets, &phases)
        .map_err(WsBlastError::Io)?;
    warn_if_generator_saturated(&metrics);
    if !lost.is_empty() {
        eprintln!(
            "{} {} of {} agents were lost; the report covers the other {}.",
            "WARNING:".bold().red(),
            lost.len(),
            cli.agents.len(),
            cli.agents.len() - lost.len()
        );
    }

    if !slo_result.passed {
        std::process::exit(2);
    }
    if !lost.is_empty() {
        std::process::exit(1);
    }
    exit_if_nothing_connected(&metrics);
    Ok(())
}

/// Warns, after the report, when the load generator was the bottleneck: on
/// one machine, or on any agent of a distributed run, each named.
fn warn_if_generator_saturated(metrics: &crate::metrics::AggregatedMetrics) {
    if let Some(distributed) = metrics
        .distributed
        .as_ref()
        .filter(|summary| !summary.generators.is_empty())
    {
        for agent in &distributed.generators {
            if let Some(lag) = agent.generator_lag.as_ref().filter(|lag| lag.saturated) {
                eprintln!(
                    "\n{} The load generator of agent {} was saturated: its runtime woke up late by {} at p99 and {} at most (threshold {}; {} of {} intervals over it). \
                     Every latency it measured, and so every latency in this report, includes that client-side delay and may be invalid. \
                     Use fewer connections or a lower rate per agent, give that machine more CPU, or add agents.",
                    "WARNING:".bold().red(),
                    agent.agent,
                    report::format_us(lag.p99_us),
                    report::format_us(lag.max_us),
                    report::format_us(lag.threshold_us),
                    lag.intervals_over_threshold,
                    lag.intervals
                );
            }
        }
        return;
    }
    let Some(lag) = metrics.generator_lag.as_ref().filter(|lag| lag.saturated) else {
        return;
    };
    eprintln!(
        "\n{} The load generator itself was saturated: its runtime woke up late by {} at p99 and {} at most (threshold {}; {} of {} intervals over it). \
         Every latency in this report includes that client-side delay and may be invalid. \
         Use fewer connections or a lower rate per machine, give wsblast more CPU, or spread the load over several machines with --agents.",
        "WARNING:".bold().red(),
        report::format_us(lag.p99_us),
        report::format_us(lag.max_us),
        report::format_us(lag.threshold_us),
        lag.intervals_over_threshold,
        lag.intervals
    );
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

/// What a completed run, single or scenario, reports.
struct RunResult {
    metrics: crate::metrics::AggregatedMetrics,
    phase_targets: Vec<report::PhaseTarget>,
    phases: Vec<report::PhaseReport>,
    /// The SLO result of a scenario, which evaluates its own; `None` for a
    /// single run, whose flags are checked after it.
    slo: Option<report::SloEvaluation>,
}

impl RunResult {
    fn single(metrics: crate::metrics::AggregatedMetrics) -> Self {
        Self {
            metrics,
            phase_targets: Vec::new(),
            phases: Vec::new(),
            slo: None,
        }
    }
}

/// Runs `run` in its own task while the dashboard draws `feed`, and returns
/// the run's result once it has returned. `cancel_token` is the run's: `q`
/// and Ctrl+C cancel it, and the run still reports what it measured. For a
/// coordinator, cancelling it stops every agent.
///
/// When the terminal cannot host the dashboard, the error is printed and
/// the run continues without it.
async fn run_with_tui<T: Send + 'static>(
    config: &LoadTestConfig,
    feed: Arc<dyn DashboardSource>,
    cancel_token: CancellationToken,
    run: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    use clap::ValueEnum;

    let done = CancellationToken::new();
    let task = tokio::spawn({
        let done = done.clone();
        async move {
            let result = run.await;
            done.cancel();
            result
        }
    });
    let info = RunInfo {
        target: config.target_url.to_string(),
        mode: config
            .mode
            .to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_default(),
    };
    let mut tui = TuiApp::new(info, feed, cancel_token, done);
    if let Err(e) = tui.run().await {
        eprintln!("TUI error: {e}");
    }
    task.await
        .map_err(|e| std::io::Error::other(e.to_string()))?
}

async fn run_app() -> Result<()> {
    if let Some(command) = AssetCommand::from_args() {
        return command.print();
    }

    let cli = parse_cli()?;

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
    // The runners register each phase's time series here; the exporters
    // and the TUI read the last completed interval from it.
    let live_intervals = LiveIntervals::new();
    let live_export = LiveExport::start(&cli, &config, &live_metrics, &live_intervals).await?;

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
                .with_live_metrics(Arc::clone(&live_metrics))
                .with_live_intervals(Some(Arc::clone(&live_intervals)));
                let outcome = if config.tui {
                    let progress = RunProgress::new();
                    let executor = executor.with_progress(Some(Arc::clone(&progress)));
                    let cancel_token = executor.cancel_token();
                    let feed = RunFeed::new(
                        Arc::clone(&live_metrics),
                        Arc::clone(&live_intervals),
                        progress,
                    );
                    run_with_tui(&config, Arc::new(feed), cancel_token, async move {
                        executor.run().await
                    })
                    .await?
                } else {
                    executor.run().await?
                };
                let phases: Vec<report::PhaseReport> = outcome
                    .phases
                    .iter()
                    .map(report::PhaseReport::from)
                    .collect();
                Ok(RunResult {
                    metrics: outcome.metrics,
                    phase_targets: outcome.phase_targets,
                    phases,
                    slo: Some(outcome.slo),
                })
            }
            #[cfg(not(feature = "scenario"))]
            {
                Err(WsBlastError::Config(ConfigError::InvalidScenario(
                    "Scenario support not compiled in. Build with --features scenario".to_string(),
                )))
            }
        } else {
            // Normal single-phase run
            let runner = Runner::new(config.clone())
                .with_live_metrics(Arc::clone(&live_metrics))
                .with_live_intervals(Some(Arc::clone(&live_intervals)));

            if config.tui {
                let progress = RunProgress::new();
                progress.begin_phase(PhasePlan::for_run(&config));
                let cancel_token = runner.cancel_token();
                let feed = RunFeed::new(
                    Arc::clone(&live_metrics),
                    Arc::clone(&live_intervals),
                    progress,
                );
                let metrics = run_with_tui(&config, Arc::new(feed), cancel_token, async move {
                    runner.run().await
                })
                .await?;
                Ok(RunResult::single(metrics))
            } else {
                Ok(RunResult::single(runner.run().await?))
            }
        }
    }
    .await;
    live_intervals.finish();
    live_export.finish().await;
    let RunResult {
        metrics,
        phase_targets,
        phases,
        slo,
    } = outcome?;

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

    // A scenario evaluated its own SLOs, its phases' and the flags'; a
    // single run checks the flags here.
    let slo_result = slo.unwrap_or_else(|| report::evaluate_slos(&config.slo, &metrics));

    report::emit_scenario_report(&config, &metrics, &slo_result, &phase_targets, &phases)
        .map_err(WsBlastError::Io)?;
    warn_if_generator_saturated(&metrics);

    if !slo_result.passed {
        std::process::exit(2);
    }

    exit_if_nothing_connected(&metrics);

    Ok(())
}
