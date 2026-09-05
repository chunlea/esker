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
