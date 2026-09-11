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

/// A value short enough to be stored inline in the `write` record, so the `default` family stays
/// empty: `esker_txn::codec::SHORT_VALUE_MAX_LEN` is 255.
const INLINE: usize = 8;

/// A value too long to inline, so every version writes a `default` entry keyed by its `start_ts`.
const SPILLED: usize = 512;

fn key(at: usize) -> Bytes {
    Bytes::from(format!("k{at:04}"))
}

/// Writes `versions` committed versions of every key, one transaction per version, through the
/// path a running cluster uses.
fn fill(store: &Arc<Store>, state: &Arc<RegionState>, versions: usize, value_len: usize) {
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
                    value: Bytes::from(vec![u8::try_from(round % 251).unwrap_or(0); value_len]),
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
fn entries_of(store: &Arc<Store>, want: &str) -> u64 {
    let families = store.cf_entries().unwrap();
    for family in &families {
        println!("      {:>8}  {} entries", family.cf, family.entries);
    }
    families
        .iter()
        .find(|family| family.cf == want)
        .unwrap_or_else(|| panic!("no `{want}` column family"))
        .entries
}

/// Builds a store, writes the history, raises the safepoint to `safepoint`, compacts, and answers
/// with the `write` entries left.
///
/// **A fresh store per safepoint, rather than one store compacted twice.** A second
/// `compact_range` over an already-compacted column family has nothing to do and returns without
/// running the filter at all — so compacting one store at zero and then at `u64::MAX` measures the
/// same compaction twice and reports "collected nothing" whatever the collector did.
fn entries_after_collecting_at(safepoint: u64, value_len: usize, family: &str) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, VERSIONS, value_len);
    store.flush().unwrap();

    let in_force = store.raise_safepoint(safepoint);
    assert_eq!(
        in_force, safepoint,
        "the store took the safepoint it was given"
    );
    for cf in store.cf_names() {
        store.compact_cf(&cf).unwrap();
    }
    println!("    safepoint {safepoint}, values of {value_len} bytes:");
    let left = entries_of(&store, family);
    store.stop();
    left
}

/// **The assertion ADR 0110 step 1 turns on.** A safepoint above every version leaves one version
/// per key; the safepoint every cluster actually has leaves all of them.
#[test]
fn a_safepoint_collects_the_versions_below_it_and_zero_collects_nothing() {
    // The state of every real cluster: a published safepoint of zero.
    let kept_everything = entries_after_collecting_at(0, INLINE, esker_engine::cf::WRITE);
    assert_eq!(
        kept_everything,
        (KEYS * VERSIONS) as u64,
        "a safepoint of zero must collect nothing: every one of {KEYS} keys keeps all \
         {VERSIONS} versions"
    );

    let kept_the_newest = entries_after_collecting_at(u64::MAX, INLINE, esker_engine::cf::WRITE);
    assert_eq!(
        kept_the_newest, KEYS as u64,
        "above every version, each of {KEYS} keys keeps exactly its newest"
    );
}

/// **#60 — the `default` family is not collected, and `gc.rs`'s module doc says it is.**
///
/// > Everything older goes, and so does its `default` entry.
///
/// Nothing in `gc.rs` touches that family: every `cf::DEFAULT` in it reads retention configuration.
/// The test above could not see the case, because its values are eight bytes and
/// `SHORT_VALUE_MAX_LEN` is 255 — short values live **inline in the `write` record**, so the
/// `default` family was empty and collecting the `write` half took the values with it.
///
/// A value of 512 bytes spills: each version writes a `default` entry keyed by `(user_key,
/// start_ts)`. Those are the **bytes** — a version record is tens of bytes and a value is as large
/// as the user made it — so this is the difference between reclaiming records and reclaiming
/// space, which is the number anyone measuring #58 will quote.
/// # Measured 2026-09-11, and this is the shape of it
///
/// ```text
///                 safepoint 0      safepoint u64::MAX
///      write           96      →         8      collected, one per key
///    default           96      →        96      not collected at all
///       lock          192      →       192      not collected either (ADR 0110)
/// ```
///
/// Ninety-six values of 512 bytes kept where eight are needed. **The bytes are precisely what is
/// not reclaimed** — a version record is tens of bytes and a value is as large as the user made
/// it — so a space measurement taken after collecting will barely move, and the reason is here.
///
/// `#[ignore]`d rather than weakened: it is the acceptance for #60 and it should stay red until
/// something collects them.
#[test]
#[ignore = "#60: the `default` family is not collected; this is its acceptance"]
fn a_spilled_value_is_collected_with_the_version_that_names_it() {
    let all_of_them = entries_after_collecting_at(0, SPILLED, esker_engine::cf::DEFAULT);
    assert_eq!(
        all_of_them,
        (KEYS * VERSIONS) as u64,
        "a safepoint of zero keeps every spilled value, one per version"
    );

    let after = entries_after_collecting_at(u64::MAX, SPILLED, esker_engine::cf::DEFAULT);
    assert_eq!(
        after,
        KEYS as u64,
        "above every version, each key keeps the value its surviving version names — and every \
         other value is {} entries of pure waste",
        all_of_them - after
    );
}

/// **Before anything is concluded about collecting spilled values, check they are stored at all.**
///
/// `default` holding nothing after twelve 512-byte versions says either that the values went
/// somewhere else or that they were lost. A read is what tells the two apart, and it is the one
/// that matters: a value that cannot be read back is a far larger problem than a value that is
/// never collected.
#[test]
fn a_long_value_is_readable_after_it_is_committed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    fill(&store, &state, 1, SPILLED);

    let answered = store
        .handle_txn(
            &state,
            TxnKvReq::Get {
                key: key(0),
                ts: u64::MAX / 2,
            },
        )
        .unwrap();
    let TxnKvResp::Get { value } = answered else {
        panic!("a get answered {answered:?}");
    };
    let value = value.expect("a committed 512-byte value reads back");
    assert_eq!(value.len(), SPILLED, "the whole value comes back");
    store.flush().unwrap();
    let spilled = entries_of(&store, esker_engine::cf::DEFAULT);
    assert_eq!(
        spilled, KEYS as u64,
        "a value too long to inline lives in `default`, keyed by its `start_ts` — one per key"
    );
    store.stop();
}
