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

use parity::{Pair, edge, reached};

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

/// **The block's own rows survive the deadlock its savepoint died of.**
///
/// A savepoint is a subtransaction, and a `40P01` inside one kills the subtransaction. The rows the
/// outer block locked *before* the mark are the rows it is going to write after the `rescue`, so
/// giving them back is ending the transaction without saying so — another session can take one,
/// write it, and commit under a block that is still open and still going to write it.
///
/// The Rails test above cannot see this: every lock it takes is inside the savepoint, where
/// "release everything" and "release the savepoint's own" are the same act. This is the sequence
/// that separates them, and the victim is not a coin toss — the transaction that closes the cycle
/// is the one that is told, so B asking last makes B the victim every time.
#[test]
fn the_rows_a_block_held_before_its_savepoint_are_still_its_own() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n bigint)",
        "INSERT INTO lk VALUES (0, 0), (1, 1), (2, 2)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();
    let sessions = pair.sessions();

    // A is the survivor: it holds row 1 and then waits for row 2, which B is holding.
    let survivor = std::thread::spawn(move || {
        let mut a = sessions.session();
        a.run("BEGIN").unwrap();
        a.run("SET lock_timeout = '20s'").unwrap();
        a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
        reached(&a_says, "A holds row 1");
        edge(&hears_b, "B holds rows 0 and 2");
        // Blocks behind B's savepoint lock, which is the edge that makes B's next ask a cycle.
        let _ = a.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE");
        a.run("ROLLBACK").unwrap();
    });

    let mut b = pair.session();
    b.run("BEGIN").unwrap();
    b.run("SET lock_timeout = '20s'").unwrap();
    // **Before the savepoint**, which is the whole point: this is the outer block's row.
    b.run("SELECT n FROM lk WHERE id = 0 FOR UPDATE").unwrap();
    b.run("SAVEPOINT sp").unwrap();
    b.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE").unwrap();
    reached(&b_says, "B holds rows 0 and 2");
    edge(&hears_a, "A holds row 1");

    // A is *waiting*, not merely started — a third session reading `pg_locks` is the only way to
    // know that, and waiting is what makes the next statement close a cycle rather than block.
    let mut watcher = pair.session();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline
        && watcher
            .rows("SELECT pid FROM pg_locks WHERE granted = false")
            .is_empty()
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    // **An `UPDATE` and not a `SELECT … FOR UPDATE`, and the difference is the whole test.** There
    // are two places a `40P01` is raised: the row-locking clause raises it on the spot, holding
    // everything until the client's `ROLLBACK TO` gives the savepoint's locks back, and the write
    // path raises it from inside the wait loop, which gives them back at once so the survivor stops
    // waiting. Only the second releases anything, so only the second can release too much — and it
    // is the one Rails reaches, through `s2.update value: 4`.
    let deadlocked = b
        .run("UPDATE lk SET n = 4 WHERE id = 1")
        .expect_err("the session that closes the cycle is the one told about it");
    assert_eq!(deadlocked.to_string(), "deadlock detected");
    b.run("ROLLBACK TO SAVEPOINT sp").unwrap();

    // The assertion the gap was hiding: row 0 is still B's, and B's block is still open.
    let refused = watcher
        .run("SELECT n FROM lk WHERE id = 0 FOR UPDATE NOWAIT")
        .expect_err("row 0 was locked before the savepoint, so the deadlock did not free it");
    assert_eq!(
        refused.to_string(),
        "could not obtain lock on row in relation \"lk\"",
        "another session must not be able to take a row this block still holds"
    );

    // And the block goes on to do exactly what the lock was for.
    b.run("UPDATE lk SET n = 10 WHERE id = 0").unwrap();
    b.run("COMMIT").unwrap();
    survivor.join().unwrap();

    let mut after = pair.session();
    assert_eq!(
        after.rows("SELECT n FROM lk WHERE id = 0"),
        vec![vec!["10".to_string()]]
    );
}
