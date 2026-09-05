//! **A re-created `bigserial` starts at 1 for a client on the wire**, which is where the suite is.
//!
//! Run 86's whole-file trace against the real binary: the created row's id is exactly
//! `1 + 32 * (k - 1)` for the k-th test of `range_test.rb` — one whole `SEQUENCE_BATCH` per
//! `create_table force: true`, in a single pass with nothing else running, where PostgreSQL gives
//! 1 at every position. r1's own twenty cycles of the identical DDL reset correctly every time,
//! and so does every in-process test in `real_backend.rs`: one connection, four concurrent
//! writers, and a `DROP`/`CREATE` on one session with the insert on another all answer 1.
//!
//! What none of those cross is the **wire**. `ActiveRecord` reaches the node through
//! `pgwire::server::Connection`, which asks `Executors::for_session` for an executor per
//! connection — and that is the one place a node-level allocator can be wired or not wired
//! ([ADR 0072](../../../docs/adr/0072-a-sequence-block-belongs-to-the-node-not-to-the-connection.md)).
//! So this drives two real protocol sessions over an in-memory duplex, with the wiring the binary
//! uses, and asks the question the file asks.
//!
//! The second test is the same thing with the sharing left out, which is how
//! `tests/psql_smoke.rs`'s `RealSessions` was built. It is here as the control: if the assertion
//! below ever starts passing for the wrong reason, this one says what the wrong reason looks like.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::exec::Executor;
use esker_sql::pgwire::server::{Config, Connection, Executors};
use esker_sql::pgwire::session::Execute;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// One tenant, as a single-database node has.
const TENANT: u64 = 1;

/// The node's executors, wired the way `bin/esker-sql.rs` wires them.
struct Node {
    backend: Arc<dyn esker_sql::backend::Backend>,
    catalog: Arc<esker_sql::catalog::Catalog>,
    sequences: Arc<esker_sql::sequence::Blocks>,
    /// Whether to hand each session the node's blocks, or let it keep its own.
    share: bool,
}

impl Executors for Node {
    fn for_session(
        &self,
        database: &str,
        identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        let executor = Executor::new(
            Arc::clone(&self.backend),
            Arc::clone(&self.catalog),
            TENANT,
            identity,
        )
        .serving_database(database);
        Ok(Box::new(if self.share {
            executor.sharing_sequence_blocks(Arc::clone(&self.sequences))
        } else {
            executor
        }))
    }
}

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

fn query(sql: &str) -> Vec<u8> {
    let mut out = vec![b'Q'];
    out.extend_from_slice(&u32::try_from(sql.len() + 5).unwrap().to_be_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    out
}

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

/// The first column of the first `DataRow` in a reply, as text.
fn first_value(reply: &[u8]) -> Option<String> {
    let (_, body) = frames(reply).into_iter().find(|(tag, _)| *tag == 'D')?;
    // `DataRow`: an `i16` column count, then per column an `i32` length and that many bytes.
    let length = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    let length = usize::try_from(length).ok()?;
    Some(String::from_utf8_lossy(&body[6..6 + length]).into_owned())
}

/// One protocol session: sends the startup packet and then each statement, answering the first
/// value of each reply.
struct Wire {
    client: tokio::io::DuplexStream,
}

impl Wire {
    async fn open(node: Arc<Node>) -> Wire {
        let (client, server) = tokio::io::duplex(256 * 1024);
        tokio::spawn(async move {
            let mut connection = Connection::new(server, Config::default());
            let _ = connection.run(node).await;
        });
        let mut wire = Wire { client };
        wire.client.write_all(&startup_packet()).await.unwrap();
        wire.client.flush().await.unwrap();
        wire.drain().await;
        wire
    }

    /// Reads whatever has arrived, up to the next `ReadyForQuery`.
    async fn drain(&mut self) -> Vec<u8> {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.client.read(&mut chunk),
            )
            .await
            .expect("the server stopped answering")
            .unwrap();
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..read]);
            if frames(&seen).iter().any(|(tag, _)| *tag == 'Z') {
                break;
            }
        }
        seen
    }

    async fn run(&mut self, sql: &str) -> Vec<u8> {
        self.client.write_all(&query(sql)).await.unwrap();
        self.client.flush().await.unwrap();
        self.drain().await
    }
}

/// The ids four rounds of the file's own `setup` hand out, over the wire.
async fn ids_over_four_rounds(share: bool) -> Vec<String> {
    let node = Arc::new(Node {
        backend: Arc::new(esker_sql::backend::MemoryBackend::new()),
        catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        sequences: Arc::new(esker_sql::sequence::Blocks::default()),
        share,
    });
    let mut ddl = Wire::open(Arc::clone(&node)).await;
    let mut writer = Wire::open(Arc::clone(&node)).await;

    let mut ids = Vec::new();
    for _ in 0..4 {
        ddl.run("DROP TABLE IF EXISTS wr").await;
        ddl.run("CREATE TABLE wr (id bigserial PRIMARY KEY, v int8)")
            .await;
        for id in 101..=105 {
            ddl.run(&format!("INSERT INTO wr (id, v) VALUES ({id}, 0)"))
                .await;
        }
        // **Both sessions draw from the same sequence**, which is what makes the wiring visible:
        // with the node's blocks they continue one run, and with a block each the second session
        // starts a whole batch further on. One session alone answers 1 either way, which is how a
        // per-connection block stayed invisible.
        for session in [&mut ddl, &mut writer] {
            let reply = session
                .run("INSERT INTO wr (v) VALUES (1) RETURNING id")
                .await;
            ids.push(first_value(&reply).unwrap_or_else(|| {
                panic!(
                    "no row came back: {}",
                    String::from_utf8_lossy(&reply).replace('\0', "|")
                )
            }));
        }
    }
    ids
}

/// **The wiring the binary uses**: every session draws from the node's blocks, so a re-created
/// sequence starts at 1 for whichever connection asks first.
#[tokio::test(flavor = "multi_thread")]
async fn a_re_created_serial_starts_at_one_over_the_wire() {
    let ids = ids_over_four_rounds(true).await;
    assert_eq!(
        ids,
        ["1", "2", "1", "2", "1", "2", "1", "2"],
        "two sessions on one node share a run and a re-created sequence starts it over — a value \
         from the old run is `range_test.rb`'s `1, 33, 65, 97`"
    );
}

/// **The control**, and the shape `tests/psql_smoke.rs`'s `RealSessions` had: a block per
/// connection.
///
/// Kept as a test rather than deleted, because it is the difference ADR 0072 is about and it is
/// invisible from a single connection — which is exactly why it survived a harness for so long.
/// The writer's first insert is 1 like any other, and what a per-connection block costs shows the
/// moment two sessions draw from one sequence.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_node_s_blocks_each_connection_takes_its_own() {
    let ids = ids_over_four_rounds(false).await;
    assert_eq!(
        ids,
        ["1", "33", "1", "33", "1", "33", "1", "33"],
        "with a block per connection the second session starts a whole batch on — this is the \
         `1, 33` the file shows, and it is what ADR 0072 removed from the product"
    );
}
