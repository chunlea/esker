//! **The P0 of run 127 attempt 4, and it is not the collector.** A transactional `Scan` over a
//! range holding more distinct keys than `txnkv::MAX_SCAN_LIMIT` answers with a **prefix** of the
//! range and no sign that it did.
//!
//! # What the preserved data directory says
//!
//! `esker-rails-harness/results/run-127/cluster-data/node-1` was kept because the catalog in it is
//! reproducibly corrupt: `a name points at table 34755, which is not there`, five files in a row,
//! five different tables, ids climbing 34755 · 34787 · 34834 · 34881 · 34928.
//!
//! Every one of those table records **is on disk and is readable**. 34755's is a `Kind::Put` in the
//! `write` column family with its `TableDef` inline — `cake_designers`, 94 bytes, nothing spilled,
//! no `Delete` above it. Nothing collected it.
//!
//! What is true of all five is their **position**. Tenant 1 holds **8,342** distinct table-record
//! keys in that database; all but 278 are keys whose newest version is a `Delete`, left by the
//! suite's `DROP TABLE`s and immortal by design (the newest version below the safepoint survives).
//! `txnkv::user_keys_in` walks the `write` family in key order and stops at
//! `ceiling = MAX_SCAN_LIMIT` **distinct keys, live or dead**, so the last table id a scan of that
//! catalog can reach is **34617**. The five sit at positions 8255, 8271, 8289, 8307 and 8325 — every
//! one of them past the 8,192nd key. The first id past the ceiling is 34619, which is why the pass
//! broke part-way through one file's schema load and every later file broke on its first scan.
//!
//! # Why the ceiling is not the bound it was sized to be
//!
//! `user_keys_in`'s own comment says "enough to fill any limit the caller could have asked for",
//! and that reasoning is right about the *caller's* denominator and wrong about its own: the
//! caller's `limit` counts **pairs returned** and the ceiling counts **keys with any version**. The
//! two differ by exactly the dead keys, and a range accumulates those for ever. Here 8,342 keys
//! yielded 278 live pairs against a limit of 1,024 — so the answer looked complete from every
//! number the caller can see.
//!
//! `esker_sql::catalog::View::table_records` asks for the whole range with `txn.scan(start, end, 0)`,
//! where zero means "as many as the server will give". It is given a prefix, builds its map from it,
//! and a name record pointing above the cutoff is then a name pointing at a table that is not there.
//!
//! # `#[ignore]`d, and that is the shape of an acceptance
//!
//! Fixing it is a decision about what a `Scan` promises — walk until the caller's limit is filled in
//! *live* pairs, or answer with a resumption cursor and let the caller page — and both change the
//! contract this store offers. That is the human's call (`CLAUDE.md`, "Ask before doing"), so this
//! file pins the defect the way `safepoint_collects.rs` pins #60: red on purpose, and removing the
//! attribute is the acceptance.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::txnkv::MAX_SCAN_LIMIT;
use esker_store::{RegionState, Store, StoreOptions};

/// Distinct keys in the range: a little past the ceiling, the way a catalog crosses it.
const KEYS: usize = MAX_SCAN_LIMIT as usize + 150;

/// How many of them are left alive. The rest are deleted, and their `write` records stay — the
/// newest version below the safepoint is what a read at the safepoint returns, so a `Delete` is as
/// immortal as a `Put`.
const ALIVE: usize = 8;

/// Sorted the way the engine sorts them, so "the last ones" are the last ones.
fn key(at: usize) -> Bytes {
    Bytes::from(format!("k{at:08}"))
}

fn commit(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    commit_ts: u64,
    mutations: Vec<TxnMutation>,
) {
    let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
    let primary = keys[0].clone();
    match store
        .handle_txn(
            state,
            TxnKvReq::Prewrite {
                start_ts,
                primary,
                ttl_ms: 10_000,
                mutations,
            },
        )
        .unwrap()
    {
        TxnKvResp::Prewrite { keys } => assert!(
            keys.iter().all(esker_proto::TxnStatus::is_ok),
            "the prewrite at {start_ts} was refused"
        ),
        other => panic!("not a prewrite answer: {other:?}"),
    }
    match store
        .handle_txn(
            state,
            TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys,
            },
        )
        .unwrap()
    {
        TxnKvResp::Commit { status } => assert!(status.is_ok(), "the commit at {commit_ts} failed"),
        other => panic!("not a commit answer: {other:?}"),
    }
}

/// **A scan of a range whose dead keys outnumber the ceiling loses the live ones above them.**
///
/// The call is the catalog's: the whole range, `limit = 0` — "as many as the server will give".
#[test]
#[ignore = "the P0 of run 127 attempt 4: a scan silently answers with a prefix of its range; \
            fixing it changes what Scan promises and is the human's call"]
fn a_scan_of_the_whole_range_answers_for_every_live_key_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    commit(
        &store,
        &state,
        10,
        11,
        (0..KEYS)
            .map(|at| TxnMutation::Put {
                key: key(at),
                value: Bytes::from_static(b"v"),
                read_ts: None,
            })
            .collect(),
    );
    // All but the last few are dropped. Their `write` records stay: a key whose newest version is
    // a `Delete` still has a version, and `user_keys_in` counts it.
    commit(
        &store,
        &state,
        20,
        21,
        (0..KEYS - ALIVE)
            .map(|at| TxnMutation::Delete {
                key: key(at),
                read_ts: None,
            })
            .collect(),
    );

    let pairs = match store
        .handle_txn(
            &state,
            TxnKvReq::Scan {
                start: key(0),
                end: Bytes::from_static(b"l"),
                limit: 0,
                ts: 100,
                reverse: false,
            },
        )
        .unwrap()
    {
        TxnKvResp::Scan { pairs } => pairs,
        other => panic!("not a scan answer: {other:?}"),
    };

    let found: Vec<Bytes> = pairs.into_iter().map(|(key, _)| key).collect();
    let want: Vec<Bytes> = (KEYS - ALIVE..KEYS).map(key).collect();
    assert_eq!(
        found,
        want,
        "{KEYS} distinct keys in the range, {ALIVE} of them live, and the store's ceiling is \
         {MAX_SCAN_LIMIT} keys — so the scan stopped {} keys short of the live ones and answered \
         with {} pairs and no sign it had stopped",
        KEYS - MAX_SCAN_LIMIT as usize,
        found.len()
    );
    store.stop();
}
