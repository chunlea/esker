//! READ COMMITTED against a **real three-store cluster**, over real sockets, through Percolator
//! ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md) §5).
//!
//! Every other test of ADR 0057 runs against `MemoryBackend`, and for most of the unit that is the
//! right place: the semantics are the same and the race is a thousand times cheaper to run. This
//! file exists for the one question a fake cannot answer — **whether any of it happens at all when
//! the backend is `StoreTxn`** — because until this unit that backend took the trait's defaults and
//! the answer was no. `lock` said "taken" to everybody, `begin_statement` did nothing, and a
//! transaction read at `BEGIN` for its whole life.
//!
//! What is proved here is node-local by construction: both sessions are on one `StoreBackend`,
//! which is one `esker-sql` process. Two *nodes* still do not see each other's row locks, and that
//! is the ADR's declared scope rather than something this file forgot to test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::sync::mpsc::channel;
use std::time::Duration;

use cluster::Cluster;

/// **A writer waits for the writer in front of it, on the store path.**
///
/// The assertion that only a real lock can produce is B still blocked while A holds the row — not
/// the final value, which two sessions racing to a lost update would also produce. Then B's own
/// `COMMIT` must succeed, which is the per-key read timestamp (§4) doing its half: without it the
/// waiter dies at its own prewrite with `40001` for the transaction it just waited for, and the
/// wait would have bought nothing at all.
#[test]
fn a_writer_waits_for_the_writer_in_front_of_it_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO lk (id, n) VALUES (1, 10)").unwrap();

    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();
    let (done_says, hears_done) = channel();
    let mut b = cluster.session();
    let waiter = std::thread::spawn(move || {
        // **Both edges, and the A→B one is the one that matters.** Without it B may reach the row
        // *first*, take the lock legitimately and finish — which is not a bug and reads exactly
        // like one. This test failed that way under a full workspace run and passed alone, which
        // is the signature of a barrier that gates one side only.
        hears_a.recv_timeout(Duration::from_secs(10)).unwrap();
        b_says.send("B is about to write").unwrap();
        // No `BEGIN`: one statement, its own transaction, and it must wait exactly as a block does.
        let update = b.run("UPDATE lk SET n = n + 100 WHERE id = 1");
        done_says.send("B has written").unwrap();
        update
    });

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE lk SET n = n + 1 WHERE id = 1").unwrap();
    a_says.send("A holds the row").unwrap();
    hears_b.recv_timeout(Duration::from_secs(10)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // **Recorded, not asserted here.** B reports "done" whether it wrote or failed, so asserting on
    // the timing alone turns "B did not wait" and "B failed in 3 ms" into the same message — and
    // the second one is the interesting failure.
    let finished_early = hears_done.try_recv().is_ok();
    a.run("COMMIT").unwrap();

    let update = waiter.join().unwrap();
    assert!(
        !finished_early,
        "B finished while A held the row. Its answer was {update:?} — if that is an error, the \
         lock is not what failed"
    );
    update.expect("B must wait for A and then write, not fail at its own commit");

    let mut reader = cluster.session();
    assert_eq!(
        reader.rows("SELECT n FROM lk WHERE id = 1"),
        [[Some("111".to_owned())]],
        "B computed from A's committed value, not from the one it first read"
    );
}

/// **Each statement of a transaction reads what was committed when that statement began.**
///
/// READ COMMITTED's whole content on the read side, and against a cluster it was simply absent:
/// every read went out at `start_ts`, which is REPEATABLE READ wearing another name. Measured on
/// PostgreSQL 19 as 10 then 99 across another session's commit.
#[test]
fn a_statement_reads_its_own_snapshot_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE rc (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO rc (id, n) VALUES (1, 10)").unwrap();

    let mut reader = cluster.session();
    reader.run("BEGIN").unwrap();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [[Some("10".to_owned())]]
    );

    let mut writer = cluster.session();
    writer.run("UPDATE rc SET n = 99 WHERE id = 1").unwrap();

    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [[Some("99".to_owned())]],
        "a second statement in the same transaction sees a commit that landed between them"
    );
    reader.run("COMMIT").unwrap();

    // And the level that keeps its snapshot still keeps it, which is what must not regress.
    let mut frozen = cluster.session();
    // **The level is set as a statement, not on the `BEGIN`.** This fixture's `run` maps `BEGIN` to
    // `Executor::begin` and drops everything the client wrote after the keyword, so a
    // `BEGIN ISOLATION LEVEL …` here would silently run at the default and this test would be
    // asserting READ COMMITTED's behaviour under REPEATABLE READ's name.
    frozen.run("BEGIN").unwrap();
    frozen
        .run("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    assert_eq!(
        frozen.rows("SELECT n FROM rc WHERE id = 1"),
        [[Some("99".to_owned())]]
    );
    let mut second = cluster.session();
    second.run("UPDATE rc SET n = 100 WHERE id = 1").unwrap();
    assert_eq!(
        frozen.rows("SELECT n FROM rc WHERE id = 1"),
        [[Some("99".to_owned())]],
        "REPEATABLE READ reads the transaction's own snapshot, statement after statement"
    );
    frozen.run("COMMIT").unwrap();
}

/// **A lock dies with its session on the store path too.**
///
/// The way out nobody writes down: a session that disconnects mid-transaction takes neither
/// `commit` nor `rollback`, and a lock released only by those two is held for the life of the
/// process. On the in-process backend that cost four suite files a round (run 66); there is no
/// reason for the store path to learn it the same way.
#[test]
fn a_dropped_session_gives_its_row_locks_back_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE gone (id bigint primary key, n bigint)")
        .unwrap();
    setup
        .run("INSERT INTO gone (id, n) VALUES (1, 10)")
        .unwrap();
    {
        let mut abandoned = cluster.session();
        abandoned.run("BEGIN").unwrap();
        abandoned.run("UPDATE gone SET n = 1 WHERE id = 1").unwrap();
        // Dropped here with no COMMIT and no ROLLBACK, as an abrupt disconnect drops one.
    }

    let mut next = cluster.session();
    next.run("SET lock_timeout = '5s'").unwrap();
    next.run("UPDATE gone SET n = 99 WHERE id = 1")
        .expect("the lock died with the session that took it");
    assert_eq!(
        next.rows("SELECT n FROM gone WHERE id = 1"),
        [[Some("99".to_owned())]]
    );
}
