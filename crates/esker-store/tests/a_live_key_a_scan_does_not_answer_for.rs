//! **#87 — a live key a scan does not answer for, hunted in `#79`'s chunked walk.**
//!
//! Run 128 (attempt 5) lost rows that were on disk in two shapes, neither of which said anything:
//! `reset_counters`' `SELECT COUNT(*)` answered **0 where 1 was expected**, four times in
//! `counter_cache_test.rb`, and a `Namespaced::Firm` lookup answered *"Couldn't find … with
//! 'id'=49"* for a row the stores held. No `ERROR`, no `WARN`, no sentinel — which is the shape
//! `#79` exists to abolish and which `#79`'s own walk is now a suspect for.
//!
//! `txnkv::scan` walks the range a **chunk of keys at a time** and resumes the next chunk strictly
//! past the last key of the previous one. `user_keys_in` builds one chunk from **two column
//! families**: `write`, walked for `KEY_CHUNK` *distinct keys*, and `lock`, walked for `KEY_CHUNK`
//! *entries*. The union is sorted and cut back to `KEY_CHUNK`, and the caller resumes past its last
//! key. Each test below is one way that arithmetic could drop a key, written so that a red one
//! names the mechanism rather than the symptom.
//!
//! **The union's cut is only sound while both walks reach it.** The chunk claims to cover
//! `[start, last]` for *both* families. The `write` walk contributes exactly `KEY_CHUNK` distinct
//! keys, so the cut can never pass its reach. The `lock` walk counts **entries**, and its
//! contribution to the set can be far smaller than its count — so its cursor can stop well short of
//! the cut, and a key that exists **only** in the `lock` family beyond that point is in neither
//! half of the union while the resume point steps over it.
//!
//! That is `a_lock_only_key_past_the_lock_walks_reach`, and it is the one of these that arithmetic
//! alone says is reachable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::{RegionState, Store, StoreOptions};

/// `txnkv::KEY_CHUNK`, which is private; the tests need the number the walk uses.
const KEY_CHUNK: usize = 1024;

fn store() -> (tempfile::TempDir, Arc<Store>, Arc<RegionState>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");
    (dir, store, state)
}

/// A row key of the shape the SQL layer writes: a table prefix and a big-endian id, so the byte
/// order is the id order and the encoding is the one production uses.
fn row_key(table: u64, id: u64) -> Bytes {
    let mut out = Vec::with_capacity(17);
    out.push(b't');
    out.extend_from_slice(&table.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    Bytes::from(out)
}

/// A secondary index key: the indexed column and then the primary key, which is the shape that
/// puts **two** varying fields either side of a chunk boundary.
fn index_key(table: u64, indexed: u64, pk: u64) -> Bytes {
    let mut out = Vec::with_capacity(25);
    out.push(b'i');
    out.extend_from_slice(&table.to_be_bytes());
    out.extend_from_slice(&indexed.to_be_bytes());
    out.extend_from_slice(&pk.to_be_bytes());
    Bytes::from(out)
}

fn prewrite(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    mutations: Vec<TxnMutation>,
) -> Vec<Bytes> {
    let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
    let primary = keys[0].clone();
    match store
        .handle_txn(
            state,
            TxnKvReq::Prewrite {
                start_ts,
                primary,
                ttl_ms: 600_000,
                mutations,
            },
        )
        .unwrap()
    {
        TxnKvResp::Prewrite { keys: status } => assert!(
            status.iter().all(esker_proto::TxnStatus::is_ok),
            "the prewrite at {start_ts} was refused"
        ),
        other => panic!("not a prewrite answer: {other:?}"),
    }
    keys
}

fn commit(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    commit_ts: u64,
    mutations: Vec<TxnMutation>,
) {
    let keys = prewrite(store, state, start_ts, mutations);
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

fn put(key: Bytes, value: &'static [u8]) -> TxnMutation {
    TxnMutation::Put {
        key,
        value: Bytes::from_static(value),
        read_ts: None,
    }
}

/// Scans the whole range and answers the keys, or the error the store gave.
fn scan(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start: Bytes,
    end: Bytes,
    ts: u64,
) -> Result<Vec<Bytes>, String> {
    match store.handle_txn(
        state,
        TxnKvReq::Scan {
            start,
            end,
            limit: 0,
            ts,
            reverse: false,
        },
    ) {
        Ok(TxnKvResp::Scan { pairs }) => Ok(pairs.into_iter().map(|(key, _)| key).collect()),
        Ok(other) => panic!("not a scan answer: {other:?}"),
        Err(error) => Err(error.to_string()),
    }
}

/// **A key whose versions straddle the chunk boundary is answered for once, and the next chunk
/// does not step over the key after it.**
///
/// The resume point is `version_range(last).1`, which is meant to sort above every version of the
/// last key and below the lock entry of the next one. With row keys and with index keys, because
/// an index key varies in **two** fields and the boundary can fall between two entries that share
/// their first.
#[test]
fn a_key_with_many_versions_at_the_chunk_boundary_is_not_stepped_over() {
    let (_dir, store, state) = store();

    // Two chunks' worth, so the boundary is crossed, and the keys either side of it rewritten so
    // the walk has to step past several versions to find the next key.
    let keys: Vec<Bytes> = (0..(KEY_CHUNK + 40) as u64)
        .map(|id| index_key(42, id / 4, id))
        .collect();
    for (round, ts) in [(0u64, 10u64), (1, 20), (2, 30)] {
        let _ = round;
        commit(
            &store,
            &state,
            ts,
            ts + 1,
            keys.iter().cloned().map(|key| put(key, b"v")).collect(),
        );
    }

    let found = scan(
        &store,
        &state,
        index_key(42, 0, 0),
        Bytes::from_static(b"j"),
        100,
    )
    .expect("the scan answered");
    assert_eq!(
        found,
        keys,
        "the walk lost {} of {} keys across the chunk boundary; three versions each and the \
         boundary falls at key {}",
        keys.len() - found.len(),
        keys.len(),
        KEY_CHUNK
    );
}

/// **The same, for row keys** — one field, so the boundary falls between two ids.
#[test]
fn a_row_key_at_the_chunk_boundary_is_not_stepped_over() {
    let (_dir, store, state) = store();
    let keys: Vec<Bytes> = (0..(KEY_CHUNK + 40) as u64)
        .map(|id| row_key(7, id))
        .collect();
    for ts in [10u64, 20, 30] {
        commit(
            &store,
            &state,
            ts,
            ts + 1,
            keys.iter().cloned().map(|key| put(key, b"v")).collect(),
        );
    }
    let found = scan(&store, &state, row_key(7, 0), Bytes::from_static(b"u"), 100)
        .expect("the scan answered");
    assert_eq!(found, keys, "the walk lost a row key at the chunk boundary");
}

/// **A lock inside the range is always reported, and that is why the `lock` walk's reach cannot be
/// the silent mechanism.**
///
/// The arithmetic worry was real on paper: the `lock` walk stops after `KEY_CHUNK` **entries**
/// while the union is cut to `KEY_CHUNK` **keys**, so its cursor can stop short of the cut and a
/// key that exists only in the `lock` family could fall between the two. Written as a test, it
/// cannot be *silent*: `esker_txn::read` meets the first lock in the chunk and `scan` returns the
/// lock as an error, so the caller resolves it and asks again. A scan over a range with any live
/// lock in it never returns a short answer — it returns no answer at all.
///
/// **The denominator is asserted**, because the first version of this test passed on the error
/// branch without ever building the state it was named for, which proves nothing.
#[test]
fn a_lock_inside_the_range_is_reported_and_never_silently_dropped() {
    let (_dir, store, state) = store();

    // A dense `lock` family at the low end: one transaction prewriting a chunk's worth of keys and
    // never committing.
    let locked_low: Vec<TxnMutation> = (0..KEY_CHUNK as u64)
        .map(|id| put(row_key(7, id), b"held"))
        .collect();
    prewrite(&store, &state, 10, locked_low);

    // A dense `write` family reaching far above them, so the union's cut lands past the last lock
    // the walk will see.
    let committed: Vec<TxnMutation> = (100_000..100_000 + KEY_CHUNK as u64)
        .map(|id| put(row_key(7, id), b"v"))
        .collect();
    commit(&store, &state, 20, 21, committed);

    // A lock and no `write` record, above the low block: a transaction that has committed its
    // primary and not yet resolved this one.
    // **Above the committed block and not inside it.** The first version of this test put it
    // at 100_500, which is *within* `100_000..101_024` — so it was not a distinct key at all
    // and the count came out one short for a reason that was the fixture's arithmetic.
    let orphan = row_key(7, 200_000);
    prewrite(&store, &state, 30, vec![put(orphan.clone(), b"orphan")]);

    let answer = scan(&store, &state, row_key(7, 0), Bytes::from_static(b"u"), 100);
    let error = answer.expect_err(
        "a range holding a thousand live locks answered without reporting one of them, which is \
         the silent short answer this file is hunting",
    );
    assert!(
        error.contains("lock") || error.contains("Lock"),
        "the scan failed for a reason that is not the lock: {error}"
    );

    // **And the state really was built**: with every lock resolved the same range answers for all
    // of them, so the refusal above was the locks and not an empty range.
    for start_ts in [10u64, 30] {
        let keys: Vec<Bytes> = if start_ts == 10 {
            (0..KEY_CHUNK as u64).map(|id| row_key(7, id)).collect()
        } else {
            vec![orphan.clone()]
        };
        match store
            .handle_txn(
                &state,
                TxnKvReq::Commit {
                    start_ts,
                    commit_ts: start_ts + 1,
                    keys,
                },
            )
            .unwrap()
        {
            TxnKvResp::Commit { status } => {
                assert!(status.is_ok(), "resolving {start_ts} failed");
            }
            other => panic!("not a commit answer: {other:?}"),
        }
    }
    let found = scan(&store, &state, row_key(7, 0), Bytes::from_static(b"u"), 100)
        .expect("the scan answers once the locks are resolved");
    assert_eq!(
        found.len(),
        KEY_CHUNK * 2 + 1,
        "the resolved range is short: {} keys against {} locked, {} committed and the orphan",
        found.len(),
        KEY_CHUNK,
        KEY_CHUNK
    );
    assert!(
        found.contains(&orphan),
        "the key that had a lock and no write record is missing once it is committed — which is \
         the union's cut stepping over it, and it IS the silent mechanism after all"
    );
}

/// **A limit that lands exactly on the chunk boundary still leaves the range walkable.**
///
/// `scan` breaks out of the chunk loop when `pairs.len() >= limit`, and the caller then resumes
/// from the last **pair** it was given. A limit equal to `KEY_CHUNK` is where "the chunk ran out"
/// and "the limit ran out" happen on the same key.
#[test]
fn a_limit_on_the_chunk_boundary_leaves_the_rest_of_the_range_reachable() {
    let (_dir, store, state) = store();
    let keys: Vec<Bytes> = (0..(KEY_CHUNK * 2) as u64)
        .map(|id| row_key(7, id))
        .collect();
    commit(
        &store,
        &state,
        10,
        11,
        keys.iter().cloned().map(|key| put(key, b"v")).collect(),
    );

    let first = match store
        .handle_txn(
            &state,
            TxnKvReq::Scan {
                start: row_key(7, 0),
                end: Bytes::from_static(b"u"),
                limit: u32::try_from(KEY_CHUNK).unwrap(),
                ts: 100,
                reverse: false,
            },
        )
        .unwrap()
    {
        TxnKvResp::Scan { pairs } => pairs,
        other => panic!("not a scan answer: {other:?}"),
    };
    assert_eq!(first.len(), KEY_CHUNK, "the limit was not filled");

    // The client's own rule: resume from the immediate successor of the last key it was given.
    let last = first.last().unwrap().0.clone();
    let mut next = last.to_vec();
    next.push(0);
    let rest = scan(
        &store,
        &state,
        Bytes::from(next),
        Bytes::from_static(b"u"),
        100,
    )
    .expect("the scan answered");

    let mut all: Vec<Bytes> = first.into_iter().map(|(key, _)| key).collect();
    all.extend(rest);
    assert_eq!(
        all,
        keys,
        "resuming past a limit that fell on the chunk boundary lost {} keys",
        keys.len() - all.len()
    );
}
