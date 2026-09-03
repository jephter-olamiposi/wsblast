//! Per-connection asynchronous WebSocket worker session lifecycle.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig};
use crate::error::ErrorCategory;
use crate::metrics::WorkerMetrics;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_util::sync::CancellationToken;

/// Represents a single asynchronous WebSocket client worker session.
pub struct WorkerSession {
    pub worker_id: usize,
    pub config: Arc<LoadTestConfig>,
    pub cancel_token: CancellationToken,
    pub metrics: WorkerMetrics,
}

impl WorkerSession {
    pub fn new(
        worker_id: usize,
        config: Arc<LoadTestConfig>,
        cancel_token: CancellationToken,
        metrics: WorkerMetrics,
    ) -> Self {
        Self {
            worker_id,
            config,
            cancel_token,
            metrics,
        }
    }

    /// Executes the full worker lifecycle: TCP+TLS handshake -> session loop -> graceful close.
    pub async fn run(mut self) -> WorkerMetrics {
        let handshake_start = Instant::now();
        let client_req = match self.build_handshake_request() {
            Ok(req) => req,
            Err(cat) => {
                self.metrics.record_connection_failed(cat);
                return self.metrics;
            }
        };

        let connect_fut = tokio_tungstenite::connect_async(client_req);
        let connect_result = tokio::time::timeout(self.config.connect_timeout, connect_fut).await;

        let (ws_stream, _) = match connect_result {
            Ok(Ok(stream_and_resp)) => stream_and_resp,
            Ok(Err(err)) => {
                let cat = categorize_tungstenite_error(&err);
                self.metrics.record_connection_failed(cat);
                return self.metrics;
            }
            Err(_) => {
                self.metrics
                    .record_connection_failed(ErrorCategory::Timeout);
                return self.metrics;
            }
        };

        let handshake_dur = handshake_start.elapsed();
        self.metrics.record_handshake(handshake_dur);

        match self.config.mode {
            TestMode::Echo => self.run_echo_loop(ws_stream).await,
            TestMode::Stream => self.run_stream_loop(ws_stream).await,
            TestMode::Listen => self.run_listen_loop(ws_stream).await,
        }

        self.metrics.record_connection_closed();
        self.metrics
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
                req.headers_mut().insert("Sec-WebSocket-Protocol", val);
            }
        }

        Ok(req)
    }

    async fn run_echo_loop<S>(&mut self, ws_stream: S)
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (mut sink, mut stream) = ws_stream.split();
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

        let mut ping_ticker = if self.config.ping_interval > Duration::ZERO {
            let mut pt = interval(self.config.ping_interval);
            pt.set_missed_tick_behavior(MissedTickBehavior::Skip);
            Some(pt)
        } else {
            None
        };

        loop {
            if self.cancel_token.is_cancelled() {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }

            let has_ping = ping_ticker.is_some();
            if let Some(ref mut t) = ticker {
                tokio::select! {
                    _ = t.tick() => {}
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
                        break;
                    }
                }
            } else if has_ping {
                tokio::select! {
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
                        break;
                    }
                }
            }

            let msg = self.render_payload(seq);
            let frame_len = message_byte_len(&msg);
            let start_time = Instant::now();

            tokio::select! {
                res = sink.send(msg) => {
                    if let Err(err) = res {
                        self.metrics.record_error(categorize_tungstenite_error(&err));
                        break;
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
                        Ok(Message::Text(txt)) => return Ok((txt.len(), true)),
                        Ok(Message::Binary(bin)) => return Ok((bin.len(), true)),
                        Ok(Message::Ping(data)) => {
                            let _ = sink.send(Message::Pong(data)).await;
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => return Ok((0, false)),
                        Ok(Message::Frame(_)) => {}
                        Err(e) => return Err(categorize_tungstenite_error(&e)),
                    }
                }
                Err(ErrorCategory::UnexpectedClose)
            };

            let response = tokio::time::timeout(self.config.message_timeout, recv_fut).await;

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
        }
    }

    async fn run_stream_loop<S>(&mut self, ws_stream: S)
    where
        S: futures_util::Sink<Message, Error = TungsteniteError>
            + futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>>
            + Unpin,
    {
        let (mut sink, mut stream) = ws_stream.split();
        let mut seq = 0u64;

        let pacing_duration = if self.config.rate_per_conn > 0 {
            Duration::from_secs_f64(1.0 / self.config.rate_per_conn as f64)
        } else {
            Duration::from_micros(10)
        };

        let mut ticker = interval(pacing_duration);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let msg = self.render_payload(seq);
                    let len = message_byte_len(&msg);
                    if let Err(e) = sink.send(msg).await {
                        self.metrics.record_error(categorize_tungstenite_error(&e));
                        break;
                    }
                    self.metrics.record_message_sent(len);
                    seq += 1;
                }
                msg_opt = stream.next() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => self.metrics.record_message_recv(t.len(), None),
                        Some(Ok(Message::Binary(b))) => self.metrics.record_message_recv(b.len(), None),
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(categorize_tungstenite_error(&e));
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
    }

    async fn run_listen_loop<S>(&mut self, ws_stream: S)
    where
        S: futures_util::Stream<Item = std::result::Result<Message, TungsteniteError>> + Unpin,
    {
        let mut stream = ws_stream;

        loop {
            tokio::select! {
                msg_opt = stream.next() => {
                    match msg_opt {
                        Some(Ok(Message::Text(t))) => self.metrics.record_message_recv(t.len(), None),
                        Some(Ok(Message::Binary(b))) => self.metrics.record_message_recv(b.len(), None),
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => {
                            self.metrics.record_error(categorize_tungstenite_error(&e));
                            break;
                        }
                        _ => {}
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    break;
                }
            }
        }
    }

    fn render_payload(&self, seq: u64) -> Message {
        match &self.config.payload {
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
}

fn message_byte_len(msg: &Message) -> usize {
    match msg {
        Message::Text(t) => t.len(),
        Message::Binary(b) => b.len(),
        Message::Ping(p) | Message::Pong(p) => p.len(),
        Message::Close(_) | Message::Frame(_) => 0,
    }
}

/// Normalizes underlying network and protocol errors into canonical taxonomy categories.
pub fn categorize_tungstenite_error(err: &TungsteniteError) -> ErrorCategory {
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
        TungsteniteError::Io(io_err) => match io_err.kind() {
            std::io::ErrorKind::TimedOut => ErrorCategory::Timeout,
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
                ErrorCategory::TcpConnect
            }
            _ => ErrorCategory::WriteError,
        },
        TungsteniteError::Tls(_) => ErrorCategory::TlsHandshake,
        TungsteniteError::Protocol(_) => ErrorCategory::ProtocolError,
        TungsteniteError::WriteBufferFull(_) => ErrorCategory::WriteError,
        _ => ErrorCategory::Other,
    }
}
