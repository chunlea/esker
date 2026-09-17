//! `DROP DATABASE … WITH (FORCE)` **ends the other sessions**, over real sockets — debt #113.
//!
//! `drop_database_force.rs` states the gap this file closes, in as many words: *deleting the
//! terminate call would redden nothing here*. Everything that file can observe about `FORCE` comes
//! from skipping the `55006` refusal, because `parity::Node` is not a connection loop — it never
//! reads the `terminate` flag, so nothing in it can die. `pgwire::session`'s own test double says
//! the same thing from the other side: *"Nothing registers this, so nothing can terminate it"*
//! (`src/pgwire/session.rs:1272`).
//!
//! So the victim has to be a real connection. The shape is
//! `tests/terminate_over_a_socket.rs`'s, including its two-step assertion: **being told and the
//! connection ending are different facts**, and a test that checks only the SQLSTATE would pass on
//! a node that answered `57P01` forever.
//!
//! # What a real server does, and where this node differs
//!
//! Measured on 19beta1 (`esker-coord/s2-h113-victim.out`), with a throwaway database and a victim
//! holding a connection to it:
//!
//! ```text
//! FATAL:  terminating connection due to administrator command
//! server closed the connection unexpectedly
//! ```
//!
//! — the **same text `pg_terminate_backend` produces**, which is why the assertions below are the
//! ones `terminate_over_a_socket.rs` already makes. The control says what the clause is worth: in
//! that same state, a plain `DROP DATABASE` is `ERROR: database "…" is being accessed by other
//! users / DETAIL: There is 1 other session using the database.` and the same statement with
//! `FORCE` succeeds.
//!
//! **One difference is real and is not what this file tests.** The oracle's victim was cut down
//! *during* `SELECT pg_sleep(6)` — its next statement never ran. This node tells a victim whose
//! statement was running when the flag was set **at the end of that statement** instead, which
//! `src/pgwire/server.rs:379` documents and chose on purpose: *"a victim terminated while its
//! statement was running is told so when that statement ends, rather than answering once more
//! first."* The victim here is idle, so both servers answer alike and the difference does not
//! reach these assertions. It is written down so that the next person does not read a passing test
//! as a claim this node severs mid-statement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

/// A listening node, the way `terminate_over_a_socket.rs` builds one.
async fn a_node() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    listen(store, catalog).await
}

/// **The forced drop ends the other session's connection**, which is the fact `#113` exists for.
///
/// Four steps, and the third is the one usually left out: the victim is asked something *before*
/// the drop, because "the victim died" and "the victim never connected" are the same observation
/// otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_drop_ends_the_other_sessions_connection() {
    let address = a_node().await;
    let mut admin = Client::connect(address).await;

    admin.query("CREATE DATABASE h113_forced").await;
    let mut victim = Client::connect_to(address, "h113_forced")
        .await
        .expect("a database that was just created is one a client can connect to");

    // **The victim is alive before anything kills it.** Without this the two assertions below are
    // also what a connection that never started would produce.
    let alive = victim.query("SELECT 1").await;
    assert_eq!(
        alive.rows,
        vec![vec!["1".to_owned()]],
        "the victim must be answering before the drop, or its silence afterwards proves nothing: \
         {alive:?}"
    );

    let dropped = admin.query("DROP DATABASE h113_forced WITH (FORCE)").await;
    assert!(
        dropped.sqlstate.is_none(),
        "the forced drop is the statement under test and must itself succeed: {dropped:?}"
    );

    let after = victim.query("SELECT 1").await;
    assert_eq!(
        after.sqlstate.as_deref(),
        Some("57P01"),
        "the terminated session must be told why its connection ended: {after:?}"
    );

    // And the socket is really gone: a second use reads end-of-stream rather than an answer.
    let gone = victim.query("SELECT 1").await;
    assert!(
        gone.rows.is_empty() && gone.tags.is_empty(),
        "the connection must be closed, not merely errored: {gone:?}"
    );
}

/// **A drop without `FORCE` leaves the other session alone**, which is the half that says the
/// termination came from the clause and not from dropping a database at all.
///
/// Measured in the same state on 19beta1: `55006`, and the victim goes on answering.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_drop_is_refused_and_the_other_session_lives() {
    let address = a_node().await;
    let mut admin = Client::connect(address).await;

    admin.query("CREATE DATABASE h113_plain").await;
    let mut victim = Client::connect_to(address, "h113_plain")
        .await
        .expect("a database that was just created is one a client can connect to");
    victim.query("SELECT 1").await;

    let refused = admin.query("DROP DATABASE h113_plain").await;
    assert_eq!(
        refused.sqlstate.as_deref(),
        Some("55006"),
        "a database with a session on it is not dropped without the clause: {refused:?}"
    );

    let still = victim.query("SELECT 1").await;
    assert_eq!(
        still.rows,
        vec![vec!["1".to_owned()]],
        "a refused drop must not have ended anything: {still:?}"
    );
}
