//! **Two nodes cannot deadlock at prewrite, and this is why** —
//! `docs/plans/cross-node-deadlock.md`.
//!
//! Debt #4 is recorded as "cross-node deadlock detection … needs a PD-held graph", sized large.
//! Reading the commit path says there is very likely nothing to detect: locks are taken in
//! **ascending key order**, so a wait-for chain that only ascends cannot return to where it
//! started. That is the classic ordered-locking argument, and here it falls out of a data
//! structure rather than being imposed — the write buffer is a `BTreeMap`, the primary is its
//! first key, and the secondaries follow sorted.
//!
//! An argument that lives in a data structure is an argument that can stop holding without anybody
//! noticing. These two tests are what would notice.
//!
//! * The first is the property from outside: two nodes committing the same two keys in **opposite
//!   application order**, exactly one winning, neither hanging. If `primary()` ever became the
//!   first key *written* rather than the smallest, node A would take `a` and wait for `z` while
//!   node B took `z` and waited for `a` — and both would fail where one should win.
//! * The second is the property from inside, so a failure names the cause instead of the symptom.
//!
//! Keys `a` and `z` are deliberately either side of the region boundary: the prewrite is grouped
//! by region, and a per-region batch is the other place an ordering could be lost.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use bytes::Bytes;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{CountingOracle, TcpStores, TimestampOracle, TxnClient};
use esker_proto::{Epoch, Peer, Region, ServerHandle, TransportConfig};
use esker_store::{Store, StoreOptions, StoreService};

/// The key the two regions divide at: `a` is region 1's and `z` is region 2's.
const BOUNDARY: &[u8] = b"m";

/// Two stores, and **two clients over them** — which is what two SQL nodes are.
struct Nodes {
    one: TxnClient,
    two: TxnClient,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    _runtime: tokio::runtime::Runtime,
}

fn start_store(runtime: &tokio::runtime::Runtime, id: u64) -> (ServerHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: id,
            ..StoreOptions::new()
        },
    )
    .expect("the store opens");
    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            StoreService::new(store),
            TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });
    (handle, dir)
}

fn routes() -> Arc<dyn RegionResolver> {
    Arc::new(RegionTable::from_routes([
        Route {
            region: Region {
                id: 1,
                start_key: Bytes::new(),
                end_key: Bytes::copy_from_slice(BOUNDARY),
                peers: vec![Peer::voter(1, 1)],
                epoch: Epoch::INITIAL,
            },
            leader: Some(Peer::voter(1, 1)),
        },
        Route {
            region: Region {
                id: 2,
                start_key: Bytes::copy_from_slice(BOUNDARY),
                end_key: Bytes::new(),
                peers: vec![Peer::voter(2, 2)],
                epoch: Epoch::INITIAL,
            },
            leader: Some(Peer::voter(2, 2)),
        },
    ]))
}

fn nodes() -> Nodes {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a runtime");
    let (low, low_dir) = start_store(&runtime, 1);
    let (high, high_dir) = start_store(&runtime, 2);
    let addresses = [low.local_addr(), high.local_addr()];

    // **One oracle, two clients.** Two independent counters would hand the same timestamp to two
    // different transactions, which is not two nodes but a broken cluster (`CLAUDE.md` invariant
    // 6). Each client gets its own connections and its own jitter, which is what makes it another
    // node.
    let oracle: Arc<dyn TimestampOracle> = Arc::new(CountingOracle::starting_at(1_000));
    let client = |seed: u64| {
        let stores = TcpStores::connect_all(&addresses, TransportConfig::new())
            .expect("a client connects to both stores");
        TxnClient::on_router(
            Arc::new(Router::with_options(
                Arc::new(stores),
                routes(),
                ClientOptions {
                    jitter_seed: Some(seed),
                    ..ClientOptions::default()
                },
            )),
            Arc::clone(&oracle),
        )
        // **No lock-resolution budget at all**, which is what makes the second test deterministic
        // rather than a stopwatch. A transaction that meets somebody's lock fails immediately
        // instead of waiting it out, so an ordering that still leaves one winner leaves it because
        // the acquisition order is total — not because one side out-waited the other. Measured
        // against the broken ordering: with the budget it is a four-second stall that usually
        // resolves to one winner anyway, and with the budget at zero it is deterministically two
        // losers.
        .with_max_lock_resolutions(0)
    };
    Nodes {
        one: client(11),
        two: client(22),
        _handles: vec![low, high],
        _dirs: vec![low_dir, high_dir],
        _runtime: runtime,
    }
}

/// **The property from inside**: the primary is the smallest key written, whatever order it was
/// written in.
///
/// The primary is prewritten alone and first, and the secondaries follow it out of the same
/// `BTreeMap` — so this one fact is what puts every transaction's lock acquisition in the same
/// total order, and it is the whole of the argument that a cycle cannot form.
#[test]
fn the_primary_is_the_smallest_key_of_the_write_set() {
    let nodes = nodes();
    for order in [
        ["z", "m0", "a"],
        ["a", "z", "m0"],
        ["m0", "a", "z"],
        ["z", "a", "m0"],
    ] {
        let mut txn = nodes.one.begin().expect("a transaction begins");
        for key in order {
            txn.put(key.as_bytes(), b"v");
        }
        assert_eq!(
            txn.primary().map(|key| key.to_vec()),
            Some(b"a".to_vec()),
            "written {order:?}"
        );
    }
}

/// **The property from outside**: two nodes, the same two keys, opposite application order.
///
/// Exactly one commits. The other meets the winner's lock, rolls back what it placed and fails —
/// it never sits on one key waiting for the other, which is the second half of why there is no
/// cycle to detect.
#[test]
fn two_nodes_writing_the_same_keys_in_opposite_orders_leave_exactly_one_winner() {
    let nodes = nodes();
    let (says, hears) = channel();

    let mut ascending = nodes.one.begin().expect("a transaction begins");
    ascending.put(b"a", b"one");
    ascending.put(b"z", b"one");

    let mut descending = nodes.two.begin().expect("a transaction begins");
    descending.put(b"z", b"two");
    descending.put(b"a", b"two");

    std::thread::scope(|scope| {
        for (name, txn) in [("ascending", ascending), ("descending", descending)] {
            let says = says.clone();
            scope.spawn(move || {
                let outcome = txn.commit();
                says.send((name, outcome.is_ok())).unwrap();
            });
        }
        // **Neither hangs**, which is the half a cycle would break: a deadlock here is two
        // transactions waiting on each other with no detector to break the tie, and it would show
        // up as this timing out rather than as a wrong answer.
        let mut winners = Vec::new();
        for _ in 0..2 {
            let (name, committed) = hears
                .recv_timeout(Duration::from_secs(60))
                .expect("both commits answer rather than deadlocking");
            if committed {
                winners.push(name);
            }
        }
        assert_eq!(
            winners.len(),
            1,
            "exactly one of the two commits, not both and not neither: {winners:?}"
        );
    });
}
