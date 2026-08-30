//! Eight writers, eight readers, and flushes underneath them.
//!
//! `prompts/01-engine.md`'s third required test: "8 writer threads + 8 reader threads +
//! flush/compaction for 30 s; then model check". The model test drives one thread and asks
//! whether the engine computes the right answer; this one asks whether it still does while
//! sixteen threads and a background flush are moving underneath the read path.
//!
//! # Writers own disjoint stripes
//!
//! Writer *t* touches only keys `s{t}-k….` Nothing else writes them, so the last write to a
//! key is knowable without any coordination between threads — which is what makes the final
//! comparison exact rather than approximate. Values encode `(stripe, generation)` and a
//! length derived from the generation, so a value is checkable three ways: it names the stripe
//! its key belongs to, it names when it was written, and its length has to match.
//!
//! # What a reader may see
//!
//! Anything, as long as it is real. A read of a key may return nothing — the key may not be
//! written yet, or may have been deleted — but a value it *does* return must belong to that
//! key's stripe and must not be **newer than the writer had started writing** at the moment of
//! the read. That bound is what catches a torn or invented value, and it works because each
//! writer publishes its generation *before* the write rather than after: publishing after
//! would leave a legitimately-observed value looking like it came from the future.
//!
//! # Snapshots must not move
//!
//! A snapshot read twice must give the same answer, while everything else is still changing.
//! That is the property a read path breaks by forgetting to filter on sequence number, and it
//! is invisible to a single-threaded test because nothing moves between the two reads.
//!
//! # Journals stay in their own threads
//!
//! Each writer keeps a plain `Vec` and hands it back when it is joined. A shared, locked
//! journal would serialise the writers and the test would measure the lock rather than the
//! engine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions};
use esker_engine::{Db, cf};

const DIR: &str = "/db";
const WRITERS: usize = 8;
const READERS: usize = 8;

/// Keys per stripe. Small enough that every key is overwritten many times in a short run,
/// which is where a read path picks the wrong version.
const SLOTS: u32 = 48;

/// Wall clock for the default run, and for the one behind `--ignored`.
const SECONDS: u64 = 3;
const SECONDS_LONG: u64 = 30;

// TODO(spine step 7): mix `compact` in beside the flushes once `Db` has a compaction API —
// same shape as here, and the same final comparison. `compaction_still_has_no_public_api` in
// tests/model.rs is the canary that fails when the API appears.

fn key_of(stripe: usize, slot: u32) -> Vec<u8> {
    format!("s{stripe:02}-k{slot:04}").into_bytes()
}

/// The stripe and slot a key belongs to, or `None` if it is not one of ours.
fn parse_key(key: &[u8]) -> Option<(usize, u32)> {
    let text = std::str::from_utf8(key).ok()?;
    let rest = text.strip_prefix('s')?;
    let (stripe, rest) = rest.split_once("-k")?;
    Some((stripe.parse().ok()?, rest.parse().ok()?))
}

/// The value writer `stripe` writes at generation `generation`.
///
/// The trailing padding varies with the generation, so a value that is the right text with the
/// wrong length — a torn write, or two values spliced — does not pass inspection.
fn value_of(stripe: usize, generation: u64) -> Vec<u8> {
    let padding = usize::try_from(generation % 29).unwrap_or(0);
    let mut out = format!("s{stripe:02}-g{generation:012}-").into_bytes();
    out.extend(std::iter::repeat_n(b'x', padding));
    out
}

/// The stripe and generation a value claims, checked against its own length.
fn parse_value(value: &[u8]) -> Option<(usize, u64)> {
    let text = std::str::from_utf8(value).ok()?;
    let rest = text.strip_prefix('s')?;
    let (stripe, rest) = rest.split_once("-g")?;
    let (generation, padding) = rest.split_once('-')?;
    let stripe: usize = stripe.parse().ok()?;
    let generation: u64 = generation.parse().ok()?;
    // The length is part of the value's identity, not decoration.
    if padding.len() != usize::try_from(generation % 29).unwrap_or(0)
        || !padding.bytes().all(|byte| byte == b'x')
    {
        return None;
    }
    Some((stripe, generation))
}

/// What one writer did, in order.
#[derive(Debug, Clone, Copy)]
enum Wrote {
    Put(u32, u64),
    Delete(u32),
}

/// Everything the threads share.
struct Shared {
    db: Db,
    stop: AtomicBool,
    /// Per writer: the generation it has *started* writing. Published before the write, so a
    /// value a reader sees can never look newer than this.
    started: Vec<AtomicU64>,
    /// Reads performed, for the summary and to prove the readers were not idle.
    reads: AtomicU64,
    snapshot_checks: AtomicU64,
    flushes: AtomicU64,
}

/// Checks one entry a reader saw. `bound` is the highest generation any writer of that stripe
/// had started, sampled after the read.
fn check_entry(key: &[u8], value: &[u8], bounds: &[u64]) -> Result<(), String> {
    let Some((stripe, _slot)) = parse_key(key) else {
        return Err(format!(
            "a scan returned {:?}, which is not a key any writer writes",
            String::from_utf8_lossy(key)
        ));
    };
    let Some((value_stripe, generation)) = parse_value(value) else {
        return Err(format!(
            "key {:?} holds {:?}, which is not a value any writer writes",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(value)
        ));
    };
    if value_stripe != stripe {
        return Err(format!(
            "key {:?} of stripe {stripe} holds a value written by stripe {value_stripe}",
            String::from_utf8_lossy(key)
        ));
    }
    let bound = bounds.get(stripe).copied().unwrap_or(0);
    if generation > bound {
        return Err(format!(
            "key {:?} holds generation {generation}, but stripe {stripe} had only started {bound}",
            String::from_utf8_lossy(key)
        ));
    }
    Ok(())
}

/// The generation each writer has started, sampled now.
fn bounds(shared: &Shared) -> Vec<u64> {
    shared
        .started
        .iter()
        .map(|counter| counter.load(Ordering::Acquire))
        .collect()
}

/// One writer: its own stripe, its own journal, nothing shared but the database.
fn writer(shared: &Shared, stripe: usize, deadline: Instant) -> Result<Vec<Wrote>, String> {
    let mut journal = Vec::new();
    let mut rng = Pcg32::from_seed(0x_5751_7E00_u64.wrapping_mul(stripe as u64 + 1));
    let mut generation = 0u64;

    while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
        for _ in 0..32 {
            let slot = rng.below(SLOTS);
            // Published *before* the write: a value a reader sees must never look newer than
            // what its writer had begun.
            shared.started[stripe].store(generation, Ordering::Release);

            if generation % 11 == 10 {
                shared
                    .db
                    .delete(cf::DEFAULT, &key_of(stripe, slot))
                    .map_err(|error| format!("writer {stripe}: delete: {error}"))?;
                journal.push(Wrote::Delete(slot));
            } else {
                shared
                    .db
                    .put(
                        cf::DEFAULT,
                        &key_of(stripe, slot),
                        &value_of(stripe, generation),
                    )
                    .map_err(|error| format!("writer {stripe}: put: {error}"))?;
                journal.push(Wrote::Put(slot, generation));
            }
            generation += 1;
        }
    }
    // The last generation is finished, so the bound can advance past it.
    shared.started[stripe].store(generation, Ordering::Release);
    Ok(journal)
}

/// One reader: point lookups, bounded scans, and snapshots that must not move.
fn reader(shared: &Shared, index: usize, deadline: Instant) -> Result<(), String> {
    let mut rng = Pcg32::from_seed(0x_2EAD_E200_u64.wrapping_mul(index as u64 + 1));

    while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
        let stripe = usize::try_from(rng.below(u32::try_from(WRITERS).unwrap_or(1))).unwrap_or(0);
        let slot = rng.below(SLOTS);

        // A point lookup. Nothing is fine; something has to be real.
        let key = key_of(stripe, slot);
        let found = shared
            .db
            .get(cf::DEFAULT, &key, &ReadOptions::default())
            .map_err(|error| format!("reader {index}: get: {error}"))?;
        let after = bounds(shared);
        if let Some(value) = &found {
            check_entry(&key, value, &after)?;
        }
        shared.reads.fetch_add(1, Ordering::Relaxed);

        // A bounded scan of one stripe, both directions on alternate rounds.
        let lo = key_of(stripe, 0);
        let hi = key_of(stripe, SLOTS);
        let reverse = rng.chance(0.5);
        let seen = scan(shared, &lo, &hi, reverse)
            .map_err(|error| format!("reader {index}: scan: {error}"))?;
        let after = bounds(shared);
        for (key, value) in &seen {
            check_entry(key, value, &after)?;
        }
        shared.reads.fetch_add(seen.len() as u64, Ordering::Relaxed);

        // A snapshot, read twice. Nothing else may move it.
        let snapshot = shared.db.snapshot();
        let options = ReadOptions {
            snapshot: Some(snapshot.clone()),
            ..ReadOptions::default()
        };
        let first = scan_with(shared, &lo, &hi, false, &options)
            .map_err(|error| format!("reader {index}: snapshot scan: {error}"))?;
        let after = bounds(shared);
        for (key, value) in &first {
            check_entry(key, value, &after)?;
        }
        // Give the writers a moment to change the database out from under it.
        std::thread::yield_now();
        let second = scan_with(shared, &lo, &hi, false, &options)
            .map_err(|error| format!("reader {index}: snapshot rescan: {error}"))?;
        if first != second {
            return Err(format!(
                "reader {index}: a snapshot of stripe {stripe} returned {} entries and then \
                 {}; a snapshot must not move",
                first.len(),
                second.len()
            ));
        }
        // And a point read through the same snapshot must agree with the scan it just did.
        let through = shared
            .db
            .get(cf::DEFAULT, &key, &options)
            .map_err(|error| format!("reader {index}: snapshot get: {error}"))?;
        let expected = first
            .iter()
            .find(|(seen_key, _)| seen_key == &key)
            .map(|(_, value)| value.clone());
        if through.map(|value| value.to_vec()) != expected {
            return Err(format!(
                "reader {index}: a snapshot's get and scan disagree about {:?}",
                String::from_utf8_lossy(&key)
            ));
        }
        shared.snapshot_checks.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn scan(
    shared: &Shared,
    lo: &[u8],
    hi: &[u8],
    reverse: bool,
) -> esker_engine::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    scan_with(shared, lo, hi, reverse, &ReadOptions::default())
}

fn scan_with(
    shared: &Shared,
    lo: &[u8],
    hi: &[u8],
    reverse: bool,
    options: &ReadOptions,
) -> esker_engine::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut iter = shared.db.iter(cf::DEFAULT, options)?;
    let mut out = Vec::new();
    if reverse {
        iter.seek_for_prev(hi);
        while iter.valid() && iter.key() >= lo {
            out.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.prev();
        }
        out.reverse();
    } else {
        iter.seek(lo);
        while iter.valid() && iter.key() <= hi {
            out.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.next();
        }
    }
    iter.status()?;
    Ok(out)
}

/// Flushes the column family every so often, so the read path is crossing memtables and L0
/// files rather than only memtables.
fn flusher(shared: &Shared, deadline: Instant) -> Result<(), String> {
    while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(40));
        shared
            .db
            .flush(cf::DEFAULT)
            .map_err(|error| format!("flusher: {error}"))?;
        shared.flushes.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// Runs the whole thing for `seconds`, then verifies the database against the journals.
fn run(seconds: u64) {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let db = Db::open_with(
        DIR,
        Options {
            create_if_missing: true,
            ..Options::default()
        },
        Arc::clone(&fs),
        &[cf::DEFAULT],
    )
    .expect("opening the database");

    let shared = Arc::new(Shared {
        db,
        stop: AtomicBool::new(false),
        started: (0..WRITERS).map(|_| AtomicU64::new(0)).collect(),
        reads: AtomicU64::new(0),
        snapshot_checks: AtomicU64::new(0),
        flushes: AtomicU64::new(0),
    });
    // Everyone starts together, so the readers are not reading an empty database for the
    // first half of the run.
    let barrier = Arc::new(Barrier::new(WRITERS + READERS + 1));
    let deadline = Instant::now() + Duration::from_secs(seconds);

    let mut writers = Vec::new();
    for stripe in 0..WRITERS {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            writer(&shared, stripe, deadline)
        }));
    }
    let mut readers = Vec::new();
    for index in 0..READERS {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        readers.push(std::thread::spawn(move || {
            barrier.wait();
            reader(&shared, index, deadline)
        }));
    }
    let flush_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || flusher(&shared, deadline))
    };
    barrier.wait();

    // Writers first: the readers must keep running while the writers stop, which is when a
    // read path that caches something stale gets its last chance to be wrong.
    let mut journals = Vec::new();
    let mut failures = Vec::new();
    for handle in writers {
        match handle.join().expect("a writer thread panicked") {
            Ok(journal) => journals.push(journal),
            Err(reason) => failures.push(reason),
        }
    }
    shared.stop.store(true, Ordering::Relaxed);
    for handle in readers {
        if let Err(reason) = handle.join().expect("a reader thread panicked") {
            failures.push(reason);
        }
    }
    if let Err(reason) = flush_thread.join().expect("the flush thread panicked") {
        failures.push(reason);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    verify(&shared, &journals, seconds);
}

/// Compares the settled database with the journals, exactly.
fn verify(shared: &Shared, journals: &[Vec<Wrote>], seconds: u64) {
    // The journals are the specification now: for each key, the last thing written to it.
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut journalled = 0usize;
    for (stripe, journal) in journals.iter().enumerate() {
        journalled += journal.len();
        for entry in journal {
            match entry {
                Wrote::Put(slot, generation) => {
                    expected.insert(key_of(stripe, *slot), value_of(stripe, *generation));
                }
                Wrote::Delete(slot) => {
                    expected.remove(&key_of(stripe, *slot));
                }
            }
        }
    }

    let reads = shared.reads.load(Ordering::Relaxed);
    let snapshots = shared.snapshot_checks.load(Ordering::Relaxed);
    let flushes = shared.flushes.load(Ordering::Relaxed);
    println!(
        "concurrency: {WRITERS} writers, {READERS} readers, {seconds}s — {journalled} writes, \
         {reads} reads, {snapshots} snapshot comparisons, {flushes} flushes, \
         {} keys live at the end",
        expected.len()
    );
    assert!(
        journalled > 1_000,
        "only {journalled} writes: the run was not a run"
    );
    assert!(reads > 1_000, "only {reads} reads");
    assert!(snapshots > 0, "no snapshot was checked twice");
    assert!(flushes > 0, "nothing was flushed");

    // Settled: one full scan each way, compared exactly against the journals.
    let lo = Vec::new();
    let hi = vec![0xff; 8];
    let forward = scan(shared, &lo, &hi, false).expect("the final forward scan");
    let backward = scan(shared, &lo, &hi, true).expect("the final reverse scan");
    assert_eq!(forward, backward, "the two directions disagree");

    let final_state: BTreeMap<Vec<u8>, Vec<u8>> = forward.into_iter().collect();
    assert_eq!(
        final_state,
        expected,
        "the database does not match the journals: {}",
        difference(&expected, &final_state)
    );

    // And every key by point lookup, which takes a different path than the scan.
    for (key, value) in &expected {
        let found = shared
            .db
            .get(cf::DEFAULT, key, &ReadOptions::default())
            .expect("the final point lookups");
        assert_eq!(
            found.as_deref(),
            Some(&value[..]),
            "get disagrees with the scan about {:?}",
            String::from_utf8_lossy(key)
        );
    }
}

/// Names the first few keys on which the two disagree, and how.
fn difference(expected: &BTreeMap<Vec<u8>, Vec<u8>>, found: &BTreeMap<Vec<u8>, Vec<u8>>) -> String {
    let show = |keys: Vec<&Vec<u8>>| -> Vec<String> {
        keys.into_iter()
            .take(4)
            .map(|key| String::from_utf8_lossy(key).into_owned())
            .collect()
    };
    let missing = show(
        expected
            .keys()
            .filter(|key| !found.contains_key(*key))
            .collect(),
    );
    let extra = show(
        found
            .keys()
            .filter(|key| !expected.contains_key(*key))
            .collect(),
    );
    let wrong = show(
        expected
            .iter()
            .filter(|(key, value)| found.get(*key).is_some_and(|seen| seen != *value))
            .map(|(key, _)| key)
            .collect(),
    );
    format!(
        "{} keys expected, {} found; missing {missing:?}, unexpected {extra:?}, \
         wrong value {wrong:?}",
        expected.len(),
        found.len()
    )
}

/// The values and keys have to survive a round trip through their own parsers, or every check
/// built on them is checking nothing.
#[test]
fn the_pattern_is_self_describing() {
    for stripe in 0..WRITERS {
        for generation in [0u64, 1, 28, 29, 1_000, 123_456_789] {
            let value = value_of(stripe, generation);
            assert_eq!(parse_value(&value), Some((stripe, generation)));
        }
        for slot in [0u32, 1, SLOTS - 1] {
            assert_eq!(parse_key(&key_of(stripe, slot)), Some((stripe, slot)));
        }
    }

    // And things that are not the pattern are rejected, or a torn value would sail through.
    assert_eq!(parse_value(b""), None);
    assert_eq!(parse_value(b"s00-g000000000000"), None, "no padding field");
    assert_eq!(
        parse_value(b"s00-g000000000001-"),
        None,
        "padding too short"
    );
    assert_eq!(
        parse_value(b"s00-g000000000000-x"),
        None,
        "padding too long"
    );
    assert_eq!(
        parse_value(b"s00-g000000000001-y"),
        None,
        "wrong padding byte"
    );
    assert_eq!(parse_key(b"not-a-key"), None);
    assert_eq!(parse_key(b"s00-k0000extra"), None);
}

/// The default run: three seconds of sixteen threads, then an exact comparison.
#[test]
fn eight_writers_and_eight_readers() {
    run(SECONDS);
}

/// The acceptance run of `prompts/01-engine.md`: thirty seconds.
#[test]
#[ignore = "the 30-second acceptance run"]
fn eight_writers_and_eight_readers_acceptance_run() {
    run(SECONDS_LONG);
}
