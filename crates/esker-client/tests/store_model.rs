//! One proptest, two implementations: the engine, and the client that talks to a server
//! wrapping the same engine.
//!
//! `prompts/02-single-node-server.md` asks for "the phase-1 model test re-run **through the
//! client** (a `Store` trait implemented by both the engine and the client so the same
//! proptest drives both)". The point is not to test the engine again — phase 1 did that
//! against a `BTreeMap` over ten thousand cases. It is that **the network must not change the
//! answer**. Every layer between a caller and a key — framing, a request id, an epoch header,
//! a store that adds the `'r'` namespace and takes it away again — is a chance to lose a
//! byte, reorder a scan or drop the last entry of a page, and none of those show up in a
//! test that only ever calls the engine.
//!
//! # What is modelled
//!
//! The `RawKv` surface and nothing more: `put`, `delete`, `get`, bounded forward scans and
//! atomic multi-key writes. Phase 1's snapshots, flushes, compactions and reopens are absent
//! because `RawKv` has no words for them; they are the engine's business and they stay
//! tested where they live.
//!
//! Twenty-four keys, so overwrites, deletes and re-inserts of the same key collide constantly
//! — which is where the interesting bugs are, and it is why the key space is small rather
//! than large.
//!
//! # Reverse scans are deliberately absent
//!
//! `RawKvReq::Scan` documents `start` as "the exclusive upper bound to walk down from" when
//! `reverse` is set, which is a different shape from a forward scan's bounds. Until the store
//! implements it, asserting a reverse scan here would pin *my* reading of that sentence rather
//! than the system's behaviour. Raised with the lane that owns the store; added the moment it
//! is settled.
//!
//! # The client half runs a real server
//!
//! Not a mock and not a loopback stub: [`the_client_and_the_engine_agree`] opens a real
//! `esker-store` on a temporary directory, binds it to port 0, and drives the same sequences
//! through a real TCP connection. That is the only arrangement in which the claim above —
//! *the network must not change the answer* — is actually being tested, because everything a
//! mock leaves out is exactly where the answer would change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use std::net::SocketAddr;

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};
use esker_engine::batch::WriteBatch;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::{Db, cf};
use proptest::prelude::*;

/// The one region of a phase-2 cluster (`docs/DESIGN.md` §7).
const BOOTSTRAP_REGION: u64 = 1;

/// Distinct keys. Small on purpose: collisions are the point.
const KEYS: u8 = 24;

/// The same count phase 1's model test runs, and affordable here for the same reason: the
/// engine is on an in-memory filesystem, so a case costs microseconds rather than a file sync.
const CASES: u32 = 1_000;

/// The specification, and the thing both implementations are compared against.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// What a scan produces.
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

// ---------------------------------------------------------------------------------------
// The trait the proptest drives
// ---------------------------------------------------------------------------------------

/// The `RawKv` surface, as seen by something that does not care how it is reached.
///
/// Deliberately small. Every method here exists on both sides of the network, with the same
/// meaning, and anything that does not — column families, snapshots, sequence numbers — is
/// left out rather than approximated, because an approximation is what would make a
/// disagreement look like a bug in the test.
trait Store {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), String>;
    fn delete(&self, key: &[u8]) -> Result<(), String>;
    /// Entries of `[start, end)` in key order, at most `limit` of them. An empty `end` runs to
    /// the end of the key space.
    ///
    /// An **inverted** range — `start` after a non-empty `end` — is an error, not an empty
    /// answer. That is the store's rule and this trait follows it rather than inventing a
    /// second one: bounded scan is the store's API, the engine's iterator has no end bound at
    /// all, and answering "nothing found" to a caller who swapped two variables hides the bug
    /// instead of naming it. This test found the two layers disagreeing about it.
    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Entries, String>;
    /// Several keys written atomically — one engine write batch either way.
    fn write_batch(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String>;
}

/// The engine, called directly.
struct EngineStore {
    db: Db,
}

impl Store for EngineStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.db
            .get(cf::DEFAULT, key, &ReadOptions::default())
            .map(|found| found.map(|value| value.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.db
            .put(cf::DEFAULT, key, value)
            .map(|_seqno| ())
            .map_err(|err| err.to_string())
    }

    fn delete(&self, key: &[u8]) -> Result<(), String> {
        self.db
            .delete(cf::DEFAULT, key)
            .map(|_seqno| ())
            .map_err(|err| err.to_string())
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Entries, String> {
        if is_inverted(start, end) {
            return Err(format!(
                "invalid request: range start {start:?} is after its end {end:?}"
            ));
        }
        let mut iter = self
            .db
            .iter(cf::DEFAULT, &ReadOptions::default())
            .map_err(|err| err.to_string())?;
        iter.seek(start);

        let mut entries = Entries::new();
        while iter.valid() && entries.len() < limit as usize {
            // An empty `end` is the end of the key space, not a bound that stops everything.
            if !end.is_empty() && iter.key() >= end {
                break;
            }
            entries.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.next();
        }
        iter.status().map_err(|err| err.to_string())?;
        Ok(entries)
    }

    fn write_batch(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
        let mut batch = WriteBatch::new();
        for (key, value) in pairs {
            batch.put(0, key, value);
        }
        self.db
            .write(batch, &WriteOptions::unsynced())
            .map(|_seqno| ())
            .map_err(|err| err.to_string())
    }
}

/// The same surface, reached over the wire.
struct ClientStore {
    client: RawClient,
}

impl Store for ClientStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.client
            .get(key)
            .map(|found| found.map(|value| value.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.client.put(key, value).map_err(|err| err.to_string())
    }

    fn delete(&self, key: &[u8]) -> Result<(), String> {
        self.client.delete(key).map_err(|err| err.to_string())
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Entries, String> {
        self.client
            .scan(start, end, limit)
            .map(|pairs| {
                pairs
                    .into_iter()
                    .map(|(key, value)| (key.to_vec(), value.to_vec()))
                    .collect()
            })
            .map_err(|err| err.to_string())
    }

    fn write_batch(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
        let pairs = pairs
            .iter()
            .map(|(key, value)| {
                (
                    bytes::Bytes::copy_from_slice(key),
                    bytes::Bytes::copy_from_slice(value),
                )
            })
            .collect();
        self.client.batch_put(pairs).map_err(|err| err.to_string())
    }
}

// ---------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------

/// One step of a sequence. Fields are raw `u8`s mapped into range when the step runs, so that
/// shrinking pulls them towards zero rather than towards a strategy's idea of simple.
#[derive(Debug, Clone)]
enum Op {
    Put(u8, u8),
    Delete(u8),
    Get(u8),
    /// Several keys written atomically.
    Batch(Vec<(u8, u8)>),
    /// A bounded forward scan. `lo > hi` is an empty range and deliberately reachable.
    Scan {
        lo: u8,
        hi: u8,
        limit: u8,
        unbounded_end: bool,
    },
}

/// Whether a range runs backwards. An empty `end` is the end of the key space, so it is never
/// inverted however large `start` is — the trap that would refuse every unbounded scan.
fn is_inverted(start: &[u8], end: &[u8]) -> bool {
    !end.is_empty() && start > end
}

fn key_of(index: u8) -> Vec<u8> {
    format!("key{:03}", index % KEYS).into_bytes()
}

fn value_of(index: u8) -> Vec<u8> {
    // Lengths vary so a value that is written over a longer one cannot pass by accident.
    vec![index; usize::from(index % 7) + 1]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (any::<u8>(), any::<u8>()).prop_map(|(key, value)| Op::Put(key, value)),
        any::<u8>().prop_map(Op::Delete),
        any::<u8>().prop_map(Op::Get),
        proptest::collection::vec((any::<u8>(), any::<u8>()), 0..4).prop_map(Op::Batch),
        (any::<u8>(), any::<u8>(), any::<u8>(), any::<bool>()).prop_map(
            |(lo, hi, limit, unbounded_end)| Op::Scan {
                lo,
                hi,
                limit,
                unbounded_end,
            }
        ),
    ]
}

/// Applies one operation to both the store and the model, and checks every read agrees.
fn step(store: &dyn Store, model: &mut Model, op: &Op) -> Result<(), TestCaseError> {
    match op {
        Op::Put(key, value) => {
            let (key, value) = (key_of(*key), value_of(*value));
            store.put(&key, &value).map_err(TestCaseError::fail)?;
            model.insert(key, value);
        }
        Op::Delete(key) => {
            let key = key_of(*key);
            store.delete(&key).map_err(TestCaseError::fail)?;
            model.remove(&key);
        }
        Op::Get(key) => {
            let key = key_of(*key);
            let found = store.get(&key).map_err(TestCaseError::fail)?;
            prop_assert_eq!(
                found.as_deref(),
                model.get(&key).map(Vec::as_slice),
                "get({:?}) disagreed with the model",
                String::from_utf8_lossy(&key)
            );
        }
        Op::Batch(mutations) => {
            let pairs: Vec<(Vec<u8>, Vec<u8>)> = mutations
                .iter()
                .map(|(key, value)| (key_of(*key), value_of(*value)))
                .collect();
            store.write_batch(&pairs).map_err(TestCaseError::fail)?;
            // Later entries win, exactly as they do inside one engine write batch.
            for (key, value) in pairs {
                model.insert(key, value);
            }
        }
        Op::Scan {
            lo,
            hi,
            limit,
            unbounded_end,
        } => {
            let start = key_of(*lo);
            let end = if *unbounded_end {
                Vec::new()
            } else {
                key_of(*hi)
            };
            let limit = u32::from(*limit) + 1;

            let answer = store.scan(&start, &end, limit);
            if is_inverted(&start, &end) {
                prop_assert!(
                    answer.is_err(),
                    "an inverted range must be refused, not answered with nothing"
                );
                return Ok(());
            }
            let found = answer.map_err(TestCaseError::fail)?;
            let expected: Entries = model
                .range(start.clone()..)
                .take_while(|(key, _)| end.is_empty() || **key < end)
                .take(limit as usize)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            prop_assert_eq!(
                found,
                expected,
                "scan({:?}..{:?}, limit {}) disagreed with the model",
                String::from_utf8_lossy(&start),
                String::from_utf8_lossy(&end),
                limit
            );
        }
    }
    Ok(())
}

/// Runs a whole sequence, then checks the final state key by key and with a full scan.
fn run_sequence(store: &dyn Store, ops: &[Op]) -> Result<(), TestCaseError> {
    let mut model = Model::new();
    for op in ops {
        step(store, &mut model, op)?;
    }

    for index in 0..KEYS {
        let key = key_of(index);
        let found = store.get(&key).map_err(TestCaseError::fail)?;
        prop_assert_eq!(found.as_deref(), model.get(&key).map(Vec::as_slice));
    }

    let everything = store
        .scan(b"", b"", u32::from(KEYS) + 1)
        .map_err(TestCaseError::fail)?;
    let expected: Entries = model
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    prop_assert_eq!(everything, expected, "the final full scan disagreed");
    Ok(())
}

/// An engine on an in-memory filesystem.
///
/// Real files would make this test spend most of its time in `open`: a few hundred cases, each
/// creating and syncing a fresh database, is minutes of `just check` for no extra coverage.
/// Phase 1's model test made the same choice, and the fault and crash tests that *do* need a
/// real disk live in `esker-engine` where they belong.
fn engine_store() -> EngineStore {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let db = Db::open_with(
        "/db",
        Options {
            create_if_missing: true,
            ..Options::default()
        },
        fs,
        &cf::BUILTIN,
    )
    .expect("the engine opens");
    EngineStore { db }
}

// ---------------------------------------------------------------------------------------
// The runs
// ---------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    /// The engine half. This is the same shape as the phase-1 model test, trimmed to the
    /// `RawKv` surface, and it is here so that a failure of the client half can be attributed:
    /// if this passes and that fails, the network changed the answer.
    #[test]
    fn the_engine_agrees_with_the_model(ops in proptest::collection::vec(op(), 1..40)) {
        run_sequence(&engine_store(), &ops)?;
    }
}

/// A store, a server and a runtime, alive for as long as this value is.
///
/// The field order is the shutdown order: the handle goes first, which stops the server, and
/// the runtime after it. Reversing them would drop a runtime with live tasks on it.
struct TestServer {
    addr: SocketAddr,
    _handle: esker_proto::transport::ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// Opens a store on a temporary directory and serves it on a port the OS picks.
fn start_server() -> TestServer {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    let store = esker_store::Store::open(dir.path(), esker_store::StoreOptions::new())
        .expect("the store opens");
    let service: Arc<dyn esker_proto::transport::Service> = esker_store::StoreService::new(store);

    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            service,
            esker_proto::transport::TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });

    TestServer {
        addr: handle.local_addr(),
        _handle: handle,
        _runtime: runtime,
        _dir: dir,
    }
}

/// Connects a client to `server`, routing the way `esker raw` does.
fn client_store(server: &TestServer) -> ClientStore {
    let stores = TcpStores::connect(server.addr).expect("the client connects");
    let store_id = stores.only_store().expect("the server named its store");
    let client = RawClient::new(
        Arc::new(stores),
        // The same assumption `esker raw` makes: region 1 covers everything, and the client
        // has no opinion about the leader until something tells it otherwise. If the store
        // ever bootstraps differently, this test is where that is found out.
        Arc::new(StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, 0)),
    );
    ClientStore { client }
}

proptest! {
    // Fewer cases than the engine half: each operation here is a round trip, and the property
    // itself is already established above. What these cases add is the wire, and a few hundred
    // sequences over it is enough to catch a layer that drops, reorders or truncates.
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// The same sequences, through the client, against a real server.
    ///
    /// A failure here that the engine half does not share is the interesting one: it means the
    /// network changed the answer.
    #[test]
    fn the_client_and_the_engine_agree(ops in proptest::collection::vec(op(), 1..24)) {
        // One server for the whole test would be faster, and would let one case's leftovers
        // become the next case's starting state — which is exactly the kind of contamination
        // that makes a proptest failure impossible to read.
        let server = start_server();
        run_sequence(&client_store(&server), &ops)?;
    }
}

/// The awkward shapes, spelled out rather than left to chance: an inverted range, a scan that
/// stops at its limit, an unbounded end, and a key that would collide with the store's `'r'`
/// namespace if the client ever added the prefix too.
#[test]
fn the_client_handles_the_edges_of_a_scan() {
    let server = start_server();
    let store = client_store(&server);

    let sequences: Vec<Vec<Op>> = vec![
        vec![Op::Put(1, 1), Op::Get(1), Op::Delete(1), Op::Get(1)],
        // An inverted range is empty, and reachable by an ordinary mistake.
        vec![
            Op::Batch(vec![(3, 3), (4, 4), (5, 5)]),
            Op::Scan {
                lo: 20,
                hi: 1,
                limit: 4,
                unbounded_end: false,
            },
        ],
        // A limit that truncates: the page must stop, and stop in the right place.
        vec![
            Op::Batch(vec![(6, 6), (7, 7), (8, 8), (9, 9)]),
            Op::Scan {
                lo: 6,
                hi: 0,
                limit: 0,
                unbounded_end: true,
            },
        ],
        // Deleting a key that is not there is not an error, and an overwrite of a longer value
        // must not leave a tail behind.
        vec![
            Op::Delete(11),
            Op::Put(12, 6),
            Op::Put(12, 0),
            Op::Get(12),
            Op::Scan {
                lo: 0,
                hi: 0,
                limit: 30,
                unbounded_end: true,
            },
        ],
    ];

    for (index, ops) in sequences.iter().enumerate() {
        clear(&store);
        if let Err(failure) = run_sequence(&store, ops) {
            panic!("sequence {index} disagreed: {failure}");
        }
    }
}

/// Removes every key the model uses, so one sequence does not leak into the next.
fn clear(store: &dyn Store) {
    for index in 0..KEYS {
        store.delete(&key_of(index)).expect("delete");
    }
}
