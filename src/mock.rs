//! A WebSocket test target with controllable failure modes, behind the
//! `wsblast mock` command.
//!
//! The server listens on plain `ws://`, or on `wss://` with a PEM certificate
//! and key ([`MockConfigBuilder::tls`]), and by default echoes every text and
//! binary frame. Its behaviours combine:
//!
//! - a fixed reply delay plus a uniformly distributed jitter,
//! - closing each connection with code 1000 after a number of replies,
//! - answering a fraction of upgrade requests with HTTP 503,
//! - periodic server-wide stalls, during which no connection gets a reply,
//! - replaying the frames a capture file (`wsblast --capture`) recorded as
//!   received, with their original timing, instead of echoing,
//! - scripted replies from a rules file ([`MockConfigBuilder::rules`]):
//!   frames matched by text or by a JSON field get templated replies, an
//!   own delay or a close, instead of an echo.
//!
//! Run it in-process to test a load test or a CI gate against known
//! behaviour:
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use wsblast::mock::{MockConfig, MockServer};
//! use wsblast::{LoadTestConfig, Runner};
//!
//! # #[tokio::main]
//! # async fn main() -> wsblast::Result<()> {
//! let config = MockConfig::builder()
//!     .listen("127.0.0.1:0")
//!     .delay(Duration::from_millis(20))
//!     .build()?;
//! let server = MockServer::bind(config).await?;
//! let url = server.url();
//! let cancel = CancellationToken::new();
//! let serving = tokio::spawn(server.run(cancel.clone()));
//!
//! let load = LoadTestConfig::builder(url).connections(4).build()?;
//! let metrics = Runner::new(load).with_signal_handling(false).run().await?;
//! assert!(metrics.message_rtt.min_us >= 20_000);
//!
//! cancel.cancel();
//! let stats = serving.await.expect("mock server task")?;
//! println!("{} connections", stats.connections_accepted);
//! # Ok(())
//! # }
//! ```
//!
//! Timing rules are described in `docs/mock.md` in the repository.

use crate::error::{ConfigError, Result, WsBlastError};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rules::{Decision, Rules};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::{CloseFrame, Utf8Bytes};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_util::sync::CancellationToken;

mod rules;

/// Address `wsblast mock` listens on by default.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9001";

/// How long a closing connection waits for the client's side of the close.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// How long [`MockServer::run`] waits for connections to close on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Time allowed for a client's TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A validated mock server configuration, built with
/// [`MockConfig::builder`].
#[derive(Debug, Clone)]
pub struct MockConfig {
    listen: String,
    delay: Duration,
    jitter: Duration,
    close_after: Option<u64>,
    reject_ratio: f64,
    stall: Option<Stall>,
    replay: Option<Arc<ReplayScript>>,
    rules: Option<Arc<Rules>>,
    /// Engine.IO ping interval and timeout, when the mock speaks Socket.IO.
    socketio: Option<(Duration, Duration)>,
    #[cfg(feature = "compression")]
    compression: bool,
    seed: Option<u64>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// A server-wide stall of `length` at every multiple of `every`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stall {
    every: Duration,
    length: Duration,
}

impl MockConfig {
    /// Starts a configuration that echoes on [`DEFAULT_LISTEN`] with no
    /// delay, no stalls and no rejections.
    pub fn builder() -> MockConfigBuilder {
        MockConfigBuilder::default()
    }

    /// The address to listen on, as given.
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// Whether the server serves `wss://` (a certificate and key were set
    /// with [`MockConfigBuilder::tls`]).
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// A one-line summary of the active behaviours, as `wsblast mock`
    /// prints it at startup.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        match (&self.replay, &self.rules) {
            (Some(script), _) => parts.push(format!(
                "replay {} frames of {} recorded connections from {}",
                script.frame_count(),
                script.workers.len(),
                script.source
            )),
            (None, Some(rules)) => parts.push(format!(
                "{} rules from {}, unmatched frames {}",
                rules.len(),
                rules.source,
                match rules.unmatched {
                    rules::Unmatched::Echo => "echoed",
                    rules::Unmatched::Ignore => "ignored",
                }
            )),
            (None, None) if self.socketio.is_some() => {
                parts.push("Socket.IO, acknowledging every event".to_string());
            }
            (None, None) => parts.push("echo".to_string()),
        }
        if !self.delay.is_zero() {
            parts.push(format!("delay {:?}", self.delay));
        }
        if !self.jitter.is_zero() {
            parts.push(format!("jitter 0..{:?}", self.jitter));
        }
        if let Some(n) = self.close_after {
            parts.push(format!("close after {n} replies"));
        }
        if self.reject_ratio > 0.0 {
            parts.push(format!(
                "reject {}% of upgrades with 503",
                self.reject_ratio * 100.0
            ));
        }
        #[cfg(feature = "compression")]
        if self.compression {
            parts.push("permessage-deflate".to_string());
        }
        if let Some(stall) = self.stall {
            parts.push(format!("stall {:?} every {:?}", stall.length, stall.every));
        }
        parts.join(", ")
    }
}

/// Builder for [`MockConfig`].
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct MockConfigBuilder {
    listen: String,
    delay: Duration,
    jitter: Duration,
    close_after: Option<u64>,
    reject_ratio: f64,
    stall_every: Option<Duration>,
    stall_for: Option<Duration>,
    replay: Option<PathBuf>,
    rules: Option<PathBuf>,
    socketio: Option<(Duration, Duration)>,
    #[cfg(feature = "compression")]
    compression: bool,
    seed: Option<u64>,
    tls: Option<(PathBuf, PathBuf)>,
}

impl Default for MockConfigBuilder {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_string(),
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            close_after: None,
            reject_ratio: 0.0,
            stall_every: None,
            stall_for: None,
            replay: None,
            rules: None,
            socketio: None,
            #[cfg(feature = "compression")]
            compression: false,
            seed: None,
            tls: None,
        }
    }
}

impl MockConfigBuilder {
    /// Sets the `host:port` to listen on. Port 0 picks a free port; read it
    /// back with [`MockServer::local_addr`].
    pub fn listen(mut self, addr: impl Into<String>) -> Self {
        self.listen = addr.into();
        self
    }

    /// Holds every reply for `delay` after its request arrived.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Adds a uniformly distributed extra delay between zero and `jitter`,
    /// drawn for each reply.
    pub fn jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    /// Closes each connection with code 1000 after it sent `replies`
    /// frames. Must be at least 1.
    pub fn close_after(mut self, replies: u64) -> Self {
        self.close_after = Some(replies);
        self
    }

    /// Answers this fraction (0.0 to 1.0) of upgrade requests with HTTP 503
    /// instead of accepting them.
    pub fn reject_ratio(mut self, ratio: f64) -> Self {
        self.reject_ratio = ratio;
        self
    }

    /// Stops replying on every connection for `length` at every multiple of
    /// `every` since the server started. `length` must be shorter than
    /// `every`.
    pub fn stall(mut self, every: Duration, length: Duration) -> Self {
        self.stall_every = Some(every);
        self.stall_for = Some(length);
        self
    }

    /// Replays the frames a capture file recorded as received, instead of
    /// echoing. The file is read by [`build`](Self::build).
    pub fn replay(mut self, capture_file: impl Into<PathBuf>) -> Self {
        self.replay = Some(capture_file.into());
        self
    }

    /// Answers frames as the rules file at `path` says, instead of echoing:
    /// the first rule whose match fits a frame sends its templated replies,
    /// after its own delay if it sets one, and closes the connection if it
    /// says so. The file is read by [`build`](Self::build); its format is
    /// described in `docs/mock.md`. Cannot be combined with
    /// [`replay`](Self::replay).
    pub fn rules(mut self, path: impl Into<PathBuf>) -> Self {
        self.rules = Some(path.into());
        self
    }

    /// Speaks Socket.IO over WebSocket, as a Socket.IO 4 server does: sends
    /// the Engine.IO open packet, accepts a connect to any namespace,
    /// acknowledges every event that asks for an ack with the event's
    /// arguments after the reply delay, and echoes events that do not.
    /// It pings every 25 seconds and closes a connection that does not answer
    /// within 20. Cannot be combined with rules or replay.
    #[cfg(feature = "socketio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "socketio")))]
    pub fn socketio(self) -> Self {
        self.socketio_ping(Duration::from_secs(25), Duration::from_secs(20))
    }

    /// Like [`socketio`](Self::socketio), with this ping interval and ping
    /// timeout.
    #[cfg(feature = "socketio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "socketio")))]
    pub fn socketio_ping(mut self, interval: Duration, timeout: Duration) -> Self {
        self.socketio = Some((interval, timeout));
        self
    }

    /// Accepts permessage-deflate (RFC 7692) when a client offers it:
    /// messages from the client are inflated before they are answered, and
    /// replies are compressed. A client that does not offer it is served
    /// uncompressed.
    #[cfg(feature = "compression")]
    #[cfg_attr(docsrs, doc(cfg(feature = "compression")))]
    pub fn compression(mut self, enabled: bool) -> Self {
        self.compression = enabled;
        self
    }

    /// Seeds the generator behind jitter and rejections, for repeatable
    /// runs. Without a seed every server draws a different sequence.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Serves `wss://` with the PEM certificate chain in `cert` and the PEM
    /// private key in `key` (PKCS#8, PKCS#1 or SEC1), which
    /// [`build`](Self::build) reads. Clients must trust the certificate:
    /// for wsblast, through `SSL_CERT_FILE` or the platform's store.
    pub fn tls(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        self.tls = Some((cert.into(), key.into()));
        self
    }

    /// Validates the settings and reads the capture file for replay and the
    /// certificate and key for TLS.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] for an empty listen address, a
    /// reject ratio outside 0.0 to 1.0, a close-after of 0, a stall that is
    /// zero or not shorter than its period, a replay file that cannot be
    /// read, is not a capture file or holds no received data frames, a rules
    /// file that cannot be read or is invalid, rules combined with replay, or
    /// a certificate or key that is not valid PEM or does not match, and
    /// [`ConfigError::PayloadFile`] for a certificate or key file that
    /// cannot be read.
    pub fn build(self) -> Result<MockConfig> {
        if self.listen.trim().is_empty() {
            return Err(invalid("--listen must be a host:port address"));
        }
        if !self.reject_ratio.is_finite() || !(0.0..=1.0).contains(&self.reject_ratio) {
            return Err(invalid(format!(
                "--reject-ratio must be between 0.0 and 1.0, received {}",
                self.reject_ratio
            )));
        }
        if self.close_after == Some(0) {
            return Err(invalid("--close-after must be at least 1"));
        }
        let stall = match (self.stall_every, self.stall_for) {
            (None, None) => None,
            (Some(every), Some(length)) => {
                if every.is_zero() || length.is_zero() {
                    return Err(invalid(
                        "--stall-every and --stall-for must be greater than zero",
                    ));
                }
                if length >= every {
                    return Err(invalid(format!(
                        "--stall-for ({length:?}) must be shorter than --stall-every ({every:?})"
                    )));
                }
                Some(Stall { every, length })
            }
            _ => {
                return Err(invalid(
                    "--stall-every and --stall-for must be given together",
                ));
            }
        };
        if self.socketio.is_some() && (self.replay.is_some() || self.rules.is_some()) {
            return Err(invalid(
                "--socketio cannot be combined with --rules or --replay",
            ));
        }
        if self
            .socketio
            .is_some_and(|(interval, timeout)| interval.is_zero() || timeout.is_zero())
        {
            return Err(invalid(
                "the Socket.IO ping interval and timeout must be greater than zero",
            ));
        }
        if self.replay.is_some() && self.rules.is_some() {
            return Err(invalid(
                "--rules and --replay cannot be combined: replay sends recorded frames, rules answer received ones",
            ));
        }
        let replay = self
            .replay
            .as_deref()
            .map(ReplayScript::load)
            .transpose()?
            .map(Arc::new);
        let rules = self
            .rules
            .as_deref()
            .map(Rules::load)
            .transpose()?
            .map(Arc::new);
        let tls = self
            .tls
            .as_ref()
            .map(|(cert, key)| crate::tls::server_config(cert, key))
            .transpose()?;
        Ok(MockConfig {
            listen: self.listen,
            delay: self.delay,
            jitter: self.jitter,
            close_after: self.close_after,
            reject_ratio: self.reject_ratio,
            stall,
            replay,
            rules,
            socketio: self.socketio,
            #[cfg(feature = "compression")]
            compression: self.compression,
            seed: self.seed,
            tls,
        })
    }
}

fn invalid(message: impl Into<String>) -> WsBlastError {
    ConfigError::InvalidOption(message.into()).into()
}

/// Counters of a mock server, read with [`MockStatsHandle::snapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MockStats {
    /// WebSocket handshakes the server completed.
    pub connections_accepted: u64,
    /// Upgrade requests answered with HTTP 503 by the reject ratio.
    pub connections_rejected: u64,
    /// Connections currently open.
    pub connections_active: u64,
    /// Text and binary frames received from clients.
    pub frames_received: u64,
    /// Text and binary frames sent to clients (echoes or replayed frames).
    pub frames_sent: u64,
    /// Connections that agreed on permessage-deflate (`--compression`).
    pub connections_compressed: u64,
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    rejected: AtomicU64,
    active: AtomicU64,
    received: AtomicU64,
    sent: AtomicU64,
    /// What the compressed connections counted.
    #[cfg(feature = "compression")]
    deflate: Arc<crate::deflate::CompressionCounters>,
}

/// Reads a running server's counters; cheap to clone.
#[derive(Debug, Clone)]
pub struct MockStatsHandle(Arc<Counters>);

impl MockStatsHandle {
    /// The counters now, each read with relaxed ordering.
    pub fn snapshot(&self) -> MockStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        MockStats {
            connections_accepted: load(&self.0.accepted),
            connections_rejected: load(&self.0.rejected),
            connections_active: load(&self.0.active),
            frames_received: load(&self.0.received),
            frames_sent: load(&self.0.sent),
            #[cfg(feature = "compression")]
            connections_compressed: self.0.deflate.snapshot().negotiated,
            #[cfg(not(feature = "compression"))]
            connections_compressed: 0,
        }
    }
}

/// A bound mock server. [`run`](Self::run) serves it.
#[derive(Debug)]
pub struct MockServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    config: Arc<MockConfig>,
    counters: Arc<Counters>,
}

impl MockServer {
    /// Binds the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Io`] when the address cannot be resolved or
    /// bound.
    pub async fn bind(config: MockConfig) -> Result<Self> {
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
            config: Arc::new(config),
            counters: Arc::default(),
        })
    }

    /// The address the server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// URL of the server: `ws://`, or `wss://` with TLS. With TLS, connect
    /// with a host name the certificate is valid for (often `localhost`)
    /// rather than the IP address in this URL, unless the certificate lists
    /// that address.
    pub fn url(&self) -> String {
        let scheme = if self.config.is_tls() { "wss" } else { "ws" };
        format!("{scheme}://{}", self.local_addr)
    }

    /// A handle to read the server's counters while it runs.
    pub fn stats(&self) -> MockStatsHandle {
        MockStatsHandle(Arc::clone(&self.counters))
    }

    /// Accepts and serves connections until `cancel` is cancelled, then
    /// sends every open connection a Close frame with code 1001 and returns
    /// the final counters.
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok`; failures of single connections are
    /// not errors of the server.
    pub async fn run(self, cancel: CancellationToken) -> Result<MockStats> {
        let stats = self.stats();
        let shared = Arc::new(Shared {
            config: Arc::clone(&self.config),
            counters: Arc::clone(&self.counters),
            epoch: Instant::now(),
        });
        let acceptor = self.config.tls.clone().map(TlsAcceptor::from);
        let mut rng = XorShift64::new(self.config.seed.unwrap_or_else(time_seed));
        let mut connections = tokio::task::JoinSet::new();
        let mut next_index = 0u64;

        loop {
            let accepted = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            while connections.try_join_next().is_some() {}
            let Ok((tcp, _peer)) = accepted else {
                // Out of file descriptors or a connection reset before
                // accept: back off briefly instead of spinning.
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let _ = tcp.set_nodelay(true);
            let plan = ConnectionPlan {
                reject: rng.next_f64() < self.config.reject_ratio,
                seed: rng.next_u64(),
                index: next_index,
            };
            if !plan.reject {
                next_index += 1;
            }
            connections.spawn(accept_connection(
                tcp,
                acceptor.clone(),
                Arc::clone(&shared),
                plan,
                cancel.child_token(),
            ));
        }

        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        Ok(stats.snapshot())
    }
}

/// Binds `config.listen` and serves until `cancel` is cancelled. Use
/// [`MockServer`] to learn the bound address first (for port 0) or to read
/// counters while it runs.
///
/// # Errors
///
/// As [`MockServer::bind`] and [`MockServer::run`].
pub async fn serve(config: MockConfig, cancel: CancellationToken) -> Result<MockStats> {
    MockServer::bind(config).await?.run(cancel).await
}

struct Shared {
    config: Arc<MockConfig>,
    counters: Arc<Counters>,
    /// Server start: stall windows are counted from here.
    epoch: Instant,
}

impl Shared {
    /// When a reply that is ready at `now` may be sent: `now`, or the end
    /// of the stall `now` falls in.
    fn stall_release(&self, now: Instant) -> Option<Instant> {
        let stall = self.config.stall?;
        let elapsed = now.saturating_duration_since(self.epoch).as_nanos();
        let every = stall.every.as_nanos();
        let cycles = elapsed / every;
        if cycles == 0 {
            return None;
        }
        let into_cycle = elapsed % every;
        let length = stall.length.as_nanos();
        (into_cycle < length).then(|| {
            now + Duration::from_nanos(u64::try_from(length - into_cycle).unwrap_or(u64::MAX))
        })
    }

    /// Delay plus jitter for one reply.
    fn reply_delay(&self, rng: &mut XorShift64) -> Duration {
        self.reply_delay_from(self.config.delay, rng)
    }

    /// `base` plus jitter for one reply: the server's delay, or a rule's.
    fn reply_delay_from(&self, base: Duration, rng: &mut XorShift64) -> Duration {
        let config = &self.config;
        if config.jitter.is_zero() {
            return base;
        }
        let jitter_ns = u64::try_from(config.jitter.as_nanos()).unwrap_or(u64::MAX);
        base + Duration::from_nanos(rng.at_most(jitter_ns))
    }
}

/// Decisions the accept loop makes for one TCP connection.
struct ConnectionPlan {
    reject: bool,
    seed: u64,
    /// Position among accepted connections, for replay round robin.
    index: u64,
}

/// Decrements the active-connection count when a session ends.
struct ActiveGuard(Arc<Counters>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Completes the TLS handshake when the server has a certificate, then
/// serves the WebSocket connection.
async fn accept_connection(
    tcp: TcpStream,
    tls: Option<TlsAcceptor>,
    shared: Arc<Shared>,
    plan: ConnectionPlan,
    cancel: CancellationToken,
) {
    let Some(acceptor) = tls else {
        return serve_connection(tcp, shared, plan, cancel).await;
    };
    // The TLS handshake and session are boxed: their state is about 11 KB,
    // and inline it would be reserved by every plain connection's task too.
    let accepted = tokio::select! {
        accepted = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, Box::pin(acceptor.accept(tcp))) => accepted,
        () = cancel.cancelled() => return,
    };
    // A failed or timed-out TLS handshake is the client's problem, like a
    // failed WebSocket handshake: the connection is dropped and not counted.
    if let Ok(Ok(stream)) = accepted {
        Box::pin(serve_connection(stream, shared, plan, cancel)).await;
    }
}

async fn serve_connection<S>(
    tcp: S,
    shared: Arc<Shared>,
    plan: ConnectionPlan,
    cancel: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if plan.reject {
        let counters = Arc::clone(&shared.counters);
        // The error type is fixed by tungstenite's handshake callback.
        #[allow(clippy::result_large_err)]
        let reject =
            move |_: &Request, _: Response| -> std::result::Result<Response, ErrorResponse> {
                counters.rejected.fetch_add(1, Ordering::Relaxed);
                let mut response =
                    ErrorResponse::new(Some("rejected by wsblast mock --reject-ratio".to_string()));
                *response.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
                Err(response)
            };
        tokio::select! {
            _ = tokio_tungstenite::accept_hdr_async(tcp, reject) => {}
            () = cancel.cancelled() => {}
        }
        return;
    }

    #[cfg(feature = "compression")]
    if shared.config.compression {
        // Boxed, as the layer's buffers and inflater are only worth
        // reserving on connections that asked for compression.
        let stream = Box::new(crate::deflate::DeflateStream::new(
            tcp,
            crate::deflate::Role::Server,
            Arc::clone(&shared.counters.deflate),
        ));
        return Box::pin(accept_session(stream, shared, plan, cancel, true)).await;
    }
    accept_session(tcp, shared, plan, cancel, false).await;
}

/// Completes the WebSocket handshake and serves the session. With
/// `compress`, the response accepts a permessage-deflate offer.
async fn accept_session<S>(
    tcp: S,
    shared: Arc<Shared>,
    plan: ConnectionPlan,
    cancel: CancellationToken,
    compress: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Whether the client asked for Engine.IO 3 (`EIO=3`), which a Socket.IO
    // session then speaks.
    let engine_io_3 = std::sync::atomic::AtomicBool::new(false);
    let socketio = shared.config.socketio.is_some();
    // The error type is fixed by tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let answer = |request: &Request,
                  mut response: Response|
     -> std::result::Result<Response, ErrorResponse> {
        if compress {
            answer_compression(request, &mut response);
        }
        if socketio {
            let eio3 = request
                .uri()
                .query()
                .is_some_and(|query| query.split('&').any(|pair| pair == "EIO=3"));
            engine_io_3.store(eio3, Ordering::Relaxed);
        }
        Ok(response)
    };
    let ws = tokio::select! {
        ws = tokio_tungstenite::accept_hdr_async_with_config(tcp, answer, Some(read_buffer_config())) => ws,
        () = cancel.cancelled() => return,
    };
    let Ok(ws) = ws else {
        return;
    };
    shared.counters.accepted.fetch_add(1, Ordering::Relaxed);
    shared.counters.active.fetch_add(1, Ordering::Relaxed);
    let _active = ActiveGuard(Arc::clone(&shared.counters));
    run_session(ws, &shared, plan, &cancel, engine_io_3.into_inner()).await;
}

type WsSink<S> = SplitSink<WebSocketStream<S>, Message>;
type WsStream<S> = SplitStream<WebSocketStream<S>>;

/// An entry of a session's reply queue.
enum Queued {
    Frame(Message),
    /// A protocol frame that is not a reply: the Socket.IO open and
    /// connect packets. It is sent, but not counted as a reply.
    Control(Message),
    /// Close the connection with this code and reason, as a rule says.
    Close(u16, String),
}

/// What a session sends next.
enum Outgoing<'a> {
    /// Replies waiting for their send time, in the order their frames
    /// arrived: echoes, or what the rules decided.
    Echo(VecDeque<(Instant, Queued)>),
    /// A recorded connection's frames, sent at `start + offset + delay`.
    Replay {
        frames: &'a [ReplayFrame],
        next: usize,
        start: Instant,
        /// When `frames[next]` is due, drawn once per frame.
        due: Option<Instant>,
    },
}

impl Outgoing<'_> {
    fn next_due(&mut self, shared: &Shared, rng: &mut XorShift64) -> Option<Instant> {
        match self {
            Self::Echo(queue) => queue.front().map(|(due, _)| *due),
            Self::Replay {
                frames,
                next,
                start,
                due,
            } => {
                let frame = frames.get(*next)?;
                Some(*due.get_or_insert_with(|| *start + frame.offset + shared.reply_delay(rng)))
            }
        }
    }

    fn pop(&mut self) -> Option<Queued> {
        match self {
            Self::Echo(queue) => queue.pop_front().map(|(_, item)| item),
            Self::Replay {
                frames, next, due, ..
            } => {
                let frame = frames.get(*next)?;
                *next += 1;
                *due = None;
                Some(Queued::Frame(frame.message.clone()))
            }
        }
    }
}

/// The mock reads with the client's largest buffer: the WebSocket library's
/// 128 KiB default would make every read of a small frame clear 128 KiB
/// first, so the mock, not the client, would limit a load test.
/// Accepts the client's permessage-deflate offer, if it made one the mock
/// can take, in the handshake response.
#[cfg(feature = "compression")]
fn answer_compression(request: &Request, response: &mut Response) {
    use http::header::SEC_WEBSOCKET_EXTENSIONS;
    let answer = request
        .headers()
        .get_all(SEC_WEBSOCKET_EXTENSIONS)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(crate::deflate::accept_offer);
    if let Some(value) = answer.and_then(|a| http::HeaderValue::from_str(&a).ok()) {
        response
            .headers_mut()
            .insert(SEC_WEBSOCKET_EXTENSIONS, value);
    }
}

#[cfg(not(feature = "compression"))]
fn answer_compression(_: &Request, _: &mut Response) {}

fn read_buffer_config() -> WebSocketConfig {
    WebSocketConfig::default().read_buffer_size(crate::connect::MAX_READ_BUFFER)
}

async fn run_session<S>(
    ws: WebSocketStream<S>,
    shared: &Shared,
    plan: ConnectionPlan,
    cancel: &CancellationToken,
    engine_io_3: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let mut rng = XorShift64::new(plan.seed);
    let mut outgoing = match &shared.config.replay {
        Some(script) => Outgoing::Replay {
            frames: script.frames_for(plan.index),
            next: 0,
            start: Instant::now(),
            due: None,
        },
        None => Outgoing::Echo(VecDeque::new()),
    };
    let mut replies = 0u64;
    // Data frames received on this connection, for rules' `after` and
    // `{{seq}}`.
    let mut received = 0u64;
    // A Socket.IO session's id, its next ping and the deadline of the pong
    // it waits for.
    let sid = format!("wsblast-mock-{}", plan.index);
    let mut sio = socketio_session(engine_io_3);
    // Engine.IO 4: when the next ping is due, and the deadline of the pong
    // to the last one. Engine.IO 3: no pings, and the deadline of the
    // client's next ping.
    let mut ping_at = None;
    let mut pong_by: Option<Instant> = None;
    if let (Some((interval, timeout)), Outgoing::Echo(queue)) =
        (shared.config.socketio, &mut outgoing)
    {
        let open = socketio_open(&sid, interval, timeout, engine_io_3);
        queue.push_back((Instant::now(), Queued::Control(Message::Text(open.into()))));
        if engine_io_3 {
            // A Socket.IO 2 server joins the client to the default
            // namespace without being asked.
            queue.push_back((Instant::now(), Queued::Control(Message::Text("40".into()))));
            pong_by = Some(Instant::now() + interval + timeout);
        } else {
            ping_at = Some(Instant::now() + interval);
        }
    }

    loop {
        let due = outgoing.next_due(shared, &mut rng);
        // A reply that is already due goes out at once. Waiting for it on a
        // timer would hold it until the timer's next 1 ms tick, which put
        // up to a millisecond into every RTT measured against the mock.
        let event = if due.is_some_and(|due| due <= Instant::now()) {
            if cancel.is_cancelled() {
                Event::Cancelled
            } else {
                Event::Due
            }
        } else {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Event::Cancelled,
                () = sleep_until(pong_by.or(ping_at)) => Event::Ping,
                () = sleep_until(due) => Event::Due,
                msg = stream.next() => Event::Received(msg),
            }
        };
        match event {
            Event::Ping => {
                let now = Instant::now();
                if pong_by.is_some_and(|by| by <= now) {
                    // An Engine.IO server drops a client that missed a ping.
                    close(&mut sink, CloseCode::Normal, "ping timeout".to_string()).await;
                    drain(&mut stream).await;
                    return;
                }
                if let (Some((interval, timeout)), Some(at)) = (shared.config.socketio, ping_at)
                    && at <= now
                {
                    if sink.send(Message::Text("2".into())).await.is_err() {
                        return;
                    }
                    pong_by = Some(now + timeout);
                    ping_at = Some(now + interval);
                }
            }
            Event::Cancelled => {
                shut_down(&mut sink, &mut stream).await;
                return;
            }
            Event::Due => {
                if let Some(release) = shared.stall_release(Instant::now()) {
                    tokio::select! {
                        () = tokio::time::sleep_until(release.into()) => {}
                        () = cancel.cancelled() => {
                            shut_down(&mut sink, &mut stream).await;
                            return;
                        }
                    }
                }
                let msg = match outgoing.pop() {
                    None => continue,
                    Some(Queued::Control(msg)) => {
                        if sink.send(msg).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Some(Queued::Frame(msg)) => msg,
                    Some(Queued::Close(code, reason)) => {
                        close(&mut sink, CloseCode::from(code), reason).await;
                        drain(&mut stream).await;
                        return;
                    }
                };
                if sink.send(msg).await.is_err() {
                    return;
                }
                shared.counters.sent.fetch_add(1, Ordering::Relaxed);
                replies += 1;
                if shared.config.close_after == Some(replies) {
                    close(&mut sink, CloseCode::Normal, "close-after".to_string()).await;
                    drain(&mut stream).await;
                    return;
                }
            }
            Event::Received(msg) => match msg {
                Some(Ok(msg @ (Message::Text(_) | Message::Binary(_))))
                    if shared.config.socketio.is_some() =>
                {
                    if matches!(&msg, Message::Text(text) if text.as_str() == "3") {
                        pong_by = None;
                        continue;
                    }
                    if engine_io_3
                        && matches!(&msg, Message::Text(text) if text.as_str() == "2")
                        && let Some((interval, timeout)) = shared.config.socketio
                    {
                        pong_by = Some(Instant::now() + interval + timeout);
                    }
                    let Some((frames, event)) = socketio_answer(&mut sio, &msg, &sid) else {
                        continue;
                    };
                    if event {
                        shared.counters.received.fetch_add(1, Ordering::Relaxed);
                        received += 1;
                    }
                    if let Outgoing::Echo(queue) = &mut outgoing {
                        let due = if event {
                            Instant::now() + shared.reply_delay(&mut rng)
                        } else {
                            Instant::now()
                        };
                        // An event's reply is its first frame; the attachments
                        // that follow it go out with it and are not counted.
                        for (i, frame) in frames.into_iter().enumerate() {
                            let item = if event && i == 0 {
                                Queued::Frame(frame)
                            } else {
                                Queued::Control(frame)
                            };
                            queue.push_back((due, item));
                        }
                    }
                }
                Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                    shared.counters.received.fetch_add(1, Ordering::Relaxed);
                    received += 1;
                    if let Outgoing::Echo(queue) = &mut outgoing {
                        queue_replies(queue, msg, received, shared, &mut rng);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    // Completes the close handshake the client started.
                    let _ = tokio::time::timeout(CLOSE_GRACE, sink.close()).await;
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
        }
    }
}

/// What woke a mock session up.
enum Event {
    Cancelled,
    /// A Socket.IO ping is due, or the pong to the last one is late.
    Ping,
    /// The next outgoing frame is due.
    Due,
    Received(Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>),
}

/// Queues what the server sends for `msg`, the `seq`-th data frame of the
/// connection: an echo, or what the rules decide. Every frame of one
/// decision is due at the same time.
fn queue_replies(
    queue: &mut VecDeque<(Instant, Queued)>,
    msg: Message,
    seq: u64,
    shared: &Shared,
    rng: &mut XorShift64,
) {
    let decision = match &shared.config.rules {
        Some(rules) => rules.decide(&msg, seq),
        None => Decision::Echo,
    };
    match decision {
        Decision::Echo => {
            queue.push_back((Instant::now() + shared.reply_delay(rng), Queued::Frame(msg)));
        }
        Decision::Ignore => {}
        Decision::Reply {
            frames,
            delay,
            close,
        } => {
            let base = delay.unwrap_or(shared.config.delay);
            let due = Instant::now() + shared.reply_delay_from(base, rng);
            queue.extend(frames.into_iter().map(|f| (due, Queued::Frame(f))));
            if let Some((code, reason)) = close {
                queue.push_back((due, Queued::Close(code, reason)));
            }
        }
    }
}

/// The Engine.IO open packet of a Socket.IO session.
#[cfg(feature = "socketio")]
fn socketio_open(sid: &str, interval: Duration, timeout: Duration, engine_io_3: bool) -> String {
    crate::socketio::mock_open(
        sid,
        interval.as_millis(),
        timeout.as_millis(),
        engine(engine_io_3),
    )
}

#[cfg(not(feature = "socketio"))]
fn socketio_open(_: &str, _: Duration, _: Duration, _: bool) -> String {
    String::new()
}

#[cfg(feature = "socketio")]
fn engine(engine_io_3: bool) -> crate::socketio::EngineIo {
    if engine_io_3 {
        crate::socketio::EngineIo::V3
    } else {
        crate::socketio::EngineIo::V4
    }
}

/// A connection's Socket.IO state, speaking the Engine.IO version the
/// client asked for.
#[cfg(feature = "socketio")]
fn socketio_session(engine_io_3: bool) -> SocketIoSession {
    SocketIoSession::new(engine(engine_io_3))
}

#[cfg(not(feature = "socketio"))]
fn socketio_session(_: bool) -> SocketIoSession {
    SocketIoSession::default()
}

#[cfg(feature = "socketio")]
type SocketIoSession = crate::socketio::MockSession;
/// Without the `socketio` feature a session has no Socket.IO state.
#[cfg(not(feature = "socketio"))]
#[derive(Default)]
struct SocketIoSession {
    _none: (),
}

/// What a Socket.IO session answers to a client's frame, and whether the
/// frame completed an event.
#[cfg(feature = "socketio")]
fn socketio_answer(
    session: &mut SocketIoSession,
    frame: &Message,
    sid: &str,
) -> Option<(Vec<Message>, bool)> {
    session
        .receive(frame, sid)
        .map(|answer| (answer.frames, answer.event))
}

#[cfg(not(feature = "socketio"))]
fn socketio_answer(_: &mut SocketIoSession, _: &Message, _: &str) -> Option<(Vec<Message>, bool)> {
    None
}

async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due.into()).await,
        None => std::future::pending().await,
    }
}

async fn close<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut WsSink<S>,
    code: CloseCode,
    reason: String,
) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from(reason),
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, sink.send(Message::Close(Some(frame)))).await;
}

/// Closes a session because the server is stopping. The close handshake is
/// completed before the socket is dropped: dropped while frames the client
/// sent are still unread, it would end with a TCP reset, which the client
/// counts as a read error.
async fn shut_down<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut WsSink<S>,
    stream: &mut WsStream<S>,
) {
    close(sink, CloseCode::Away, "server shutting down".to_string()).await;
    drain(stream).await;
}

/// Reads until the client finishes the close handshake or `CLOSE_GRACE`
/// passes, discarding what arrives.
async fn drain<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut WsStream<S>) {
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        while let Some(Ok(_)) = stream.next().await {}
    })
    .await;
}

/// One frame of a recorded connection.
#[derive(Debug)]
struct ReplayFrame {
    /// Time since the recorded connection's first record.
    offset: Duration,
    message: Message,
}

/// Received frames of a capture file, grouped by recorded worker.
#[derive(Debug)]
struct ReplayScript {
    source: String,
    /// One entry per recorded worker that received at least one data
    /// frame, in agent and worker order.
    workers: Vec<Vec<ReplayFrame>>,
}

/// A recorded worker's first record time and its received frames.
type RecordedWorker = (u64, Vec<(u64, Message)>);

#[derive(serde::Deserialize)]
struct CaptureLine {
    t_us: u64,
    /// The agent of a distributed capture; connection slots are numbered
    /// per agent.
    #[serde(default)]
    agent: u64,
    worker: u64,
    dir: String,
    kind: String,
    #[serde(default)]
    data: Option<String>,
}

impl ReplayScript {
    fn load(path: &Path) -> Result<Self> {
        let source = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .map_err(|err| invalid(format!("--replay: cannot read '{source}': {err}")))?;
        Self::parse(&text, source)
    }

    fn parse(text: &str, source: String) -> Result<Self> {
        let fail = |line: usize, message: &str| {
            invalid(format!("--replay: {source}, line {line}: {message}"))
        };
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(index, line)| (index + 1, line.trim()))
            .filter(|(_, line)| !line.is_empty());

        let (header_line, header) = lines
            .next()
            .ok_or_else(|| invalid(format!("--replay: '{source}' is empty")))?;
        let version = serde_json::from_str::<serde_json::Value>(header)
            .ok()
            .and_then(|value| value.get("wsblast_capture")?.as_u64())
            .ok_or_else(|| {
                fail(
                    header_line,
                    "not a wsblast capture file (no wsblast_capture header)",
                )
            })?;
        if version != 1 {
            return Err(fail(
                header_line,
                &format!("unsupported capture format version {version}"),
            ));
        }

        // Per recorded worker: its first record's time and its frames.
        let mut workers: BTreeMap<(u64, u64), RecordedWorker> = BTreeMap::new();
        for (number, line) in lines {
            let record: CaptureLine = serde_json::from_str(line)
                .map_err(|err| fail(number, &format!("invalid record: {err}")))?;
            let entry = workers
                .entry((record.agent, record.worker))
                .or_insert((record.t_us, Vec::new()));
            entry.0 = entry.0.min(record.t_us);
            if record.dir != "recv" {
                continue;
            }
            let data = record.data.unwrap_or_default();
            let message = match record.kind.as_str() {
                "text" => Message::Text(Utf8Bytes::from(data)),
                "binary" => match crate::base64::decode(&data) {
                    Some(bytes) => Message::Binary(bytes.into()),
                    None => return Err(fail(number, "binary data is not valid base64")),
                },
                _ => continue,
            };
            entry.1.push((record.t_us, message));
        }

        let workers: Vec<Vec<ReplayFrame>> = workers
            .into_values()
            .filter(|(_, frames)| !frames.is_empty())
            .map(|(first, mut frames)| {
                frames.sort_by_key(|(t_us, _)| *t_us);
                frames
                    .into_iter()
                    .map(|(t_us, message)| ReplayFrame {
                        offset: Duration::from_micros(t_us - first),
                        message,
                    })
                    .collect()
            })
            .collect();
        if workers.is_empty() {
            return Err(invalid(format!(
                "--replay: '{source}' has no received text or binary frames to replay"
            )));
        }
        Ok(Self { source, workers })
    }

    /// The frames for the `index`-th accepted connection: recorded workers
    /// in turn.
    fn frames_for(&self, index: u64) -> &[ReplayFrame] {
        let slot = usize::try_from(index % self.workers.len() as u64).unwrap_or(0);
        &self.workers[slot]
    }

    fn frame_count(&self) -> usize {
        self.workers.iter().map(Vec::len).sum()
    }
}

/// Marsaglia's xorshift64: fast, small and good enough for jitter and
/// rejection draws. Not for anything that needs real randomness.
#[derive(Debug, Clone)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // A zero state would stay zero; any other value works.
        Self(splitmix64(seed).max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..=max`, up to a modulo bias below 2^-64 * max.
    fn at_most(&mut self, max: u64) -> u64 {
        match max.checked_add(1) {
            Some(bound) => self.next_u64() % bound,
            None => self.next_u64(),
        }
    }
}

/// Spreads a seed's bits so nearby seeds give unrelated sequences.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn time_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ u64::from(std::process::id()).rotate_left(32)
}

/// `wsblast mock`: validates the flags, serves until Ctrl+C or SIGTERM and
/// prints a stats line every second.
pub(crate) async fn run_command(args: &crate::cli::MockArgs) -> Result<()> {
    use crate::config::parse_human_duration;

    let mut builder = MockConfig::builder()
        .listen(&args.listen)
        .delay(parse_human_duration(&args.delay)?)
        .jitter(parse_human_duration(&args.jitter)?)
        .reject_ratio(args.reject_ratio);
    if let Some(n) = args.close_after {
        builder = builder.close_after(n);
    }
    if let (Some(every), Some(length)) = (&args.stall_every, &args.stall_for) {
        builder = builder.stall(parse_human_duration(every)?, parse_human_duration(length)?);
    }
    if let Some(path) = &args.replay {
        builder = builder.replay(path);
    }
    if let Some(path) = &args.rules {
        builder = builder.rules(path);
    }
    #[cfg(feature = "socketio")]
    if args.socketio {
        builder = builder.socketio();
    }
    #[cfg(feature = "compression")]
    {
        builder = builder.compression(args.compression);
    }
    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        builder = builder.tls(cert, key);
    }
    let config = builder.build()?;

    let cancel = CancellationToken::new();
    let _signals = crate::runner::spawn_signal_listener(cancel.clone());
    let server = MockServer::bind(config).await?;
    crate::output::outln!(
        "wsblast mock listening on {} ({})",
        server.url(),
        server.config.describe()
    );

    let stats = server.stats();
    let printer = tokio::spawn(print_stats(stats.clone(), cancel.clone()));
    let last = server.run(cancel.clone()).await?;
    cancel.cancel();
    let _ = printer.await;
    crate::output::outln!(
        "wsblast mock stopped: {} connections accepted, {} rejected, {} frames received, {} sent{}",
        last.connections_accepted,
        last.connections_rejected,
        last.frames_received,
        last.frames_sent,
        if last.connections_compressed > 0 {
            format!(", {} compressed connections", last.connections_compressed)
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Prints one line per second while anything is connected or changed.
async fn print_stats(stats: MockStatsHandle, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut previous = stats.snapshot();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let now = stats.snapshot();
        if now == previous && now.connections_active == 0 {
            continue;
        }
        crate::output::outln!(
            "[stats] connections: {} active, {} accepted, {} rejected | frames: {} received, {} sent | rate: {} recv/s, {} sent/s",
            now.connections_active,
            now.connections_accepted,
            now.connections_rejected,
            now.frames_received,
            now.frames_sent,
            now.frames_received - previous.frames_received,
            now.frames_sent - previous.frames_sent,
        );
        previous = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(builder: MockConfigBuilder) -> MockConfig {
        builder.build().expect("valid mock config")
    }

    #[test]
    fn invalid_settings_are_rejected_with_the_flag_name() {
        let cases = [
            (MockConfig::builder().reject_ratio(1.5), "--reject-ratio"),
            (MockConfig::builder().reject_ratio(-0.1), "--reject-ratio"),
            (
                MockConfig::builder().reject_ratio(f64::NAN),
                "--reject-ratio",
            ),
            (MockConfig::builder().close_after(0), "--close-after"),
            (
                MockConfig::builder().stall(Duration::from_secs(1), Duration::from_secs(1)),
                "--stall-for",
            ),
            (
                MockConfig::builder().stall(Duration::ZERO, Duration::ZERO),
                "--stall-every",
            ),
            (MockConfig::builder().listen(" "), "--listen"),
            (
                MockConfig::builder().replay("/nonexistent/capture.jsonl"),
                "--replay",
            ),
        ];
        for (builder, flag) in cases {
            let err = builder.build().expect_err(flag).to_string();
            assert!(err.contains(flag), "{flag}: {err}");
        }
    }

    #[test]
    fn stalls_start_at_each_multiple_of_the_period() {
        let shared = Shared {
            config: Arc::new(config(
                MockConfig::builder().stall(Duration::from_secs(1), Duration::from_millis(200)),
            )),
            counters: Arc::default(),
            epoch: Instant::now(),
        };
        let at = |ms: u64| shared.epoch + Duration::from_millis(ms);
        assert_eq!(
            shared.stall_release(at(100)),
            None,
            "no stall before the first period"
        );
        assert_eq!(shared.stall_release(at(999)), None);
        assert_eq!(shared.stall_release(at(1000)), Some(at(1200)));
        assert_eq!(shared.stall_release(at(1150)), Some(at(1200)));
        assert_eq!(shared.stall_release(at(1200)), None);
        assert_eq!(shared.stall_release(at(3050)), Some(at(3200)));
    }

    #[test]
    fn jitter_stays_within_its_bound_and_covers_it() {
        let shared = Shared {
            config: Arc::new(config(
                MockConfig::builder()
                    .delay(Duration::from_millis(10))
                    .jitter(Duration::from_millis(5)),
            )),
            counters: Arc::default(),
            epoch: Instant::now(),
        };
        let mut rng = XorShift64::new(7);
        let delays: Vec<Duration> = (0..10_000).map(|_| shared.reply_delay(&mut rng)).collect();
        let min = *delays.iter().min().unwrap();
        let max = *delays.iter().max().unwrap();
        assert!(
            min >= Duration::from_millis(10)
                && min < Duration::from_millis(10) + Duration::from_micros(50)
        );
        assert!(max <= Duration::from_millis(15) && max > Duration::from_micros(14_950));
    }

    #[test]
    fn xorshift_fractions_are_in_unit_interval_and_spread() {
        let mut rng = XorShift64::new(0);
        let draws: Vec<f64> = (0..10_000).map(|_| rng.next_f64()).collect();
        assert!(draws.iter().all(|x| (0.0..1.0).contains(x)));
        let below_half = draws.iter().filter(|&&x| x < 0.5).count();
        assert!((4_500..5_500).contains(&below_half), "{below_half}");
        assert_eq!(XorShift64::new(1).next_u64(), XorShift64::new(1).next_u64());
        assert_ne!(XorShift64::new(1).next_u64(), XorShift64::new(2).next_u64());
    }

    const CAPTURE: &str = r#"{"wsblast_capture":1,"target":"ws://127.0.0.1:9001/","started":"2026-10-25T00:00:00.000Z"}
{"t_us":100,"worker":1,"dir":"recv","kind":"open","len":0}
{"t_us":120,"worker":0,"dir":"recv","kind":"open","len":0}
{"t_us":150,"worker":1,"dir":"sent","kind":"text","len":1,"data":"a"}
{"t_us":400,"worker":1,"dir":"recv","kind":"text","len":1,"data":"a"}
{"t_us":900,"worker":1,"dir":"recv","kind":"binary","len":2,"data":"AP8="}
{"t_us":1000,"worker":0,"dir":"recv","kind":"text","len":2,"data":"hi"}
{"t_us":1100,"worker":2,"dir":"sent","kind":"text","len":2,"data":"no"}
{"t_us":1200,"worker":1,"dir":"recv","kind":"close","len":0}
"#;

    #[test]
    fn replay_groups_received_frames_by_worker_with_offsets() {
        let script = ReplayScript::parse(CAPTURE, "test".into()).unwrap();
        assert_eq!(script.workers.len(), 2, "worker 2 received nothing");
        assert_eq!(script.frame_count(), 3);

        let worker0 = script.frames_for(0);
        assert_eq!(worker0.len(), 1);
        assert_eq!(worker0[0].offset, Duration::from_micros(880));
        assert_eq!(worker0[0].message, Message::Text("hi".into()));

        let worker1 = script.frames_for(1);
        assert_eq!(worker1[0].offset, Duration::from_micros(300));
        assert_eq!(worker1[1].offset, Duration::from_micros(800));
        assert_eq!(
            worker1[1].message,
            Message::Binary(bytes::Bytes::from_static(&[0, 255]))
        );
        assert_eq!(script.frames_for(2).len(), 1, "round robin wraps");
    }

    #[test]
    fn replay_keeps_the_agents_of_a_distributed_capture_apart() {
        let capture = r#"{"wsblast_capture":1,"target":"ws://127.0.0.1:9001/","started":"2026-10-25T00:00:00.000Z","agents":["a:1","b:1"]}
{"t_us":100,"agent":0,"worker":0,"dir":"recv","kind":"text","len":1,"data":"a"}
{"t_us":150,"agent":1,"worker":0,"dir":"recv","kind":"text","len":1,"data":"b"}
{"t_us":300,"agent":1,"worker":0,"dir":"recv","kind":"text","len":1,"data":"c"}
"#;
        let script = ReplayScript::parse(capture, "test".into()).unwrap();
        assert_eq!(script.workers.len(), 2);
        assert_eq!(script.frames_for(0).len(), 1);
        assert_eq!(script.frames_for(1).len(), 2);
        assert_eq!(script.frames_for(1)[1].offset, Duration::from_micros(150));
    }

    #[test]
    fn replay_rejects_files_that_are_not_captures() {
        let cases = [
            ("", "is empty"),
            ("{\"t_us\":1}\n", "no wsblast_capture header"),
            ("{\"wsblast_capture\":2}\n", "version 2"),
            ("{\"wsblast_capture\":1}\nnot json\n", "line 2"),
            (
                "{\"wsblast_capture\":1}\n{\"t_us\":1,\"worker\":0,\"dir\":\"recv\",\"kind\":\"binary\",\"len\":1,\"data\":\"!!\"}\n",
                "base64",
            ),
            (
                "{\"wsblast_capture\":1}\n{\"t_us\":1,\"worker\":0,\"dir\":\"sent\",\"kind\":\"text\",\"len\":1,\"data\":\"a\"}\n",
                "no received",
            ),
        ];
        for (text, expected) in cases {
            let err = ReplayScript::parse(text, "f.jsonl".into())
                .expect_err(expected)
                .to_string();
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn describe_lists_active_behaviours() {
        assert_eq!(config(MockConfig::builder()).describe(), "echo");
        let described = config(
            MockConfig::builder()
                .delay(Duration::from_millis(10))
                .jitter(Duration::from_millis(5))
                .close_after(3)
                .reject_ratio(0.25)
                .stall(Duration::from_secs(1), Duration::from_millis(200)),
        )
        .describe();
        assert_eq!(
            described,
            "echo, delay 10ms, jitter 0..5ms, close after 3 replies, reject 25% of upgrades with 503, stall 200ms every 1s"
        );
    }
}
