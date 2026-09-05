//! What a `ROLLBACK TO SAVEPOINT` has to undo, and the half it was not undoing.
//!
//! A savepoint's undo restores the **value** a key had at the mark, which is the right answer for
//! what the transaction can *see*. It is the wrong answer for what the transaction *writes*: a key
//! the subtransaction touched and nothing else did is still in the write buffer at `COMMIT`, so
//! first-committer-wins prewrites it and a concurrent commit on that key refuses the whole
//! transaction — for a write it no longer intends to make. PostgreSQL's aborted subtransaction
//! leaves no tuple behind and nothing to conflict with.
//!
//! Found in Rails' `transaction_nested_test.rb`, both of whose errors are this: the 40001 and the
//! 40P01 are raised not by the statement inside the savepoint, which the test expects and catches,
//! but by the **next thing the outer transaction does** — `transaction_nested_test.rb:104` is the
//! outer `Sample.transaction do … end`, so the error escapes `assert_raises` and the test errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Pair;

/// **A key written only inside a rolled-back savepoint must not be prewritten.**
///
/// The order is Rails': the other session finishes *before* this one touches the row, so nothing
/// ever waits on a lock — the conflict is discovered at prewrite, not at a wait. Inside the
/// savepoint the statement legitimately answers `40001` (that is the `assert_raises` in the Rails
/// test, and it is correct); the transaction then rolls back to the mark, and its `COMMIT` must
/// succeed, because after the rollback it has no interest in row 2 at all.
#[test]
fn a_row_written_only_inside_a_rolled_back_savepoint_does_not_conflict() {
    let pair = Pair::new(&[
        "CREATE TABLE t (id bigint primary key, v bigint)",
        "INSERT INTO t VALUES (1, 0), (2, 0)",
    ]);

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    // A real write before the mark, so the commit is not trivially empty.
    a.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();
    a.run("SAVEPOINT s1").unwrap();

    // Committed and finished before A goes near row 2: no lock is held, nothing waits.
    let mut b = pair.session();
    b.run("UPDATE t SET v = 9 WHERE id = 2").unwrap();

    let refused = a
        .run("UPDATE t SET v = 2 WHERE id = 2")
        .expect_err("the row moved under a SERIALIZABLE statement");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");

    a.run("ROLLBACK TO SAVEPOINT s1").unwrap();
    a.run("COMMIT")
        .expect("the savepoint's write was undone, so A has nothing on row 2 to conflict with");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT v FROM t ORDER BY id"),
        [["1".to_owned()], ["9".to_owned()]],
        "A's row 1 committed and B's row 2 stands"
    );
}

/// **A row lock taken inside a rolled-back savepoint is given back with it** — Rails' *other*
/// error, and the same defect wearing a different coat.
///
/// In `transaction_nested_test.rb` the deadlock test takes `s1.lock!` inside the nested block,
/// rescues the `Deadlocked`, and then runs `s2.update value: 10` in the outer transaction
/// (`transaction_nested_test.rb:187`, which is where the second escaping error is raised). If the
/// subtransaction's locks outlive its rollback, that next statement is still standing in a queue
/// the transaction has already left.
///
/// **`lock_timeout` is what makes this a failure instead of a hang.** Written without it, this test
/// wedged for 237 seconds and had to be killed — which is the bug, but a test that never returns
/// reports nothing and costs an agent.
#[test]
fn a_lock_taken_inside_a_rolled_back_savepoint_is_released() {
    let pair = Pair::new(&[
        "CREATE TABLE t (id bigint primary key, v bigint)",
        "INSERT INTO t VALUES (1, 0)",
    ]);

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();
    a.run("SAVEPOINT s1").unwrap();
    a.run("INSERT INTO t VALUES (3, 3)").unwrap();
    a.run("ROLLBACK TO SAVEPOINT s1").unwrap();

    // A has rolled back the only statement that touched row 3, so B must not queue behind it.
    let mut b = pair.session();
    b.run("SET lock_timeout = '2s'").unwrap();
    b.run("INSERT INTO t VALUES (3, 30)")
        .expect("row 3 belongs to nobody: A's insert was rolled back with the savepoint");

    a.run("COMMIT").expect("A writes row 1 only");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT v FROM t ORDER BY id"),
        [["1".to_owned()], ["30".to_owned()]],
        "row 3 is B's; A's rolled-back insert left neither a lock nor a tombstone"
    );
}
