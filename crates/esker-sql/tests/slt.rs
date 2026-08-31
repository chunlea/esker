//! The `.slt` harness: statements and their answers, in files rather than in Rust.
//!
//! Unit 7 of `docs/plans/phase-6a.md`. Every other test in this crate is written in Rust, which is
//! right for the ones that are about *how* something works — the encodings, the conflict
//! translation, the access paths. This one is about **what a statement answers**, and that is
//! better written as the statement and the answer next to each other, because a reader can then
//! check it against a real server by pasting it into `psql`.
//!
//! ```text
//! # a comment
//! statement ok
//! CREATE TABLE t (a int8 PRIMARY KEY)
//!
//! statement ok INSERT 0 2
//! INSERT INTO t VALUES (1), (2)
//!
//! statement error 23505
//! INSERT INTO t VALUES (1)
//!
//! query I
//! SELECT a FROM t ORDER BY a
//! ----
//! 1
//! 2
//! ```
//!
//! `statement ok` may name the command tag it expects; `statement error` takes a SQLSTATE, and may
//! be followed by a `:` and a substring the message must contain. A `query` line names one letter
//! per output column — `I` integer, `T` text, `R` real, `B` boolean — which is what fixes the
//! column count; the letters are checked against the types the server reported, so a query that
//! silently changed shape fails here rather than in whatever reads it next.
//!
//! # The corpus was replayed against a real server
//!
//! Every directive in these files was put to a running PostgreSQL 19beta1 and compared with what
//! it answered — the same capture-first method the rest of the phase used, applied to the test
//! corpus itself, because a corpus that only records *our* behaviour proves nothing.
//!
//! It agrees, with exactly two kinds of exception, and both are marked in the files:
//!
//! * `unsupported.slt` in its entirety, which is contract C2 — every statement in it is one
//!   PostgreSQL runs and this node answers `0A000` for, by design;
//! * three lines marked `DIVERGES`, each with the reason beside it: a table with no primary key,
//!   a decimal literal in an integer column, and `text` ordering by bytes rather than by a locale.
//!
//! Rows are one per line with columns separated by a **tab**, and a NULL is the four characters
//! `NULL`. That is not quite the `sqllogictest` crate's own dialect, which separates on runs of
//! spaces and so cannot express a value with a space in it; the difference is a deliberate one and
//! a conversion is a `sed` away if these files are ever handed to that crate for acceptance
//! (`docs/plans/phase-6a.md` §7.7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

/// Every `.slt` file, with its contents. Listed rather than globbed so that adding a file is a
/// visible change and a file cannot go missing without the build noticing.
const FILES: &[(&str, &str)] = &[
    ("create_table.slt", include_str!("slt/create_table.slt")),
    ("index.slt", include_str!("slt/index.slt")),
    ("insert.slt", include_str!("slt/insert.slt")),
    ("select.slt", include_str!("slt/select.slt")),
    ("update_delete.slt", include_str!("slt/update_delete.slt")),
    ("transactions.slt", include_str!("slt/transactions.slt")),
    ("types.slt", include_str!("slt/types.slt")),
    ("unsupported.slt", include_str!("slt/unsupported.slt")),
];

#[test]
fn every_slt_file_passes() {
    let mut checked = 0;
    for (name, body) in FILES {
        checked += run_file(name, body);
    }
    assert!(
        checked > 100,
        "only {checked} directives ran; the files did not load"
    );
}

/// One file, in its own node: a file is a self-contained story and must not depend on another
/// having run first.
fn run_file(name: &str, body: &str) -> usize {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let mut executor = Executor::new(backend, Arc::new(Catalog::new()), 1);
    let mut directives = 0;

    for directive in parse_file(name, body) {
        directives += 1;
        let at = format!("{name}:{}", directive.line);
        match directive.kind {
            Kind::Ok { tag } => {
                let outcome = run(&mut executor, &directive.sql)
                    .unwrap_or_else(|error| panic!("{at}: {} -> {error}", directive.sql));
                if let Some(expected) = tag {
                    assert_eq!(command_tag(&outcome), expected, "{at}: wrong command tag");
                }
            }
            Kind::Error { sqlstate, message } => {
                let error = match run(&mut executor, &directive.sql) {
                    Err(error) => error,
                    Ok(outcome) => {
                        panic!("{at}: {} succeeded with {outcome:?}", directive.sql)
                    }
                };
                assert_eq!(
                    error.sqlstate(),
                    sqlstate,
                    "{at}: {} -> `{error}`",
                    directive.sql
                );
                if let Some(message) = message {
                    assert!(
                        error.to_string().contains(&message),
                        "{at}: `{error}` does not contain `{message}`"
                    );
                }
            }
            Kind::Query { types, expected } => {
                let outcome = run(&mut executor, &directive.sql)
                    .unwrap_or_else(|error| panic!("{at}: {} -> {error}", directive.sql));
                let Outcome::Rows { fields, rows, .. } = outcome else {
                    panic!("{at}: {} returned no rows at all", directive.sql)
                };
                assert_eq!(
                    fields.len(),
                    types.len(),
                    "{at}: {} returned {} columns, not {}",
                    directive.sql,
                    fields.len(),
                    types.len()
                );
                for (field, letter) in fields.iter().zip(types.chars()) {
                    assert_eq!(
                        type_letter(field.type_oid),
                        letter,
                        "{at}: column {} is not `{letter}`",
                        field.name
                    );
                }
                let actual: Vec<String> = rows.iter().map(|row| render(row)).collect();
                assert_eq!(actual, expected, "{at}: {}", directive.sql);
            }
        }
    }
    directives
}

/// Runs a statement the way the session does — transaction control is the session's, not the
/// executor's, so a file that says `BEGIN` has to reach the same place a client's `BEGIN` does.
fn run(executor: &mut Executor, sql: &str) -> esker_sql::Result<Outcome> {
    let mut last = Outcome::done("");
    for parsed in parse_statements(sql)? {
        last = match parsed.class() {
            StatementClass::Begin => executor.begin().map(|()| Outcome::done("BEGIN"))?,
            StatementClass::Commit => executor.commit().map(|()| Outcome::done("COMMIT"))?,
            StatementClass::Rollback => executor.rollback().map(|()| Outcome::done("ROLLBACK"))?,
            _ => executor.execute(&parsed, &Params::NONE)?,
        };
    }
    Ok(last)
}

fn command_tag(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Rows { tag, .. } | Outcome::Done { tag } => tag,
    }
}

/// One row, tab-separated, `NULL` for a NULL.
fn render(row: &[Option<Vec<u8>>]) -> String {
    row.iter()
        .map(|value| {
            value.as_ref().map_or_else(
                || "NULL".to_owned(),
                |bytes| String::from_utf8_lossy(bytes).into_owned(),
            )
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// The letter a `query` line uses for a type OID.
fn type_letter(oid: u32) -> char {
    match oid {
        20 => 'I',
        25 => 'T',
        16 => 'B',
        701 => 'R',
        17 => 'X',
        1184 => 'D',
        _ => '?',
    }
}

struct Directive {
    line: usize,
    sql: String,
    kind: Kind,
}

enum Kind {
    Ok {
        tag: Option<String>,
    },
    Error {
        sqlstate: String,
        message: Option<String>,
    },
    Query {
        types: String,
        expected: Vec<String>,
    },
}

fn parse_file(name: &str, body: &str) -> Vec<Directive> {
    let lines: Vec<&str> = body.lines().collect();
    let mut directives = Vec::new();
    let mut at = 0;

    while at < lines.len() {
        let line = lines[at].trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            at += 1;
            continue;
        }

        let header = line;
        let start = at + 1;
        at += 1;
        // The statement runs until a blank line, a `----`, or the end of the file.
        let mut sql = Vec::new();
        while at < lines.len() && !lines[at].trim().is_empty() && lines[at].trim() != "----" {
            sql.push(lines[at]);
            at += 1;
        }
        let sql = sql.join("\n");
        assert!(
            !sql.is_empty(),
            "{name}:{start}: `{header}` has no statement"
        );

        let kind = if let Some(rest) = header.strip_prefix("statement ok") {
            Kind::Ok {
                tag: Some(rest.trim())
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_owned),
            }
        } else if let Some(rest) = header.strip_prefix("statement error") {
            let rest = rest.trim();
            let (sqlstate, message) = match rest.split_once(':') {
                Some((code, message)) => (code.trim(), Some(message.trim().to_owned())),
                None => (rest, None),
            };
            assert!(
                sqlstate.len() == 5,
                "{name}:{start}: `{sqlstate}` is not a SQLSTATE"
            );
            Kind::Error {
                sqlstate: sqlstate.to_owned(),
                message,
            }
        } else if let Some(types) = header.strip_prefix("query ") {
            assert_eq!(
                lines.get(at).map(|line| line.trim()),
                Some("----"),
                "{name}:{start}: a query needs a `----` before its rows"
            );
            at += 1;
            let mut expected = Vec::new();
            while at < lines.len() && !lines[at].trim().is_empty() {
                expected.push(lines[at].to_owned());
                at += 1;
            }
            Kind::Query {
                types: types.trim().to_owned(),
                expected,
            }
        } else {
            panic!("{name}:{start}: `{header}` is not a directive");
        };

        directives.push(Directive {
            line: start,
            sql,
            kind,
        });
    }
    directives
}
