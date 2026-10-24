//! WebSocket mock server for contract testing: record/replay with configurable behavior.

use crate::error::{ConfigError, Result, WorkerError, WsBlastError};
use base64::Engine as _;
use base64::engine::general_purpose;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

/// Mock server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockConfig {
    pub bind_addr: SocketAddr,
    pub record_path: Option<PathBuf>,
    pub replay_path: Option<PathBuf>,
    pub delay_ms: Option<u64>,
    pub error_injection: Option<ErrorInjectionConfig>,
}

impl MockConfig {
    /// Rejects contradictory modes and inert error injection before binding.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidMock`] when no mode is selected, both modes
    /// are selected, or error-injection options cannot take effect.
    pub fn validate(&self) -> Result<()> {
        match (&self.record_path, &self.replay_path) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::InvalidMock(
                    "record and replay modes cannot be enabled together".to_string(),
                )
                .into());
            }
            (None, None) => {
                return Err(ConfigError::InvalidMock(
                    "mock server requires either a record path or a replay path".to_string(),
                )
                .into());
            }
            _ => {}
        }

        if let Some(injection) = &self.error_injection {
            if injection.close_after_messages.is_none() && injection.inject_error_after.is_none() {
                return Err(ConfigError::InvalidMock(
                    "error injection requires --error-close-after or --error-inject-after"
                        .to_string(),
                )
                .into());
            }
            if injection.error_message.is_some() && injection.inject_error_after.is_none() {
                return Err(ConfigError::InvalidMock(
                    "--error-message requires --error-inject-after".to_string(),
                )
                .into());
            }
        }

        if let Some(path) = &self.record_path
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(ConfigError::InvalidMock(format!(
                "recording directory '{}' does not exist",
                parent.display()
            ))
            .into());
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInjectionConfig {
    pub close_after_messages: Option<usize>,
    pub inject_error_after: Option<usize>,
    pub error_message: Option<String>,
}

/// Recorded frame for replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedFrame {
    pub timestamp_ms: u64,
    pub direction: FrameDirection,
    pub payload: String,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDirection {
    ClientToServer,
    ServerToClient,
}

/// Pre-decoded server frame ready to send on any replayed connection.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplayFrame {
    Text(String),
    Binary(Bytes),
}

impl ReplayFrame {
    fn to_message(&self) -> Message {
        match self {
            Self::Text(text) => Message::Text(text.clone().into()),
            Self::Binary(bytes) => Message::Binary(bytes.clone()),
        }
    }
}

struct SessionState {
    message_count: usize,
}

/// Mock server that can record or replay WebSocket interactions.
pub struct MockServer {
    config: MockConfig,
    recordings: Arc<RwLock<Vec<RecordedFrame>>>,
    cancel_token: CancellationToken,
}

impl MockServer {
    pub fn new(config: MockConfig) -> Self {
        Self {
            config,
            recordings: Arc::new(RwLock::new(Vec::new())),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Run the mock server.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid, the replay file
    /// cannot be read or parsed, binding fails, or recordings cannot be saved.
    pub async fn run(&self) -> Result<()> {
        self.config.validate()?;
        let replay_frames = match &self.config.replay_path {
            Some(path) => Arc::new(load_replay_frames(path)?),
            None => Arc::new(Vec::new()),
        };

        let listener = TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(WsBlastError::Io)?;

        println!("Mock server listening on {}", self.config.bind_addr);

        let cancel_token = self.cancel_token.clone();
        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

        tokio::select! {
            _ = async {
                while !self.cancel_token.is_cancelled() {
                    tokio::select! {
                        res = listener.accept() => {
                            match res {
                                Ok((stream, addr)) => {
                                    let server = self.clone();
                                    let frames = Arc::clone(&replay_frames);
                                    tokio::spawn(async move {
                                        if let Err(e) = server.handle_connection(stream, addr, frames).await {
                                            eprintln!("Error handling connection from {}: {}", addr, e);
                                        }
                                    });
                                }
                                Err(e) => {
                                    eprintln!("Accept error: {}", e);
                                }
                            }
                        }
                        _ = self.cancel_token.cancelled() => {
                            break;
                        }
                    }
                }
            } => {}
            _ = tokio::signal::ctrl_c() => {
                cancel_token.cancel();
            }
            _ = async {
                #[cfg(unix)]
                if let Some(ref mut st) = sigterm {
                    st.recv().await;
                } else {
                    std::future::pending().await
                }
            } => {
                cancel_token.cancel();
            }
        }

        if let Some(path) = &self.config.record_path {
            self.save_recordings(path).await?;
        }

        Ok(())
    }

    async fn handle_connection(
        &self,
        stream: TcpStream,
        _addr: SocketAddr,
        replay_frames: Arc<Vec<ReplayFrame>>,
    ) -> Result<()> {
        let ws_stream = accept_async(stream)
            .await
            .map_err(|e| WsBlastError::Worker(WorkerError::Protocol(e.to_string())))?;

        let (mut sink, mut stream) = ws_stream.split();
        let mut state = SessionState { message_count: 0 };
        let delay = self.config.delay_ms.map(Duration::from_millis);
        let replaying = self.config.replay_path.is_some();
        let mut replay_index = 0;

        while let Some(msg) = stream.next().await {
            let msg =
                msg.map_err(|e| WsBlastError::Worker(WorkerError::Protocol(e.to_string())))?;

            self.record_frame(FrameDirection::ClientToServer, &msg)
                .await;

            match msg {
                Message::Close(_) => break,
                Message::Ping(payload) => {
                    if sink.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                    continue;
                }
                Message::Pong(_) => continue,
                _ => {}
            }

            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }

            if let Some(injection) = &self.config.error_injection {
                let inject_due = injection
                    .inject_error_after
                    .is_some_and(|after| state.message_count >= after);
                if inject_due {
                    let text = injection
                        .error_message
                        .as_deref()
                        .unwrap_or("Injected error");
                    let error = Message::Text(text.to_string().into());
                    self.record_frame(FrameDirection::ServerToClient, &error)
                        .await;
                    let _ = sink.send(error).await;
                    break;
                }

                let close_due = injection
                    .close_after_messages
                    .is_some_and(|after| state.message_count >= after);
                if close_due {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }

            state.message_count += 1;

            let response = if replaying {
                let Some(frame) = replay_frames.get(replay_index) else {
                    // A replay that runs out of recorded server frames ends the
                    // contract instead of inventing echo responses.
                    break;
                };
                replay_index += 1;
                frame.to_message()
            } else {
                msg
            };

            self.record_frame(FrameDirection::ServerToClient, &response)
                .await;

            if sink.send(response).await.is_err() {
                break;
            }
        }

        let _ = sink.send(Message::Close(None)).await;
        Ok(())
    }

    async fn record_frame(&self, direction: FrameDirection, msg: &Message) {
        if self.config.record_path.is_none() {
            return;
        }

        self.recordings.write().await.push(RecordedFrame {
            timestamp_ms: current_timestamp_ms(),
            direction,
            payload: message_to_string(msg),
            is_binary: msg.is_binary(),
        });
    }

    async fn save_recordings(&self, path: &Path) -> Result<()> {
        let frames = self.recordings.read().await.clone();
        let json = serde_json::to_string_pretty(&frames).map_err(WsBlastError::Serialization)?;

        fs::write(path, json).map_err(WsBlastError::Io)?;
        println!("Recorded {} frames to {}", frames.len(), path.display());
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

impl Clone for MockServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            recordings: self.recordings.clone(),
            cancel_token: self.cancel_token.clone(),
        }
    }
}

fn load_replay_frames(path: &Path) -> Result<Vec<ReplayFrame>> {
    let data = fs::read_to_string(path).map_err(|e| {
        ConfigError::InvalidMock(format!("cannot read replay file '{}': {e}", path.display()))
    })?;

    parse_replay_frames(&data, path)
}

fn parse_replay_frames(data: &str, path: &Path) -> Result<Vec<ReplayFrame>> {
    let recorded: Vec<RecordedFrame> = serde_json::from_str(data).map_err(|e| {
        ConfigError::InvalidMock(format!(
            "cannot parse replay file '{}': {e}",
            path.display()
        ))
    })?;

    let mut frames = Vec::new();
    for frame in recorded {
        if frame.direction != FrameDirection::ServerToClient {
            continue;
        }
        if frame.is_binary {
            let payload = general_purpose::STANDARD
                .decode(&frame.payload)
                .map_err(|e| {
                    ConfigError::InvalidMock(format!(
                        "replay file '{}' contains invalid base64 in a binary frame: {e}",
                        path.display()
                    ))
                })?;
            frames.push(ReplayFrame::Binary(Bytes::from(payload)));
        } else {
            frames.push(ReplayFrame::Text(frame.payload));
        }
    }

    if frames.is_empty() {
        return Err(ConfigError::InvalidMock(format!(
            "replay file '{}' contains no server-to-client frames",
            path.display()
        ))
        .into());
    }

    Ok(frames)
}

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn message_to_string(msg: &Message) -> String {
    match msg {
        Message::Text(text) => text.to_string(),
        Message::Binary(bytes) => general_purpose::STANDARD.encode(bytes),
        Message::Ping(payload) => format!("PING:{}", general_purpose::STANDARD.encode(payload)),
        Message::Pong(payload) => format!("PONG:{}", general_purpose::STANDARD.encode(payload)),
        Message::Close(frame) => format!("CLOSE:{frame:?}"),
        Message::Frame(frame) => format!("FRAME:{frame:?}"),
    }
}

#[cfg(feature = "mock")]
pub async fn run_mock_server(config: MockConfig) -> Result<()> {
    MockServer::new(config).run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio_tungstenite::connect_async;

    fn base_config() -> MockConfig {
        MockConfig {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            record_path: None,
            replay_path: None,
            delay_ms: None,
            error_injection: None,
        }
    }

    fn replay_config() -> MockConfig {
        MockConfig {
            replay_path: Some(PathBuf::from("recording.json")),
            ..base_config()
        }
    }

    #[test]
    fn validates_exactly_one_mode() {
        assert!(base_config().validate().is_err());

        let mut both = replay_config();
        both.record_path = Some(PathBuf::from("output.json"));
        assert!(both.validate().is_err());

        assert!(replay_config().validate().is_ok());
    }

    #[test]
    fn rejects_error_message_without_injection_threshold() {
        let mut config = replay_config();
        config.error_injection = Some(ErrorInjectionConfig {
            close_after_messages: Some(1),
            inject_error_after: None,
            error_message: Some("boom".to_string()),
        });
        assert!(config.validate().is_err());

        if let Some(injection) = config.error_injection.as_mut() {
            injection.inject_error_after = Some(1);
        }
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_inert_error_injection() {
        let mut config = replay_config();
        config.error_injection = Some(ErrorInjectionConfig {
            close_after_messages: None,
            inject_error_after: None,
            error_message: None,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_server_frames_and_decodes_binary_payloads() {
        let recording = r#"[
            {"timestamp_ms":1,"direction":"ClientToServer","payload":"hello","is_binary":false},
            {"timestamp_ms":2,"direction":"ServerToClient","payload":"world","is_binary":false},
            {"timestamp_ms":3,"direction":"ServerToClient","payload":"AAEC","is_binary":true}
        ]"#;

        let frames =
            parse_replay_frames(recording, Path::new("inline.json")).expect("valid recording");
        assert_eq!(
            frames,
            vec![
                ReplayFrame::Text("world".to_string()),
                ReplayFrame::Binary(Bytes::from_static(&[0, 1, 2])),
            ]
        );
    }

    #[test]
    fn rejects_malformed_or_empty_replay_files() {
        assert!(parse_replay_frames("not json", Path::new("inline.json")).is_err());
        assert!(
            parse_replay_frames(
                r#"[{"timestamp_ms":1,"direction":"ClientToServer","payload":"x","is_binary":false}]"#,
                Path::new("inline.json")
            )
            .is_err()
        );
        assert!(
            parse_replay_frames(
                r#"[{"timestamp_ms":1,"direction":"ServerToClient","payload":"***","is_binary":true}]"#,
                Path::new("inline.json")
            )
            .is_err()
        );
    }

    #[test]
    fn reports_missing_replay_file() {
        let path = std::env::temp_dir().join(format!(
            "wsblast-missing-replay-{}-{}.json",
            std::process::id(),
            current_timestamp_ms()
        ));
        assert!(load_replay_frames(&path).is_err());
    }

    #[tokio::test]
    async fn replays_recorded_frames_without_echoing() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener address");
        let server = MockServer::new(replay_config());
        let frames = Arc::new(vec![ReplayFrame::Text("recorded".to_string())]);

        let task = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept client");
            server
                .handle_connection(stream, peer, frames)
                .await
                .expect("serve connection");
        });

        let (mut client, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("connect client");
        client
            .send(Message::Text("first".into()))
            .await
            .expect("send first frame");

        let response = client.next().await.expect("response").expect("valid frame");
        assert_eq!(response, Message::Text("recorded".into()));

        // The second request has no recorded counterpart; the server must close
        // rather than silently echoing the client frame.
        client
            .send(Message::Text("second".into()))
            .await
            .expect("send second frame");
        let exhausted = client.next().await;
        assert!(matches!(
            exhausted,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));

        task.await.expect("server task");
    }

    #[tokio::test]
    async fn records_frames_before_the_connection_closes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener address");
        let mut config = base_config();
        config.record_path = Some(PathBuf::from("recording.json"));
        let server = MockServer::new(config);
        let recording_server = server.clone();

        let task = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept client");
            server
                .handle_connection(stream, peer, Arc::new(Vec::new()))
                .await
                .expect("serve connection");
        });

        let (mut client, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("connect client");
        client
            .send(Message::Text("hello".into()))
            .await
            .expect("send frame");
        let response = client.next().await.expect("response").expect("valid frame");
        assert_eq!(response, Message::Text("hello".into()));

        // Both directions are visible while the client session is still open,
        // which is what makes Ctrl+C persistence lossless.
        let frames = recording_server.recordings.read().await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].direction, FrameDirection::ClientToServer);
        assert_eq!(frames[1].direction, FrameDirection::ServerToClient);
        drop(frames);

        client.close(None).await.expect("close client");
        task.await.expect("server task");
    }
}
