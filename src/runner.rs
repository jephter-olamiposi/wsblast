//! Test execution orchestrator and worker pool management.

use crate::config::LoadTestConfig;
use crate::metrics::{AggregatedMetrics, LiveMetrics, WorkerMetrics};
use crate::worker::WorkerSession;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Orchestrates the lifecycle of concurrent WebSocket load sessions.
pub struct Runner {
    config: Arc<LoadTestConfig>,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
}

impl Runner {
    pub fn new(config: LoadTestConfig) -> Self {
        Self {
            config: Arc::new(config),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn live_metrics(&self) -> Arc<LiveMetrics> {
        Arc::clone(&self.live_metrics)
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Spawns the worker pool, tracks real-time progress, and aggregates final metrics.
    ///
    /// Cancellation via timeout or interrupt signal gracefully stops all worker loops
    /// and ensures all in-flight session metrics are aggregated without data loss.
    pub async fn run(&self) -> AggregatedMetrics {
        let total_connections = self.config.connections;
        check_file_descriptor_limit(total_connections);

        let start_time = Instant::now();
        let spawn_delay = if self.config.ramp_rate > 0 {
            Some(Duration::from_secs_f64(1.0 / self.config.ramp_rate as f64))
        } else {
            None
        };

        let mut handles = Vec::with_capacity(total_connections);
        for worker_id in 0..total_connections {
            if self.cancel_token.is_cancelled() {
                break;
            }

            let metrics = WorkerMetrics::new(Arc::clone(&self.live_metrics));
            let session = WorkerSession::new(
                worker_id,
                Arc::clone(&self.config),
                self.cancel_token.clone(),
                metrics,
            );

            handles.push(tokio::spawn(session.run()));

            if let Some(delay) = spawn_delay {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = self.cancel_token.cancelled() => break,
                }
            }
        }

        let duration_token = self.cancel_token.clone();
        let test_duration = self.config.duration;
        tokio::spawn(async move {
            tokio::time::sleep(test_duration).await;
            duration_token.cancel();
        });

        if let Some(max_reqs) = self.config.max_requests {
            let req_token = self.cancel_token.clone();
            let live = Arc::clone(&self.live_metrics);
            tokio::spawn(async move {
                while !req_token.is_cancelled() {
                    if live
                        .messages_sent
                        .load(std::sync::atomic::Ordering::Relaxed)
                        >= max_reqs
                    {
                        req_token.cancel();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }

        let sig_token = self.cancel_token.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate()).ok();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        sig_token.cancel();
                    }
                    _ = async {
                        if let Some(ref mut st) = sigterm {
                            st.recv().await;
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        sig_token.cancel();
                    }
                }
            }
            #[cfg(not(unix))]
            {
                if tokio::signal::ctrl_c().await.is_ok() {
                    sig_token.cancel();
                }
            }
        });

        if !self.config.no_progress && !self.config.tui {
            let pb = ProgressBar::new(test_duration.as_millis() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% | Conns: {msg} | Press Ctrl+C to stop")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("#>-"),
            );

            while !self.cancel_token.is_cancelled() {
                let snap = self.live_metrics.snapshot();
                let elapsed = start_time.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;
                pb.set_position(elapsed_ms.min(test_duration.as_millis() as u64));

                let rps = if elapsed.as_secs_f64() > 0.0 {
                    snap.messages_received as f64 / elapsed.as_secs_f64()
                } else {
                    0.0
                };

                pb.set_message(format!(
                    "Active: {} | Msg/s: {:.1} | Recv: {} | Err: {}",
                    snap.active_connections, rps, snap.messages_received, snap.total_errors
                ));

                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            pb.finish_and_clear();
        } else {
            self.cancel_token.cancelled().await;
        }

        let elapsed = start_time.elapsed();

        let mut worker_metrics = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(metrics) = handle.await {
                worker_metrics.push(metrics);
            }
        }

        AggregatedMetrics::merge(worker_metrics, elapsed, total_connections as u64)
    }
}

/// Warns if target concurrency approaches or exceeds the process open file descriptor limit.
fn check_file_descriptor_limit(required_conns: usize) {
    #[cfg(unix)]
    {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // SAFETY: getrlimit is invoked with a valid pointer to stack-allocated rlimit struct.
        let res = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
        if res == 0 {
            let soft_limit = rlim.rlim_cur as usize;
            // 64 descriptors reserved for runtime I/O, event loops, and DNS resolution.
            if required_conns.saturating_add(64) > soft_limit {
                eprintln!(
                    "{} Configured connections ({}) approach or exceed system open file limit (ulimit -n = {}). Consider running 'ulimit -n 65535' to avoid EMFILE socket drops.",
                    colored::Colorize::bold("Warning:").yellow(),
                    required_conns,
                    soft_limit
                );
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = required_conns;
    }
}
