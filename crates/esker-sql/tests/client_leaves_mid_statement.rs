//! **A client that dies mid-statement takes its session with it** — `debts-v1.1.md` #47.
//!
//! r1 isolated this on the real cluster in run 112b and the two arms are one number apart: a client
//! killed while **idle** is reaped in about eight seconds and its socket goes with it, while a
//! client killed **during** a statement leaves the session and a `CLOSE_WAIT` socket in place until
//! that statement ends on its own. `psql -c "SELECT pg_sleep(60)"` killed at once still held both
//! for the full sixty seconds; the 112 series' runner statements were killed by an 1800 s watchdog
//! and never ended at all, so about three thousand sessions accumulated permanently and
//! `pg_stat_activity` went 2,983 → 3,006 without ever coming down.
//!
//! The cause is that nobody reads the socket while a statement runs, so the peer's `FIN` sits in
//! the kernel until the connection loop next goes to read — which is after the executor returns.
//!
//! **Both arms are here, and the idle one is the control that makes the other mean something.** A
//! test with only the failing arm cannot tell a fixed lifecycle from a reaper that happens to run
//! on a timer: the idle arm passed before the fix and passes after it, so a change that made both
//! arms pass by waiting would be visible as the mid-statement arm taking as long as the reaper.
//!
//! The statement is sixty seconds long and the deadline is eight, which is the whole assertion:
//! the session cannot come back inside the deadline by the statement finishing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

/// A listener with a catalog that has been through a session once.
///
/// **The bootstrap is the point.** `listen` serves whatever catalog it is handed, and a bare
/// `Catalog::new()` has no `esker` database in it — a client connects, and every statement after
/// that answers nothing at all, `SELECT 1` included. `tests/cancel_over_a_socket.rs` builds a
/// `parity::Node` before it listens because it needs a table; this needs no table and still needs
/// the same first session.
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

/// One statement that must answer, with the whole reply in the failure.
async fn must_answer(client: &mut Client, sql: &str, rows: &[&str]) {
    let answer = client.query(sql).await;
    assert!(answer.error.is_none(), "{sql}: {answer:?}");
    let want: Vec<Vec<String>> = vec![rows.iter().map(|cell| (*cell).to_owned()).collect()];
    assert_eq!(answer.rows, want, "{sql}: {answer:?}");
}

/// How many sessions the node is holding — r1's `SELECT count(*) FROM pg_stat_activity`, **counted
/// on this side**.
///
/// The rows are counted here rather than by the server so that the instrument depends on one
/// feature and not two: the view, not the view *and* an aggregate over it. r1's own line is the
/// aggregate, on a real server that has both.
///
/// **It panics with the whole answer rather than defaulting.** A first draft counted with `count(*)`
/// and returned `-1` for anything it could not parse; the first run reported `left: 1, right: 0` —
/// an arithmetic puzzle standing in for whatever the server actually said, which had been thrown
/// away. A reader that swallows the answer cannot say what went wrong, and this is the only
/// instrument the test has.
async fn sessions(observer: &mut Client) -> i64 {
    let answer = observer.query("SELECT pid FROM pg_stat_activity").await;
    assert!(
        answer.error.is_none(),
        "pg_stat_activity did not answer: {answer:?}"
    );
    i64::try_from(answer.rows.len()).expect("a plausible session count")
}

/// Waits for the session count to reach `want`, and answers how long it took.
///
/// `None` when it never did, which is what the leak looks like.
async fn wait_for(observer: &mut Client, want: i64, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if sessions(observer).await == want {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// **The mid-statement arm** — r1's four lines, as far as an in-process listener can carry them.
///
/// `kill -KILL` on the client closes its socket, which is what dropping the connection does here;
/// what the node sees is the same `FIN` either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_dies_mid_statement_takes_its_session_with_it() {
    let address = bootstrapped().await;

    let mut observer = Client::connect(address).await;
    // One ordinary statement first: `connect` returns when `ReadyForQuery` arrives, and the
    // baseline should be taken from a session that has already been round the loop once.
    must_answer(&mut observer, "SELECT 1", &["1"]).await;
    let alone = sessions(&mut observer).await;
    assert!(alone >= 1, "the observer should be counted: {alone}");

    let mut victim = Client::connect(address).await;
    victim.send_query("SELECT pg_sleep(60)").await;
    // The statement has to be *running* before the socket goes, or the test would be measuring a
    // client that left between messages — which is the other arm.
    assert!(
        wait_for(&mut observer, alone + 1, Duration::from_secs(5))
            .await
            .is_some(),
        "the victim's session never appeared"
    );
    drop(victim);

    let back = wait_for(&mut observer, alone, Duration::from_secs(8)).await;
    assert!(
        back.is_some(),
        "the session outlived its client: still {} sessions after 8 s, and the statement it is \
         waiting on has 60 s to run",
        sessions(&mut observer).await
    );
}

/// **The idle arm, and it is the control**: this one passed before the fix too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_dies_between_statements_is_reaped_as_it_always_was() {
    let address = bootstrapped().await;

    let mut observer = Client::connect(address).await;
    must_answer(&mut observer, "SELECT 1", &["1"]).await;
    let alone = sessions(&mut observer).await;
    assert!(alone >= 1, "the observer should be counted: {alone}");

    let mut idle = Client::connect(address).await;
    idle.query("SELECT 1").await;
    assert_eq!(sessions(&mut observer).await, alone + 1);
    drop(idle);

    assert!(
        wait_for(&mut observer, alone, Duration::from_secs(8))
            .await
            .is_some(),
        "an idle client's session was not reaped"
    );
}

/// **What the client sent behind its statement is not lost**, which is the risk the watcher adds:
/// it has to read to learn the peer is gone, and a client is entitled to pipeline while its
/// statement runs. Two queries in one write, and both answers come back in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pipelined_message_survives_the_watching() {
    let address = bootstrapped().await;

    let mut client = Client::connect(address).await;
    client.send_query("SELECT 1").await;
    client.send_query("SELECT 2").await;
    for want in ["1", "2"] {
        let answer = client.read_until_ready().await;
        assert!(answer.error.is_none(), "{answer:?}");
        assert_eq!(answer.rows, vec![vec![want.to_owned()]], "{answer:?}");
    }
}
