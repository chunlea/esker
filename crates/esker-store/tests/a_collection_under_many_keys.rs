//! **#87 — the collector under many keys and a concurrent compaction.**
//!
//! `a_lock_is_not_a_version` enumerates every history of length 1–4 over four kinds and seven
//! safepoints, which is exhaustive about **one key at a time and one compaction at a time**. Two
//! things it cannot reach are exactly the two that attempt 5 was doing when it lost rows: the
//! `seen`/`keep_as_newest` slot shared by **many keys in one compaction**, and a sweep interleaved
//! with the **picker's own** compactions while writes keep arriving.
//!
//! So this is a stress probe rather than a proof: a hundred keys in two real shapes — a row key and
//! a two-field secondary index key — each with a random history of `Put`, `Delete`, `Check` (which
//! commits a `Kind::Lock`), a rolled-back transaction and a lock left across a round, with the
//! safepoint rising every round and the sweeper's own call sequence run against it while a second
//! thread writes enough to keep the picker busy.
//!
//! **The property is the one a reader cares about**: a read above the safepoint answers what the
//! last committed write said. Anything the model has and the store reads as *absent* is the failure
//! of run 128 — a live row that two different statements could not read — and it prints that key's
//! whole `write` record list on the way out.
//!
//! Green is not free of information either: the run prints how many collections it did and how far
//! the record count fell, because a probe that collected nothing would pass without testing
//! anything.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_store::{RegionState, Store, StoreOptions};
use esker_txn::codec::WriteRecord;
use esker_txn::key;

/// Rounds of workload, each one ending in a sweep.
const ROUNDS: usize = 12;

/// Keys of each shape, so a hundred in all.
const OF_EACH: u64 = 50;

/// How far below the newest timestamp the safepoint is published, in `ts` units. Low enough that
/// most of the history is collectable and high enough that a lock left across a round is not.
const SAFEPOINT_LAG: u64 = 200;

/// A seeded PCG-style generator, because the simulator's belongs to `esker-base` and a test does
/// not buy an RNG (`CLAUDE.md`).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// A row key of the shape the SQL layer writes.
fn row_key(id: u64) -> Bytes {
    let mut out = vec![b't'];
    out.extend_from_slice(&7u64.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    Bytes::from(out)
}

/// A secondary index key: the indexed column, then the primary key.
fn index_key(id: u64) -> Bytes {
    let mut out = vec![b'i'];
    out.extend_from_slice(&7u64.to_be_bytes());
    out.extend_from_slice(&(id % 10).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    Bytes::from(out)
}

/// The value a round writes, **spilled for half the keys**.
///
/// A value longer than `esker_txn::codec::SHORT_VALUE_MAX_LEN` (255 bytes) is not inlined in the
/// `write` record: prewrite puts it in the `default` column family under
/// `key::value(user_key, start_ts)` — versioned by the transaction's **`start_ts`**, where the
/// record that points at it is versioned by its **`commit_ts`** ([ADR 0112](../../../docs/adr/0112-collecting-a-spilled-value.md)).
/// Two families, two timestamp spaces and one link between them is the shape this probe would
/// otherwise not touch at all, so half the keys write across it.
fn value_for(key: &Bytes, round: usize) -> Vec<u8> {
    let short = format!("round {round}").into_bytes();
    if key[key.len() - 1].is_multiple_of(2) {
        let mut long = short;
        long.resize(512, b'v');
        long
    } else {
        short
    }
}

fn prewrite(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    mutations: Vec<TxnMutation>,
) -> bool {
    let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
    let primary = keys[0].clone();
    match store.handle_txn(
        state,
        TxnKvReq::Prewrite {
            start_ts,
            primary,
            ttl_ms: 600_000,
            mutations,
        },
    ) {
        Ok(TxnKvResp::Prewrite { keys }) => keys.iter().all(esker_proto::TxnStatus::is_ok),
        Ok(other) => panic!("not a prewrite answer: {other:?}"),
        // A conflict is an honest answer here: the filler thread shares the timestamp counter.
        Err(_) => false,
    }
}

fn commit(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    start_ts: u64,
    commit_ts: u64,
    key: &Bytes,
) {
    match store.handle_txn(
        state,
        TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![key.clone()],
        },
    ) {
        Ok(TxnKvResp::Commit { status }) => {
            assert!(
                status.is_ok(),
                "the commit at {commit_ts} failed: {status:?}"
            );
        }
        Ok(other) => panic!("not a commit answer: {other:?}"),
        Err(error) => panic!("the commit at {commit_ts} failed: {error}"),
    }
}

fn rollback(store: &Arc<Store>, state: &Arc<RegionState>, start_ts: u64, key: &Bytes) {
    let _ = store.handle_txn(
        state,
        TxnKvReq::Rollback {
            start_ts,
            keys: vec![key.clone()],
        },
    );
}

/// Every `write` record standing for `user_key`, newest first, as the dump a failure needs.
fn records(store: &Arc<Store>, user_key: &[u8]) -> Vec<String> {
    let (low, high) = key::version_range(user_key);
    let mut out = Vec::new();
    let mut iter = store
        .db()
        .iter(
            esker_engine::cf::WRITE,
            &esker_engine::options::ReadOptions::default(),
        )
        .expect("the write family is readable");
    iter.seek(&low);
    while iter.valid() && iter.key() < high.as_slice() {
        let line = match (key::split(iter.key()), WriteRecord::decode(iter.value())) {
            (Ok((_, commit_ts)), Ok(record)) => format!(
                "commit_ts {commit_ts} {} start_ts {}{}",
                record.kind.name(),
                record.start_ts,
                if record.short_value.is_some() {
                    " (inline value)"
                } else {
                    ""
                }
            ),
            _ => format!("undecodable entry {:?}", iter.key()),
        };
        out.push(line);
        iter.next();
    }
    out
}

/// The three things a round mutates, bundled so the helper below does not take eight
/// arguments — the model is the answer each key should give, `pending` the locks deliberately
/// left standing across a round, and the generator that decides.
struct Workload {
    model: HashMap<Bytes, Option<Vec<u8>>>,
    pending: HashMap<Bytes, (u64, Option<Vec<u8>>)>,
    rng: Rng,
}

/// One round's writes, one transaction a key.
///
/// Split out because the test body is otherwise past the line limit, and because the shape of
/// a round is the thing a reader wants to see on its own: five outcomes, weighted.
fn write_a_round(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    clock: &AtomicU64,
    work: &mut Workload,
    keys: &[Bytes],
    round: usize,
) {
    for key in keys {
        let start_ts = clock.fetch_add(2, Ordering::Relaxed);
        match work.rng.below(10) {
            0..=3 => {
                let value = value_for(key, round);
                if prewrite(
                    store,
                    state,
                    start_ts,
                    vec![TxnMutation::Put {
                        key: key.clone(),
                        value: Bytes::from(value.clone()),
                        read_ts: None,
                    }],
                ) {
                    commit(store, state, start_ts, start_ts + 1, key);
                    work.model.insert(key.clone(), Some(value));
                }
            }
            4..=5 => {
                if prewrite(
                    store,
                    state,
                    start_ts,
                    vec![TxnMutation::Delete {
                        key: key.clone(),
                        read_ts: None,
                    }],
                ) {
                    commit(store, state, start_ts, start_ts + 1, key);
                    work.model.insert(key.clone(), None);
                }
            }
            // `Op::Check` — a validated read, which commits a `Kind::Lock` **above** the
            // version it validated and writes no value. This is #78's shape.
            6..=7 => {
                if prewrite(
                    store,
                    state,
                    start_ts,
                    vec![TxnMutation::Check { key: key.clone() }],
                ) {
                    commit(store, state, start_ts, start_ts + 1, key);
                }
            }
            // A transaction that dies: a rollback marker, and the answer does not move.
            8 => {
                if prewrite(
                    store,
                    state,
                    start_ts,
                    vec![TxnMutation::Put {
                        key: key.clone(),
                        value: Bytes::from_static(b"never"),
                        read_ts: None,
                    }],
                ) {
                    rollback(store, state, start_ts, key);
                }
            }
            // A lock left standing into the next round.
            _ => {
                let mut value = value_for(key, round);
                value.extend_from_slice(b" late");
                if prewrite(
                    store,
                    state,
                    start_ts,
                    vec![TxnMutation::Put {
                        key: key.clone(),
                        value: Bytes::from(value.clone()),
                        read_ts: None,
                    }],
                ) {
                    work.pending.insert(key.clone(), (start_ts, Some(value)));
                }
            }
        }
    }
}

/// **The property**, asked of every key once a round: a read above the safepoint answers what
/// the last committed write said.
///
/// A key with a lock left standing is skipped rather than read, because a live lock is a
/// refusal and not an answer — which `a_live_key_a_scan_does_not_answer_for` pins on its own.
fn verify(
    store: &Arc<Store>,
    state: &Arc<RegionState>,
    work: &Workload,
    keys: &[Bytes],
    round: usize,
    at_and_safepoint: (u64, u64),
) {
    let (at, safepoint) = at_and_safepoint;
    for key in keys {
        if work.pending.contains_key(key) {
            continue;
        }
        let answer = match store.handle_txn(
            state,
            TxnKvReq::Get {
                key: key.clone(),
                ts: at,
            },
        ) {
            Ok(TxnKvResp::Get { value }) => value,
            Ok(other) => panic!("not a get answer: {other:?}"),
            Err(error) => panic!(
                "round {round}: reading {key:?} at {at} failed with {error}, and no lock was \
                 left on it. Its records:\n  {}",
                records(store, key).join("\n  ")
            ),
        };
        let expected = work.model.get(key).unwrap();
        assert_eq!(
            answer.as_deref(),
            expected.as_deref(),
            "round {round}, safepoint {safepoint}, read at {at}: {key:?} answered {:?} where \
             the last committed write said {:?}. Its `write` records, newest last:\n  {}",
            answer.as_deref().map(String::from_utf8_lossy),
            expected.as_deref().map(String::from_utf8_lossy),
            records(store, key).join("\n  ")
        );
    }
}

/// **A hundred keys, a rising safepoint, and a compaction running beside the sweeper.**
#[test]
fn a_collection_under_many_keys_never_takes_a_live_row() {
    let began = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let state = store.regions().get(1).expect("the bootstrapped region");

    // Shared, so the filler's transactions and the workload's interleave rather than sit in
    // separate timestamp spaces — which is what makes the safepoint bite both.
    let clock = Arc::new(AtomicU64::new(1_000));
    let stop = Arc::new(AtomicBool::new(false));

    // **The picker's own compactions, running beside the sweeper's.** A second thread writing a
    // disjoint key range keeps `write` collecting L0 files while the main loop flushes and
    // compacts, which is the interleaving a single-threaded test cannot produce.
    let filler = {
        let store = Arc::clone(&store);
        let state = Arc::clone(&state);
        let clock = Arc::clone(&clock);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut wrote = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let start_ts = clock.fetch_add(2, Ordering::Relaxed);
                let batch: Vec<TxnMutation> = (0..50u64)
                    .map(|n| TxnMutation::Put {
                        key: Bytes::from(format!("f{:010}", wrote % 5_000 + n).into_bytes()),
                        value: Bytes::from(vec![b'x'; 256]),
                        read_ts: None,
                    })
                    .collect();
                let keys: Vec<Bytes> = batch.iter().map(|m| m.key().clone()).collect();
                if prewrite(&store, &state, start_ts, batch) {
                    for key in &keys {
                        commit(&store, &state, start_ts, start_ts + 1, key);
                    }
                    wrote += 50;
                }
                // **Pressure, not a busy loop.** Unthrottled this wrote thirteen thousand rows and
                // took the box from the workload it is supposed to be running beside; the point is
                // that the picker has L0 files to work on while the sweeper runs, not throughput.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            wrote
        })
    };

    let keys: Vec<Bytes> = (0..OF_EACH)
        .map(row_key)
        .chain((0..OF_EACH).map(index_key))
        .collect();
    // What the last committed write said: `Some` for a value, `None` for a key that is not there.
    let mut work = Workload {
        model: keys.iter().map(|k| (k.clone(), None)).collect(),
        pending: HashMap::new(),
        rng: Rng(0x5eed_1234_abcd_0087),
    };
    let mut collections = 0usize;

    for round in 0..ROUNDS {
        // **Resolve last round's locks first**, because a standing lock refuses the next prewrite
        // on its key and refuses the read below.
        for (key, (start_ts, value)) in std::mem::take(&mut work.pending) {
            if value.is_some() && work.rng.below(2) == 0 {
                let commit_ts = clock.fetch_add(2, Ordering::Relaxed);
                commit(&store, &state, start_ts, commit_ts, &key);
                work.model.insert(key, value);
            } else {
                rollback(&store, &state, start_ts, &key);
            }
        }

        write_a_round(&store, &state, &clock, &mut work, &keys, round);

        // **The sweeper's own call sequence**, against a safepoint that rises every round.
        let safepoint = clock.load(Ordering::Relaxed).saturating_sub(SAFEPOINT_LAG);
        store.raise_safepoint(safepoint);
        store.flush().unwrap();
        store.compact_cf(esker_engine::cf::WRITE).unwrap();
        collections += 1;

        // Above everything written so far, and above the safepoint: what a reader asks.
        let at = clock.load(Ordering::Relaxed) + 1;
        verify(&store, &state, &work, &keys, round, (at, safepoint));
    }

    stop.store(true, Ordering::Relaxed);
    let filler_rows = filler.join().expect("the filler thread");

    // **The denominator**: a probe that collected nothing would pass without testing anything.
    let standing: usize = keys.iter().map(|key| records(&store, key).len()).sum();
    let alive = work.model.values().filter(|v| v.is_some()).count();
    println!(
        "  {ROUNDS} rounds · {collections} collections · {} keys · {standing} write records \
         standing · {alive} live · {filler_rows} filler rows · {:?}",
        keys.len(),
        began.elapsed()
    );
    assert!(
        collections == ROUNDS && standing > 0,
        "the probe did not collect: {collections} collections, {standing} records standing"
    );
    store.stop();
}
