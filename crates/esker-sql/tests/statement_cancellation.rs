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

/// **Serialises this file under plain `cargo test`, which is where the nextest group does not
/// reach.**
///
/// `.config/nextest.toml` puts this binary in the `timed` group at `max-threads = 1`, and that is
/// the contract these tests are written against: every one of them is a claim about *when* — a
/// cancellation observed within a bound, a sleep that must not return early, a `lock_timeout` that
/// must fire before a holder releases — and a box running three thousand other tests beside them is
/// a box where "when" moves.
///
/// But the group only binds `cargo nextest`. Under `cargo test` these eight are **threads in one
/// process**, all racing each other for the clock, and a test that passes only under the runner the
/// author happened to use is a test that fails for the next person on a busy machine. So the
/// contract is enforced twice, and the two do not overlap: nextest gives each test its own process,
/// where this mutex is uncontended and free.
///
/// **Poisoning is deliberately ignored.** A panicking test would otherwise poison the mutex and
/// fail the other seven, turning one real failure into eight and hiding which one broke.
static CLOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Held for the body of a timing test. See [`CLOCK`].
fn alone() -> std::sync::MutexGuard<'static, ()> {
    CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// **A wait bounded by `statement_timeout` answers `57014`.**
///
/// The same interleaving the `lock_timeout` tests use, with the other parameter — so the only thing
/// that differs from a passing `55P03` test is which parameter was set, and the only thing that may
/// differ in the answer is the code. That is what makes this a test of the provenance rather than
/// of the wait.
#[test]
fn a_wait_that_runs_out_of_statement_timeout_is_57014() {
    let _alone = alone();
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
    let _alone = alone();
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
    let _alone = alone();
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
    let _alone = alone();
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
    let _alone = alone();
    let pair = Pair::new(&[]);

    let mut victim = pair.session();
    let sleeping = std::thread::spawn(move || victim.run("SELECT pg_sleep(10)"));

    let mut hunter = pair.session();
    // **Visibly `active`, not merely present.** A row can carry the query text of a statement that
    // has already finished — `pg_stat_activity` retains it, as a real server does — so matching on
    // the text alone can find a session that is idle and cancel nothing. The state column is what
    // says the statement is running *now*.
    let mut pid = None;
    for _ in 0..200 {
        // **`AND pid <> pg_backend_pid()`, and it is not decoration.** This very query contains
        // the text `pg_sleep`, so it matches *itself* — the first version of this test cancelled
        // the hunter and answered `57014` from the `SELECT pg_cancel_backend(...)` line. It passed
        // alone, because the victim happened to hold the lower pid and sorted first, and failed
        // only in the full suite when the order flipped. Excluding yourself is why a real server
        // has `pg_backend_pid()`.
        let rows = hunter.rows(
            "SELECT pid FROM pg_stat_activity WHERE query LIKE '%pg_sleep%' \
             AND state = 'active' AND pid <> pg_backend_pid()",
        );
        if let Some(found) = rows.first().and_then(|row| row.first()) {
            pid = Some(found.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = pid.expect("the sleeping session must appear in pg_stat_activity with its statement");

    // **Asked until it lands, not once — and that is the semantics, not a workaround.**
    // `pg_cancel_backend` is best-effort on a real server too: it *requests* a cancellation, and
    // PostgreSQL's own documentation says so. A test that requires one request to be observed is
    // asserting something no server promises, and this one did: it went red under a loaded gate
    // with the sleep running its full ten seconds.
    //
    // The bound is what keeps it a test. Each round re-asks and gives the victim a moment; if the
    // statement is still running after five seconds the loop stops asking and the assertion below
    // fails on a completed sleep, which is a report rather than a hang.
    let started = Instant::now();
    while !sleeping.is_finished() && started.elapsed() < Duration::from_secs(5) {
        assert_eq!(
            hunter.rows(&format!("SELECT pg_cancel_backend({pid})")),
            [["t".to_owned()]],
            "the pid was found, so there is a session to ask"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

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
    let _alone = alone();
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
    let _alone = alone();
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    assert_eq!(
        session.rows("SELECT pg_cancel_backend(999999)"),
        [["f".to_owned()]]
    );
}

/// **A statement blocked in a row-lock wait is cancellable too** — run 86's survivor.
///
/// `pg_sleep` polls `exec::cancel` between its steps, so a sleep stops. The row wait had its own
/// loop that watched only its deadline, so a cancel from another session set a flag nothing on that
/// path ever read: `pg_cancel_backend` answered `true` and the waiter went on waiting. That is what
/// `transaction_test.rb`'s last failure is —
/// `ActiveRecord::QueryCanceled expected but nothing was raised` — and it is the shape a real
/// application actually cancels: a statement stuck behind somebody else's lock.
#[test]
fn a_statement_waiting_for_a_row_lock_is_cancellable() {
    let _alone = alone();
    let pair = Pair::new(&[
        "CREATE TABLE t (id bigint primary key, v bigint)",
        "INSERT INTO t VALUES (1, 0)",
    ]);

    // The holder keeps the row for the whole test.
    let mut holder = pair.session();
    holder.run("BEGIN").unwrap();
    holder.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();

    let mut waiter = pair.session();
    let blocked = std::thread::spawn(move || {
        // **A `lock_timeout` far past the cancellation, so a failure is a failure.** Written
        // without one this test did not fail when the cancel was ignored — it *hung*, for 295
        // seconds, until nextest killed it. A bound turns "the cancel never landed" into a
        // `55P03` the assertion below can name, which is the difference between a test that
        // reports and a test that stops the suite.
        waiter.run("SET lock_timeout = '20s'").unwrap();
        waiter.run("UPDATE t SET v = 2 WHERE id = 1")
    });

    let mut hunter = pair.session();
    let mut pid = None;
    for _ in 0..300 {
        let rows = hunter.rows(
            "SELECT pid FROM pg_stat_activity \
             WHERE query LIKE '%SET v = 2%' AND state = 'active' AND pid <> pg_backend_pid()",
        );
        if let Some(found) = rows.first().and_then(|row| row.first()) {
            pid = Some(found.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = pid.expect("the blocked writer must be visible with its statement");

    // Asked until it lands, for the reason the sleeping test gives: a cancellation is a request,
    // and one request being observed is not something a server promises.
    let started = Instant::now();
    while !blocked.is_finished() && started.elapsed() < Duration::from_secs(10) {
        assert_eq!(
            hunter.rows(&format!("SELECT pg_cancel_backend({pid})")),
            [["t".to_owned()]]
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let stopped = blocked
        .join()
        .unwrap()
        .expect_err("the wait was cancelled, so the statement is an error");
    assert_eq!(
        stopped.sqlstate(),
        "57014",
        "a 55P03 here means the lock timeout ended the wait and the cancellation never did: \
         {stopped}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it ended only when the holder did, which is not a cancellation"
    );

    holder.run("ROLLBACK").unwrap();
}
