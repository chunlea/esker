//! **ADR 0112's acceptance.** A spilled value goes when nothing names it, and stays when something
//! might.
//!
//! A value longer than `esker_txn::SHORT_VALUE_MAX_LEN` is not inlined in its `write` record: it is
//! stored at **prewrite** in the `default` family under `key::value(user_key, start_ts)`, and the
//! `write` record's `start_ts` is the link back to it. `MvccCollector` is installed on `write` and
//! only on `write`, so nothing ever collected those values — #60 measured it at `write` 96 → 8 and
//! `default` 96 → 96 — and [ADR 0111](../../../docs/adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md)
//! sharpened it: a deleted key's `write` records now go entirely, so the value loses its last
//! reference and is unreachable rather than merely unread.
//!
//! # The hazard this is mostly about
//!
//! "Delete what nothing names" is wrong read literally, and the reason is the order the two keys
//! are written in: the value lands at **prewrite** and the record that names it at **commit**. A
//! transaction between the two has a `default` entry with no `write` record anywhere — and deleting
//! it there loses a value that is about to be committed. `a_value_a_transaction_has_not_committed_yet_survives`
//! is that case, and it is why the pass keeps an entry whose key still holds a lock.
//!
//! The failure direction is the one this system takes everywhere: keep too much, never delete a
//! live value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::ReadOptions;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::{RegionState, Store, StoreOptions};

const KEY: &[u8] = b"books";

/// Longer than `esker_txn::SHORT_VALUE_MAX_LEN`, so prewrite spills it rather than inlining it.
fn spilled() -> Bytes {
    Bytes::from(vec![b'v'; 512])
}

fn prewrite(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, mutation: TxnMutation) {
    let response = store
        .handle_txn(
            state,
            TxnKvReq::Prewrite {
                start_ts,
                primary: Bytes::from_static(KEY),
                ttl_ms: 10_000,
                mutations: vec![mutation],
            },
        )
        .unwrap();
    match response {
        TxnKvResp::Prewrite { keys } => assert!(
            keys.iter().all(esker_proto::TxnStatus::is_ok),
            "the prewrite at {start_ts} was refused: {keys:?}"
        ),
        other => panic!("not a prewrite answer: {other:?}"),
    }
}

fn commit(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, commit_ts: u64) {
    let response = store
        .handle_txn(
            state,
            TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys: vec![Bytes::from_static(KEY)],
            },
        )
        .unwrap();
    match response {
        TxnKvResp::Commit { status } => assert!(status.is_ok(), "the commit failed: {status:?}"),
        other => panic!("not a commit answer: {other:?}"),
    }
}

fn put(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, commit_ts: u64) {
    prewrite(
        store,
        state,
        start_ts,
        TxnMutation::Put {
            key: Bytes::from_static(KEY),
            value: spilled(),
            read_ts: None,
        },
    );
    commit(store, state, start_ts, commit_ts);
}

/// How many entries the `default` family holds for `KEY`'s version span.
fn spilled_values(store: &Arc<Store>) -> usize {
    let (start, end) = esker_txn::key::version_range(KEY);
    let mut iter = store
        .db()
        .iter(esker_engine::cf::DEFAULT, &ReadOptions::default())
        .unwrap();
    let mut found = 0;
    iter.seek(&start);
    while iter.valid() && iter.key() < end.as_slice() {
        found += 1;
        iter.next();
    }
    iter.status().unwrap();
    found
}

/// The sweeper's own call sequence, by hand.
fn collect(store: &Arc<Store>, safepoint: u64) -> u64 {
    assert_eq!(store.raise_safepoint(safepoint), safepoint);
    store.flush().unwrap();
    // **The sweeper's order, and it is load-bearing**: `write` first, so ADR 0111 has dropped the
    // records that named these values and the orphans are visible; then the pass; then `default`,
    // which applies the tombstones the pass wrote.
    store.compact_cf(esker_engine::cf::WRITE).unwrap();
    let orphans = store.collect_spilled_values().unwrap();
    eprintln!(
        "  after the pass:       {} (orphans {orphans})",
        spilled_values(store)
    );
    store.flush().unwrap();
    store.compact_cf(esker_engine::cf::DEFAULT).unwrap();
    orphans
}

/// **The claim.** A deleted key's spilled value goes with the segment that named it.
#[test]
fn a_spilled_value_goes_when_nothing_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            // **The sweeper off**, because this file drives its sequence by hand. Left on, the
            // background sweep `raise_safepoint` wakes does the work first and every measurement below
            // reads its result rather than the step it names — which is how the first version of this
            // test reported "the pass removed nothing" while the value was long gone.
            collect_debounce: None,
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    prewrite(
        &store,
        &state,
        20,
        TxnMutation::Delete {
            key: Bytes::from_static(KEY),
            read_ts: None,
        },
    );
    commit(&store, &state, 20, 21);
    // **The denominator.** The value is there before anything collects.
    assert_eq!(
        spilled_values(&store),
        1,
        "the workload did not spill the value this test is about"
    );

    let orphans = collect(&store, 100);

    assert_eq!(
        store.write_records(KEY).unwrap(),
        0,
        "ADR 0111 did not drop the segment, so this is not yet a test about the orphan it leaves"
    );
    assert_eq!(orphans, 1, "the pass reported removing nothing");
    assert_eq!(
        spilled_values(&store),
        0,
        "the value outlived every record that could name it: nothing in the database points at it, \
         so no reader can ever be shown it and its bytes are pure loss (#60, ADR 0112)"
    );
    store.stop();
}

/// **The wiring**, which the three tests around it do not pin: that the *sweeper* runs this pass,
/// and runs it where it can see anything.
///
/// They call `Store::collect_spilled_values` directly, so backing the call out of `collect::Sweeper`
/// leaves every one of them green — the shape #77 is registered for. This one raises a safepoint
/// and waits for the sweeper's own round, and it is also the test that says the **order** matters:
/// the pass runs between `write`'s compaction and `default`'s, because before the first it sees
/// every record still in place and finds nothing, and after the second its deletions wait a whole
/// debounce.
#[test]
fn the_sweeper_runs_the_pass_itself() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            // Every rise sweeps, so the test waits for a round rather than for a cadence.
            collect_debounce: Some(std::time::Duration::ZERO),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    prewrite(
        &store,
        &state,
        20,
        TxnMutation::Delete {
            key: Bytes::from_static(KEY),
            read_ts: None,
        },
    );
    commit(&store, &state, 20, 21);
    assert_eq!(
        spilled_values(&store),
        1,
        "the workload did not spill the value this test is about"
    );

    store.raise_safepoint(100);
    let swept = store
        .sweeper()
        .expect("this store sweeps")
        .wait_for_sweeps(1, std::time::Duration::from_secs(30));
    assert!(swept, "the sweeper never finished a round to be measured");

    assert_eq!(
        spilled_values(&store),
        0,
        "a sweep left the orphan behind: the pass exists and nothing calls it, or it is called \
         somewhere it cannot see what `write`'s compaction decided (ADR 0112)"
    );
    store.stop();
}

/// **The control.** A value a surviving record still names stays.
#[test]
fn a_spilled_value_a_record_still_names_survives() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            // **The sweeper off**, because this file drives its sequence by hand. Left on, the
            // background sweep `raise_safepoint` wakes does the work first and every measurement below
            // reads its result rather than the step it names — which is how the first version of this
            // test reported "the pass removed nothing" while the value was long gone.
            collect_debounce: None,
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    let _ = collect(&store, 100);

    assert_eq!(
        store.write_records(KEY).unwrap(),
        1,
        "the live key lost its record, so the assertion below would pass for the wrong reason"
    );
    assert_eq!(
        spilled_values(&store),
        1,
        "the pass deleted a value its own `write` record still names — the read path resolves a \
         spilled value through exactly that link, so this is a committed row read back as \
         corruption"
    );
    store.stop();
}

/// **The hazard.** A value is written at prewrite and named at commit, so a transaction between the
/// two has a `default` entry no `write` record names — and it is about to be committed.
///
/// The safepoint is put above the transaction's `start_ts` by hand, which ADR 0110 keeps from
/// happening in a cluster (the number is at or below the oldest active read) and an abandoned lock
/// can outlive anyway. What keeps the value is the **lock**: a key that still holds one is a key
/// whose value may yet be named.
#[test]
fn a_value_a_transaction_has_not_committed_yet_survives() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            // **The sweeper off**, because this file drives its sequence by hand. Left on, the
            // background sweep `raise_safepoint` wakes does the work first and every measurement below
            // reads its result rather than the step it names — which is how the first version of this
            // test reported "the pass removed nothing" while the value was long gone.
            collect_debounce: None,
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    prewrite(
        &store,
        &state,
        10,
        TxnMutation::Put {
            key: Bytes::from_static(KEY),
            value: spilled(),
            read_ts: None,
        },
    );
    assert_eq!(
        (store.write_records(KEY).unwrap(), spilled_values(&store)),
        (0, 1),
        "a prewrite is supposed to leave the value and no record naming it"
    );

    let _ = collect(&store, 100);

    assert_eq!(
        spilled_values(&store),
        1,
        "the pass deleted a prewritten value: nothing named it because the commit that would name \
         it had not run, and the transaction is still holding the lock"
    );

    // And it is still the right value once the commit does name it.
    commit(&store, &state, 10, 11);
    assert_eq!(
        store.write_records(KEY).unwrap(),
        1,
        "the commit did not land"
    );
    store.stop();
}
