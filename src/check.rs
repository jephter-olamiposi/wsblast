//! Checks on echo replies (`expect`, `--expect`) and values taken from
//! them (`capture`).
//!
//! A reply that fails a check still records its round-trip time, since it
//! did arrive, and counts one `check_failed` error against the message it
//! answers, as a reply timeout would. A capture whose pointer finds nothing
//! is a failed check too.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::OnceCell;
use tokio_tungstenite::tungstenite::protocol::Message;

/// What a reply must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Check {
    /// The reply is JSON with a value at the RFC 6901 pointer, equal to
    /// `equals` when it is given.
    Json {
        pointer: String,
        equals: Option<Value>,
    },
    /// The reply's text contains this.
    Contains(String),
}

impl Check {
    /// Parses the `--expect` spelling: `json:/pointer`, `json:/pointer=VALUE`
    /// or `contains:TEXT`. `VALUE` is read as JSON when it is valid JSON,
    /// and as a string otherwise, so `=ok`, `="ok"` and `=200` all work.
    pub(crate) fn parse_flag(spec: &str) -> Result<Self, String> {
        if let Some(text) = spec.strip_prefix("contains:") {
            if text.is_empty() {
                return Err("--expect contains: needs the text to look for".to_string());
            }
            return Ok(Self::Contains(text.to_string()));
        }
        let Some(rest) = spec.strip_prefix("json:") else {
            return Err(format!(
                "invalid --expect '{spec}': expected json:/pointer, json:/pointer=VALUE or contains:TEXT"
            ));
        };
        let (pointer, equals) = match rest.split_once('=') {
            Some((pointer, value)) => (
                pointer,
                Some(
                    serde_json::from_str(value)
                        .unwrap_or_else(|_| Value::String(value.to_string())),
                ),
            ),
            None => (rest, None),
        };
        let check = Self::Json {
            pointer: pointer.to_string(),
            equals,
        };
        check.validate()?;
        Ok(check)
    }

    /// Checks a JSON pointer's syntax.
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Json { pointer, .. } => check_pointer(pointer),
            Self::Contains(text) if text.is_empty() => {
                Err("contains needs the text to look for".to_string())
            }
            Self::Contains(_) => Ok(()),
        }
    }

    /// The `--expect` spelling of the check.
    #[cfg(any(test, feature = "distributed"))]
    pub(crate) fn flag_value(&self) -> String {
        match self {
            Self::Json {
                pointer,
                equals: None,
            } => format!("json:{pointer}"),
            Self::Json {
                pointer,
                equals: Some(value),
            } => format!("json:{pointer}={value}"),
            Self::Contains(text) => format!("contains:{text}"),
        }
    }

    pub(crate) fn passes(&self, reply: &Reply<'_>) -> bool {
        match self {
            Self::Contains(text) => reply
                .text()
                .is_some_and(|reply| reply.contains(text.as_str())),
            Self::Json { pointer, equals } => {
                match reply.json().and_then(|json| json.pointer(pointer)) {
                    None => false,
                    Some(found) => equals.as_ref().is_none_or(|expected| same(found, expected)),
                }
            }
        }
    }
}

/// Rejects a string that is not an RFC 6901 pointer: empty, or a sequence
/// of `/`-prefixed tokens.
pub(crate) fn check_pointer(pointer: &str) -> Result<(), String> {
    if pointer.is_empty() || pointer.starts_with('/') {
        Ok(())
    } else {
        Err(format!(
            "'{pointer}' is not a JSON pointer: it must be empty or start with '/', such as /status"
        ))
    }
}

/// Whether a reply's value equals the expected one. A number or boolean
/// also equals its text, so `equals: "200"` matches `200` and `equals: 200`
/// matches `"200"`.
fn same(found: &Value, expected: &Value) -> bool {
    if found == expected {
        return true;
    }
    match (found, expected) {
        (Value::String(text), other @ (Value::Number(_) | Value::Bool(_)))
        | (other @ (Value::Number(_) | Value::Bool(_)), Value::String(text)) => {
            serde_json::from_str::<Value>(text).is_ok_and(|parsed| parsed == *other)
        }
        _ => false,
    }
}

/// A variable a reply sets: the value at `pointer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Capture {
    /// The variable's position in its phase's variables.
    pub(crate) var: usize,
    pub(crate) pointer: String,
}

impl Capture {
    /// The value to store: a string as it is, any other JSON value as JSON
    /// text. `None` when the reply is not JSON or has nothing at the
    /// pointer.
    pub(crate) fn value(&self, reply: &Reply<'_>) -> Option<String> {
        let found = reply.json()?.pointer(&self.pointer)?;
        Some(match found {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    }
}

/// A reply frame, with its JSON parsed when a check first needs it, and
/// at most once.
pub(crate) struct Reply<'a> {
    text: Option<&'a str>,
    json: OnceCell<Option<Value>>,
}

impl<'a> Reply<'a> {
    pub(crate) fn new(frame: &'a Message) -> Self {
        let text = match frame {
            Message::Text(text) => Some(text.as_str()),
            Message::Binary(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        };
        Self {
            text,
            json: OnceCell::new(),
        }
    }

    fn text(&self) -> Option<&'a str> {
        self.text
    }

    fn json(&self) -> Option<&Value> {
        self.json
            .get_or_init(|| self.text.and_then(|text| serde_json::from_str(text).ok()))
            .as_ref()
    }
}

/// Whether `name` can name a captured variable: a letter or `_`, then
/// letters, digits or `_`, and not one of the built-in placeholders.
#[cfg(any(test, feature = "scenario"))]
pub(crate) fn check_var_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!(
            "'{name}' cannot name a variable: use letters, digits and _, starting with a letter or _"
        ));
    }
    if matches!(name, "seq" | "timestamp" | "worker_id" | "data") {
        return Err(format!(
            "'{name}' is a built-in placeholder and cannot be captured"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passes(check: &Check, reply: &str) -> bool {
        let frame = Message::Text(reply.into());
        check.passes(&Reply::new(&frame))
    }

    #[test]
    fn flag_spellings() {
        assert_eq!(
            Check::parse_flag("json:/status=ok").unwrap(),
            Check::Json {
                pointer: "/status".into(),
                equals: Some(Value::String("ok".into()))
            }
        );
        assert_eq!(
            Check::parse_flag("json:/code=200").unwrap(),
            Check::Json {
                pointer: "/code".into(),
                equals: Some(Value::from(200))
            }
        );
        assert_eq!(
            Check::parse_flag("json:/id").unwrap(),
            Check::Json {
                pointer: "/id".into(),
                equals: None
            }
        );
        assert_eq!(
            Check::parse_flag("contains:pong").unwrap(),
            Check::Contains("pong".into())
        );
        for bad in ["status", "json:status", "contains:", "regex:x"] {
            assert!(Check::parse_flag(bad).is_err(), "{bad}");
        }
        for spec in ["json:/a=\"ok\"", "json:/a", "contains:x y"] {
            let check = Check::parse_flag(spec).unwrap();
            assert_eq!(
                Check::parse_flag(&check.flag_value()).unwrap(),
                check,
                "{spec}"
            );
        }
    }

    #[test]
    fn json_checks_compare_values_and_their_text() {
        let ok = Check::parse_flag("json:/status=ok").unwrap();
        assert!(passes(&ok, r#"{"status":"ok"}"#));
        assert!(!passes(&ok, r#"{"status":"error"}"#));
        assert!(!passes(&ok, r#"{"other":1}"#));
        assert!(!passes(&ok, "not json"));

        let code = Check::parse_flag("json:/code=200").unwrap();
        assert!(passes(&code, r#"{"code":200}"#));
        assert!(passes(&code, r#"{"code":"200"}"#));
        assert!(!passes(&code, r#"{"code":201}"#));

        let nested = Check::parse_flag("json:/data/0/id").unwrap();
        assert!(passes(&nested, r#"{"data":[{"id":null}]}"#));
        assert!(!passes(&nested, r#"{"data":[]}"#));

        assert!(passes(&Check::Contains("pong".into()), "a pong!"));
        assert!(!passes(&Check::Contains("pong".into()), "ping"));
    }

    #[test]
    fn captures_take_strings_as_they_are_and_other_values_as_json() {
        let frame = Message::Text(r#"{"session":"s-1","n":7,"o":{"a":1}}"#.into());
        let reply = Reply::new(&frame);
        let capture = |pointer: &str| Capture {
            var: 0,
            pointer: pointer.into(),
        };
        assert_eq!(capture("/session").value(&reply).as_deref(), Some("s-1"));
        assert_eq!(capture("/n").value(&reply).as_deref(), Some("7"));
        assert_eq!(capture("/o").value(&reply).as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(capture("/missing").value(&reply), None);
    }

    #[test]
    fn variable_names() {
        for good in ["session", "_id", "sub2"] {
            assert!(check_var_name(good).is_ok(), "{good}");
        }
        for bad in ["", "2x", "a-b", "seq", "data", "a.b"] {
            assert!(check_var_name(bad).is_err(), "{bad}");
        }
    }
}
