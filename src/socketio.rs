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
//! unsolicited frames.
//!
//! A binary data frame is emitted as a binary event: the packet
//! `451-[event,{"_placeholder":true,"num":0}]` followed by the bytes as a
//! binary frame. A binary packet the server sends (a `45` event or a `46`
//! ack) is complete once its attachments have arrived, each in a binary
//! frame of its own; a binary ack whose only argument is one attachment is
//! passed back as that attachment's bytes.

use bytes::Bytes;
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

/// The argument that stands for a binary event's single attachment.
const PLACEHOLDER: &str = r#"{"_placeholder":true,"num":0}"#;

/// The most attachments a binary packet may announce, against a server that
/// would make a session wait for unbounded frames.
const MAX_ATTACHMENTS: usize = 1024;

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
    /// How many binary frames follow a binary packet (`5` or `6`).
    pub(crate) attachments: usize,
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
        let mut attachments = 0;
        if matches!(kind, b'5' | b'6') {
            let count = rest.bytes().take_while(u8::is_ascii_digit).count();
            if count == 0 || !rest[count..].starts_with('-') {
                return None;
            }
            attachments = rest[..count]
                .parse()
                .ok()
                .filter(|n| *n <= MAX_ATTACHMENTS)?;
            rest = &rest[count + 1..];
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
            attachments,
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
        if matches!(self.kind, b'5' | b'6') {
            out.push_str(&self.attachments.to_string());
            out.push('-');
        }
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
        attachments: 0,
        namespace: &config.namespace,
        id: Some(id),
        data: &format!("[{event},{argument}]"),
    }
    .encode()
}

/// The packet of a binary event whose one argument is the attachment sent
/// after it.
fn binary_event_packet(config: &SocketIoConfig, id: u64) -> String {
    let event = serde_json::Value::String(config.event.clone()).to_string();
    Packet {
        kind: b'5',
        attachments: 1,
        namespace: &config.namespace,
        id: Some(id),
        data: &format!("[{event},{PLACEHOLDER}]"),
    }
    .encode()
}

/// The reply a binary ack gives: the attachment's bytes when its only
/// argument is the one attachment, and otherwise its arguments as text,
/// with the placeholders as the server sent them.
fn binary_reply(data: &str, mut attachments: Vec<Bytes>) -> Message {
    let only_attachment = attachments.len() == 1
        && serde_json::from_str::<serde_json::Value>(data).ok()
            == serde_json::from_str(&format!("[{PLACEHOLDER}]")).ok();
    match attachments.pop() {
        Some(bytes) if only_attachment => Message::Binary(bytes),
        _ => Message::Text(data.to_string().into()),
    }
}

/// A binary packet from the server whose attachments are still arriving.
struct Pending {
    /// Whether it is an ack on this session's namespace.
    ack: bool,
    id: Option<u64>,
    data: String,
    needed: usize,
    attachments: Vec<Bytes>,
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
                attachments: 0,
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
    let mut pending: Option<Pending> = None;
    let disconnect = Packet {
        kind: b'1',
        attachments: 0,
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
                    Some(Message::Text(text)) => Some((event_packet(config, next_id, text.as_str()), None)),
                    Some(Message::Binary(bytes)) => Some((binary_event_packet(config, next_id), Some(bytes))),
                    Some(other) => {
                        let _ = sink.send(other).await;
                        None
                    }
                };
                if let Some((packet, attachment)) = frame {
                    awaited = Some(next_id);
                    next_id += 1;
                    let mut sent = sink.feed(Message::Text(packet.into())).await;
                    if let (Ok(()), Some(bytes)) = (&sent, attachment) {
                        sent = sink.feed(Message::Binary(bytes)).await;
                    }
                    if let Err(err) = sent.and(sink.flush().await) {
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
                    Some(Ok(Message::Binary(bytes))) => {
                        let Some(mut packet) = pending.take() else {
                            *unsolicited += 1;
                            continue;
                        };
                        packet.attachments.push(bytes);
                        if packet.attachments.len() < packet.needed {
                            pending = Some(packet);
                            continue;
                        }
                        if packet.ack && packet.id.is_some() && packet.id == awaited {
                            awaited = None;
                            let reply = binary_reply(&packet.data, packet.attachments);
                            if incoming.send(Ok(reply)).await.is_err() {
                                return;
                            }
                        } else {
                            *unsolicited += 1;
                        }
                        continue;
                    }
                    Some(Ok(_)) => continue,
                };
                // A text frame ends a binary packet still waiting for its
                // attachments; it is counted as it came, incomplete.
                if pending.take().is_some() {
                    *unsolicited += 1;
                }
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
                    Some(packet) if packet.attachments > 0 => {
                        pending = Some(Pending {
                            ack: packet.kind == b'6' && packet.namespace == config.namespace,
                            id: packet.id,
                            data: packet.data.to_string(),
                            needed: packet.attachments,
                            attachments: Vec::with_capacity(packet.attachments.min(8)),
                        });
                    }
                    Some(packet) if packet.namespace == config.namespace => match packet.kind {
                        b'3' | b'6' if packet.id.is_some() && packet.id == awaited => {
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

/// What `wsblast mock --socketio` sends for a client's frames.
#[cfg(feature = "mock")]
pub(crate) struct MockAnswer {
    /// The frames, in order: a packet and its attachments.
    pub(crate) frames: Vec<Message>,
    /// Whether they answer an event, which the mock counts as a received
    /// data frame and delays like a reply; otherwise they are protocol
    /// frames sent at once.
    pub(crate) event: bool,
}

/// One connection's Socket.IO state in `wsblast mock --socketio`: the
/// binary packet whose attachments are still arriving.
#[cfg(feature = "mock")]
#[derive(Default)]
pub(crate) struct MockSession {
    pending: Option<(String, usize, Vec<Message>)>,
}

#[cfg(feature = "mock")]
impl MockSession {
    /// What the mock answers to a frame the client sent: the connect
    /// packet for a namespace connect, the ack for an event that asks for
    /// one (with the event's arguments, and their attachments for a binary
    /// event), the event itself for one that does not, and a pong for a
    /// ping. A binary event is answered once its attachments have arrived.
    pub(crate) fn receive(&mut self, frame: &Message, sid: &str) -> Option<MockAnswer> {
        let text = match frame {
            Message::Text(text) => text.as_str(),
            Message::Binary(_) => {
                let (header, needed, mut attachments) = self.pending.take()?;
                attachments.push(frame.clone());
                if attachments.len() < needed {
                    self.pending = Some((header, needed, attachments));
                    return None;
                }
                let packet = Packet::parse(&header)?;
                let mut frames = vec![Message::Text(match packet.id {
                    Some(id) => Packet {
                        kind: b'6',
                        attachments: packet.attachments,
                        namespace: packet.namespace,
                        id: Some(id),
                        data: &event_arguments(packet.data),
                    }
                    .encode()
                    .into(),
                    None => header.clone().into(),
                })];
                frames.extend(attachments);
                return Some(MockAnswer {
                    frames,
                    event: true,
                });
            }
            _ => return None,
        };
        self.pending = None;
        if text == "2" {
            return Some(control("3".to_string()));
        }
        let packet = Packet::parse(text)?;
        match packet.kind {
            b'0' => {
                let data = format!(r#"{{"sid":"{sid}"}}"#);
                let reply = Packet {
                    kind: b'0',
                    attachments: 0,
                    namespace: packet.namespace,
                    id: None,
                    data: &data,
                };
                Some(control(reply.encode()))
            }
            b'5' if packet.attachments > 0 => {
                self.pending = Some((
                    text.to_string(),
                    packet.attachments,
                    Vec::with_capacity(packet.attachments.min(8)),
                ));
                None
            }
            b'2' | b'5' => {
                let reply = match packet.id {
                    Some(id) => Packet {
                        kind: packet.kind + 1,
                        attachments: 0,
                        namespace: packet.namespace,
                        id: Some(id),
                        data: &event_arguments(packet.data),
                    }
                    .encode(),
                    None => text.to_string(),
                };
                Some(MockAnswer {
                    frames: vec![Message::Text(reply.into())],
                    event: true,
                })
            }
            _ => None,
        }
    }
}

#[cfg(feature = "mock")]
fn control(text: String) -> MockAnswer {
    MockAnswer {
        frames: vec![Message::Text(text.into())],
        event: false,
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

    #[test]
    fn binary_packets_carry_their_attachment_count() {
        let binary =
            Packet::parse("451-/admin,3[\"a\",{\"_placeholder\":true,\"num\":0}]").unwrap();
        assert_eq!(
            (binary.kind, binary.attachments, binary.namespace, binary.id),
            (b'5', 1, "/admin", Some(3))
        );
        assert_eq!(
            binary.encode(),
            "451-/admin,3[\"a\",{\"_placeholder\":true,\"num\":0}]"
        );
        assert_eq!(Packet::parse("4612-7[]").unwrap().attachments, 12);
        for bad in ["45[\"a\"]", "45x-[]", "4599999-[]"] {
            assert!(Packet::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(
            binary_event_packet(&config("/"), 4),
            r#"451-4["chat",{"_placeholder":true,"num":0}]"#
        );
        let bytes = Bytes::from_static(b"\x00\x01");
        assert_eq!(
            binary_reply(r#"[{"num":0,"_placeholder":true}]"#, vec![bytes.clone()]),
            Message::Binary(bytes.clone())
        );
        assert_eq!(
            binary_reply(r#"["ok",{"_placeholder":true,"num":0}]"#, vec![bytes]),
            Message::Text(r#"["ok",{"_placeholder":true,"num":0}]"#.into())
        );
    }

    #[cfg(feature = "mock")]
    fn mock_reply(session: &mut MockSession, frame: &str) -> (Option<String>, bool) {
        match session.receive(&Message::Text(frame.to_string().into()), "s") {
            Some(answer) => {
                assert_eq!(answer.frames.len(), 1);
                let Message::Text(text) = &answer.frames[0] else {
                    panic!("not text");
                };
                (Some(text.as_str().to_string()), answer.event)
            }
            None => (None, false),
        }
    }

    #[cfg(feature = "mock")]
    #[test]
    fn the_mock_acks_events_with_their_arguments() {
        let s = &mut MockSession::default();
        assert_eq!(mock_reply(s, "40").0.as_deref(), Some(r#"40{"sid":"s"}"#));
        assert_eq!(
            mock_reply(s, "40/admin,{\"t\":1}").0.as_deref(),
            Some(r#"40/admin,{"sid":"s"}"#)
        );
        assert_eq!(
            mock_reply(s, r#"427["chat",{"a":1},2]"#),
            (Some(r#"437[{"a":1},2]"#.to_string()), true)
        );
        assert_eq!(
            mock_reply(s, r#"42["chat",1]"#),
            (Some(r#"42["chat",1]"#.to_string()), true)
        );
        assert_eq!(mock_reply(s, "2"), (Some("3".to_string()), false));
        assert_eq!(mock_reply(s, "3"), (None, false));
        assert_eq!(mock_reply(s, "41"), (None, false));
    }

    #[cfg(feature = "mock")]
    #[test]
    fn the_mock_acks_binary_events_with_their_attachments() {
        let s = &mut MockSession::default();
        let header = Message::Text(
            r#"452-/a,9["up",{"_placeholder":true,"num":0},{"_placeholder":true,"num":1}]"#.into(),
        );
        let one = Message::Binary(Bytes::from_static(b"one"));
        let two = Message::Binary(Bytes::from_static(b"two"));
        assert!(s.receive(&header, "s").is_none());
        assert!(s.receive(&one, "s").is_none());
        let answer = s.receive(&two, "s").unwrap();
        assert!(answer.event);
        assert_eq!(
            answer.frames,
            vec![
                Message::Text(
                    r#"462-/a,9[{"_placeholder":true,"num":0},{"_placeholder":true,"num":1}]"#
                        .into()
                ),
                one.clone(),
                two,
            ]
        );
        // A stray attachment is ignored.
        assert!(s.receive(&one, "s").is_none());
    }
}
