//! What the live dashboard (`--tui`) of a distributed run reads.
//!
//! The coordinator has no workers of its own. Its dashboard reads the same
//! three things a local run's does, filled from the agents: the cluster's
//! live counters (the sum of every agent's latest snapshot), the latency of
//! the last interval merged from the agents' interval histograms (the remote
//! mode of [`LiveIntervals`](crate::timeseries::LiveIntervals)), and the
//! plan and SLO result of each phase. On top of these it shows how many
//! agents run the test and which ones were lost.

use crate::metrics::LiveSnapshot;
use crate::timeseries::{CompletedInterval, PhaseClock};
use crate::tui::source::ProgressView;
use crate::tui::{AgentsView, DashboardSource, RunFeed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The agents of a run and which of them are lost, updated by the
/// coordinator as it loses them.
#[derive(Debug)]
pub(crate) struct AgentBoard {
    names: Vec<String>,
    lost: Mutex<Vec<bool>>,
}

impl AgentBoard {
    pub(crate) fn new(names: Vec<String>) -> Arc<Self> {
        let lost = Mutex::new(vec![false; names.len()]);
        Arc::new(Self { names, lost })
    }

    /// Records that agent `index` is lost.
    pub(crate) fn mark_lost(&self, index: usize) {
        if let Some(lost) = self
            .lost
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(index)
        {
            *lost = true;
        }
    }

    fn view(&self) -> AgentsView {
        let lost = self.lost.lock().unwrap_or_else(|e| e.into_inner());
        AgentsView {
            total: self.names.len(),
            lost: self
                .names
                .iter()
                .zip(lost.iter())
                .filter(|(_, lost)| **lost)
                .map(|(name, _)| name.clone())
                .collect(),
        }
    }
}

/// The [`DashboardSource`] of a coordinator: a [`RunFeed`] of the cluster's
/// live counters, the merged interval latency and the phases, and the
/// agents.
pub(crate) struct ClusterFeed {
    feed: RunFeed,
    board: Arc<AgentBoard>,
}

impl ClusterFeed {
    pub(crate) fn new(feed: RunFeed, board: Arc<AgentBoard>) -> Self {
        Self { feed, board }
    }
}

impl DashboardSource for ClusterFeed {
    fn counters(&self) -> LiveSnapshot {
        self.feed.counters()
    }

    fn run_elapsed(&self) -> Duration {
        self.feed.run_elapsed()
    }

    fn phase_clock(&self) -> Option<PhaseClock> {
        self.feed.phase_clock()
    }

    fn last_interval(&self) -> Option<CompletedInterval> {
        self.feed.last_interval()
    }

    fn progress(&self) -> ProgressView {
        self.feed.progress()
    }

    fn agents(&self) -> Option<AgentsView> {
        Some(self.board.view())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeseries::{IntervalLatency, LiveIntervals};
    use crate::tui::{PhasePlan, RunInfo, RunProgress, render_test_frame};
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn text(buffer: &ratatui::buffer::Buffer) -> String {
        let width = usize::from(buffer.area.width).max(1);
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn info(target: &str) -> RunInfo {
        RunInfo {
            target: target.to_string(),
            mode: "echo".to_string(),
        }
    }

    #[test]
    fn the_header_shows_the_agents_and_the_lost_ones() {
        let live = crate::metrics::LiveMetrics::new();
        let shard = live.shard(0);
        shard.active_connections.store(6, Ordering::Relaxed);
        shard.messages_sent.store(4_000, Ordering::Relaxed);
        shard.messages_received.store(3_990, Ordering::Relaxed);
        let intervals = LiveIntervals::new();
        let start = Instant::now()
            .checked_sub(Duration::from_secs(3))
            .unwrap_or_else(Instant::now);
        intervals.begin_remote_phase(start, Duration::from_secs(1), "rtt");
        intervals.publish_remote(
            1,
            IntervalLatency {
                count: 1_500,
                p50_us: 1_200,
                p90_us: 3_400,
                p99_us: 8_100,
                max_us: 9_000,
            },
        );
        let progress = RunProgress::new();
        progress.begin_phase(PhasePlan {
            index: 0,
            total: 1,
            connections: 9,
            duration: Duration::from_secs(60),
            ..PhasePlan::default()
        });
        let board = AgentBoard::new(vec!["a:1".into(), "b:2".into(), "c:3".into()]);
        let feed = ClusterFeed::new(RunFeed::new(live, intervals, progress), Arc::clone(&board));

        let full = text(&render_test_frame(&feed, info("ws://t/"), 2, 120, 32));
        let header = full.lines().next().unwrap_or_default();
        assert!(header.contains("agents 3"), "{full}");
        assert!(!header.contains("lost"), "{full}");
        assert!(full.contains("8.1ms"), "the merged p99: {full}");

        board.mark_lost(1);
        let full = text(&render_test_frame(&feed, info("ws://t/"), 2, 120, 32));
        let header = full.lines().next().unwrap_or_default();
        assert!(header.contains("agents 3  lost 1: b:2"), "{full}");
        let compact = text(&render_test_frame(&feed, info("ws://t/"), 2, 60, 10));
        assert!(compact.contains("agents 3  lost 1: b:2"), "{compact}");
    }

    #[cfg(feature = "mock")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_coordinator_drives_the_dashboard_and_quitting_it_stops_every_agent() {
        use crate::distributed::{AgentConfig, AgentServer, Coordinator};
        use tokio_util::sync::CancellationToken;

        const TOKEN: &str = "dashboard-token-0123456789";
        let stop = CancellationToken::new();
        let server = crate::mock::MockServer::bind(
            crate::mock::MockConfig::builder()
                .listen("127.0.0.1:0")
                .build()
                .expect("mock config"),
        )
        .await
        .expect("mock server");
        let target = server.url();
        tokio::spawn(server.run(stop.clone()));
        let mut agents = Vec::new();
        for _ in 0..2 {
            let config = AgentConfig::builder()
                .listen("127.0.0.1:0")
                .token(TOKEN)
                .build()
                .expect("agent config");
            let agent = AgentServer::bind(config).await.expect("agent");
            agents.push(agent.local_addr().to_string());
            tokio::spawn(agent.run(stop.clone()));
        }
        let config = crate::LoadTestConfig::builder(&target)
            .connections(4)
            .duration(Duration::from_secs(60))
            .rate_per_connection(40)
            .progress(false)
            .build()
            .expect("config");
        let coordinator = Coordinator::new(config, &agents, TOKEN)
            .expect("coordinator")
            .with_signal_handling(false)
            .with_live_intervals(Some(LiveIntervals::new()))
            .with_progress(RunProgress::new());
        let source = coordinator.dashboard().expect("a dashboard source");
        // What the dashboard's quit keys cancel.
        let quit = coordinator.cancel_token();
        let run = tokio::spawn(async move { coordinator.run().await });

        // The merged latency of an interval reaches the dashboard one
        // second after the interval ends.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(source.counters().messages_received > 0
            && source
                .last_interval()
                .is_some_and(|interval| interval.latency.count > 0))
        {
            assert!(Instant::now() < deadline, "no merged interval in time");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let frame = text(&render_test_frame(&source, info(&target), 2, 120, 32));
        let header = frame.lines().next().unwrap_or_default();
        assert!(header.contains("agents 2"), "{frame}");
        assert!(!header.contains("lost"), "{frame}");
        assert!(frame.contains("response time"), "paced echo: {frame}");
        assert!(source.counters().active_connections > 0, "{frame}");
        let view = source.progress();
        assert_eq!(view.current.map(|plan| plan.connections), Some(4));

        quit.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the agents stopped")
            .expect("the run task")
            .expect("the run reports");
        assert_eq!(outcome.lost_agents().count(), 0);
        assert!(outcome.metrics.elapsed < Duration::from_secs(30));
        assert!(outcome.metrics.total_messages_recv > 0);
        assert!(outcome.agents.iter().all(|agent| {
            agent
                .metrics
                .as_ref()
                .is_some_and(|m| m.total_messages_recv > 0)
        }));
        stop.cancel();
    }
}
