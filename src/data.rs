//! Per-connection test data (`--data FILE`).
//!
//! A data file is a table: a CSV file with a header row, or JSON Lines with
//! one flat object per line. Connection `n` of a run uses row `n` modulo
//! the number of rows, so a file with at least as many rows as connections
//! gives every connection its own values, which templates read as
//! `{{data.COLUMN}}`. In a distributed run, the agents interleave their
//! connections over the rows (see [`DataSet::with_share`]), so no two
//! machines use the same row while there are rows to spare.

use std::path::Path;

/// The largest data file wsblast reads, so a wrong path cannot fill memory.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A loaded data file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DataSet {
    path: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    /// On a distributed agent: its position and the number of agents.
    share: Option<(usize, usize)>,
}

impl DataSet {
    /// Reads a data file. JSON Lines when the name ends in `.jsonl` or
    /// `.ndjson`, or the first character that is not white space is `{`;
    /// CSV otherwise.
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let shown = path.display().to_string();
        let size = std::fs::metadata(path)
            .map_err(|e| format!("{shown}: {e}"))?
            .len();
        if size > MAX_BYTES {
            return Err(format!(
                "{shown}: {size} bytes is more than the {MAX_BYTES} a data file may have"
            ));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
        Self::parse(&shown, &text)
    }

    /// Parses the text of a data file named `path`.
    pub(crate) fn parse(path: &str, text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let lower = path.to_ascii_lowercase();
        let json_lines = lower.ends_with(".jsonl")
            || lower.ends_with(".ndjson")
            || text.trim_start().starts_with('{');
        let (columns, rows) = if json_lines {
            parse_json_lines(text)
        } else {
            parse_csv(text)
        }
        .map_err(|e| format!("{path}: {e}"))?;
        if rows.is_empty() {
            return Err(format!("{path}: no rows"));
        }
        Ok(Self {
            path: path.to_string(),
            columns,
            rows,
            share: None,
        })
    }

    /// Builds a data set from columns and rows, as a distributed agent
    /// receives it.
    #[cfg(feature = "distributed")]
    pub(crate) fn from_parts(
        path: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    ) -> Result<Self, String> {
        if rows.is_empty() {
            return Err(format!("{path}: no rows"));
        }
        if let Some(n) = rows.iter().position(|row| row.len() != columns.len()) {
            return Err(format!(
                "{path}: row {} has {} values for {} columns",
                n + 1,
                rows[n].len(),
                columns.len()
            ));
        }
        Ok(Self {
            path,
            columns,
            rows,
            share: None,
        })
    }

    /// The same rows, for agent `index` of `agents`: its connection `n`
    /// uses row `n * agents + index`, so agents never share a row while
    /// the file has enough of them.
    #[cfg(any(test, feature = "distributed"))]
    pub(crate) fn with_share(mut self, index: usize, agents: usize) -> Self {
        self.share = (agents > 1).then_some((index, agents));
        self
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn columns(&self) -> &[String] {
        &self.columns
    }

    #[cfg(feature = "distributed")]
    pub(crate) fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The position of a column.
    pub(crate) fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column == name)
    }

    /// The row connection `worker_id` uses.
    pub(crate) fn row_for(&self, worker_id: usize) -> usize {
        let global = match self.share {
            Some((index, agents)) => worker_id.saturating_mul(agents).saturating_add(index),
            None => worker_id,
        };
        global % self.rows.len()
    }

    /// A row's value in a column.
    pub(crate) fn value(&self, row: usize, column: &str) -> Option<&str> {
        let index = self.column(column)?;
        self.rows.get(row)?.get(index).map(String::as_str)
    }
}

/// CSV as RFC 4180 describes it: comma separated, a header row, fields in
/// double quotes may hold commas, line breaks and `""` for a quote. Blank
/// lines are skipped.
fn parse_csv(text: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut field_started = false;
    let mut line = 1usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                quoted = true;
                field_started = true;
            }
            '"' => return Err(format!("line {line}: a quote inside an unquoted field")),
            ',' => {
                record.push(std::mem::take(&mut field));
                field_started = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                line += 1;
                if field_started || !field.is_empty() || !record.is_empty() {
                    record.push(std::mem::take(&mut field));
                    records.push((line - 1, std::mem::take(&mut record)));
                }
                field_started = false;
            }
            _ => {
                field.push(c);
                field_started = true;
            }
        }
    }
    if quoted {
        return Err(format!("line {line}: a quoted field is not closed"));
    }
    if field_started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push((line, record));
    }
    let mut records = records.into_iter();
    let (_, columns) = records.next().ok_or("no header row")?;
    check_columns(&columns)?;
    let rows = records
        .map(|(line, row)| {
            if row.len() == columns.len() {
                Ok(row)
            } else {
                Err(format!(
                    "line {line}: {} values for {} columns",
                    row.len(),
                    columns.len()
                ))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((columns, rows))
}

/// JSON Lines: one object per line, with the keys of the first line.
/// Strings are used as they are; numbers and booleans as JSON writes them.
fn parse_json_lines(text: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut columns: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("line {line_no}: {e}"))?;
        let serde_json::Value::Object(object) = value else {
            return Err(format!("line {line_no}: expected a JSON object"));
        };
        let columns = columns.get_or_insert_with(|| object.keys().cloned().collect());
        if object.len() != columns.len() || columns.iter().any(|c| !object.contains_key(c)) {
            return Err(format!(
                "line {line_no}: the keys differ from the first line's ({})",
                columns.join(", ")
            ));
        }
        let row = columns
            .iter()
            .map(|column| match &object[column] {
                serde_json::Value::String(s) => Ok(s.clone()),
                v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_)) => Ok(v.to_string()),
                serde_json::Value::Null => Ok(String::new()),
                _ => Err(format!(
                    "line {line_no}: '{column}' is an array or object; values must be strings, numbers or booleans"
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(row);
    }
    let columns = columns.ok_or("no rows")?;
    check_columns(&columns)?;
    Ok((columns, rows))
}

fn check_columns(columns: &[String]) -> Result<(), String> {
    for (n, column) in columns.iter().enumerate() {
        if column.trim().is_empty() {
            return Err(format!("column {} has no name", n + 1));
        }
        if columns[..n].contains(column) {
            return Err(format!("column '{column}' appears twice"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_with_quotes_commas_and_line_breaks() {
        let data = DataSet::parse(
            "users.csv",
            "user,note\r\nada,\"hi, there\"\r\n\"bob\",\"two\nlines\"\r\n\ncy,\"say \"\"hi\"\"\"",
        )
        .unwrap();
        assert_eq!(data.columns(), ["user", "note"]);
        assert_eq!(data.len(), 3);
        assert_eq!(data.value(0, "note"), Some("hi, there"));
        assert_eq!(data.value(1, "note"), Some("two\nlines"));
        assert_eq!(data.value(2, "note"), Some("say \"hi\""));
        assert_eq!(data.value(2, "user"), Some("cy"));
    }

    #[test]
    fn csv_errors_name_the_line() {
        let short = DataSet::parse("u.csv", "a,b\n1,2\n3\n").unwrap_err();
        assert!(
            short.contains("u.csv: line 3: 1 values for 2 columns"),
            "{short}"
        );
        let open = DataSet::parse("u.csv", "a\n\"x\n").unwrap_err();
        assert!(open.contains("not closed"), "{open}");
        let stray = DataSet::parse("u.csv", "a\nx\"y\n").unwrap_err();
        assert!(stray.contains("line 2"), "{stray}");
        assert!(
            DataSet::parse("u.csv", "a,b\n")
                .unwrap_err()
                .contains("no rows")
        );
        assert!(
            DataSet::parse("u.csv", "a,a\n1,2\n")
                .unwrap_err()
                .contains("twice")
        );
    }

    #[test]
    fn json_lines_take_the_keys_of_the_first_line() {
        let data = DataSet::parse(
            "users.jsonl",
            "{\"user\":\"ada\",\"id\":7,\"admin\":true}\n\n{\"admin\":false,\"user\":\"bob\",\"id\":8}\n",
        )
        .unwrap();
        assert_eq!(data.columns(), ["admin", "id", "user"]);
        assert_eq!(data.value(1, "user"), Some("bob"));
        assert_eq!(data.value(0, "id"), Some("7"));
        assert_eq!(data.value(1, "admin"), Some("false"));

        let differ = DataSet::parse("u.jsonl", "{\"a\":1}\n{\"b\":1}\n").unwrap_err();
        assert!(differ.contains("line 2: the keys differ"), "{differ}");
        let nested = DataSet::parse("u.jsonl", "{\"a\":[1]}\n").unwrap_err();
        assert!(nested.contains("array or object"), "{nested}");
        // Sniffed from the content, whatever the name.
        assert!(DataSet::parse("users.txt", "{\"a\":1}\n").is_ok());
    }

    #[test]
    fn agents_interleave_their_connections_over_the_rows() {
        let data = DataSet::parse("u.csv", "n\n0\n1\n2\n3\n4\n5\n").unwrap();
        let first = data.clone().with_share(0, 2);
        let second = data.clone().with_share(1, 2);
        let rows = |d: &DataSet| (0..3).map(|w| d.row_for(w)).collect::<Vec<_>>();
        assert_eq!(rows(&first), [0, 2, 4]);
        assert_eq!(rows(&second), [1, 3, 5]);
        assert_eq!(rows(&data), [0, 1, 2]);
        assert_eq!(data.row_for(7), 1);
    }
}
