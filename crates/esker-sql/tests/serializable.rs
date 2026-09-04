//! SERIALIZABLE: snapshot isolation plus a validated read set
//! ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md)).
//!
//! Snapshot isolation refuses dirty reads, non-repeatable reads and lost updates, and permits
//! **write skew**: two transactions each read what the other is about to invalidate, neither writes
//! a key the other writes, and first-committer-wins has no opinion. This file is the two shapes
//! that has — one over rows that exist, one over a row that does not exist yet — plus the case that
//! must **not** become a conflict, because a level that refuses everything is not serializable, it
//! is broken.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;

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
