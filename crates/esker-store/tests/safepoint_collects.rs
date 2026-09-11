//! ADR 0110 step 1 — a safepoint handed to a store, and the versions below it are collected.
//!
//! # The mechanism has been built since ADR 0021 and never given a number
//!
//! `MvccCollector` is a compaction filter every store is opened *with* — a filter is a
//! column-family setting, so the engine has to be opened carrying it — and it keeps the newest
//! version below a key's effective safepoint and drops the rest, with the `default` entry following
//! the `write` record that names it.
//!
//! **Every sender of `TxnKvReq::GcSafepoint` in this repository is a test.** `esker-pd` does not
//! contain the word `safepoint`. So in every real cluster the published safepoint is the `0` it was
//! constructed with, every key's effective safepoint is zero, and no version has ever been
//! collected — the floor under #58, and what this file is the first half of closing.
//!
//! # Why the counterfactual is inside the test
//!
//! A safepoint of **zero** is the state every real cluster is in today, and it must collect
//! nothing. So the same store is compacted twice — once at zero, once above every version — and
//! what is asserted is the **difference**. A test that only compacted at a high safepoint would
//! pass against a build that collected whatever number it was given, which is the one mistake that
//! would make this whole measurement meaningless.
//!
//! The versions are written through **prewrite and commit** rather than `RawKV`: safepoint collection
//! is about MVCC `write` records, and a raw key has no versions for it to have an opinion about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp};
use esker_store::{RegionState, Store, StoreOptions};

/// Distinct user keys.
const KEYS: usize = 8;

/// Versions per key. Enough that "one per key" and "all of them" are nowhere near each other.
const VERSIONS: usize = 12;

fn key(at: usize) -> Bytes {
    Bytes::from(format!("k{at:04}"))
}

/// Writes `versions` committed versions of every key, one transaction per version, through the
/// path a running cluster uses.
fn fill(store: &Arc<Store>, state: &Arc<RegionState>, versions: usize) {
    let mut ts = 10_u64;
    for round in 0..versions {
        for at in 0..KEYS {
            let start_ts = ts;
            let commit_ts = ts + 1;
            ts += 2;
            let prewrite = TxnKvReq::Prewrite {
                start_ts,
                primary: key(at),
                ttl_ms: 10_000,
                mutations: vec![esker_proto::TxnMutation::Put {
                    key: key(at),
                    value: Bytes::from(format!("v{round:04}")),
                    read_ts: None,
                }],
            };
            assert!(matches!(
                store.handle_txn(state, prewrite).unwrap(),
                TxnKvResp::Prewrite { .. }
            ));
            let commit = TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys: vec![key(at)],
            };
            assert!(matches!(
                store.handle_txn(state, commit).unwrap(),
                TxnKvResp::Commit { .. }
            ));
        }
    }
}

/// The `write` column family's entries — one per committed version of a key, and the only
/// quantity safepoint collection has an opinion about.
///
/// **Not the total across every family**, which is the trap this nearly fell into: the `raft`
/// column family holds a log entry per proposal, so twelve versions of eight keys put nearly two
/// hundred entries there — more than the versions themselves — and a total would be dominated by
/// a number garbage collection must never touch. It is also why the verb's receipt is per family.
fn write_entries(store: &Arc<Store>) -> u64 {
    let families = store.cf_entries().unwrap();
    for family in &families {
        println!("      {:>8}  {} entries", family.cf, family.entries);
    }
    families
        .iter()
        .find(|family| family.cf == esker_engine::cf::WRITE)
        .expect("every store has a `write` column family")
        .entries
}

/// Builds a store, writes the history, raises the safepoint to `safepoint`, compacts, and answers
/// with the `write` entries left.
///
/// **A fresh store per safepoint, rather than one store compacted twice.** A second
/// `compact_range` over an already-compacted column family has nothing to do and returns without
/// running the filter at all — so compacting one store at zero and then at `u64::MAX` measures the
/// same compaction twice and reports "collected nothing" whatever the collector did.
fn write_entries_after_collecting_at(safepoint: u64) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, VERSIONS);
    store.flush().unwrap();

    let in_force = store.raise_safepoint(safepoint);
    assert_eq!(
        in_force, safepoint,
        "the store took the safepoint it was given"
    );
    for cf in store.cf_names() {
        store.compact_cf(&cf).unwrap();
    }
    println!("    safepoint {safepoint}:");
    let left = write_entries(&store);
    store.stop();
    left
}

/// **The assertion ADR 0110 step 1 turns on.** A safepoint above every version leaves one version
/// per key; the safepoint every cluster actually has leaves all of them.
#[test]
fn a_safepoint_collects_the_versions_below_it_and_zero_collects_nothing() {
    // The state of every real cluster: a published safepoint of zero.
    let kept_everything = write_entries_after_collecting_at(0);
    assert_eq!(
        kept_everything,
        (KEYS * VERSIONS) as u64,
        "a safepoint of zero must collect nothing: every one of {KEYS} keys keeps all \
         {VERSIONS} versions"
    );

    let kept_the_newest = write_entries_after_collecting_at(u64::MAX);
    assert_eq!(
        kept_the_newest, KEYS as u64,
        "above every version, each of {KEYS} keys keeps exactly its newest"
    );
}
