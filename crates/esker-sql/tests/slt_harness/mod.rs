//! The `.slt` harness: statements and their answers, in files rather than in Rust.
//!
//! Shared by two test binaries — `tests/slt.rs` runs the corpus through this parser, and
//! `tests/sqllogictest.rs` runs the same files through the `sqllogictest` crate's. It lives in a
//! subdirectory so that cargo treats it as a module rather than as a third test target.
//!
//! Unit 7 of `docs/plans/phase-6a.md`. Every other test in this crate is written in Rust, which is
//! right for the ones that are about *how* something works — the encodings, the conflict
//! translation, the access paths. This one is about **what a statement answers**, and that is
//! better written as the statement and the answer next to each other, because a reader can check it
//! against a real server by pasting it into `psql`.
//!
//! ```text
//! # a comment
//! statement ok
//! CREATE TABLE t (a int8 PRIMARY KEY)
//!
//! # tag: INSERT 0 2
//! statement ok
//! INSERT INTO t VALUES (1), (2)
//!
//! statement error (23505)
//! INSERT INTO t VALUES (1)
//!
//! query I
//! SELECT a FROM t ORDER BY a
//! ----
//! 1
//! 2
//! ```
//!
//! # Two runners, one file set
//!
//! The syntax is the `sqllogictest` crate's, exactly, so that **both** runners read these files:
//! this one, and `tests/sqllogictest.rs`, which hands the same files to that crate. It is the same
//! argument as the `psql` smoke test — ours checks that the files mean what we think they mean,
//! theirs checks that they mean what the rest of the world thinks, and a disagreement between the
//! two is a fact worth learning rather than a duplicated effort.
//!
//! One thing the format has no word for, so it rides in a comment: `# tag:` on the line before a
//! statement names the **command tag** it must report. That is a real compatibility surface —
//! `psql` prints it and shell scripts branch on it — and `statement count 3` cannot tell an
//! `INSERT 0 3` from an `UPDATE 3`. The crate's parser sees a comment and skips it, so the files
//! stay standard and the assertion is not lost.
//!
//! A `query` line names one letter per output column — `I` integer, `T` text, `R` real, `B`
//! boolean, `X` bytea, `D` timestamptz — which is what fixes the column count. The letters are
//! checked against the types the *server reported*, so a query that silently changed shape fails
//! here rather than in whatever reads it next.
//!
//! `query <types> rowsort` sorts the rows before comparing, which is the format's way of saying
//! **the order is not part of the answer**. A `SELECT` with no `ORDER BY`, or one whose `ORDER BY`
//! leaves ties, has no order to promise — PostgreSQL does not guarantee one and neither does this
//! node. Writing such a result down as though it did would pin an accident, and the first change to
//! the scan or the sort would break a test that was never testing anything. The rows are sorted the
//! way the crate sorts them, column vector by column vector, so both runners agree on what
//! `rowsort` means.
//!
//! # The corpus was replayed against a real server
//!
//! Every directive in these files was put to a running PostgreSQL 19beta1 and compared with what it
//! answered — the same capture-first method the rest of the phase used, applied to the test corpus
//! itself, because a corpus that only records *our* behaviour proves nothing.
//!
//! It agrees, with exactly two kinds of exception, and both are marked in the files:
//!
//! * `unsupported.slt` in its entirety, which is contract C2 — every statement in it is one
//!   PostgreSQL runs and this node answers `0A000` for, by design;
//! * the lines marked `DIVERGES`, each with the reason beside it: a decimal literal in an integer
//!   column, `text` ordering by bytes rather than by a locale, and the six system columns a
//!   PostgreSQL table has and this one does not (`no_primary_key.slt`, where `ctid` is the one
//!   worth reading about).
//!
//! Rows are one per line with columns separated by a **tab**, and a NULL is the four characters
//! `NULL`.

// Two test binaries include this module and each uses a subset of it: `tests/slt.rs` drives the
// whole runner, `tests/sqllogictest.rs` borrows only the file list, the statement dispatch and the
// row rendering. Everything here is used by one of them.
#![allow(dead_code, reason = "shared by two test binaries; each uses a subset")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

/// Every `.slt` file, with its contents. Listed rather than globbed so that adding a file is a
/// visible change and a file cannot go missing without the build noticing.
pub(crate) const FILES: &[(&str, &str)] = &[
    ("access_paths.slt", include_str!("../slt/access_paths.slt")),
    ("aggregate.slt", include_str!("../slt/aggregate.slt")),
    ("alter_table.slt", include_str!("../slt/alter_table.slt")),
    ("create_table.slt", include_str!("../slt/create_table.slt")),
    (
        "empty_vs_null.slt",
        include_str!("../slt/empty_vs_null.slt"),
    ),
    ("index.slt", include_str!("../slt/index.slt")),
    ("insert.slt", include_str!("../slt/insert.slt")),
    ("join.slt", include_str!("../slt/join.slt")),
    ("limits.slt", include_str!("../slt/limits.slt")),
    (
        "no_primary_key.slt",
        include_str!("../slt/no_primary_key.slt"),
    ),
    ("nulls.slt", include_str!("../slt/nulls.slt")),
    ("ordering.slt", include_str!("../slt/ordering.slt")),
    (
        "rowsort_check.slt",
        include_str!("../slt/rowsort_check.slt"),
    ),
    ("select.slt", include_str!("../slt/select.slt")),
    ("time_machine.slt", include_str!("../slt/time_machine.slt")),
    (
        "update_delete.slt",
        include_str!("../slt/update_delete.slt"),
    ),
    ("transactions.slt", include_str!("../slt/transactions.slt")),
    ("types.slt", include_str!("../slt/types.slt")),
    ("unsupported.slt", include_str!("../slt/unsupported.slt")),
];

/// One file, in its own node: a file is a self-contained story and must not depend on another
/// having run first.
pub(crate) fn run_file(name: &str, body: &str) -> usize {
    run_file_on(name, body, Arc::new(MemoryBackend::new()))
}

/// The same, over a backend the caller chose — which is how the corpus is replayed against a real
/// cluster (`tests/real_corpus.rs`) rather than only against the fake.
///
/// The whole corpus, unchanged, is the point: a file that had to be written differently for the
/// real store would be testing the file rather than the store.
pub(crate) fn run_file_on(name: &str, body: &str, backend: Arc<dyn Backend>) -> usize {
    let mut executor = Executor::new(backend, Arc::new(Catalog::new()), 1);
    let mut directives = 0;

    for directive in parse_file(name, body) {
        directives += 1;
        let at = format!("{name}:{}", directive.line);
        let sql = &directive.sql;
        match directive.kind {
            Kind::Ok { tag } => {
                let outcome = run(&mut executor, sql)
                    .unwrap_or_else(|error| panic!("{at}: {sql} -> {error}"));
                if let Some(expected) = tag {
                    assert_eq!(command_tag(&outcome), expected, "{at}: wrong command tag");
                }
            }
            Kind::Count(expected) => {
                let outcome = run(&mut executor, sql)
                    .unwrap_or_else(|error| panic!("{at}: {sql} -> {error}"));
                assert_eq!(
                    rows_touched(&outcome),
                    Some(expected),
                    "{at}: {sql} reported `{}`",
                    command_tag(&outcome)
                );
            }
            Kind::Error { sqlstate } => {
                let error = match run(&mut executor, sql) {
                    Err(error) => error,
                    Ok(outcome) => panic!("{at}: {sql} succeeded with {outcome:?}"),
                };
                assert_eq!(error.sqlstate(), sqlstate, "{at}: {sql} -> `{error}`");
            }
            Kind::Query {
                types,
                sorted,
                expected,
            } => {
                let outcome = run(&mut executor, sql)
                    .unwrap_or_else(|error| panic!("{at}: {sql} -> {error}"));
                let Outcome::Rows { fields, rows, .. } = outcome else {
                    panic!("{at}: {sql} returned no rows at all")
                };
                assert_eq!(
                    fields.len(),
                    types.len(),
                    "{at}: {sql} returned {} columns, not {}",
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
                // Sorted as column vectors, not as rendered lines, because that is what the
                // `sqllogictest` crate does and the two runners must mean the same thing by it.
                let mut columns: Vec<Vec<String>> =
                    rows.iter().map(|row| render_row(row)).collect();
                if sorted {
                    columns.sort_unstable();
                }
                let actual: Vec<String> = columns.iter().map(|row| row.join("\t")).collect();
                assert_eq!(actual, expected, "{at}: {sql}");
            }
        }
    }
    directives
}

/// Runs a statement the way the session does — transaction control is the session's, not the
/// executor's, so a file that says `BEGIN` reaches the same place a client's `BEGIN` does.
pub(crate) fn run(executor: &mut Executor, sql: &str) -> esker_sql::Result<Outcome> {
    let mut last = Outcome::done("");
    for parsed in parse_statements(sql)? {
        last = match parsed.class() {
            StatementClass::Begin => executor
                .begin(parsed.begins_read_only())
                .map(|()| Outcome::done("BEGIN"))?,
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

/// The count a command tag carries, which is what `statement count` is about. `INSERT` puts a
/// legacy OID first, so the count is the last field either way.
fn rows_touched(outcome: &Outcome) -> Option<usize> {
    command_tag(outcome)
        .rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
}

/// One row as its columns, `NULL` for a NULL.
pub(crate) fn render_row(row: &[Option<Vec<u8>>]) -> Vec<String> {
    row.iter()
        .map(|value| {
            value.as_ref().map_or_else(
                || "NULL".to_owned(),
                |bytes| String::from_utf8_lossy(bytes).into_owned(),
            )
        })
        .collect()
}

/// The letter a `query` line uses for a type OID.
pub(crate) fn type_letter(oid: u32) -> char {
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
        /// From a `# tag:` comment, when the file names one.
        tag: Option<String>,
    },
    /// `statement count N`: how many rows the statement reports having touched.
    Count(usize),
    Error {
        sqlstate: String,
    },
    Query {
        types: String,
        /// `rowsort`: the order is not part of the answer, so compare the rows as a set.
        sorted: bool,
        expected: Vec<String>,
    },
}

/// How many directives a file holds, for the cross-check in `tests/sqllogictest.rs`.
pub(crate) fn directive_count(body: &str) -> usize {
    parse_file("<cross-check>", body).len()
}

fn parse_file(name: &str, body: &str) -> Vec<Directive> {
    let lines: Vec<&str> = body.lines().collect();
    let mut directives = Vec::new();
    let mut tag: Option<String> = None;
    let mut at = 0;

    while at < lines.len() {
        let line = lines[at].trim_end();
        if line.trim().is_empty() {
            at += 1;
            continue;
        }
        // `# tag:` is a comment to the crate's parser and a directive to this one. It applies to
        // the next statement and to nothing else, so a stray one cannot drift onto a later line.
        if let Some(named) = line.trim_start().strip_prefix("# tag:") {
            tag = Some(named.trim().to_owned());
            at += 1;
            continue;
        }
        if line.trim_start().starts_with('#') {
            at += 1;
            continue;
        }

        let header = line;
        let start = at + 1;
        let tag = tag.take();
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

        let kind = if header.trim() == "statement ok" {
            Kind::Ok { tag }
        } else if let Some(count) = header.strip_prefix("statement count ") {
            Kind::Count(
                count
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("{name}:{start}: `{count}` is not a count")),
            )
        } else if let Some(rest) = header.strip_prefix("statement error ") {
            // The crate's SQLSTATE form, and only that form, so a file cannot drift into a syntax
            // this runner understands and the crate's does not.
            let sqlstate = rest
                .trim()
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or_else(|| {
                    panic!("{name}:{start}: expected `statement error (SQLSTATE)`, got `{header}`")
                });
            assert_eq!(
                sqlstate.len(),
                5,
                "{name}:{start}: `{sqlstate}` is not a SQLSTATE"
            );
            Kind::Error {
                sqlstate: sqlstate.to_owned(),
            }
        } else if let Some(rest) = header.strip_prefix("query ") {
            // `query I rowsort`: the types, then the sort mode the format defines.
            let (types, sorted) = match rest.trim().split_once(char::is_whitespace) {
                Some((types, mode)) => {
                    assert_eq!(
                        mode.trim(),
                        "rowsort",
                        "{name}:{start}: `{}` is not a sort mode this corpus uses",
                        mode.trim()
                    );
                    (types, true)
                }
                None => (rest.trim(), false),
            };
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
                types: types.to_owned(),
                sorted,
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
