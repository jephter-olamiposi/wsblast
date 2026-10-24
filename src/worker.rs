//! Per-connection asynchronous WebSocket worker session lifecycle.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig};
use crate::error::ErrorCategory;
use crate::metrics::WorkerMetrics;
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(feature = "chaos")]
use crate::config::ChaosConfig;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::Message;
#[cfg(feature = "scenario")]
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "graphql-ws")]
use crate::config::{GraphQLWSMessage, GraphQLWSStartPayload};

#[cfg(feature = "scenario")]
use crate::config::{ScenarioAction, ScenarioPhase};

#[cfg(feature = "capture")]
use crate::metrics::FrameCapture;

#[cfg(feature = "chaos")]
use fastrand;

/// Represents a single asynchronous WebSocket client worker session.
pub struct WorkerSession {
    pub worker_id: usize,
    pub config: Arc<LoadTestConfig>,
    pub cancel_token: CancellationToken,
    pub metrics: WorkerMetrics,
    #[cfg(feature = "capture")]
    frame_capture: Option<FrameCapture>,
    #[cfg(feature = "scenario")]
    scenario: Option<Arc<ScenarioPhase>>,
}

impl WorkerSession {
    pub fn new(
        worker_id: usize,
        config: Arc<LoadTestConfig>,
        cancel_token: CancellationToken,
        metrics: WorkerMetrics,
    ) -> Self {
        #[cfg(feature = "capture")]
        let frame_capture = if config.capture.enabled {
            Some(FrameCapture::new(config.capture.max_frames))
        } else {
            None
        };

        Self {
            worker_id,
            config,
            cancel_token,
            metrics,
            #[cfg(feature = "capture")]
            frame_capture,
            #[cfg(feature = "scenario")]
            scenario: None,
        }
    }

    /// Attaches a compiled scenario phase that drives think-time and actions.
    /// Shares a frame buffer across workers so one capture file covers the run.
    #[cfg(feature = "capture")]
    pub fn with_frame_capture(mut self, capture: Option<FrameCapture>) -> Self {
        self.frame_capture = capture;
        self
    }

    #[cfg(feature = "scenario")]
    pub fn with_scenario(mut self, phase: Arc<ScenarioPhase>) -> Self {
        self.scenario = Some(phase);
        self
    }

    /// Executes the full worker lifecycle: TCP+TLS handshake -> session loop -> graceful close.
    ///
    /// Unless `no_reconnect` is set, a failed handshake or a lost session is
    /// followed by a fresh handshake after an exponential backoff, until the
    /// run is cancelled. Echo mode needs the fresh socket: a late reply on the
    /// old one would be attributed to the next request.
    pub async fn run(mut self) -> WorkerMetrics {
        let client_req = match self.build_handshake_request() {
            Ok(req) => req,
            Err(cat) => {
                // The request is derived from static config, so retrying cannot help.
                self.metrics.record_connection_failed(cat);
                return self.metrics;
            }
        };

        let mut backoff = ReconnectBackoff::new();
        let mut connected_before = false;

        loop {
            let handshake_start = Instant::now();
            let connect_fut = tokio::time::timeout(
                self.config.connect_timeout,
                tokio_tungstenite::connect_async(client_req.clone()),
            );
            let connect_result = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break,
                res = connect_fut => res,
            };

            match connect_result {
                Ok(Ok((ws_stream, _))) => {
                    if connected_before {
                        self.metrics.record_reconnect(handshake_start.elapsed());
                    } else {
                        self.metrics.record_handshake(handshake_start.elapsed());
                        connected_before = true;
                    }
                    backoff.reset();

                    let end = self.run_session(ws_stream).await;
                    self.metrics.record_connection_closed();
                    if end == SessionEnd::Finished {
                        break;
                    }
                }
                Ok(Err(err)) => {
                    self.metrics
                        .record_connection_failed(categorize_tungstenite_error(&err));
                }
                Err(_) => {
                    self.metrics
                        .record_connection_failed(ErrorCategory::Timeout);
                }
            }

            if self.config.no_reconnect || self.cancel_token.is_cancelled() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(backoff.next_delay()) => {}
                _ = self.cancel_token.cancelled() => break,
            }
        }

        self.metrics
    }

    /// Drives one established connection with the configured session loop.
    async fn run_session(
        &mut self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> SessionEnd {
        #[cfg(feature = "scenario")]
        if let Some(phase) = self.scenario.clone() {
            if !phase.actions.is_empty() {
                return self.run_action_loop(ws_stream, phase).await;
            }
        }

        match self.config.mode {
            TestMode::Echo => self.run_echo_loop(ws_stream).await,
            TestMode::Stream => self.run_stream_loop(ws_stream).await,
            TestMode::Listen => self.run_listen_loop(ws_stream).await,
            #[cfg(feature = "graphql-ws")]
            TestMode::GraphQLWS => self.run_graphql_ws_loop(ws_stream).await,
        }
    }

    /// Classifies a session that ended on its own: cancellation (including the
    /// request cap, which cancels the run) means the work is done, anything
    /// else means the connection was lost.
    fn session_end(&self) -> SessionEnd {
        if self.cancel_token.is_cancelled() {
            SessionEnd::Finished
        } else {
            SessionEnd::Lost
        }
    }

    fn build_handshake_request(&self) -> Result<Request, ErrorCategory> {
        let mut req = self
            .config
            .target_url
            .as_str()
            .into_client_request()
            .map_err(|_| ErrorCategory::Other)?;

        for (name, value) in &self.config.headers {
            req.headers_mut().insert(name.clone(), value.clone());
        }

        if let Some(subproto) = &self.config.subprotocol {
            if let Ok(val) = http::HeaderValue::from_str(subproto) {
                req.headers_mut().insert(
                    http::header::HeaderName::from_static("sec-websocket-protocol"),
                    val,
                );
            }
        }

        Ok(req)
    }

    /// Records an outbound frame when capture is enabled.
    fn capture_sent(&self, _message: &Message) {
        #[cfg(feature = "capture")]
        if let Some(capture) = &self.frame_capture {
            capture.record_sent(self.worker_id, _message);
        }
    }

    /// Records an inbound frame when capture is enabled.
    fn capture_received(&self, _message: &Message) {
        #[cfg(feature = "capture")]
        if let Some(capture) = &self.frame_capture {
            capture.record_received(self.worker_id, _message);
        }
    }

    async fn run_echo_loop<S>(&mut self, ws_stream: S) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (sink, stream) = ws_stream.split();

        #[cfg(feature = "chaos")]
        let mut sink = ChaosInjector::new(sink, self.config.chaos.clone());

        #[cfg(feature = "chaos")]
        let mut stream = ChaosInjector::new(stream, self.config.chaos.clone());

        #[cfg(not(feature = "chaos"))]
        let mut sink = sink;

        #[cfg(not(feature = "chaos"))]
        let mut stream = stream;

        let mut seq = 0u64;

        let pacing_duration = if self.config.rate_per_conn > 0 {
            Some(Duration::from_secs_f64(
                1.0 / self.config.rate_per_conn as f64,
            ))
        } else {
            None
        };

        // Skip missed ticks to prevent burst storms after temporary latency pauses
        let mut ticker = pacing_duration.map(interval);
        if let Some(ref mut t) = ticker {
            t.set_missed_tick_behavior(MissedTickBehavior::Skip);
        }

        let mut ping_ticker = self.keepalive_ticker();

        'session: loop {
            if self.cancel_token.is_cancelled() {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }

            if let Some(max_reqs) = self.config.max_requests {
                if self.metrics.live.messages_sent.load(Ordering::Relaxed) >= max_reqs {
                    self.cancel_token.cancel();
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }

            if let Some(ref mut t) = ticker {
                // Keepalive ticks that land while waiting for the pacing slot
                // only send a ping; the data send still waits for the slot.
                loop {
                    tokio::select! {
                        _ = t.tick() => break,
                        _ = async {
                            match ping_ticker.as_mut() {
                                Some(pt) => { pt.tick().await; }
                                None => std::future::pending().await,
                            }
                        } => {
                            let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                        }
                        _ = self.cancel_token.cancelled() => {
                            let _ = sink.send(Message::Close(None)).await;
                            break 'session;
                        }
                    }
                }
            } else if let Some(pt) = ping_ticker.as_mut() {
                // Unpaced load must not wait for the keepalive period, so the
                // ticker is only polled once; `Interval::tick` is cancel safe.
                if pt.tick().now_or_never().is_some() {
                    let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                }
            }

            let msg = self.render_payload(seq);
            let frame_len = message_byte_len(&msg);
            #[cfg(feature = "capture")]
            let captured_msg = self.frame_capture.as_ref().map(|_| msg.clone());
            let start_time = Instant::now();

            tokio::select! {
                res = sink.send(msg) => {
                    if let Err(err) = res {
                        self.metrics
                            .record_error(categorize_session_error(&err, ErrorDirection::Write));
                        break;
                    }
                    #[cfg(feature = "capture")]
                    if let (Some(capture), Some(captured)) = (&self.frame_capture, captured_msg) {
                        capture.record_sent(self.worker_id, &captured);
                    }
                    self.metrics.record_message_sent(frame_len);
                }
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }

            let recv_fut = async {
                while let Some(msg_res) = stream.next().await {
                    match msg_res {
                        Ok(Message::Text(txt)) => {
                            #[cfg(feature = "capture")]
                            if let Some(ref capture) = self.frame_capture {
                                capture
                                    .record_received(self.worker_id, &Message::Text(txt.clone()));
                            }
                            return Ok((txt.len(), true));
                        }
                        Ok(Message::Binary(bin)) => {
                            #[cfg(feature = "capture")]
                            if let Some(ref capture) = self.frame_capture {
                                capture
                                    .record_received(self.worker_id, &Message::Binary(bin.clone()));
                            }
                            return Ok((bin.len(), true));
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = sink.send(Message::Pong(data)).await;
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => return Ok((0, false)),
                        Ok(Message::Frame(_)) => {}
                        Err(e) => return Err(categorize_session_error(&e, ErrorDirection::Read)),
                    }
                }
                Err(ErrorCategory::UnexpectedClose)
            };

            // Cancellation wins over a simultaneously ready reply so nothing
            // after the test window is recorded.
            let response = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => None,
                res = tokio::time::timeout(self.config.message_timeout, recv_fut) => Some(res),
            };
            let Some(response) = response else {
                let _ = sink.send(Message::Close(None)).await;
                break;
            };

            match response {
                Ok(Ok((bytes_recv, is_alive))) => {
                    let rtt = start_time.elapsed();
                    self.metrics.record_message_recv(bytes_recv, Some(rtt));
                    if !is_alive {
                        break;
                    }
                }
                Ok(Err(cat)) => {
                    self.metrics.record_error(cat);
                    break;
                }
                Err(_) => {
                    self.metrics.record_error(ErrorCategory::Timeout);
                    break;
                }
            }

            seq += 1;

            #[cfg(feature = "scenario")]
            if !self.apply_think_time().await {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
        }

        self.session_end()
    }

    async fn run_stream_loop<S>(&mut self, ws_stream: S) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (sink, stream) = ws_stream.split();

        #[cfg(feature = "chaos")]
        let mut sink = ChaosInjector::new(sink, self.config.chaos.clone());

        #[cfg(feature = "chaos")]
        let mut stream = ChaosInjector::new(stream, self.config.chaos.clone());

        #[cfg(not(feature = "chaos"))]
        let mut sink = sink;

        #[cfg(not(feature = "chaos"))]
        let mut stream = stream;

        let mut seq = 0u64;
        let mut ping_ticker = self.keepalive_ticker();

        let pacing_duration = if self.config.rate_per_conn > 0 {
            Duration::from_secs_f64(1.0 / self.config.rate_per_conn as f64)
        } else {
            Duration::from_micros(10)
        };

        let mut ticker = interval(pacing_duration);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if let Some(max_reqs) = self.config.max_requests {
                if self.metrics.live.messages_sent.load(Ordering::Relaxed) >= max_reqs {
                    self.cancel_token.cancel();
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }

            tokio::select! {
                _ = ticker.tick() => {
                    let msg = self.render_payload(seq);
                    let len = message_byte_len(&msg);
                    self.capture_sent(&msg);
                    if let Err(e) = sink.send(msg).await {
                        self.metrics.record_error(categorize_session_error(
                            &e,
                            ErrorDirection::Write,
                        ));
                        break;
                    }
                    self.metrics.record_message_sent(len);
                    seq += 1;

                    #[cfg(feature = "scenario")]
                    if !self.apply_think_time().await {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                }
                _ = async {
                    match ping_ticker.as_mut() {
                        Some(ticker) => {
                            ticker.tick().await;
                        }
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                }
                msg_opt = stream.next() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => {
                            let len = t.len();
                            self.capture_received(&Message::Text(t));
                            self.metrics.record_message_recv(len, None);
                        }
                        Some(Ok(Message::Binary(b))) => {
                            let len = b.len();
                            self.capture_received(&Message::Binary(b));
                            self.metrics.record_message_recv(len, None);
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(categorize_session_error(
                                &e,
                                ErrorDirection::Read,
                            ));
                            break;
                        }
                        _ => {}
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }
        }

        self.session_end()
    }

    async fn run_listen_loop<S>(&mut self, ws_stream: S) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (sink, stream) = ws_stream.split();

        #[cfg(feature = "chaos")]
        let mut sink = ChaosInjector::new(sink, self.config.chaos.clone());

        #[cfg(feature = "chaos")]
        let mut stream = ChaosInjector::new(stream, self.config.chaos.clone());

        #[cfg(not(feature = "chaos"))]
        let mut sink = sink;

        #[cfg(not(feature = "chaos"))]
        let mut stream = stream;

        let mut ping_ticker = self.keepalive_ticker();

        loop {
            tokio::select! {
                _ = async {
                    match ping_ticker.as_mut() {
                        Some(ticker) => {
                            ticker.tick().await;
                        }
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                }
                msg_opt = stream.next() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => {
                            let len = t.len();
                            self.capture_received(&Message::Text(t));
                            self.metrics.record_message_recv(len, None);
                        }
                        Some(Ok(Message::Binary(b))) => {
                            let len = b.len();
                            self.capture_received(&Message::Binary(b));
                            self.metrics.record_message_recv(len, None);
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(categorize_session_error(
                                &e,
                                ErrorDirection::Read,
                            ));
                            break;
                        }
                        _ => {}
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }
        }

        self.session_end()
    }

    /// Executes a compiled action script until the phase ends, the server
    /// closes, cancellation fires, or an explicit Close action runs.
    ///
    /// Scripts repeat after their final action so a phase without an explicit
    /// Close keeps generating load for its full duration. `rate_per_conn`
    /// paces individual sends and `think_time` pauses after each Send burst.
    #[cfg(feature = "scenario")]
    async fn run_action_loop<S>(&mut self, ws_stream: S, phase: Arc<ScenarioPhase>) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (sink, stream) = ws_stream.split();

        #[cfg(feature = "chaos")]
        let mut sink = ChaosInjector::new(sink, self.config.chaos.clone());

        #[cfg(feature = "chaos")]
        let mut stream = ChaosInjector::new(stream, self.config.chaos.clone());

        #[cfg(not(feature = "chaos"))]
        let mut sink = sink;

        #[cfg(not(feature = "chaos"))]
        let mut stream = stream;

        let mut seq = 0u64;
        let mut ticker = if self.config.rate_per_conn > 0 {
            let mut t = interval(Duration::from_secs_f64(
                1.0 / self.config.rate_per_conn as f64,
            ));
            t.set_missed_tick_behavior(MissedTickBehavior::Skip);
            Some(t)
        } else {
            None
        };

        'script: loop {
            if self.cancel_token.is_cancelled() {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }

            for action in &phase.actions {
                if self.cancel_token.is_cancelled() || self.max_requests_reached() {
                    if self.max_requests_reached() {
                        self.cancel_token.cancel();
                    }
                    let _ = sink.send(Message::Close(None)).await;
                    break 'script;
                }

                match action {
                    ScenarioAction::Send { payload, count } => {
                        for _ in 0..*count {
                            if self.cancel_token.is_cancelled() || self.max_requests_reached() {
                                if self.max_requests_reached() {
                                    self.cancel_token.cancel();
                                }
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }

                            if let Some(ref mut t) = ticker {
                                tokio::select! {
                                    _ = t.tick() => {}
                                    _ = self.cancel_token.cancelled() => {
                                        let _ = sink.send(Message::Close(None)).await;
                                        break 'script;
                                    }
                                }
                            }

                            let active_payload = payload.as_ref().unwrap_or(&self.config.payload);
                            let msg = self.render_message(active_payload, seq);
                            let frame_len = message_byte_len(&msg);
                            #[cfg(feature = "capture")]
                            let captured_msg = self.frame_capture.as_ref().map(|_| msg.clone());
                            let start_time = Instant::now();

                            if let Err(err) = sink.send(msg).await {
                                self.metrics.record_error(categorize_session_error(
                                    &err,
                                    ErrorDirection::Write,
                                ));
                                break 'script;
                            }

                            #[cfg(feature = "capture")]
                            if let (Some(capture), Some(captured)) =
                                (&self.frame_capture, captured_msg)
                            {
                                capture.record_sent(self.worker_id, &captured);
                            }
                            self.metrics.record_message_sent(frame_len);

                            if self.config.mode == TestMode::Echo {
                                let recv_fut = async {
                                    while let Some(msg_res) = stream.next().await {
                                        match msg_res {
                                            Ok(Message::Text(txt)) => {
                                                #[cfg(feature = "capture")]
                                                if let Some(ref capture) = self.frame_capture {
                                                    capture.record_received(
                                                        self.worker_id,
                                                        &Message::Text(txt.clone()),
                                                    );
                                                }
                                                return Ok((txt.len(), true));
                                            }
                                            Ok(Message::Binary(bin)) => {
                                                #[cfg(feature = "capture")]
                                                if let Some(ref capture) = self.frame_capture {
                                                    capture.record_received(
                                                        self.worker_id,
                                                        &Message::Binary(bin.clone()),
                                                    );
                                                }
                                                return Ok((bin.len(), true));
                                            }
                                            Ok(Message::Ping(data)) => {
                                                let _ = sink.send(Message::Pong(data)).await;
                                            }
                                            Ok(Message::Pong(_)) => {}
                                            Ok(Message::Close(_)) => return Ok((0, false)),
                                            Ok(Message::Frame(_)) => {}
                                            Err(e) => {
                                                return Err(categorize_session_error(
                                                    &e,
                                                    ErrorDirection::Read,
                                                ));
                                            }
                                        }
                                    }
                                    Err(ErrorCategory::UnexpectedClose)
                                };

                                let response = tokio::select! {
                                    biased;
                                    _ = self.cancel_token.cancelled() => None,
                                    res = tokio::time::timeout(
                                        self.config.message_timeout,
                                        recv_fut,
                                    ) => Some(res),
                                };
                                let Some(response) = response else {
                                    let _ = sink.send(Message::Close(None)).await;
                                    break 'script;
                                };

                                match response {
                                    Ok(Ok((bytes_recv, is_alive))) => {
                                        self.metrics.record_message_recv(
                                            bytes_recv,
                                            Some(start_time.elapsed()),
                                        );
                                        if !is_alive {
                                            break 'script;
                                        }
                                    }
                                    Ok(Err(cat)) => {
                                        self.metrics.record_error(cat);
                                        break 'script;
                                    }
                                    Err(_) => {
                                        self.metrics.record_error(ErrorCategory::Timeout);
                                        break 'script;
                                    }
                                }
                            }

                            seq += 1;
                        }

                        if !self.apply_think_time().await {
                            let _ = sink.send(Message::Close(None)).await;
                            break 'script;
                        }
                    }
                    ScenarioAction::Wait(duration) => {
                        tokio::select! {
                            _ = tokio::time::sleep(*duration) => {}
                            _ = self.cancel_token.cancelled() => {
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }
                        }
                    }
                    ScenarioAction::Ping(interval) => {
                        if let Err(err) = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await
                        {
                            self.metrics.record_error(categorize_session_error(
                                &err,
                                ErrorDirection::Write,
                            ));
                            break 'script;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(*interval) => {}
                            _ = self.cancel_token.cancelled() => {
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }
                        }
                    }
                    ScenarioAction::Close { code, reason } => {
                        let frame = Message::Close(Some(CloseFrame {
                            code: (*code).into(),
                            reason: Utf8Bytes::from(reason.clone()),
                        }));
                        let _ = sink.send(frame).await;
                        return SessionEnd::Finished;
                    }
                }
            }
        }

        self.session_end()
    }

    #[cfg(feature = "scenario")]
    fn max_requests_reached(&self) -> bool {
        self.config
            .max_requests
            .is_some_and(|max| self.metrics.live.messages_sent.load(Ordering::Relaxed) >= max)
    }

    /// Pauses for the phase think-time, returning false when cancellation fires.
    #[cfg(feature = "scenario")]
    async fn apply_think_time(&self) -> bool {
        let think_time = match &self.scenario {
            Some(phase) => phase.think_time,
            None => return true,
        };
        if think_time.is_zero() {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(think_time) => true,
            _ = self.cancel_token.cancelled() => false,
        }
    }

    /// Builds the optional keepalive ticker shared by every session mode.
    fn keepalive_ticker(&self) -> Option<tokio::time::Interval> {
        if self.config.ping_interval > Duration::ZERO {
            let mut ticker = interval(self.config.ping_interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            Some(ticker)
        } else {
            None
        }
    }

    fn render_payload(&self, seq: u64) -> Message {
        self.render_message(&self.config.payload, seq)
    }

    fn render_message(&self, payload: &PayloadConfig, seq: u64) -> Message {
        match payload {
            PayloadConfig::Binary(bytes) => Message::Binary(bytes.clone()),
            PayloadConfig::StaticText(utf8) => Message::Text(utf8.clone()),
            PayloadConfig::DynamicText(template) => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);

                let rendered = template
                    .replace("{{timestamp}}", &now_ms.to_string())
                    .replace("{{worker_id}}", &self.worker_id.to_string())
                    .replace("{{seq}}", &seq.to_string());

                Message::Text(Utf8Bytes::from(rendered))
            }
        }
    }

    #[cfg(feature = "graphql-ws")]
    async fn run_graphql_ws_loop<S>(&mut self, ws_stream: S) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (sink, stream) = ws_stream.split();

        #[cfg(feature = "chaos")]
        let mut sink = ChaosInjector::new(sink, self.config.chaos.clone());

        #[cfg(feature = "chaos")]
        let mut stream = ChaosInjector::new(stream, self.config.chaos.clone());

        #[cfg(not(feature = "chaos"))]
        let mut sink = sink;

        #[cfg(not(feature = "chaos"))]
        let mut stream = stream;

        let mut ping_ticker = self.keepalive_ticker();

        let graphql_config = match &self.config.graphql_ws {
            Some(cfg) => cfg,
            None => {
                self.metrics.record_error(ErrorCategory::ProtocolError);
                return SessionEnd::Finished;
            }
        };
        let legacy_protocol = self
            .config
            .subprotocol
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("graphql-ws"));

        // Send connection_init
        let init_msg = GraphQLWSMessage::ConnectionInit {
            payload: None, // Could add auth payload here if needed
        };
        let init_json = match serde_json::to_string(&init_msg) {
            Ok(json) => json,
            Err(_) => {
                self.metrics.record_error(ErrorCategory::ProtocolError);
                return SessionEnd::Finished;
            }
        };
        let init_len = init_json.len();
        let init_message = Message::Text(Utf8Bytes::from(init_json));
        self.capture_sent(&init_message);
        if let Err(e) = sink.send(init_message).await {
            self.metrics
                .record_error(categorize_session_error(&e, ErrorDirection::Write));
            return self.session_end();
        }
        self.metrics.record_message_sent(init_len);

        // Wait for connection_ack without blocking cancellation or the timeout.
        let ack_timeout = tokio::time::sleep(self.config.message_timeout);
        tokio::pin!(ack_timeout);
        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    return self.session_end();
                }
                _ = &mut ack_timeout => {
                    self.metrics.record_error(ErrorCategory::Timeout);
                    return self.session_end();
                }
                _ = async {
                    match ping_ticker.as_mut() {
                        Some(ticker) => {
                            ticker.tick().await;
                        }
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                }
                msg_result = stream.next() => {
                    match msg_result {
                        Some(Ok(Message::Text(text))) => {
                            #[cfg(feature = "capture")]
                            if let Some(capture) = &self.frame_capture {
                                capture.record_received(
                                    self.worker_id,
                                    &Message::Text(text.clone()),
                                );
                            }
                            if let Ok(msg) = serde_json::from_str::<GraphQLWSMessage>(&text) {
                                match msg {
                                    GraphQLWSMessage::ConnectionAck => {
                                        break;
                                    }
                                    GraphQLWSMessage::ConnectionError { .. } => {
                                        self.metrics.record_error(ErrorCategory::ProtocolError);
                                        return self.session_end();
                                    }
                                    GraphQLWSMessage::Ping => {
                                        if let Ok(json) =
                                            serde_json::to_string(&GraphQLWSMessage::Pong)
                                        {
                                            let _ = sink
                                                .send(Message::Text(Utf8Bytes::from(json)))
                                                .await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            self.metrics.record_error(ErrorCategory::UnexpectedClose);
                            return self.session_end();
                        }
                        Some(Err(e)) => {
                            self.metrics
                                .record_error(categorize_session_error(&e, ErrorDirection::Read));
                            return self.session_end();
                        }
                        _ => {}
                    }
                }
            }
        }

        // Send start message with operation
        let start_payload = GraphQLWSStartPayload {
            query: graphql_config.operation.clone(),
            operation_name: graphql_config.operation_name.clone(),
            variables: graphql_config.variables.clone(),
            extensions: graphql_config.extensions.clone(),
        };

        let subscription_id = format!("wsblast-sub-{}", self.worker_id);
        let start_msg = if legacy_protocol {
            GraphQLWSMessage::Start {
                id: subscription_id.clone(),
                payload: start_payload,
            }
        } else {
            GraphQLWSMessage::Subscribe {
                id: subscription_id.clone(),
                payload: start_payload,
            }
        };
        let start_json = match serde_json::to_string(&start_msg) {
            Ok(json) => json,
            Err(_) => {
                self.metrics.record_error(ErrorCategory::ProtocolError);
                return SessionEnd::Finished;
            }
        };
        let start_len = start_json.len();
        let start_message = Message::Text(Utf8Bytes::from(start_json));
        self.capture_sent(&start_message);
        if let Err(e) = sink.send(start_message).await {
            self.metrics
                .record_error(categorize_session_error(&e, ErrorDirection::Write));
            return self.session_end();
        }
        self.metrics.record_message_sent(start_len);

        // Handle incoming messages (data, error, complete, ka).
        let mut subscription_complete = false;
        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                _ = async {
                    match ping_ticker.as_mut() {
                        Some(ticker) => {
                            ticker.tick().await;
                        }
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await;
                }
                msg_result = stream.next() => {
                    match msg_result {
                        Some(Ok(Message::Text(text))) => {
                            #[cfg(feature = "capture")]
                            if let Some(capture) = &self.frame_capture {
                                capture.record_received(
                                    self.worker_id,
                                    &Message::Text(text.clone()),
                                );
                            }
                            if let Ok(msg) = serde_json::from_str::<GraphQLWSMessage>(&text) {
                                match msg {
                                    GraphQLWSMessage::Data { id: _, payload: _ }
                                    | GraphQLWSMessage::Next { id: _, payload: _ } => {
                                        self.metrics.record_message_recv(text.len(), None);
                                    }
                                    GraphQLWSMessage::Error { .. } => {
                                        self.metrics.record_error(ErrorCategory::ProtocolError);
                                    }
                                    GraphQLWSMessage::ConnectionError { .. } => {
                                        self.metrics.record_error(ErrorCategory::ProtocolError);
                                    }
                                    GraphQLWSMessage::Complete { .. } => {
                                        subscription_complete = true;
                                        break;
                                    }
                                    GraphQLWSMessage::Ping => {
                                        if let Ok(json) =
                                            serde_json::to_string(&GraphQLWSMessage::Pong)
                                        {
                                            let _ = sink
                                                .send(Message::Text(Utf8Bytes::from(json)))
                                                .await;
                                        }
                                    }
                                    GraphQLWSMessage::ConnectionKeepAlive
                                    | GraphQLWSMessage::Pong => {}
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            break;
                        }
                        Some(Err(e)) => {
                            self.metrics.record_error(categorize_session_error(
                                &e,
                                ErrorDirection::Read,
                            ));
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Send stop/complete to cleanly end the subscription.
        let stop_msg = if legacy_protocol {
            GraphQLWSMessage::Stop {
                id: subscription_id,
            }
        } else {
            GraphQLWSMessage::Complete {
                id: subscription_id,
            }
        };
        let stop_json = match serde_json::to_string(&stop_msg) {
            Ok(json) => json,
            Err(_) => {
                self.metrics.record_error(ErrorCategory::ProtocolError);
                return SessionEnd::Finished;
            }
        };
        let stop_message = Message::Text(Utf8Bytes::from(stop_json));
        self.capture_sent(&stop_message);
        let _ = sink.send(stop_message).await;

        if !subscription_complete {
            // Connection closed unexpectedly
            self.metrics.record_error(ErrorCategory::UnexpectedClose);
            return self.session_end();
        }

        SessionEnd::Finished
    }
}

/// How a session loop ended, deciding whether the worker reconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEnd {
    /// The run was cancelled or the script or subscription completed.
    Finished,
    /// The connection failed or the server closed it while the run continued.
    Lost,
}

/// Exponential reconnect delay: 100ms doubling to a 5s cap, reset after a
/// successful handshake.
struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const INITIAL: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    fn reset(&mut self) {
        self.next = Self::INITIAL;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(Self::MAX);
        delay
    }
}

fn message_byte_len(msg: &Message) -> usize {
    match msg {
        Message::Text(t) => t.len(),
        Message::Binary(b) => b.len(),
        Message::Ping(p) | Message::Pong(p) => p.len(),
        Message::Close(_) | Message::Frame(_) => 0,
    }
}

/// Chaos injection middleware for testing resilience.
/// Applies a deterministic single-character or single-byte corruption to a frame.
#[cfg(feature = "chaos")]
fn corrupt_message(message: Message) -> Message {
    match message {
        Message::Text(text) => {
            let mut chars: Vec<char> = text.chars().collect();
            if chars.is_empty() {
                return Message::Text(text);
            }
            let index = fastrand::usize(..chars.len());
            chars[index] = '\u{FFFD}';
            Message::Text(Utf8Bytes::from(chars.into_iter().collect::<String>()))
        }
        Message::Binary(bytes) => {
            if bytes.is_empty() {
                return Message::Binary(bytes);
            }
            let mut corrupted = bytes.to_vec();
            let index = fastrand::usize(..corrupted.len());
            corrupted[index] ^= 0x01;
            Message::Binary(Bytes::from(corrupted))
        }
        other => other,
    }
}

/// Injects latency, packet loss, corruption, and connection closes on the hot path.
#[cfg(feature = "chaos")]
pub struct ChaosInjector<S> {
    inner: S,
    config: ChaosConfig,
    latency: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    sink_latency_done: bool,
    stream_latency_done: bool,
}

#[cfg(feature = "chaos")]
impl<S> ChaosInjector<S> {
    pub fn new(inner: S, config: ChaosConfig) -> Self {
        Self {
            inner,
            config,
            latency: None,
            sink_latency_done: false,
            stream_latency_done: false,
        }
    }

    /// Applies latency injection without blocking the async runtime.
    fn poll_latency(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if !self.config.enabled {
            return std::task::Poll::Ready(());
        }
        let latency_ms = match self.config.latency_ms {
            Some(latency_ms) if latency_ms > 0 => latency_ms,
            _ => return std::task::Poll::Ready(()),
        };

        let sleep = self
            .latency
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(latency_ms))));
        let elapsed = match std::future::Future::poll(sleep.as_mut(), cx) {
            std::task::Poll::Ready(()) => true,
            std::task::Poll::Pending => false,
        };

        if elapsed {
            self.latency = None;
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }

    /// Determines if a message should be dropped based on loss percentage.
    fn should_drop(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        if let Some(loss_percent) = self.config.loss_percent {
            if loss_percent > 0.0 {
                let roll: f64 = fastrand::f64() * 100.0;
                return roll < loss_percent;
            }
        }
        false
    }

    /// Determines if a message should be corrupted based on corruption percentage.
    fn should_corrupt(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        if let Some(corrupt_percent) = self.config.corrupt_percent {
            if corrupt_percent > 0.0 {
                let roll: f64 = fastrand::f64() * 100.0;
                return roll < corrupt_percent;
            }
        }
        false
    }

    /// Determines if connection should be closed based on close percentage.
    fn should_close(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        if let Some(close_percent) = self.config.close_percent {
            if close_percent > 0.0 {
                let roll: f64 = fastrand::f64() * 100.0;
                return roll < close_percent;
            }
        }
        false
    }
}

#[cfg(feature = "chaos")]
impl<S> futures_util::Sink<Message> for ChaosInjector<S>
where
    S: futures_util::Sink<Message, Error = TungsteniteError> + Unpin,
{
    type Error = TungsteniteError;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = self.as_mut().get_mut();
        if !this.sink_latency_done {
            match this.poll_latency(cx) {
                std::task::Poll::Ready(()) => this.sink_latency_done = true,
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        std::pin::Pin::new(&mut this.inner).poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let this = self.as_mut().get_mut();
        this.sink_latency_done = false;
        if this.should_drop() {
            return Ok(()); // Silently drop
        }
        if this.should_close() {
            return Err(TungsteniteError::ConnectionClosed);
        }
        let item = if this.should_corrupt() {
            corrupt_message(item)
        } else {
            item
        };
        std::pin::Pin::new(&mut this.inner).start_send(item)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(feature = "chaos")]
impl<S> futures_util::Stream for ChaosInjector<S>
where
    S: futures_util::Stream<Item = Result<Message, TungsteniteError>> + Unpin,
{
    type Item = Result<Message, TungsteniteError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if !this.stream_latency_done {
            match this.poll_latency(cx) {
                std::task::Poll::Ready(()) => this.stream_latency_done = true,
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }

        loop {
            let poll = std::pin::Pin::new(&mut this.inner).poll_next(cx);
            match poll {
                std::task::Poll::Ready(Some(Ok(msg))) => {
                    this.stream_latency_done = false;
                    if this.should_drop() {
                        // Packet loss removes the frame from the stream instead of
                        // surfacing a protocol error that would end the session.
                        continue;
                    }
                    if this.should_close() {
                        return std::task::Poll::Ready(Some(Err(
                            TungsteniteError::ConnectionClosed,
                        )));
                    }
                    return std::task::Poll::Ready(Some(Ok(msg)));
                }
                std::task::Poll::Ready(other) => {
                    this.stream_latency_done = false;
                    return std::task::Poll::Ready(other);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Direction of the I/O operation that produced a session error.
#[derive(Clone, Copy)]
enum ErrorDirection {
    Connect,
    Write,
    Read,
}

/// Returns true when an I/O error came from hostname resolution.
///
/// `tokio-tungstenite`/`std` surface `getaddrinfo` failures as
/// `ErrorKind::Uncategorized` with a platform-specific message, so the message
/// is the only portable signal available.
fn is_dns_resolution_error(io_err: &std::io::Error) -> bool {
    let message = io_err.to_string().to_ascii_lowercase();
    message.contains("failed to lookup address information")
        || message.contains("nodename nor servname")
        || message.contains("name or service not known")
        || message.contains("temporary failure in name resolution")
        || message.contains("no address associated with hostname")
}

fn categorize_session_error(err: &TungsteniteError, direction: ErrorDirection) -> ErrorCategory {
    match err {
        TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => {
            ErrorCategory::UnexpectedClose
        }
        TungsteniteError::Http(resp) => {
            if resp.status().is_client_error() || resp.status().is_server_error() {
                ErrorCategory::HttpUpgradeRejected
            } else {
                ErrorCategory::ProtocolError
            }
        }
        TungsteniteError::Io(io_err) => {
            if is_dns_resolution_error(io_err) {
                return ErrorCategory::DnsResolution;
            }
            match io_err.kind() {
                std::io::ErrorKind::TimedOut => ErrorCategory::Timeout,
                std::io::ErrorKind::ConnectionRefused => ErrorCategory::TcpConnect,
                std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof => match direction {
                    ErrorDirection::Connect => ErrorCategory::TcpConnect,
                    ErrorDirection::Write => ErrorCategory::WriteError,
                    ErrorDirection::Read => ErrorCategory::ReadError,
                },
                _ => match direction {
                    ErrorDirection::Connect => ErrorCategory::TcpConnect,
                    ErrorDirection::Write => ErrorCategory::WriteError,
                    ErrorDirection::Read => ErrorCategory::ReadError,
                },
            }
        }
        TungsteniteError::Tls(_) => ErrorCategory::TlsHandshake,
        TungsteniteError::Protocol(_) => ErrorCategory::ProtocolError,
        TungsteniteError::WriteBufferFull(_) => ErrorCategory::WriteError,
        _ => ErrorCategory::Other,
    }
}

/// Normalizes connection-establishment errors into canonical taxonomy categories.
pub fn categorize_tungstenite_error(err: &TungsteniteError) -> ErrorCategory {
    categorize_session_error(err, ErrorDirection::Connect)
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn reconnect_backoff_doubles_caps_and_resets() {
        let mut backoff = ReconnectBackoff::new();
        let delays: Vec<u64> = (0..8)
            .map(|_| backoff.next_delay().as_millis() as u64)
            .collect();
        assert_eq!(delays, [100, 200, 400, 800, 1600, 3200, 5000, 5000]);

        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    }
}

#[cfg(all(test, feature = "chaos"))]
mod chaos_tests {
    use super::*;

    #[test]
    fn text_corruption_stays_valid_utf8_and_changes_content() {
        match corrupt_message(Message::Text(Utf8Bytes::from_static("hello"))) {
            Message::Text(text) => assert_ne!(text.as_str(), "hello"),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    #[test]
    fn binary_corruption_changes_content() {
        match corrupt_message(Message::Binary(Bytes::from_static(&[1, 2, 3]))) {
            Message::Binary(bytes) => assert_ne!(bytes.as_ref(), &[1, 2, 3]),
            other => panic!("expected binary frame, got {other:?}"),
        }
    }

    #[test]
    fn empty_frames_are_left_unchanged() {
        assert_eq!(
            corrupt_message(Message::Text(Utf8Bytes::from_static(""))),
            Message::Text(Utf8Bytes::from_static(""))
        );
        assert_eq!(
            corrupt_message(Message::Binary(Bytes::new())),
            Message::Binary(Bytes::new())
        );
    }
}
