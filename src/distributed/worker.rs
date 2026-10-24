//! Distributed worker node for multi-node WebSocket load testing.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig};
use crate::distributed::proto::wsblast::{
    GetWorkRequest, HeartbeatRequest, LoadTestConfig as ProtoLoadTestConfig, ReportMetricsRequest,
    WorkerMetrics as ProtoWorkerMetrics, WorkerStatus, coordinator_client::CoordinatorClient,
    worker_client::WorkerClient,
};
use crate::distributed::telemetry;
use crate::metrics::{AggregatedMetrics, LiveMetrics};
use crate::runner::Runner;
use std::sync::Arc;
use std::time::Duration;
use tonic::{Request, Status, transport::Channel};
use tracing::{info, warn};

/// Consecutive transport failures tolerated before a worker aborts.
///
/// Covers coordinator restarts and start-up races while still guaranteeing that a
/// worker cannot spin forever against a coordinator that has already finished.
const MAX_CONSECUTIVE_TRANSPORT_ERRORS: u32 = 30;

/// Pause between empty work polls and transport retries.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Progress heartbeat cadence while a worker is executing a range.
///
/// Must stay well below the coordinator's 30-second stale-worker deadline.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Worker node state.
pub struct WorkerNode {
    worker_id: String,
    coordinator_client: CoordinatorClient<Channel>,
    worker_client: WorkerClient<Channel>,
}

impl WorkerNode {
    /// Connects to the coordinator gRPC endpoint.
    pub async fn new(
        worker_id: String,
        coordinator_addr: String,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let channel = Channel::from_shared(coordinator_addr.clone())?
            .connect()
            .await?;

        info!(
            "Worker {} connected to coordinator at {}",
            worker_id, coordinator_addr
        );

        Ok(Self {
            worker_id,
            coordinator_client: CoordinatorClient::new(channel.clone()),
            worker_client: WorkerClient::new(channel),
        })
    }

    /// Claims connection ranges from the coordinator until the test completes.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            worker_id,
            mut coordinator_client,
            mut worker_client,
        } = self;

        let mut consecutive_errors = 0;
        loop {
            match claim_work(&worker_id, &mut worker_client).await {
                Ok(WorkClaim::Work(start, end, config)) => {
                    consecutive_errors = 0;
                    info!(
                        "Worker {} executing connections {}..{}",
                        worker_id, start, end
                    );

                    let live_metrics = Arc::new(LiveMetrics::new());
                    let runner = Runner::new(*config).with_live_metrics(Arc::clone(&live_metrics));

                    // Long ranges must report progress while they run; otherwise
                    // the coordinator would treat a healthy worker as stale.
                    let mut heartbeat_client = coordinator_client.clone();
                    let heartbeat_worker = worker_id.clone();
                    let heartbeat_task = tokio::spawn(async move {
                        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        ticker.tick().await;
                        loop {
                            ticker.tick().await;
                            let snapshot = live_metrics.snapshot();
                            if send_heartbeat(
                                &heartbeat_worker,
                                &mut heartbeat_client,
                                WorkerStatus::Running,
                                0,
                                snapshot.messages_sent,
                                snapshot.messages_received,
                                snapshot.total_errors,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    });

                    let collected = runner.run_and_collect_worker_metrics().await;
                    heartbeat_task.abort();
                    let (worker_metrics, elapsed, attempted) = collected?;
                    #[cfg(feature = "capture")]
                    runner.write_capture();

                    let submitted = telemetry::worker_metrics_to_proto(&worker_id, &worker_metrics);
                    report_metrics(&worker_id, &mut coordinator_client, submitted).await?;

                    let metrics = AggregatedMetrics::merge(worker_metrics, elapsed, attempted)?;
                    let errors = metrics.error_breakdown.values().sum();
                    let completed =
                        u32::try_from(metrics.total_connections_established).unwrap_or(u32::MAX);

                    let should_shutdown = send_heartbeat(
                        &worker_id,
                        &mut coordinator_client,
                        WorkerStatus::Completed,
                        completed,
                        metrics.total_messages_sent,
                        metrics.total_messages_recv,
                        errors,
                    )
                    .await?;

                    if should_shutdown {
                        break;
                    }
                }
                Ok(WorkClaim::Idle) => {
                    consecutive_errors = 0;
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Ok(WorkClaim::Shutdown) => {
                    info!("Coordinator signalled shutdown for worker {}", worker_id);
                    break;
                }
                Err(status) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_TRANSPORT_ERRORS {
                        return Err(format!(
                            "worker {worker_id} lost contact with the coordinator: {status}"
                        )
                        .into());
                    }
                    warn!(
                        "Worker {} could not claim work ({}); retrying",
                        worker_id, status
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }

        info!("Worker {} shut down", worker_id);
        Ok(())
    }
}

/// Outcome of asking the coordinator for work.
enum WorkClaim {
    Work(u32, u32, Box<LoadTestConfig>),
    Idle,
    Shutdown,
}

/// Claims the next connection range from the coordinator.
async fn claim_work(
    worker_id: &str,
    worker: &mut WorkerClient<Channel>,
) -> Result<WorkClaim, Status> {
    let response = worker
        .get_work(Request::new(GetWorkRequest {
            worker_id: worker_id.to_string(),
        }))
        .await?
        .into_inner();

    if response.shutdown {
        return Ok(WorkClaim::Shutdown);
    }
    if !response.has_work {
        return Ok(WorkClaim::Idle);
    }

    let proto_config = response
        .config
        .ok_or_else(|| Status::internal("coordinator returned work without a configuration"))?;
    let config = LoadTestConfig::try_from(proto_config)
        .map_err(|e| Status::internal(format!("invalid load config: {e}")))?;

    Ok(WorkClaim::Work(
        response.start_connection,
        response.end_connection,
        Box::new(config),
    ))
}

/// Submits aggregated metrics for the completed connection range.
async fn report_metrics(
    worker_id: &str,
    coordinator: &mut CoordinatorClient<Channel>,
    worker_metrics: ProtoWorkerMetrics,
) -> Result<(), Status> {
    let response = coordinator
        .report_metrics(Request::new(ReportMetricsRequest {
            worker_id: worker_id.to_string(),
            metrics: Vec::new(),
            worker_metrics: Some(worker_metrics),
        }))
        .await?
        .into_inner();

    if response.success {
        Ok(())
    } else {
        Err(Status::internal(response.message))
    }
}

/// Reports worker progress and returns whether the coordinator wants the worker to stop.
async fn send_heartbeat(
    worker_id: &str,
    coordinator: &mut CoordinatorClient<Channel>,
    status: WorkerStatus,
    connections_completed: u32,
    messages_sent: u64,
    messages_recv: u64,
    errors: u64,
) -> Result<bool, Status> {
    let response = coordinator
        .heartbeat(Request::new(HeartbeatRequest {
            worker_id: worker_id.to_string(),
            status: status as i32,
            connections_completed,
            messages_sent,
            messages_recv,
            errors,
        }))
        .await?
        .into_inner();

    Ok(response.should_shutdown)
}

impl TryFrom<ProtoLoadTestConfig> for LoadTestConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(proto: ProtoLoadTestConfig) -> Result<Self, Self::Error> {
        let target_url = proto.target_url.parse()?;
        let mode = match proto.mode {
            0 => TestMode::Echo,
            1 => TestMode::Stream,
            2 => TestMode::Listen,
            #[cfg(feature = "graphql-ws")]
            3 => TestMode::GraphQLWS,
            other => {
                return Err(format!("unsupported test mode id {other}").into());
            }
        };

        let mut headers = http::HeaderMap::new();
        for (name, value) in proto.headers {
            let name = http::HeaderName::from_bytes(name.as_bytes())?;
            headers.insert(name, http::HeaderValue::from_str(&value)?);
        }

        #[cfg(feature = "graphql-ws")]
        let graphql_ws = if mode == TestMode::GraphQLWS {
            let variables = (!proto.graphql_variables.is_empty())
                .then(|| serde_json::from_str::<serde_json::Value>(&proto.graphql_variables))
                .transpose()?;
            let extensions = (!proto.graphql_extensions.is_empty())
                .then(|| serde_json::from_str::<serde_json::Value>(&proto.graphql_extensions))
                .transpose()?;
            Some(crate::config::GraphQLWSConfig {
                operation_name: (!proto.graphql_operation_name.is_empty())
                    .then_some(proto.graphql_operation_name),
                operation: proto.graphql_operation,
                variables,
                extensions,
            })
        } else {
            None
        };

        #[cfg(feature = "chaos")]
        let chaos = crate::config::ChaosConfig {
            enabled: proto.chaos_enabled,
            latency_ms: (proto.chaos_latency_ms > 0).then_some(proto.chaos_latency_ms),
            loss_percent: (proto.chaos_loss_percent > 0.0).then_some(proto.chaos_loss_percent),
            corrupt_percent: (proto.chaos_corrupt_percent > 0.0)
                .then_some(proto.chaos_corrupt_percent),
            close_percent: (proto.chaos_close_percent > 0.0).then_some(proto.chaos_close_percent),
        };

        Ok(Self {
            target_url,
            connections: proto.connections as usize,
            ramp_rate: proto.ramp_rate,
            duration: Duration::from_millis(proto.duration_ms),
            max_requests: (proto.max_requests > 0).then_some(proto.max_requests),
            rate_per_conn: proto.rate_per_conn,
            payload: PayloadConfig::from_text(proto.payload),
            headers,
            subprotocol: (!proto.subprotocol.is_empty()).then_some(proto.subprotocol),
            mode,
            connect_timeout: Duration::from_millis(proto.connect_timeout_ms),
            message_timeout: Duration::from_millis(proto.message_timeout_ms),
            ping_interval: Duration::from_millis(proto.ping_interval_ms),
            #[cfg(feature = "compression")]
            compression: crate::config::CompressionConfig {
                enabled: false,
                level: 6,
            },
            #[cfg(feature = "auth")]
            auth: None,
            #[cfg(feature = "graphql-ws")]
            graphql_ws,
            #[cfg(feature = "capture")]
            capture: crate::config::CaptureConfig {
                enabled: false,
                output_path: Default::default(),
                format: crate::config::CaptureFormat::PcapNg,
                max_frames: 0,
            },
            #[cfg(feature = "chaos")]
            chaos,
            slo: crate::config::SloThresholds::default(),
            output_format: crate::cli::OutputFormat::Text,
            output_path: None,
            tui: false,
            no_progress: true,
            no_reconnect: proto.no_reconnect,
            reply_match: crate::config::ReplyMatch::default(),
        })
    }
}

/// Runs a worker node that claims work from the configured coordinator.
pub async fn run_worker(
    config: crate::DistributedConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let coordinator_url = config
        .coordinator_url
        .ok_or("--coordinator-url is required in worker mode")?;

    crate::distributed::init_tracing();

    if coordinator_url.scheme() != "http" {
        return Err(format!(
            "coordinator URL must use the http scheme for plaintext gRPC, got '{}'",
            coordinator_url.scheme()
        )
        .into());
    }

    let worker_id = config
        .worker_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let coordinator_addr = coordinator_url.to_string();

    let mut attempt = 0;
    let worker = loop {
        match WorkerNode::new(worker_id.clone(), coordinator_addr.clone()).await {
            Ok(worker) => break worker,
            Err(err) => {
                attempt += 1;
                if attempt >= MAX_CONSECUTIVE_TRANSPORT_ERRORS {
                    return Err(format!(
                        "worker {worker_id} could not connect to coordinator {coordinator_addr}: {err}"
                    )
                    .into());
                }
                warn!(
                    "Worker {} connection attempt {} failed ({}); retrying",
                    worker_id, attempt, err
                );
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    };

    worker.run().await
}
