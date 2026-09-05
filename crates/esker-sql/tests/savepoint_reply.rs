//! **Every statement gets a reply**, including one that blocks inside a savepoint.
//!
//! `transaction_test.rb`'s `QueryCanceled` test, whose shape r1 captured on 2026-09-05
//! (`triage/query-canceled-capture.md`). With `use_transactional_tests = false` all four values are
//! right: the active waiter sorts first, `query_value` picks it, `pg_cancel_backend` answers `true`
//! and the main thread raises `57014`. Turning transactional tests **on** — which makes both
//! transactions savepoints inside the harness's wrapping transaction — makes the probe **hang**,
//! and the node's own view says why it is not a cancellation problem at all:
//!
//! ```text
//! pid=50  idle in transaction  query=BEGIN
//! pid=52  idle in transaction  query=RELEASE SAVEPOINT active_record_1
//! ```
//!
//! **Neither session is running a `FOR UPDATE`**, while the client sits in `read` waiting for a
//! reply — 246 frames of it. So a statement either never reached the executor or finished without
//! a reply the client could see. A hang is worse than the failure it was filed as, which is why
//! this file asserts on the *bytes* and gives every statement its own deadline: a lost reply must
//! be a named failure and never a stopped suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

/// The fixture, and the address to reach it on.
async fn cluster() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    {
        let mut setup = parity::Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        setup
            .run("CREATE TABLE samples (id bigint primary key, value bigint)")
            .unwrap();
        setup.run("INSERT INTO samples VALUES (1, 1)").unwrap();
    }
    listen(store, catalog).await
}

/// **The whole sequence, and every statement is checked for its reply.**
///
/// `ActiveRecord` under transactional tests opens the wrapping transaction, then names each
/// `transaction do` a savepoint — so the holder and the waiter are both *inside* one, and the
/// holder releases its savepoint while still holding the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_statement_in_a_savepoint_gets_a_reply() {
    let address = cluster().await;

    // A: the harness's wrapping transaction, then the thread's `transaction do`.
    let mut a = Client::connect(address).await;
    for sql in ["BEGIN", "SAVEPOINT active_record_1"] {
        let answer = a.query(sql).await;
        assert!(answer.tags.ends_with('Z'), "{sql} got {:?}", answer.tags);
    }
    let held = a
        .query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
        .await;
    assert!(held.sqlstate.is_none(), "the holder was refused: {held:?}");

    // B: its own wrapping transaction and savepoint, then the statement that must block.
    let mut b = Client::connect(address).await;
    for sql in [
        "BEGIN",
        "SAVEPOINT active_record_2",
        "SET lock_timeout = '5s'",
    ] {
        let answer = b.query(sql).await;
        assert!(answer.tags.ends_with('Z'), "{sql} got {:?}", answer.tags);
    }
    let blocked = tokio::spawn(async move {
        let answer = b
            .query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
            .await;
        (b, answer)
    });

    // A releases its savepoint **while still holding the row** — the state the capture found the
    // node in, `RELEASE SAVEPOINT active_record_1` as its last statement.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let released = a.query("RELEASE SAVEPOINT active_record_1").await;
    assert!(
        released.tags.ends_with('Z'),
        "RELEASE SAVEPOINT got no reply: {:?}",
        released.tags
    );

    // The view the capture read, from a third connection, so a failure names what the node thought.
    let mut watcher = Client::connect(address).await;
    let seen = watcher
        .query("SELECT pid, state, query FROM pg_stat_activity")
        .await;

    // And the whole point: B's statement must be answered — by rows, or by an error, but answered.
    let answered = tokio::time::timeout(Duration::from_secs(20), blocked).await;
    let Ok(Ok((_b, answer))) = answered else {
        panic!(
            "the blocked statement never got a reply — the hang r1 captured.\n  the node saw: {:?}",
            seen.rows
        );
    };
    assert!(
        answer.tags.ends_with('Z'),
        "the blocked statement's exchange did not end in ReadyForQuery: {:?}\n  the node saw: {:?}",
        answer.tags,
        seen.rows
    );

    a.query("ROLLBACK").await;
}

/// **The waiter with no `lock_timeout`, which is what `ActiveRecord` actually sends.**
///
/// The test above gives B a five-second bound, so a lock that is never released still produces a
/// `55P03` — a reply, and the assertion passes. Rails sets none: the wait is unbounded, and a lock
/// the savepoint path fails to hand back is then a **hang** rather than a refusal. That is the
/// difference between the shape r1 captured and the shape a bounded test can see, so it is asserted
/// separately: the holder releases its savepoint while holding the row, and then ends its
/// transaction, and B must be answered because the row is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_taken_in_a_released_savepoint_is_still_given_back_at_the_end() {
    let address = cluster().await;

    let mut a = Client::connect(address).await;
    for sql in [
        "BEGIN",
        "SAVEPOINT active_record_1",
        "SELECT value FROM samples WHERE id = 1 FOR UPDATE",
        // Released while the row is still held — the state the capture found the node in.
        "RELEASE SAVEPOINT active_record_1",
    ] {
        let answer = a.query(sql).await;
        assert!(answer.tags.ends_with('Z'), "{sql} got {:?}", answer.tags);
    }

    // B waits with **no bound at all**, exactly as `ActiveRecord` does.
    let mut b = Client::connect(address).await;
    for sql in ["BEGIN", "SAVEPOINT active_record_2"] {
        b.query(sql).await;
    }
    let blocked = tokio::spawn(async move {
        b.query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
            .await
    });

    // A ends its transaction, which must free the row whichever savepoint took it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.query("ROLLBACK").await;

    let answered = tokio::time::timeout(Duration::from_secs(20), blocked).await;
    let Ok(Ok(answer)) = answered else {
        panic!(
            "the waiter was never answered after the holder's transaction ended — a lock taken \
             inside a released savepoint was not given back"
        );
    };
    assert!(
        answer.sqlstate.is_none(),
        "the waiter should have got the row once it was free: {answer:?}"
    );
    assert_eq!(answer.rows, vec![vec!["1".to_owned()]], "{answer:?}");
}
