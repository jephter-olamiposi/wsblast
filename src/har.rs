//! `wsblast import`: a scenario from a browser's HAR file.
//!
//! Chrome and Edge record a WebSocket's frames in the HAR they export from
//! the Network panel, under the entry's `_webSocketMessages`: each frame's
//! direction (`send` or `receive`), time in seconds, opcode and data. The
//! import takes one connection's frames and writes a scenario that sends
//! what the browser sent:
//!
//! - every text frame the browser sent is a `Send` action, and a run of the
//!   same message is one `Send` with a `count`;
//! - the time the user took between messages is a `Wait`: from the reply to
//!   the next message when every message got a reply (echo mode), from one
//!   message to the next otherwise (stream mode), capped at `--max-wait`;
//! - a Socket.IO connection (an `EIO` query, or an Engine.IO open packet
//!   first) becomes a Socket.IO scenario: Engine.IO and namespace packets
//!   are left out, each event's argument is a `Send`, and the flags to run
//!   it with name the event, the namespace and the version. A session that
//!   started on HTTP long-polling and upgraded, as browsers do by default,
//!   is matched by its `sid` to the packets it posted before the upgrade,
//!   where its namespace connect and auth object travel.
//!
//! The generated file is parsed and validated as `--scenario` would before
//! it is written. Header values and Socket.IO auth objects, which often hold
//! credentials, are never copied: the output names them so they can be
//! passed with `-H` or `--sio-auth`.

use crate::error::{ConfigError, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

/// What to import and how to shape the phase.
#[derive(Debug, Clone)]
pub(crate) struct ImportOptions {
    /// Only connections whose URL contains this text.
    pub(crate) url_filter: Option<String>,
    /// Which matching connection, from 1, in the HAR's order.
    pub(crate) index: Option<usize>,
    pub(crate) connections: u64,
    pub(crate) duration: String,
    /// The longest `Wait`; longer pauses are shortened to it.
    pub(crate) max_wait: Duration,
    /// Leave every pause out.
    pub(crate) no_waits: bool,
}

/// A generated scenario: the YAML, and what the import found out.
#[derive(Debug, Clone)]
pub(crate) struct Imported {
    pub(crate) yaml: String,
    /// The flags the scenario runs with, after `--scenario FILE`.
    pub(crate) run_flags: Vec<String>,
    /// Things the import left out or could not express.
    pub(crate) notes: Vec<String>,
    pub(crate) url: String,
    pub(crate) sends: usize,
}

/// One WebSocket connection of the HAR.
#[derive(Debug, Clone)]
struct Connection {
    url: String,
    /// Header names of the upgrade request, as recorded.
    headers: Vec<String>,
    frames: Vec<Frame>,
    /// Engine.IO packets the browser posted over HTTP long-polling for the
    /// same Socket.IO session before it upgraded to this connection.
    polling: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Frame {
    sent: bool,
    time: f64,
    text: Option<String>,
}

impl Connection {
    fn sends(&self) -> usize {
        self.frames.iter().filter(|f| f.sent).count()
    }
}

/// Reads the WebSocket connections of a HAR document.
fn connections(har: &Value) -> Result<Vec<Connection>> {
    let entries = har
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("not a HAR file: it has no log.entries"))?;
    let mut polled: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in entries {
        let url = entry.pointer("/request/url").and_then(Value::as_str);
        let body = entry
            .pointer("/request/postData/text")
            .and_then(Value::as_str);
        if let (Some(url), Some(body)) = (url, body)
            && url.contains("transport=polling")
            && let Some(sid) = session_id(url)
        {
            polled
                .entry(sid)
                .or_default()
                .extend(polling_packets(body, url.contains("EIO=3")));
        }
    }
    let mut found = Vec::new();
    for entry in entries {
        let Some(messages) = entry.get("_webSocketMessages").and_then(Value::as_array) else {
            continue;
        };
        let url = entry
            .pointer("/request/url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let headers = entry
            .pointer("/request/headers")
            .and_then(Value::as_array)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|h| h.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let frames = messages
            .iter()
            .filter_map(|message| {
                let sent = match message.get("type").and_then(Value::as_str)? {
                    "send" => true,
                    "receive" => false,
                    _ => return None,
                };
                let time = message.get("time").and_then(Value::as_f64)?;
                // Opcode 1 is text; binary frames (2) are base64 in the HAR.
                let text = (message.get("opcode").and_then(Value::as_u64) == Some(1))
                    .then(|| message.get("data").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_string);
                Some(Frame { sent, time, text })
            })
            .collect();
        let polling = session_id(&url)
            .and_then(|sid| polled.get(&sid).cloned())
            .unwrap_or_default();
        found.push(Connection {
            url,
            headers,
            frames,
            polling,
        });
    }
    Ok(found)
}

/// The Engine.IO session id in a Socket.IO URL's `sid` query.
fn session_id(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == "sid")
        .map(|(_, sid)| sid.into_owned())
        .filter(|sid| !sid.is_empty())
}

/// The packets of a long-polling request body: separated by the record
/// separator in Engine.IO 4, each prefixed with its length and a colon in
/// Engine.IO 3. A body that is not in that shape gives no packets.
fn polling_packets(body: &str, engine_io_3: bool) -> Vec<String> {
    if !engine_io_3 {
        return body
            .split('\u{1e}')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    }
    let mut packets = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let Some((len, tail)) = rest.split_once(':') else {
            break;
        };
        // The length counts UTF-16 code units, as JavaScript strings do.
        let Ok(len) = len.parse::<usize>() else {
            break;
        };
        let mut units = 0;
        let end = tail
            .char_indices()
            .find(|(_, c)| {
                let at_end = units >= len;
                units += c.len_utf16();
                at_end
            })
            .map_or(tail.len(), |(i, _)| i);
        if units < len {
            break;
        }
        packets.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    packets
}

/// Picks the connection to import: the one `--index` names among those
/// whose URL matches `--url`, or the one that sent the most messages.
fn choose(
    mut all: Vec<Connection>,
    options: &ImportOptions,
    notes: &mut Vec<String>,
) -> Result<Connection> {
    if all.is_empty() {
        return Err(invalid(
            "the HAR has no WebSocket messages. Chrome and Edge record them: open DevTools before the page connects, then Network > Export HAR",
        ));
    }
    if let Some(filter) = &options.url_filter {
        all.retain(|c| c.url.contains(filter.as_str()));
        if all.is_empty() {
            return Err(invalid(format!(
                "no WebSocket connection in the HAR has '{filter}' in its URL"
            )));
        }
    }
    let list = |all: &[Connection]| {
        all.iter()
            .enumerate()
            .map(|(i, c)| format!("  {}: {} ({} sent)", i + 1, c.url, c.sends()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if let Some(index) = options.index {
        if index == 0 || index > all.len() {
            return Err(invalid(format!(
                "--index {index}: the HAR has {} matching WebSocket connections:\n{}",
                all.len(),
                list(&all)
            )));
        }
        return Ok(all.swap_remove(index - 1));
    }
    let best = (0..all.len())
        .max_by_key(|&i| (all[i].sends(), std::cmp::Reverse(i)))
        .unwrap_or(0);
    if all.len() > 1 {
        notes.push(format!(
            "the HAR has {} WebSocket connections; imported {}, the one that sent the most messages. Pick another with --index N or --url TEXT:\n{}",
            all.len(),
            best + 1,
            list(&all)
        ));
    }
    Ok(all.swap_remove(best))
}

/// The scenario file's shape, in the order its keys are written.
#[derive(Serialize)]
struct OutScenario {
    version: &'static str,
    target: String,
    metadata: OutMeta,
    phases: Vec<OutPhase>,
}

#[derive(Serialize)]
struct OutMeta {
    name: String,
    description: String,
}

#[derive(Serialize)]
struct OutPhase {
    name: &'static str,
    connections: u64,
    duration: String,
    actions: Vec<OutAction>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
enum OutAction {
    Send {
        payload: String,
        #[serde(skip_serializing_if = "is_one")]
        count: u64,
    },
    Wait {
        duration: String,
    },
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_one(count: &u64) -> bool {
    *count == 1
}

/// A sent message and the pause before it.
struct Step {
    payload: String,
    pause: f64,
}

/// Socket.IO details of a connection.
struct SocketIo {
    event: String,
    namespace: String,
    engine_io_3: bool,
}

/// A Socket.IO message packet's parts: kind, namespace and data. `None` for
/// Engine.IO packets other than messages.
fn socketio_packet(text: &str) -> Option<(u8, &str, &str)> {
    let rest = text.strip_prefix('4')?;
    let kind = *rest.as_bytes().first()?;
    let mut rest = &rest[1..];
    if matches!(kind, b'5' | b'6') {
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        rest = rest[count..].strip_prefix('-')?;
    }
    let mut namespace = "/";
    if rest.starts_with('/') {
        let end = rest.find(',').unwrap_or(rest.len());
        namespace = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or("");
    }
    let id = rest.bytes().take_while(u8::is_ascii_digit).count();
    Some((kind, namespace, &rest[id..]))
}

fn is_socketio(connection: &Connection) -> bool {
    connection.url.contains("EIO=")
        || connection
            .frames
            .iter()
            .find(|f| !f.sent)
            .and_then(|f| f.text.as_deref())
            .is_some_and(|t| t.starts_with("0{\"sid\""))
}

/// Builds the scenario for the HAR at `path`.
pub(crate) fn import(path: &Path, options: &ImportOptions) -> Result<Imported> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::PayloadFile {
        path: path.display().to_string(),
        source: e,
    })?;
    let har: Value = serde_json::from_str(&text)
        .map_err(|e| invalid(format!("{} is not JSON: {e}", path.display())))?;
    let mut notes = Vec::new();
    let connection = choose(connections(&har)?, options, &mut notes)?;
    let scheme_ok = connection.url.starts_with("ws://") || connection.url.starts_with("wss://");
    if !scheme_ok {
        return Err(invalid(format!(
            "the connection's URL '{}' is not ws:// or wss://",
            connection.url
        )));
    }
    let binary = connection
        .frames
        .iter()
        .filter(|f| f.sent && f.text.is_none())
        .count();
    if binary > 0 {
        notes.push(format!(
            "{} the browser sent {} left out: scenario payloads are text",
            plural(binary, "binary message"),
            if binary == 1 { "is" } else { "are" }
        ));
    }

    let (steps, socketio, echo) = if is_socketio(&connection) {
        let (steps, sio) = socketio_steps(&connection, &mut notes);
        (steps, Some(sio), true)
    } else {
        let (steps, echo) = plain_steps(&connection, &mut notes);
        (steps, None, echo)
    };
    if steps.is_empty() {
        return Err(invalid(format!(
            "the connection to {} sent no text messages to import",
            connection.url
        )));
    }
    let sends = steps.len();
    let actions = actions(&steps, options);
    let mut run_flags = Vec::new();
    match &socketio {
        Some(sio) => {
            run_flags.extend(["--mode".to_string(), "socketio".to_string()]);
            run_flags.extend(["--sio-event".to_string(), shell_word(&sio.event)]);
            if sio.namespace != "/" {
                run_flags.extend(["--sio-namespace".to_string(), shell_word(&sio.namespace)]);
            }
            if sio.engine_io_3 {
                run_flags.extend(["--sio-version".to_string(), "2".to_string()]);
            }
        }
        None if echo => {}
        None => run_flags.extend(["--mode".to_string(), "stream".to_string()]),
    }
    let sensitive: Vec<&str> = connection
        .headers
        .iter()
        .map(String::as_str)
        .filter(|h| {
            ["cookie", "authorization"]
                .iter()
                .any(|s| h.eq_ignore_ascii_case(s))
        })
        .collect();
    if !sensitive.is_empty() {
        notes.push(format!(
            "the recorded handshake sent {}; {} not copied: pass {} with -H 'Name: value' if the server needs {}",
            sensitive.join(" and "),
            if sensitive.len() == 1 { "its value is" } else { "their values are" },
            if sensitive.len() == 1 { "it" } else { "them" },
            if sensitive.len() == 1 { "it" } else { "them" }
        ));
    }
    let target = target_url(&connection.url, socketio.is_some());
    let scenario = OutScenario {
        version: "1.0",
        target: target.clone(),
        metadata: OutMeta {
            name: format!("Recorded session ({})", host_of(&target)),
            description: format!(
                "Imported from {} by wsblast import: {} messages of one connection",
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                ),
                sends
            ),
        },
        phases: vec![OutPhase {
            name: "recorded",
            connections: options.connections,
            duration: options.duration.clone(),
            actions,
        }],
    };
    let body = serde_norway::to_string(&scenario)
        .map_err(|e| invalid(format!("cannot write the scenario: {e}")))?;
    let mut yaml = String::new();
    let _ = writeln!(
        yaml,
        "# Generated by wsblast import from {}.",
        path.display()
    );
    let run = std::iter::once("wsblast --scenario FILE".to_string())
        .chain(run_flags.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(yaml, "# Run it with: {run}");
    for note in &notes {
        for (i, line) in note.lines().enumerate() {
            let _ = writeln!(yaml, "# {}{line}", if i == 0 { "Note: " } else { "" });
        }
    }
    yaml.push_str(&body);
    // What `--scenario` would reject must not be written.
    crate::config::ScenarioConfig::from_yaml(&yaml)?;
    Ok(Imported {
        yaml,
        run_flags,
        notes,
        url: target,
        sends,
    })
}

/// The steps of a plain WebSocket connection, and whether every message got
/// a reply before the next one (so it runs in echo mode).
fn plain_steps(connection: &Connection, notes: &mut Vec<String>) -> (Vec<Step>, bool) {
    let mut unanswered = 0;
    let mut sends = 0;
    let frames = &connection.frames;
    for (i, frame) in frames.iter().enumerate() {
        if !frame.sent || frame.text.is_none() {
            continue;
        }
        sends += 1;
        let answered = frames[i + 1..]
            .iter()
            .take_while(|f| !(f.sent && f.text.is_some()))
            .any(|f| !f.sent);
        if !answered {
            unanswered += 1;
        }
    }
    let echo = sends > 0 && unanswered == 0;
    if !echo && sends > 0 {
        notes.push(format!(
            "{unanswered} of {} got no reply before the next one, so the scenario runs in stream mode, which sends without waiting for replies",
            plural(sends, "message")
        ));
    }
    (steps(frames, echo, |_| true, |f| f.text.clone()), echo)
}

/// The steps of a Socket.IO connection: the arguments of the most frequent
/// event the browser emitted, with the event's namespace.
fn socketio_steps(connection: &Connection, notes: &mut Vec<String>) -> (Vec<Step>, SocketIo) {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut auth_keys = None;
    let mut polled_events = 0;
    let sent = connection
        .frames
        .iter()
        .filter(|f| f.sent)
        .filter_map(|f| f.text.as_deref());
    let polled = connection.polling.iter().map(String::as_str);
    for (over_polling, text) in polled.map(|t| (true, t)).chain(sent.map(|t| (false, t))) {
        let Some((kind, namespace, data)) = socketio_packet(text) else {
            continue;
        };
        match kind {
            b'0' if !data.is_empty() => {
                if let Ok(Value::Object(auth)) = serde_json::from_str::<Value>(data) {
                    auth_keys = Some(auth.keys().cloned().collect::<Vec<_>>().join(", "));
                }
            }
            b'2' | b'5' if over_polling => polled_events += 1,
            b'2' => {
                if let Some(name) = event_name(data) {
                    *counts.entry((name, namespace.to_string())).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    let ((event, namespace), _) = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, n)| (k.clone(), *n))
        .unwrap_or_else(|| (("message".to_string(), "/".to_string()), 0));
    let others: Vec<String> = counts
        .iter()
        .filter(|((e, ns), _)| !(e == &event && ns == &namespace))
        .map(|((e, ns), n)| {
            if ns == "/" {
                format!("'{e}' ({n})")
            } else {
                format!("'{e}' in {ns} ({n})")
            }
        })
        .collect();
    if !others.is_empty() {
        notes.push(format!(
            "a Socket.IO scenario emits one event; imported '{event}', the most frequent, and left out {}",
            others.join(", ")
        ));
    }
    if let Some(keys) = auth_keys {
        notes.push(format!(
            "the recorded namespace connect carried an auth object ({keys}); it is not copied: pass it with --sio-auth '{{...}}'"
        ));
    }
    if polled_events > 0 {
        notes.push(format!(
            "{} sent over HTTP long-polling before the WebSocket upgrade {} left out",
            plural(polled_events, "event"),
            if polled_events == 1 { "is" } else { "are" }
        ));
    }
    let binary_events = connection
        .frames
        .iter()
        .filter(|f| f.sent)
        .filter_map(|f| f.text.as_deref().and_then(socketio_packet))
        .filter(|(kind, _, _)| *kind == b'5')
        .count();
    if binary_events > 0 {
        notes.push(format!(
            "{} {} left out: scenario payloads are text",
            plural(binary_events, "binary event"),
            if binary_events == 1 { "is" } else { "are" }
        ));
    }
    let payload = |frame: &Frame| -> Option<String> {
        let (kind, ns, data) = socketio_packet(frame.text.as_deref()?)?;
        if kind != b'2' || ns != namespace {
            return None;
        }
        let Ok(Value::Array(mut values)) = serde_json::from_str::<Value>(data) else {
            return None;
        };
        if values.first().and_then(Value::as_str) != Some(event.as_str()) {
            return None;
        }
        values.remove(0);
        Some(match values.len() {
            0 => String::new(),
            _ => match values.swap_remove(0) {
                Value::String(text) => text,
                other => other.to_string(),
            },
        })
    };
    let engine_io_3 = connection.url.contains("EIO=3");
    (
        // Socket.IO packets are replies; Engine.IO pings are not.
        steps(
            &connection.frames,
            true,
            |f| f.text.as_deref().is_some_and(|t| t.starts_with('4')) || f.text.is_none(),
            payload,
        ),
        SocketIo {
            event,
            namespace,
            engine_io_3,
        },
    )
}

/// The event name of a Socket.IO event packet's data, `["name", ...]`.
fn event_name(data: &str) -> Option<String> {
    match serde_json::from_str::<Value>(data).ok()? {
        Value::Array(values) => values.first()?.as_str().map(str::to_string),
        _ => None,
    }
}

/// The messages `payload` keeps, each with the pause before it: since the
/// last reply (a received frame `is_reply` accepts) after the previous
/// message when `from_reply`, since the previous message otherwise. The
/// first message has no pause. Once a reply has come, a message that is
/// left out and its own replies do not move the start of the pause: the
/// user's time before and after it is all pause in the scenario.
fn steps(
    frames: &[Frame],
    from_reply: bool,
    is_reply: impl Fn(&Frame) -> bool,
    payload: impl Fn(&Frame) -> Option<String>,
) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut last_send: Option<f64> = None;
    let mut last_reply: Option<f64> = None;
    let mut settled = false;
    for frame in frames {
        if !frame.sent {
            if last_send.is_some() && !settled && is_reply(frame) {
                last_reply = Some(frame.time);
            }
            continue;
        }
        let Some(text) = payload(frame) else {
            settled |= last_reply.is_some();
            continue;
        };
        let pause = match (last_send, last_reply) {
            (None, _) => 0.0,
            (Some(_), Some(reply)) if from_reply => frame.time - reply,
            (Some(sent), _) => frame.time - sent,
        };
        steps.push(Step {
            payload: text,
            pause: pause.max(0.0),
        });
        last_send = Some(frame.time);
        last_reply = None;
        settled = false;
    }
    steps
}

/// The actions for `steps`: a `Wait` for every pause of a millisecond or
/// more (at most `max_wait`), and a `Send` with a `count` for a run of the
/// same message with no pause between.
fn actions(steps: &[Step], options: &ImportOptions) -> Vec<OutAction> {
    let mut actions: Vec<OutAction> = Vec::new();
    for step in steps {
        let pause = Duration::from_secs_f64(step.pause).min(options.max_wait);
        let ms = pause.as_millis();
        if !options.no_waits && ms >= 1 {
            actions.push(OutAction::Wait {
                duration: format!("{ms}ms"),
            });
        }
        match actions.last_mut() {
            Some(OutAction::Send { payload, count }) if *payload == step.payload => *count += 1,
            _ => actions.push(OutAction::Send {
                payload: step.payload.clone(),
                count: 1,
            }),
        }
    }
    actions
}

/// The scenario's target: the recorded URL, without the Engine.IO query of
/// a Socket.IO connection, which `--mode socketio` adds itself.
fn target_url(url: &str, socketio: bool) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if socketio {
        let kept: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| !matches!(k.as_ref(), "EIO" | "transport" | "sid" | "t"))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            parsed.set_query(None);
        } else {
            parsed.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    parsed.to_string()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// A word for a shell command line, quoted when it needs to be.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// `1 message`, `2 messages`.
fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn invalid(message: impl Into<String>) -> crate::error::WsBlastError {
    ConfigError::InvalidOption(message.into()).into()
}

/// `wsblast import`: writes the scenario and says how to run it.
pub(crate) fn run_command(args: &crate::cli::ImportArgs) -> Result<()> {
    let options = ImportOptions {
        url_filter: args.url.clone(),
        index: args.index,
        connections: args.connections,
        duration: args.duration.clone(),
        max_wait: crate::config::parse_human_duration(&args.max_wait)?,
        no_waits: args.no_waits,
    };
    crate::config::parse_human_duration(&options.duration)?;
    let imported = import(&args.har, &options)?;
    let file = args.output.as_ref().map_or_else(
        || "scenario.yaml".to_string(),
        |p| shell_word(&p.display().to_string()),
    );
    match &args.output {
        Some(path) => {
            std::fs::write(path, &imported.yaml).map_err(|e| ConfigError::PayloadFile {
                path: path.display().to_string(),
                source: e,
            })?;
            eprintln!(
                "Wrote {}: {} messages to {}",
                path.display(),
                imported.sends,
                imported.url
            );
        }
        None => crate::output::outln!("{}", imported.yaml.trim_end()),
    }
    for note in &imported.notes {
        eprintln!("Note: {note}");
    }
    let run = std::iter::once(format!("wsblast --scenario {file}"))
        .chain(imported.run_flags.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("Run it with: {run}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ImportOptions {
        ImportOptions {
            url_filter: None,
            index: None,
            connections: 10,
            duration: "1m".to_string(),
            max_wait: Duration::from_secs(10),
            no_waits: false,
        }
    }

    fn frame(sent: bool, time: f64, text: &str) -> Value {
        serde_json::json!({
            "type": if sent { "send" } else { "receive" },
            "time": time,
            "opcode": 1,
            "data": text,
        })
    }

    fn har(entries: Vec<Value>) -> tempfile_path::TempHar {
        tempfile_path::TempHar::new(
            &serde_json::json!({ "log": { "version": "1.2", "entries": entries } }),
        )
    }

    fn ws_entry(url: &str, messages: Vec<Value>) -> Value {
        serde_json::json!({
            "_resourceType": "websocket",
            "request": {
                "method": "GET",
                "url": url,
                "headers": [
                    { "name": "Origin", "value": "https://app.example.com" },
                    { "name": "Cookie", "value": "session=secret" }
                ]
            },
            "response": { "status": 101 },
            "_webSocketMessages": messages
        })
    }

    mod tempfile_path {
        pub(super) struct TempHar(pub(super) std::path::PathBuf);
        impl TempHar {
            pub(super) fn new(value: &serde_json::Value) -> Self {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "wsblast-har-{}-{}.har",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::write(&path, value.to_string()).unwrap();
                Self(path)
            }
        }
        impl Drop for TempHar {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    #[test]
    fn a_request_reply_session_becomes_an_echo_scenario_with_think_times() {
        let file = har(vec![ws_entry(
            "wss://chat.example.com/ws?room=1",
            vec![
                frame(true, 100.000, r#"{"op":"join"}"#),
                frame(false, 100.020, r#"{"ok":true}"#),
                frame(true, 101.520, r#"{"op":"say","text":"hi"}"#),
                frame(false, 101.540, r#"{"ok":true}"#),
                frame(true, 101.540, r#"{"op":"say","text":"hi"}"#),
                frame(false, 101.560, r#"{"ok":true}"#),
                frame(true, 200.0, r#"{"op":"leave"}"#),
                frame(false, 200.010, "bye"),
            ],
        )]);
        let imported = import(&file.0, &options()).unwrap();
        assert!(imported.run_flags.is_empty(), "{:?}", imported.run_flags);
        assert_eq!(imported.sends, 4);
        let scenario = crate::config::ScenarioConfig::from_yaml(&imported.yaml).unwrap();
        let yaml = &imported.yaml;
        assert!(
            yaml.contains("target: wss://chat.example.com/ws?room=1"),
            "{yaml}"
        );
        // 1.5 s from the reply to the next message; the 98 s pause is cut
        // to --max-wait; the repeated message is one Send with a count.
        assert!(yaml.contains("duration: 1500ms"), "{yaml}");
        assert!(yaml.contains("duration: 10000ms"), "{yaml}");
        assert!(yaml.contains("count: 2"), "{yaml}");
        assert!(!yaml.contains("secret"), "{yaml}");
        assert!(
            imported.notes.iter().any(|n| n.contains("Cookie")),
            "{:?}",
            imported.notes
        );
        assert_eq!(scenario.phase_names().collect::<Vec<_>>(), ["recorded"]);
    }

    #[test]
    fn a_message_left_out_keeps_the_pause_around_it() {
        // As Chrome recorded it: the binary message is left out, so the
        // pause before "leave" is the user's whole time since the reply to
        // the last kept message, 250 ms before the binary one and 300 ms
        // after its reply.
        let binary = |sent: bool, time: f64| {
            serde_json::json!({
                "type": if sent { "send" } else { "receive" },
                "time": time, "opcode": 2, "data": "AQID",
            })
        };
        let file = har(vec![ws_entry(
            "ws://app.example.com/ws",
            vec![
                frame(true, 10.000, "say"),
                frame(false, 10.002, "said"),
                binary(true, 10.252),
                binary(false, 10.253),
                frame(true, 10.553, "leave"),
                frame(false, 10.555, "bye"),
            ],
        )]);
        let imported = import(&file.0, &options()).unwrap();
        assert!(
            imported.yaml.contains("duration: 551ms"),
            "{}",
            imported.yaml
        );
    }

    #[test]
    fn a_socketio_ping_does_not_shorten_the_pause() {
        let file = har(vec![ws_entry(
            "wss://rt.example.com/socket.io/?EIO=4&transport=websocket",
            vec![
                frame(false, 1.000, r#"0{"sid":"abc","pingInterval":25000}"#),
                frame(true, 1.001, "40"),
                frame(false, 1.002, r#"40{"sid":"def"}"#),
                frame(true, 2.000, r#"420["say","a"]"#),
                frame(false, 2.010, r#"430["ok"]"#),
                frame(false, 3.000, "2"),
                frame(true, 3.001, "3"),
                frame(true, 4.010, r#"421["say","b"]"#),
            ],
        )]);
        let imported = import(&file.0, &options()).unwrap();
        // From the ack at 2.010 to the next event at 4.010; the server's
        // ping at 3.000 and the pong are Engine.IO, not replies.
        assert!(
            imported.yaml.contains("duration: 2000ms"),
            "{}",
            imported.yaml
        );
    }

    #[test]
    fn an_upgraded_socketio_session_reads_what_it_posted_before_the_upgrade() {
        // Browsers start on long-polling and upgrade: the namespace connect
        // with its auth object, and an early emit, go in a POST body.
        let poll = |url: &str, body: &str| {
            serde_json::json!({
                "_resourceType": "xhr",
                "request": { "method": "POST", "url": url, "headers": [],
                             "postData": { "mimeType": "text/plain;charset=UTF-8", "text": body } },
                "response": { "status": 200 }
            })
        };
        let file = har(vec![
            poll(
                "https://rt.example.com/socket.io/?EIO=4&transport=polling&t=x&sid=s1",
                "40/chat,{\"token\":\"secret\"}\u{1e}42/chat,[\"early\",1]",
            ),
            poll(
                "https://rt.example.com/socket.io/?EIO=4&transport=polling&t=y&sid=other",
                "40,{\"user\":\"someone-else\"}",
            ),
            ws_entry(
                "wss://rt.example.com/socket.io/?EIO=4&transport=websocket&sid=s1",
                vec![
                    frame(true, 1.0, "2probe"),
                    frame(false, 1.001, "3probe"),
                    frame(true, 1.002, "5"),
                    frame(true, 2.0, r#"42/chat,0["say","hi"]"#),
                    frame(false, 2.001, r#"43/chat,0["ok"]"#),
                ],
            ),
        ]);
        let imported = import(&file.0, &options()).unwrap();
        let notes = imported.notes.join("\n");
        assert!(notes.contains("auth object (token)"), "{notes}");
        assert!(!notes.contains("user"), "{notes}");
        assert!(
            notes.contains("1 event sent over HTTP long-polling"),
            "{notes}"
        );
        assert!(!imported.yaml.contains("secret"), "{}", imported.yaml);
        assert_eq!(imported.sends, 1);
        assert!(
            imported
                .run_flags
                .windows(2)
                .any(|w| w == ["--sio-namespace", "/chat"]),
            "{:?}",
            imported.run_flags
        );
    }

    #[test]
    fn engine_io_3_polling_bodies_are_length_prefixed() {
        assert_eq!(
            polling_packets("7:40/a,{}12:42/a,[\"é\",1]", true),
            ["40/a,{}", "42/a,[\"é\",1]"]
        );
        assert_eq!(polling_packets("2:40", false), ["2:40"]);
        assert_eq!(polling_packets("9:short", true), Vec::<String>::new());
    }

    #[test]
    fn messages_without_replies_run_in_stream_mode() {
        let file = har(vec![ws_entry(
            "ws://feed.example.com/",
            vec![
                frame(true, 1.0, "subscribe"),
                frame(false, 1.1, "tick"),
                frame(true, 2.0, "ping"),
                frame(true, 2.5, "ping"),
            ],
        )]);
        let imported = import(&file.0, &options()).unwrap();
        assert_eq!(imported.run_flags, ["--mode", "stream"]);
        assert!(
            imported.notes.iter().any(|n| n.contains("2 of 3")),
            "{:?}",
            imported.notes
        );
        // Stream mode pauses count from the previous message.
        assert!(
            imported.yaml.contains("duration: 1000ms"),
            "{}",
            imported.yaml
        );
        assert!(
            imported.yaml.contains("duration: 500ms"),
            "{}",
            imported.yaml
        );
    }

    #[test]
    fn a_socketio_session_becomes_a_socketio_scenario() {
        let file = har(vec![ws_entry(
            "wss://rt.example.com/socket.io/?EIO=3&transport=websocket&token=abc",
            vec![
                frame(false, 1.0, r#"0{"sid":"x","pingInterval":25000}"#),
                frame(false, 1.0, "40"),
                frame(true, 1.1, r#"40/chat,{"token":"t"}"#),
                frame(false, 1.2, "40/chat,"),
                frame(true, 1.3, r#"42/chat,0["say",{"text":"a"}]"#),
                frame(false, 1.4, r#"43/chat,0[{"ok":true}]"#),
                frame(true, 2.4, "2"),
                frame(true, 3.4, r#"42/chat,1["say",{"text":"b"}]"#),
                frame(false, 3.5, r#"43/chat,1[{"ok":true}]"#),
                frame(true, 3.6, r#"42/chat,2["typing"]"#),
            ],
        )]);
        let imported = import(&file.0, &options()).unwrap();
        assert_eq!(
            imported.run_flags,
            [
                "--mode",
                "socketio",
                "--sio-event",
                "say",
                "--sio-namespace",
                "/chat",
                "--sio-version",
                "2"
            ]
        );
        assert_eq!(imported.sends, 2);
        let yaml = &imported.yaml;
        // The Engine.IO query is left to --mode socketio; others are kept.
        assert!(
            yaml.contains("target: wss://rt.example.com/socket.io/?token=abc"),
            "{yaml}"
        );
        assert!(yaml.contains(r#"'{"text":"a"}'"#), "{yaml}");
        assert!(yaml.contains("duration: 2000ms"), "{yaml}");
        let notes = imported.notes.join("\n");
        assert!(notes.contains("'typing' in /chat (1)"), "{notes}");
        assert!(notes.contains("auth object (token)"), "{notes}");
        assert!(!yaml.contains("\"t\""), "{yaml}");
    }

    #[test]
    fn connections_are_chosen_by_messages_url_or_index() {
        let file = har(vec![
            ws_entry("ws://a.example.com/one", vec![frame(true, 1.0, "x")]),
            ws_entry(
                "ws://b.example.com/two",
                vec![frame(true, 1.0, "y"), frame(true, 2.0, "y")],
            ),
            serde_json::json!({ "request": { "url": "https://a.example.com/" } }),
        ]);
        let imported = import(&file.0, &options()).unwrap();
        assert!(imported.url.contains("/two"));
        assert!(
            imported.notes[0].contains("--index N"),
            "{:?}",
            imported.notes
        );
        let mut by_url = options();
        by_url.url_filter = Some("/one".to_string());
        assert!(import(&file.0, &by_url).unwrap().url.contains("/one"));
        let mut by_index = options();
        by_index.index = Some(1);
        assert!(import(&file.0, &by_index).unwrap().url.contains("/one"));
        by_index.index = Some(3);
        let err = import(&file.0, &by_index).unwrap_err().to_string();
        assert!(err.contains("2 matching WebSocket connections"), "{err}");
    }

    #[test]
    fn a_har_without_websocket_messages_says_how_to_record_one() {
        let file = har(vec![
            serde_json::json!({ "request": { "url": "https://x/" } }),
        ]);
        let err = import(&file.0, &options()).unwrap_err().to_string();
        assert!(err.contains("Export HAR"), "{err}");
        let not_har = tempfile_path::TempHar::new(&serde_json::json!({ "x": 1 }));
        assert!(
            import(&not_har.0, &options())
                .unwrap_err()
                .to_string()
                .contains("not a HAR")
        );
    }

    #[test]
    fn no_waits_and_shell_words() {
        let file = har(vec![ws_entry(
            "ws://x.example.com/",
            vec![
                frame(true, 1.0, "a"),
                frame(false, 1.1, "r"),
                frame(true, 5.0, "b"),
                frame(false, 5.1, "r"),
            ],
        )]);
        let mut opts = options();
        opts.no_waits = true;
        assert!(!import(&file.0, &opts).unwrap().yaml.contains("Wait"));
        assert_eq!(shell_word("chat"), "chat");
        assert_eq!(shell_word("new message"), "'new message'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }
}
