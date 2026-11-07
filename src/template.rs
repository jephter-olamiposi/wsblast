//! Payload and header templates.
//!
//! A template is parsed once into literal text and placeholders, so sending
//! a frame copies the literal parts and formats the values instead of
//! searching the text for every placeholder on every send.
//!
//! The placeholders are `{{seq}}`, `{{timestamp}}`, `{{worker_id}}` and
//! `{{data.COLUMN}}`, a column of the run's data file ([`crate::data`]).
//! Any other `{{...}}` is kept as text, as it always was, so payloads that
//! carry their own template syntax are sent unchanged.

use crate::data::DataSet;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    source: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Seq,
    Timestamp,
    WorkerId,
    /// A data file column, by name; checked against the file with
    /// [`Template::check_data`].
    Data(String),
}

/// The values a template is filled with for one frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Values<'a> {
    pub(crate) seq: u64,
    pub(crate) worker_id: usize,
    /// The data file and the connection's row in it.
    pub(crate) data: Option<(&'a DataSet, usize)>,
}

impl Template {
    /// Parses `source`.
    pub(crate) fn parse(source: impl Into<String>) -> Self {
        let source = source.into();
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = source.as_str();
        while let Some(start) = rest.find("{{") {
            let Some(len) = rest[start + 2..].find("}}") else {
                break;
            };
            let name = &rest[start + 2..start + 2 + len];
            let part = match name {
                "seq" => Some(Part::Seq),
                "timestamp" => Some(Part::Timestamp),
                "worker_id" => Some(Part::WorkerId),
                _ => name
                    .strip_prefix("data.")
                    .filter(|column| !column.is_empty())
                    .map(|column| Part::Data(column.to_string())),
            };
            let end = start + 2 + len + 2;
            match part {
                Some(part) => {
                    text.push_str(&rest[..start]);
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(part);
                }
                // Not a placeholder: kept as written.
                None => text.push_str(&rest[..end]),
            }
            rest = &rest[end..];
        }
        text.push_str(rest);
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Self { source, parts }
    }

    /// The text the template was parsed from.
    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    /// Whether the template has a placeholder, so that it can differ
    /// between frames or connections.
    pub(crate) fn is_dynamic(&self) -> bool {
        self.parts.iter().any(|part| !matches!(part, Part::Text(_)))
    }

    /// Whether the template has `{{seq}}`.
    pub(crate) fn has_seq(&self) -> bool {
        self.parts.contains(&Part::Seq)
    }

    /// The data file columns the template reads.
    pub(crate) fn data_columns(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            Part::Data(column) => Some(column.as_str()),
            _ => None,
        })
    }

    /// Checks that every `{{data.COLUMN}}` names a column of `data`, or
    /// that there are none when the run has no data file. The error
    /// message names the column and what is available.
    pub(crate) fn check_data(&self, data: Option<&DataSet>) -> Result<(), String> {
        for column in self.data_columns() {
            match data {
                None => {
                    return Err(format!(
                        "{{{{data.{column}}}}} needs a data file: pass --data FILE, or data: in the scenario"
                    ));
                }
                Some(data) if data.column(column).is_none() => {
                    return Err(format!(
                        "{{{{data.{column}}}}}: {} has no column '{column}' (columns: {})",
                        data.path(),
                        data.columns().join(", ")
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Fills the template in.
    pub(crate) fn render(&self, values: Values<'_>) -> String {
        let mut out = String::with_capacity(self.source.len() + 16);
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Seq => {
                    let _ = write!(out, "{}", values.seq);
                }
                Part::Timestamp => {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let _ = write!(out, "{now_ms}");
                }
                Part::WorkerId => {
                    let _ = write!(out, "{}", values.worker_id);
                }
                Part::Data(column) => {
                    if let Some((data, row)) = values.data
                        && let Some(value) = data.value(row, column)
                    {
                        out.push_str(value);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(seq: u64, worker_id: usize) -> Values<'static> {
        Values {
            seq,
            worker_id,
            data: None,
        }
    }

    #[test]
    fn placeholders_are_filled_in_and_other_braces_are_kept() {
        let template = Template::parse(r#"{"id":{{seq}},"w":{{worker_id}},"x":"{{name}}"}"#);
        assert!(template.is_dynamic());
        assert!(template.has_seq());
        assert_eq!(
            template.render(values(7, 3)),
            r#"{"id":7,"w":3,"x":"{{name}}"}"#
        );
    }

    #[test]
    fn text_without_placeholders_is_static() {
        for text in ["plain", "{{name}} and {{", "}} {{data.}}", ""] {
            let template = Template::parse(text);
            assert!(!template.is_dynamic(), "{text}");
            assert_eq!(template.render(values(1, 1)), text);
        }
    }

    #[test]
    fn timestamp_is_unix_milliseconds() {
        let rendered = Template::parse("{{timestamp}}").render(values(0, 0));
        let ms: u128 = rendered.parse().unwrap();
        assert!(ms > 1_700_000_000_000, "{rendered}");
    }

    #[test]
    fn data_columns_come_from_the_connection_row() {
        let data = DataSet::parse("users.csv", "user,token\nada,t1\nbob,t2\n").unwrap();
        let template = Template::parse("{{data.user}}:{{data.token}}:{{seq}}");
        assert_eq!(
            template.data_columns().collect::<Vec<_>>(),
            ["user", "token"]
        );
        assert!(template.check_data(Some(&data)).is_ok());
        let render = |worker_id| {
            template.render(Values {
                seq: 9,
                worker_id,
                data: Some((&data, data.row_for(worker_id))),
            })
        };
        assert_eq!(render(0), "ada:t1:9");
        assert_eq!(render(1), "bob:t2:9");
        // More connections than rows: the rows repeat.
        assert_eq!(render(2), "ada:t1:9");
    }

    #[test]
    fn unknown_columns_and_missing_files_are_named() {
        let template = Template::parse("{{data.email}}");
        let missing = template.check_data(None).unwrap_err();
        assert!(missing.contains("needs a data file"), "{missing}");
        let data = DataSet::parse("users.csv", "user,token\nada,t1\n").unwrap();
        let unknown = template.check_data(Some(&data)).unwrap_err();
        assert!(
            unknown.contains("users.csv has no column 'email' (columns: user, token)"),
            "{unknown}"
        );
    }
}
