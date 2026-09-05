//! Replaying a **bind** corpus: four columns, because three have nowhere to put the values.
//!
//! `parity_harness` drives the simple query protocol, where nothing is ever bound. This one drives
//! `Parse`/`Bind`/`Execute` — the path `ActiveRecord` uses with `prepared_statements: true`, which
//! this harness's `config.yml` defaults to — so it can send the parameters a statement references.
//!
//! ```text
//! sql <tab> [json params] <tab> types <tab> rows | !SQLSTATE message
//! ```
//!
//! A parameter arrives on the wire as **text or nothing**: JSON `null` is SQL NULL (a length of
//! -1), and every other value is its JSON rendering as bytes. That is not a simplification, it is
//! what a `Bind` carries — the wire has no integers, and the declared type of the *position* is
//! what decides how the bytes are read.

#![allow(dead_code)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::value::PgType as _;

/// What one statement answered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Rows {
        types: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Done,
    Refused(String),
}

impl std::fmt::Display for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Refused(message) => write!(f, "!{message}"),
            Answer::Done => write!(f, "(a command, no result set)"),
            Answer::Rows { types, rows } => write!(
                f,
                "{}\t{}",
                types.join(","),
                if rows.is_empty() {
                    "-".to_owned()
                } else {
                    rows.iter()
                        .map(|row| row.join("|"))
                        .collect::<Vec<_>>()
                        .join(" ; ")
                }
            ),
        }
    }
}

/// What a corpus is allowed to disagree about, and why.
///
/// Held from **both** sides, the way `parity_harness`'s are: an unlisted divergence fails the
/// test and so does a listed one that has started agreeing. Closing a gap cannot be absorbed
/// silently, and neither can opening one.
#[derive(Default)]
pub(crate) struct Divergences {
    /// Statements whose rows agree and whose declared types do not.
    pub(crate) types: &'static [&'static str],
    /// Statements answered differently, each with the reason.
    pub(crate) answers: &'static [(&'static str, &'static str)],
}

/// A node the corpus is replayed against.
pub(crate) struct Node {
    pub(crate) executor: Executor,
}

impl Node {
    pub(crate) fn new() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        Node {
            executor: Executor::new(
                backend,
                Arc::new(Catalog::new()),
                1,
                esker_sql::session::register(),
            ),
        }
    }

    /// One statement with its values, through `Parse`/`Bind`/`Execute`.
    pub(crate) fn bound(
        &mut self,
        sql: &str,
        values: &[Option<Vec<u8>>],
    ) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(
                &parsed,
                &Params {
                    values,
                    formats: &[],
                    declared: &[],
                    bound: true,
                },
            )?;
        }
        Ok(last)
    }

    /// The same, in the corpus's own shape.
    pub(crate) fn answer(&mut self, sql: &str, values: &[Option<Vec<u8>>]) -> Answer {
        use std::fmt::Write as _;
        match self.bound(sql, values) {
            Err(error) => {
                let mut message = format!("{} {error}", error.sqlstate());
                if let Some(detail) = error.detail() {
                    let _ = write!(message, " DETAIL: {detail}");
                }
                if let Some(hint) = error.hint() {
                    let _ = write!(message, " HINT: {hint}");
                }
                Answer::Refused(message)
            }
            Ok(Outcome::Done { .. }) => Answer::Done,
            Ok(Outcome::Rows { fields, rows, .. }) => Answer::Rows {
                types: fields
                    .iter()
                    .map(|field| type_name(field.type_oid, field.type_modifier))
                    .collect(),
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|value| {
                                value.map_or_else(
                                    || "\\N".to_owned(),
                                    |bytes| String::from_utf8(bytes).unwrap(),
                                )
                            })
                            .collect()
                    })
                    .collect(),
            },
        }
    }
}

/// One value as a `Bind` carries it: text bytes, or nothing for NULL.
///
/// The JSON is only a transport for the capture file. A number, a string and a boolean all arrive
/// as **the characters they print as**, because that is all a text-format `Bind` has.
fn bind_value(json: &str) -> Option<Vec<u8>> {
    let json = json.trim();
    if json == "null" {
        return None;
    }
    let text = json
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map_or_else(|| json.to_owned(), |inner| inner.replace("\\\"", "\""));
    Some(text.into_bytes())
}

/// The values of one corpus line: a JSON array, split at the commas that are not inside a string.
fn bind_values(field: &str) -> Vec<Option<Vec<u8>>> {
    let inner = field
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or("")
        .trim();
    if inner.is_empty() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let mut depth = 0;
    let mut quoted = false;
    let mut current = String::new();
    for c in inner.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '[' | '{' if !quoted => {
                depth += 1;
                current.push(c);
            }
            ']' | '}' if !quoted => {
                depth -= 1;
                current.push(c);
            }
            ',' if !quoted && depth == 0 => {
                items.push(bind_value(&current));
                current.clear();
            }
            _ => current.push(c),
        }
    }
    items.push(bind_value(&current));
    items
}

/// One line of a bind corpus: where it is, what it says, what it binds, what it answered.
struct Probe {
    line: usize,
    sql: String,
    values: Vec<Option<Vec<u8>>>,
    expected: Answer,
}

/// One corpus file, as probes.
fn parse(corpus: &str) -> Vec<Probe> {
    corpus
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let sql = fields.next().unwrap_or_default().to_owned();
            let values = bind_values(fields.next().unwrap_or("[]"));
            let types = fields.next().unwrap_or("");
            let rows = fields.next().unwrap_or("-");
            let answer = match rows.strip_prefix('!') {
                Some(message) => Answer::Refused(message.to_owned()),
                None if rows == "-" && types.is_empty() => Answer::Done,
                None => Answer::Rows {
                    types: if types.is_empty() {
                        Vec::new()
                    } else {
                        types.split(',').map(str::to_owned).collect()
                    },
                    rows: if rows == "-" {
                        Vec::new()
                    } else {
                        rows.split(" ; ")
                            .map(|row| row.split('|').map(str::to_owned).collect())
                            .collect()
                    },
                },
            };
            Probe {
                line: index + 1,
                sql,
                values,
                expected: answer,
            }
        })
        .collect()
}

/// Replays a bind corpus and asserts every answer, with the declared divergences held both ways.
pub(crate) fn replay(corpus: &str, divergences: &Divergences) -> usize {
    let mut node = Node::new();
    let mut mismatched = Vec::new();
    let mut type_mismatched = Vec::new();
    let mut agreed_after_all = Vec::new();
    let mut checked = 0;
    // **One entry, however many times the statement appears** — `parity_harness` says why. An
    // entry is stale only when *every* occurrence agreed. `(entry, line, statement)`.
    let mut listed_agreements: Vec<(usize, usize, String)> = Vec::new();
    let mut listed_seen: Vec<usize> = Vec::new();

    for Probe {
        line: line_number,
        sql,
        values,
        expected,
    } in parse(corpus)
    {
        checked += 1;
        let listed = divergences
            .answers
            .iter()
            .position(|(statement, _)| *statement == sql);
        // **Run it even when it is listed.** A replay is stateful, so a statement skipped here is
        // a row this node never wrote and every line after it sees a different table than
        // PostgreSQL did. A corpus of `SELECT`s hid that; one whose divergences are `DELETE`s
        // would have compared its counts against a node that never deleted anything.
        let actual = node.answer(&sql, &values);
        if let Some(entry) = listed {
            listed_seen.push(entry);
            if actual == expected {
                listed_agreements.push((entry, line_number, sql.clone()));
            }
            continue;
        }
        match (&expected, &actual) {
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
                // **An undeclared types column pins nothing**, the rule `parity_harness` has had
                // since it was written and this one was missing: a bind corpus's `\gdesc` pass
                // runs in a second session, so a statement whose fixture the first session rolled
                // back records *no* types — and asserting against nothing is asserting that this
                // node declares nothing. The rows are the whole claim there.
            ) if rows == theirs && types.is_empty() && types != ours => {}
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
            ) if rows == theirs && types != ours => {
                if !divergences.types.contains(&sql.as_str()) {
                    type_mismatched.push(format!(
                        "line {line_number}: {sql}\n  PostgreSQL: {types:?}\n  Esker:      {ours:?}"
                    ));
                }
            }
            _ if actual == expected => {
                if divergences.types.contains(&sql.as_str()) {
                    agreed_after_all.push(format!("line {line_number}: {sql}"));
                }
            }
            _ => mismatched.push(format!(
                "line {line_number}: {sql}  {values:?}\n  PostgreSQL: {expected}\n  Esker:      \
                 {actual}"
            )),
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} of {checked} statements disagree with PostgreSQL 19 and are not listed as \
         divergences:\n\n{}",
        mismatched.len(),
        mismatched.join("\n\n")
    );
    assert!(
        type_mismatched.is_empty(),
        "{} statements have the right rows and an unlisted type divergence:\n\n{}",
        type_mismatched.len(),
        type_mismatched.join("\n\n")
    );
    // Rule 2, decided per entry: an entry is stale only when **every** occurrence of its
    // statement agreed. One that still covers a second occurrence stays.
    for (at, (sql, _)) in divergences.answers.iter().enumerate() {
        let occurrences = listed_seen.iter().filter(|&&seen| seen == at).count();
        let agreements = listed_agreements
            .iter()
            .filter(|(entry, ..)| *entry == at)
            .count();
        if occurrences > 0 && agreements == occurrences {
            let line = listed_agreements
                .iter()
                .find(|(entry, ..)| *entry == at)
                .map_or(0, |(_, line, _)| *line);
            agreed_after_all.push(format!("line {line}: {sql}"));
        }
    }
    assert!(
        agreed_after_all.is_empty(),
        "{} statements are listed as divergences and now agree -- delete the entries:\n\n{}",
        agreed_after_all.len(),
        agreed_after_all.join("\n")
    );
    checked
}

fn type_name(oid: u32, typmod: i32) -> String {
    esker_sql::value::ColumnType::ALL
        .into_iter()
        .find(|ty| ty.oid() == oid)
        .map_or_else(
            || "?".to_owned(),
            |ty| esker_sql::value::format_type(ty, typmod),
        )
}
