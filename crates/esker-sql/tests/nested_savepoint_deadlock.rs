//! **A deadlock inside a savepoint leaves the transaction that lost it usable.**
//!
//! `transaction_nested_test.rb`'s *deadlock inside nested `SavepointTransaction` is recoverable*.
//! Two sessions each open a block, take a savepoint, lock one row, and then write the other's —
//! so one of them is the victim. What the test asserts is what happens **after** the `40P01`:
//! `ROLLBACK TO SAVEPOINT`, then the *same* block writes and commits, and both rows end at 10.
//!
//! ```text
//! A: BEGIN; SAVEPOINT sp; SELECT … id=1 FOR UPDATE;   B: BEGIN; SAVEPOINT sp; SELECT … id=2 FOR UPDATE;
//!                              ── both sides ready ──
//! A: UPDATE … id=2                                    B: UPDATE … id=1
//!                    one gets 40P01, the other's write goes through
//! victim: ROLLBACK TO SAVEPOINT sp; UPDATE <the same row> = 10; COMMIT
//! winner:                           UPDATE <the same row> = 10; COMMIT
//! ```
//!
//! The node raises the deadlock at the right moment and against the right session — r1 measured
//! that against PostgreSQL 19 and the two agree. The divergence is the recovery, and the code said
//! so before this test did: the deadlock arm gives every lock back with `abandon_locks`, and its
//! own comment ended "what this does *not* copy is a real server's lock lifetime under
//! `ROLLBACK TO SAVEPOINT` recovery — declared".

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Pair;

/// Both sides arrive before either goes on, which is `Concurrent::CyclicBarrier.new(2)`.
fn meet(mine: &Sender<&'static str>, theirs: &Receiver<&'static str>, what: &'static str) {
    mine.send(what).unwrap();
    theirs
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| panic!("the other session never reached `{what}`: {error}"));
}

#[test]
fn a_victim_recovers_through_its_savepoint_and_commits() {
    let pair = Pair::new(&[
        "CREATE TABLE samples (id bigint primary key, value bigint)",
        "INSERT INTO samples VALUES (1, 1), (2, 2)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    // One session's whole part, mirrored: lock `mine`, then write `theirs`.
    let sessions = pair.sessions();
    let other = std::thread::spawn(move || {
        let mut node = sessions.session();
        run_half(&mut node, 2, 1, &b_says, &hears_a)
    });
    let mut node = pair.session();
    let first = run_half(&mut node, 1, 2, &a_says, &hears_b);
    let second = other.join().unwrap();

    assert_eq!(
        usize::from(first) + usize::from(second),
        1,
        "exactly one of the two is the victim: {first} and {second}"
    );
    // **Both blocks committed**, which is the whole assertion: the victim's `40P01` ended its
    // savepoint and not its transaction.
    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT id, value FROM samples ORDER BY id"),
        vec![
            vec!["1".to_owned(), "10".to_owned()],
            vec!["2".to_owned(), "10".to_owned()],
        ]
    );
}

/// Returns whether this session was the deadlock's victim.
fn run_half(
    node: &mut parity::Node,
    mine: i64,
    theirs: i64,
    says: &Sender<&'static str>,
    hears: &Receiver<&'static str>,
) -> bool {
    node.run("BEGIN").unwrap();
    // Far past anything here, so a deadlock that is never detected is a named refusal rather than
    // a suite that hangs.
    node.run("SET lock_timeout = '20s'").unwrap();
    node.run("SAVEPOINT sp").unwrap();
    node.run(&format!(
        "SELECT value FROM samples WHERE id = {mine} FOR UPDATE"
    ))
    .unwrap();
    meet(says, hears, "locked my row");
    let victim = match node.run(&format!("UPDATE samples SET value = 4 WHERE id = {theirs}")) {
        Ok(_) => false,
        Err(error) => {
            assert_eq!(
                error.sqlstate(),
                esker_sql::sqlstate::DEADLOCK_DETECTED,
                "the only failure here is the deadlock: {error}"
            );
            // What `rescue ActiveRecord::Deadlocked` does, and the whole of what this test is
            // about: the savepoint ends, the block does not.
            node.run("ROLLBACK TO SAVEPOINT sp")
                .expect("the block must be usable again");
            true
        }
    };
    // **The other row, which is the one this half just failed to write.** The Rails test writes
    // `s2` from the thread that locked `s1`, both inside the savepoint and again after it — so the
    // winner's own `4` is overwritten by its own `10`, and the two halves end up writing one row
    // each. Writing `mine` here instead would leave the winner's `4` standing and make the
    // expected state depend on which block committed first.
    node.run(&format!(
        "UPDATE samples SET value = 10 WHERE id = {theirs}"
    ))
    .expect("the recovered block must be able to write");
    node.run("COMMIT").expect("and to commit");
    victim
}
