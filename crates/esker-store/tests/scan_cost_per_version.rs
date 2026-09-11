//! #58 — the same scan gets dearer as a key's history grows, and it must not.
//!
//! r1's isolation established the shape and ruled out everything above the KV interface: with the
//! catalog size pinned, four passes of one file emitted **identical** work to the digit — 33.5
//! reads, 27.3 scans, 61.4 round trips per statement, reproduced in two arms — while the
//! milliseconds per statement rose 34.4 → 52.6 (+53%) on a load-matched pair. A flush did not stop
//! it (sst 0 → 12 → 21), a compaction did not (21 → 20), and restarting the store before each pass
//! did not. Nothing reclaims the versions a write supersedes, and **every read walks all of them**.
//!
//! # Why this counts entries instead of timing them
//!
//! A timer on a machine shared with a gate and three other lanes measures the machine; r1's arm C
//! lost its milliseconds to exactly that and won on its counts. So the assertion here is
//! `esker.entries-stepped` — stored entries an iterator examined — which is deterministic, is the
//! quantity the hypothesis is about, and cannot be confounded by load.
//!
//! # What it asserts
//!
//! The same key set at V = 1, 16 and 256 versions each. The scan's **answer** is identical at every
//! V: one row per key, because the scan collects distinct user keys into a set. So if the steps
//! grow with V, the extra work produced nothing — which is the whole of #58.
//!
//! `O(keys × V)` is what a version-by-version walk costs and `O(keys × log n)` is what seeking to
//! each key's successor costs. The bound below is generous on purpose: it is not trying to pin a
//! constant, it is trying to tell a curve that tracks V from one that does not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp};
use esker_store::{RegionState, Store, StoreOptions};

/// Distinct user keys. Small, because what is varied is the versions per key and a bigger key set
/// would only make every column of the table bigger by the same factor.
const KEYS: usize = 20;

/// The three depths. 1 is the control — a store with no history at all — and 256 is far enough
/// from 16 that a linear term cannot hide inside the noise of a constant one.
const VERSIONS: [usize; 3] = [1, 16, 256];

fn key(at: usize) -> Bytes {
    Bytes::from(format!("k{at:04}"))
}

/// Writes `versions` committed versions of every one of [`KEYS`], through the real transactional
/// path — prewrite then commit, one transaction per version — so the records on disk are the ones
/// a running cluster leaves behind rather than ones this test invented.
fn fill(store: &Arc<Store>, state: &Arc<RegionState>, versions: usize) {
    let mut ts = 10_u64;
    for round in 0..versions {
        for at in 0..KEYS {
            let start_ts = ts;
            let commit_ts = ts + 1;
            ts += 2;
            let value = Bytes::from(format!("v{round:04}"));
            let prewrite = TxnKvReq::Prewrite {
                start_ts,
                primary: key(at),
                ttl_ms: 10_000,
                mutations: vec![esker_proto::TxnMutation::Put {
                    key: key(at),
                    value,
                    // No read-your-writes check: this is a blind overwrite, which is what a
                    // history of versions is made of.
                    read_ts: None,
                }],
            };
            let answered = store.handle_txn(state, prewrite).unwrap();
            assert!(
                matches!(answered, TxnKvResp::Prewrite { .. }),
                "prewrite refused: {answered:?}"
            );
            let commit = TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys: vec![key(at)],
            };
            let answered = store.handle_txn(state, commit).unwrap();
            assert!(
                matches!(answered, TxnKvResp::Commit { .. }),
                "commit refused: {answered:?}"
            );
        }
    }
}

/// One full scan of the key range, and the entries the engine stepped over to answer it.
fn scan_cost(store: &Arc<Store>, state: &Arc<RegionState>, read_ts: u64) -> (usize, u64) {
    let before: u64 = store
        .property("esker.entries-stepped")
        .expect("the engine counts the entries a scan steps over")
        .parse()
        .unwrap();
    let answered = store
        .handle_txn(
            state,
            TxnKvReq::Scan {
                start: key(0),
                end: Bytes::from("l"),
                limit: 1_000,
                ts: read_ts,
                reverse: false,
            },
        )
        .unwrap();
    let TxnKvResp::Scan { pairs } = answered else {
        panic!("a scan answered {answered:?}");
    };
    let after: u64 = store
        .property("esker.entries-stepped")
        .unwrap()
        .parse()
        .unwrap();
    (pairs.len(), after - before)
}

/// One point read of one key, and the entries the engine stepped over to answer it.
fn get_cost(store: &Arc<Store>, state: &Arc<RegionState>, read_ts: u64) -> u64 {
    let before: u64 = store
        .property("esker.entries-stepped")
        .unwrap()
        .parse()
        .unwrap();
    store
        .handle_txn(
            state,
            TxnKvReq::Get {
                key: key(0),
                ts: read_ts,
            },
        )
        .unwrap();
    let after: u64 = store
        .property("esker.entries-stepped")
        .unwrap()
        .parse()
        .unwrap();
    after - before
}

/// **The assertion #58 turns on.** The rows a scan returns do not depend on how many versions each
/// key has; the entries it steps over must not either.
///
/// # It took two fixes, in two layers, and the second was the larger
///
/// `user_keys_in`'s **`write` CF loop** stepped one MVCC version at a time; it now seeks past each
/// key's remaining versions, using the prefix successor `key::version_range` already computes.
/// That took V=256 from 15,380 entries to 10,280.
///
/// The rest was **not** `txnkv`'s at all, and only the counter found it: the `lock` CF half. A lock
/// key has no timestamp suffix, so there is nothing to seek past — the entries under it are the
/// *engine's* own superseded versions, because every prewrite puts a lock and every commit deletes
/// it. `DbIterator::find_next` stepped all `2V` of them to decide the key was currently absent. It
/// now steps eight and then seeks past the key, which is what takes this flat.
///
///     V=1     80        V=16    220        V=256   220
///
/// Flat from 16 upward, and the 80 → 220 step is the threshold being paid once per key —
/// 20 keys × 8 steps — rather than a term that grows.
#[test]
fn a_scan_costs_the_same_however_deep_the_history() {
    let mut measured = Vec::new();
    for versions in VERSIONS {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
        let state = store.regions().get(1).expect("the bootstrapped region");
        fill(&store, &state, versions);
        // A read timestamp above every commit, which is what a fresh transaction gets and the
        // case that has to walk the most.
        let (rows, stepped) = scan_cost(&store, &state, u64::MAX / 2);
        let one_read = get_cost(&store, &state, u64::MAX / 2);
        assert_eq!(
            rows, KEYS,
            "the answer must not change with the history: {versions} versions returned {rows} rows"
        );
        println!(
            "  V={versions:4}   rows={rows}   scan stepped={stepped}   one point read={one_read}"
        );
        measured.push((versions, stepped));
        store.stop();
    }

    let (_, at_one) = measured[0];
    let (deepest, at_deepest) = measured[2];
    // Generous, and deliberately not a constant: what separates the two shapes is whether the
    // count tracks V at all. A version-by-version walk makes this ratio about 256; seeking to each
    // key's successor makes it about 1.
    assert!(
        at_deepest < at_one * 4,
        "the scan's cost tracks the history: {at_one} entries at V=1 against {at_deepest} at \
         V={deepest}, for the same {KEYS} rows.\n  measured: {measured:?}"
    );
}

/// **The limit means what it meant.** Seeking past a key's versions changes which entries are
/// walked and must not change which rows come back or how many.
///
/// The old loop could stop part-way through a key's versions once the set was full; the new one
/// stops right after taking the last key it is allowed. The same keys either way, and this says so
/// against a store deep enough that the difference would show.
#[test]
fn a_limit_returns_the_same_rows_it_always_did() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, 8);

    let answered = store
        .handle_txn(
            &state,
            TxnKvReq::Scan {
                start: key(0),
                end: Bytes::from("l"),
                limit: 3,
                ts: u64::MAX / 2,
                reverse: false,
            },
        )
        .unwrap();
    let TxnKvResp::Scan { pairs } = answered else {
        panic!("a scan answered something else");
    };
    let keys: Vec<Bytes> = pairs.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(
        keys,
        vec![key(0), key(1), key(2)],
        "the limit took the first three keys"
    );
    store.stop();
}

/// **The upper bound is still exclusive, and still compared the same way.** A seek that overshot
/// would drop a key inside the range; one that undershot would return a key outside it.
///
/// `end` is a key that exists, so it also pins the half-open edge: the row *at* `end` is out.
#[test]
fn the_range_is_the_same_half_open_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, 8);

    let answered = store
        .handle_txn(
            &state,
            TxnKvReq::Scan {
                start: key(5),
                end: key(10),
                limit: 1_000,
                ts: u64::MAX / 2,
                reverse: false,
            },
        )
        .unwrap();
    let TxnKvResp::Scan { pairs } = answered else {
        panic!("a scan answered something else");
    };
    let keys: Vec<Bytes> = pairs.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(
        keys,
        (5..10).map(key).collect::<Vec<Bytes>>(),
        "[k0005, k0010) is five rows and does not include k0010"
    );
    store.stop();
}

/// **A key that is only locked is still found.** The `lock` CF half of `user_keys_in` is what sees
/// a key whose transaction prewrote it and has not resolved it, and a scan that missed it would
/// answer without the row *and* report no lock — the silent wrong answer `scan`'s own header
/// promises not to have.
///
/// It is here because the fix above is in the loop next door: the two halves share a `keys` set
/// and a ceiling, and a change to how one of them walks must leave the other's contribution
/// intact.
#[test]
fn a_key_that_is_only_locked_still_stops_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, 4);

    // A prewrite with no commit: a lock and no new `write` record.
    let answered = store
        .handle_txn(
            &state,
            TxnKvReq::Prewrite {
                start_ts: 900_000,
                primary: key(7),
                ttl_ms: 60_000,
                mutations: vec![esker_proto::TxnMutation::Put {
                    key: key(7),
                    value: Bytes::from_static(b"uncommitted"),
                    read_ts: None,
                }],
            },
        )
        .unwrap();
    assert!(matches!(answered, TxnKvResp::Prewrite { .. }));

    let refused = store.handle_txn(
        &state,
        TxnKvReq::Scan {
            start: key(0),
            end: Bytes::from("l"),
            limit: 1_000,
            ts: 950_000,
            reverse: false,
        },
    );
    let error = refused.expect_err("a scan meeting an unresolved lock refuses");
    assert!(
        error.to_string().contains("lock"),
        "the refusal must name the lock so the client can resolve it: {error}"
    );
    store.stop();
}
