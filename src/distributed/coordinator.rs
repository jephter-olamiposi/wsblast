//! Distributed coordinator for multi-node WebSocket load testing.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig};
use crate::distributed::proto::wsblast::{
    AssignWorkRequest, AssignWorkResponse, GetWorkRequest, GetWorkResponse, HeartbeatRequest,
    HeartbeatResponse, HistogramData, LoadTestConfig as ProtoLoadTestConfig,
    MergeHistogramsRequest, MergeHistogramsResponse, PushMetricsRequest, PushMetricsResponse,
    ReportMetricsRequest, ReportMetricsResponse, ShutdownRequest, ShutdownResponse,
    TestMode as ProtoTestMode, WorkerMetrics, WorkerStatus,
    coordinator_server::{Coordinator, CoordinatorServer},
    worker_server::{Worker, WorkerServer},
};
use crate::distributed::telemetry;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, mpsc};
use tonic::{Request, Response, Status, transport::Server};
use tracing::{info, warn};

/// Per-worker registration and progress state.
#[derive(Debug, Clone)]
struct WorkerInfo {
    status: WorkerStatus,
    connections_completed: u32,
    messages_sent: u64,
    messages_recv: u64,
    errors: u64,
    last_heartbeat: Instant,
    assigned: Option<(u32, u32, LoadTestConfig)>,
}

impl WorkerInfo {
    fn new() -> Self {
        Self {
            status: WorkerStatus::Idle,
            connections_completed: 0,
            messages_sent: 0,
            messages_recv: 0,
            errors: 0,
            last_heartbeat: Instant::now(),
            assigned: None,
        }
    }
}

/// Shared coordinator state: work queue, worker registry, and collected telemetry.
pub struct CoordinatorState {
    workers: RwLock<HashMap<String, WorkerInfo>>,
    pending_work: Mutex<VecDeque<(u32, u32, LoadTestConfig)>>,
    committed_metrics: Mutex<Vec<WorkerMetrics>>,
    pending_metrics: Mutex<HashMap<String, WorkerMetrics>>,
    histograms: Mutex<Vec<(String, HistogramData)>>,
    shutdown_tx: mpsc::Sender<()>,
    shutdown_rx: Mutex<Option<mpsc::Receiver<()>>>,
    base_config: LoadTestConfig,
    worker_count: usize,
    start_time: Mutex<Option<Instant>>,
    shutdown_scheduled: AtomicBool,
    aborted: AtomicBool,
}

impl CoordinatorState {
    /// Creates coordinator state for the given load configuration.
    ///
    /// `worker_count` determines how many connection ranges the total connection
    /// count is split into; workers claim ranges on demand, so the split only
    /// bounds the granularity of work distribution.
    pub fn new(base_config: LoadTestConfig, worker_count: usize) -> Self {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        Self {
            workers: RwLock::new(HashMap::new()),
            pending_work: Mutex::new(VecDeque::new()),
            committed_metrics: Mutex::new(Vec::new()),
            pending_metrics: Mutex::new(HashMap::new()),
            histograms: Mutex::new(Vec::new()),
            shutdown_tx,
            shutdown_rx: Mutex::new(Some(shutdown_rx)),
            base_config,
            worker_count: worker_count.max(1),
            start_time: Mutex::new(None),
            shutdown_scheduled: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
        }
    }

    /// Splits the total connection budget into contiguous ranges ready for claiming.
    pub async fn start_test(&self) {
        let total = u32::try_from(self.base_config.connections).unwrap_or(u32::MAX);
        let worker_slots = u32::try_from(self.worker_count).unwrap_or(u32::MAX).max(1);
        let chunk = total.div_ceil(worker_slots).max(1);
        let range_count = total.div_ceil(chunk).max(1);

        let ranges: Vec<(u32, u32)> = (0..range_count)
            .map(|index| {
                let start = index.saturating_mul(chunk);
                (start, start.saturating_add(chunk).min(total))
            })
            .collect();

        // Budgets are divided across the ranges that will actually run, not
        // across configured worker slots; extra workers stay idle without
        // changing the total request cap.
        let range_count = u64::from(range_count);
        let ramp_total = self.base_config.ramp_rate;
        let request_total = self.base_config.max_requests;

        let mut pending = self.pending_work.lock().await;
        for (index, (start, end)) in ranges.into_iter().enumerate() {
            let index = index as u64;
            let mut config = self.base_config.clone();
            config.connections = usize::try_from(end - start).unwrap_or(usize::MAX);
            config.ramp_rate = if ramp_total == 0 {
                0
            } else {
                let base = ramp_total / range_count;
                let remainder = ramp_total % range_count;
                (base + u64::from(index < remainder)).max(1)
            };
            config.max_requests = request_total.map(|requests| {
                let base = requests / range_count;
                let remainder = requests % range_count;
                base + u64::from(index < remainder)
            });
            pending.push_back((start, end, config));
        }
        info!(
            "Queued {} connection range(s) across {} worker slot(s)",
            pending.len(),
            worker_slots
        );
    }

    /// Registers a worker on first contact without resetting an existing registration.
    pub async fn register_worker(&self, worker_id: &str) {
        let mut workers = self.workers.write().await;
        workers
            .entry(worker_id.to_string())
            .or_insert_with(WorkerInfo::new);
    }

    /// Claims the next connection range for a worker and marks it as running.
    pub async fn assign_work(&self, worker_id: &str) -> Option<(u32, u32, LoadTestConfig)> {
        let chunk = self.pending_work.lock().await.pop_front();

        // The elapsed wall clock drives throughput and must start with the first
        // claimed range, not with coordinator start-up while waiting for workers.
        if chunk.is_some() {
            let mut start_time = self.start_time.lock().await;
            start_time.get_or_insert_with(Instant::now);
        }

        let mut workers = self.workers.write().await;
        if let Some(worker) = workers.get_mut(worker_id) {
            // Empty polls are contact too; idle workers must not look stale.
            worker.last_heartbeat = Instant::now();
            if let Some(chunk) = &chunk {
                worker.status = WorkerStatus::Running;
                worker.assigned = Some(chunk.clone());
            }
        }
        chunk
    }

    /// Queues a connection range supplied by an external orchestrator.
    pub async fn inject_work(&self, start: u32, end: u32, config: LoadTestConfig) {
        self.pending_work
            .lock()
            .await
            .push_back((start, end, config));
    }

    /// Stages worker metrics until the matching completion heartbeat arrives.
    ///
    /// Staging keeps a crashed worker's partial measurements out of the final
    /// report: its range is requeued, and only the rerun's metrics are committed.
    pub async fn report_metrics(&self, worker_id: &str, metrics: WorkerMetrics) {
        self.pending_metrics
            .lock()
            .await
            .insert(worker_id.to_string(), metrics);
    }

    /// Stores worker-submitted latency histograms for later merging.
    pub async fn record_histograms(&self, worker_id: &str, histograms: Vec<HistogramData>) {
        let mut stored = self.histograms.lock().await;
        stored.extend(
            histograms
                .into_iter()
                .map(|histogram| (worker_id.to_string(), histogram)),
        );
    }

    /// Updates worker progress and requeues assignments abandoned by failed workers.
    ///
    /// Returns `true` once every queued and in-flight connection range has finished,
    /// signalling that workers and the coordinator can shut down.
    pub async fn heartbeat(
        &self,
        worker_id: &str,
        status: WorkerStatus,
        connections_completed: u32,
        messages_sent: u64,
        messages_recv: u64,
        errors: u64,
    ) -> bool {
        let mut abandoned = None;
        let mut completion = false;
        {
            let mut workers = self.workers.write().await;
            if let Some(worker) = workers.get_mut(worker_id) {
                worker.status = status;
                worker.connections_completed = connections_completed;
                worker.messages_sent = messages_sent;
                worker.messages_recv = messages_recv;
                worker.errors = errors;
                worker.last_heartbeat = Instant::now();

                match status {
                    WorkerStatus::Completed => {
                        worker.assigned = None;
                        completion = true;
                    }
                    WorkerStatus::Failed => {
                        abandoned = worker.assigned.take();
                        worker.status = WorkerStatus::Idle;
                        worker.connections_completed = 0;
                    }
                    WorkerStatus::Unspecified | WorkerStatus::Idle | WorkerStatus::Running => {}
                }
            }
        }

        let staged = self.pending_metrics.lock().await.remove(worker_id);
        if completion {
            if let Some(metrics) = staged {
                self.committed_metrics.lock().await.push(metrics);
            }
        }

        if let Some((start, end, config)) = abandoned {
            info!(
                "Worker {} failed connections {}-{}; requeueing",
                worker_id, start, end
            );
            self.pending_work
                .lock()
                .await
                .push_front((start, end, config));
        }

        self.all_work_completed().await
    }

    /// Returns `true` when no work is queued and no worker has an in-flight range.
    pub async fn all_work_completed(&self) -> bool {
        if !self.pending_work.lock().await.is_empty() {
            return false;
        }
        self.workers
            .read()
            .await
            .values()
            .all(|worker| worker.assigned.is_none())
    }

    /// Requeues ranges owned by workers that stopped reporting heartbeats.
    ///
    /// Returns the number of ranges requeued. The failed range is retried by any
    /// live worker instead of leaving the coordinator waiting forever.
    pub async fn requeue_stale_workers(&self, timeout: Duration) -> usize {
        let mut requeued = Vec::new();
        {
            let mut workers = self.workers.write().await;
            for (worker_id, worker) in workers.iter_mut() {
                if worker.assigned.is_some() && worker.last_heartbeat.elapsed() > timeout {
                    if let Some(assignment) = worker.assigned.take() {
                        worker.status = WorkerStatus::Idle;
                        worker.connections_completed = 0;
                        requeued.push((worker_id.clone(), assignment));
                    }
                }
            }
        }

        let count = requeued.len();
        if count > 0 {
            let mut pending = self.pending_work.lock().await;
            for (worker_id, (start, end, config)) in requeued {
                warn!(
                    "Worker {} missed the heartbeat deadline; requeueing connections {}-{}",
                    worker_id, start, end
                );
                pending.push_back((start, end, config));
            }
        }
        count
    }

    /// Returns `true` when at least one worker contacted the coordinator recently.
    pub async fn has_live_worker(&self, timeout: Duration) -> bool {
        self.workers
            .read()
            .await
            .values()
            .any(|worker| worker.last_heartbeat.elapsed() <= timeout)
    }

    /// Schedules a graceful shutdown after a short grace period.
    ///
    /// The delay lets idle workers poll `GetWork` and observe the shutdown flag
    /// instead of failing against a closed gRPC server.
    pub fn schedule_shutdown(self: &Arc<Self>) {
        if self.shutdown_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.request_shutdown().await;
        });
    }

    /// Marks the run as aborted because no live workers remain.
    pub fn mark_aborted(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }

    /// Returns whether the coordinator aborted the run.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Signals the gRPC server to stop accepting connections.
    pub async fn request_shutdown(&self) {
        let _ = self.shutdown_tx.try_send(());
    }

    /// Takes ownership of the shutdown receiver; it can only be consumed once.
    pub async fn take_shutdown_receiver(&self) -> Option<mpsc::Receiver<()>> {
        self.shutdown_rx.lock().await.take()
    }

    /// Snapshot of all committed worker metrics.
    pub async fn get_aggregated_metrics(&self) -> Vec<WorkerMetrics> {
        self.committed_metrics.lock().await.clone()
    }

    /// Snapshot of all histograms received so far.
    pub async fn get_histograms(&self) -> Vec<(String, HistogramData)> {
        self.histograms.lock().await.clone()
    }

    /// Aggregates worker telemetry, emits the run report, and returns SLO compliance.
    ///
    /// Returns `true` when no worker reported metrics (an empty run cannot breach an
    /// SLO) or when every configured threshold passed.
    pub async fn finalize(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let submitted = self.get_aggregated_metrics().await;
        if submitted.is_empty() {
            warn!("No worker metrics received; skipping report");
            return Ok(true);
        }

        let mut workers = Vec::with_capacity(submitted.len());
        for proto in &submitted {
            workers.push(
                telemetry::proto_to_worker_metrics(proto)
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?,
            );
        }

        let elapsed = self.elapsed().await;
        let attempted = u64::try_from(self.base_config.connections).unwrap_or(u64::MAX);
        let metrics = crate::metrics::AggregatedMetrics::merge(workers, elapsed, attempted)?;
        let slo = crate::report::evaluate_slos(&self.base_config.slo, &metrics);
        crate::report::emit_report(&self.base_config, &metrics, &slo)?;

        Ok(slo.passed)
    }

    async fn elapsed(&self) -> Duration {
        self.start_time
            .lock()
            .await
            .map(|started| started.elapsed())
            .unwrap_or_default()
    }
}

/// Coordinator gRPC service implementation.
#[derive(Clone)]
pub struct CoordinatorService {
    state: Arc<CoordinatorState>,
}

impl CoordinatorService {
    pub fn new(state: Arc<CoordinatorState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl Coordinator for CoordinatorService {
    async fn assign_work(
        &self,
        request: Request<AssignWorkRequest>,
    ) -> Result<Response<AssignWorkResponse>, Status> {
        let req = request.into_inner();
        let Some(proto_config) = req.config else {
            return Ok(Response::new(AssignWorkResponse {
                success: false,
                message: "AssignWork requires a config; workers claim ranges via Worker.GetWork"
                    .to_string(),
            }));
        };

        if req.end_connection <= req.start_connection {
            return Err(Status::invalid_argument(
                "end_connection must be greater than start_connection",
            ));
        }

        let config = LoadTestConfig::try_from(proto_config)
            .map_err(|e| Status::invalid_argument(format!("invalid load config: {e}")))?;

        self.state
            .inject_work(req.start_connection, req.end_connection, config)
            .await;
        info!(
            "Queued externally assigned connections {}-{}",
            req.start_connection, req.end_connection
        );

        Ok(Response::new(AssignWorkResponse {
            success: true,
            message: format!(
                "Queued connections {}-{}",
                req.start_connection, req.end_connection
            ),
        }))
    }

    async fn report_metrics(
        &self,
        request: Request<ReportMetricsRequest>,
    ) -> Result<Response<ReportMetricsResponse>, Status> {
        let req = request.into_inner();
        let worker_id = req.worker_id;
        self.state.record_histograms(&worker_id, req.metrics).await;
        if let Some(metrics) = req.worker_metrics {
            self.state.report_metrics(&worker_id, metrics).await;
        }

        Ok(Response::new(ReportMetricsResponse {
            success: true,
            message: "Metrics recorded".to_string(),
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        let should_shutdown = self
            .state
            .heartbeat(
                &req.worker_id,
                req.status(),
                req.connections_completed,
                req.messages_sent,
                req.messages_recv,
                req.errors,
            )
            .await;

        if should_shutdown {
            self.state.schedule_shutdown();
        }

        Ok(Response::new(HeartbeatResponse {
            success: true,
            should_shutdown,
            message: if should_shutdown {
                "Load test complete".to_string()
            } else {
                "OK".to_string()
            },
        }))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let req = request.into_inner();
        info!("Shutdown requested: {}", req.reason);
        self.state.request_shutdown().await;
        Ok(Response::new(ShutdownResponse {
            success: true,
            message: "Shutdown initiated".to_string(),
        }))
    }

    async fn merge_histograms(
        &self,
        request: Request<MergeHistogramsRequest>,
    ) -> Result<Response<MergeHistogramsResponse>, Status> {
        let histograms = request.into_inner().histograms;
        let merged =
            telemetry::merge_histogram_data(&histograms).map_err(Status::invalid_argument)?;
        Ok(Response::new(MergeHistogramsResponse {
            merged_histogram: merged,
        }))
    }
}

#[tonic::async_trait]
impl Worker for CoordinatorService {
    async fn get_work(
        &self,
        request: Request<GetWorkRequest>,
    ) -> Result<Response<GetWorkResponse>, Status> {
        let worker_id = request.into_inner().worker_id;
        if worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        self.state.register_worker(&worker_id).await;

        match self.state.assign_work(&worker_id).await {
            Some((start, end, config)) => {
                info!(
                    "Assigned connections {}-{} to worker {}",
                    start, end, worker_id
                );
                let proto_config = ProtoLoadTestConfig::try_from(config)
                    .map_err(|e| Status::internal(format!("failed to encode load config: {e}")))?;
                Ok(Response::new(GetWorkResponse {
                    start_connection: start,
                    end_connection: end,
                    has_work: true,
                    config: Some(proto_config),
                    shutdown: false,
                }))
            }
            None => {
                let shutdown = self.state.all_work_completed().await;
                Ok(Response::new(GetWorkResponse {
                    has_work: false,
                    shutdown,
                    ..Default::default()
                }))
            }
        }
    }

    async fn push_metrics(
        &self,
        request: Request<PushMetricsRequest>,
    ) -> Result<Response<PushMetricsResponse>, Status> {
        let req = request.into_inner();
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }
        self.state
            .record_histograms(&req.worker_id, req.metrics)
            .await;
        Ok(Response::new(PushMetricsResponse { success: true }))
    }
}

impl TryFrom<LoadTestConfig> for ProtoLoadTestConfig {
    type Error = String;

    fn try_from(config: LoadTestConfig) -> Result<Self, Self::Error> {
        let payload = match &config.payload {
            PayloadConfig::StaticText(text) => text.as_str().to_string(),
            PayloadConfig::DynamicText(text) => text.clone(),
            PayloadConfig::Binary(_) => {
                return Err("binary payloads are not supported in distributed mode".to_string());
            }
        };

        let mut headers = HashMap::with_capacity(config.headers.len());
        for (name, value) in &config.headers {
            let value = value.to_str().map_err(|_| {
                format!("header '{name}' contains a non-UTF-8 value unsupported by the wire config")
            })?;
            headers.insert(name.as_str().to_string(), value.to_string());
        }

        #[cfg(feature = "graphql-ws")]
        let graphql = config.graphql_ws.as_ref();
        #[cfg(feature = "graphql-ws")]
        let graphql_operation = graphql
            .map(|value| value.operation.clone())
            .unwrap_or_default();
        #[cfg(feature = "graphql-ws")]
        let graphql_operation_name = graphql
            .and_then(|value| value.operation_name.clone())
            .unwrap_or_default();
        #[cfg(feature = "graphql-ws")]
        let graphql_variables = graphql
            .and_then(|value| value.variables.as_ref())
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("failed to serialize GraphQL variables: {e}"))?
            .unwrap_or_default();
        #[cfg(feature = "graphql-ws")]
        let graphql_extensions = graphql
            .and_then(|value| value.extensions.as_ref())
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("failed to serialize GraphQL extensions: {e}"))?
            .unwrap_or_default();
        #[cfg(not(feature = "graphql-ws"))]
        let (graphql_operation, graphql_operation_name, graphql_variables, graphql_extensions) =
            (String::new(), String::new(), String::new(), String::new());

        #[cfg(feature = "chaos")]
        let (
            chaos_enabled,
            chaos_latency_ms,
            chaos_loss_percent,
            chaos_corrupt_percent,
            chaos_close_percent,
        ) = (
            config.chaos.enabled,
            config.chaos.latency_ms.unwrap_or(0),
            config.chaos.loss_percent.unwrap_or(0.0),
            config.chaos.corrupt_percent.unwrap_or(0.0),
            config.chaos.close_percent.unwrap_or(0.0),
        );
        #[cfg(not(feature = "chaos"))]
        let (
            chaos_enabled,
            chaos_latency_ms,
            chaos_loss_percent,
            chaos_corrupt_percent,
            chaos_close_percent,
        ) = (false, 0, 0.0, 0.0, 0.0);

        let mode = match config.mode {
            TestMode::Echo => ProtoTestMode::Echo,
            TestMode::Stream => ProtoTestMode::Stream,
            TestMode::Listen => ProtoTestMode::Listen,
            #[cfg(feature = "graphql-ws")]
            TestMode::GraphQLWS => ProtoTestMode::GraphqlWs,
        };

        Ok(Self {
            target_url: config.target_url.to_string(),
            connections: u32::try_from(config.connections).unwrap_or(u32::MAX),
            ramp_rate: config.ramp_rate,
            duration_ms: config.duration.as_millis() as u64,
            max_requests: config.max_requests.unwrap_or(0),
            rate_per_conn: config.rate_per_conn,
            payload,
            headers,
            subprotocol: config.subprotocol.unwrap_or_default(),
            mode: mode as i32,
            connect_timeout_ms: config.connect_timeout.as_millis() as u64,
            message_timeout_ms: config.message_timeout.as_millis() as u64,
            ping_interval_ms: config.ping_interval.as_millis() as u64,
            graphql_operation,
            graphql_operation_name,
            graphql_variables,
            graphql_extensions,
            chaos_enabled,
            chaos_latency_ms,
            chaos_loss_percent,
            chaos_corrupt_percent,
            chaos_close_percent,
        })
    }
}

/// Runs the coordinator gRPC server until the load test completes or a shutdown is requested.
///
/// Returns whether every configured SLO threshold passed.
pub async fn run_coordinator(
    config: crate::DistributedConfig,
    load_config: LoadTestConfig,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    const WORKER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
    const HEARTBEAT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

    let addr = config.coordinator_bind_addr.parse()?;
    crate::distributed::init_tracing();
    let state = Arc::new(CoordinatorState::new(load_config, config.worker_count));
    state.start_test().await;

    let monitor_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEARTBEAT_SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let requeued = monitor_state
                .requeue_stale_workers(WORKER_HEARTBEAT_TIMEOUT)
                .await;
            if requeued > 0
                && !monitor_state
                    .has_live_worker(WORKER_HEARTBEAT_TIMEOUT)
                    .await
            {
                warn!("No live workers remain; aborting the distributed run");
                monitor_state.mark_aborted();
                monitor_state.request_shutdown().await;
                break;
            }
            if monitor_state.all_work_completed().await {
                break;
            }
        }
    });

    let mut shutdown_rx = state
        .take_shutdown_receiver()
        .await
        .ok_or("coordinator shutdown channel already consumed")?;
    let service = CoordinatorService::new(Arc::clone(&state));

    info!("Starting coordinator gRPC server on {}", addr);

    Server::builder()
        .add_service(CoordinatorServer::new(service.clone()))
        .add_service(WorkerServer::new(service))
        .serve_with_shutdown(addr, async move {
            let _ = shutdown_rx.recv().await;
        })
        .await?;

    info!("Coordinator stopped");
    let slo_passed = state.finalize().await?;
    if state.is_aborted() {
        return Err("distributed run aborted: no live workers remained".into());
    }
    Ok(slo_passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distributed::proto::wsblast::TestMode as ProtoTestMode;

    fn base_config(connections: usize) -> LoadTestConfig {
        LoadTestConfig::try_from(ProtoLoadTestConfig {
            target_url: "ws://127.0.0.1:9001".to_string(),
            connections: u32::try_from(connections).expect("small connection count"),
            duration_ms: 1_000,
            rate_per_conn: 10,
            mode: ProtoTestMode::Echo as i32,
            ..Default::default()
        })
        .expect("valid prototype config")
    }

    #[tokio::test]
    async fn splits_connection_budget_across_worker_slots() {
        let state = CoordinatorState::new(base_config(5), 2);
        state.start_test().await;
        assert!(!state.all_work_completed().await);

        state.register_worker("w1").await;
        state.register_worker("w2").await;

        let (start, end, range) = state.assign_work("w1").await.expect("first range");
        assert_eq!((start, end), (0, 3));
        assert_eq!(range.connections, 3);
        assert_eq!(range.ramp_rate, 0);

        let (start, end, _) = state.assign_work("w2").await.expect("second range");
        assert_eq!((start, end), (3, 5));

        assert!(state.assign_work("w3").await.is_none());
    }

    #[tokio::test]
    async fn requeues_ranges_abandoned_by_failed_workers() {
        let state = CoordinatorState::new(base_config(4), 1);
        state.start_test().await;
        state.register_worker("w1").await;

        let (start, end, _) = state.assign_work("w1").await.expect("range");
        assert_eq!((start, end), (0, 4));

        let shutdown = state
            .heartbeat("w1", WorkerStatus::Failed, 0, 0, 0, 1)
            .await;
        assert!(!shutdown);

        let retried = state.assign_work("w1").await.expect("requeued range");
        assert_eq!((retried.0, retried.1), (0, 4));
    }

    #[tokio::test]
    async fn signals_shutdown_once_every_range_completes() {
        let state = CoordinatorState::new(base_config(4), 2);
        state.start_test().await;
        state.register_worker("w1").await;
        state.register_worker("w2").await;

        let _ = state.assign_work("w1").await.expect("first range");
        let _ = state.assign_work("w2").await.expect("second range");

        assert!(
            !state
                .heartbeat("w1", WorkerStatus::Completed, 2, 0, 0, 0)
                .await
        );
        assert!(
            state
                .heartbeat("w2", WorkerStatus::Completed, 2, 0, 0, 0)
                .await
        );
    }

    #[tokio::test]
    async fn discards_staged_metrics_when_a_worker_fails() {
        let state = CoordinatorState::new(base_config(1), 1);
        state.start_test().await;
        state.register_worker("w1").await;
        let _ = state.assign_work("w1").await.expect("range");

        let metrics = WorkerMetrics {
            worker_id: "w1".to_string(),
            ..Default::default()
        };
        state.report_metrics("w1", metrics.clone()).await;
        state
            .heartbeat("w1", WorkerStatus::Failed, 0, 0, 0, 1)
            .await;
        assert!(state.get_aggregated_metrics().await.is_empty());

        let _ = state.assign_work("w1").await.expect("requeued range");
        state.report_metrics("w1", metrics).await;
        state
            .heartbeat("w1", WorkerStatus::Completed, 1, 0, 0, 0)
            .await;
        assert_eq!(state.get_aggregated_metrics().await.len(), 1);
    }

    #[test]
    fn rejects_binary_payloads_in_wire_config() {
        let mut config = base_config(1);
        config.payload = PayloadConfig::Binary(bytes::Bytes::from_static(b"binary"));

        assert!(ProtoLoadTestConfig::try_from(config).is_err());
    }

    #[test]
    fn wire_config_round_trips_text_payload_and_headers() {
        let mut config = base_config(3);
        config.payload = PayloadConfig::from_text(r#"{"seq":{{seq}}}"#.to_string());
        config
            .headers
            .insert("x-test", http::HeaderValue::from_static("value"));

        let proto = ProtoLoadTestConfig::try_from(config.clone()).expect("encodable config");
        assert_eq!(proto.connections, 3);
        assert_eq!(proto.duration_ms, 1_000);
        assert_eq!(
            proto.headers.get("x-test").map(String::as_str),
            Some("value")
        );

        let restored = LoadTestConfig::try_from(proto).expect("decodable config");
        assert_eq!(restored.connections, config.connections);
        assert!(matches!(
            &restored.payload,
            PayloadConfig::DynamicText(text) if text == r#"{"seq":{{seq}}}"#
        ));
        assert_eq!(restored.max_requests, config.max_requests);
    }

    #[cfg(feature = "graphql-ws")]
    #[test]
    fn wire_config_round_trips_graphql_options() {
        let mut config = base_config(1);
        config.mode = TestMode::GraphQLWS;
        config.graphql_ws = Some(crate::config::GraphQLWSConfig {
            operation_name: Some("Prices".to_string()),
            operation: "subscription Prices { prices }".to_string(),
            variables: Some(serde_json::json!({"symbol": "BTC"})),
            extensions: Some(serde_json::json!({"persisted": true})),
        });

        let proto = ProtoLoadTestConfig::try_from(config.clone()).expect("encodable config");
        assert_eq!(proto.graphql_operation, "subscription Prices { prices }");
        assert_eq!(proto.graphql_operation_name, "Prices");
        assert!(proto.graphql_variables.contains("BTC"));

        let restored = LoadTestConfig::try_from(proto).expect("decodable config");
        let graphql = restored.graphql_ws.expect("graphql config");
        assert_eq!(
            graphql.operation,
            config.graphql_ws.clone().unwrap().operation
        );
        assert_eq!(graphql.operation_name.as_deref(), Some("Prices"));
        assert_eq!(
            graphql.variables,
            Some(serde_json::json!({"symbol": "BTC"}))
        );
    }

    #[cfg(feature = "chaos")]
    #[test]
    fn wire_config_round_trips_chaos_options() {
        let mut config = base_config(1);
        config.chaos = crate::config::ChaosConfig {
            enabled: true,
            latency_ms: Some(25),
            loss_percent: Some(1.5),
            corrupt_percent: Some(2.0),
            close_percent: Some(0.5),
        };

        let proto = ProtoLoadTestConfig::try_from(config.clone()).expect("encodable config");
        assert!(proto.chaos_enabled);
        assert_eq!(proto.chaos_latency_ms, 25);

        let restored = LoadTestConfig::try_from(proto).expect("decodable config");
        assert!(restored.chaos.enabled);
        assert_eq!(restored.chaos.latency_ms, Some(25));
        assert_eq!(restored.chaos.loss_percent, Some(1.5));
        assert_eq!(restored.chaos.corrupt_percent, Some(2.0));
        assert_eq!(restored.chaos.close_percent, Some(0.5));
    }

    #[tokio::test]
    async fn distributes_request_budget_across_actual_ranges() {
        let mut config = base_config(10);
        config.max_requests = Some(5);
        let state = CoordinatorState::new(config, 4);
        state.start_test().await;
        state.register_worker("w1").await;

        let mut ranges = Vec::new();
        while let Some((start, end, range)) = state.assign_work("w1").await {
            ranges.push((start, end, range.max_requests));
        }

        assert_eq!(ranges.len(), 4);
        assert_eq!(
            ranges
                .iter()
                .map(|(start, end, _)| end - start)
                .sum::<u32>(),
            10
        );
        let total_requests: u64 = ranges
            .iter()
            .map(|(_, _, requests)| requests.unwrap_or(0))
            .sum();
        assert_eq!(total_requests, 5);
        assert!(
            ranges
                .iter()
                .all(|(_, _, requests)| requests.is_some_and(|value| value <= 2))
        );
    }

    #[tokio::test]
    async fn requeues_ranges_from_stale_workers() {
        let state = Arc::new(CoordinatorState::new(base_config(4), 1));
        state.start_test().await;
        state.register_worker("w1").await;
        let _ = state.assign_work("w1").await.expect("range");

        {
            let mut workers = state.workers.write().await;
            let worker = workers.get_mut("w1").expect("worker registered");
            worker.last_heartbeat = Instant::now() - Duration::from_secs(60);
        }

        assert_eq!(
            state.requeue_stale_workers(Duration::from_secs(30)).await,
            1
        );
        assert!(!state.all_work_completed().await);

        let retried = state.assign_work("w1").await.expect("requeued range");
        assert_eq!((retried.0, retried.1), (0, 4));
    }

    #[tokio::test]
    async fn fresh_worker_progress_is_not_requeued() {
        let state = Arc::new(CoordinatorState::new(base_config(4), 1));
        state.start_test().await;
        state.register_worker("w1").await;
        let _ = state.assign_work("w1").await.expect("range");

        assert_eq!(
            state.requeue_stale_workers(Duration::from_secs(30)).await,
            0
        );
        assert!(state.has_live_worker(Duration::from_secs(30)).await);
    }
}
