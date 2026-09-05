//! The same cancellation, over **two real connections to a real listener**.
//!
//! `tests/cross_session_cancel.rs` drives two `Session`s in one thread pair and passes, so the
//! message layer is not what `transaction_test.rb` trips over. What is left is the server: a
//! connection per socket, each statement handed to `tokio::task::spawn_blocking`, and the
//! cancelling session on a different thread of that pool from the one blocked on the row. This
//! file is that, and nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

/// **The waiter's statement dies with `57014`, over sockets.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_statement_is_cancelled_across_two_connections() {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    {
        let mut setup = parity::Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        setup
            .run("CREATE TABLE samples (id bigint primary key, value bigint)")
            .unwrap();
        setup.run("INSERT INTO samples VALUES (1, 1)").unwrap();
    }
    let address = listen(Arc::clone(&store), Arc::clone(&catalog)).await;

    // A: the holder, and the canceller, from inside its own transaction.
    let mut a = Client::connect(address).await;
    a.query("BEGIN").await;
    a.query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
        .await;

    // B: blocks behind A on its own connection.
    let victim = tokio::spawn(async move {
        let mut b = Client::connect(address).await;
        b.query("BEGIN").await;
        b.query("SET lock_timeout = '20s'").await;
        b.query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
            .await
    });

    // The Rails hunt, verbatim.
    let mut pid = None;
    for _ in 0..500 {
        let found = a
            .query("SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'")
            .await;
        if let Some(row) = found.rows.first().and_then(|row| row.first()) {
            pid = Some(row.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = pid.expect("the blocked waiter must be visible with its statement");

    // One request, as the Rails test issues.
    let cancelled = a.query(&format!("SELECT pg_cancel_backend({pid})")).await;
    assert_eq!(
        cancelled.rows,
        vec![vec!["t".to_owned()]],
        "the pid must be one this node holds: {cancelled:?}"
    );

    let answered = victim.await.unwrap();
    a.query("ROLLBACK").await;
    assert_eq!(
        answered.sqlstate.as_deref(),
        Some("57014"),
        "the blocked statement must be cancelled: {answered:?}"
    );
}
