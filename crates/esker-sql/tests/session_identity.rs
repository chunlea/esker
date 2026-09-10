//! **Who is connected, from where, and since when** — `debts-v1.1.md` #47's second half.
//!
//! r1 met the first half on the real cluster: about three thousand sessions that never came back.
//! What made it a *leak* rather than a bill was that nothing in `pg_stat_activity` said whose the
//! sessions were — `usename`, `application_name`, `client_addr`, `client_port` and `backend_start`
//! were NULL or empty for every session this node had ever had. They could be counted and not
//! chased.
//!
//! Measured on PostgreSQL 19beta1, 2026-09-10, inside `BEGIN … ROLLBACK`, over a TCP connection:
//!
//! ```text
//! usename          esker             name
//! application_name psql              text     — and the empty string for a client that sets none
//! client_addr      192.168.215.1     inet     — not text, which is what this column used to say
//! client_hostname  NULL              text     — NULL without log_hostname, the default there too
//! client_port      32601             integer
//! backend_start    2026-09-10 12:36:05.692361+00   timestamptz
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

/// A listener whose catalog has been through a session once — see
/// `tests/client_leaves_mid_statement.rs` for why a bare `Catalog::new()` answers nothing.
async fn bootstrapped() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    drop(parity::Node::on(
        Arc::clone(&store),
        Arc::clone(&catalog),
        1,
        "esker",
        &[],
    ));
    listen(store, catalog).await
}

/// **The five columns, over a real socket.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_says_who_it_is_and_where_it_came_from() {
    let address = bootstrapped().await;
    let mut client = Client::connect_named(address, "esker", "", "b4-probe")
        .await
        .unwrap();

    let answer = client
        .query(
            "SELECT usename, application_name, client_addr, client_port > 0, \
             backend_start IS NOT NULL, client_hostname IS NULL \
             FROM pg_stat_activity WHERE application_name = 'b4-probe'",
        )
        .await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(
        answer.rows,
        vec![vec![
            "esker".to_owned(),
            "b4-probe".to_owned(),
            "127.0.0.1".to_owned(),
            "t".to_owned(),
            "t".to_owned(),
            "t".to_owned(),
        ]],
        "{answer:?}"
    );
}

/// **A client that sets no `application_name` shows the empty string**, which is what a real
/// server shows — not NULL, and not a stand-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_names_itself_nothing_shows_nothing() {
    let address = bootstrapped().await;
    let mut client = Client::connect(address).await;
    let answer = client
        .query("SELECT application_name = '' FROM pg_stat_activity WHERE client_port > 0")
        .await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(answer.rows, vec![vec!["t".to_owned()]], "{answer:?}");
}

/// **A session with no socket under it answers NULL for the address and the port**, which is the
/// nearest true thing this node can say and is what a real server answers for a Unix socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_process_session_has_no_address_to_give() {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let mut node = parity::Node::on(store, Arc::new(Catalog::new()), 1, "esker", &[]);
    assert_eq!(
        node.rows(
            "SELECT client_addr IS NULL, client_port IS NULL, backend_start IS NULL \
             FROM pg_stat_activity"
        ),
        vec![vec!["t".to_owned(), "t".to_owned(), "t".to_owned()]]
    );
}

/// **How far `backend_start` is from the clock every other timestamp in this node reads** — the
/// measurement the ruling of 2026-09-10 asked for, and the reason this file prints rather than
/// only asserts.
///
/// Invariant 6 gives the TSO's physical half as the only clock this node may read, and `now()`,
/// `clock_timestamp()` and everything else inside a statement obey it: they are derived from
/// `txn.start_ts()`. A **connection** has no transaction, so `backend_start` reads the wall clock
/// — ruled acceptable because the constitution forbids the wall clock for *ordering* and this
/// orders nothing.
///
/// **They are two clocks, and the measurement says they agree — which is not what I expected when
/// I wrote this test.** The prediction here was that a `MemoryBackend`'s TSO is a fixed constant,
/// so `now()` would be stale by however long ago it was chosen while `backend_start` was the real
/// time of day. Measured on 2026-09-10, one connection, in-process node:
///
/// ```text
/// backend_start (wall clock)  2026-09-10 12:40:57.110092+00
/// now() (this node's TSO)     2026-09-10 12:40:57.11+00
/// ```
///
/// **Ten microseconds.** This backend's oracle is wall-clock-derived too, so the two readings are
/// the same instant taken twice, and the difference is the work between them.
///
/// **What that does and does not establish.** It says the display column will not look absurd
/// beside `now()` on this configuration. It does *not* say the two are one clock: a session's
/// `backend_start` is taken once, on the connection task, from `SystemTime`, while every timestamp
/// inside a statement comes from `txn.start_ts()` — a different source that a real cluster serves
/// from the placement driver and over the wire. Nothing compares them, and nothing may: the gap is
/// unbounded in principle and this number is one sample of one configuration. It is printed rather
/// than asserted for exactly that reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_start_and_the_nodes_own_clock_are_two_clocks() {
    let address = bootstrapped().await;
    let mut client = Client::connect_named(address, "esker", "", "b4-clock")
        .await
        .unwrap();
    let answer = client
        .query(
            "SELECT backend_start, now() FROM pg_stat_activity \
             WHERE application_name = 'b4-clock'",
        )
        .await;
    assert!(answer.error.is_none(), "{answer:?}");
    let row = answer.rows.first().expect("one row");
    println!("\n  backend_start (wall clock)  {}", row[0]);
    println!("  now() (this node's TSO)     {}\n", row[1]);
    // The only assertion is that they are both real timestamps and that the wall-clock one is
    // *this* year: the size of the gap is a property of the backend under the node, and pinning
    // it would pin the fixture rather than the rule.
    assert!(row[0].starts_with("20"), "{answer:?}");
    assert!(!row[1].is_empty(), "{answer:?}");
}
