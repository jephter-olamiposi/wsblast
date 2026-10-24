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

/// Resolves configured authentication once and merges it into handshake headers.
#[cfg(feature = "auth")]
fn apply_auth(config: &mut LoadTestConfig) -> Result<()> {
    if let Some(auth) = config.auth.take() {
        crate::auth::apply_auth_header(&mut config.headers, &auth).map_err(WsBlastError::Worker)?;
    }
    Ok(())
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
    /// Print the wsblast(1) man page in roff format to stdout
    Man,
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
            Self::Man => clap_mangen::Man::new(Cli::command()).render(&mut rendered)?,
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

async fn run_app() -> Result<()> {
    if let Some(command) = AssetCommand::from_args() {
        return command.print();
    }

    let cli = parse_cli();

    // Handle compare command first (doesn't run load test)
    #[cfg(feature = "compare")]
    if !cli.compare.is_empty() {
        return run_compare(&cli);
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
    apply_auth(&mut config)?;

    let live_metrics = LiveMetrics::new();

    // For scenario mode, run the ScenarioExecutor instead of single Runner
    let (metrics, phase_targets) = if scenario_mode {
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
            let outcome = executor.run().await?;
            (outcome.metrics, outcome.phase_targets)
        }
        #[cfg(not(feature = "scenario"))]
        {
            return Err(WsBlastError::Config(ConfigError::InvalidScenario(
                "Scenario support not compiled in. Build with --features scenario".to_string(),
            )));
        }
    } else {
        // Normal single-phase run
        let runner = Runner::new(config.clone()).with_live_metrics(Arc::clone(&live_metrics));

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
            (metrics, Vec::new())
        } else {
            (runner.run().await?, Vec::new())
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

    report::emit_scenario_report(&config, &metrics, &slo_result, &phase_targets)
        .map_err(WsBlastError::Io)?;

    if !slo_result.passed {
        std::process::exit(2);
    }

    if metrics.total_connections_attempted > 0 && metrics.total_connections_established == 0 {
        eprintln!(
            "{} All connection attempts failed. Target may be unreachable.",
            "Failure:".bold().red()
        );
        std::process::exit(1);
    }

    Ok(())
}
