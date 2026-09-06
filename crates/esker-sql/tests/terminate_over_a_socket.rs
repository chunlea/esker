//! `pg_terminate_backend` — one session ends another's **connection**, over real sockets.
//!
//! Two Rails tests want it and neither is about the function's return value:
//! `AdapterConnectionTest#test_#execute_is_retryable` terminates its own backend through
//! `kill_connection_from_server` and then expects `execute("SELECT 1", allow_retry: true)` to come
//! back on a *different* pid, and
//! `PostgreSQLAdapterTest#test_translate_no_connection_exception_to_not_established` terminates a
//! connection and expects the next use of it to raise `ConnectionNotEstablished`. Both need the
//! socket to actually go.
//!
//! Measured on PostgreSQL 19beta1:
//!
//! ```text
//! SELECT pg_typeof(pg_terminate_backend(999999));  -> boolean
//! SELECT pg_terminate_backend(999999);             -> f, with
//!     WARNING:  PID 999999 is not a PostgreSQL backend process
//! SELECT pg_terminate_backend(<a live pid>);       -> t, and that pid leaves pg_stat_activity
//! ```
//!
//! and what the terminated session is told, with `VERBOSITY verbose`:
//!
//! ```text
//! FATAL:  57P01: terminating connection due to administrator command
//! server closed the connection unexpectedly
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

async fn two_connections() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    listen(store, catalog).await
}

/// **The terminated session is told `57P01` and its socket ends.**
///
/// `t` for the caller, and the victim learns of it the next time it uses the connection — which is
/// exactly when both Rails tests look.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminated_session_is_told_so_and_its_connection_ends() {
    let address = two_connections().await;
    let mut a = Client::connect(address).await;
    let mut b = Client::connect(address).await;

    let pid = b.query("SELECT pg_backend_pid()").await;
    let pid = pid
        .rows
        .first()
        .and_then(|row| row.first())
        .expect("a session knows its own pid")
        .clone();

    let killed = a
        .query(&format!("SELECT pg_terminate_backend({pid})"))
        .await;
    assert_eq!(
        killed.rows,
        vec![vec!["t".to_owned()]],
        "the pid is one this node holds: {killed:?}"
    );

    let after = b.query("SELECT 1").await;
    assert_eq!(
        after.sqlstate.as_deref(),
        Some("57P01"),
        "the terminated session must be told why its connection ended: {after:?}"
    );

    // And the socket is really gone: a second use reads end-of-stream rather than an answer.
    let gone = b.query("SELECT 1").await;
    assert!(
        gone.rows.is_empty() && gone.tags.is_empty(),
        "the connection must be closed, not merely errored: {gone:?}"
    );
}

/// **A pid nobody holds is `f`, with the warning PostgreSQL emits** — not an error.
///
/// `pg_cancel_backend` already answers `false` here for the same reason; the difference is that
/// PostgreSQL says why, on a channel that does not disturb the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminating_a_pid_nobody_holds_answers_false() {
    let address = two_connections().await;
    let mut a = Client::connect(address).await;
    let answered = a.query("SELECT pg_terminate_backend(999999)").await;
    assert_eq!(
        answered.rows,
        vec![vec!["f".to_owned()]],
        "no such backend: {answered:?}"
    );
    assert_eq!(
        answered.sqlstate, None,
        "a missing pid is answered, not raised: {answered:?}"
    );
}

/// **A session may end its own connection**, which is how `kill_connection_from_server` is used
/// when the pool hands back the same backend: the answer never reaches the client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_can_terminate_itself() {
    let address = two_connections().await;
    let mut a = Client::connect(address).await;
    let answered = a
        .query("SELECT pg_terminate_backend(pg_backend_pid())")
        .await;
    assert_eq!(
        answered.sqlstate.as_deref(),
        Some("57P01"),
        "terminating yourself ends your own connection: {answered:?}"
    );
}
