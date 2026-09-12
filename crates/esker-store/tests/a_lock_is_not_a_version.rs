//! **#78.** A `Kind::Lock` record must not stand in for the version beneath it.
//!
//! # The shape the P0 had
//!
//! Run 127 attempt 4 (`091f07bb`, 64 MiB, `--retention-ms 60000`, the #70 sweeper on) is the first
//! pass in this repository in which collection ever ran, and after twenty-eight clean files five
//! consecutive files stopped the same way:
//!
//! ```text
//! PG::DataCorrupted: ERROR: corrupt data: a name points at table 34755, which is not there
//! ```
//!
//! The `write` record for the table was **not in any SST** — the record was collected, not the
//! `default` value it names — while the name record pointing at it survived. Five different tables,
//! ids climbing, every one created seconds earlier by the file that broke on it.
//!
//! # The mechanism
//!
//! `MvccCollector::filter` special-cases exactly one non-version: `Kind::Rollback`. `Kind::Lock` is
//! not reserved any more — `Op::Check` commits as one
//! ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md),
//! [ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)), and so
//! does the row lock `SELECT … FOR UPDATE` takes
//! ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)) — so a key can carry
//! `Put@c1` with `Lock@cL` above it. Below the safepoint the filter meets the `Lock` first (versions
//! arrive newest first), `keep_as_newest` records it as *the* newest and keeps it, and the `Put`
//! underneath is then older than what was kept and is **removed**. The read side steps past a
//! `Lock` looking for a version (`esker_txn::percolator::newest_version_at`), finds nothing, and
//! the key has vanished.
//!
//! A key nobody validated keeps its version. A key some transaction only *read* loses it. That is
//! why the name record lived and the table record it named did not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::compaction::{CompactionFilter, FilterDecision};
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::gc::{MvccCollector, RetentionPolicy};
use esker_store::{RegionState, Store, StoreOptions};
use esker_txn::codec::{Kind, WriteRecord};
use esker_txn::key;

/// The key the transactions in this file write and hold.
const KEY: &[u8] = b"books";

/// The value that must survive being held.
const VALUE: &[u8] = b"the table definition";

/// **The aggressive answer**: no level below holds any version of the key, which is what a
/// compaction that has reached the bottom reports and what ADR 0111's segment rule acts on. This
/// file's exhaustive property therefore covers that rule too — if dropping a deleted key's whole
/// segment ever changed an answer at or above the safepoint, it would fail here.
fn nothing_below(_start: &[u8], _end: &[u8]) -> bool {
    true
}

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
        TxnKvResp::Commit { status } => {
            assert!(
                status.is_ok(),
                "the commit at {commit_ts} failed: {status:?}"
            );
        }
        other => panic!("not a commit answer: {other:?}"),
    }
}

fn read_at(store: &Arc<Store>, state: &Arc<RegionState>, ts: u64) -> Option<Bytes> {
    match store
        .handle_txn(
            state,
            TxnKvReq::Get {
                key: Bytes::from_static(KEY),
                ts,
            },
        )
        .unwrap()
    {
        TxnKvResp::Get { value } => value,
        other => panic!("not a get answer: {other:?}"),
    }
}

/// **The P0 in six statements.** A written key, a transaction that only *read* it, a safepoint
/// above both, and the collection the sweeper runs — and the value is still there afterwards.
///
/// `Op::Check` is what a SERIALIZABLE transaction leaves on a key in its validated read set, and
/// `TxnMutation::Check` is the wire form of it. It writes no value; what it leaves is a
/// `Kind::Lock` write record at its own commit timestamp, which is **above** the `Put` it validated.
///
/// The call sequence is `collect::Sweeper`'s: flush, then `compact_range(write, None, None)` — the
/// same two, in the same order, so that what this asserts is what a running store does.
#[test]
fn a_key_held_by_a_read_keeps_its_value_through_a_collection() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    // The write, and then a transaction that read the key and validated it at commit.
    prewrite_and_commit(
        &store,
        &state,
        10,
        11,
        TxnMutation::Put {
            key: Bytes::from_static(KEY),
            value: Bytes::from_static(VALUE),
            read_ts: None,
        },
    );
    assert_eq!(
        read_at(&store, &state, 15).as_deref(),
        Some(VALUE),
        "the value is there before anything holds the key"
    );
    prewrite_and_commit(
        &store,
        &state,
        20,
        21,
        TxnMutation::Check {
            key: Bytes::from_static(KEY),
        },
    );
    assert_eq!(
        read_at(&store, &state, 25).as_deref(),
        Some(VALUE),
        "a `Check` writes no value, so the read is unchanged by it"
    );

    // A safepoint above both records: everything here is collectable, and only the newest
    // *version* may survive.
    let safepoint = 100;
    assert_eq!(store.raise_safepoint(safepoint), safepoint);
    store.flush().unwrap();
    store.compact_cf(esker_engine::cf::WRITE).unwrap();

    assert_eq!(
        read_at(&store, &state, 200).as_deref(),
        Some(VALUE),
        "the collection kept the lock record and dropped the version under it: the key a \
         transaction only *read* has lost its value"
    );
    store.stop();
}

/// What a reader **sees**, which is what the property below is about.
///
/// [`read_of`] answers with the record that decides, and this turns that into the answer: a `Put`
/// is its value, and **a `Delete` and no record at all are the same thing** — the key is not there.
///
/// The distinction started to matter with [ADR 0111](../../../docs/adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md),
/// which drops a deleted key's whole segment where nothing below can hide an older version: the
/// record that said "gone" goes with it, and comparing *records* calls that a changed answer when
/// no reader can tell. Comparing answers still catches #78, which is what this file is for — there
/// a `Put` was lost under a `Lock`, and the answer went from a value to absent.
fn answer_of(history: &[(u64, Kind)], ts: u64) -> Option<u64> {
    match read_of(history, ts) {
        Some((commit_ts, Kind::Put)) => Some(commit_ts),
        _ => None,
    }
}

/// The read a store answers with, as `esker_txn::percolator::newest_version_at` performs it:
/// newest first, stepping past everything that is not a version, and the first version decides.
fn read_of(history: &[(u64, Kind)], ts: u64) -> Option<(u64, Kind)> {
    history
        .iter()
        .copied()
        .filter(|(commit_ts, _)| *commit_ts <= ts)
        .find(|(_, kind)| kind.is_a_version())
}

/// **Every history of four records over four kinds, against every interesting safepoint.**
///
/// Exhaustive rather than sampled: four kinds at four positions is 340 histories of length one to
/// four, and enumerating them is both cheaper than a property runner and a stronger statement —
/// there is no seed under which this passes and another under which it does not.
///
/// The property is the one a store owes its readers, and it is about the **answer**, not about
/// which records survive: `esker-store`'s decision 5 refuses any read below the safepoint
/// ([ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md)), so a
/// collection is correct exactly when every read **at or above** the safepoint gives the same
/// answer it gave before.
#[test]
fn no_read_above_the_safepoint_changes_its_answer_when_the_collector_runs() {
    // Spaced so that a safepoint can fall below, between, on, and above them.
    const STAMPS: [u64; 4] = [10, 20, 30, 40];
    let kinds = Kind::ALL;

    let mut histories: Vec<Vec<(u64, Kind)>> = Vec::new();
    for len in 1..=STAMPS.len() {
        let mut each = vec![0usize; len];
        loop {
            // Newest first, which is the order `enc_ts` delivers them in.
            histories.push(
                (0..len)
                    .map(|at| (STAMPS[len - 1 - at], kinds[each[at]]))
                    .collect(),
            );
            let mut at = 0;
            while at < len {
                each[at] += 1;
                if each[at] < kinds.len() {
                    break;
                }
                each[at] = 0;
                at += 1;
            }
            if at == len {
                break;
            }
        }
    }
    assert_eq!(
        histories.len(),
        4 + 16 + 64 + 256,
        "every history is enumerated"
    );

    let mut checked = 0_usize;
    for history in &histories {
        for safepoint in [0, 5, 15, 25, 35, 45, u64::MAX] {
            let collector = MvccCollector::new(RetentionPolicy::uniform(0), safepoint);
            let survivors: Vec<(u64, Kind)> = history
                .iter()
                .copied()
                .filter(|(commit_ts, kind)| {
                    let record = match kind {
                        // A marker sits at `commit_ts == start_ts`; every other record's
                        // `start_ts` is below its commit.
                        Kind::Rollback => WriteRecord::rollback(*commit_ts),
                        _ => WriteRecord::new(*kind, commit_ts - 1),
                    };
                    collector.filter(
                        0,
                        &key::write(KEY, *commit_ts),
                        &record.encode(),
                        &nothing_below,
                    ) == FilterDecision::Keep
                })
                .collect();

            for ts in [safepoint, safepoint.saturating_add(1), u64::MAX] {
                assert_eq!(
                    answer_of(&survivors, ts),
                    answer_of(history, ts),
                    "safepoint {safepoint}, read at {ts}: the collection changed the answer\n\
                     before {history:?}\nafter  {survivors:?}"
                );
                checked += 1;
            }
        }
    }
    println!("  {} histories · {checked} reads compared", histories.len());
}
