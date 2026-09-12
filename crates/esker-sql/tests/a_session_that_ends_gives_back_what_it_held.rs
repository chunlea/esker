//! **#83 — a session that ends gives back what it was holding, and the ordinary endings did not.**
//!
//! Run 128 (attempt 5, `29e66e60`) printed this into the node log at 07:40:58Z, one line after a
//! `DROP TABLE IF EXISTS "things"`:
//!
//! ```text
//! thread 'tokio-rt-worker' panicked at tokio-1.53.1/.../multi_thread/mod.rs:91:9:
//! Cannot start a runtime from within a runtime. ...
//!   2: <tokio::runtime::runtime::Runtime>::block_on
//!   3: <esker_proto::transport::client::BlockingTransport>::call
//!   ...
//!   7: <esker_client::txn::TxnClient>::begin
//!   9: <esker_sql::exec::Executor as core::ops::drop::Drop>::drop
//!  11: core::ptr::drop_glue::<esker_sql::pgwire::server::Work>
//!  12: <esker_sql::pgwire::server::Connection<...>>::run::{closure#0}
//! ```
//!
//! # What it is, which is not quite what the backtrace looks like
//!
//! **The panic is caught.** `BlockingTransport::call` asks `tokio` whether this thread is driving a
//! runtime the only way `tokio` will answer — it runs the `block_on` inside `catch_unwind` and reads
//! the refusal — and turns it into `ProtoError::internal`. Its own documentation says the message is
//! printed by the default hook on the way past. So the node was never in danger, which is why the
//! pass carried on, and the log line is the hook rather than a crash.
//!
//! **What the caught panic costs is silent, and that is the defect.** `Executor::drop` reads the
//! refusal as `let Ok(mut txn) = self.begin_txn() else { return; }` — so the whole of the
//! session's cleanup becomes a no-op: the temporary schema is not dropped, and the
//! `let _ = txn.rollback()` above it, which on a real cluster is a Percolator rollback over the
//! wire, is swallowed the same way. [ADR 0054](../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md)
//! does sanction leaving the schema behind — but for *"a backend that will not answer"*. Here the
//! backend answers perfectly; only the thread is wrong. The degraded path became the normal one.
//!
//! # Why the ordinary endings and not the others
//!
//! `Connection::run` leaves its loop seven ways, and **three of them already move `Work` onto the
//! blocking pool before dropping it**: the two `terminated()` arms and the idle-in-transaction
//! timeout. Those three are correct — `Runtime::block_on` on a `spawn_blocking` thread is exactly
//! what that pool is for, and it does not panic. The four that were not converted are the
//! *ordinary* ones: the client leaving, a `Terminate` message, a message that will not decode, and
//! every `?`. This repository has already written the lesson down once, in `exec::wait_for_row`:
//! *"Cleared here, at the one exit every path takes, rather than at each `return`: there are five
//! of those and the two that were missed are the two nobody thinks of as a wait ending."*
//!
//! # The trigger, measured rather than supposed
//!
//! `Executor::drop` returns at once unless `temp_schema` is `Some`, so only a session that made a
//! temporary relation reaches any of this. The 234,000-line node log of that pass holds **exactly
//! one** `CREATE TEMPORARY TABLE` — `things`, at 07:40:54.72 — and **exactly one** such panic,
//! 3.6 seconds later on the same connection. One temporary table in 178,138 statements, and it
//! leaked. This is not load-sensitive: load decides only when the connection ends.
//!
//! The test below is the whole path — three real stores, a real `Connection::run` over a duplex,
//! and a client that simply goes away, which is the commonest ending a connection has.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

mod cluster;

use cluster::{Cluster, TENANT};

/// Hands every connection a **real** executor over the cluster's three stores.
///
/// The harness's own `Cluster::session()` builds one of these directly; this is the same executor
/// reached the way the binary reaches it, through the connection loop — which is the half being
/// measured, because the defect is not in the executor but in where the connection drops it.
struct RealExecutors {
    backend: Arc<dyn esker_sql::backend::Backend>,
    catalog: Arc<esker_sql::catalog::Catalog>,
    sequences: Arc<esker_sql::sequence::Blocks>,
}

impl esker_sql::pgwire::server::Executors for RealExecutors {
    fn for_session(
        &self,
        _database: &str,
        identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn esker_sql::pgwire::session::Execute + Send>> {
        Ok(Box::new(
            esker_sql::exec::Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                identity,
            )
            .sharing_sequence_blocks(Arc::clone(&self.sequences)),
        ))
    }
}

/// A startup packet for `esker`/`esker`.
fn startup_packet() -> Vec<u8> {
    let mut body = 0x0003_0000u32.to_be_bytes().to_vec();
    for (name, value) in [("user", "esker"), ("database", "esker")] {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = u32::try_from(body.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    packet.extend_from_slice(&body);
    packet
}

/// A simple `Query` message.
fn query(sql: &str) -> Vec<u8> {
    let mut out = vec![b'Q'];
    let body_len = u32::try_from(sql.len() + 5).unwrap();
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    out
}

/// `(tag, body)` for every complete message in `bytes`.
fn frames(bytes: &[u8]) -> Vec<(char, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 5 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
                as usize;
        if at + 1 + length > bytes.len() {
            break;
        }
        out.push((bytes[at] as char, bytes[at + 5..at + 1 + length].to_vec()));
        at += 1 + length;
    }
    out
}

/// Every `pg_temp_*` schema the cluster is holding, read by a **second** session.
///
/// `Executor::ensure_temp_schema`'s own documentation is what makes this observable: *"The schema
/// is an ordinary schema record, so `pg_namespace` reports it"*. So a session that never made one
/// can say whether another session's is still standing.
fn temp_schemas(cluster: &Cluster) -> Vec<String> {
    let mut session = cluster.session();
    session
        .rows("SELECT nspname FROM pg_namespace")
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .filter(|name| name.starts_with("pg_temp_"))
        .collect()
}

/// **A client that goes away gives its temporary schema back.**
///
/// The commonest ending a connection has, and one of the four that never reached the blocking
/// pool. Before the fix the schema is still standing afterwards and nothing anywhere says so —
/// the only trace is `tokio`'s panic message in the log, from a panic that was caught.
#[test]
fn a_client_that_leaves_gives_its_temporary_schema_back() {
    let began = std::time::Instant::now();
    let cluster = Cluster::start();
    let executors = Arc::new(RealExecutors {
        backend: Arc::clone(&cluster.backend),
        catalog: Arc::clone(&cluster.catalog),
        sequences: Arc::clone(&cluster.sequences),
    });

    // A runtime of this test's own, because `Cluster::start` is not inside one and the connection
    // has to be: the whole question is which kind of thread `Work` is dropped on.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // Both built inside the runtime: a duplex and a spawn belong to one, and the whole question
    // here is which kind of thread a value ends up on.
    let (mut client, connection) = runtime.block_on(async move {
        let (client, server) = tokio::io::duplex(256 * 1024);
        let connection = tokio::spawn(async move {
            let mut connection = esker_sql::pgwire::server::Connection::new(
                server,
                esker_sql::pgwire::server::Config::default(),
            );
            connection.run(executors).await
        });
        (client, connection)
    });

    // Startup, then the one statement that makes any of this reachable at all.
    runtime.block_on(async {
        let mut input = startup_packet();
        input.extend_from_slice(&query(
            "CREATE TEMPORARY TABLE things (id bigserial primary key)",
        ));
        client.write_all(&input).await.unwrap();
        client.flush().await.unwrap();

        // Read until the second `ReadyForQuery`: one for startup, one for the statement. Reading a
        // fixed number of bytes would race the server's framing.
        let mut reply = Vec::new();
        let mut chunk = [0_u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let read = tokio::time::timeout_at(deadline, client.read(&mut chunk))
                .await
                .expect("the server never finished the CREATE")
                .unwrap();
            assert!(read > 0, "the server closed before answering the CREATE");
            reply.extend_from_slice(&chunk[..read]);
            if frames(&reply).iter().filter(|(tag, _)| *tag == 'Z').count() >= 2 {
                break;
            }
        }
        let errors: Vec<String> = frames(&reply)
            .into_iter()
            .filter(|(tag, _)| *tag == 'E')
            .map(|(_, body)| String::from_utf8_lossy(&body).replace('\0', " "))
            .collect();
        assert!(
            errors.is_empty(),
            "the CREATE TEMPORARY TABLE did not run, so this test would prove nothing: {errors:?}"
        );
    });

    // **The denominator.** Without this the assertion below passes on a session that never made a
    // schema at all — which is every session in this suite but one.
    let standing = temp_schemas(&cluster);
    assert_eq!(
        standing.len(),
        1,
        "the session did not make a temporary schema, so nothing below is being measured: \
         {standing:?}"
    );

    // **The ending.** The client simply goes away, which is `run`'s `let Some(..) = next else`
    // arm: `release_advisory_locks`, then `Work` is dropped right there on the runtime thread.
    drop(client);
    runtime
        .block_on(async move { tokio::time::timeout(Duration::from_secs(30), connection).await })
        .expect("the connection task never ended after the client left")
        .expect("the connection task panicked")
        .expect("the connection ended with an I/O error");

    let left_behind = temp_schemas(&cluster);
    // Printed and never asserted on: the whole path is milliseconds when the ending runs where
    // it can, and seconds when it does not — the refused calls retry — so the number is worth a
    // reader's eye and worth nothing as a bound.
    println!(
        "  standing while the session lived {standing:?} · left behind {left_behind:?} · {:?}",
        began.elapsed()
    );
    assert!(
        left_behind.is_empty(),
        "the session ended and its temporary schema is still standing: {left_behind:?}. \
         `Executor::drop` ran on a thread driving the runtime, where `BlockingTransport::call` \
         refuses rather than blocking, so `begin_txn` failed and the cleanup was skipped in \
         silence. Three of `Connection::run`'s seven exits move `Work` onto the blocking pool \
         first; this one — the client leaving — is not one of them."
    );
}
