//! Replaying a captured corpus against this node, and holding the divergences from both sides.
//!
//! Three corpora use this and there will be more: `pg19_aggregate.txt`, `pg19_returning.txt` and
//! `pg19_sequence.txt`, each a file of statements put to a real PostgreSQL 19beta1 with what came
//! back beside them. The shape is one line per statement:
//!
//! ```text
//! statement <tab> types <tab> rows      a result set: `\gdesc`'s types, then the rows
//! statement <tab> <tab> -               a command that returned no result set, only a tag
//! statement <tab> !SQLSTATE message     a refusal; DETAIL and HINT follow the message
//! ```
//!
//! In the rows field ` ; ` separates rows, `|` separates columns, `\N` is SQL NULL, and a lone `-`
//! is no rows at all — which is a different answer from one row of NULL and is what the empty-input
//! rules are about.
//!
//! # The replay is stateful, and that is the point
//!
//! Statements run **in file order against one node**, so an `INSERT` in a corpus is a row the next
//! line sees and a `CREATE TABLE` is a table the rest of the file uses. A harness that ran each
//! line independently would be a table of assertions with extra steps, and would miss every rule
//! that is about what a *previous* statement left behind — which is most of what a sequence is.
//!
//! # Divergences are held from both sides
//!
//! [`Divergences`] carries two lists. `types` is where the **rows agree** and the declared type
//! does not; `answers` is where the answer itself differs. Both are checked in both directions: an
//! unlisted divergence fails the test, and so does a listed one that has started agreeing. Closing
//! a gap cannot be absorbed silently, and neither can opening one.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::value::PgType;

/// What one statement answered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// A result set: the declared types, then the rows.
    Rows {
        types: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A command with no result set. The tag is not compared — `psql` prints a result set where it
    /// would have printed one, so the container cannot be asked what tag it sent, and a corpus
    /// that pretended otherwise would be asserting a recollection.
    Done,
    /// A refusal: `SQLSTATE message`, with ` DETAIL: …` and ` HINT: …` when the server sent them.
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

/// The three statements a failed block still accepts, mirroring `crate::pgwire::session`.
fn allowed_in_a_failed_block(class: &StatementClass) -> bool {
    matches!(
        class,
        StatementClass::Commit | StatementClass::Rollback | StatementClass::RollbackTo(_)
    )
}

/// What a corpus is allowed to disagree about, and why.
#[derive(Default)]
pub(crate) struct Divergences {
    /// Statements whose rows agree and whose declared types do not.
    pub(crate) types: &'static [&'static str],
    /// Statements answered differently, each with the reason.
    pub(crate) answers: &'static [(&'static str, &'static str)],
}

/// A node with a fixture loaded, which is what a corpus was captured over.
pub(crate) struct Node {
    /// Public to the tests that need the extended protocol rather than the simple one — a
    /// `Describe` has no place in a corpus, because `psql` never sends one.
    pub(crate) executor: Executor,
    /// Whether an explicit block is open, and whether it has failed. The session's state, mirrored
    /// (see [`Node::run`]).
    in_block: bool,
    failed: bool,
}

impl Node {
    pub(crate) fn new(fixture: &[&str]) -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Node {
            executor: Executor::new(backend, Arc::new(Catalog::new()), 1),
            in_block: false,
            failed: false,
        };
        for statement in fixture {
            node.run(statement)
                .unwrap_or_else(|error| panic!("the fixture did not load: {statement}\n{error}"));
        }
        node
    }

    /// One statement, through the same dispatch a client's would take.
    ///
    /// Transaction control does not go through `execute` — the session handles it, because it is
    /// what moves the status a client sees — so this mirrors `crate::pgwire::session`'s dispatch,
    /// the way `tests/slt_harness` already does. It mirrors the **aborted-block rule** too, which
    /// is the one thing here that is a rule rather than a call: after an error every statement is
    /// `25P02` until the block ends, except the three that are allowed through.
    ///
    /// That the real session agrees is not assumed: `tests/savepoint.rs` drives `Session` itself
    /// and reads the status out of its `ReadyForQuery`.
    pub(crate) fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            let class = parsed.class().clone();
            if self.failed && !allowed_in_a_failed_block(&class) {
                return Err(esker_sql::SqlError::InFailedTransaction);
            }
            let outcome = match &class {
                StatementClass::Begin => {
                    self.in_block = true;
                    self.executor.begin(false).map(|()| Outcome::done("BEGIN"))
                }
                // A `COMMIT` on a **failed** block rolls it back and says so, which is
                // PostgreSQL's own answer and the reason the tag is `ROLLBACK`.
                StatementClass::Commit if self.failed => {
                    self.in_block = false;
                    self.failed = false;
                    self.executor.rollback().map(|()| Outcome::done("ROLLBACK"))
                }
                StatementClass::Commit => {
                    self.in_block = false;
                    self.executor.commit().map(|()| Outcome::done("COMMIT"))
                }
                StatementClass::Rollback => {
                    self.in_block = false;
                    self.failed = false;
                    self.executor.rollback().map(|()| Outcome::done("ROLLBACK"))
                }
                StatementClass::Savepoint(name) => {
                    if self.in_block {
                        self.executor
                            .savepoint(name)
                            .map(|()| Outcome::done("SAVEPOINT"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock("SAVEPOINT"))
                    }
                }
                StatementClass::RollbackTo(name) => {
                    if self.in_block {
                        // The one statement that recovers an aborted block.
                        self.executor
                            .rollback_to(name)
                            .inspect(|()| {
                                self.failed = false;
                            })
                            .map(|()| Outcome::done("ROLLBACK"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock(
                            "ROLLBACK TO SAVEPOINT",
                        ))
                    }
                }
                StatementClass::Release(name) => {
                    if self.in_block {
                        self.executor
                            .release(name)
                            .map(|()| Outcome::done("RELEASE"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock(
                            "RELEASE SAVEPOINT",
                        ))
                    }
                }
                _ => self.executor.execute(&parsed, &Params::NONE),
            };
            match outcome {
                Ok(outcome) => last = outcome,
                Err(error) => {
                    if self.in_block && error.aborts_transaction() {
                        self.failed = true;
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    }

    /// The notices the last statement produced, **after** `client_min_messages` has filtered
    /// them — which is the only place a suppressed notice can be observed, because a corpus
    /// records rows and a notice is not one.
    pub(crate) fn executor_notices(&mut self) -> Vec<esker_sql::error::SqlError> {
        self.executor.take_notices()
    }

    /// The rows a query returns, or a panic naming the refusal — for the assertions a corpus
    /// cannot carry.
    pub(crate) fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self.answer(sql) {
            Answer::Rows { rows, .. } => rows,
            other => panic!("{sql}: {other}"),
        }
    }

    /// One statement, in the corpus's own shape.
    pub(crate) fn answer(&mut self, sql: &str) -> Answer {
        match self.run(sql) {
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

/// Replays a corpus over a fixture and asserts every line, holding the divergences from both
/// sides. Answers how many statements ran, so a caller can assert the file loaded at all.
pub(crate) fn replay(corpus: &str, fixture: &[&str], divergences: &Divergences) -> usize {
    let mut node = Node::new(fixture);
    let mut checked = 0;
    let mut mismatched = Vec::new();
    // Statements the aborted transaction swallowed — see the `25P02` arm below.
    let mut cascaded = 0_usize;
    let mut type_mismatched = Vec::new();
    let mut agreed_after_all = Vec::new();

    for (line_number, statement, expected) in parse(corpus) {
        let listed = divergences.answers.iter().any(|(sql, _)| *sql == statement);
        let actual = node.answer(&statement);
        checked += 1;

        if listed {
            if actual == expected {
                agreed_after_all.push(format!("line {line_number}: {statement}"));
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
                // An undeclared types column pins nothing: the rows are the whole claim, and a
                // difference in the declared types is neither a divergence nor a disagreement.
            ) if rows == theirs && types.is_empty() && types != ours => {}
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
            ) if rows == theirs && types != ours => {
                if !divergences.types.contains(&statement.as_str()) {
                    type_mismatched.push(format!(
                        "line {line_number}: {statement}\n  PostgreSQL: {types:?}\n  \
                         Esker:      {ours:?}"
                    ));
                }
            }
            _ if actual == expected => {
                if divergences.types.contains(&statement.as_str()) {
                    agreed_after_all.push(format!("line {line_number}: {statement}"));
                }
            }
            // **A cascade, not a disagreement.** A session capture runs inside one `BEGIN`, so the
            // first statement this node refuses that PostgreSQL answers aborts the transaction and
            // every statement after it comes back `25P02` — forty of them in one file. Listing
            // those as forty divergences would bury the one that caused them and would need forty
            // entries to silence; they are counted instead, and the root divergence above is what
            // has to be declared or fixed.
            (_, Answer::Refused(message))
                if message.starts_with("25P02") || message.starts_with("3B001") =>
            {
                // `3B001` is the second-order consequence: the `SAVEPOINT` that would have created
                // it was itself swallowed by the abort, so the `ROLLBACK TO` has nothing to return
                // to. Counted with the rest rather than reported as a divergence of its own.
                cascaded += 1;
            }
            _ => mismatched.push(format!(
                "line {line_number}: {statement}\n  PostgreSQL: {expected}\n  Esker:      {actual}"
            )),
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} of {checked} statements disagree with PostgreSQL 19 and are not listed as \
         divergences ({cascaded} more were swallowed by the aborted transaction and are not \
         counted):\n\n{}",
        mismatched.len(),
        mismatched.join("\n\n")
    );
    assert!(
        type_mismatched.is_empty(),
        "{} statements have the right rows and an unlisted type divergence:\n\n{}",
        type_mismatched.len(),
        type_mismatched.join("\n\n")
    );
    assert!(
        agreed_after_all.is_empty(),
        "{} statements are listed as divergences and now agree with PostgreSQL -- delete the \
         entries:\n\n{}",
        agreed_after_all.len(),
        agreed_after_all.join("\n")
    );
    checked
}

/// The name `\gdesc` prints for an OID, which is the name [`PgType`] already knows.
/// A column's type as `\gdesc` writes it, **with its typmod**: `character varying(5)`, not
/// `character varying`.
///
/// The number is what the capture holds, so leaving it out would make every corpus with a
/// `varchar(n)`, a `character(n)` or a `timestamp(p)` in it agree by not looking.
fn type_name(oid: u32, typmod: i32) -> String {
    esker_sql::value::ColumnType::ALL
        .into_iter()
        .find(|ty| ty.oid() == oid)
        .map_or_else(
            || "?".to_owned(),
            |ty| esker_sql::value::format_type(ty, typmod),
        )
}

/// One corpus file, as `(line number, statement, what PostgreSQL answered)`.
fn parse(corpus: &str) -> Vec<(usize, String, Answer)> {
    corpus
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let statement = fields.next().unwrap_or_default().to_owned();
            let second = fields
                .next()
                .unwrap_or_else(|| panic!("line {}: no answer", index + 1));
            let answer = match second.strip_prefix('!') {
                Some(message) => Answer::Refused(message.to_owned()),
                // **An empty types field means the types were not declared, not that there is no
                // result set.** A `CREATE TABLE` leaves it blank because `\gdesc` describes
                // nothing for one — and so does a `SELECT` in a session capture whose `\gdesc`
                // pass ran after its transaction rolled back, which is how such a capture records
                // a query over a table that no longer exists. What decides is the *rows* field:
                // `-` or nothing at all is a command, and anything else is rows whose declared
                // types this line does not pin.
                None if second.is_empty() => match fields.next() {
                    None | Some("-") => Answer::Done,
                    Some(rows) => Answer::Rows {
                        types: Vec::new(),
                        rows: rows
                            .split(" ; ")
                            .map(|row| row.split('|').map(str::to_owned).collect())
                            .collect(),
                    },
                },
                None => Answer::Rows {
                    types: second.split(',').map(str::to_owned).collect(),
                    rows: match fields.next().unwrap_or("-") {
                        "-" => Vec::new(),
                        rows => rows
                            .split(" ; ")
                            .map(|row| row.split('|').map(str::to_owned).collect())
                            .collect(),
                    },
                },
            };
            (index + 1, statement, answer)
        })
        .collect()
}
