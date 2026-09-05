//! A running statement has to be stoppable, and the three ways a client asks.
//!
//! `adapters/postgresql/transaction_test.rb`'s three failures look like three missing functions and
//! are one mechanism. Each has a thread take a row lock (`Sample.lock.find` — `SELECT … FOR
//! UPDATE`), blocks the main thread behind it, and then interrupts the wait a different way:
//! `statement_timeout`, `pg_cancel_backend()`, and the protocol's `CancelRequest`. `pg_sleep()` is
//! not wanted for itself; it is how the third test makes a statement long enough to cancel.
//!
//! **`57014`, not `55P03`.** All three assert `ActiveRecord::QueryCanceled`, which is `57014` —
//! "this statement was cancelled" — where `55P03` says "a lock was not available". A wait bounded
//! by both parameters therefore has to report *which* one fired.
//!
//! **The order these land in is not a preference.** `parameter.rs` records that refusing a non-zero
//! `statement_timeout` was added because its *absence* was measured as a **hang, not a wrong
//! answer**: a client that holds a 150 ms cancellation waits for one, and this very file waited
//! twenty minutes. So cancellation works first and the parameter is accepted second; accepting it
//! earlier turns three clean failures into a twenty-minute stall in every run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Pair;

/// **A wait bounded by `statement_timeout` answers `57014`.**
///
/// The same interleaving the `lock_timeout` tests use, with the other parameter — so the only thing
/// that differs from a passing `55P03` test is which parameter was set, and the only thing that may
/// differ in the answer is the code. That is what makes this a test of the provenance rather than
/// of the wait.
#[test]
fn a_wait_that_runs_out_of_statement_timeout_is_57014() {
    let pair = Pair::new(&[
        "CREATE TABLE t (id bigint primary key, v bigint)",
        "INSERT INTO t VALUES (1, 0)",
    ]);

    let mut holder = pair.session();
    holder.run("BEGIN").unwrap();
    holder.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();

    let mut waiter = pair.session();
    waiter.run("SET statement_timeout = '150ms'").unwrap();
    let cancelled = waiter
        .run("UPDATE t SET v = 2 WHERE id = 1")
        .expect_err("the holder never commits, so the wait must be cut short");
    assert_eq!(cancelled.sqlstate(), "57014", "{cancelled}");

    holder.run("ROLLBACK").unwrap();
}

/// **And `lock_timeout` still answers `55P03`** — the regression guard for the change above, since
/// the cheap way to make the test above pass is to answer `57014` for both.
#[test]
fn a_wait_that_runs_out_of_lock_timeout_is_still_55p03() {
    let pair = Pair::new(&[
        "CREATE TABLE t (id bigint primary key, v bigint)",
        "INSERT INTO t VALUES (1, 0)",
    ]);

    let mut holder = pair.session();
    holder.run("BEGIN").unwrap();
    holder.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();

    let mut waiter = pair.session();
    waiter.run("SET lock_timeout = '150ms'").unwrap();
    let refused = waiter
        .run("UPDATE t SET v = 2 WHERE id = 1")
        .expect_err("the holder never commits");
    assert_eq!(refused.sqlstate(), "55P03", "{refused}");

    holder.run("ROLLBACK").unwrap();
}

/// **`pg_sleep` sleeps**, which is the only reason this node wants it: it is how a test makes a
/// statement that is *working* rather than waiting, so that cancelling one can be tested at all.
#[test]
fn pg_sleep_takes_about_as_long_as_it_is_asked_to() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();

    let started = Instant::now();
    session.rows("SELECT pg_sleep(0.2)");
    let took = started.elapsed();

    // A floor and no ceiling: a loaded box may take much longer and that is not this test's
    // business, but returning early would mean it did not sleep at all.
    assert!(
        took >= Duration::from_millis(180),
        "pg_sleep(0.2) returned after {took:?}, so it did not sleep"
    );
}

/// **The mechanism the other two only imply: a statement that is *working* is cancellable.**
///
/// A row wait is a loop the SQL layer drives, and `lock_timeout` was allowed years before this
/// because of that. `pg_sleep` is the opposite case — a statement doing something — and if the
/// deadline is only checked in the wait loop this test hangs for five seconds and then returns a
/// row, rather than answering `57014`. It is the test that says the cancellation is general.
#[test]
fn statement_timeout_cancels_a_statement_that_is_working() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("SET statement_timeout = '150ms'").unwrap();

    let started = Instant::now();
    let cancelled = session
        .run("SELECT pg_sleep(5)")
        .expect_err("five seconds is far past the deadline");
    assert_eq!(cancelled.sqlstate(), "57014", "{cancelled}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "it answered 57014 only after the sleep finished, which is not cancellation"
    );
}

/// **The whole Rails mechanism, with a simpler victim.**
///
/// `transaction_test.rb` finds its target with `SELECT pid FROM pg_stat_activity WHERE query LIKE
/// '% FOR UPDATE'` and cancels it. That query could not have worked before: the view returned one
/// row whose pid was `std::process::id()` — the same number for every session — with `query` NULL,
/// so the `LIKE` matched nothing and there was no pid to pass. This asserts the three halves of
/// that working: **a row per session**, **the statement text in it**, and **a cancel that lands**.
///
/// `pg_sleep` rather than a row lock because the victim only has to be busy, and a sleep is busy
/// without a second transaction to arrange.
#[test]
fn one_session_finds_another_in_pg_stat_activity_and_cancels_it() {
    let pair = Pair::new(&[]);

    let mut victim = pair.session();
    let sleeping = std::thread::spawn(move || victim.run("SELECT pg_sleep(10)"));

    let mut hunter = pair.session();
    // The victim has to have started before it can be found; poll rather than sleep a guess.
    let mut pid = None;
    for _ in 0..200 {
        // **`AND pid <> pg_backend_pid()`, and it is not decoration.** This very query contains
        // the text `pg_sleep`, so it matches *itself* — the first version of this test cancelled
        // the hunter and answered `57014` from the `SELECT pg_cancel_backend(...)` line. It passed
        // alone, because the victim happened to hold the lower pid and sorted first, and failed
        // only in the full suite when the order flipped. Excluding yourself is why a real server
        // has `pg_backend_pid()`.
        let rows = hunter.rows(
            "SELECT pid FROM pg_stat_activity              WHERE query LIKE '%pg_sleep%' AND pid <> pg_backend_pid()",
        );
        if let Some(found) = rows.first().and_then(|row| row.first()) {
            pid = Some(found.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = pid.expect("the sleeping session must appear in pg_stat_activity with its statement");

    assert_eq!(
        hunter.rows(&format!("SELECT pg_cancel_backend({pid})")),
        [["t".to_owned()]],
        "the pid was found, so there was a session to ask"
    );

    let started = Instant::now();
    let stopped = sleeping
        .join()
        .unwrap()
        .expect_err("the sleep was cancelled, so the statement is an error");
    assert_eq!(stopped.sqlstate(), "57014", "{stopped}");
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "it ended only when the sleep did, which is not a cancellation"
    );
}

/// **Cancelling yourself stops the statement that asked** — measured against PG19, where
/// `SELECT pg_cancel_backend(pg_backend_pid())` answers `canceling statement due to user request`
/// rather than returning a row.
#[test]
fn a_session_can_cancel_itself_and_the_statement_is_the_one_that_dies() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();

    // **`pg_backend_pid()`, not `LIMIT 1`.** The view lists every session in the process now, so
    // the first row is whichever session has the lowest pid — in a test binary, somebody else's.
    // That is the whole reason a real server has this function, and the first draft of this test
    // cancelled a stranger and passed for the wrong reason.
    let pid = session.rows("SELECT pg_backend_pid()")[0][0].clone();
    let cancelled = session
        .run(&format!("SELECT pg_cancel_backend({pid})"))
        .expect_err("asking yourself to stop stops the asking statement");
    assert_eq!(cancelled.sqlstate(), "57014", "{cancelled}");

    // And the session is still usable afterwards: a cancel ends a statement, not a connection.
    assert_eq!(session.rows("SELECT 1"), [["1".to_owned()]]);
}

/// A pid nobody holds is `false`, not an error.
///
/// PostgreSQL also raises `WARNING: PID 999999 is not a PostgreSQL backend process` beside it;
/// this node does not, which is a declared divergence recorded on `CatalogFunc::PgCancelBackend`.
/// The boolean, which is what a caller branches on, is the same.
#[test]
fn cancelling_a_pid_that_is_not_here_is_false() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    assert_eq!(
        session.rows("SELECT pg_cancel_backend(999999)"),
        [["f".to_owned()]]
    );
}
