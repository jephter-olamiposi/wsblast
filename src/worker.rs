//! Per-connection asynchronous WebSocket worker session lifecycle.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig, ReplyMatch};
use crate::connect::{ConnectError, ConnectTarget};
use crate::error::ErrorCategory;
use crate::metrics::{ErrorScope, WorkerMetrics};
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::{CloseFrame, Utf8Bytes};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "graphql-ws")]
use crate::config::{GraphQLWSMessage, GraphQLWSStartPayload};

#[cfg(feature = "scenario")]
use crate::config::{ScenarioAction, ScenarioPhase};

/// Represents a single asynchronous WebSocket client worker session.
pub(crate) struct WorkerSession {
    worker_id: usize,
    config: Arc<LoadTestConfig>,
    target: Arc<ConnectTarget>,
    cancel_token: CancellationToken,
    metrics: WorkerMetrics,
    #[cfg(feature = "scenario")]
    scenario: Option<Arc<ScenarioPhase>>,
    /// Records this worker's frames when the run captures.
    #[cfg(feature = "capture")]
    capture: Option<crate::capture::CaptureHandle>,
    /// This worker's fault injection, when the run injects faults.
    #[cfg(feature = "chaos")]
    chaos: Option<crate::chaos::ChaosState>,
    /// Values the connection's scenario captured from replies, by the
    /// phase's variable positions; cleared on every new connection.
    vars: Vec<String>,
}

impl WorkerSession {
    pub(crate) fn new(
        worker_id: usize,
        config: Arc<LoadTestConfig>,
        target: Arc<ConnectTarget>,
        cancel_token: CancellationToken,
        metrics: WorkerMetrics,
    ) -> Self {
        #[cfg(feature = "chaos")]
        let (chaos, metrics) = match &config.chaos {
            Some(chaos) => {
                let mut metrics = metrics;
                metrics.chaos = Some(crate::metrics::ChaosSummary::new(
                    chaos.seed.unwrap_or_default(),
                ));
                (
                    Some(crate::chaos::ChaosState::new(chaos, worker_id)),
                    metrics,
                )
            }
            None => (None, metrics),
        };
        Self {
            worker_id,
            config,
            target,
            cancel_token,
            metrics,
            #[cfg(feature = "scenario")]
            scenario: None,
            #[cfg(feature = "capture")]
            capture: None,
            #[cfg(feature = "chaos")]
            chaos,
            vars: Vec::new(),
        }
    }

    /// Records every frame of this worker's sessions through `capture`.
    #[cfg(feature = "capture")]
    pub(crate) fn with_capture(mut self, capture: Option<crate::capture::CaptureHandle>) -> Self {
        self.capture = capture;
        self
    }

    /// Attaches a compiled scenario phase that drives think-time and actions.
    #[cfg(feature = "scenario")]
    pub(crate) fn with_scenario(mut self, phase: Arc<ScenarioPhase>) -> Self {
        self.scenario = Some(phase);
        self
    }

    /// Executes the full worker lifecycle: TCP+TLS handshake -> session loop -> graceful close.
    ///
    /// Unless `no_reconnect` is set, a failed handshake or a lost session is
    /// followed by a fresh handshake after an exponential backoff, until the
    /// run is cancelled. Echo mode needs the fresh socket: a late reply on the
    /// old one would be attributed to the next request.
    ///
    /// The session is boxed: an `async fn` taking `self` by value keeps two
    /// copies of it in the task.
    pub(crate) async fn run(mut self: Box<Self>) -> WorkerMetrics {
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
            // The session starts outside the handshake's match, so nothing
            // from the handshake stays in this task while the session runs:
            // the socket has moved into the boxed session loop.
            let end = {
                let session = match self.handshake(&client_req, connected_before).await {
                    Handshake::Open(ws_stream) => {
                        connected_before = true;
                        backoff.reset();
                        Some(self.session(ws_stream))
                    }
                    Handshake::Failed => None,
                    Handshake::Stop => break,
                };
                match session {
                    Some(session) => Some(session.await),
                    None => None,
                }
            };
            if let Some(end) = end {
                self.metrics.record_connection_closed();
                if end == SessionEnd::Finished {
                    break;
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

    /// Opens one connection, recording its handshake latency or why it
    /// failed.
    async fn handshake(&mut self, client_req: &Request, connected_before: bool) -> Handshake {
        // A failed handshake belongs to no action, even when the last
        // session ended in the middle of one.
        self.metrics.current_action = None;
        #[cfg_attr(not(feature = "auth"), allow(unused_mut))]
        let mut request = client_req.clone();
        // A refreshed token is read per attempt, so reconnects present the
        // latest one.
        #[cfg(feature = "auth")]
        if let Some(auth) = &self.config.live_auth {
            auth.apply(request.headers_mut());
        }
        let handshake_start = Instant::now();
        // The handshake future is over 9 KB with its TLS state. Boxed, it is
        // freed once the connection is open instead of staying in the task.
        let connect_fut = tokio::time::timeout(
            self.config.connect_timeout,
            Box::pin(self.target.connect(request)),
        );
        let connect_result = tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => {
                // The run ended during this slot's first handshake, before
                // it could succeed or fail: report it apart from failures.
                if !connected_before && self.metrics.errors.is_empty() {
                    self.metrics.interrupted = true;
                }
                return Handshake::Stop;
            }
            res = connect_fut => res,
        };
        match connect_result {
            Ok(Ok(ws_stream)) => {
                if connected_before {
                    self.metrics.record_reconnect(handshake_start.elapsed());
                } else {
                    self.metrics.record_handshake(handshake_start.elapsed());
                }
                Handshake::Open(ws_stream)
            }
            Ok(Err(ConnectError::Static(cat))) => {
                // Retrying cannot help, as with a bad handshake request.
                self.metrics.record_connection_failed(cat);
                Handshake::Stop
            }
            Ok(Err(ConnectError::Tungstenite(err))) => {
                self.metrics
                    .record_connection_failed(categorize_tungstenite_error(&err));
                Handshake::Failed
            }
            Err(_) => {
                self.metrics
                    .record_connection_failed(ErrorCategory::Timeout);
                Handshake::Failed
            }
        }
    }

    /// The session loop that drives one established connection.
    ///
    /// With capture on, the socket is wrapped so every frame is recorded.
    /// Without it the loops run on the bare socket, so a run that does not
    /// capture pays nothing for the feature.
    ///
    /// The loop is boxed, and built here rather than in an `async fn`, so
    /// the connection's task holds only the state of its own mode's loop.
    /// Awaited inline, every connection would reserve room for the largest
    /// loop and for the handshake, about 12 KB instead of about 3 KB.
    fn session(&mut self, ws_stream: crate::connect::WsStream) -> BoxFuture<'_, SessionEnd> {
        #[cfg(feature = "capture")]
        if let Some(capture) = self.capture.clone() {
            let stream = crate::capture::CaptureStream::open(ws_stream, capture, self.worker_id);
            return self.mode_loop(stream);
        }
        self.mode_loop(ws_stream)
    }

    fn mode_loop<'a, S>(&'a mut self, ws_stream: S) -> BoxFuture<'a, SessionEnd>
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin
            + Send
            + 'a,
    {
        #[cfg(feature = "scenario")]
        if let Some(phase) = self.scenario.clone()
            && !phase.actions.is_empty()
        {
            return Box::pin(self.run_action_loop(ws_stream, phase));
        }

        match self.config.mode {
            TestMode::Echo => Box::pin(self.run_echo_loop(ws_stream)),
            TestMode::Stream => Box::pin(self.run_stream_loop(ws_stream)),
            TestMode::Listen => Box::pin(self.run_listen_loop(ws_stream)),
            #[cfg(feature = "graphql-ws")]
            TestMode::GraphQLWS => Box::pin(self.run_graphql_ws_loop(ws_stream)),
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

        req.headers_mut().extend(self.config.headers.clone());
        // Checked with every row of the data file when the run was
        // configured, so a value fails here only if it is not valid HTTP.
        for (name, template) in &self.config.header_templates {
            let value = http::HeaderValue::from_str(&template.render(self.template_values(0)))
                .map_err(|_| ErrorCategory::Other)?;
            req.headers_mut().append(name.clone(), value);
        }

        if let Some(subproto) = &self.config.subprotocol
            && let Ok(val) = http::HeaderValue::from_str(subproto)
        {
            req.headers_mut().insert(
                http::header::HeaderName::from_static("sec-websocket-protocol"),
                val,
            );
        }

        Ok(req)
    }

    async fn run_echo_loop<S>(&mut self, ws_stream: S) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (mut sink, mut stream) = ws_stream.split();

        let mut seq = 0u64;

        // Paced sends follow a fixed schedule: request n is due at
        // `session start + n * period`. A request is never sent before it is
        // due, and one that falls behind (because an earlier reply was slow)
        // is sent as soon as the previous reply arrives, so the configured
        // rate is kept on average. Response time is RTT plus the time the
        // request spent waiting past its due time behind a slow one, less the
        // timer's own slack; RTT is measured from the actual send. See
        // METHODOLOGY.md.
        let mut schedule = SendSchedule::new(self.config.rate_per_conn);
        // Think time is a deliberate pause after each reply, not queueing, so
        // response time is only recorded without it.
        #[cfg(feature = "scenario")]
        let record_response_time = self
            .scenario
            .as_ref()
            .is_none_or(|phase| phase.think_time.is_zero());
        #[cfg(not(feature = "scenario"))]
        let record_response_time = true;
        self.metrics.interval_uses_response = schedule.is_some() && record_response_time;

        let mut ping_ticker = self.keepalive_ticker();

        'session: loop {
            if self.cancel_token.is_cancelled() {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }

            let due = schedule.as_ref().map(|s| s.next_due);
            if let Some(due) = due {
                // Keepalive ticks that land while waiting for the pacing slot
                // only send a ping; the data send still waits for the slot.
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(due.into()) => break,
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

            if !self.claim_send() {
                self.cancel_token.cancel();
                let _ = sink.send(Message::Close(None)).await;
                break;
            }

            let plan = self.chaos_plan();
            if plan.drop {
                // The slot is used up; nothing is sent and no reply awaited.
                self.metrics.record_chaos_dropped();
                seq += 1;
                if let Some(schedule) = schedule.as_mut() {
                    schedule.advance();
                }
                continue;
            }
            let mut due = due;
            if !plan.delay.is_zero() {
                if !self.chaos_delay(plan.delay).await {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                // The delay moves this request's slot and every later one,
                // so it is never counted as response time.
                if let Some(schedule) = schedule.as_mut() {
                    schedule.shift(plan.delay);
                }
                due = due.map(|due| due + plan.delay);
            }

            let (msg, corrupted) = self.chaos_corrupt(self.render_payload(seq), plan.corrupt);
            let frame_len = message_byte_len(&msg);
            let sent = self.retain_for_exact_match(&msg);
            let start_time = Instant::now();

            tokio::select! {
                res = sink.send(msg) => {
                    if let Err(err) = res {
                        self.metrics.record_error(
                            categorize_session_error(&err, ErrorDirection::Write),
                            ErrorScope::Attempt,
                        );
                        break;
                    }
                    self.metrics.record_message_sent(frame_len);
                }
                _ = self.cancel_token.cancelled() => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }

            let mut unsolicited = 0u64;
            let recv_fut = self.await_reply(
                &mut stream,
                &mut sink,
                sent.as_ref(),
                seq,
                corrupted,
                &mut unsolicited,
            );

            // Cancellation wins over a simultaneously ready reply so nothing
            // after the test window is recorded.
            let response = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => None,
                res = tokio::time::timeout(self.config.message_timeout, recv_fut) => Some(res),
            };
            self.metrics.record_unsolicited_frames(unsolicited);
            let Some(response) = response else {
                let _ = sink.send(Message::Close(None)).await;
                break;
            };

            match response {
                Ok(Ok(Some(reply))) => {
                    let received_at = Instant::now();
                    let rtt = received_at - start_time;
                    self.metrics
                        .record_message_recv(message_byte_len(&reply), Some(rtt));
                    if !corrupted {
                        self.check_reply(&reply, None, &[]);
                    }
                    if let Some(due) = due
                        && record_response_time
                    {
                        self.metrics
                            .record_response_time(SendSchedule::response_time(
                                due, start_time, rtt,
                            ));
                    }
                    if let Some(graceful) = self.chaos_close_now() {
                        return self.chaos_close(&mut sink, graceful).await;
                    }
                }
                // A server Close ends the session; it is not the reply.
                Ok(Ok(None)) => break,
                Ok(Err(cat)) => {
                    self.metrics.record_error(cat, ErrorScope::SentMessage);
                    break;
                }
                Err(_) => {
                    self.metrics
                        .record_error(ErrorCategory::Timeout, ErrorScope::SentMessage);
                    break;
                }
            }

            seq += 1;
            if let Some(schedule) = schedule.as_mut() {
                schedule.advance();
            }

            // A slow consumer takes its read pause after the reply is
            // recorded. Like a send delay, it moves every later slot, so it
            // is never counted as response time.
            if let Some(pause) = self.chaos_read_pause() {
                if !self.chaos_pause(pause).await {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                if let Some(schedule) = schedule.as_mut() {
                    schedule.shift(pause);
                }
            }

            #[cfg(feature = "scenario")]
            if !self.apply_think_time().await {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
            // After think time the schedule restarts from now, so the pause
            // is not counted as lateness.
            #[cfg(feature = "scenario")]
            if !record_response_time && let Some(schedule) = schedule.as_mut() {
                schedule.restart_after_pause();
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
        let (mut sink, mut stream) = ws_stream.split();

        let mut seq = 0u64;
        let mut ping_ticker = self.keepalive_ticker();

        let pacing_duration = if self.config.rate_per_conn > 0 {
            Duration::from_secs_f64(1.0 / self.config.rate_per_conn as f64)
        } else {
            Duration::from_micros(10)
        };

        let mut ticker = interval(pacing_duration);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut read_resume: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !self.claim_send() {
                        self.cancel_token.cancel();
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                    let plan = self.chaos_plan();
                    if plan.drop {
                        self.metrics.record_chaos_dropped();
                        seq += 1;
                        continue;
                    }
                    if !plan.delay.is_zero() && !self.chaos_delay(plan.delay).await {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                    let (msg, _) = self.chaos_corrupt(self.render_payload(seq), plan.corrupt);
                    let len = message_byte_len(&msg);
                    if let Err(e) = sink.send(msg).await {
                        self.metrics.record_error(
                            categorize_session_error(&e, ErrorDirection::Write),
                            ErrorScope::Attempt,
                        );
                        break;
                    }
                    self.metrics.record_message_sent(len);
                    seq += 1;
                    if let Some(graceful) = self.chaos_close_now() {
                        return self.chaos_close(&mut sink, graceful).await;
                    }

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
                // A slow consumer stops reading until its pause ends; sends
                // and keepalives go on, and the server's frames wait in the
                // socket buffers.
                () = sleep_until_resume(read_resume), if read_resume.is_some() => {
                    read_resume = None;
                }
                msg_opt = stream.next(), if read_resume.is_none() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => {
                            self.metrics.record_message_recv(t.len(), None);
                            read_resume = self.chaos_read_pause().map(|p| Instant::now() + p);
                        }
                        Some(Ok(Message::Binary(b))) => {
                            self.metrics.record_message_recv(b.len(), None);
                            read_resume = self.chaos_read_pause().map(|p| Instant::now() + p);
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(
                                categorize_session_error(&e, ErrorDirection::Read),
                                ErrorScope::Attempt,
                            );
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
        let (mut sink, mut stream) = ws_stream.split();

        let mut ping_ticker = self.keepalive_ticker();
        let mut read_resume: Option<Instant> = None;

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
                // A slow consumer stops reading until its pause ends; sends
                // and keepalives go on, and the server's frames wait in the
                // socket buffers.
                () = sleep_until_resume(read_resume), if read_resume.is_some() => {
                    read_resume = None;
                }
                msg_opt = stream.next(), if read_resume.is_none() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => {
                            self.metrics.record_message_recv(t.len(), None);
                            read_resume = self.chaos_read_pause().map(|p| Instant::now() + p);
                        }
                        Some(Ok(Message::Binary(b))) => {
                            self.metrics.record_message_recv(b.len(), None);
                            read_resume = self.chaos_read_pause().map(|p| Instant::now() + p);
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(
                                categorize_session_error(&e, ErrorDirection::Read),
                                ErrorScope::Attempt,
                            );
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
    ///
    /// In echo mode paced sends follow the fixed schedule of the paced echo
    /// loop and record response time; `Wait`, `Ping` and think time are
    /// deliberate pauses, after which the schedule restarts from now. Other
    /// modes pace sends with an interval that skips missed ticks.
    #[cfg(feature = "scenario")]
    async fn run_action_loop<S>(&mut self, ws_stream: S, phase: Arc<ScenarioPhase>) -> SessionEnd
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (mut sink, mut stream) = ws_stream.split();
        // Captured values belong to one connection: a reconnect starts
        // without them, as a new session would.
        self.vars = vec![String::new(); phase.vars.len()];

        let mut seq = 0u64;
        let echo = self.config.mode == TestMode::Echo;
        let mut schedule = if echo {
            SendSchedule::new(self.config.rate_per_conn)
        } else {
            None
        };
        let mut ticker = if !echo && self.config.rate_per_conn > 0 {
            let mut t = interval(Duration::from_secs_f64(
                1.0 / self.config.rate_per_conn as f64,
            ));
            t.set_missed_tick_behavior(MissedTickBehavior::Skip);
            Some(t)
        } else {
            None
        };
        self.metrics.interval_uses_response = schedule.is_some();

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
                    ScenarioAction::Send {
                        payload,
                        count,
                        expect,
                        captures,
                        name,
                    } => {
                        // Sends, latencies and errors until the burst ends
                        // also count for the action's name.
                        self.metrics.current_action = *name;
                        for _ in 0..*count {
                            if self.cancel_token.is_cancelled() {
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }

                            let due = schedule.as_ref().map(|s| s.next_due);
                            if let Some(due) = due {
                                tokio::select! {
                                    _ = tokio::time::sleep_until(due.into()) => {}
                                    _ = self.cancel_token.cancelled() => {
                                        let _ = sink.send(Message::Close(None)).await;
                                        break 'script;
                                    }
                                }
                            } else if let Some(ref mut t) = ticker {
                                tokio::select! {
                                    _ = t.tick() => {}
                                    _ = self.cancel_token.cancelled() => {
                                        let _ = sink.send(Message::Close(None)).await;
                                        break 'script;
                                    }
                                }
                            }

                            if !self.claim_send() {
                                self.cancel_token.cancel();
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }

                            let plan = self.chaos_plan();
                            if plan.drop {
                                self.metrics.record_chaos_dropped();
                                seq += 1;
                                if let Some(schedule) = schedule.as_mut() {
                                    schedule.advance();
                                }
                                continue;
                            }
                            let mut due = due;
                            if !plan.delay.is_zero() {
                                if !self.chaos_delay(plan.delay).await {
                                    let _ = sink.send(Message::Close(None)).await;
                                    break 'script;
                                }
                                if let Some(schedule) = schedule.as_mut() {
                                    schedule.shift(plan.delay);
                                }
                                due = due.map(|due| due + plan.delay);
                            }

                            let active_payload = payload.as_ref().unwrap_or(&self.config.payload);
                            let msg = self.render_message(active_payload, seq);
                            let (msg, corrupted) = self.chaos_corrupt(msg, plan.corrupt);
                            let frame_len = message_byte_len(&msg);
                            let sent = self.retain_for_exact_match(&msg);
                            let start_time = Instant::now();

                            if let Err(err) = sink.send(msg).await {
                                self.metrics.record_error(
                                    categorize_session_error(&err, ErrorDirection::Write),
                                    ErrorScope::Attempt,
                                );
                                break 'script;
                            }

                            self.metrics.record_message_sent(frame_len);

                            if self.config.mode == TestMode::Echo {
                                let mut unsolicited = 0u64;
                                let recv_fut = self.await_reply(
                                    &mut stream,
                                    &mut sink,
                                    sent.as_ref(),
                                    seq,
                                    corrupted,
                                    &mut unsolicited,
                                );

                                let response = tokio::select! {
                                    biased;
                                    _ = self.cancel_token.cancelled() => None,
                                    res = tokio::time::timeout(
                                        self.config.message_timeout,
                                        recv_fut,
                                    ) => Some(res),
                                };
                                self.metrics.record_unsolicited_frames(unsolicited);
                                let Some(response) = response else {
                                    let _ = sink.send(Message::Close(None)).await;
                                    break 'script;
                                };

                                match response {
                                    Ok(Ok(Some(reply))) => {
                                        let rtt = start_time.elapsed();
                                        self.metrics.record_message_recv(
                                            message_byte_len(&reply),
                                            Some(rtt),
                                        );
                                        // A corrupted request may well get
                                        // a reply that fails its checks;
                                        // that is the fault, not the server.
                                        if !corrupted {
                                            self.check_reply(&reply, expect.as_ref(), captures);
                                        }
                                        if let Some(due) = due {
                                            self.metrics.record_response_time(
                                                SendSchedule::response_time(due, start_time, rtt),
                                            );
                                        }
                                        if let Some(pause) = self.chaos_read_pause() {
                                            if !self.chaos_pause(pause).await {
                                                let _ = sink.send(Message::Close(None)).await;
                                                break 'script;
                                            }
                                            if let Some(schedule) = schedule.as_mut() {
                                                schedule.shift(pause);
                                            }
                                        }
                                    }
                                    Ok(Ok(None)) => break 'script,
                                    Ok(Err(cat)) => {
                                        self.metrics.record_error(cat, ErrorScope::SentMessage);
                                        break 'script;
                                    }
                                    Err(_) => {
                                        self.metrics.record_error(
                                            ErrorCategory::Timeout,
                                            ErrorScope::SentMessage,
                                        );
                                        break 'script;
                                    }
                                }
                            }

                            seq += 1;
                            if let Some(schedule) = schedule.as_mut() {
                                schedule.advance();
                            }
                            // After the reply in echo mode, after the send in
                            // the other modes.
                            if let Some(graceful) = self.chaos_close_now() {
                                return self.chaos_close(&mut sink, graceful).await;
                            }
                        }
                        self.metrics.current_action = None;

                        if !self.apply_think_time().await {
                            let _ = sink.send(Message::Close(None)).await;
                            break 'script;
                        }
                        if !phase.think_time.is_zero()
                            && let Some(schedule) = schedule.as_mut()
                        {
                            schedule.restart_after_pause();
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
                        if let Some(schedule) = schedule.as_mut() {
                            schedule.restart_after_pause();
                        }
                    }
                    ScenarioAction::Ping(interval) => {
                        if let Err(err) = sink.send(Message::Ping(Bytes::from_static(b"hb"))).await
                        {
                            self.metrics.record_error(
                                categorize_session_error(&err, ErrorDirection::Write),
                                ErrorScope::Attempt,
                            );
                            break 'script;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(*interval) => {}
                            _ = self.cancel_token.cancelled() => {
                                let _ = sink.send(Message::Close(None)).await;
                                break 'script;
                            }
                        }
                        if let Some(schedule) = schedule.as_mut() {
                            schedule.restart_after_pause();
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
            .is_some_and(|max| self.metrics.live.requests_exhausted(max))
    }

    /// Claims the next send against `--requests`. Returns false once the
    /// limit is used up; always true without a limit.
    fn claim_send(&self) -> bool {
        self.config
            .max_requests
            .is_none_or(|max| self.metrics.live.claim_request(max))
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

    /// Keeps a handle on the outgoing frame when the reply must equal it.
    ///
    /// Frames hold refcounted buffers, so this never copies the payload.
    fn retain_for_exact_match(&self, msg: &Message) -> Option<Message> {
        (self.config.reply_match == ReplyMatch::Exact).then(|| msg.clone())
    }

    /// Reads frames until one matches `--match` for request `seq`, answering
    /// pings on the way. Returns the reply size, or `None` on a server Close.
    ///
    /// The reply to a request that chaos `corrupted` is matched against the
    /// corrupted frame with `exact`; with `json:POINTER` the first data frame
    /// is the reply, since the corruption may have changed the value at the
    /// pointer.
    ///
    /// Non-matching data frames are added to `unsolicited`, which the caller
    /// records even when this future is dropped by a timeout or cancellation.
    async fn await_reply<St, Si>(
        &self,
        stream: &mut St,
        sink: &mut Si,
        sent: Option<&Message>,
        seq: u64,
        corrupted: bool,
        unsolicited: &mut u64,
    ) -> Result<Option<Message>, ErrorCategory>
    where
        St: futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>> + Unpin,
        Si: futures_util::Sink<Message, Error = TungsteniteError> + Unpin,
    {
        while let Some(msg_res) = stream.next().await {
            let frame = match msg_res {
                Ok(frame @ (Message::Text(_) | Message::Binary(_))) => frame,
                Ok(Message::Ping(data)) => {
                    let _ = sink.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Message::Close(_)) => return Ok(None),
                Ok(Message::Pong(_) | Message::Frame(_)) => continue,
                Err(e) => return Err(categorize_session_error(&e, ErrorDirection::Read)),
            };
            let rule = match &self.config.reply_match {
                ReplyMatch::JsonPointer(_) if corrupted => &ReplyMatch::First,
                rule => rule,
            };
            if reply_matches(rule, &frame, sent, seq) {
                return Ok(Some(frame));
            }
            *unsolicited += 1;
        }
        Err(ErrorCategory::UnexpectedClose)
    }

    /// What chaos does to the next data send; nothing without chaos.
    #[inline]
    fn chaos_plan(&mut self) -> SendPlan {
        #[cfg(feature = "chaos")]
        if let Some(chaos) = self.chaos.as_mut() {
            return chaos.plan_send();
        }
        SendPlan::default()
    }

    /// Waits out a chaos delay before a send. Returns false when the run
    /// was cancelled meanwhile.
    async fn chaos_delay(&mut self, delay: Duration) -> bool {
        self.metrics.record_chaos_delayed();
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.cancel_token.cancelled() => false,
        }
    }

    /// The pause chaos takes after a received data frame, counted; `None`
    /// without `--chaos-read-pause`.
    #[inline]
    fn chaos_read_pause(&mut self) -> Option<Duration> {
        #[cfg(feature = "chaos")]
        if let Some(pause) = self.chaos.as_mut().and_then(|chaos| chaos.read_pause()) {
            self.metrics.record_chaos_read_pause();
            return Some(pause);
        }
        None
    }

    /// Sleeps a chaos read pause between a reply and the next send. Returns
    /// false when the run was cancelled meanwhile.
    async fn chaos_pause(&self, pause: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(pause) => true,
            _ = self.cancel_token.cancelled() => false,
        }
    }

    /// Corrupts `msg` when the plan says so. Returns the frame to send and
    /// whether chaos changed it.
    #[inline]
    fn chaos_corrupt(&mut self, msg: Message, corrupt: bool) -> (Message, bool) {
        #[cfg(feature = "chaos")]
        if corrupt && let Some(chaos) = self.chaos.as_mut() {
            let (msg, changed) = chaos.corrupt(msg);
            if changed {
                self.metrics.record_chaos_corrupted();
            }
            return (msg, changed);
        }
        let _ = corrupt;
        (msg, false)
    }

    /// Whether chaos closes the connection now; `Some(graceful)` when it
    /// does.
    #[inline]
    fn chaos_close_now(&mut self) -> Option<bool> {
        #[cfg(feature = "chaos")]
        if let Some(chaos) = self.chaos.as_mut() {
            return chaos.close_now();
        }
        None
    }

    /// Ends the session for chaos: with a Close frame with code 1001 when
    /// `graceful`, else by dropping the TCP connection without a Close
    /// frame. The connection counts as lost, so the worker reconnects
    /// unless the run ends or `--no-reconnect` is set; it is not an error.
    async fn chaos_close<Si>(&mut self, sink: &mut Si, graceful: bool) -> SessionEnd
    where
        Si: futures_util::Sink<Message, Error = TungsteniteError> + Unpin,
    {
        self.metrics.record_chaos_close();
        if graceful {
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Away,
                    reason: Utf8Bytes::from_static("wsblast chaos close"),
                })))
                .await;
        }
        self.session_end()
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

    /// Applies `--expect`, the action's `expect` and its captures to a
    /// reply, and counts one `check_failed` error when any of them fails.
    /// A capture that finds nothing leaves its variable as it was.
    fn check_reply(
        &mut self,
        reply: &Message,
        expect: Option<&crate::check::Check>,
        captures: &[crate::check::Capture],
    ) {
        if self.config.expect.is_none() && expect.is_none() && captures.is_empty() {
            return;
        }
        let reply = crate::check::Reply::new(reply);
        let mut passed = self
            .config
            .expect
            .as_ref()
            .is_none_or(|check| check.passes(&reply))
            && expect.is_none_or(|check| check.passes(&reply));
        for capture in captures {
            match (capture.value(&reply), self.vars.get_mut(capture.var)) {
                (Some(value), Some(var)) => *var = value,
                _ => passed = false,
            }
        }
        if !passed {
            self.metrics
                .record_error(ErrorCategory::CheckFailed, ErrorScope::SentMessage);
        }
    }

    /// The values this connection fills templates with for frame `seq`.
    fn template_values(&self, seq: u64) -> crate::template::Values<'_> {
        crate::template::Values {
            seq,
            worker_id: self.worker_id,
            data: self
                .config
                .data
                .as_deref()
                .map(|data| (data, data.row_for(self.worker_id))),
            vars: &self.vars,
        }
    }

    fn render_message(&self, payload: &PayloadConfig, seq: u64) -> Message {
        match payload {
            PayloadConfig::Binary(bytes) => Message::Binary(bytes.clone()),
            PayloadConfig::StaticText(utf8) => Message::Text(utf8.clone()),
            PayloadConfig::DynamicText(template) => {
                Message::Text(Utf8Bytes::from(template.render(self.template_values(seq))))
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
        let (mut sink, mut stream) = ws_stream.split();

        let mut ping_ticker = self.keepalive_ticker();

        let graphql_config = match &self.config.graphql_ws {
            Some(cfg) => cfg,
            None => {
                self.metrics
                    .record_error(ErrorCategory::ProtocolError, ErrorScope::Attempt);
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
                self.metrics
                    .record_error(ErrorCategory::ProtocolError, ErrorScope::Attempt);
                return SessionEnd::Finished;
            }
        };
        let init_len = init_json.len();
        let init_message = Message::Text(Utf8Bytes::from(init_json));
        if let Err(e) = sink.send(init_message).await {
            self.metrics.record_error(
                categorize_session_error(&e, ErrorDirection::Write),
                ErrorScope::Attempt,
            );
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
                    self.metrics.record_error(ErrorCategory::Timeout, ErrorScope::SentMessage);
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
                            if let Ok(msg) = serde_json::from_str::<GraphQLWSMessage>(&text) {
                                match msg {
                                    GraphQLWSMessage::ConnectionAck => {
                                        break;
                                    }
                                    GraphQLWSMessage::ConnectionError { .. } => {
                                        self.metrics.record_error(
                                            ErrorCategory::ProtocolError,
                                            ErrorScope::SentMessage,
                                        );
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
                            self.metrics.record_error(
                                ErrorCategory::UnexpectedClose,
                                ErrorScope::SentMessage,
                            );
                            return self.session_end();
                        }
                        Some(Err(e)) => {
                            self.metrics.record_error(
                                categorize_session_error(&e, ErrorDirection::Read),
                                ErrorScope::SentMessage,
                            );
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
                self.metrics
                    .record_error(ErrorCategory::ProtocolError, ErrorScope::Attempt);
                return SessionEnd::Finished;
            }
        };
        let start_len = start_json.len();
        let start_message = Message::Text(Utf8Bytes::from(start_json));
        if let Err(e) = sink.send(start_message).await {
            self.metrics.record_error(
                categorize_session_error(&e, ErrorDirection::Write),
                ErrorScope::Attempt,
            );
            return self.session_end();
        }
        self.metrics.record_message_sent(start_len);

        // Handle incoming messages (data, error, complete, ka).
        let mut subscription_complete = false;
        let mut read_resume: Option<Instant> = None;
        let mut cancelled = false;
        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    cancelled = true;
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
                () = sleep_until_resume(read_resume), if read_resume.is_some() => {
                    read_resume = None;
                }
                msg_result = stream.next(), if read_resume.is_none() => {
                    match msg_result {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(msg) = serde_json::from_str::<GraphQLWSMessage>(&text) {
                                match msg {
                                    GraphQLWSMessage::Data { id: _, payload: _ }
                                    | GraphQLWSMessage::Next { id: _, payload: _ } => {
                                        self.metrics.record_message_recv(text.len(), None);
                                        read_resume = self
                                            .chaos_read_pause()
                                            .map(|p| Instant::now() + p);
                                    }
                                    GraphQLWSMessage::Error { .. } => {
                                        self.metrics.record_error(
                                            ErrorCategory::ProtocolError,
                                            ErrorScope::Attempt,
                                        );
                                    }
                                    GraphQLWSMessage::ConnectionError { .. } => {
                                        self.metrics.record_error(
                                            ErrorCategory::ProtocolError,
                                            ErrorScope::Attempt,
                                        );
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
                            self.metrics.record_error(
                                categorize_session_error(&e, ErrorDirection::Read),
                                ErrorScope::Attempt,
                            );
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
                self.metrics
                    .record_error(ErrorCategory::ProtocolError, ErrorScope::Attempt);
                return SessionEnd::Finished;
            }
        };
        let stop_message = Message::Text(Utf8Bytes::from(stop_json));
        let _ = sink.send(stop_message).await;

        // Ending an open subscription because the run is over is not a failure.
        if cancelled {
            let _ = sink.send(Message::Close(None)).await;
            return SessionEnd::Finished;
        }

        if !subscription_complete {
            // Connection closed unexpectedly
            self.metrics
                .record_error(ErrorCategory::UnexpectedClose, ErrorScope::Attempt);
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

/// Decides whether an inbound data frame answers the request in flight.
///
/// `sent` is only required by [`ReplyMatch::Exact`]; JSON parsing happens
/// only for [`ReplyMatch::JsonPointer`], so the default rule stays
/// allocation-free.
fn reply_matches(rule: &ReplyMatch, frame: &Message, sent: Option<&Message>, seq: u64) -> bool {
    match rule {
        ReplyMatch::First => true,
        ReplyMatch::Exact => sent == Some(frame),
        ReplyMatch::JsonPointer(pointer) => {
            let data: &[u8] = match frame {
                Message::Text(text) => text.as_bytes(),
                Message::Binary(bytes) => bytes,
                _ => return false,
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
                return false;
            };
            match value.pointer(pointer) {
                Some(serde_json::Value::Number(number)) => number.as_u64() == Some(seq),
                Some(serde_json::Value::String(text)) => text.parse::<u64>() == Ok(seq),
                _ => false,
            }
        }
    }
}

/// Waits until a slow consumer's read pause ends; never ready without one.
async fn sleep_until_resume(resume: Option<Instant>) {
    match resume {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
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
            // tokio-rustls reports a failed handshake as an I/O error wrapping
            // the rustls error, not as `TungsteniteError::Tls`.
            if matches!(direction, ErrorDirection::Connect)
                && io_err
                    .get_ref()
                    .is_some_and(|inner| inner.is::<rustls::Error>())
            {
                return ErrorCategory::TlsHandshake;
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
/// How late a paced send can be from timer wake-up alone. Tokio timers fire
/// on the next millisecond tick after their deadline, and Windows wakes them
/// at its default 15.6 ms timer resolution. Lateness up to this slack is the
/// client's own timer, not time spent waiting behind a slow reply, so it is
/// left out of response time.
const TIMER_SLACK: Duration = if cfg!(windows) {
    Duration::from_millis(16)
} else {
    Duration::from_millis(2)
};

/// What chaos does to one data send. The default does nothing, which is
/// all a run without chaos ever sees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SendPlan {
    /// Pause before the send.
    pub(crate) delay: Duration,
    /// Skip the send.
    pub(crate) drop: bool,
    /// Corrupt the payload.
    pub(crate) corrupt: bool,
}

/// What one handshake attempt of a connection slot produced.
// The socket is not boxed: the value lives only from the handshake to the
// boxed session loop it moves into, and a box would cost an allocation.
#[allow(clippy::large_enum_variant)]
enum Handshake {
    Open(crate::connect::WsStream),
    /// The attempt failed; the slot may retry.
    Failed,
    /// The run ended, or the failure is one a retry cannot fix.
    Stop,
}

/// The send times of a paced echo session or echo action script.
///
/// Request `n` of a session is due at `session start + n * period`. A
/// request is never sent before it is due; one that falls behind because an
/// earlier reply was slow is sent as soon as that reply arrives, and the
/// time it waited past its due time is part of its response time.
struct SendSchedule {
    period: Duration,
    /// When the next request is due.
    next_due: Instant,
}

impl SendSchedule {
    /// A schedule of `rate` requests per second starting now; `None` for an
    /// unpaced session (`rate` 0).
    fn new(rate: u64) -> Option<Self> {
        (rate > 0).then(|| Self {
            period: Duration::from_secs_f64(1.0 / rate as f64),
            next_due: Instant::now(),
        })
    }

    /// Moves on to the next request's slot.
    fn advance(&mut self) {
        self.next_due += self.period;
    }

    /// Moves the next slot, and with it every later one, by `delay`.
    fn shift(&mut self, delay: Duration) {
        self.next_due += delay;
    }

    /// Restarts the schedule from now after a deliberate pause (think time,
    /// a `Wait` or `Ping` action), so the pause is not counted as lateness.
    #[cfg(feature = "scenario")]
    fn restart_after_pause(&mut self) {
        self.next_due = self.next_due.max(Instant::now());
    }

    /// The response time of a request that was due at `due`, sent at
    /// `sent_at` and answered `rtt` after that: RTT plus the time it waited
    /// past its due time, less the timer's own slack.
    fn response_time(due: Instant, sent_at: Instant, rtt: Duration) -> Duration {
        let late = sent_at.saturating_duration_since(due);
        rtt + late.saturating_sub(TIMER_SLACK)
    }
}

fn categorize_tungstenite_error(err: &TungsteniteError) -> ErrorCategory {
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

#[cfg(test)]
mod reply_match_tests {
    use super::*;

    fn text(raw: &'static str) -> Message {
        Message::Text(Utf8Bytes::from_static(raw))
    }

    #[test]
    fn first_accepts_any_data_frame() {
        let rule = ReplyMatch::First;
        assert!(reply_matches(&rule, &text("anything"), None, 7));
        assert!(reply_matches(
            &rule,
            &Message::Binary(Bytes::from_static(b"\x00")),
            None,
            7
        ));
    }

    #[test]
    fn exact_requires_identical_frame_type_and_bytes() {
        let rule = ReplyMatch::Exact;
        let sent = text("ping");
        assert!(reply_matches(&rule, &text("ping"), Some(&sent), 0));
        assert!(!reply_matches(&rule, &text("pong"), Some(&sent), 0));
        assert!(!reply_matches(
            &rule,
            &Message::Binary(Bytes::from_static(b"ping")),
            Some(&sent),
            0
        ));
    }

    #[test]
    fn json_pointer_compares_numbers_and_numeric_strings_with_seq() {
        let rule = ReplyMatch::JsonPointer("/data/seq".to_string());
        assert!(reply_matches(
            &rule,
            &text(r#"{"data":{"seq":7}}"#),
            None,
            7
        ));
        assert!(reply_matches(
            &rule,
            &text(r#"{"data":{"seq":"7"}}"#),
            None,
            7
        ));
        assert!(reply_matches(
            &rule,
            &Message::Binary(Bytes::from_static(br#"{"data":{"seq":7}}"#)),
            None,
            7
        ));

        assert!(!reply_matches(
            &rule,
            &text(r#"{"data":{"seq":6}}"#),
            None,
            7
        ));
        assert!(!reply_matches(
            &rule,
            &text(r#"{"data":{"seq":"x"}}"#),
            None,
            7
        ));
        assert!(!reply_matches(
            &rule,
            &text(r#"{"data":{"seq":7.5}}"#),
            None,
            7
        ));
        assert!(!reply_matches(
            &rule,
            &text(r#"{"event":"heartbeat"}"#),
            None,
            7
        ));
        assert!(!reply_matches(&rule, &text("not json"), None, 7));
    }
}
