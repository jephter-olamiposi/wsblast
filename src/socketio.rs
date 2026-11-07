//! Socket.IO over WebSocket (`--mode socketio`): Engine.IO protocol 4 with
//! Socket.IO protocol 5, as Socket.IO 3 and later servers speak them, or
//! with `--sio-version 2` Engine.IO 3 with Socket.IO protocol 4, as
//! Socket.IO 2 servers do. The packets are the same; Engine.IO 3 differs in
//! who pings (the client, every `pingInterval` of the open packet), in
//! binary frames (each starts with the message type byte `4`), in the
//! default namespace (joined by the server on its own, so the client sends
//! no connect for it) and in authentication (query parameters of the URL
//! instead of a connect payload).
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
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::PollSender;

use crate::data::DataSet;
use crate::error::ErrorCategory;
use crate::template::{Template, Values};

/// The Engine.IO protocol a session speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum EngineIo {
    /// Engine.IO 3, of Socket.IO 2 servers (`--sio-version 2`).
    V3,
    /// Engine.IO 4, of Socket.IO 3 and later servers.
    #[default]
    V4,
}

impl EngineIo {
    /// The `EIO` query value.
    pub(crate) fn query(self) -> &'static str {
        match self {
            Self::V3 => "3",
            Self::V4 => "4",
        }
    }

    /// The protocol of a `--sio-version`: 2 for Engine.IO 3, 3 and 4 for
    /// Engine.IO 4.
    pub(crate) fn from_version(version: u8) -> Option<Self> {
        match version {
            2 => Some(Self::V3),
            3 | 4 => Some(Self::V4),
            _ => None,
        }
    }

    /// The Engine.IO version an `EIO` query value asks for.
    pub(crate) fn from_query(value: &str) -> Option<Self> {
        match value {
            "3" => Some(Self::V3),
            "4" => Some(Self::V4),
            _ => None,
        }
    }

    /// A binary frame as Engine.IO sends it: as is in version 4, after the
    /// message type byte in version 3.
    pub(crate) fn wrap_binary(self, bytes: Bytes) -> Bytes {
        match self {
            Self::V3 => {
                let mut framed = Vec::with_capacity(bytes.len() + 1);
                framed.push(4);
                framed.extend_from_slice(&bytes);
                framed.into()
            }
            Self::V4 => bytes,
        }
    }

    /// The bytes of a binary frame Engine.IO sent; in version 3 a frame
    /// that does not start with the message type is not a message.
    pub(crate) fn unwrap_binary(self, bytes: Bytes) -> Option<Bytes> {
        match self {
            Self::V3 => (bytes.first() == Some(&4)).then(|| bytes.slice(1..)),
            Self::V4 => Some(bytes),
        }
    }
}

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
    /// The JSON object sent with the namespace connect, if any, as a
    /// template filled in for each connection; with Engine.IO 3, its fields
    /// are query parameters of the handshake.
    pub(crate) auth: Option<Template>,
    pub(crate) engine: EngineIo,
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
        // A template is checked once it is filled in, by `check_auth`.
        if let Some(auth) = &self.auth
            && !auth.is_dynamic()
            && !is_json_object(auth.source())
        {
            return Err(
                "--sio-auth must be a JSON object, such as {\"token\":\"abc\"}".to_string(),
            );
        }
        Ok(())
    }

    /// The `--sio-auth` object of the connection whose templates `values`
    /// fill.
    pub(crate) fn auth_for(&self, values: Values<'_>) -> Option<String> {
        self.auth.as_ref().map(|auth| auth.render(values))
    }
}

fn is_json_object(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| v.is_object())
}

/// Checks a `--sio-auth` template against the data file: every column it
/// reads must exist, and filled in with every row (or once, without a
/// file) it must give a JSON object, so a value that breaks the JSON fails
/// before the run, naming the row.
pub(crate) fn check_auth(auth: &Template, data: Option<&DataSet>) -> Result<(), String> {
    if !auth.is_dynamic() {
        return Ok(());
    }
    auth.check_data(data)
        .map_err(|e| format!("--sio-auth: {e}"))?;
    let rows = data.map_or(1, DataSet::len);
    for row in 0..rows {
        let rendered = auth.render(Values {
            seq: 0,
            worker_id: row,
            data: data.map(|data| (data, row)),
            vars: &[],
        });
        if !is_json_object(&rendered) {
            let source = data.map_or_else(String::new, |data| {
                format!(" with row {} of {}", row + 1, data.path())
            });
            return Err(format!(
                "--sio-auth must give a JSON object{source}, but gives {rendered}; values are inserted as they are, so quote them in the template (\"{{{{data.token}}}}\") and keep quotes out of the data"
            ));
        }
    }
    Ok(())
}

/// Adds the Engine.IO query to `url`, and the default Socket.IO path when
/// the URL has none. An `EIO` the URL already sets is replaced by the
/// session's, which [`check_url`] rejects when they differ.
pub(crate) fn socketio_url(url: &mut url::Url, engine: EngineIo) {
    if url.path().is_empty() || url.path() == "/" {
        url.set_path(DEFAULT_PATH);
    }
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "EIO")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let had_eio = url
        .query_pairs()
        .any(|(k, v)| k == "EIO" && v == engine.query());
    if had_eio {
        // Keep the URL as it was written.
        pairs = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    } else {
        pairs.push(("EIO".to_string(), engine.query().to_string()));
    }
    if !pairs.iter().any(|(k, _)| k == "transport") {
        pairs.push(("transport".to_string(), "websocket".to_string()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

/// Rejects a URL whose `EIO` asks for another Engine.IO version than the
/// session speaks.
pub(crate) fn check_url(url: &url::Url, engine: EngineIo) -> Result<(), String> {
    match url.query_pairs().find(|(k, _)| k == "EIO") {
        Some((_, value)) if value != engine.query() => Err(format!(
            "the URL asks for EIO={value}, but the session speaks Engine.IO {}; {}",
            engine.query(),
            match EngineIo::from_query(&value) {
                Some(EngineIo::V3) => "add --sio-version 2 for a Socket.IO 2 server",
                Some(EngineIo::V4) => "drop --sio-version 2 for a Socket.IO 3 or later server",
                None => "remove EIO from the URL",
            }
        )),
        _ => Ok(()),
    }
}

/// The handshake URL of an Engine.IO 3 session with `auth`: each field of
/// the object is a query parameter, a string as it is and any other value
/// as JSON, as a Socket.IO 2 client's `query` option sends it.
pub(crate) fn auth_query(url: &url::Url, auth: &str) -> url::Url {
    let mut url = url.clone();
    if let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(auth) {
        let mut query = url.query_pairs_mut();
        for (key, value) in fields {
            match value {
                serde_json::Value::String(text) => query.append_pair(&key, &text),
                other => query.append_pair(&key, &other.to_string()),
            };
        }
    }
    url
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
/// packet, answering pings meanwhile. Returns how often an Engine.IO 3
/// session must ping the server: the open packet's `pingInterval`.
///
/// # Errors
///
/// `protocol_error` when the server does not speak Engine.IO 4 or refuses
/// the namespace (a connect error packet), `unexpected_close` when it
/// closes, and `read_error` or `write_error` when the connection fails.
pub(crate) async fn open<S>(
    ws: &mut S,
    config: &SocketIoConfig,
    auth: Option<&str>,
) -> Result<Option<Duration>, ErrorCategory>
where
    S: Sink<Message, Error = TungsteniteError>
        + Stream<Item = Result<Message, TungsteniteError>>
        + Unpin,
{
    let mut opened = false;
    let mut ping_every = None;
    loop {
        let text = match ws.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(data))) => {
                let _ = ws.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Close(_))) | None => return Err(ErrorCategory::UnexpectedClose),
            // Before the open packet, a binary frame is not Engine.IO.
            Some(Ok(Message::Binary(_))) if !opened => return Err(ErrorCategory::ProtocolError),
            // After it, the attachment of an event the server emits on
            // another namespace, such as a Socket.IO 2 server's default one
            // before it answers the connect.
            Some(Ok(_)) => continue,
            Some(Err(_)) => return Err(ErrorCategory::ReadError),
        };
        if !opened {
            // The open packet: `0{"sid":...,"pingInterval":...}`.
            let Some(open) = text.strip_prefix('0') else {
                return Err(ErrorCategory::ProtocolError);
            };
            let Ok(open) = serde_json::from_str::<serde_json::Value>(open) else {
                return Err(ErrorCategory::ProtocolError);
            };
            opened = true;
            if config.engine == EngineIo::V3 {
                // An Engine.IO 3 client pings; without an interval it
                // cannot keep the session.
                let interval = open
                    .get("pingInterval")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|ms| *ms > 0)
                    .ok_or(ErrorCategory::ProtocolError)?;
                ping_every = Some(Duration::from_millis(interval));
                // The server joins the default namespace on its own.
                if config.namespace == "/" {
                    continue;
                }
            }
            let auth = match config.engine {
                EngineIo::V3 => "",
                EngineIo::V4 => auth.unwrap_or_default(),
            };
            let connect = Packet {
                kind: b'0',
                attachments: 0,
                namespace: &config.namespace,
                id: None,
                data: auth,
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
                b'0' => return Ok(ping_every),
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
pub(crate) async fn pump<S>(
    ws: S,
    config: &SocketIoConfig,
    ping_every: Option<Duration>,
    ends: PumpEnds,
    unsolicited: &mut u64,
) where
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
    // An Engine.IO 3 client pings the server; version 4 servers ping.
    let mut pings = ping_every.map(|every| {
        let mut pings = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        pings
    });
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
            () = next_ping(pings.as_mut()) => {
                if let Err(err) = sink.send(Message::Text("2".into())).await {
                    let _ = incoming.send(Err(err)).await;
                    return;
                }
            }
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
                    Some(Message::Binary(bytes)) => Some((
                        binary_event_packet(config, next_id),
                        Some(config.engine.wrap_binary(bytes)),
                    )),
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
                        let (Some(mut packet), Some(bytes)) =
                            (pending.take(), config.engine.unwrap_binary(bytes))
                        else {
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

/// The next tick of `pings`, or never without them.
async fn next_ping(pings: Option<&mut tokio::time::Interval>) {
    match pings {
        Some(pings) => {
            pings.tick().await;
        }
        None => std::future::pending().await,
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
/// Engine.IO version the client asked for, and the binary packet whose
/// attachments are still arriving.
#[cfg(feature = "mock")]
#[derive(Default)]
pub(crate) struct MockSession {
    engine: EngineIo,
    pending: Option<(String, usize, Vec<Bytes>)>,
}

#[cfg(feature = "mock")]
impl MockSession {
    /// A session speaking `engine`.
    pub(crate) fn new(engine: EngineIo) -> Self {
        Self {
            engine,
            pending: None,
        }
    }

    /// What the mock answers to a frame the client sent: the connect
    /// packet for a namespace connect, the ack for an event that asks for
    /// one (with the event's arguments, and their attachments for a binary
    /// event), the event itself for one that does not, and a pong for a
    /// ping. A binary event is answered once its attachments have arrived.
    pub(crate) fn receive(&mut self, frame: &Message, sid: &str) -> Option<MockAnswer> {
        let text = match frame {
            Message::Text(text) => text.as_str(),
            Message::Binary(bytes) => {
                let (header, needed, mut attachments) = self.pending.take()?;
                attachments.push(self.engine.unwrap_binary(bytes.clone())?);
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
                frames.extend(
                    attachments
                        .into_iter()
                        .map(|bytes| Message::Binary(self.engine.wrap_binary(bytes))),
                );
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
                // A Socket.IO 2 client appends its query to the namespace
                // it connects to: `40/chat?token=x,`.
                let namespace = packet.namespace.split('?').next().unwrap_or("/");
                let reply = Packet {
                    kind: b'0',
                    attachments: 0,
                    namespace,
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

/// The Engine.IO open packet the mock sends first; version 3 has no
/// `maxPayload`.
#[cfg(feature = "mock")]
pub(crate) fn mock_open(
    sid: &str,
    ping_interval_ms: u128,
    ping_timeout_ms: u128,
    engine: EngineIo,
) -> String {
    let max_payload = match engine {
        EngineIo::V3 => "",
        EngineIo::V4 => r#","maxPayload":1000000"#,
    };
    format!(
        r#"0{{"sid":"{sid}","upgrades":[],"pingInterval":{ping_interval_ms},"pingTimeout":{ping_timeout_ms}{max_payload}}}"#
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
            engine: EngineIo::V4,
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
        socketio_url(&mut url, EngineIo::V4);
        assert_eq!(
            url.as_str(),
            "ws://h:3000/socket.io/?EIO=4&transport=websocket"
        );
        let mut url = url::Url::parse("wss://h/io/?token=x&EIO=4").unwrap();
        socketio_url(&mut url, EngineIo::V4);
        assert_eq!(
            url.as_str(),
            "wss://h/io/?token=x&EIO=4&transport=websocket"
        );
    }

    #[test]
    fn engine_io_3_urls_frames_and_auth() {
        let mut url = url::Url::parse("ws://h:3000").unwrap();
        socketio_url(&mut url, EngineIo::V3);
        assert_eq!(
            url.as_str(),
            "ws://h:3000/socket.io/?EIO=3&transport=websocket"
        );
        let mut url = url::Url::parse("ws://h/socket.io/?EIO=4&a=1").unwrap();
        assert!(
            check_url(&url, EngineIo::V3)
                .unwrap_err()
                .contains("drop --sio-version 2")
        );
        assert!(check_url(&url, EngineIo::V4).is_ok());
        let v2 = url::Url::parse("ws://h/?EIO=3").unwrap();
        assert!(
            check_url(&v2, EngineIo::V4)
                .unwrap_err()
                .contains("add --sio-version 2")
        );
        socketio_url(&mut url, EngineIo::V3);
        assert_eq!(url.query(), Some("a=1&EIO=3&transport=websocket"));
        let with_auth = auth_query(&url, r#"{"token":"a b","n":5}"#);
        assert_eq!(
            with_auth.query(),
            Some("a=1&EIO=3&transport=websocket&n=5&token=a+b")
        );

        let bytes = Bytes::from_static(b"abc");
        assert_eq!(EngineIo::V3.wrap_binary(bytes.clone()).as_ref(), b"\x04abc");
        assert_eq!(
            EngineIo::V3.unwrap_binary(Bytes::from_static(b"\x04abc")),
            Some(bytes.clone())
        );
        assert_eq!(EngineIo::V3.unwrap_binary(bytes.clone()), None);
        assert_eq!(EngineIo::V4.wrap_binary(bytes.clone()), bytes);
        assert_eq!(EngineIo::from_version(2), Some(EngineIo::V3));
        assert_eq!(EngineIo::from_version(3), Some(EngineIo::V4));
        assert_eq!(EngineIo::from_version(5), None);
    }

    #[test]
    fn auth_templates_are_checked_with_every_row() {
        let data = DataSet::parse("users.csv", "user,token\nada,t1\nbob,\"t\"\"2\"\n").unwrap();
        let auth = Template::parse(r#"{"user":"{{data.user}}","token":"{{data.token}}"}"#);
        let err = check_auth(&auth, Some(&data)).unwrap_err();
        assert!(err.contains("row 2 of users.csv"), "{err}");
        let good = DataSet::parse("users.csv", "user,token\nada,t1\nbob,t2\n").unwrap();
        assert!(check_auth(&auth, Some(&good)).is_ok());
        assert!(
            check_auth(&auth, None)
                .unwrap_err()
                .contains("needs a data file")
        );
        let missing = Template::parse(r#"{"k":"{{data.key}}"}"#);
        assert!(
            check_auth(&missing, Some(&good))
                .unwrap_err()
                .contains("no column 'key'")
        );
        // A placeholder outside quotes is fine when its value is JSON.
        let worker = Template::parse(r#"{"worker":{{worker_id}}}"#);
        assert!(check_auth(&worker, None).is_ok());
        let mut sio = config("/");
        sio.auth = Some(worker);
        assert!(sio.validate().is_ok());
        let values = Values {
            seq: 0,
            worker_id: 7,
            data: None,
            vars: &[],
        };
        assert_eq!(sio.auth_for(values).as_deref(), Some(r#"{"worker":7}"#));
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
        auth.auth = Some(Template::parse("[1]"));
        assert!(auth.validate().unwrap_err().contains("JSON object"));
        auth.auth = Some(Template::parse(r#"{"token":"t"}"#));
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

        // Engine.IO 3: attachments carry the message type byte, and a
        // Socket.IO 2 client's connect carries its query.
        let s = &mut MockSession::new(EngineIo::V3);
        assert_eq!(
            mock_reply(s, "40/files?token=x,").0.as_deref(),
            Some(r#"40/files,{"sid":"s"}"#)
        );
        let header = Message::Text(r#"451-3["up",{"_placeholder":true,"num":0}]"#.into());
        assert!(s.receive(&header, "s").is_none());
        let answer = s
            .receive(&Message::Binary(Bytes::from_static(b"\x04one")), "s")
            .unwrap();
        assert_eq!(
            answer.frames[1],
            Message::Binary(Bytes::from_static(b"\x04one"))
        );
        assert!(mock_open("s", 1, 2, EngineIo::V3).ends_with(r#""pingTimeout":2}"#));
    }
}
