//! The same `.slt` files, run by somebody else's runner.
//!
//! `tests/slt.rs` is our own harness over `tests/slt/*.slt`. This hands the identical files to the
//! `sqllogictest` crate — the dev-only dependency `CLAUDE.md`'s allowlist reserves for phase 6 —
//! and it is here for exactly the reason the `psql` smoke test is here: **the other end was written
//! by someone else.**
//!
//! A harness and the files it reads are written together, and a harness that quietly means
//! something slightly different by `statement error` or by a `----` block will agree with its own
//! files forever. The only way to find that out is to give the files to a reader that never saw
//! them. If the two runners ever disagree, one of them is wrong about what the corpus says, and
//! that is worth knowing before the corpus is used as evidence about anything else.
//!
//! It also fixes the syntax. Ours accepts what the crate accepts and nothing more — `statement
//! error (23505)`, not a spelling of our own — so the files stay portable, and the acceptance step
//! `docs/plans/phase-6a.md` §7.7 describes is this file rather than a future conversion.
//!
//! # What this runner sees that ours does not
//!
//! `rowsort`, `halt`, `skipif`/`onlyif`, retries, multiline expected errors, and its own view of
//! how a result should be normalised before comparison. None of those are used by the corpus yet;
//! they are available the moment a file wants one, and that is the point of not writing a second
//! dialect.
//!
//! # What ours sees that this does not
//!
//! The **command tag**. `INSERT 0 3` and `UPDATE 3` are different answers to a client and the
//! format has one word — `statement count 3` — for both. The corpus carries the tag in a `# tag:`
//! comment, which this runner skips and ours reads.
//!
//! # The one thing that had to be configured, and what it taught
//!
//! The two runners disagreed the first time this file ran, and the disagreement was about the
//! *format* rather than about the server — which is the more useful kind to find.
//!
//! The crate's default validator normalises a result by splitting on whitespace and rejoining with
//! single spaces. That cannot represent an **empty string**: a row of `9223372036854775807`, `''`,
//! `f` collapses to two visible columns and a run of spaces, and no expected line can be written
//! that means "the second column is empty". `types.slt` stores an empty `text` and a `' padded '`
//! deliberately, because an empty string that is not a NULL is precisely the distinction the row
//! encoding is built around.
//!
//! So this runner is given a validator that joins columns with a tab and compares verbatim — the
//! same rule our own harness uses, and the crate's own extension point for exactly this. The
//! limitation is the whitespace dialect's, not the corpus's, and pinning it here is better than
//! quietly dropping the values that expose it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::session::Outcome;
use sqllogictest::{ColumnType, DBOutput, Normalizer, Runner};

#[path = "slt_harness/mod.rs"]
mod harness;

/// One column type, named by the letter the corpus uses for it.
///
/// The crate's own `DefaultColumnType` has `T`, `I`, `R` and a catch-all. Ours has one letter per
/// type this node stores, because that is the check worth having: a `SELECT` whose column silently
/// became `text` still renders the same characters, and only the type says so.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Letter(char);

impl ColumnType for Letter {
    fn from_char(value: char) -> Option<Self> {
        matches!(value, 'I' | 'T' | 'B' | 'R' | 'X' | 'D' | '?').then_some(Letter(value))
    }

    fn to_char(&self) -> char {
        self.0
    }
}

/// One node, one connection. The store is per-connection because each file is a self-contained
/// story, which is the same rule our own harness follows.
struct Node {
    executor: Executor,
}

impl Node {
    fn new() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        Node {
            executor: Executor::new(backend, Arc::new(Catalog::new()), 1),
        }
    }
}

impl sqllogictest::DB for Node {
    type Error = esker_sql::SqlError;
    type ColumnType = Letter;

    fn run(&mut self, sql: &str) -> Result<DBOutput<Letter>, Self::Error> {
        Ok(match harness::run(&mut self.executor, sql)? {
            Outcome::Rows { fields, rows, .. } => DBOutput::Rows {
                types: fields
                    .iter()
                    .map(|field| Letter(harness::type_letter(field.type_oid)))
                    .collect(),
                // Both runners render a row through the same function, so a NULL and an empty
                // string mean the same thing to both and a disagreement can only be about the
                // server.
                rows: rows.iter().map(|row| harness::render_row(row)).collect(),
            },
            Outcome::Done { tag } => DBOutput::StatementComplete(rows_touched(&tag)),
        })
    }

    /// What makes `statement error (23505)` work: the crate asks the driver for the code rather
    /// than matching a message, which is the assertion worth having anyway — a message is prose and
    /// a SQLSTATE is what a client branches on.
    fn error_sql_state(error: &Self::Error) -> Option<String> {
        Some(error.sqlstate().to_owned())
    }

    // The trait's signature ties the lifetime to `&self`; ours is a literal.
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "the signature is the trait's, not ours"
    )]
    fn engine_name(&self) -> &str {
        "esker"
    }
}

/// The count a command tag carries. `INSERT` puts a legacy OID first, so it is the last field.
fn rows_touched(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

/// Compares a result against a `----` block by joining columns with a tab, verbatim.
///
/// The crate's default splits on whitespace, which cannot express an empty column or a value with
/// a space in it; both are in the corpus on purpose. See the module note.
fn tab_separated(_normalizer: Normalizer, actual: &[Vec<String>], expected: &[String]) -> bool {
    let rendered: Vec<String> = actual.iter().map(|row| row.join("\t")).collect();
    rendered == expected
}

#[test]
fn the_sqllogictest_crate_reads_the_same_corpus() {
    for (name, body) in harness::FILES {
        let mut runner = Runner::new(|| async { Ok(Node::new()) });
        runner.with_validator(tab_separated);
        runner
            .run_script_with_name(body, *name)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

/// The two runners must agree about how many directives are in the corpus, or one of them is
/// skipping something the other is checking — which is the failure this file exists to catch, and
/// the one that would otherwise look like a pass.
#[test]
fn both_runners_see_the_same_number_of_records() {
    for (name, body) in harness::FILES {
        let theirs = sqllogictest::parser::parse::<Letter>(body)
            .unwrap_or_else(|error| panic!("{name}: the crate could not parse it: {error}"))
            .into_iter()
            .filter(|record| {
                matches!(
                    record,
                    sqllogictest::Record::Statement { .. } | sqllogictest::Record::Query { .. }
                )
            })
            .count();
        let ours = harness::directive_count(body);
        assert_eq!(
            theirs, ours,
            "{name}: the crate sees {theirs} records and our harness sees {ours}"
        );
    }
}
