//! **#80 — a reverse `RawKv` scan comes back whole, and its bounds mean what the forward ones do.**
//!
//! #79 gave every other scan the rule that only an **empty** batch ends a range: a batch is
//! bounded by the caller's page and by a byte budget no caller can see, so a short one says
//! nothing. `RawClient::scan_with` paged on that rule forward and not backward, and the reason was
//! a disagreement between two pieces of code rather than an omission:
//!
//! * `RawClient::regions_of` hands out its pieces as `(low, high)` in key order **whichever way
//!   the walk goes** — it routes on the low key and clamps with `clamp_end`, and the CLI's
//!   `esker raw scan <start> --end <end> --reverse` writes an ordinary range — so the client's
//!   own API is `(low, high)` in both directions;
//! * `rawkv::scan_bounds` reads a **reverse** `RawKvReq::Scan`'s `start` field as the *exclusive
//!   upper* bound and its `end` as the inclusive lower one, which is what the wire's own doc says
//!   and what `server.rs`'s `a_reverse_scan_walks_down_from_its_upper_bound` pins.
//!
//! `scan_region` built the request from `(from, to)` verbatim, so a reverse request went out with
//! the piece's **low** key in the field the store reads as the upper bound. The one caller in the
//! repository passes two empty bounds, where the swap cannot show itself.
//!
//! So the pairing is settled here and in `scan_region`: **the client's arguments are `(low, high)`
//! and the wire's reverse fields are `(upper, lower)`, and the one place that builds a request is
//! the one place that swaps them.** No wire change — the store's convention is untouched and is
//! what the tests above pin.
//!
//! Measured against a real store over a real socket, because the bounds are the store's rule and a
//! scripted transport would answer whatever it was told.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};

/// More keys than one page carries, so a walk that stops on a short batch stops early.
/// `DEFAULT_SCAN_LIMIT` is 1,024 and a `limit` of zero asks for the whole range.
const KEYS: usize = 1_500;

/// The region the store bootstraps with.
const BOOTSTRAP_REGION: u64 = 1;

struct TestServer {
    addr: SocketAddr,
    _handle: esker_proto::transport::ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

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

fn client(server: &TestServer) -> RawClient {
    let stores = TcpStores::connect(server.addr).expect("the client connects");
    let store_id = stores.only_store().expect("the server named its store");
    RawClient::new(
        Arc::new(stores),
        Arc::new(StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, 0)),
    )
}

fn key(at: usize) -> Vec<u8> {
    format!("k{at:06}").into_bytes()
}

/// **The whole range, backwards.** One region, more keys than a page, and a `limit` of zero —
/// which means every pair in the range.
///
/// Before this the answer was one page at best: the walk asked each piece once, so a batch that
/// was merely *short* ended it. With the fields the wrong way round it was worse than short.
#[test]
fn a_reverse_scan_of_a_whole_region_comes_back_whole() {
    let server = start_server();
    let client = client(&server);
    for at in 0..KEYS {
        client.put(&key(at), b"v").expect("the write lands");
    }

    let pairs = client
        .scan_reverse(b"k", b"l", 0)
        .expect("the scan answers");
    let found: Vec<Vec<u8>> = pairs.iter().map(|(key, _)| key.to_vec()).collect();
    let want: Vec<Vec<u8>> = (0..KEYS).rev().map(key).collect();
    println!(
        "  {KEYS} keys · {} returned · page {}",
        found.len(),
        esker_client::wire::DEFAULT_SCAN_LIMIT
    );
    assert_eq!(found, want, "every key, from the top down");
}

/// **A bounded reverse scan takes the top of the range**, which is the half a "return everything"
/// fix could pass without meaning it.
#[test]
fn a_bounded_reverse_scan_takes_the_highest_keys() {
    let server = start_server();
    let client = client(&server);
    for at in 0..10 {
        client.put(&key(at), b"v").expect("the write lands");
    }

    let pairs = client
        .scan_reverse(b"k", b"l", 3)
        .expect("the scan answers");
    assert_eq!(
        pairs
            .iter()
            .map(|(key, _)| key.to_vec())
            .collect::<Vec<_>>(),
        vec![key(9), key(8), key(7)],
        "the three highest, descending"
    );
}

/// **The bounds are a range and the upper one is exclusive**, the same as forward.
///
/// The store's own rule, pinned in `esker-store`'s `a_reverse_scan_walks_down_from_its_upper_bound`
/// — `[low, high)` walked downwards — and this is the client saying the same thing through its own
/// `(low, high)` arguments.
#[test]
fn a_reverse_scan_excludes_its_upper_bound_and_includes_its_lower() {
    let server = start_server();
    let client = client(&server);
    for at in 0..6 {
        client.put(&key(at), b"v").expect("the write lands");
    }

    let pairs = client
        .scan_reverse(&key(1), &key(4), 0)
        .expect("the scan answers");
    assert_eq!(
        pairs
            .iter()
            .map(|(key, _)| key.to_vec())
            .collect::<Vec<_>>(),
        vec![key(3), key(2), key(1)],
        "`[k1, k4)` downwards: the lower bound is in and the upper one is out"
    );
    // And the forward scan of the same range is the same keys the other way up, which is what
    // says the two directions are reading one pair of arguments rather than two conventions.
    let forward = client.scan(&key(1), &key(4), 0).expect("the scan answers");
    assert_eq!(
        forward
            .iter()
            .map(|(key, _)| key.to_vec())
            .collect::<Vec<_>>(),
        vec![key(1), key(2), key(3)]
    );
}
