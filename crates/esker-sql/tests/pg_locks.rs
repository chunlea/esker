//! `pg_locks`: who holds a row lock on this node, and who is waiting for one.
//!
//! Built for a specific failure. Ten passes of the Rails suite have lost a file to a request the
//! node never answered, and at the moment of the hang the server could not be asked who else was
//! connected or what they held: `pg_stat_activity` answers only the asking session, and `pg_locks`
//! did not exist. The harness's forensics had to ask the operating system instead.
//!
//! The shape is PostgreSQL 19's, measured with one session holding `SELECT … FOR UPDATE` and a
//! second blocked behind it: the holder is a `tuple` row with `granted = true`, and **the waiter is
//! a `transactionid` row with `granted = false`** naming the transaction it waits for. Reading the
//! two together is how a stuck session's counterpart is found.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::{Pair, edge, reached};

/// **An idle node holds nothing, and says so in sixteen columns.**
///
/// The empty answer matters as much as the full one: it is what the harness reads when it wants to
/// know that nothing is held, and a view that could not be selected from at all — which is what
/// `pg_locks` was — cannot say that.
#[test]
fn an_idle_node_holds_no_locks_and_the_view_has_postgresql_s_columns() {
    let mut node = parity::Node::new(&["CREATE TABLE lk (id bigint primary key, n text)"]);
    assert!(node.rows("SELECT * FROM pg_locks").is_empty());
    // The columns a client selects by name, in PostgreSQL's order.
    assert!(
        node.run(
            "SELECT locktype, database, relation, page, tuple, virtualxid, transactionid, \
             classid, objid, objsubid, virtualtransaction, pid, mode, granted, fastpath, \
             waitstart FROM pg_locks"
        )
        .is_ok()
    );
    // And it is a relation like any other: `pg_catalog.pg_locks` resolves, and so does the bare
    // name, which is what a schema dump and a `::regclass` both need.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_catalog.pg_locks")[0][0],
        "0"
    );
}

/// **A held row is one `tuple` row, and a blocked writer is one `transactionid` row that is not
/// granted.**
///
/// The assertion this view exists for: with A holding and B blocked, a *third* session can see both
/// of them — which is the question the harness could not ask when a file hung.
#[test]
fn a_held_row_and_its_waiter_are_both_visible_to_a_third_session() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to write");
        b.run("UPDATE lk SET n = 'b' WHERE id = 1")
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(300));

    let mut watcher = pair.session();
    let rows = watcher.rows(
        "SELECT locktype, granted, mode, relation IS NOT NULL, virtualtransaction \
         FROM pg_locks ORDER BY locktype",
    );
    assert_eq!(rows.len(), 2, "one holder and one waiter: {rows:?}");
    assert_eq!(
        rows[0][..4].to_vec(),
        vec!["transactionid", "f", "ShareLock", "f"],
        "the waiter is not granted and names a transaction rather than a relation"
    );
    assert_eq!(
        rows[1][..4].to_vec(),
        vec!["tuple", "t", "ExclusiveLock", "t"],
        "the holder is granted and names the relation"
    );
    assert_ne!(
        rows[0][4], rows[1][4],
        "the two are different transactions, which is the whole point of the column"
    );

    a.run("COMMIT").unwrap();
    waiter.join().unwrap().unwrap();
    // And when nobody holds anything, it is empty again — a lock that outlived its session would
    // show here, which is the regression this view makes visible rather than fatal.
    assert!(watcher.rows("SELECT * FROM pg_locks").is_empty());
}

/// **A waiter that stops waiting stops being a waiter** — and it did not, which is what run 78's
/// capture found.
///
/// The instrument caught a session shown as `granted = f`, waiting on transaction id
/// `9223372036854775807`, still there an hour after its client had exited, with a second one
/// accumulated beside it. Every part of that is this bug:
///
/// * a wait-for edge was recorded when a transaction **blocked** and removed only when it
///   **released a key**, so a transaction that waited and then acquired kept its edge, and one that
///   waited and never acquired anything kept its edge for the life of the process — `release`
///   returned early when it held nothing;
/// * `i64::MAX` was `pg_locks` inventing a transaction id for a holder that no longer held
///   anything, which is precisely what a leaked edge looks like.
///
/// So the "waiter on a sentinel" was a stale row, not a stuck session. A diagnostic that invents
/// waiters is worse than one that says nothing, so this test asserts the view goes **empty**.
#[test]
fn a_wait_that_ended_leaves_no_row_behind() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to write");
        // B waits for A, then acquires the row when A commits.
        b.run("UPDATE lk SET n = 'b' WHERE id = 1").unwrap();
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT").unwrap();
    waiter.join().unwrap();

    let mut watcher = pair.session();
    assert_eq!(
        watcher.rows("SELECT locktype, granted, transactionid FROM pg_locks"),
        Vec::<Vec<String>>::new(),
        "both transactions are over: nothing is held and nobody is waiting"
    );
}

/// **A transaction that waited and never got anything leaves nothing either.**
///
/// The other half, and the one that accumulated in the capture: a waiter whose statement gives up —
/// `lock_timeout` fires — held no key at all, so the release that would have forgotten it returned
/// early on an empty list.
#[test]
fn a_wait_that_timed_out_leaves_no_row_behind() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    let (a_says, hears_a) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        b.run("SET lock_timeout = '150ms'").unwrap();
        let refused = b
            .run("UPDATE lk SET n = 'b' WHERE id = 1")
            .expect_err("A holds it and B gave up");
        assert_eq!(refused.sqlstate(), "55P03", "{refused}");
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    waiter.join().unwrap();

    // A still holds its row, so exactly one row — and no waiter.
    let mut watcher = pair.session();
    let rows = watcher.rows("SELECT locktype, granted FROM pg_locks");
    assert_eq!(
        rows,
        vec![vec!["tuple".to_owned(), "t".to_owned()]],
        "the holder is there and the transaction that gave up is not"
    );
    a.run("ROLLBACK").unwrap();
    assert!(watcher.rows("SELECT * FROM pg_locks").is_empty());
}

/// **A savepoint is not a lock release, and it must not look like one.**
///
/// The third instance of one shape in this crate, and the only one of the three that was a live
/// wrong answer. `Txn::locks` is a *defaulted* trait method, so a wrapper that forwards everything
/// it was written to forward silently opts out of it and answers `LockView::default()` — empty.
/// `Recording` is that wrapper, and the executor puts it around the real transaction for **every
/// statement while a savepoint is open** (`exec/mod.rs`, `savepoints.recording()`). Rails opens one
/// for every nested `transaction do`, so this is the state a stuck session is most likely to be in
/// when somebody finally asks what it holds — and an empty `pg_locks` there says "nothing is held
/// on this node", which is a different claim from "I cannot tell you".
///
/// The assertion is the pair, not the second line alone: the same question is asked either side of
/// one `SAVEPOINT`, so the test names the savepoint as the only thing that changed.
#[test]
fn a_savepoint_does_not_hide_the_lock_the_session_is_holding() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    node.run("BEGIN").unwrap();
    node.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE")
        .unwrap();

    let held = vec![vec!["tuple".to_owned(), "t".to_owned()]];
    let query = "SELECT locktype, granted FROM pg_locks";
    assert_eq!(node.rows(query), held, "the row this session locked");

    node.run("SAVEPOINT s").unwrap();
    assert_eq!(
        node.rows(query),
        held,
        "the same lock, the same session, one savepoint later"
    );

    // And after a rollback to it: the lock was taken before the savepoint, so it survives — which
    // is the answer that would still be right if `Recording` were bypassed only on the way in.
    node.run("ROLLBACK TO SAVEPOINT s").unwrap();
    assert_eq!(node.rows(query), held, "a rollback to a later savepoint");
    node.run("ROLLBACK").unwrap();
    assert!(node.rows(query).is_empty(), "and the block released it");
}

/// **`pid` names the session that holds the lock**, and it is the number that session reports.
///
/// It was `std::process::id()` for every row — the same number for every session, so the column
/// could not tell two holders apart and a join to `pg_stat_activity.pid` matched nothing. On a
/// real server the two views agree by definition: `pg_locks.pid` *is* the backend pid, which is
/// what `pg_backend_pid()` returns and what `pg_cancel_backend` takes.
///
/// Found while writing `cross_session_cancel`'s third test, where the readiness check could not
/// ask "is *this* session waiting" and had to count ungranted rows instead.
#[test]
fn each_lock_names_the_session_that_holds_it() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a'), (2, 'b')",
    ]);
    // **Two rows, not one**: the sessions must not block each other, or the second is a waiter and
    // this test is about holders.
    let mut a = pair.session();
    let mut b = pair.session();
    let a_pid = a.rows("SELECT pg_backend_pid()")[0][0].clone();
    let b_pid = b.rows("SELECT pg_backend_pid()")[0][0].clone();
    assert_ne!(a_pid, b_pid, "two sessions are two backends");

    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    b.run("BEGIN").unwrap();
    b.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE").unwrap();

    let mut watcher = pair.session();
    let held = watcher.rows("SELECT pid FROM pg_locks WHERE granted = true ORDER BY pid");
    let mut expected = [a_pid.clone(), b_pid.clone()];
    expected.sort();
    assert_eq!(
        held,
        vec![vec![expected[0].clone()], vec![expected[1].clone()]],
        "one row per holder, each naming its own session"
    );

    // And the join a client actually writes: the holder of row 1 is the session that took it.
    let joined = watcher.rows(&format!(
        "SELECT count(*) FROM pg_locks WHERE granted = true AND pid = {a_pid}"
    ));
    assert_eq!(joined, vec![vec!["1".to_owned()]]);

    a.run("ROLLBACK").unwrap();
    b.run("ROLLBACK").unwrap();
}

/// **A waiter's row names the waiting session**, not the holder and not the process.
#[test]
fn a_waiter_row_names_the_session_that_is_waiting() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    let (b_says, hears_b) = channel();
    let (b_pid_says, hears_b_pid) = channel();
    let (a_says, hears_a) = channel();

    let sessions = pair.sessions();
    let waiter = std::thread::spawn(move || {
        let hears_a = hears_a;
        let mut node = sessions.session();
        node.run("BEGIN").unwrap();
        node.run("SET lock_timeout = '20s'").unwrap();
        b_pid_says
            .send(node.rows("SELECT pg_backend_pid()")[0][0].clone())
            .unwrap();
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to block");
        let _ = node.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE");
    });

    let mut a = pair.session();
    let a_pid = a.rows("SELECT pg_backend_pid()")[0][0].clone();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    let b_pid = hears_b_pid.recv().expect("B says who it is");
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to block");

    let mut watcher = pair.session();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut waiting = Vec::new();
    while std::time::Instant::now() < deadline {
        waiting = watcher.rows("SELECT pid FROM pg_locks WHERE granted = false");
        if !waiting.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        waiting,
        vec![vec![b_pid.clone()]],
        "the ungranted row is the waiter's own session, and A is {a_pid}"
    );

    a.run("ROLLBACK").unwrap();
    let _ = waiter.join();
}
