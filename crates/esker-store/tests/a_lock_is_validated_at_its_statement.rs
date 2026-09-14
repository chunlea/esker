//! **A `Check` is validated at the read timestamp it carries**
//! ([ADR 0114](../../../docs/adr/0114-a-unique-key-being-written-waits-at-read-committed.md) §2).
//!
//! A READ COMMITTED `SELECT … FOR UPDATE` reads the newest committed version of a row and locks
//! that version. Its eager lock (ADR 0088) is a `Check`, and a `Check` used to be validated at the
//! transaction's `start_ts` — so a row another transaction committed after `BEGIN` was always
//! `40001`, where PostgreSQL 19 returns the row and holds it (debt #91). The client now sends the
//! statement's read timestamp with the lock, as tag 7 on the wire and kind 7 in the log, and this
//! file asks the store the one question that decides: **which timestamp is a commit measured
//! against?** The request goes through `Store::handle_txn`, so it is proposed, replicated as a log
//! entry and applied — kind 7's bytes are on the path, not beside it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
use esker_store::{RegionState, Store, StoreOptions};

/// The row.
const KEY: &[u8] = b"lk";

/// The transaction that locks the row: it began before the row's commit.
const LOCKER_TS: u64 = 10;

/// The commit the locker's `BEGIN` did not see, and a statement after it did.
const COMMIT_TS: u64 = 20;

fn prewrite(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    mutation: TxnMutation,
) -> Vec<TxnStatus> {
    match store
        .handle_txn(
            state,
            TxnKvReq::Prewrite {
                start_ts,
                primary: Bytes::from_static(KEY),
                ttl_ms: 10_000,
                mutations: vec![mutation],
            },
        )
        .unwrap()
    {
        TxnKvResp::Prewrite { keys } => keys,
        other => panic!("not a prewrite answer: {other:?}"),
    }
}

fn check(read_ts: Option<u64>) -> TxnMutation {
    TxnMutation::Check {
        key: Bytes::from_static(KEY),
        read_ts,
    }
}

/// **The row committed at 20, and three locks by a transaction that began at 10.** With no read
/// timestamp the lock is measured against the transaction's snapshot and refused, as it always was
/// — that is a SERIALIZABLE read set's check, and it must not change. With a statement snapshot
/// that is still older than the commit it is refused too. With one that saw the commit it is taken,
/// and it holds the row against the next writer.
///
/// The two refusals go first: a refused prewrite stages nothing, while the lock that is taken stays
/// on the key and would answer any later prewrite of this transaction as its own.
#[test]
fn a_check_is_measured_against_the_read_timestamp_it_carries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    // The commit the locker's `BEGIN` did not see.
    assert!(
        prewrite(
            &store,
            &state,
            15,
            TxnMutation::Put {
                key: Bytes::from_static(KEY),
                value: Bytes::from_static(b"11"),
                read_ts: None,
            },
        )
        .iter()
        .all(TxnStatus::is_ok)
    );
    match store
        .handle_txn(
            &state,
            TxnKvReq::Commit {
                start_ts: 15,
                commit_ts: COMMIT_TS,
                keys: vec![Bytes::from_static(KEY)],
            },
        )
        .unwrap()
    {
        TxnKvResp::Commit { status } => assert!(status.is_ok(), "{status:?}"),
        other => panic!("not a commit answer: {other:?}"),
    }

    assert_eq!(
        prewrite(&store, &state, LOCKER_TS, check(None)),
        vec![TxnStatus::Conflict {
            commit_ts: COMMIT_TS
        }],
        "a check with no read timestamp is measured against the transaction's own snapshot"
    );
    assert_eq!(
        prewrite(&store, &state, LOCKER_TS, check(Some(COMMIT_TS - 5))),
        vec![TxnStatus::Conflict {
            commit_ts: COMMIT_TS
        }],
        "a statement snapshot older than the commit did not see it either"
    );
    assert_eq!(
        prewrite(&store, &state, LOCKER_TS, check(Some(COMMIT_TS + 5))),
        vec![TxnStatus::Ok],
        "a statement that read the committed row may lock it"
    );

    // And the lock is there: a later writer of the row meets the locker.
    match prewrite(
        &store,
        &state,
        40,
        TxnMutation::Put {
            key: Bytes::from_static(KEY),
            value: Bytes::from_static(b"12"),
            read_ts: None,
        },
    )
    .as_slice()
    {
        [TxnStatus::Locked(lock)] => assert_eq!(lock.start_ts, LOCKER_TS, "{lock:?}"),
        other => panic!("the lock was not left on the row: {other:?}"),
    }
    store.stop();
}
