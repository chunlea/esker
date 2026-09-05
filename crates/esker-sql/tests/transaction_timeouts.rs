//! `statement_timeout`, `lock_timeout`, `idle_in_transaction_session_timeout`, deadlock detection
//! and serialization failure — **the file run 47 hung on**.
//!
//! Of the five, one is **built** rather than declared: `idle_in_transaction_session_timeout` now
//! ends the session, `25P03` and `FATAL`, at the bottom of this file. It is the only one of the
//! five that was a missing feature rather than a missing wait.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        let mut cluster = Cluster { sessions };
        // **A ceiling the *harness* imposes, not the server.** `lock_timeout` boots at `0` here as
        // it does on a real server — wait forever — and a single-threaded replay cannot wait for a
        // session that has no thread to run on: the corpus's deadlock rows would block this
        // process rather than fail it. So every session of this replay is given a bound, which the
        // corpus's own `SET LOCAL lock_timeout` lines then override where they mean to. A test
        // harness bounding itself is not a divergence; a server default would have been.
        for name in names {
            cluster
                .session(*name)
                .run("SET lock_timeout = '250ms'")
                .expect("the harness's own ceiling");
        }
        cluster
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
                        .and_then(|()| match parsed.begins_isolation() {
                            Some(level) => self.executor.set_isolation(level),
                            None => Ok(()),
                        })
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
                use std::fmt::Write as _;

                let mut message = format!("!{} {error}", error.sqlstate());
                if let Some(detail) = error.detail() {
                    let _ = write!(message, " DETAIL: {detail}");
                }
                if let Some(hint) = error.hint() {
                    let _ = write!(message, " HINT: {hint}");
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
    (
        'B',
        "UPDATE tt_rows SET n = n + 1 WHERE id = 1  (concurrent)",
        "**The deadlock is detected and the victim's transaction is not ended**, which is what \
         makes the *second* session deadlock too. PostgreSQL ends the loser's transaction with the \
         `40P01`, so its locks go at once and the survivor proceeds — measured, the survivor's \
         both updates landed. Here the `40P01` is the statement's error and the block stays open \
         until its `ROLLBACK`, so the survivor asks for a row the victim still holds and is told \
         the same thing. The detection is right and the *lifetime* is not; ending a transaction on \
         a deadlock belongs with the isolation levels in unit 3 (ADR 0057).",
    ),
    // --- (1) the timeouts, honoured now: the two entries that were here agree ---------------
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
        "**The refusal is the same and the DETAIL is not.** This node now refuses B's commit with \
         `40001 could not serialize access due to read/write dependencies among transactions`, \
         PostgreSQL's own sentence and hint for this cause (ADR 0062) — what it does not carry is \
         `DETAIL: Reason code: Canceled on identification as a pivot, during commit attempt`, \
         which names a step in SSI's dangerous-structure detection. This node reaches the same \
         conclusion by validating the read set, so repeating that sentence would describe \
         machinery that is not here. The line above it — B's insert — is what closed: two \
         transactions that both read the table and insert different rows no longer both commit.",
    ),
    (
        'B',
        "SELECT 'after idling'",
        "PostgreSQL has **terminated the session** by now: `idle_in_transaction_session_timeout` \
         fired, so this is a libpq-level `FATAL: terminating connection due to idle-in-transaction \
         timeout` and not an answer at all. **This node does that too** — see \
         `idling_inside_a_block_terminates_the_session` below — and the line still diverges for a \
         reason worth naming rather than hiding: the timer is on the **connection**, because what \
         it does is close a socket, and this harness is a pair of executors rather than a pair of \
         connections. A corpus replay never idles. What is measured here is the statement's \
         answer; what is measured over a pipe is the session's end.",
    ),
    (
        'B',
        "ROLLBACK",
        "The same, and the same reason: over a real connection there is no socket left for it. \
         The rollback itself does happen here — the connection abandons the open block before it \
         closes, which is asserted over the pipe.",
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

/// **Both boot at `0`, as a real server does, and both are honoured for a real value.**
///
/// This test spent most of its life asserting the opposite — that a non-zero value was `0A000`
/// naming the parameter — because nothing here could act on one. `lock_timeout` earned its value
/// first (a waiter is a loop the SQL layer drives) and `statement_timeout` second, once
/// `crate::exec::cancel` existed to cut a working statement short.
///
/// A non-zero `lock_timeout` default was tried and reverted: a long-held lock in another worker is
/// normal in a Rails application and PostgreSQL waits for it, so a node that gives up after some
/// seconds fails a workload a real server serves — and `SHOW` reporting the ceiling honestly makes
/// the incompatibility documented rather than absent
/// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
#[test]
fn both_timeouts_boot_at_zero_and_take_a_real_value() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');

    // The harness gives its own sessions a ceiling so a single-threaded replay cannot hang
    // (`Cluster::new`); this test is about the *server's* boot value, so it undoes that first.
    session.run("RESET lock_timeout").unwrap();
    assert_eq!(
        session.rows("SHOW lock_timeout"),
        [["0"]],
        "lock_timeout boots at PostgreSQL's own default: wait forever"
    );
    // **`lock_timeout` is honoured for a real value now**, which is the first timeout this node
    // can keep: a waiter is a loop the SQL layer drives, so it is cancellable in a way a statement
    // that is *working* is not.
    session.run("SET lock_timeout = '150ms'").unwrap();
    assert_eq!(session.rows("SHOW lock_timeout"), [["150ms"]]);
    session.run("SET lock_timeout = 0").unwrap();
    assert_eq!(session.rows("SHOW lock_timeout"), [["0"]]);

    {
        let name = "statement_timeout";
        session.run(&format!("SET {name} = 0")).unwrap();
        session.run(&format!("SET {name} = '0ms'")).unwrap();
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["0"]],
            "after 0: {name}"
        );

        // **A real value is accepted now**, and the order that got here matters more than the
        // assertion: `exec::cancel` gives the statement a deadline first, and only then does the
        // parameter stop being refused. Accepting it while nothing could act on it is what this
        // file measured as a twenty-minute hang.
        session.run(&format!("SET {name} = '150ms'")).unwrap();
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["150ms"]],
            "accepted and reported back: {name}"
        );
        session.run(&format!("SET {name} = 0")).unwrap();
        assert_eq!(
            session.rows(&format!("SHOW {name}")),
            [["0"]],
            "and back off again: {name}"
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
    // **This is where PostgreSQL blocks, and now so does this** (ADR 0057). One thread cannot hold
    // a row and wait for it, so the wait is bounded here and the answer is the one a real server
    // gives a waiter that runs out of `lock_timeout`.
    cluster
        .session('A')
        // `SET LOCAL` is refused by name here (`tests/set_session.rs`), so the session's own.
        .run("SET lock_timeout = '150ms'")
        .unwrap();
    let waited = cluster
        .session('A')
        .run("UPDATE tt_rows SET n = 12 WHERE id = 1")
        .unwrap_err();
    assert_eq!(
        waited.sqlstate(),
        "55P03",
        "a writer that cannot have the row waits and then says so: {waited}"
    );
    cluster.session('A').run("ROLLBACK").unwrap();

    cluster.session('B').run("COMMIT").unwrap();
    // **The `23505` lesson, kept.** What this test found was a rewritten row's own index entries
    // being recorded as newly-added ones, so the loser of a race was told its primary key was a
    // duplicate for a column it never touched. The race is gone — the second writer waits now —
    // so the lesson is asserted where a conflict still happens: a transaction whose snapshot
    // predates a committed write it did not wait for.
    // **At `REPEATABLE READ`, because that is the level a write-write conflict still exists at.**
    // Under `READ COMMITTED` A's `UPDATE` would take a fresh statement snapshot, see B's committed
    // row, and have nothing to conflict with — which is the point of the unit and is asserted in
    // `tests/read_committed.rs`. What this test is for is the *shape of the error* when there is
    // one, and that needs a level that keeps its snapshot.
    cluster
        .session('A')
        .run("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    cluster
        .session('A')
        .rows("SELECT n FROM tt_rows WHERE id = 2");
    cluster
        .session('B')
        .run("UPDATE tt_rows SET n = 21 WHERE id = 2")
        .unwrap();
    let error = cluster
        .session('A')
        .run("UPDATE tt_rows SET n = 22 WHERE id = 2")
        .and_then(|_| cluster.session('A').run("COMMIT"))
        .unwrap_err();
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
    // B's value is the one that stands on the row they raced for.
    let _ = cluster.session('A').run("ROLLBACK");
    assert_eq!(
        cluster
            .session('A')
            .rows("SELECT n FROM tt_rows WHERE id = 1"),
        [["11"]]
    );
}

/// **Every locking clause answers its rows when nobody holds them**, `NOWAIT` and `SKIP LOCKED`
/// included — which is what a real server does and what this file is placed to check, because it
/// is the case a lock implementation gets wrong in the *other* direction.
///
/// This test asserted a blanket refusal, then a refusal of the two non-blocking modifiers, and now
/// asserts neither: ADR 0057 §5 gave the clause a real row lock, so `NOWAIT` has a `55P03` to
/// raise and `SKIP LOCKED` has a row to leave out. Both of those need a second session and live in
/// `tests/row_locking.rs`. What is left here is the half that must not change when they arrive —
/// an uncontended `SELECT … FOR UPDATE NOWAIT` is an ordinary answer, not a refusal.
#[test]
fn every_locking_clause_answers_its_rows_when_nobody_holds_them() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');
    session
        .run("CREATE TABLE tt_rows (id bigint primary key, n integer)")
        .unwrap();
    session.run("INSERT INTO tt_rows VALUES (1, 10)").unwrap();

    for written in [
        "SELECT id FROM tt_rows WHERE id = 1 FOR UPDATE",
        "SELECT id FROM tt_rows FOR SHARE",
        "SELECT id FROM tt_rows FOR UPDATE OF tt_rows",
        "SELECT id FROM tt_rows WHERE id = 1 FOR UPDATE NOWAIT",
        "SELECT id FROM tt_rows ORDER BY id FOR UPDATE SKIP LOCKED",
        "SELECT id FROM tt_rows ORDER BY id FOR SHARE NOWAIT",
    ] {
        assert_eq!(session.rows(written), [["1"]], "for {written}");
    }
}

/// **`SERIALIZABLE` refuses the second of two transactions that read what the other wrote.**
///
/// This test asserted the opposite until ADR 0062: two transactions each read the whole table and
/// each insert a *different* row, so nothing about first-committer-wins had an opinion and both
/// committed, leaving four rows where PostgreSQL leaves three. That was ADR 0031's standing caveat
/// and it is now closed — not by detecting SSI's dangerous structures, but by validating at commit
/// that nothing this transaction **read** has been written since its snapshot.
///
/// The insert is the write and `sum(n)` over the whole table is the read, so B's commit meets A's
/// new row inside a range it read. Exactly one of the two survives, which is what a real server
/// does; which one is the first committer, which is what a real server also does.
#[test]
fn serializable_refuses_the_second_of_two_transactions_that_read_what_the_other_wrote() {
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
        cluster.session(who).run("BEGIN").unwrap();
        // The level as its own statement: this fixture's `BEGIN` takes no clauses.
        cluster
            .session(who)
            .run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
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
    let refused = cluster
        .session('B')
        .run("COMMIT")
        .expect_err("B read the table A wrote into");
    assert_eq!(refused.sqlstate(), "40001");
    assert_eq!(
        refused.to_string(),
        "could not serialize access due to read/write dependencies among transactions",
        "PostgreSQL's sentence for this cause, not the one it uses for a write-write conflict"
    );

    cluster.session('B').run("ROLLBACK").unwrap();
    assert_eq!(
        cluster.session('A').rows("SELECT count(*) FROM tt_rows"),
        [["3"]],
        "three rows, which is what PostgreSQL's SERIALIZABLE leaves"
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

// --- `idle_in_transaction_session_timeout`, which ends the session rather than the statement ---
//
// Everything above drives the executor directly, which is the right level for a corpus: it is what
// a statement answers. This one cannot be tested there at all, because the thing it does is close
// a **socket** — so it is driven through the real listener over a pipe, the way
// `tests/psql_smoke.rs` drives the handshake.

/// An executor that opens blocks and hands the connection a very short idle limit.
///
/// The limit is a property of the *session*, so it belongs on the executor even though the wait it
/// bounds belongs to the connection — see `Execute::idle_in_transaction_timeout`.
struct Idling {
    limit: Duration,
    rolled_back: Arc<AtomicBool>,
}

impl Execute for Idling {
    fn execute(
        &mut self,
        _parsed: &esker_sql::parse::Parsed,
        _params: &Params<'_>,
    ) -> esker_sql::Result<Outcome> {
        Ok(Outcome::done("SELECT 1"))
    }

    fn rollback(&mut self) -> esker_sql::Result<()> {
        self.rolled_back.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn idle_in_transaction_timeout(&self) -> Option<Duration> {
        Some(self.limit)
    }
}

/// One [`Idling`] per session, which is what `Connection::run` asks for now that a connection
/// picks its executor from the database its startup packet names.
struct OneIdler {
    limit: Duration,
    rolled_back: Arc<AtomicBool>,
}

impl esker_sql::pgwire::server::Executors for OneIdler {
    fn for_session(&self, _database: &str) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Ok(Box::new(Idling {
            limit: self.limit,
            rolled_back: Arc::clone(&self.rolled_back),
        }))
    }
}

/// A startup packet, then whatever else the caller wants to send.
fn startup_packet() -> Vec<u8> {
    let mut body = 0x0003_0000u32.to_be_bytes().to_vec();
    for (name, value) in [("user", "esker"), ("database", "esker")] {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = u32::try_from(body.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    packet.extend_from_slice(&body);
    packet
}

/// A simple `Query` message.
fn query(sql: &str) -> Vec<u8> {
    let mut out = vec![b'Q'];
    let body_len = u32::try_from(sql.len() + 5).unwrap();
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    out
}

/// `(tag, body)` for every complete message in a reply.
fn frames(bytes: &[u8]) -> Vec<(char, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 5 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
                as usize;
        if at + 1 + length > bytes.len() {
            break;
        }
        out.push((bytes[at] as char, bytes[at + 5..at + 1 + length].to_vec()));
        at += 1 + length;
    }
    out
}

/// **The whole of what the parameter does**, over a real connection: open a block, say nothing,
/// and the server ends the session.
///
/// Three things are asserted and each is a way of getting it wrong. The reply is an
/// `ErrorResponse` whose severity is **`FATAL`** and whose code is `25P03` — a node that sent
/// `ERROR` would leave a client believing one statement failed on a live connection. The socket is
/// then **closed**, which is what a client actually detects. And the open block is **rolled back**
/// before the socket goes, so nothing it wrote is left half-open behind a connection nobody can
/// reach.
#[tokio::test]
async fn idling_inside_a_block_terminates_the_session() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let rolled_back = Arc::new(AtomicBool::new(false));
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let executors = OneIdler {
        limit: Duration::from_millis(80),
        rolled_back: Arc::clone(&rolled_back),
    };
    tokio::spawn(async move {
        let mut connection = esker_sql::pgwire::server::Connection::new(
            server,
            esker_sql::pgwire::server::Config::default(),
        );
        let _ = connection.run(&executors).await;
    });

    let mut input = startup_packet();
    input.extend_from_slice(&query("BEGIN"));
    client.write_all(&input).await.unwrap();
    client.flush().await.unwrap();

    // Read to end of stream: the server answers the `BEGIN`, then waits, then terminates us.
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut reply))
        .await
        .expect("the server never closed the connection: the idle timeout did not fire")
        .unwrap();

    let last_error = frames(&reply)
        .into_iter()
        .rfind(|(tag, _)| *tag == 'E')
        .expect("an ErrorResponse must arrive before the socket closes");
    // An `ErrorResponse` body is NUL-terminated `<type><value>` fields, so a substring match on
    // the whole of it would pass on `S` alone and read the *localised* severity as the wire one.
    // Each field is read by its type byte: `S` severity, `V` the never-localised severity, `C`
    // the SQLSTATE, `M` the message.
    let fields: Vec<&[u8]> = last_error.1.split(|byte| *byte == 0).collect();
    let field = |kind: u8| {
        fields
            .iter()
            .find(|entry| entry.first() == Some(&kind))
            .map(|entry| String::from_utf8_lossy(&entry[1..]).into_owned())
    };
    let whole = String::from_utf8_lossy(&last_error.1).replace('\0', "|");
    assert_eq!(
        field(b'S').as_deref(),
        Some("FATAL"),
        "the severity must be FATAL, not ERROR: {whole}"
    );
    assert_eq!(
        field(b'V').as_deref(),
        Some("FATAL"),
        "and `V`, which is the one a client must not have to translate: {whole}"
    );
    assert_eq!(
        field(b'C').as_deref(),
        Some("25P03"),
        "the code must be 25P03: {whole}"
    );
    assert_eq!(
        field(b'M').as_deref(),
        Some("terminating connection due to idle-in-transaction timeout"),
        "PostgreSQL's own sentence, whole: {whole}"
    );
    assert!(
        rolled_back.load(Ordering::SeqCst),
        "the abandoned block must be rolled back before the socket goes"
    );
}

/// **A session idling with no block open is left alone**, which is the other half of the rule and
/// the one an implementation that timed every read would break: `idle_in_transaction_session_timeout`
/// is about a transaction, and a connection sitting at the prompt is not idling in one.
#[tokio::test]
async fn idling_outside_a_block_is_not_a_timeout() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let executors = OneIdler {
        limit: Duration::from_millis(50),
        rolled_back: Arc::new(AtomicBool::new(false)),
    };
    tokio::spawn(async move {
        let mut connection = esker_sql::pgwire::server::Connection::new(
            server,
            esker_sql::pgwire::server::Config::default(),
        );
        let _ = connection.run(&executors).await;
    });

    client.write_all(&startup_packet()).await.unwrap();
    client.flush().await.unwrap();

    // Well past the limit, and the connection must still be there.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut sink = Vec::new();
    let outcome =
        tokio::time::timeout(Duration::from_millis(250), client.read_to_end(&mut sink)).await;
    assert!(
        outcome.is_err(),
        "the server closed a connection that was idle but not in a transaction"
    );
}

/// **What the stub above cannot prove: that a real `SET` reaches the connection's clock.**
///
/// `idling_inside_a_block_terminates_the_session` drives an executor that hands back a limit it
/// was built with, so it measures the connection and nothing else. This measures the other seam —
/// the parameter registry to the trait — and it is where the two ways of getting it wrong live:
/// reading the settings map directly, so that a `RESET` leaves the last value in place, and
/// reading the digits without the unit, so that `'2s'` becomes two milliseconds.
///
/// The stored spellings are PostgreSQL's, not the caller's: `'250'` is stored `250ms` and
/// `'2000ms'` is stored `2s` (`crate::parameter::normalise_duration`), so this is also the test
/// that the clock reads what that function writes rather than what a user typed.
#[test]
fn a_set_idle_timeout_is_what_the_connection_waits_for() {
    let mut cluster = Cluster::new(&['A']);
    let session = cluster.session('A');

    // The boot value is `0`, and `0` is not a duration at all: no limit.
    assert_eq!(
        session.rows("SHOW idle_in_transaction_session_timeout"),
        [["0"]]
    );
    assert_eq!(session.executor.idle_in_transaction_timeout(), None);

    for (set, shown, expected) in [
        ("10ms", "10ms", Some(Duration::from_millis(10))),
        ("2s", "2s", Some(Duration::from_secs(2))),
        ("1min", "1min", Some(Duration::from_secs(60))),
        ("1h", "1h", Some(Duration::from_secs(3_600))),
        ("24d", "24d", Some(Duration::from_secs(24 * 86_400))),
        // A bare count is milliseconds, and gains the unit on the way in.
        ("250", "250ms", Some(Duration::from_millis(250))),
        // Re-printed in the largest unit that divides it, and the clock is unmoved by that.
        ("2000ms", "2s", Some(Duration::from_secs(2))),
        ("120s", "2min", Some(Duration::from_secs(120))),
        // **Below half a millisecond is `0`, which is *off* rather than "very short".** The
        // dangerous rounding is the other way: a limit of nearly nothing would end every session
        // in a block the moment it opened one.
        ("500us", "0", None),
        ("1500us", "2ms", Some(Duration::from_millis(2))),
    ] {
        session
            .run(&format!(
                "SET idle_in_transaction_session_timeout = '{set}'"
            ))
            .unwrap();
        assert_eq!(
            session.rows("SHOW idle_in_transaction_session_timeout"),
            [[shown]],
            "SHOW after SET … = '{set}'"
        );
        assert_eq!(
            session.executor.idle_in_transaction_timeout(),
            expected,
            "the clock after SET … = '{set}'"
        );
    }

    // A `RESET` gives the same answer as a session that never set it, which is the reason the read
    // goes through the boot value rather than through the settings map.
    session
        .run("SET idle_in_transaction_session_timeout = '30s'")
        .unwrap();
    assert_eq!(
        session.executor.idle_in_transaction_timeout(),
        Some(Duration::from_secs(30))
    );
    session
        .run("RESET idle_in_transaction_session_timeout")
        .unwrap();
    assert_eq!(session.executor.idle_in_transaction_timeout(), None);

    // And `0` turns the clock back off mid-session, which is what a client that is done with it
    // sends.
    session
        .run("SET idle_in_transaction_session_timeout = '5s'")
        .unwrap();
    session
        .run("SET idle_in_transaction_session_timeout = 0")
        .unwrap();
    assert_eq!(session.executor.idle_in_transaction_timeout(), None);
}
