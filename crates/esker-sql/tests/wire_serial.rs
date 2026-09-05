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

/// `Parse`/`Bind`/`Execute`/`Sync` for one statement with text-format parameters — the extended
/// protocol, which is what `ActiveRecord` speaks because it prepares by default.
///
/// The simple-query path above already answers correctly, and `range_test.rb` does not use it:
/// its fixture inserts are `INSERT INTO … VALUES ($1, $2, …)` prepared once and bound per row.
/// That is the one combination none of this lane's tests had crossed — the in-process `describe`
/// probe covered the executor, not the wire.
fn extended(sql: &str, params: &[&str]) -> Vec<u8> {
    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    // Parse: unnamed statement, no declared parameter types — the server infers them, which is
    // what `ActiveRecord` leaves it to do.
    let mut parse = vec![0u8];
    parse.extend_from_slice(sql.as_bytes());
    parse.push(0);
    parse.extend_from_slice(&0i16.to_be_bytes());

    // Bind: unnamed portal of the unnamed statement, every parameter in text format.
    let mut bind = vec![0u8, 0u8];
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind.extend_from_slice(&i16::try_from(params.len()).unwrap().to_be_bytes());
    for value in params {
        bind.extend_from_slice(&i32::try_from(value.len()).unwrap().to_be_bytes());
        bind.extend_from_slice(value.as_bytes());
    }
    bind.extend_from_slice(&0i16.to_be_bytes());

    // Execute the unnamed portal to completion, then Sync so the server answers ReadyForQuery.
    let mut execute = vec![0u8];
    execute.extend_from_slice(&0i32.to_be_bytes());

    let mut out = frame(b'P', &parse);
    out.extend_from_slice(&frame(b'B', &bind));
    // `Describe` the portal too, which is what a client that wants the row shape sends and the
    // step the first version of this hypothesis suspected of drawing a value.
    out.extend_from_slice(&frame(b'D', &[b'P', 0]));
    out.extend_from_slice(&frame(b'E', &execute));
    out.extend_from_slice(&frame(b'S', &[]));
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
        self.send(&query(sql)).await
    }

    /// Whatever bytes the caller built — the extended protocol's five messages, in one write.
    async fn send(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.client.write_all(bytes).await.unwrap();
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

/// **The fixture insert as `ActiveRecord` actually sends it**: prepared, with the id bound as a
/// parameter, over the wire, on the backend r1's node runs.
///
/// `range_test.rb`'s `setup` inserts its five fixtures with explicit ids through the extended
/// protocol — `Parse`/`Bind`/`Describe`/`Execute` — and only then does the `create!` that the test
/// reads back. Every probe of that path so far has been a simple query or an in-process executor;
/// this is the combination the file uses and none of them did.
///
/// The assertion is PostgreSQL's: an explicit id does not move the sequence, so `last_value` is
/// still the start, `is_called` is false, and the first row that leaves the id to the sequence
/// gets **1**.
#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_explicit_id_insert_does_not_move_the_sequence() {
    let node = Arc::new(Node {
        backend: Arc::new(esker_sql::backend::MemoryBackend::new()),
        catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        sequences: Arc::new(esker_sql::sequence::Blocks::default()),
        share: true,
    });
    let mut ddl = Wire::open(Arc::clone(&node)).await;
    let mut writer = Wire::open(Arc::clone(&node)).await;

    ddl.run("CREATE TABLE px (id bigserial PRIMARY KEY, v int8)")
        .await;

    // The five fixtures, prepared and bound exactly as the file sends them — and on a *second*
    // connection, because a pool need not give the schema change and the inserts the same one.
    for id in 101..=105 {
        let reply = writer
            .send(&extended(
                "INSERT INTO px (id, v) VALUES ($1, $2)",
                &[&id.to_string(), "0"],
            ))
            .await;
        assert!(
            !frames(&reply).iter().any(|(tag, _)| *tag == 'E'),
            "the prepared fixture insert failed: {}",
            String::from_utf8_lossy(&reply).replace('\0', "|")
        );
    }

    // What r1 reads after each CREATE, read here after the fixtures instead.
    let seq = ddl.run("SELECT last_value, is_called FROM px_id_seq").await;
    assert_eq!(
        first_value(&seq).as_deref(),
        Some("1"),
        "five prepared explicit-id inserts moved last_value"
    );

    // And the create!, which is what the test reads back.
    let created = writer
        .send(&extended(
            "INSERT INTO px (v) VALUES ($1) RETURNING id",
            &["1"],
        ))
        .await;
    assert_eq!(
        first_value(&created).as_deref(),
        Some("1"),
        "the first row that leaves the id to the sequence is 1, not a block on"
    );
}

/// **A dropped table's sequence goes with it — for the statement `ActiveRecord` actually sends.**
///
/// Run 88's instrumented pass: one `sequence_id` (603) served all 23 reserved blocks across
/// `range_test.rb`'s 46 tests, so `create_table force: true` was not re-creating the sequence and
/// the counter climbed all file. Every in-process test in `real_backend.rs` gets a *new* sequence
/// id per round and answers 1, which is why none of them could show this.
///
/// The difference has to be the statement or the path, so this uses both as sent: the wire, and
/// `DROP TABLE IF EXISTS "postgresql_ranges"` with the identifier **quoted**, which is how the
/// adapter writes every name. The unquoted spelling runs beside it, because if the quoting is the
/// trigger then the pair says so and a single case would not.
///
/// The assertion is on the **next** statement, never on the `DROP`'s own outcome — a name record
/// that outlives its object always reports far from its cause, and this repository has had five of
/// them, one of which was a `serial`'s sequence outliving its table.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_table_takes_its_sequence_for_both_spellings() {
    for quoted in [true, false] {
        let node = Arc::new(Node {
            backend: Arc::new(esker_sql::backend::MemoryBackend::new()),
            catalog: Arc::new(esker_sql::catalog::Catalog::new()),
            sequences: Arc::new(esker_sql::sequence::Blocks::default()),
            share: true,
        });
        let mut wire = Wire::open(Arc::clone(&node)).await;
        let name = if quoted { "\"pr\"" } else { "pr" };
        let spelling = if quoted { "quoted" } else { "unquoted" };

        let mut ids = Vec::new();
        for _ in 0..3 {
            wire.run(&format!("DROP TABLE IF EXISTS {name}")).await;
            wire.run(&format!(
                "CREATE TABLE {name} (id bigserial PRIMARY KEY, v int8)"
            ))
            .await;
            let reply = wire
                .run(&format!("INSERT INTO {name} (v) VALUES (1) RETURNING id"))
                .await;
            ids.push(first_value(&reply).unwrap_or_else(|| {
                panic!(
                    "{spelling}: no row came back: {}",
                    String::from_utf8_lossy(&reply).replace('\0', "|")
                )
            }));
        }
        assert_eq!(
            ids,
            ["1", "1", "1"],
            "{spelling}: a re-created table's sequence carried on from the dropped one — this is \
             `range_test.rb`'s climb, and `1, 33, 65` is what one surviving sequence produces"
        );
    }
}

/// **The schema change inside a transaction**, which is the shape `ActiveRecord`'s transactional
/// tests give every `setup`.
///
/// Both halves matter and they answer different questions. A `DROP`/`CREATE` that **commits** must
/// leave a sequence that starts at 1, like the autocommit case beside it. A `DROP`/`CREATE` that is
/// **rolled back** must leave the *original* table and its original sequence — and then the next
/// value is the one after whatever was already drawn, because a sequence is non-transactional here
/// exactly as it is on a real server.
///
/// Written because it is the one structural difference between every probe that answers 1 and the
/// file that climbs: run 88 showed one `sequence_id` serving all 46 tests, and a schema change that
/// does not survive its transaction is a way for that to happen without any drop being wrong.
#[tokio::test(flavor = "multi_thread")]
async fn a_schema_change_in_a_transaction_settles_the_sequence_either_way() {
    let node = Arc::new(Node {
        backend: Arc::new(esker_sql::backend::MemoryBackend::new()),
        catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        sequences: Arc::new(esker_sql::sequence::Blocks::default()),
        share: true,
    });
    let mut wire = Wire::open(Arc::clone(&node)).await;

    wire.run("CREATE TABLE tx (id bigserial PRIMARY KEY, v int8)")
        .await;
    let first = wire.run("INSERT INTO tx (v) VALUES (1) RETURNING id").await;
    assert_eq!(first_value(&first).as_deref(), Some("1"), "the first id");

    // Committed: the table is genuinely new, so its sequence is too.
    wire.run("BEGIN").await;
    wire.run("DROP TABLE IF EXISTS tx").await;
    wire.run("CREATE TABLE tx (id bigserial PRIMARY KEY, v int8)")
        .await;
    wire.run("COMMIT").await;
    let after_commit = wire.run("INSERT INTO tx (v) VALUES (2) RETURNING id").await;
    assert_eq!(
        first_value(&after_commit).as_deref(),
        Some("1"),
        "a committed DROP/CREATE gives a sequence that starts over"
    );

    // Rolled back: the table that survives is the one the transaction started with, and so is its
    // sequence — which has already handed out 1.
    wire.run("BEGIN").await;
    wire.run("DROP TABLE IF EXISTS tx").await;
    wire.run("CREATE TABLE tx (id bigserial PRIMARY KEY, v int8)")
        .await;
    wire.run("ROLLBACK").await;
    let after_rollback = wire.run("INSERT INTO tx (v) VALUES (3) RETURNING id").await;
    assert_eq!(
        first_value(&after_rollback).as_deref(),
        Some("2"),
        "a rolled-back DROP/CREATE leaves the original sequence, which continues"
    );
}

/// **An enum crosses both boundaries as its label**, in and out.
///
/// Run 89's `invalid input syntax for type smallint: "ok"` — 3 tests across `enum_test.rb` and
/// `invertible_migration_test.rb`. An enum's value is stored as its position
/// ([ADR 0050](../../../docs/adr/0050-an-enum-is-its-ordinal.md)), so the `Datum` is an `int2`, and
/// the ordinal leaked across the boundary in **both** directions:
///
/// * a **bound** parameter was read with the column's storage type, so `'ok'` was handed to the
///   `int2` input function — which is the run-89 error, and it is every `ActiveRecord` write to an
///   enum column, because `ActiveRecord` prepares;
/// * `RETURNING` rendered the ordinal straight out, so `INSERT … RETURNING current_mood` answered
///   `3` where `SELECT current_mood` answered `happy` — one renderer, two output paths, and only
///   the `SELECT` one used it.
///
/// A literal, a `::text` cast and a `WHERE` comparison were all already right, which is what made
/// the two gaps look like one and neither look like a leak.
#[tokio::test(flavor = "multi_thread")]
async fn an_enum_crosses_both_boundaries_as_its_label() {
    let node = Arc::new(Node {
        backend: Arc::new(esker_sql::backend::MemoryBackend::new()),
        catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        sequences: Arc::new(esker_sql::sequence::Blocks::default()),
        share: true,
    });
    let mut wire = Wire::open(Arc::clone(&node)).await;
    wire.run("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .await;
    wire.run("CREATE TABLE pe (id int8, current_mood mood)")
        .await;
    wire.run("INSERT INTO pe VALUES (1, 'ok')").await;

    for (sql, want) in [
        ("SELECT current_mood FROM pe", "ok"),
        ("SELECT current_mood::text FROM pe", "ok"),
        ("SELECT id FROM pe WHERE current_mood = 'ok'", "1"),
        (
            "INSERT INTO pe VALUES (3, 'happy') RETURNING current_mood",
            "happy",
        ),
    ] {
        let reply = wire.run(sql).await;
        assert_eq!(
            first_value(&reply).as_deref(),
            Some(want),
            "{sql}: {}",
            String::from_utf8_lossy(&reply).replace('\0', "|")
        );
    }

    // The bound form, which is the one run 89 failed on.
    let bound = wire
        .send(&extended(
            "INSERT INTO pe VALUES (4, $1) RETURNING current_mood",
            &["ok"],
        ))
        .await;
    assert!(
        !frames(&bound).iter().any(|(tag, _)| *tag == 'E'),
        "a bound enum label was refused: {}",
        String::from_utf8_lossy(&bound).replace('\0', "|")
    );
    assert_eq!(
        first_value(&bound).as_deref(),
        Some("ok"),
        "a bound label goes in as the label and comes back as the label"
    );
}
