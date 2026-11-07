//! Socket.IO over WebSocket (`--mode socketio`): Engine.IO protocol 4 with
//! Socket.IO protocol 5, as Socket.IO 3 and later servers speak them.
//!
//! A session opens like a Socket.IO client: it reads the Engine.IO open
//! packet, connects to the namespace and waits for the server's connect
//! packet. It then runs the echo loop (or a scenario's actions) through a
//! [`SocketIoChannel`]: every data frame the loop sends becomes an event
//! with an acknowledgement id, and the server's acknowledgement of that id
//! is the reply, so the round-trip time is the time from event to ack.
//!
//! A pump between the channel and the WebSocket answers the server's
//! Engine.IO pings while the loop waits for its next send, so a slow send
//! rate never misses the server's ping timeout. Events the server emits on
//! its own, and acknowledgements of earlier events, are counted as
//! unsolicited frames. Binary attachments are not supported.

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::PollSender;

use crate::error::ErrorCategory;

/// The query parameters a Socket.IO client adds to the URL.
pub(crate) const QUERY: [(&str, &str); 2] = [("EIO", "4"), ("transport", "websocket")];

/// The path Socket.IO servers listen on unless configured otherwise.
pub(crate) const DEFAULT_PATH: &str = "/socket.io/";

/// Event names the Socket.IO client reserves.
const RESERVED_EVENTS: [&str; 6] = [
    "connect",
    "connect_error",
    "disconnect",
    "disconnecting",
    "newListener",
    "removeListener",
];

/// What a Socket.IO session connects to and emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SocketIoConfig {
    /// The namespace, `/` by default.
    pub(crate) namespace: String,
    /// The event every data frame is emitted as.
    pub(crate) event: String,
    /// The JSON object sent with the namespace connect, if any.
    pub(crate) auth: Option<String>,
}

impl SocketIoConfig {
    /// Checks the settings, naming them as the flags do.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.namespace.starts_with('/') || self.namespace.contains(',') {
            return Err(format!(
                "--sio-namespace '{}' must start with '/' and hold no ',', such as /chat",
                self.namespace
            ));
        }
        if self.event.is_empty() {
            return Err("--sio-event must not be empty".to_string());
        }
        if RESERVED_EVENTS.contains(&self.event.as_str()) {
            return Err(format!(
                "--sio-event '{}' is reserved by Socket.IO; choose another event name",
                self.event
            ));
        }
        if let Some(auth) = &self.auth
            && !serde_json::from_str::<serde_json::Value>(auth).is_ok_and(|v| v.is_object())
        {
            return Err(
                "--sio-auth must be a JSON object, such as {\"token\":\"abc\"}".to_string(),
            );
        }
        Ok(())
    }
}

/// Adds the Engine.IO query to `url`, and the default Socket.IO path when
/// the URL has none.
pub(crate) fn socketio_url(url: &mut url::Url) {
    if url.path().is_empty() || url.path() == "/" {
        url.set_path(DEFAULT_PATH);
    }
    let missing: Vec<(&str, &str)> = QUERY
        .iter()
        .filter(|(key, _)| !url.query_pairs().any(|(k, _)| k == *key))
        .copied()
        .collect();
    if !missing.is_empty() {
        url.query_pairs_mut().extend_pairs(missing);
    }
}

/// A Socket.IO packet, carried in an Engine.IO message packet (`4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packet<'a> {
    /// `0` connect, `1` disconnect, `2` event, `3` ack, `4` connect error,
    /// `5` binary event, `6` binary ack.
    pub(crate) kind: u8,
    pub(crate) namespace: &'a str,
    pub(crate) id: Option<u64>,
    pub(crate) data: &'a str,
}

impl<'a> Packet<'a> {
    /// Parses an Engine.IO message packet's text; `None` for any other
    /// Engine.IO packet or a malformed one.
    pub(crate) fn parse(text: &'a str) -> Option<Self> {
        let rest = text.strip_prefix('4')?;
        let kind = *rest.as_bytes().first()?;
        if !(b'0'..=b'6').contains(&kind) {
            return None;
        }
        let mut rest = &rest[1..];
        // Binary packets carry their attachment count and a '-' first.
        if matches!(kind, b'5' | b'6') {
            let count = rest.bytes().take_while(u8::is_ascii_digit).count();
            if count > 0 && rest[count..].starts_with('-') {
                rest = &rest[count + 1..];
            }
        }
        let mut namespace = "/";
        if rest.starts_with('/') {
            match rest.find(',') {
                Some(end) => {
                    namespace = &rest[..end];
                    rest = &rest[end + 1..];
                }
                None => {
                    namespace = rest;
                    rest = "";
                }
            }
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let id = (digits > 0).then(|| rest[..digits].parse().ok()).flatten();
        Some(Self {
            kind,
            namespace,
            id,
            data: &rest[digits..],
        })
    }

    /// The Engine.IO message packet text.
    pub(crate) fn encode(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + self.namespace.len() + 8);
        out.push('4');
        out.push(self.kind as char);
        if self.namespace != "/" {
            out.push_str(self.namespace);
            out.push(',');
        }
        if let Some(id) = self.id {
            out.push_str(&id.to_string());
        }
        out.push_str(self.data);
        out
    }
}

/// The event packet for one data frame: the payload is the event's argument,
/// as JSON when it is valid JSON and as a string otherwise.
fn event_packet(config: &SocketIoConfig, id: u64, payload: &str) -> String {
    let event = serde_json::Value::String(config.event.clone()).to_string();
    let argument = if serde_json::from_str::<serde::de::IgnoredAny>(payload).is_ok() {
        payload.to_string()
    } else {
        serde_json::Value::String(payload.to_string()).to_string()
    };
    Packet {
        kind: b'2',
        namespace: &config.namespace,
        id: Some(id),
        data: &format!("[{event},{argument}]"),
    }
    .encode()
}

/// Opens the Socket.IO session on a WebSocket: reads the Engine.IO open
/// packet, connects to the namespace and waits for the server's connect
/// packet, answering pings meanwhile.
///
/// # Errors
///
/// `protocol_error` when the server does not speak Engine.IO 4 or refuses
/// the namespace (a connect error packet), `unexpected_close` when it
/// closes, and `read_error` or `write_error` when the connection fails.
pub(crate) async fn open<S>(ws: &mut S, config: &SocketIoConfig) -> Result<(), ErrorCategory>
where
    S: Sink<Message, Error = TungsteniteError>
        + Stream<Item = Result<Message, TungsteniteError>>
        + Unpin,
{
    let mut opened = false;
    loop {
        let text = match ws.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(data))) => {
                let _ = ws.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Close(_))) | None => return Err(ErrorCategory::UnexpectedClose),
            Some(Ok(_)) => return Err(ErrorCategory::ProtocolError),
            Some(Err(_)) => return Err(ErrorCategory::ReadError),
        };
        if !opened {
            // The open packet: `0{"sid":...,"pingInterval":...}`.
            if !text.starts_with("0{") {
                return Err(ErrorCategory::ProtocolError);
            }
            opened = true;
            let connect = Packet {
                kind: b'0',
                namespace: &config.namespace,
                id: None,
                data: config.auth.as_deref().unwrap_or_default(),
            };
            ws.send(Message::Text(connect.encode().into()))
                .await
                .map_err(|_| ErrorCategory::WriteError)?;
            continue;
        }
        match text.as_str() {
            "2" => {
                ws.send(Message::Text("3".into()))
                    .await
                    .map_err(|_| ErrorCategory::WriteError)?;
                continue;
            }
            "1" => return Err(ErrorCategory::UnexpectedClose),
            _ => {}
        }
        match Packet::parse(&text) {
            Some(packet) if packet.namespace == config.namespace => match packet.kind {
                b'0' => return Ok(()),
                b'4' | b'1' => return Err(ErrorCategory::ProtocolError),
                _ => {}
            },
            _ => {}
        }
    }
}

/// The side of a Socket.IO session the echo loop and action scripts drive:
/// data frames sent into it become events, and the acknowledgement of each
/// event comes out of it as a text frame holding the ack's arguments.
pub(crate) struct SocketIoChannel {
    outgoing: PollSender<Message>,
    incoming: mpsc::Receiver<Incoming>,
}

/// The other ends of a [`SocketIoChannel`], which [`pump`] drives.
pub(crate) struct PumpEnds {
    outgoing: mpsc::Receiver<Message>,
    incoming: mpsc::Sender<Incoming>,
}

/// What the pump passes to the loop: a frame, or the error that ended the
/// connection.
type Incoming = Result<Message, TungsteniteError>;

/// A channel and the ends its pump takes.
pub(crate) fn channel() -> (SocketIoChannel, PumpEnds) {
    // One request is in flight at a time, so a few slots are plenty.
    let (out_tx, out_rx) = mpsc::channel(8);
    let (in_tx, in_rx) = mpsc::channel(8);
    (
        SocketIoChannel {
            outgoing: PollSender::new(out_tx),
            incoming: in_rx,
        },
        PumpEnds {
            outgoing: out_rx,
            incoming: in_tx,
        },
    )
}

impl Sink<Message> for SocketIoChannel {
    type Error = TungsteniteError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.outgoing
            .poll_reserve(cx)
            .map_err(|_| TungsteniteError::ConnectionClosed)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.outgoing
            .send_item(item)
            .map_err(|_| TungsteniteError::ConnectionClosed)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.outgoing.close();
        Poll::Ready(Ok(()))
    }
}

impl Stream for SocketIoChannel {
    type Item = Result<Message, TungsteniteError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming.poll_recv(cx)
    }
}

/// Moves frames between a [`SocketIoChannel`] and the WebSocket until
/// either side ends: wraps data frames as events, passes back the ack of the
/// latest event, answers Engine.IO pings, and adds every other data frame
/// the server sends to `unsolicited`. When the channel's sender is dropped
/// or sends a Close, it disconnects from the namespace and closes.
pub(crate) async fn pump<S>(ws: S, config: &SocketIoConfig, ends: PumpEnds, unsolicited: &mut u64)
where
    S: Sink<Message, Error = TungsteniteError>
        + Stream<Item = Result<Message, TungsteniteError>>
        + Unpin,
{
    let PumpEnds {
        mut outgoing,
        incoming,
    } = ends;
    let (mut sink, mut stream) = ws.split();
    let mut next_id = 0u64;
    let mut awaited: Option<u64> = None;
    let disconnect = Packet {
        kind: b'1',
        namespace: &config.namespace,
        id: None,
        data: "",
    }
    .encode();
    loop {
        tokio::select! {
            out = outgoing.recv() => {
                let frame = match out {
                    None => {
                        let _ = sink.send(Message::Text(disconnect.into())).await;
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    }
                    Some(Message::Close(frame)) => {
                        let _ = sink.send(Message::Text(disconnect.into())).await;
                        let _ = sink.send(Message::Close(frame)).await;
                        return;
                    }
                    Some(Message::Text(text)) => Some(text.as_str().to_string()),
                    Some(Message::Binary(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                    Some(other) => {
                        let _ = sink.send(other).await;
                        None
                    }
                };
                if let Some(payload) = frame {
                    let id = next_id;
                    next_id += 1;
                    awaited = Some(id);
                    let packet = event_packet(config, id, &payload);
                    if let Err(err) = sink.send(Message::Text(packet.into())).await {
                        let _ = incoming.send(Err(err)).await;
                        return;
                    }
                }
            }
            received = stream.next() => {
                let text = match received {
                    None => return,
                    Some(Err(err)) => {
                        let _ = incoming.send(Err(err)).await;
                        return;
                    }
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(data))) => {
                        let _ = sink.send(Message::Pong(data)).await;
                        continue;
                    }
                    Some(Ok(frame @ Message::Close(_))) => {
                        let _ = incoming.send(Ok(frame)).await;
                        return;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        *unsolicited += 1;
                        continue;
                    }
                    Some(Ok(_)) => continue,
                };
                match text.as_str() {
                    "2" => {
                        let _ = sink.send(Message::Text("3".into())).await;
                        continue;
                    }
                    // The server closed the Engine.IO session.
                    "1" => {
                        let _ = incoming.send(Ok(Message::Close(None))).await;
                        return;
                    }
                    "3" | "6" => continue,
                    _ => {}
                }
                match Packet::parse(&text) {
                    Some(packet) if packet.namespace == config.namespace => match packet.kind {
                        b'3' if packet.id.is_some() && packet.id == awaited => {
                            awaited = None;
                            let reply = Message::Text(packet.data.to_string().into());
                            if incoming.send(Ok(reply)).await.is_err() {
                                return;
                            }
                        }
                        // The server left the namespace.
                        b'1' | b'4' => {
                            let _ = incoming.send(Ok(Message::Close(None))).await;
                            return;
                        }
                        _ => *unsolicited += 1,
                    },
                    _ => *unsolicited += 1,
                }
            }
        }
    }
}

/// What `wsblast mock --socketio` answers to a frame a client sent: the
/// connect packet for a namespace connect, the ack for an event that asks
/// for one (with the event's arguments), the event itself for one that does
/// not, and a pong for a ping. The flag says whether the frame was an event,
/// which the mock counts as a received data frame.
#[cfg(feature = "mock")]
pub(crate) fn mock_reply(text: &str, sid: &str) -> (Option<String>, bool) {
    if text == "2" {
        return (Some("3".to_string()), false);
    }
    let Some(packet) = Packet::parse(text) else {
        return (None, false);
    };
    match packet.kind {
        b'0' => {
            let data = format!(r#"{{"sid":"{sid}"}}"#);
            let reply = Packet {
                kind: b'0',
                namespace: packet.namespace,
                id: None,
                data: &data,
            };
            (Some(reply.encode()), false)
        }
        b'2' => match packet.id {
            Some(id) => {
                let arguments = event_arguments(packet.data);
                let reply = Packet {
                    kind: b'3',
                    namespace: packet.namespace,
                    id: Some(id),
                    data: &arguments,
                };
                (Some(reply.encode()), true)
            }
            None => (Some(text.to_string()), true),
        },
        _ => (None, false),
    }
}

/// The arguments of an event's data, without its name: `["chat",1,2]`
/// gives `[1,2]`.
#[cfg(feature = "mock")]
fn event_arguments(data: &str) -> String {
    match serde_json::from_str::<Vec<serde_json::Value>>(data) {
        Ok(mut values) if !values.is_empty() => {
            values.remove(0);
            serde_json::Value::Array(values).to_string()
        }
        _ => "[]".to_string(),
    }
}

/// The Engine.IO open packet the mock sends first.
#[cfg(feature = "mock")]
pub(crate) fn mock_open(sid: &str, ping_interval_ms: u128, ping_timeout_ms: u128) -> String {
    format!(
        r#"0{{"sid":"{sid}","upgrades":[],"pingInterval":{ping_interval_ms},"pingTimeout":{ping_timeout_ms},"maxPayload":1000000}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(namespace: &str) -> SocketIoConfig {
        SocketIoConfig {
            namespace: namespace.to_string(),
            event: "chat".to_string(),
            auth: None,
        }
    }

    #[test]
    fn packets_round_trip() {
        for text in [
            "40",
            "40/admin,",
            "42/admin,7[\"a\",1]",
            "4312[\"ok\"]",
            "41",
            "44{\"message\":\"no\"}",
        ] {
            let packet = Packet::parse(text).unwrap();
            assert_eq!(packet.encode(), text, "{packet:?}");
        }
        let ack = Packet::parse("43/admin,12[\"ok\"]").unwrap();
        assert_eq!(
            (ack.kind, ack.namespace, ack.id, ack.data),
            (b'3', "/admin", Some(12), "[\"ok\"]")
        );
        let event = Packet::parse("42[\"a\"]").unwrap();
        assert_eq!((event.namespace, event.id), ("/", None));
        let binary =
            Packet::parse("451-/admin,3[\"a\",{\"_placeholder\":true,\"num\":0}]").unwrap();
        assert_eq!((binary.namespace, binary.id), ("/admin", Some(3)));
        for other in ["2", "3", "0{}", "4", "49", ""] {
            assert!(Packet::parse(other).is_none(), "{other}");
        }
    }

    #[test]
    fn payloads_become_event_arguments() {
        assert_eq!(
            event_packet(&config("/"), 0, r#"{"a":1}"#),
            r#"420["chat",{"a":1}]"#
        );
        assert_eq!(
            event_packet(&config("/chat"), 5, "hello"),
            r#"42/chat,5["chat","hello"]"#
        );
        assert_eq!(event_packet(&config("/"), 1, "12"), r#"421["chat",12]"#);
    }

    #[test]
    fn urls_get_the_engine_io_query_and_default_path() {
        let mut url = url::Url::parse("ws://h:3000").unwrap();
        socketio_url(&mut url);
        assert_eq!(
            url.as_str(),
            "ws://h:3000/socket.io/?EIO=4&transport=websocket"
        );
        let mut url = url::Url::parse("wss://h/io/?token=x&EIO=4").unwrap();
        socketio_url(&mut url);
        assert_eq!(
            url.as_str(),
            "wss://h/io/?token=x&EIO=4&transport=websocket"
        );
    }

    #[test]
    fn settings_are_checked() {
        assert!(config("/").validate().is_ok());
        assert!(
            config("chat")
                .validate()
                .unwrap_err()
                .contains("must start with '/'")
        );
        assert!(config("/a,b").validate().is_err());
        let mut reserved = config("/");
        reserved.event = "connect".into();
        assert!(reserved.validate().unwrap_err().contains("reserved"));
        let mut auth = config("/");
        auth.auth = Some("[1]".into());
        assert!(auth.validate().unwrap_err().contains("JSON object"));
        auth.auth = Some(r#"{"token":"t"}"#.into());
        assert!(auth.validate().is_ok());
    }

    #[cfg(feature = "mock")]
    #[test]
    fn the_mock_acks_events_with_their_arguments() {
        assert_eq!(mock_reply("40", "s").0.as_deref(), Some(r#"40{"sid":"s"}"#));
        assert_eq!(
            mock_reply("40/admin,{\"t\":1}", "s").0.as_deref(),
            Some(r#"40/admin,{"sid":"s"}"#)
        );
        assert_eq!(
            mock_reply(r#"427["chat",{"a":1},2]"#, "s"),
            (Some(r#"437[{"a":1},2]"#.to_string()), true)
        );
        assert_eq!(
            mock_reply(r#"42["chat",1]"#, "s"),
            (Some(r#"42["chat",1]"#.to_string()), true)
        );
        assert_eq!(mock_reply("2", "s"), (Some("3".to_string()), false));
        assert_eq!(mock_reply("3", "s"), (None, false));
        assert_eq!(mock_reply("41", "s"), (None, false));
    }
}
