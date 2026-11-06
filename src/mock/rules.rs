//! Scripted replies for `wsblast mock --rules FILE`.
//!
//! A rules file is a list of rules tried in order for every text or binary
//! frame a connection receives. The first rule that matches decides what the
//! connection sends back: zero or more reply frames built from templates,
//! after an optional delay, optionally followed by a Close frame. A frame no
//! rule matches is echoed, or ignored with `unmatched: ignore`.
//!
//! ```yaml
//! version: "1.0"
//! rules:
//!   - match: { json: /type, equals: subscribe }
//!     reply: '{"type":"ack","id":{{json:/id}}}'
//!     delay: 20ms
//!   - match: { text: ping }
//!     reply: pong
//!   - after: 100
//!     close: { code: 4000, reason: done }
//! unmatched: echo
//! ```
//!
//! The only state is each connection's count of received frames, which
//! `after` and `{{seq}}` read. See `docs/mock.md`, section 9.

use crate::error::Result;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::Message;

/// What a connection does with a frame no rule matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Unmatched {
    /// Send the frame back unchanged, as the mock does without rules.
    #[default]
    Echo,
    /// Send nothing.
    Ignore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    version: String,
    rules: Vec<RuleSpec>,
    #[serde(default)]
    unmatched: Unmatched,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    #[serde(default, rename = "match")]
    matcher: Option<MatchSpec>,
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    reply: Option<OneOrMany>,
    #[serde(default)]
    delay: Option<String>,
    #[serde(default)]
    close: Option<CloseSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchSpec {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    json: Option<String>,
    #[serde(default)]
    equals: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CloseSpec {
    Code(u16),
    Full {
        code: u16,
        #[serde(default)]
        reason: String,
    },
}

/// Which frames a rule applies to.
#[derive(Debug, Clone, PartialEq)]
enum Matcher {
    /// Every text or binary frame.
    Any,
    /// Text frames equal to this text.
    Text(String),
    /// Frames whose JSON has a value at the pointer, equal to `equals` when
    /// given.
    Json {
        pointer: String,
        equals: Option<serde_json::Value>,
    },
}

/// One piece of a reply template.
#[derive(Debug, Clone, PartialEq)]
enum Part {
    Literal(String),
    /// `{{request}}`: the received frame as text.
    Request,
    /// `{{seq}}`: the frame's number on its connection, from 1.
    Seq,
    /// `{{json:/pointer}}`: the JSON value at the pointer, written as JSON;
    /// `null` when absent.
    Json(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Template(Vec<Part>);

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    matcher: Matcher,
    /// The first frame number (from 1) the rule applies to.
    after: u64,
    replies: Vec<Template>,
    delay: Option<Duration>,
    close: Option<(u16, String)>,
}

/// A validated rules file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rules {
    pub(crate) source: String,
    rules: Vec<Rule>,
    pub(crate) unmatched: Unmatched,
}

/// What a connection does with one received frame.
#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    /// Send the frame back unchanged.
    Echo,
    /// Send nothing.
    Ignore,
    /// Send these frames, after `delay` instead of the server's `--delay`
    /// when one is set, then close when `close` is set.
    Reply {
        frames: Vec<Message>,
        delay: Option<Duration>,
        close: Option<(u16, String)>,
    },
}

impl Rules {
    /// Reads and validates a rules file.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let source = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .map_err(|err| super::invalid(format!("--rules: cannot read '{source}': {err}")))?;
        Self::parse(&text, source)
    }

    pub(crate) fn parse(text: &str, source: String) -> Result<Self> {
        let fail = |message: String| super::invalid(format!("--rules '{source}': {message}"));
        let file: RulesFile = serde_norway::from_str(text).map_err(|err| fail(err.to_string()))?;
        if file.version != "1.0" {
            return Err(fail(format!(
                "version must be \"1.0\", found \"{}\"",
                file.version
            )));
        }
        if file.rules.is_empty() {
            return Err(fail("rules must list at least one rule".to_string()));
        }
        let rules = file
            .rules
            .into_iter()
            .enumerate()
            .map(|(index, spec)| {
                Rule::compile(spec)
                    .map_err(|message| fail(format!("rule {}: {message}", index + 1)))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            source,
            rules,
            unmatched: file.unmatched,
        })
    }

    /// Number of rules.
    pub(crate) fn len(&self) -> usize {
        self.rules.len()
    }

    /// Decides what to send for `frame`, the `seq`-th data frame (from 1)
    /// its connection received.
    pub(crate) fn decide(&self, frame: &Message, seq: u64) -> Decision {
        let text: Option<&str> = match frame {
            Message::Text(text) => Some(text.as_str()),
            Message::Binary(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        };
        // Parsed once, and only when a rule or a template needs it.
        let mut json: Option<Option<serde_json::Value>> = None;
        let mut parsed = || {
            json.get_or_insert_with(|| text.and_then(|t| serde_json::from_str(t).ok()))
                .clone()
        };
        for rule in &self.rules {
            if seq < rule.after {
                continue;
            }
            let matched = match &rule.matcher {
                Matcher::Any => true,
                Matcher::Text(expected) => {
                    matches!(frame, Message::Text(t) if t.as_str() == expected)
                }
                Matcher::Json { pointer, equals } => {
                    let value = parsed();
                    match (value.as_ref().and_then(|v| v.pointer(pointer)), equals) {
                        (Some(found), Some(expected)) => found == expected,
                        (Some(_), None) => true,
                        (None, _) => false,
                    }
                }
            };
            if !matched {
                continue;
            }
            let needs_json = rule
                .replies
                .iter()
                .any(|t| t.0.iter().any(|p| matches!(p, Part::Json(_))));
            let value = if needs_json { parsed() } else { None };
            let frames = rule
                .replies
                .iter()
                .map(|template| {
                    Message::text(template.render(text.unwrap_or_default(), seq, value.as_ref()))
                })
                .collect();
            return Decision::Reply {
                frames,
                delay: rule.delay,
                close: rule.close.clone(),
            };
        }
        match self.unmatched {
            Unmatched::Echo => Decision::Echo,
            Unmatched::Ignore => Decision::Ignore,
        }
    }
}

impl Rule {
    fn compile(spec: RuleSpec) -> std::result::Result<Self, String> {
        let matcher = match spec.matcher {
            None => Matcher::Any,
            Some(MatchSpec {
                text: Some(text),
                json: None,
                equals: None,
            }) => Matcher::Text(text),
            Some(MatchSpec {
                text: None,
                json: Some(pointer),
                equals,
            }) => {
                if !pointer.starts_with('/') {
                    return Err(format!(
                        "match.json must be a JSON pointer starting with '/', found '{pointer}'"
                    ));
                }
                Matcher::Json { pointer, equals }
            }
            Some(MatchSpec {
                text: Some(_),
                json: Some(_),
                ..
            }) => return Err("match takes text or json, not both".to_string()),
            Some(MatchSpec {
                text: Some(_),
                equals: Some(_),
                ..
            }) => return Err("match.equals needs match.json".to_string()),
            Some(MatchSpec { .. }) => {
                return Err(
                    "match needs text or json; leave match out to match every frame".to_string(),
                );
            }
        };
        let after = spec.after.unwrap_or(1);
        if after == 0 {
            return Err("after counts frames from 1 and must be at least 1".to_string());
        }
        if spec.reply.is_none() && spec.close.is_none() {
            return Err("a rule needs reply, close or both; reply: [] sends nothing".to_string());
        }
        let replies = match spec.reply {
            None => Vec::new(),
            Some(OneOrMany::One(one)) => vec![one],
            Some(OneOrMany::Many(many)) => many,
        }
        .iter()
        .map(|raw| Template::parse(raw))
        .collect::<std::result::Result<Vec<_>, _>>()?;
        let delay = spec
            .delay
            .as_deref()
            .map(|raw| crate::config::parse_human_duration(raw).map_err(|e| format!("delay: {e}")))
            .transpose()?;
        let close = match spec.close {
            None => None,
            Some(CloseSpec::Code(code)) => Some((code, String::new())),
            Some(CloseSpec::Full { code, reason }) => Some((code, reason)),
        };
        if let Some((code, reason)) = &close {
            if !crate::config::is_sendable_close_code(*code) {
                return Err(format!(
                    "close code {code} cannot be sent; use 1000-1003, 1007-1014 or 3000-4999"
                ));
            }
            // A Close frame's payload is at most 125 bytes, 2 of them the code.
            if reason.len() > 123 {
                return Err("close reason must be at most 123 bytes".to_string());
            }
        }
        Ok(Self {
            matcher,
            after,
            replies,
            delay,
            close,
        })
    }
}

impl Template {
    fn parse(raw: &str) -> std::result::Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = raw;
        while let Some(start) = rest.find("{{") {
            if start > 0 {
                parts.push(Part::Literal(rest[..start].to_string()));
            }
            let after_open = &rest[start + 2..];
            let Some(end) = after_open.find("}}") else {
                return Err(format!("reply '{raw}' has '{{{{' without a closing '}}}}'"));
            };
            let name = after_open[..end].trim();
            parts.push(match name {
                "request" => Part::Request,
                "seq" => Part::Seq,
                _ => match name.strip_prefix("json:") {
                    Some(pointer) if pointer.starts_with('/') => Part::Json(pointer.to_string()),
                    _ => {
                        return Err(format!(
                            "reply '{raw}' uses {{{{{name}}}}}; use {{{{request}}}}, {{{{seq}}}} or {{{{json:/pointer}}}}"
                        ));
                    }
                },
            });
            rest = &after_open[end + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_string()));
        }
        Ok(Self(parts))
    }

    fn render(&self, request: &str, seq: u64, json: Option<&serde_json::Value>) -> String {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Request => out.push_str(request),
                Part::Seq => out.push_str(&seq.to_string()),
                Part::Json(pointer) => match json.and_then(|value| value.pointer(pointer)) {
                    Some(value) => out.push_str(&value.to_string()),
                    None => out.push_str("null"),
                },
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Rules> {
        Rules::parse(text, "rules.yaml".to_string())
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    fn reply_texts(decision: Decision) -> Vec<String> {
        match decision {
            Decision::Reply { frames, .. } => frames
                .into_iter()
                .map(|f| f.into_text().unwrap().to_string())
                .collect(),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    const EXAMPLE: &str = r#"
version: "1.0"
rules:
  - match: { json: /type, equals: subscribe }
    reply: '{"type":"ack","id":{{json:/id}},"n":{{seq}}}'
    delay: 20ms
  - match: { text: ping }
    reply: [pong, "again {{request}}"]
  - after: 3
    close: { code: 4000, reason: done }
unmatched: ignore
"#;

    #[test]
    fn the_first_matching_rule_replies_with_rendered_templates() {
        let rules = parse(EXAMPLE).unwrap();
        assert_eq!(rules.len(), 3);
        let decision = rules.decide(&Message::text(r#"{"type":"subscribe","id":"a7"}"#), 1);
        let Decision::Reply { delay, close, .. } = &decision else {
            panic!("{decision:?}");
        };
        assert_eq!(*delay, Some(Duration::from_millis(20)));
        assert!(close.is_none());
        assert_eq!(reply_texts(decision), [r#"{"type":"ack","id":"a7","n":1}"#]);
        assert_eq!(
            reply_texts(rules.decide(&Message::text("ping"), 2)),
            ["pong", "again ping"]
        );
    }

    #[test]
    fn after_counts_frames_and_unmatched_frames_follow_the_setting() {
        let rules = parse(EXAMPLE).unwrap();
        assert_eq!(rules.decide(&Message::text("hello"), 2), Decision::Ignore);
        assert_eq!(
            rules.decide(&Message::text("hello"), 3),
            Decision::Reply {
                frames: Vec::new(),
                delay: None,
                close: Some((4000, "done".to_string())),
            }
        );
        // Earlier rules still win from frame 3 on.
        assert_eq!(
            reply_texts(rules.decide(&Message::text("ping"), 9))[0],
            "pong"
        );
        let echo =
            parse("version: \"1.0\"\nrules:\n  - match: { text: x }\n    reply: y\n").unwrap();
        assert_eq!(echo.decide(&Message::text("z"), 1), Decision::Echo);
    }

    #[test]
    fn json_matching_needs_the_value_and_handles_binary_and_bad_json() {
        let rules = parse(
            "version: \"1.0\"\nrules:\n  - match: { json: /id }\n    reply: 'got {{json:/id}} {{json:/missing}}'\n",
        )
        .unwrap();
        assert_eq!(
            reply_texts(rules.decide(&Message::binary(br#"{"id":5}"#.to_vec()), 1)),
            ["got 5 null"]
        );
        assert_eq!(rules.decide(&Message::text("not json"), 1), Decision::Echo);
        assert_eq!(
            rules.decide(&Message::text(r#"{"other":1}"#), 1),
            Decision::Echo
        );
        // Text matching compares whole text frames only.
        let text = parse("version: \"1.0\"\nrules:\n  - match: { text: ping }\n    reply: pong\n")
            .unwrap();
        assert_eq!(
            text.decide(&Message::binary(b"ping".to_vec()), 1),
            Decision::Echo
        );
    }

    #[test]
    fn an_empty_reply_list_sends_nothing() {
        let rules =
            parse("version: \"1.0\"\nrules:\n  - match: { text: ping }\n    reply: []\n").unwrap();
        assert_eq!(
            rules.decide(&Message::text("ping"), 1),
            Decision::Reply {
                frames: Vec::new(),
                delay: None,
                close: None,
            }
        );
        assert_eq!(rules.decide(&Message::text("data"), 2), Decision::Echo);
    }

    #[test]
    fn invalid_files_are_rejected_with_the_reason() {
        let cases = [
            (
                "version: \"2.0\"\nrules: [{reply: x}]\n",
                "version must be \"1.0\"",
            ),
            ("version: \"1.0\"\nrules: []\n", "at least one rule"),
            (
                "version: \"1.0\"\nrules: [{reply: x, colour: red}]\n",
                "colour",
            ),
            (
                "version: \"1.0\"\nrules: [{match: {text: a, json: /b}, reply: x}]\n",
                "not both",
            ),
            (
                "version: \"1.0\"\nrules: [{match: {json: id}, reply: x}]\n",
                "starting with '/'",
            ),
            (
                "version: \"1.0\"\nrules: [{match: {text: a, equals: 1}, reply: x}]\n",
                "needs match.json",
            ),
            (
                "version: \"1.0\"\nrules: [{match: {}, reply: x}]\n",
                "needs text or json",
            ),
            (
                "version: \"1.0\"\nrules: [{after: 0, reply: x}]\n",
                "at least 1",
            ),
            (
                "version: \"1.0\"\nrules: [{close: 1005}]\n",
                "close code 1005",
            ),
            (
                "version: \"1.0\"\nrules: [{reply: 'a {{nope}}'}]\n",
                "{{nope}}",
            ),
            (
                "version: \"1.0\"\nrules: [{reply: 'a {{seq'}]\n",
                "without a closing",
            ),
            (
                "version: \"1.0\"\nrules: [{delay: soon, reply: x}]\n",
                "delay",
            ),
            (
                "version: \"1.0\"\nrules: [{match: {text: a}}]\n",
                "needs reply, close or both",
            ),
            (
                "version: \"1.0\"\nrules: [{reply: x}]\nunmatched: drop\n",
                "unmatched",
            ),
        ];
        for (text, expected) in cases {
            let message = error(text);
            assert!(message.contains(expected), "{text:?}: {message}");
            assert!(message.contains("--rules 'rules.yaml'"), "{message}");
        }
        assert!(error("version: \"1.0\"\nrules: [{reply: x}, {close: 5000}]\n").contains("rule 2"));
    }
}
