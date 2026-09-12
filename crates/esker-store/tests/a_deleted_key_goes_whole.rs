//! **ADR 0111's acceptance.** A deleted key's versions go as one segment, or none of them do.
//!
//! `MvccCollector` keeps the newest version at or below the safepoint because that is what a read
//! at the safepoint returns — and for a key whose newest version is a **delete**, what a read
//! returns is "gone". Keeping the record that says so is keeping a gravestone for ever: #70's
//! twelve-round probe measured twelve dropped tables leaving ninety-six such records a round, and
//! because `CREATE TABLE` takes a fresh id every time they sit at a strictly higher, disjoint key
//! range — so the sweep's output overlaps nothing already in the bottom level, lands as another
//! file, and the bottom level never merges it away.
//!
//! # Why these three and not one
//!
//! Dropping a delete is the one collection decision that can **resurrect** a key, so each condition
//! gets the counterfactual that fails without it. `a_deleted_key_goes_whole` is the claim;
//! `a_reader_below_the_delete_keeps_the_whole_segment` is condition (3), the live reader; condition
//! (2), a lower level still holding an older version, needs a multi-level database and lives beside
//! #62's own tests in `esker-engine`.
//!
//! # "Newest" is the newest **version**
//!
//! Not the newest record: a `Lock` from a validated read set and a `Rollback` are records and not
//! versions, and reading them as one cost five catalog table records in run 127 attempt 4 (#78).
//! `a_lock_above_a_delete_does_not_save_the_segment` is that half — the segment is still a deleted
//! key's, and the lock at the head of it does not make it look alive.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::{RegionState, Store, StoreOptions};

const KEY: &[u8] = b"books";
const VALUE: &[u8] = b"the table definition";

fn prewrite_and_commit(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    commit_ts: u64,
    mutation: TxnMutation,
) {
    let key = Bytes::from_static(KEY);
    let response = store
        .handle_txn(
            state,
            TxnKvReq::Prewrite {
                start_ts,
                primary: key.clone(),
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
    let response = store
        .handle_txn(
            state,
            TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys: vec![key],
            },
        )
        .unwrap();
    match response {
        TxnKvResp::Commit { status } => assert!(
            status.is_ok(),
            "the commit at {commit_ts} failed: {status:?}"
        ),
        other => panic!("not a commit answer: {other:?}"),
    }
}

fn put(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, commit_ts: u64) {
    prewrite_and_commit(
        store,
        state,
        start_ts,
        commit_ts,
        TxnMutation::Put {
            key: Bytes::from_static(KEY),
            value: Bytes::from_static(VALUE),
            read_ts: None,
        },
    );
}

fn delete(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, commit_ts: u64) {
    prewrite_and_commit(
        store,
        state,
        start_ts,
        commit_ts,
        TxnMutation::Delete {
            key: Bytes::from_static(KEY),
            read_ts: None,
        },
    );
}

/// The sweeper's own call sequence, by hand: flush, then compact the whole family.
fn collect(store: &Arc<Store>, safepoint: u64) {
    assert_eq!(store.raise_safepoint(safepoint), safepoint);
    store.flush().unwrap();
    store.compact_cf(esker_engine::cf::WRITE).unwrap();
}

/// **The claim.** A key whose newest version is a delete, with nothing below it and nobody reading
/// it, leaves no record at all.
///
/// Red until ADR 0111: `keep_as_newest` keeps the delete, because a read at the safepoint is
/// entitled to be told the key is gone — and telling it that costs a record whose key will never be
/// written again.
#[test]
fn a_deleted_key_goes_whole() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    delete(&store, &state, 20, 21);
    // **The denominator.** Both records are there before anything collects, so a zero below is the
    // collection and not a workload that never wrote.
    assert_eq!(
        store.write_records(KEY).unwrap(),
        2,
        "the workload did not leave the two versions this test is about"
    );

    collect(&store, 100);

    assert_eq!(
        store.write_records(KEY).unwrap(),
        0,
        "the delete survived its own segment: a key nothing can read keeps a record for ever, and \
         because its key is never written again that record is a file the bottom level never \
         merges away (ADR 0111)"
    );
    store.stop();
}

/// **Condition (3), the live reader.** A snapshot below the delete still sees what it deleted.
///
/// The counterfactual for dropping the segment: without this the collection answers a reader that
/// was entitled to the old value with nothing at all.
#[test]
fn a_reader_below_the_delete_keeps_the_whole_segment() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    delete(&store, &state, 20, 21);

    // **A safepoint below the delete**, which is what a live reader at 15 forces: ADR 0110's
    // number is at or below the oldest active read, so a reader between the put and the delete
    // holds the safepoint under both.
    collect(&store, 15);

    assert_eq!(
        store.write_records(KEY).unwrap(),
        2,
        "a segment whose delete is above the safepoint was collected anyway: a read at 15 is \
         entitled to the value, and dropping the segment answers it with nothing"
    );
    store.stop();
}

/// **"Newest" is the newest version.** A `Lock` at the head of the segment does not make a deleted
/// key look alive.
///
/// The record a committed `Op::Check` leaves — a validated read set (ADR 0062) — is a record and
/// not a version, and the read path steps past it. Reading it as the newest is what #78 was, from
/// the other side: there it kept a lock and dropped the value under it; here it would keep a whole
/// segment that could go.
#[test]
fn a_lock_above_a_delete_does_not_save_the_segment() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    put(&store, &state, 10, 11);
    delete(&store, &state, 20, 21);
    // A transaction that only *read* the key and validated it at commit, after the delete.
    prewrite_and_commit(
        &store,
        &state,
        30,
        31,
        TxnMutation::Check {
            key: Bytes::from_static(KEY),
        },
    );
    assert_eq!(
        store.write_records(KEY).unwrap(),
        3,
        "the check did not leave the record this test is about"
    );

    collect(&store, 100);

    assert_eq!(
        store.write_records(KEY).unwrap(),
        0,
        "the newest *record* is a lock and the newest *version* is a delete; reading the first as \
         the second keeps a segment that every condition says may go (#78, ADR 0111)"
    );
    store.stop();
}
