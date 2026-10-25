//! Render tests: the dashboard drawn from a scripted [`DashboardSource`]
//! into ratatui's `TestBackend`.

use super::model::{Dashboard, DashboardView, RunInfo};
use super::source::{DashboardSource, FinishedPhase, PhasePlan, ProgressView};
use super::widgets::render_dashboard;
use crate::config::{SloThresholds, Stage};
use crate::metrics::LiveSnapshot;
use crate::timeseries::{CompletedInterval, IntervalLatency, PhaseClock};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use std::sync::Mutex;
use std::time::Duration;

/// A source whose every value the test sets.
#[derive(Default)]
struct Script {
    state: Mutex<ScriptState>,
}

#[derive(Default)]
struct ScriptState {
    counters: LiveSnapshot,
    run_elapsed: Duration,
    clock: Option<PhaseClock>,
    last: Option<CompletedInterval>,
    progress: ProgressView,
}

impl DashboardSource for Script {
    fn counters(&self) -> LiveSnapshot {
        self.state.lock().unwrap().counters
    }
    fn run_elapsed(&self) -> Duration {
        self.state.lock().unwrap().run_elapsed
    }
    fn phase_clock(&self) -> Option<PhaseClock> {
        self.state.lock().unwrap().clock
    }
    fn last_interval(&self) -> Option<CompletedInterval> {
        self.state.lock().unwrap().last.clone()
    }
    fn progress(&self) -> ProgressView {
        self.state.lock().unwrap().progress.clone()
    }
}

fn latency(count: u64, p50_us: u64, p90_us: u64, p99_us: u64) -> IntervalLatency {
    IntervalLatency {
        count,
        p50_us,
        p90_us,
        p99_us,
        max_us: p99_us,
    }
}

fn info() -> RunInfo {
    RunInfo {
        target: "ws://127.0.0.1:9001/".to_string(),
        mode: "echo".to_string(),
    }
}

/// Plays `seconds` of a run at 4 refreshes per second, with `rate` messages
/// per second, the p99 of interval `i` given by `p99(i)`, and returns the
/// last frame. `seq` is the phase number of the clock.
fn play(
    plan: PhasePlan,
    finished: Vec<FinishedPhase>,
    basis: &'static str,
    seconds: u64,
    seq: u64,
    p99: impl Fn(u64) -> u64,
) -> DashboardView {
    let script = Script::default();
    let mut dashboard = Dashboard::new(info());
    let mut view = None;
    for tick in 0..=seconds * 4 {
        let now = Duration::from_millis(tick * 250);
        {
            let mut state = script.state.lock().unwrap();
            let messages = tick * 250;
            state.counters = LiveSnapshot {
                active_connections: 48,
                connections_established: 50,
                connections_failed: 2,
                reconnects: 1,
                messages_sent: messages,
                messages_received: messages,
                total_errors: tick / 4,
                ..LiveSnapshot::default()
            };
            state.run_elapsed = now + Duration::from_secs(seq * 30);
            state.clock = Some(PhaseClock {
                seq,
                elapsed: now,
                ended: false,
                interval: Duration::from_secs(1),
                basis,
            });
            let whole = now.as_secs();
            state.last = whole.checked_sub(1).map(|index| CompletedInterval {
                phase: seq,
                index,
                basis,
                latency: latency(950, 1_200, 4_800, p99(index)),
            });
            state.progress = ProgressView {
                current: Some(plan.clone()),
                finished: finished.clone(),
            };
        }
        view = Some(dashboard.update(&script));
    }
    view.expect("at least one frame")
}

fn draw(view: &DashboardView, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render_dashboard(f, view)).unwrap();
    terminal.backend().buffer().clone()
}

fn lines(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

fn text(buffer: &Buffer) -> String {
    lines(buffer).join("\n")
}

/// The foreground color of the first cell of `needle` on the screen.
fn color_of(buffer: &Buffer, needle: &str) -> Option<Color> {
    let width = buffer.area.width as usize;
    for (y, line) in lines(buffer).iter().enumerate() {
        if let Some(byte) = line.find(needle) {
            let x = line[..byte].chars().count();
            return Some(buffer.content[y * width + x].fg);
        }
    }
    None
}

fn single(connections: usize, slo: SloThresholds) -> PhasePlan {
    PhasePlan {
        name: None,
        index: 0,
        total: 1,
        connections,
        ramp_rate: 0,
        duration: Duration::from_secs(60),
        stages: Vec::new(),
        slo,
        run_slo: SloThresholds::default(),
    }
}

#[test]
fn a_plain_echo_run_shows_rtt_percentiles_of_the_last_interval() {
    let view = play(
        single(50, SloThresholds::default()),
        vec![],
        "rtt",
        5,
        0,
        |_| 9_000,
    );
    let screen = draw(&view, 100, 30);
    let screen = text(&screen);
    for expected in [
        "ws://127.0.0.1:9001/",
        " RTT ",
        "p50     1.2ms",
        "p90     4.8ms",
        "p99     9.0ms",
        "950 samples in 1.0s",
        "open    48 / 50 target",
        "no stages",
        "failed  2 handshakes",
        "reconn  1  down 2",
        "msg/s   1000",
        "errors  1",
        "no thresholds set",
        "p99 RTT, max 9.0ms",
        "quit and report",
        "abort",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
    assert!(!screen.contains("Response time"), "{screen}");
    assert!(!screen.contains("Phases"), "{screen}");
    // One p99 point per completed interval.
    assert_eq!(view.p99_history, vec![9_000; 5]);
}

#[test]
fn a_paced_run_labels_response_time_and_colors_a_breaching_threshold() {
    let slo = SloThresholds::new()
        .with_max_p50(Duration::from_millis(5))
        .with_max_p99(Duration::from_millis(25))
        .with_max_error_rate(0.5);
    let view = play(single(50, slo), vec![], "response_time", 5, 0, |_| 40_000);
    let buffer = draw(&view, 100, 30);
    let screen = text(&buffer);
    for expected in [
        " Response time ",
        "p99 response time, max 40.0ms",
        "SLO (live; the final report gates)",
        "rt p50",
        "<= 5.0ms",
        "rt p99",
        "<= 25.0ms",
        "40.0ms",
        "BREACH",
        "<= 50.00%",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
    assert_eq!(color_of(&buffer, "BREACH"), Some(Color::Red));
    assert_eq!(color_of(&buffer, "40.0ms BREACH"), Some(Color::Red));
    assert_eq!(color_of(&buffer, "1.2ms ok"), Some(Color::Green));
}

fn staged_plan() -> PhasePlan {
    PhasePlan {
        stages: vec![
            Stage::new(Duration::from_secs(4), 100),
            Stage::new(Duration::from_secs(20), 100),
            Stage::new(Duration::from_secs(6), 0),
        ],
        duration: Duration::from_secs(30),
        ..single(
            100,
            SloThresholds::new().with_max_p99(Duration::from_millis(25)),
        )
    }
}

#[test]
fn a_staged_run_shows_the_stage_its_target_and_the_time_left() {
    let view = play(staged_plan(), vec![], "rtt", 6, 0, |_| 3_000);
    let screen = text(&draw(&view, 100, 30));
    for expected in [
        "open    48 / 100 target",
        "stage   2/3 -> 100",
        "left    18.0s",
        "reconn  1  down 52",
        "20%  6.0s / 30.0s",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
}

/// The layout to inspect: a paced, staged run. Run with `--nocapture` to
/// print it.
#[test]
fn a_paced_staged_run_renders_every_panel() {
    let view = play(staged_plan(), vec![], "response_time", 14, 0, |index| {
        2_000 + index * 2_500
    });
    for (width, height) in [(100, 30), (80, 24)] {
        let screen = text(&draw(&view, width, height));
        println!("--- {width}x{height} ---\n{screen}");
        for expected in ["Response time", "stage   2/3 -> 100", "rt p99", "BREACH"] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in\n{screen}"
            );
        }
    }
}

#[test]
fn a_scenario_lists_finished_phases_and_shows_the_phase_slos() {
    let plan = PhasePlan {
        name: Some("soak".to_string()),
        index: 2,
        total: 3,
        slo: SloThresholds::new().with_max_p99(Duration::from_millis(50)),
        run_slo: SloThresholds::new().with_max_error_rate(0.01),
        ..single(20, SloThresholds::default())
    };
    let finished = vec![
        FinishedPhase {
            name: "warmup".to_string(),
            passed: true,
            checks: 0,
            failed: vec![],
        },
        FinishedPhase {
            name: "spike".to_string(),
            passed: false,
            checks: 2,
            failed: vec!["Latency p99".to_string()],
        },
    ];
    let view = play(plan, finished, "rtt", 3, 2, |_| 7_000);
    let buffer = draw(&view, 100, 30);
    let screen = text(&buffer);
    for expected in [
        "phase 3/3 soak",
        "Phase SLO (live; the final report gates)",
        "<= 50.0ms",
        " Phases ",
        "PASS warmup (no SLO)",
        "FAIL spike (Latency p99)",
        "NOW  soak",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
    // The scenario's own thresholds are not the phase's.
    assert!(!screen.contains("<= 1.00%"), "{screen}");
    assert_eq!(color_of(&buffer, "FAIL"), Some(Color::Red));
    assert_eq!(color_of(&buffer, "PASS"), Some(Color::Green));
}

#[test]
fn a_phase_without_slos_shows_the_scenario_thresholds() {
    let plan = PhasePlan {
        name: Some("only".to_string()),
        run_slo: SloThresholds::new().with_max_error_rate(0.01),
        ..single(20, SloThresholds::default())
    };
    let view = play(plan, vec![], "rtt", 2, 0, |_| 7_000);
    let screen = text(&draw(&view, 100, 30));
    assert!(screen.contains("Scenario SLO"), "{screen}");
    assert!(screen.contains("<= 1.00%"), "{screen}");
}

#[test]
fn a_phase_whose_clock_has_not_started_shows_no_stale_latency() {
    // Phase 2 is announced while the clock of phase 1 is still registered.
    let plan = PhasePlan {
        name: Some("second".to_string()),
        index: 1,
        total: 2,
        ..staged_plan()
    };
    let view = play(plan, vec![], "rtt", 3, 0, |_| 7_000);
    assert!(view.phase.as_ref().unwrap().starting);
    assert_eq!(view.latency.samples, None);
    assert_eq!(view.stage, None);
    let screen = text(&draw(&view, 100, 30));
    assert!(screen.contains("starting phase"), "{screen}");
    assert!(screen.contains("first interval pending"), "{screen}");
}

#[test]
fn an_80x24_terminal_shows_the_full_layout() {
    let view = play(staged_plan(), vec![], "response_time", 6, 0, |_| 40_000);
    let buffer = draw(&view, 80, 24);
    let screen = text(&buffer);
    for expected in [
        "wsblast",
        "Response time",
        "stage   2/3",
        "rt p99",
        "BREACH",
        "Throughput msg/s",
        "quit and report",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
}

#[test]
fn a_40x10_terminal_shows_the_compact_view() {
    let plan = PhasePlan {
        name: Some("spike".to_string()),
        index: 1,
        total: 2,
        ..staged_plan()
    };
    let finished = vec![FinishedPhase {
        name: "warmup".to_string(),
        passed: false,
        checks: 1,
        failed: vec!["Error Rate".to_string()],
    }];
    let view = play(plan, finished, "response_time", 6, 1, |_| 40_000);
    let buffer = draw(&view, 40, 10);
    let rows = lines(&buffer);
    let screen = rows.join("\n");
    for expected in [
        "phase 2/2 spike",
        "response time p99 40.0ms",
        "open 48/100 stage 2/3",
        "SLO rt p99 BREACH",
        "done warmup FAIL",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in\n{screen}"
        );
    }
    assert!(rows[9].starts_with("[q] quit [^C] abort"), "{screen}");
}

#[test]
fn any_terminal_size_renders_without_panicking() {
    let plan = PhasePlan {
        name: Some("a-phase-with-a-rather-long-name".to_string()),
        index: 0,
        total: 9,
        ..staged_plan()
    };
    let finished: Vec<FinishedPhase> = (0..12)
        .map(|i| FinishedPhase {
            name: format!("phase-{i}"),
            passed: i % 2 == 0,
            checks: 1,
            failed: vec![],
        })
        .collect();
    let view = play(plan, finished, "rtt", 3, 0, |_| 1_500_000);
    for (width, height) in [
        (1, 1),
        (2, 1),
        (10, 3),
        (20, 5),
        (40, 10),
        (71, 21),
        (72, 22),
        (80, 24),
        (120, 40),
        (250, 80),
    ] {
        let _ = draw(&view, width, height);
    }
    // Before the run starts there is nothing but the header.
    let empty = Dashboard::new(info()).update(&Script::default());
    for (width, height) in [(1, 1), (40, 10), (80, 24)] {
        let screen = text(&draw(&empty, width, height));
        assert!(width < 8 || screen.contains("wsblast"), "{screen}");
    }
}
