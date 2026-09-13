//! SERIALIZABLE: snapshot isolation plus a validated read set
//! ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md)).
//!
//! Snapshot isolation refuses dirty reads, non-repeatable reads and lost updates, and permits
//! **write skew**: two transactions each read what the other is about to invalidate, neither writes
//! a key the other writes, and first-committer-wins has no opinion. This file is the two shapes
//! that has — one over rows that exist, one over a row that does not exist yet — plus the case that
//! must **not** become a conflict, because a level that refuses everything is not serializable, it
//! is broken.
//!
//! **And a lost race on a unique key**
//! ([ADR 0114](../../../docs/adr/0114-a-unique-key-being-written-waits-at-read-committed.md) §3): the
//! code a transaction that loses one is refused with — `40001` where what it read has moved under it,
//! `23505` where it only collided — at SERIALIZABLE, and for an `ON CONFLICT` arbiter's key at
//! REPEATABLE READ as well. The sequences are `unique_race`'s, shared with
//! `concurrent_unique_insert.rs`, which runs the same ones against three real stores.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;
mod unique_race;

use parity::{Pair, edge, reached};

/// Runs both sides to the edge of their commits, then commits them in order, and answers what each
/// `COMMIT` said.
///
/// The interleaving is the whole point: both transactions must have **read** before either commits,
/// or there is no skew to catch.
fn both_then_commit(
    fixture: &[&str],
    a_sql: &[&str],
    b_sql: &[&str],
) -> (esker_sql::Result<()>, esker_sql::Result<()>) {
    let pair = Pair::new(fixture);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let a_sql: Vec<String> = a_sql.iter().map(|s| (*s).to_owned()).collect();
    let b_sql: Vec<String> = b_sql.iter().map(|s| (*s).to_owned()).collect();

    let mut b = pair.session();
    let second = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        b.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        for sql in &b_sql {
            b.run(sql).unwrap();
        }
        reached(&b_says, "B has read and written");
        edge(&hears_a, "A has committed");
        b.run("COMMIT").map(|_| ())
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    for sql in &a_sql {
        a.run(sql).unwrap();
    }
    // Both have read at their own snapshots before either commits.
    edge(&hears_b, "B has read and written");
    let first = a.run("COMMIT").map(|_| ());
    reached(&a_says, "A has committed");
    let second = second.join().unwrap();
    std::thread::sleep(Duration::from_millis(10));
    (first, second)
}

/// **Write skew over rows that exist.** The doctors, and the shape ADR 0062 opens with.
///
/// Each transaction reads both rows, sees that two are on duty, and takes one of them off. Neither
/// writes a key the other writes, so snapshot isolation commits both and nobody is on call.
/// PostgreSQL under SERIALIZABLE commits exactly one.
#[test]
fn write_skew_over_rows_that_exist_loses_one_of_the_two() {
    let (first, second) = both_then_commit(
        &[
            "CREATE TABLE on_call (name text primary key, duty boolean)",
            "INSERT INTO on_call VALUES ('alice', true), ('bob', true)",
        ],
        &[
            "SELECT count(*) FROM on_call WHERE duty",
            "UPDATE on_call SET duty = false WHERE name = 'alice'",
        ],
        &[
            "SELECT count(*) FROM on_call WHERE duty",
            "UPDATE on_call SET duty = false WHERE name = 'bob'",
        ],
    );
    first.expect("the first committer always wins");
    let refused = second.expect_err("the second read what the first wrote and must not commit");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
}

/// **Write skew over a row that does not exist yet — a phantom.**
///
/// Each transaction counts the rows in a range and inserts one more into it. The keys they write
/// are different and the keys they *read* did not include each other's, because neither row existed
/// when they read. Only a range recorded as a range can catch this.
#[test]
fn a_phantom_in_a_range_two_transactions_read_is_a_conflict() {
    let (first, second) = both_then_commit(
        &[
            "CREATE TABLE bookings (id bigint primary key, room bigint)",
            "INSERT INTO bookings VALUES (1, 7)",
        ],
        &[
            "SELECT count(*) FROM bookings WHERE id BETWEEN 1 AND 100",
            "INSERT INTO bookings VALUES (50, 7)",
        ],
        &[
            "SELECT count(*) FROM bookings WHERE id BETWEEN 1 AND 100",
            "INSERT INTO bookings VALUES (60, 7)",
        ],
    );
    first.expect("the first committer always wins");
    let refused = second.expect_err("B's insert landed in a range A read; one of them must fail");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
}

/// **What must not become a conflict.** Two transactions that read and write different rows both
/// commit, and a level that refused this would be unusable rather than strict.
#[test]
fn two_transactions_over_different_rows_both_commit() {
    let (first, second) = both_then_commit(
        &[
            "CREATE TABLE apart (id bigint primary key, n bigint)",
            "INSERT INTO apart VALUES (1, 0), (2, 0)",
        ],
        &[
            "SELECT n FROM apart WHERE id = 1",
            "UPDATE apart SET n = 1 WHERE id = 1",
        ],
        &[
            "SELECT n FROM apart WHERE id = 2",
            "UPDATE apart SET n = 2 WHERE id = 2",
        ],
    );
    first.expect("A touched only row 1");
    second.expect("B touched only row 2, and read nothing A wrote");
}

/// **The level is what decides.** The same interleaving under READ COMMITTED commits both, which is
/// snapshot isolation's answer and must not change: this ADR buys a guarantee for the transactions
/// that ask for it and costs the others nothing.
#[test]
fn the_same_skew_under_read_committed_commits_both() {
    let pair = Pair::new(&[
        "CREATE TABLE on_call (name text primary key, duty boolean)",
        "INSERT INTO on_call VALUES ('alice', true), ('bob', true)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let second = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        b.rows("SELECT count(*) FROM on_call WHERE duty");
        b.run("UPDATE on_call SET duty = false WHERE name = 'bob'")
            .unwrap();
        reached(&b_says, "B has read and written");
        edge(&hears_a, "A has committed");
        b.run("COMMIT").map(|_| ())
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.rows("SELECT count(*) FROM on_call WHERE duty");
    a.run("UPDATE on_call SET duty = false WHERE name = 'alice'")
        .unwrap();
    edge(&hears_b, "B has read and written");
    a.run("COMMIT").unwrap();
    reached(&a_says, "A has committed");
    second
        .join()
        .unwrap()
        .expect("READ COMMITTED is snapshot isolation here and commits both");
}

/// **The same skew with a savepoint open**, which is what `ActiveRecord` writes for every nested
/// `transaction do` block.
///
/// A statement inside a savepoint runs through `savepoint::Recording`, a `Txn` that wraps the real
/// one — and a trait method it does not forward is a method that statement does not really call.
/// `validate_reads` defaulted to doing nothing, so a SERIALIZABLE transaction with a savepoint open
/// recorded nothing and validated nothing. That is the third method to be missed this way, after
/// `lock` in unit 1, and the reason this test exists rather than a note.
#[test]
fn write_skew_is_caught_with_a_savepoint_open() {
    let pair = Pair::new(&[
        "CREATE TABLE nested (name text primary key, duty boolean)",
        "INSERT INTO nested VALUES ('alice', true), ('bob', true)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let second = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        b.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        b.run("SAVEPOINT inner_one").unwrap();
        b.rows("SELECT count(*) FROM nested WHERE duty");
        b.run("UPDATE nested SET duty = false WHERE name = 'bob'")
            .unwrap();
        reached(&b_says, "B has read and written");
        edge(&hears_a, "A has committed");
        b.run("COMMIT").map(|_| ())
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    a.run("SAVEPOINT inner_one").unwrap();
    a.rows("SELECT count(*) FROM nested WHERE duty");
    a.run("UPDATE nested SET duty = false WHERE name = 'alice'")
        .unwrap();
    edge(&hears_b, "B has read and written");
    a.run("COMMIT").unwrap();
    reached(&a_says, "A has committed");

    let refused = second
        .join()
        .unwrap()
        .expect_err("a savepoint must not turn validation off");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
}

/// **A write to a row committed since this transaction's snapshot fails at the statement, not at
/// the commit** — under REPEATABLE READ and SERIALIZABLE, which keep one snapshot for their whole
/// life.
///
/// PostgreSQL raises `40001` from the `UPDATE` itself: the row it would write is newer than the
/// snapshot it can see, and there is no re-read available to a level that may not take one. This
/// node deferred it to the commit, where the per-key check found it — the same code, from a
/// statement the client had already been told succeeded.
///
/// Measured against the suite rather than reasoned:
/// `transaction_nested_test.rb`'s *"`SerializationFailure` inside nested `SavepointTransaction` is
/// recoverable"* asserts the raise around the **inner** block, so a `40001` at `COMMIT` arrives
/// after the assertion has already failed. That test is why this exists, and it is the shape the
/// savepoint-forward fix did **not** close.
#[test]
fn a_repeatable_read_write_over_a_newer_commit_fails_at_the_statement() {
    let pair = Pair::new(&[
        "CREATE TABLE snap (id bigint primary key, n bigint)",
        "INSERT INTO snap VALUES (1, 1)",
    ]);

    let mut held = pair.session();
    held.run("BEGIN").unwrap();
    held.run("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    // The snapshot is taken here, and this level keeps it.
    assert_eq!(held.rows("SELECT n FROM snap WHERE id = 1"), [["1"]]);

    // Another session commits the row, and finishes: nothing is held by the time we write.
    let mut other = pair.session();
    other.run("UPDATE snap SET n = 2 WHERE id = 1").unwrap();

    let refused = held
        .run("UPDATE snap SET n = 4 WHERE id = 1")
        .expect_err("the row is newer than this transaction's snapshot");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
    held.run("ROLLBACK").unwrap();

    // And READ COMMITTED, which may re-read, still proceeds: it waits for nobody, re-runs on the
    // committed value and writes 4 over 2.
    let mut fresh = pair.session();
    fresh.run("BEGIN").unwrap();
    fresh.rows("SELECT n FROM snap WHERE id = 1");
    let mut third = pair.session();
    third.run("UPDATE snap SET n = 3 WHERE id = 1").unwrap();
    fresh
        .run("UPDATE snap SET n = 4 WHERE id = 1")
        .expect("READ COMMITTED re-reads rather than refusing");
    fresh.run("COMMIT").unwrap();
    assert_eq!(fresh.rows("SELECT n FROM snap WHERE id = 1"), [["4"]]);
}

impl unique_race::Sql for parity::Node {
    fn sql(&mut self, sql: &str) -> esker_sql::Result<esker_sql::pgwire::session::Outcome> {
        self.run(sql)
    }
}

/// One of ADR 0114 §3's races on a fresh `MemoryBackend`: three sessions of one node.
fn run_race(race: unique_race::Race) {
    let pair = Pair::new(&unique_race::SUBSCRIBERS);
    unique_race::assert_refused_as_postgres(
        &mut pair.session(),
        &mut pair.session(),
        &mut pair.session(),
        race,
    );
}

/// **Case 09: SERIALIZABLE, B read `bob`, and A committed it before B's `INSERT`** — `40001`, because
/// what B read has moved under it. The rows of `unique_race`'s table, one test each, as
/// `concurrent_unique_insert.rs` has them against real stores.
#[test]
fn serializable_refuses_a_unique_key_committed_after_it_was_read_with_40001() {
    run_race(unique_race::CASE_09);
}

/// Case 07: SERIALIZABLE, B never read `bob` — `23505`, the control for case 09.
#[test]
fn serializable_after_no_read_is_a_duplicate_key() {
    run_race(unique_race::CASE_07);
}

/// Case 13: SERIALIZABLE `ON CONFLICT DO NOTHING` after a `count(*)` of `bob` — `40001`.
#[test]
fn serializable_on_conflict_do_nothing_after_a_count_is_refused_with_40001() {
    run_race(unique_race::CASE_13);
}

/// Case 13b: the same after Rails' `find_by` — `40001`.
#[test]
fn serializable_on_conflict_do_nothing_after_a_find_by_is_refused_with_40001() {
    run_race(unique_race::CASE_13B);
}

/// Case 14: SERIALIZABLE `ON CONFLICT DO NOTHING`, B never read `bob` — `40001`: the arbiter read it.
#[test]
fn serializable_on_conflict_do_nothing_after_no_read_is_refused_with_40001() {
    run_race(unique_race::CASE_14);
}

/// Case 15: REPEATABLE READ `ON CONFLICT DO NOTHING` after a `count(*)` of `bob` — `40001`.
#[test]
fn repeatable_read_on_conflict_do_nothing_after_a_count_is_refused_with_40001() {
    run_race(unique_race::CASE_15);
}

/// Case 16: REPEATABLE READ `ON CONFLICT DO NOTHING`, B never read `bob` — `40001`.
#[test]
fn repeatable_read_on_conflict_do_nothing_after_no_read_is_refused_with_40001() {
    run_race(unique_race::CASE_16);
}

/// Case 04: REPEATABLE READ, a plain `INSERT` after a read, while A is live — `23505`.
#[test]
fn repeatable_read_insert_after_a_read_is_a_duplicate_key_while_the_holder_is_live() {
    run_race(unique_race::CASE_04);
}

/// Case 10: the same with A committed before B's `INSERT` — `23505`.
#[test]
fn repeatable_read_insert_after_a_read_is_a_duplicate_key_when_the_holder_committed_first() {
    run_race(unique_race::CASE_10);
}
