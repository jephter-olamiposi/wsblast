//! The load agent behind `wsblast agent`: accepts one authenticated
//! coordinator at a time and runs its share of a test.

use super::protocol::{
    Message, ResultMeta, WireInterval, WireResult, bearer_token, check_protocol, to_sparse,
    tokens_match, validate_token,
};
use super::{HANDSHAKE_TIMEOUT, SETUP_TIMEOUT, ws_config};
use crate::error::{ConfigError, Result};
use crate::metrics::{LiveMetrics, MetricsTotals};
use crate::runner::Runner;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_util::sync::CancellationToken;

/// How long [`AgentServer::run`] waits on shutdown for a running test to
/// send its result.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How long past the coordinator's barrier timeout a ready agent waits for
/// `start` before it gives up on the test.
const START_SLACK: Duration = Duration::from_secs(10);

/// A validated agent configuration, built with [`AgentConfig::builder`].
#[derive(Clone)]
pub struct AgentConfig {
    listen: String,
    token: String,
    tls: Option<(PathBuf, PathBuf)>,
    ready_delay: Duration,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("listen", &self.listen)
            .field("token", &"<redacted>")
            .field("tls", &self.tls)
            .finish()
    }
}

impl AgentConfig {
    /// Starts a configuration. A token is required.
    pub fn builder() -> AgentConfigBuilder {
        AgentConfigBuilder::default()
    }

    /// The address to listen on, as given.
    pub fn listen(&self) -> &str {
        &self.listen
    }
}

/// Builder for [`AgentConfig`].
#[derive(Clone, Default)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct AgentConfigBuilder {
    listen: Option<String>,
    token: Option<String>,
    tls: Option<(PathBuf, PathBuf)>,
    ready_delay: Duration,
}

impl std::fmt::Debug for AgentConfigBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfigBuilder")
            .field("listen", &self.listen)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl AgentConfigBuilder {
    /// Sets the `host:port` to listen on. Port 0 picks a free port; read it
    /// back with [`AgentServer::local_addr`].
    pub fn listen(mut self, addr: impl Into<String>) -> Self {
        self.listen = Some(addr.into());
        self
    }

    /// Sets the token a coordinator must present as
    /// `Authorization: Bearer <token>`. At least 16 characters.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Serves the control channel over TLS (`wss://`) with the PEM
    /// certificate chain and private key in these files.
    pub fn tls(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        self.tls = Some((cert.into(), key.into()));
        self
    }

    /// Delays the `ready` reply, to test the coordinator's start barrier.
    #[doc(hidden)]
    pub fn ready_delay(mut self, delay: Duration) -> Self {
        self.ready_delay = delay;
        self
    }

    /// Validates the settings.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] when the listen address or the
    /// token is missing, or the token is shorter than
    /// [`MIN_TOKEN_LEN`](super::MIN_TOKEN_LEN) characters.
    pub fn build(self) -> Result<AgentConfig> {
        let listen = self
            .listen
            .filter(|l| !l.trim().is_empty())
            .ok_or_else(|| {
                ConfigError::InvalidOption("wsblast agent needs --listen ADDR".to_string())
            })?;
        let token = self.token.filter(|t| !t.is_empty()).ok_or_else(|| {
            ConfigError::InvalidOption(format!(
                "wsblast agent needs a token: pass --token or set {}",
                super::TOKEN_ENV
            ))
        })?;
        validate_token(&token)?;
        Ok(AgentConfig {
            listen,
            token,
            tls: self.tls,
            ready_delay: self.ready_delay,
        })
    }
}

/// A bound load agent. [`AgentServer::run`] serves coordinators until it is
/// cancelled.
pub struct AgentServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: Option<TlsAcceptor>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AgentServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentServer")
            .field("local_addr", &self.local_addr)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

struct Shared {
    token: String,
    /// Set while a coordinator holds the agent.
    busy: AtomicBool,
    ready_delay: Duration,
}

/// Releases the agent for the next coordinator.
struct BusyGuard(Arc<Shared>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

impl AgentServer {
    /// Loads the TLS files, if any, and binds the listen address.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for unreadable or invalid TLS files,
    /// and [`WsBlastError::Io`](crate::WsBlastError::Io) when the address
    /// cannot be bound.
    pub async fn bind(config: AgentConfig) -> Result<Self> {
        let tls = match &config.tls {
            Some((cert, key)) => Some(TlsAcceptor::from(super::tls::server_config(cert, key)?)),
            None => None,
        };
        let listener = TcpListener::bind(config.listen.as_str())
            .await
            .map_err(|err| {
                std::io::Error::new(
                    err.kind(),
                    format!("cannot listen on {}: {err}", config.listen),
                )
            })?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
            tls,
            shared: Arc::new(Shared {
                token: config.token,
                busy: AtomicBool::new(false),
                ready_delay: config.ready_delay,
            }),
        })
    }

    /// The address the agent is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The agent's control URL: `ws://` or, with TLS, `wss://`.
    pub fn url(&self) -> String {
        let scheme = if self.tls.is_some() { "wss" } else { "ws" };
        format!("{scheme}://{}", self.local_addr)
    }

    /// Serves coordinators until `cancel` is cancelled. A test in progress
    /// is then stopped and still sends its result, for up to 10 seconds.
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok`; a failed control connection is not an
    /// error of the agent.
    pub async fn run(self, cancel: CancellationToken) -> Result<()> {
        let mut sessions = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            while sessions.try_join_next().is_some() {}
            let Ok((tcp, peer)) = accepted else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let _ = tcp.set_nodelay(true);
            sessions.spawn(serve(
                tcp,
                peer,
                Arc::clone(&self.shared),
                self.tls.clone(),
                cancel.child_token(),
            ));
        }
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while sessions.join_next().await.is_some() {}
        })
        .await;
        Ok(())
    }
}

async fn serve(
    tcp: TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    tls: Option<TlsAcceptor>,
    cancel: CancellationToken,
) {
    match tls {
        Some(acceptor) => {
            let accepted = tokio::select! {
                accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)) => accepted,
                () = cancel.cancelled() => return,
            };
            match accepted {
                Ok(Ok(stream)) => control(stream, peer, shared, cancel).await,
                Ok(Err(err)) => eprintln!("wsblast agent: TLS handshake with {peer} failed: {err}"),
                Err(_) => {}
            }
        }
        None => control(tcp, peer, shared, cancel).await,
    }
}

fn http_error(status: http::StatusCode, message: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(format!("{message}\n")));
    *response.status_mut() = status;
    if status == http::StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
    }
    response
}

/// Authenticates the control connection, claims the agent and runs the test.
async fn control<S>(stream: S, peer: SocketAddr, shared: Arc<Shared>, cancel: CancellationToken)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let claimed = Arc::new(AtomicBool::new(false));
    let callback = {
        let shared = Arc::clone(&shared);
        let claimed = Arc::clone(&claimed);
        // The error type is fixed by tungstenite's handshake callback.
        #[allow(clippy::result_large_err)]
        move |request: &Request,
              response: Response|
              -> std::result::Result<Response, ErrorResponse> {
            let presented = request
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(bearer_token)
                .unwrap_or("");
            if !tokens_match(&shared.token, presented) {
                eprintln!("wsblast agent: rejected {peer}: missing or wrong token");
                return Err(http_error(
                    http::StatusCode::UNAUTHORIZED,
                    "missing or wrong agent token",
                ));
            }
            if shared
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                eprintln!("wsblast agent: rejected {peer}: busy with another test");
                return Err(http_error(
                    http::StatusCode::CONFLICT,
                    "agent is busy running another test",
                ));
            }
            claimed.store(true, Ordering::Release);
            Ok(response)
        }
    };
    let handshake = tokio::select! {
        handshake = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(ws_config())),
        ) => handshake,
        () = cancel.cancelled() => {
            if claimed.load(Ordering::Acquire) {
                shared.busy.store(false, Ordering::Release);
            }
            return;
        }
    };
    let _busy = claimed
        .load(Ordering::Acquire)
        .then(|| BusyGuard(Arc::clone(&shared)));
    let Ok(Ok(ws)) = handshake else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    // An empty message means the connection is already gone.
    if let Err(message) = run_test(&mut sink, &mut stream, peer, &shared, &cancel).await
        && !message.is_empty()
    {
        eprintln!("wsblast agent: test from {peer} failed: {message}");
        let _ = send(&mut sink, &Message::Error { message }).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), sink.close()).await;
}

type Sink<S> = SplitSink<WebSocketStream<S>, WsMessage>;
type Stream<S> = SplitStream<WebSocketStream<S>>;

/// Sends `message` on the control channel. The error is boxed because
/// `tungstenite::Error` is large and every caller only reports it.
pub(crate) async fn send<S>(
    sink: &mut Sink<S>,
    message: &Message,
) -> std::result::Result<(), Box<tokio_tungstenite::tungstenite::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(WsMessage::Text(message.encode().into()))
        .await
        .map_err(Box::new)
}

/// Why the next message could not be read.
pub(crate) enum RecvError {
    /// The peer closed the connection or it failed.
    Closed(String),
    /// A frame that is not a control message.
    Invalid(String),
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed(reason) | Self::Invalid(reason) => f.write_str(reason),
        }
    }
}

/// Reads the next control message, skipping pings and pongs.
pub(crate) async fn recv<S>(stream: &mut Stream<S>) -> std::result::Result<Message, RecvError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(text))) => {
                return Message::decode(text.as_str()).map_err(RecvError::Invalid);
            }
            Some(Ok(WsMessage::Close(_))) | None => {
                return Err(RecvError::Closed("the connection was closed".to_string()));
            }
            Some(Ok(WsMessage::Binary(_))) => {
                return Err(RecvError::Invalid(
                    "unexpected binary frame on the control channel".to_string(),
                ));
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(RecvError::Closed(err.to_string())),
        }
    }
}

/// Reads the next message within `limit`, or until `cancel`.
async fn recv_within<S>(
    stream: &mut Stream<S>,
    limit: Duration,
    cancel: &CancellationToken,
) -> std::result::Result<Message, RecvError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        received = tokio::time::timeout(limit, recv(stream)) => received
            .unwrap_or_else(|_| Err(RecvError::Closed(format!("no message within {limit:?}")))),
        () = cancel.cancelled() => Err(RecvError::Closed("the agent is shutting down".to_string())),
    }
}

fn unexpected(message: &Message, expected: &str) -> String {
    format!("expected a {expected} message, received {}", message.kind())
}

/// Batches of capture records a phase may hold before its forwarding
/// thread waits for the control channel.
#[cfg(feature = "capture")]
const CAPTURE_BATCHES: usize = 16;

/// How a phase of a test ended.
enum PhaseEnd {
    /// The result was sent; the coordinator may send another phase.
    Done,
    /// The coordinator disconnected; the run was stopped.
    CoordinatorGone,
}

/// Runs one test on an authenticated control connection: one phase, or
/// every phase of a scenario, each a `prepare` to `result` round. Returns
/// the message of an `error` to send when the test cannot go on; a closed
/// connection ends the test silently.
async fn run_test<S>(
    sink: &mut Sink<S>,
    stream: &mut Stream<S>,
    peer: SocketAddr,
    shared: &Shared,
    cancel: &CancellationToken,
) -> std::result::Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let closed = |_| String::new();
    match recv_within(stream, SETUP_TIMEOUT, cancel).await {
        Ok(Message::Hello { protocol, version }) => check_protocol(protocol, &version)?,
        Ok(other) => return Err(unexpected(&other, "hello")),
        Err(RecvError::Invalid(reason)) => return Err(reason),
        Err(RecvError::Closed(_)) => return Ok(()),
    }
    send(sink, &Message::hello()).await.map_err(closed)?;

    // The live counters count from the start of the test, across every
    // phase, as a single-machine scenario's do.
    let live = LiveMetrics::new();
    let mut wait = SETUP_TIMEOUT;
    loop {
        let (wire, start_timeout) = loop {
            match recv_within(stream, wait, cancel).await {
                Ok(Message::Prepare {
                    config,
                    start_timeout,
                }) => break (config, start_timeout),
                // The next `prepare` carries the current value.
                Ok(Message::Token { .. }) => {}
                Ok(Message::Stop) | Err(RecvError::Closed(_)) => return Ok(()),
                Ok(other) => return Err(unexpected(&other, "prepare")),
                Err(RecvError::Invalid(reason)) => return Err(reason),
            }
        };
        // The coordinator sends the next phase once every agent's result is
        // in, at most the agent timeout after the end of this one.
        wait = wire.duration + start_timeout + SETUP_TIMEOUT;
        match run_phase(
            sink,
            stream,
            peer,
            shared,
            cancel,
            *wire,
            start_timeout,
            &live,
        )
        .await?
        {
            PhaseEnd::Done => {}
            PhaseEnd::CoordinatorGone => return Ok(()),
        }
    }
}

/// A runner for `config`, running `phase`'s actions when there is one.
fn phase_runner(
    config: crate::LoadTestConfig,
    phase: Option<super::protocol::WirePhase>,
) -> std::result::Result<Runner, String> {
    match phase {
        None => Ok(Runner::new(config)),
        #[cfg(feature = "scenario")]
        Some(phase) => {
            let phase = phase
                .into_phase(&config)
                .map_err(|e| format!("invalid configuration: {e}"))?;
            Ok(Runner::with_scenario(config, phase))
        }
        #[cfg(not(feature = "scenario"))]
        Some(_) => Err(
            "invalid configuration: scenario phases need the 'scenario' feature, which this agent's build of wsblast does not include"
                .to_string(),
        ),
    }
}

/// Applies a refreshed authentication header value. The value is never
/// logged.
#[cfg(feature = "auth")]
fn apply_token(live: Option<&crate::auth::LiveAuthHeader>, value: &str, peer: SocketAddr) {
    let Some(live) = live else {
        return;
    };
    match super::protocol::sensitive_value(value) {
        Ok(value) => live.replace(value),
        Err(_) => eprintln!(
            "wsblast agent: ignored a refreshed authentication header from {peer} that is not a valid header value"
        ),
    }
}

/// The latency histograms of the last two completed intervals of the run in
/// progress.
fn recent_intervals(runner: &Runner) -> Vec<WireInterval> {
    let Some(store) = runner.interval_store() else {
        return Vec::new();
    };
    let completed = store.start().elapsed().as_nanos() / store.interval().as_nanos().max(1);
    let completed = u64::try_from(completed).unwrap_or(u64::MAX);
    (completed.saturating_sub(2)..completed)
        .filter_map(|index| {
            store.bucket(index).map(|hist| WireInterval {
                index,
                values: to_sparse(&hist),
            })
        })
        .collect()
}

#[cfg(feature = "capture")]
type CaptureBatches = Option<tokio::sync::mpsc::Receiver<Vec<super::WireRecord>>>;
#[cfg(not(feature = "capture"))]
type CaptureBatches = Option<std::convert::Infallible>;

/// The next batch of capture records; pending forever without capture.
async fn next_batch(batches: &mut CaptureBatches) -> Option<Vec<super::WireRecord>> {
    match batches {
        #[cfg(feature = "capture")]
        Some(receiver) => receiver.recv().await,
        #[cfg(not(feature = "capture"))]
        Some(never) => match *never {},
        None => std::future::pending().await,
    }
}

/// Runs one phase: validate and answer `ready`, wait for `start`, run while
/// sending snapshots and capture records, and send the result.
#[allow(clippy::too_many_arguments)]
async fn run_phase<S>(
    sink: &mut Sink<S>,
    stream: &mut Stream<S>,
    peer: SocketAddr,
    shared: &Shared,
    cancel: &CancellationToken,
    mut wire: super::protocol::WireConfig,
    start_timeout: Duration,
    live: &Arc<LiveMetrics>,
) -> std::result::Result<PhaseEnd, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let closed = |_| String::new();
    // The phase stays in the share too: whether it has an action script
    // decides whether chaos may act in a mode that sends no data itself.
    let phase = wire.phase.clone();
    let capture = wire.capture.take();
    #[cfg(not(feature = "capture"))]
    if capture.is_some() {
        return Err(
            "invalid configuration: --capture needs the 'capture' feature, which this agent's build of wsblast does not include"
                .to_string(),
        );
    }
    let config = wire
        .into_config()
        .map_err(|e| format!("invalid configuration: {e}"))?;
    let connections = config.connections;
    let sizing = if config.stages.is_empty() {
        format!("{connections} connections for {:?}", config.duration)
    } else {
        format!(
            "up to {connections} connections in {} stages over {:?}",
            config.stages.len(),
            config.duration
        )
    };
    let phase_label = phase
        .as_ref()
        .map(|phase| format!(" phase '{}'", phase.name))
        .unwrap_or_default();
    crate::output::outln!(
        "wsblast agent: test from {peer}{phase_label}: {} with {sizing}",
        config.target_url
    );
    #[cfg(feature = "auth")]
    let live_auth = config.live_auth.clone();

    // The runner is built once `start` arrives, so a capture counts from the
    // same instant as the run; this one only resolves the target.
    let probe = phase_runner(config.clone(), phase.clone())?;
    let target = tokio::select! {
        target = probe.prepare_target() => target,
        () = cancel.cancelled() => return Err("the agent is shutting down".to_string()),
    };
    drop(probe);
    if !shared.ready_delay.is_zero() {
        tokio::time::sleep(shared.ready_delay).await;
    }
    send(sink, &Message::Ready { connections })
        .await
        .map_err(closed)?;

    let start_wait = tokio::time::Instant::now() + start_timeout + START_SLACK;
    loop {
        let limit = start_wait.saturating_duration_since(tokio::time::Instant::now());
        match recv_within(stream, limit, cancel).await {
            Ok(Message::Start) => break,
            #[cfg(feature = "auth")]
            Ok(Message::Token { value }) => apply_token(live_auth.as_ref(), &value, peer),
            #[cfg(not(feature = "auth"))]
            Ok(Message::Token { .. }) => {}
            Ok(Message::Stop) | Err(RecvError::Closed(_)) => return Ok(PhaseEnd::CoordinatorGone),
            Ok(other) => return Err(unexpected(&other, "start")),
            Err(RecvError::Invalid(reason)) => return Err(reason),
        }
    }

    let started_at_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let started = Instant::now();
    #[cfg(feature = "capture")]
    let (forwarding, mut batches): (Option<crate::capture::ForwardingCapture>, CaptureBatches) =
        match capture {
            Some(capture) => {
                let (tx, rx) = tokio::sync::mpsc::channel(CAPTURE_BATCHES);
                let forwarding =
                    crate::capture::ForwardingCapture::start(started, capture.limit, tx)
                        .map_err(|e| format!("cannot start the capture: {e}"))?;
                (Some(forwarding), Some(rx))
            }
            None => (None, None),
        };
    #[cfg(not(feature = "capture"))]
    let mut batches: CaptureBatches = None;
    #[cfg(not(feature = "capture"))]
    let _ = capture;

    let runner = phase_runner(config, phase)?
        .with_live_metrics(Arc::clone(live))
        .with_signal_handling(false);
    #[cfg(feature = "capture")]
    let runner = runner.with_capture(
        forwarding
            .as_ref()
            .map(crate::capture::ForwardingCapture::handle),
    );
    let runner = Arc::new(runner);
    let run_cancel = runner.cancel_token();
    let mut task = tokio::spawn({
        let runner = Arc::clone(&runner);
        async move { runner.run_prepared(target).await }
    });
    let mut ticker = tokio::time::interval_at(
        (started + Duration::from_secs(1)).into(),
        Duration::from_secs(1),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut coordinator_gone = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut task => break outcome,
            _ = ticker.tick(), if !coordinator_gone => {
                let snapshot = Message::Snapshot {
                    elapsed: started.elapsed(),
                    live: runner.live_snapshot().into(),
                    intervals: recent_intervals(&runner),
                };
                if send(sink, &snapshot).await.is_err() {
                    coordinator_gone = true;
                    run_cancel.cancel();
                }
            }
            Some(records) = next_batch(&mut batches), if !coordinator_gone => {
                if send(sink, &Message::Capture { records }).await.is_err() {
                    coordinator_gone = true;
                    run_cancel.cancel();
                }
            }
            received = recv(stream), if !coordinator_gone => match received {
                Ok(Message::Stop) => run_cancel.cancel(),
                #[cfg(feature = "auth")]
                Ok(Message::Token { value }) => apply_token(live_auth.as_ref(), &value, peer),
                Ok(_) | Err(RecvError::Invalid(_)) => {}
                Err(RecvError::Closed(_)) => {
                    coordinator_gone = true;
                    run_cancel.cancel();
                }
            },
            () = cancel.cancelled(), if !run_cancel.is_cancelled() => run_cancel.cancel(),
        }
    };
    if coordinator_gone {
        eprintln!("wsblast agent: coordinator {peer} disconnected; its test was stopped");
        return Ok(PhaseEnd::CoordinatorGone);
    }
    let outcome = outcome
        .map_err(|e| format!("the run failed: {e}"))?
        .map_err(|e| format!("the run failed: {e}"))?;

    // Every worker has stopped: forward the rest of the capture, then the
    // final counters, then the result.
    #[cfg(feature = "capture")]
    let capture_dropped = match forwarding {
        Some(forwarding) => {
            let mut finish = std::pin::pin!(forwarding.finish());
            let dropped = loop {
                tokio::select! {
                    dropped = &mut finish => break dropped,
                    Some(records) = next_batch(&mut batches) => {
                        send(sink, &Message::Capture { records }).await.map_err(closed)?;
                    }
                }
            };
            if let Some(receiver) = batches.as_mut() {
                while let Ok(records) = receiver.try_recv() {
                    send(sink, &Message::Capture { records })
                        .await
                        .map_err(closed)?;
                }
            }
            Some(dropped)
        }
        None => None,
    };
    #[cfg(not(feature = "capture"))]
    let capture_dropped = None;
    send(
        sink,
        &Message::Snapshot {
            elapsed: outcome.elapsed,
            live: runner.live_snapshot().into(),
            intervals: Vec::new(),
        },
    )
    .await
    .map_err(closed)?;

    let mut totals = MetricsTotals::new().map_err(|e| e.to_string())?;
    for worker in outcome.worker_metrics {
        totals.add_worker(worker);
    }
    let result = WireResult::new(
        &totals,
        ResultMeta {
            elapsed: outcome.elapsed,
            started_at_unix_ms,
            connections: outcome.connections,
            not_started: outcome.not_started,
            capture_dropped,
            lag: outcome.lag,
            open_file_limit: crate::runner::open_file_limit(connections),
        },
        outcome.time_series,
        &outcome.interval_store.buckets(),
    );
    send(
        sink,
        &Message::Result {
            result: Box::new(result),
        },
    )
    .await
    .map_err(closed)?;
    Ok(PhaseEnd::Done)
}
