//! READ COMMITTED: a writer waits for the writer in front of it
//! ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
//!
//! **This file starts with the test that would have caught the ADR's own hole**, and it is the red
//! test for the whole unit: the waiter's locker *commits*, the waiter waits, re-runs — and its own
//! `COMMIT` **succeeds**, with the row at 111. A version of this unit that implements the wait and
//! the statement re-run and nothing else passes every assertion up to that last one, because
//! first-committer-wins is measured against the transaction's `start_ts` and the locker's commit
//! landed after it. The failure would simply move from the `UPDATE` to the `COMMIT`.
//!
//! Measured on PostgreSQL 19 with two interleaved `psql` sessions: B's `UPDATE` returned 1.74 s
//! after it was sent, and `n + 100` over a row A had just moved from 10 to 11 gave **111**.
//!
//! # The barrier rule
//!
//! Every gate here is on a **transaction's edge** — A's write is buffered, A has committed — and
//! never on "the thread started". Every earlier racy test in this family was the second kind
//! (`docs/plans/phase-9-rails.md`, the flakes lane), and the thing under test here is precisely
//! what happens *between* two edges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::{Pair, edge, reached};

/// **The red test for the whole unit.** A holds the row, B blocks, A commits, B proceeds on A's
/// version — and B's own `COMMIT` succeeds.
///
/// The last assertion is the one a wait-and-re-run without a per-key read timestamp fails: B's
/// prewrite finds A's write at a `commit_ts` above B's `start_ts` and answers `40001` at the
/// commit instead of at the update (ADR 0057 §4).
#[test]
fn a_waiter_whose_locker_commits_commits_too_and_sees_the_new_row() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        b.run("BEGIN").unwrap();
        reached(&b_says, "B is about to write");
        // Blocks here on a real server, and must block here: A's lock is live.
        let update = b.run("UPDATE rc SET n = n + 100 WHERE id = 1");
        let commit = b.run("COMMIT");
        (update, commit)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET n = n + 1 WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    // B is inside its `UPDATE` now, or about to be. A commits, which is what releases it.
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT")
        .expect("A is the first writer: nothing may stop its commit");

    let (update, commit) = waiter.join().unwrap();
    update.expect("B's UPDATE must wait for A and then proceed, not fail");
    commit.expect("B's COMMIT must succeed: A's commit is B's input, not its conflict");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [["111"]],
        "the arithmetic is on A's committed version, not on the one B first read"
    );
}

/// **The locker aborts, and the waiter works from the row as it was.** Measured on PostgreSQL 19:
/// `n + 100` over a row A had moved to 11 and then rolled back gives **110**.
///
/// The pair to the test above, and the reason the re-run reads rather than replays: what the
/// waiter must use is whatever is *committed* when the wait ends, which is the original row here
/// and A's new one there. A design that remembered A's value and applied it would answer 111 to
/// both.
#[test]
fn a_waiter_whose_locker_aborts_works_from_the_row_as_it_was() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        b.run("BEGIN").unwrap();
        reached(&b_says, "B is about to write");
        let update = b.run("UPDATE rc SET n = n + 100 WHERE id = 1");
        let commit = b.run("COMMIT");
        (update, commit)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET n = n + 1 WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    a.run("ROLLBACK").unwrap();

    let (update, commit) = waiter.join().unwrap();
    update.expect("B waits for A and then proceeds");
    commit.expect("a rolled-back locker leaves nothing to conflict with");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [["110"]],
        "A's write never happened, so B's input is the row it first read"
    );
}

/// **`EvalPlanQual`: a row whose `WHERE` stops matching is skipped, not failed.**
///
/// A changes the column B's predicate tests. B waits, re-runs, and its `WHERE` no longer selects
/// the row — so B updates *nothing* and says so. Measured: `n` stayed 10 and the `UPDATE` reported
/// no rows.
///
/// This is the row that says the re-run is a re-**evaluation** and not a retry of a decision already
/// already made. An implementation that waited and then applied the update it had planned would write a
/// row its own `WHERE` no longer matches.
#[test]
fn a_row_that_stops_matching_is_skipped_by_the_waiter() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint, tag text)",
        "INSERT INTO rc (id, n, tag) VALUES (1, 10, 'a')",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        b.run("BEGIN").unwrap();
        reached(&b_says, "B is about to write");
        let update = b.run("UPDATE rc SET n = n + 100 WHERE tag = 'a'");
        let commit = b.run("COMMIT");
        (update, commit)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET tag = 'moved' WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT").unwrap();

    let (update, commit) = waiter.join().unwrap();
    update.expect("a row that no longer qualifies is skipped, not an error");
    commit.expect("B wrote nothing, so it has nothing to conflict over");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n, tag FROM rc WHERE id = 1"),
        [["10", "moved"]],
        "B's WHERE stopped matching, so B left the row alone"
    );
}

/// **What protects the keys an earlier statement wrote is the lock, not the timestamp** — which
/// is a stronger answer than ADR 0057 §4 asked for, and worth writing down because the ADR's own
/// test list asked for the weaker one.
///
/// §4 worried that a fresh read timestamp might become a licence to lose an update: a transaction
/// writes row 1, then waits on row 2 and re-runs, and a third transaction commits row 1 while it
/// waited. The per-key rule answers that — row 1 keeps its own, older stamp — but with row locks
/// taken at the statement (unit 1) the situation **cannot arise at all**: row 1 is locked from the
/// moment it is written until the transaction ends, so nobody else can commit it in the middle.
///
/// So the assertion is the lock's: a third session that tries waits, and says so.
#[test]
fn a_key_an_earlier_statement_wrote_is_held_until_the_transaction_ends() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10), (2, 20)",
    ]);
    let (b_says, hears_b) = channel();
    let (c_says, hears_c) = channel();

    let mut b = pair.session();
    let holder = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        b.run("UPDATE rc SET n = n + 100 WHERE id = 1").unwrap();
        reached(&b_says, "B has written row 1");
        edge(&hears_c, "C has tried and failed to take row 1");
        b.run("COMMIT").unwrap();
    });

    edge(&hears_b, "B has written row 1");
    let mut c = pair.session();
    // Bounded, because the point is that this *waits*: with `lock_timeout` at PostgreSQL's own
    // default it would wait until B ended, which is exactly the guarantee under test.
    c.run("SET lock_timeout = '150ms'").unwrap();
    let blocked = c
        .run("UPDATE rc SET n = 999 WHERE id = 1")
        .expect_err("row 1 is B's until B ends");
    assert_eq!(
        blocked.sqlstate(),
        "55P03",
        "a third session waits for a row an open transaction wrote: {blocked}"
    );
    reached(&c_says, "C has tried and failed to take row 1");
    holder.join().unwrap();

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [["110"]],
        "B's value stands: C never got the row"
    );
}

/// **`REPEATABLE READ` does not wait: it answers `40001`.** The level that already worked before
/// ADR 0057 keeps working, which is the half of the change that cannot regress.
#[test]
fn a_repeatable_read_writer_does_not_wait_it_conflicts() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        b.run("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        reached(&b_says, "B is about to write");
        let update = b.run("UPDATE rc SET n = n + 100 WHERE id = 1");
        let _ = b.run("ROLLBACK");
        update
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET n = n + 1 WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT").unwrap();

    let error = waiter
        .join()
        .unwrap()
        .expect_err("a repeatable-read writer meets a lock and loses rather than waiting");
    assert_eq!(error.sqlstate(), "40001", "{error}");
}

/// **Each statement re-snapshots under `READ COMMITTED` and the transaction's is fixed under
/// `REPEATABLE READ`.** Measured on PostgreSQL 19 as `10` then `99`, and `10` then `10`.
///
/// This is the half of READ COMMITTED that is about *reads* rather than about waiting, and the two
/// levels are asserted side by side because a node that got only one of them right would look
/// correct in whichever test was written first.
#[test]
fn a_statement_re_snapshots_under_read_committed_and_not_under_repeatable_read() {
    for (level, expected) in [
        ("BEGIN", "99"),
        ("BEGIN ISOLATION LEVEL REPEATABLE READ", "10"),
    ] {
        let pair = Pair::new(&[
            "CREATE TABLE rc (id bigint primary key, n bigint)",
            "INSERT INTO rc (id, n) VALUES (1, 10)",
        ]);
        let mut reader = pair.session();
        reader.run(level).unwrap();
        assert_eq!(
            reader.rows("SELECT n FROM rc WHERE id = 1"),
            [["10"]],
            "before, at {level}"
        );

        let mut writer = pair.session();
        writer.run("UPDATE rc SET n = 99 WHERE id = 1").unwrap();

        assert_eq!(
            reader.rows("SELECT n FROM rc WHERE id = 1"),
            [[expected]],
            "after another session committed, at {level}"
        );
        reader.run("COMMIT").unwrap();
    }
}

/// **The level is a session setting under four spellings**, and they agree.
#[test]
fn the_isolation_level_is_a_session_setting() {
    let pair = Pair::new(&[]);
    let mut node = pair.session();

    // PostgreSQL's own default, and this node's.
    assert_eq!(
        node.rows("SHOW transaction_isolation"),
        [["read committed"]]
    );
    assert_eq!(
        node.rows("SHOW default_transaction_isolation"),
        [["read committed"]]
    );

    // `SET TRANSACTION ISOLATION LEVEL` inside a block, undone when the block ends — which is what
    // makes it the *transaction's* and not the session's.
    node.run("BEGIN").unwrap();
    node.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    assert_eq!(node.rows("SHOW transaction_isolation"), [["serializable"]]);
    node.run("COMMIT").unwrap();
    assert_eq!(
        node.rows("SHOW transaction_isolation"),
        [["read committed"]],
        "a level set inside a block ends with it"
    );

    // The session default seeds each new transaction.
    node.run("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.rows("SHOW transaction_isolation"),
        [["repeatable read"]],
        "default_transaction_isolation is where a transaction starts"
    );
    node.run("COMMIT").unwrap();

    // And `READ UNCOMMITTED` is `READ COMMITTED`, as it is on a real server: there is no weaker
    // level to give it.
    node.run("SET default_transaction_isolation = 'read uncommitted'")
        .unwrap();
    node.run("BEGIN").unwrap();
    assert!(matches!(
        node.rows("SHOW transaction_isolation")[0][0].as_str(),
        "read uncommitted" | "read committed"
    ));
    node.run("COMMIT").unwrap();
}

/// **A statement with no transaction block of its own waits too — and its wait must not reach the
/// client.**
///
/// The restart loop ADR 0057 built lives in the *open-block* branch, so every test above sends a
/// `BEGIN` before the statement that waits. `ActiveRecord` does not: `update_attribute`,
/// `increment!` and `touch` are single statements in autocommit, and two workers touching one row
/// is the ordinary case rather than the exotic one.
///
/// What a client saw is the signal itself. `SqlError::StatementMustRestart`'s own comment says it
/// "reaches a client only if something forgot to catch it, which is exactly an internal error" —
/// and `XX000` is what an autocommit writer got the moment the transaction in front of it
/// committed. PostgreSQL answers `UPDATE 1`.
#[test]
fn a_waiter_with_no_block_of_its_own_waits_and_then_writes() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        reached(&b_says, "B is about to write");
        // No `BEGIN`: one statement, its own transaction, and it blocks on A's lock exactly as a
        // statement inside a block does.
        b.run("UPDATE rc SET n = n + 100 WHERE id = 1")
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET n = n + 1 WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT").unwrap();

    waiter
        .join()
        .unwrap()
        .expect("an autocommit UPDATE must wait for A and then write, not report a signal");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [["111"]],
        "the arithmetic is on A's committed version"
    );
}
