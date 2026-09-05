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
//!
//! SERIALIZABLE's read-set validation is here for the same reason and against the same cluster
//! ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md),
//! [ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)): the
//! read set only means anything if it survives the wire, and `MemoryBackend` never puts it there.

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

/// **Write skew is refused against a real cluster**, which is what ADR 0062 unit 8b is for.
///
/// The same doctors as `tests/serializable.rs`, over three real stores and real sockets. In process
/// the validation needs no lock, because it and the version writes share one critical section;
/// here they are separate messages to separate regions, and what closes that window is the `Check`
/// mutation's lock record (ADR 0062 §2, ADR 0067 §1). So this test is not a duplicate of the
/// in-process one: it is the case the in-process one cannot make.
#[test]
fn write_skew_is_refused_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE on_call (name text primary key, duty boolean)")
        .unwrap();
    setup
        .run("INSERT INTO on_call VALUES ('alice', true), ('bob', true)")
        .unwrap();

    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();
    let mut b = cluster.session();
    let second = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        b.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        b.rows("SELECT count(*) FROM on_call WHERE duty");
        b.run("UPDATE on_call SET duty = false WHERE name = 'bob'")
            .unwrap();
        b_says.send("B has read and written").unwrap();
        hears_a.recv_timeout(Duration::from_secs(20)).unwrap();
        b.run("COMMIT").map(|_| ())
    });

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    a.rows("SELECT count(*) FROM on_call WHERE duty");
    a.run("UPDATE on_call SET duty = false WHERE name = 'alice'")
        .unwrap();
    // Both have read at their own snapshots before either commits, which is the whole scenario.
    hears_b.recv_timeout(Duration::from_secs(20)).unwrap();
    a.run("COMMIT").expect("the first committer always wins");
    a_says.send("A has committed").unwrap();

    let refused = second
        .join()
        .unwrap()
        .expect_err("B read the rows A wrote; exactly one of the two may commit");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");

    let mut reader = cluster.session();
    assert_eq!(
        reader.rows("SELECT count(*) FROM on_call WHERE duty"),
        [[Some("1".to_owned())]],
        "one doctor is still on call, which is what a serial order leaves"
    );
}

/// **`changed_since_statement` is exact on the store path** (ADR 0067 §2, debt #1).
///
/// A writer whose lock is free but whose value is stale must **re-run** and write over what is
/// there, not answer `40001`. Before `LatestCommit` the store path could not ask, so it took the
/// second road: the conflict surfaced at the commit instead.
#[test]
fn a_statement_whose_row_moved_re_runs_rather_than_failing_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE moved (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO moved VALUES (1, 1)").unwrap();

    let mut session = cluster.session();
    session.run("BEGIN").unwrap();
    // The statement's snapshot is taken here and the row is read at it.
    assert_eq!(
        session.rows("SELECT n FROM moved WHERE id = 1"),
        [[Some("1".to_owned())]]
    );

    // Another session commits and finishes: no lock is held by the time we write, so nothing waits.
    let mut other = cluster.session();
    other.run("UPDATE moved SET n = 2 WHERE id = 1").unwrap();

    session
        .run("UPDATE moved SET n = n + 10 WHERE id = 1")
        .expect("READ COMMITTED re-runs on what is there rather than refusing");
    session.run("COMMIT").unwrap();

    let mut reader = cluster.session();
    assert_eq!(
        reader.rows("SELECT n FROM moved WHERE id = 1"),
        [[Some("12".to_owned())]],
        "the arithmetic is on the committed value, 2, and not on the 1 the statement first read"
    );
}

/// **A row read by primary key is validated as a key** (ADR 0067 §1, `Check` tag 5).
///
/// Every other real-store test of the read set reads with a scan, so it exercises `CheckRange` and
/// says nothing about the point tag. Here the transaction reads one row by key and writes to a
/// *different table*, so its own prewrite cannot catch anything — there is no write-write conflict
/// to find, and no range over `watched` was ever scanned. The only thing that can refuse this
/// commit is the `Check` on the key it read.
///
/// Shown red by removing `StoreTxn::record_key`: this test fails and `write_skew_is_refused_...`
/// above does not, which is also the evidence that the two tests carry different tags.
#[test]
fn a_row_read_by_key_is_validated_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE watched (id bigint primary key, n bigint)")
        .unwrap();
    setup
        .run("CREATE TABLE elsewhere (id bigint primary key, n bigint)")
        .unwrap();
    setup
        .run("INSERT INTO watched VALUES (1, 0), (2, 0)")
        .unwrap();
    setup.run("INSERT INTO elsewhere VALUES (1, 0)").unwrap();

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    assert_eq!(
        a.rows("SELECT n FROM watched WHERE id = 1"),
        [[Some("0".to_owned())]]
    );
    a.run("UPDATE elsewhere SET n = 1 WHERE id = 1").unwrap();

    // Committed and finished before A commits, so no lock is held and nothing waits: the conflict
    // is visible only to a transaction that remembers what it read.
    let mut other = cluster.session();
    other.run("UPDATE watched SET n = 9 WHERE id = 1").unwrap();

    let refused = a
        .run("COMMIT")
        .expect_err("A's answer came from a row that has since changed");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
}

/// **And validated as a key rather than as its table** — the half that makes the first test mean
/// something.
///
/// The same shape, except the other session writes the row A did *not* read. A read set recorded at
/// table granularity refuses this too, and a `CheckRange` over `watched` would report exactly the
/// conflict the first test wants; only a per-key read set lets it through. So this is the test that
/// says which tag carried the refusal above.
///
/// Shown red by widening `record_key` to record the key's whole prefix as a range: this test fails
/// and the one above does not. So each of the pair is red under exactly one counterfactual, and a
/// commit — which is what this one asserts, and what a great many broken things also produce — is
/// evidence here rather than an absence of it.
#[test]
fn a_row_the_transaction_never_read_does_not_refuse_it_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE watched (id bigint primary key, n bigint)")
        .unwrap();
    setup
        .run("CREATE TABLE elsewhere (id bigint primary key, n bigint)")
        .unwrap();
    setup
        .run("INSERT INTO watched VALUES (1, 0), (2, 0)")
        .unwrap();
    setup.run("INSERT INTO elsewhere VALUES (1, 0)").unwrap();

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    assert_eq!(
        a.rows("SELECT n FROM watched WHERE id = 1"),
        [[Some("0".to_owned())]]
    );
    a.run("UPDATE elsewhere SET n = 1 WHERE id = 1").unwrap();

    let mut other = cluster.session();
    other.run("UPDATE watched SET n = 9 WHERE id = 2").unwrap();

    a.run("COMMIT")
        .expect("row 2 is not a row A read, and its table is not what A recorded");
}

/// **The savepoint's undo reaches the client's write buffer, against real stores.**
///
/// The node-local version of this is in `tests/savepoint_rollback.rs`; this is the half a fake
/// cannot answer, because the buffer that has to forget the row lives in `esker-client` and the
/// conflict that would refuse the commit is a real prewrite against a real `write` column family.
///
/// Rails' shape exactly (`transaction_nested_test.rb`): the other session finishes first, the
/// statement inside the savepoint legitimately answers `40001`, and the **outer** `COMMIT` must
/// still succeed — that commit is where the escaping error was raised.
#[test]
fn a_savepoint_rollback_leaves_no_write_to_conflict_against_real_stores() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE t (id bigint primary key, v bigint)")
        .unwrap();
    setup.run("INSERT INTO t VALUES (1, 0), (2, 0)").unwrap();

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    a.run("UPDATE t SET v = 1 WHERE id = 1").unwrap();
    a.run("SAVEPOINT s1").unwrap();

    let mut other = cluster.session();
    other.run("UPDATE t SET v = 9 WHERE id = 2").unwrap();

    let refused = a
        .run("UPDATE t SET v = 2 WHERE id = 2")
        .expect_err("the row moved under a SERIALIZABLE statement");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");

    a.run("ROLLBACK TO SAVEPOINT s1").unwrap();
    a.run("COMMIT")
        .expect("after the rollback A neither writes row 2 nor depends on having read it");

    let mut reader = cluster.session();
    assert_eq!(
        reader.rows("SELECT v FROM t ORDER BY id"),
        [[Some("1".to_owned())], [Some("9".to_owned())]],
        "A's row 1 committed and the other session's row 2 stands"
    );
}

/// **Concurrent increments must not lose one** — debts #1 and #2 on the store path, and the only
/// shape that can see them.
///
/// # Why this is a stress test and not a two-session script
///
/// `changed_since_statement` is consulted at exactly one moment: a writer takes a row lock **at
/// once** (`waited == 0`) and has to ask whether the holder in front committed *between this
/// statement's read and this lock*. A statement that genuinely waited restarts unconditionally and
/// never asks; and a script that reads in one statement and writes in the next reads fresh anyway,
/// because READ COMMITTED gives the second statement its own snapshot.
///
/// So the window is inside a single statement and cannot be scheduled from a client. Two scripted
/// tests were written for this debt before this one and **both passed with the mechanism removed** —
/// they are not in this file. The window is reachable by *contention*, which is how run 66 found the
/// original as ~100 spurious `40001`s in 1,200 transactions.
///
/// # What it detects, measured rather than asserted
///
/// The window is **rare**. Removing `StoreTxn::changed_since_statement` and running this:
///
/// | size | without the override | with it |
/// |---|---|---|
/// | 4 × 15 | 0 refusals, 3/3 green | 3/3 green — **detects nothing** |
/// | 8 × 50 | 1 refusal in 400, fails ~1 run in 2 | **4/4 green, 0 refusals** |
///
/// It catches the mechanism's absence about half the time — a guard rather than a proof, and the
/// size is what buys that. Three smaller scripted tests were written first and every one passed
/// with the mechanism removed; they are not in this file, because a test that cannot fail for the
/// reason it names is worse than no test.
///
/// **It said "it never fails while the mechanism is there", and that was wrong.** The `4/4` above
/// is four runs on a quiet machine, and I wrote a *rate* down as a *property*. Under a full gate —
/// 3,664 tests, this one at 7.8 s — it failed on `refused == 0`: a `40001` that nobody's missing
/// check caused, raised because eight writers contending on one row on a saturated box is a
/// different experiment from eight writers on an idle one. So both directions of this test are
/// rates:
///
/// * with the mechanism, refusals are **usually** zero and not provably zero;
/// * without it, one appears in about half of runs.
///
/// The assertion is left at `refused == 0` rather than widened to a threshold, because a threshold
/// is a number nobody can defend and it would hide the very signal the test exists for. What the
/// experiment needed instead was the machine, so it takes it: `.config/nextest.toml` gives this one
/// test `threads-required = 'num-test-threads'`, the same knob the real-process cluster binaries
/// use. Serialising it against the other members of a group would not have been enough — the load
/// is the other three thousand tests, not its neighbours.
///
/// If it ever reddens anyway, **re-run it alone before believing it**, and the failure worth
/// chasing is the *unloaded* one.
///
/// **The count is refusals, not lost updates.** A lost update cannot happen here: the per-key read
/// stamp (ADR 0057 §4) makes first-committer-wins refuse a write computed from a stale value, so
/// the cost of the missing check is a `40001` nobody needed rather than a wrong answer. Asserting
/// the total alone hid that — the first version of this test filtered on `is_ok()` and threw the
/// evidence away.
#[test]
fn concurrent_increments_do_not_lose_one_against_real_stores() {
    const WRITERS: usize = 8;
    const EACH: usize = 50;

    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE counter (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO counter VALUES (1, 0)").unwrap();

    let committed: Vec<(usize, usize)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..WRITERS)
            .map(|_| {
                let cluster = &cluster;
                scope.spawn(move || {
                    let mut session = cluster.session();
                    // PostgreSQL's own default: wait for the writer in front rather than give up.
                    session.run("SET lock_timeout = 0").unwrap();
                    let mut refused = 0_usize;
                    let wins = (0..EACH)
                        .filter(|_| {
                            match session.run("UPDATE counter SET n = n + 1 WHERE id = 1") {
                                Ok(_) => true,
                                Err(error) => {
                                    // **`40001` is the thing being counted.** A writer that
                                    // took the lock at once and did not notice the holder in
                                    // front had just committed computes from a stale value,
                                    // and first-committer-wins refuses it at prewrite — a
                                    // refusal nobody needed, for a statement that could
                                    // simply have re-run.
                                    refused += usize::from(error.sqlstate() == "40001");
                                    false
                                }
                            }
                        })
                        .count();
                    (wins, refused)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let wins: usize = committed.iter().map(|(won, _)| won).sum();
    let refused: usize = committed.iter().map(|(_, refused)| refused).sum();
    let mut reader = cluster.session();
    assert_eq!(
        reader.rows("SELECT n FROM counter WHERE id = 1"),
        [[Some(wins.to_string())]],
        "{wins} increments committed and the row must hold every one of them; a smaller number is \
         a lost update — a statement that re-ran and computed from the value it read before the \
         writer in front of it committed"
    );
    // **And nothing was refused for no reason**, which is the debt itself. Without
    // `changed_since_statement` on this path a writer that took the lock at once cannot tell that
    // the holder in front had committed, computes from the stale value, and is refused at prewrite
    // — run 66 measured ~100 of those in 1,200 transactions. With it the statement re-runs instead.
    assert_eq!(
        refused,
        0,
        "{refused} of {} increments were refused with 40001; every one is a statement that could \
         have re-read and re-run",
        WRITERS * EACH
    );
}
