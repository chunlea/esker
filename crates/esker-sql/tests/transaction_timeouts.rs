//! `statement_timeout`, `lock_timeout`, `idle_in_transaction_session_timeout`, deadlock detection
//! and serialization failure — **the file run 47 hung on**.
//!
//! `adapters/postgresql/transaction_test.rb` sat for twenty minutes on one `ESTABLISHED`
//! connection with both sides at 0% CPU. Its tests synchronise two connections on *one side
//! blocking*: A takes a row, B waits for it, and the assertion is about how the wait ends. This
//! node never waits — a Percolator prewrite that meets a live lock is `40001` after a bounded
//! backoff, never an indefinite wait — so a test that waits for the wait waits for ever.
//!
//! That is the shape of every divergence below, and it is not a bug to fix here: making the SQL
//! layer block on a row lock would be a different concurrency-control scheme, not a repair.
//! What *is* fixable, and what this unit does, is stop the node accepting a timeout it cannot
//! enforce. `statement_timeout` and `lock_timeout` join the registry, so
//!
//! * `SHOW statement_timeout` answers `0`, which is **true**: no statement here is cancelled by a
//!   clock, and `0` is PostgreSQL's own spelling of that;
//! * `SET statement_timeout = 0` and `SET lock_timeout = 0` succeed, because they ask for what is
//!   already the case;
//! * any **non-zero** value is `0A000` naming the parameter, because a node that took `150ms` and
//!   then ran for a minute has told a client something untrue — and a client that believes it will
//!   wait for the cancellation that never comes, which is exactly the twenty minutes.
//!
//! Both were `42704` before, which was the wrong sentence about a parameter a real server has.
//!
//! The corpus is `tests/corpus/pg19_transaction_timeouts.txt` and it is **four columns**, not the
//! three every other corpus has: a lock wait, a `lock_timeout` firing and a deadlock all need a
//! *second* session to hold the conflicting lock, so each line names the session it ran on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

const TENANT: u64 = 1;

/// Two sessions over one store: separate executors, one backend and one catalog cache.
///
/// The arrangement the capture needs and the one `tests/parity_harness` cannot provide, because a
/// corpus there is one node's answer to a sequence of statements. Here the *interleaving* is the
/// subject.
struct Cluster {
    sessions: Vec<(char, Session)>,
}

/// One session: an executor plus the block state the real `crate::pgwire::session` keeps.
struct Session {
    executor: Executor,
    in_block: bool,
    failed: bool,
}

impl Cluster {
    fn new(names: &[char]) -> Self {
        let backend = Arc::new(MemoryBackend::new());
        let catalog = Arc::new(Catalog::new());
        let sessions = names
            .iter()
            .map(|name| {
                (
                    *name,
                    Session {
                        executor: Executor::new(
                            Arc::clone(&backend) as Arc<dyn Backend>,
                            Arc::clone(&catalog),
                            TENANT,
                        ),
                        in_block: false,
                        failed: false,
                    },
                )
            })
            .collect();
        Cluster { sessions }
    }

    fn session(&mut self, name: char) -> &mut Session {
        &mut self
            .sessions
            .iter_mut()
            .find(|(session, _)| *session == name)
            .expect("the corpus names a session the cluster has")
            .1
    }
}

impl Session {
    /// One statement, through the dispatch `crate::pgwire::session` uses — transaction control to
    /// the executor's own calls, everything else to `execute`, and the aborted-block rule on top.
    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            let class = parsed.class().clone();
            if self.failed
                && !matches!(
                    class,
                    StatementClass::Commit
                        | StatementClass::Rollback
                        | StatementClass::RollbackTo(_)
                )
            {
                return Err(esker_sql::SqlError::InFailedTransaction);
            }
            let outcome = match &class {
                StatementClass::Begin => {
                    self.in_block = true;
                    self.executor
                        .begin(parsed.begins_read_only())
                        .map(|()| Outcome::done("BEGIN"))
                }
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

    /// The answer in the corpus's own shape: `SQLSTATE message`, rows, or a bare command.
    fn answer(&mut self, sql: &str) -> String {
        match self.run(sql) {
            Err(error) => {
                let mut message = format!("!{} {error}", error.sqlstate());
                if let Some(detail) = error.detail() {
                    message.push_str(&format!(" DETAIL: {detail}"));
                }
                if let Some(hint) = error.hint() {
                    message.push_str(&format!(" HINT: {hint}"));
                }
                message
            }
            Ok(Outcome::Done { .. }) => "-".to_owned(),
            Ok(Outcome::Rows { rows, .. }) => {
                if rows.is_empty() {
                    "-".to_owned()
                } else {
                    rows.into_iter()
                        .map(|row| {
                            row.into_iter()
                                .map(|value| {
                                    value.map_or_else(
                                        || "\\N".to_owned(),
                                        |bytes| String::from_utf8(bytes).unwrap(),
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .collect::<Vec<_>>()
                        .join(" ; ")
                }
            }
        }
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
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
            other @ Outcome::Done { .. } => panic!("expected rows, got {other:?}"),
        }
    }
}

/// What this node answers differently, keyed by the statement **and its session**, each with the
/// reason.
///
/// Every entry is one of two facts, and they are worth separating because only the first is a
/// choice: **(1) a timeout this node cannot enforce is refused rather than accepted**, and **(2)
/// nothing here waits for a row lock**, from which the deadlock, the `NOWAIT` and the two
/// blocked-`UPDATE` lines all follow.
const DIVERGENCES: &[(char, &str, &str)] = &[
    // --- (1) the timeouts, refused by name rather than accepted -------------------------------
    (
        'A',
        "SET statement_timeout = '150ms'",
        "`0A000` naming the parameter. Nothing here cancels a running statement: the executor \
         runs one to completion on a blocking thread and no clock interrupts it. Accepting the \
         value would be the answer `crate::parameter` exists to refuse — a setting a client asked \
         for, was told it got, and did not get — and a client that believes it holds a 150 ms \
         cancellation waits for one for ever. `SET statement_timeout = 0` succeeds, because that \
         asks for what is already true.",
    ),
    (
        'A',
        "SELECT pg_sleep(2)",
        "`pg_sleep` is not a function this node has, so this is `42883` rather than a statement \
         that runs for two seconds and is cancelled. The line is kept because it is the oracle's \
         own proof that `statement_timeout` fires at all, which is the fact the refusal above is \
         about.",
    ),
    (
        'A',
        "SET LOCAL lock_timeout = '150ms'",
        "Two refusals meet on one line and the first one wins: `SET LOCAL` keeps a per-block undo \
         this node has only for `esker.read_as_of`, so it is `0A000 SET LOCAL lock_timeout` before \
         the value is looked at. The session-wide spelling is refused too, one entry down.",
    ),
    (
        'A',
        "SET LOCAL statement_timeout = '150ms'",
        "The same `SET LOCAL` refusal, for the same reason.",
    ),
    // --- (2) nothing waits for a row lock ------------------------------------------------------
    (
        'A',
        "UPDATE tt_rows SET n = 12 WHERE id = 1",
        "**The heart of the file.** PostgreSQL blocks here on B's row lock and ends the wait with \
         `55P03` when `lock_timeout` fires. This node takes no row locks: the `UPDATE` buffers its \
         write and succeeds, and the conflict — if the two transactions really do write the same \
         key — is detected at `COMMIT` as `40001`. So there is no wait to time out, which is why \
         `lock_timeout` is refused rather than implemented, and why the suite's tests hang: they \
         synchronise on one side blocking.",
    ),
    (
        'A',
        "UPDATE tt_rows SET n = 13 WHERE id = 1",
        "The same statement under `statement_timeout` instead, which PostgreSQL answers `57014` — \
         the same wait ended by a different clock. Here it is the same non-wait.",
    ),
    (
        'A',
        "SELECT id FROM tt_rows WHERE id = 1 FOR UPDATE NOWAIT",
        "`0A000 a row-level locking clause is not supported`. PostgreSQL answers `55P03 could not \
         obtain lock on row in relation \"tt_rows\"` — the same SQLSTATE as `lock_timeout` with a \
         different sentence, which is why a client cannot tell the two apart by code. A row lock \
         is what this node does not have, so the clause is refused by name rather than answered \
         with a lock nobody took.",
    ),
    (
        'A',
        "SELECT id FROM tt_rows ORDER BY id FOR UPDATE SKIP LOCKED",
        "The same refusal. PostgreSQL returns the rows it *could* take — `2`, with row 1 held by \
         B — which is a report about locks and therefore unanswerable here.",
    ),
    (
        'A',
        "UPDATE tt_rows SET n = n + 1 WHERE id = 2  (concurrent)",
        "**A deadlock cannot form where nobody waits.** PostgreSQL detects the cycle and kills \
         exactly one of the two transactions with `40P01`, leaving the other's `UPDATE` to \
         succeed. Here both `UPDATE`s succeed immediately and the losing side, if any, fails at \
         its `COMMIT` with `40001`. `40P01` is a code this node can never raise, and saying so is \
         the honest answer: a lock graph is what a waiter builds.",
    ),
    (
        'B',
        "COMMIT",
        "PostgreSQL's `SERIALIZABLE` detects the read/write dependency between two transactions \
         that both read `sum(n)` and each insert a *different* row, and refuses B's commit with \
         `40001`. This node is snapshot isolation (ADR 0031): its conflict rule is about the keys \
         a transaction **wrote**, so two inserts of different keys both commit and the table ends \
         with 4 rows rather than 3. `BEGIN ISOLATION LEVEL SERIALIZABLE` is accepted and gives \
         snapshot isolation — the standing caveat this whole phase is measured under.",
    ),
    (
        'A',
        "SELECT count(*) FROM tt_rows",
        "The row above's consequence, one line later: 4 here where PostgreSQL has 3, because B's \
         insert survived. A follow-on and not a divergence of its own.",
    ),
    (
        'B',
        "SELECT 'after idling'",
        "PostgreSQL has **terminated the session** by now: `idle_in_transaction_session_timeout` \
         fired, so this is a libpq-level `FATAL: terminating connection due to \
         idle-in-transaction timeout` and not an answer at all. This node has no idle timer, so \
         the session is alive and answers the string. The next unit is what closes it.",
    ),
    (
        'B',
        "ROLLBACK",
        "The same: PostgreSQL's connection is gone and libpq cannot even find a socket. Here the \
         block is open and rolls back.",
    ),
];

/// Every line of the capture, replayed across two sessions in file order.
#[test]
fn every_transaction_timeout_answer_is_postgresql_19_s() {
    let mut cluster = Cluster::new(&['A', 'B']);
    let mut checked = 0;
    let mut mismatched = Vec::new();
    // `(entry, line, what ran)` for the occurrences that agreed, and the entries that were reached
    // at all — the two halves of rule 2.
    let mut agreed: Vec<(usize, usize, String)> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();

    for (line_number, session, statement, expected) in corpus() {
        let listed = DIVERGENCES
            .iter()
            .position(|(who, sql, _)| *who == session && *sql == statement);
        // **The `(concurrent)` marker is a fact about the capture, not about the SQL.** Two lines
        // there ran in threads deliberately blocked on each other; nothing here blocks, so they
        // are replayed in file order with the marker stripped, and the marker stays in the key so
        // the divergence entry names the line the capture actually holds.
        let sql = statement
            .strip_suffix("  (concurrent)")
            .unwrap_or(&statement);
        let actual = cluster.session(session).answer(sql);
        checked += 1;

        // **One entry, however many times the pair appears.** `ROLLBACK` on session B is three
        // lines of this capture and only the last one diverges — the one after PostgreSQL has
        // terminated the session. So rule 2 is decided per *entry* after the whole file: an entry
        // is stale only when **every** occurrence of it agreed.
        if let Some(at) = listed {
            seen.push(at);
            if actual == expected {
                agreed.push((at, line_number, format!("{session}: {statement}")));
            }
            continue;
        }
        if actual != expected {
            mismatched.push(format!(
                "line {line_number}: {session}: {statement}\n  PostgreSQL 19: {expected}\n  \
                 here:          {actual}"
            ));
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} of {checked} statements disagree with PostgreSQL 19 and are not listed as \
         divergences:\n\n{}",
        mismatched.len(),
        mismatched.join("\n\n")
    );
    let stale: Vec<String> = DIVERGENCES
        .iter()
        .enumerate()
        .filter(|(at, _)| {
            let occurrences = seen.iter().filter(|seen| *seen == at).count();
            occurrences > 0
                && agreed.iter().filter(|(entry, ..)| entry == at).count() == occurrences
        })
        .map(|(at, (who, sql, _))| {
            let line = agreed
                .iter()
                .find(|(entry, ..)| *entry == at)
                .map_or(0, |(_, line, _)| *line);
            format!("line {line}: {who}: {sql}")
        })
        .collect();
    assert!(
        stale.is_empty(),
        "{} entries are listed as divergences and every occurrence now agrees -- delete \
         them:\n\n{}",
        stale.len(),
        stale.join("\n")
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `(line number, session, statement, what PostgreSQL answered)`, from the four-column corpus.
///
/// A line whose session column is `#` is a section marker rather than a statement.
fn corpus() -> Vec<(usize, char, String, String)> {
    include_str!("corpus/pg19_transaction_timeouts.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|(index, line)| {
            let mut fields = line.split('\t');
            let session = fields.next().unwrap_or_default().chars().next()?;
            if session == '#' {
                return None;
            }
            let statement = fields.next().unwrap_or_default().to_owned();
            // The types column is never declared in this capture — `txncap.rb` drives libpq
            // rather than `psql`, so there is no `\gdesc` pass — and the rows column is what
            // carries the answer.
            let _types = fields.next().unwrap_or_default();
            let answer = fields.next().unwrap_or("-").to_owned();
            Some((index + 1, session, statement, answer))
        })
        .collect()
}

/// **`0` is the truth and it is accepted; anything else is refused by name.**
///
/// The whole of what this unit adds, asserted on both new parameters. `0` is PostgreSQL's own
/// spelling of "no timeout", which is this node's permanent condition, so `SHOW` answering `0` is
/// exact rather than a placeholder.
#[test]
fn a_timeout_of_zero_is_accepted_and_a_real_one_is_refused_by_name() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');

    for name in ["statement_timeout", "lock_timeout"] {
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["0"]],
            "boot: {name}"
        );
        session.run(&format!("SET {name} = 0")).unwrap();
        session.run(&format!("SET {name} = '0ms'")).unwrap();
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["0"]],
            "after 0: {name}"
        );

        let error = session.run(&format!("SET {name} = '150ms'")).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {name}");
        assert!(
            error.to_string().contains(name),
            "the refusal must name the parameter, got: {error}"
        );
        // The refusal did not change the value: a `SET` that failed leaves the old one.
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["0"]],
            "unchanged: {name}"
        );
    }
}

/// **`42704` was the wrong sentence about these two**, and this is what changed.
///
/// A real server *has* `statement_timeout` and `lock_timeout`, so "unrecognized configuration
/// parameter" was a claim about PostgreSQL rather than about this node. `0A000` naming the
/// parameter is contract C2's answer and is true of both.
#[test]
fn they_are_no_longer_unrecognized_parameters() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');
    for name in ["statement_timeout", "lock_timeout"] {
        // Neither `SHOW` nor `RESET` may claim the parameter does not exist any more.
        session.run(&format!("SHOW {name}")).unwrap();
        session.run(&format!("RESET {name}")).unwrap();
    }
}

/// **Nothing waits, so the conflict lands at `COMMIT`** — the fact every divergence above rests
/// on, asserted directly rather than inferred from the corpus.
///
/// Two sessions update the same row. On PostgreSQL the second `UPDATE` blocks until the first
/// transaction ends; here both return at once and the loser is refused when it commits. That is
/// the whole reason `lock_timeout` has nothing to bound.
#[test]
fn two_writers_to_one_row_both_proceed_and_the_loser_fails_at_commit() {
    let mut cluster = Cluster::new(&['A', 'B']);
    cluster
        .session('A')
        .run("CREATE TABLE tt_rows (id bigint primary key, n integer)")
        .unwrap();
    cluster
        .session('A')
        .run("INSERT INTO tt_rows VALUES (1, 10), (2, 20)")
        .unwrap();

    cluster.session('B').run("BEGIN").unwrap();
    cluster
        .session('B')
        .run("UPDATE tt_rows SET n = 11 WHERE id = 1")
        .unwrap();

    cluster.session('A').run("BEGIN").unwrap();
    // **This is where PostgreSQL blocks.** It returns immediately here.
    cluster
        .session('A')
        .run("UPDATE tt_rows SET n = 12 WHERE id = 1")
        .unwrap();

    cluster.session('B').run("COMMIT").unwrap();
    let error = cluster.session('A').run("COMMIT").unwrap_err();
    // **`23505` was the answer here and it was wrong.** The `UPDATE` never touched `id`, and the
    // loser was told its primary key was a duplicate — because a rewritten row's own index
    // entries were recorded as newly-added ones (`exec::Written::rewritten`).
    assert_eq!(
        error.sqlstate(),
        "40001",
        "the loser of a write-write race is 40001, and it arrives at COMMIT"
    );
    assert!(
        !error.to_string().contains("duplicate key"),
        "a row neither session re-keyed is not a duplicate: {error}"
    );
    // B's value is the one that stands.
    assert_eq!(
        cluster
            .session('A')
            .rows("SELECT n FROM tt_rows WHERE id = 1"),
        [["11"]]
    );
}

/// **A row-level locking clause is refused by name in all three spellings**, and the plain
/// `FOR UPDATE` with them.
///
/// The capture only carries `NOWAIT` and `SKIP LOCKED`; the bare clause is here because it is the
/// one `ActiveRecord`'s `lock!` sends, and a refusal that covered only the two decorated forms
/// would be a gap nobody had looked at.
#[test]
fn every_row_locking_clause_is_refused_by_name() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');
    session
        .run("CREATE TABLE tt_rows (id bigint primary key, n integer)")
        .unwrap();

    for written in [
        "SELECT id FROM tt_rows WHERE id = 1 FOR UPDATE",
        "SELECT id FROM tt_rows WHERE id = 1 FOR UPDATE NOWAIT",
        "SELECT id FROM tt_rows ORDER BY id FOR UPDATE SKIP LOCKED",
        "SELECT id FROM tt_rows FOR SHARE",
    ] {
        let error = session.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {written}");
        assert_eq!(
            error.to_string(),
            "a row-level locking clause is not supported",
            "for {written}"
        );
    }
}

/// **`BEGIN ISOLATION LEVEL SERIALIZABLE` is accepted and gives snapshot isolation**, which is the
/// caveat ADR 0031 makes permanent — and the observable half of it is here.
///
/// Two transactions each read the whole table and each insert a *different* row. PostgreSQL's SSI
/// sees the read/write dependency and refuses the second commit; this node's conflict rule is
/// about keys **written**, so both commit and the table has four rows where PostgreSQL has three.
#[test]
fn serializable_is_snapshot_isolation_and_two_different_keys_both_commit() {
    let mut cluster = Cluster::new(&['A', 'B']);
    cluster
        .session('A')
        .run("CREATE TABLE tt_rows (id bigint primary key, n integer)")
        .unwrap();
    cluster
        .session('A')
        .run("INSERT INTO tt_rows VALUES (1, 10), (2, 20)")
        .unwrap();

    for who in ['A', 'B'] {
        cluster
            .session(who)
            .run("BEGIN ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        assert_eq!(
            cluster.session(who).rows("SELECT sum(n) FROM tt_rows"),
            [["30"]],
            "both read the same snapshot"
        );
    }
    cluster
        .session('A')
        .run("INSERT INTO tt_rows VALUES (3, 30)")
        .unwrap();
    cluster
        .session('B')
        .run("INSERT INTO tt_rows VALUES (4, 40)")
        .unwrap();
    cluster.session('A').run("COMMIT").unwrap();
    cluster
        .session('B')
        .run("COMMIT")
        .expect("different keys do not conflict under snapshot isolation");

    assert_eq!(
        cluster.session('A').rows("SELECT count(*) FROM tt_rows"),
        [["4"]],
        "PostgreSQL's SERIALIZABLE leaves 3 here; snapshot isolation leaves 4"
    );
}

/// The cluster's two handles really are one store, which is what makes every test above a
/// statement about concurrency rather than about two unrelated databases.
///
/// Worth its own test because the failure is silent: two `MemoryBackend`s would make every
/// conflict test above pass by never conflicting.
#[test]
fn the_two_sessions_share_one_store() {
    let mut cluster = Cluster::new(&['A', 'B']);
    cluster
        .session('A')
        .run("CREATE TABLE tt_rows (id bigint primary key, n integer)")
        .unwrap();
    cluster
        .session('A')
        .run("INSERT INTO tt_rows VALUES (1, 10)")
        .unwrap();
    assert_eq!(
        cluster
            .session('B')
            .rows("SELECT n FROM tt_rows WHERE id = 1"),
        [["10"]],
        "B reads the table and the row A committed"
    );
    // And it is the same store under a snapshot rather than two that happen to agree: a row A
    // writes inside an open block is invisible to B until A commits.
    cluster.session('A').run("BEGIN").unwrap();
    cluster
        .session('A')
        .run("INSERT INTO tt_rows VALUES (2, 20)")
        .unwrap();
    assert_eq!(
        cluster.session('B').rows("SELECT count(*) FROM tt_rows"),
        [["1"]],
        "B does not see A's uncommitted row"
    );
    cluster.session('A').run("COMMIT").unwrap();
    assert_eq!(
        cluster.session('B').rows("SELECT count(*) FROM tt_rows"),
        [["2"]],
        "and sees it once A commits"
    );
}
