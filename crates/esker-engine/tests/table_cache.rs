//! The table cache evicts by use, not by file number.
//!
//! The rule this replaced picked the **lowest file number** as its victim, which is not a neutral
//! choice dressed up as one. File numbers rise monotonically, so the lowest is the oldest file,
//! which in a levelled engine is the one that has survived the most compactions — the deepest,
//! largest, most-read file in the tree. The cache was systematically throwing away its best
//! entry.
//!
//! What makes that testable is `Options::max_open_tables`. A cache with room for 256 readers on a
//! database with a dozen files never evicts anything, so the question "which one goes" has no
//! observable answer; turned down to two, it has exactly one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{CfOptions, Options, ReadOptions};
use esker_engine::{Db, cf};

fn key(n: u32) -> Vec<u8> {
    format!("key-{n:05}").into_bytes()
}

/// A database with `max_open_tables` readers of room, filled into many small files.
fn open(fs: &Arc<dyn FileSystem>, max_open_tables: usize) -> Db {
    Db::open_with(
        "/db",
        Options {
            create_if_missing: true,
            max_open_tables,
            cf_options: CfOptions {
                write_buffer_size: 4 * 1024,
                target_file_size: 2 * 1024,
                // No filter, so a point read that misses cannot be short-circuited before it
                // reaches the file — the cache has to be consulted for the answer to be about
                // the cache.
                bloom_bits_per_key: 0,
                ..CfOptions::default()
            },
            ..Options::default()
        },
        Arc::clone(fs),
        &[cf::DEFAULT],
    )
    .unwrap()
}

fn counter(db: &Db, name: &str) -> u64 {
    db.property(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no property {name}"))
}

const KEYS: u32 = 2_000;

/// Iterations of the hot/wandering/cold cycle.
const ROUNDS: u32 = 200;

fn fill(db: &Db) {
    for n in 0..KEYS {
        db.put(cf::DEFAULT, &key(n), b"v").unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();
}

fn files_below_l0(db: &Db) -> usize {
    (1..7)
        .map(|level| {
            db.property(&format!("esker.num-files-at-level{level}.{}", cf::DEFAULT))
                .and_then(|count| count.parse::<usize>().ok())
                .unwrap_or(0)
        })
        .sum()
}

/// A key in the lowest-numbered file of the level, and one in the highest-numbered.
///
/// A compaction writes its outputs in key order, so the file holding `key(0)` carries the lowest
/// number in the level. That is the whole setup: under the old rule that file was the victim of
/// every eviction, however often it was read.
fn hot_and_cold_keys() -> (Vec<u8>, Vec<u8>) {
    (key(0), key(KEYS - 1))
}

#[test]
fn a_file_read_every_time_is_not_the_one_evicted() {
    let inner = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = inner.clone();
    // Room for **three** readers against a level of many files, and three distinct files touched
    // per round. Three is the smallest capacity that can discriminate: with two, the round's own
    // third lookup evicts the hot file whatever the rule is, and both rules score the same — the
    // first version of this test used two and measured 0.215 either way, which is a test that
    // cannot see the bug it is about.
    let db = open(&fs, 3);
    fill(&db);
    let files = files_below_l0(&db);
    assert!(
        files >= 4,
        "only {files} file(s) below L0, so nothing is ever evicted and this test asserts nothing"
    );

    let (hot, cold) = hot_and_cold_keys();
    let read = ReadOptions::default();

    // Warm the hot file, then read it and a different file alternately. The hot key is asked for
    // on every iteration, so an LRU keeps its file resident throughout.
    assert!(db.get(cf::DEFAULT, &hot, &read).unwrap().is_some());
    let hits_before = counter(&db, "esker.table-cache-hits");
    let misses_before = counter(&db, "esker.table-cache-misses");

    for round in 0..ROUNDS {
        assert!(db.get(cf::DEFAULT, &hot, &read).unwrap().is_some());
        // A different cold key each round, so the second cache slot is always a miss and the
        // hot file is the only thing an LRU could be keeping.
        let wandering = key((round * 7) % (KEYS - 1) + 1);
        let _unused = db.get(cf::DEFAULT, &wandering, &read).unwrap();
        let _unused = db.get(cf::DEFAULT, &cold, &read).unwrap();
    }

    let hits = counter(&db, "esker.table-cache-hits") - hits_before;
    let misses = counter(&db, "esker.table-cache-misses") - misses_before;
    // Two counters that cannot exceed a few thousand here, so the cast is exact; written with
    // `precision_loss` in mind rather than around it.
    let rate = f64::from(u32::try_from(hits).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(hits + misses).unwrap_or(u32::MAX));

    println!(
        "hit rate {rate:.3} ({hits} hits, {misses} misses, {} evictions) over {files} files",
        counter(&db, "esker.table-cache-evictions")
    );

    // **Measured, both ways.** On this fixture — 2,000 keys over 7 files with room for 3 — the
    // rules score:
    //
    //     least-recently-used   0.992  (595 hits, 5 misses, 254 evictions)
    //     lowest file number    0.615  (369 hits, 231 misses, 469 evictions)
    //
    // Two of the three files touched per round never change, so an LRU keeps both and almost
    // everything hits. Evicting by file number throws out the *lowest-numbered* file on every
    // insert, which is the hot one, so it is reopened every round — and the cache then evicts
    // nearly twice as often, because everything it discards is something that gets asked for
    // again.
    //
    // The threshold sits between the two with room on both sides. It was 0.55 first, which is
    // below *both* numbers: a test that passes against the rule it was written to catch.
    assert!(
        rate > 0.90,
        "hit rate {rate:.3} over {ROUNDS} rounds ({hits} hits, {misses} misses) across {files} \
         files with room for 3. A file read on every iteration has to stay resident; evicting by \
         file number throws out exactly the one that is being used, and scores 0.615 here."
    );
    assert!(
        counter(&db, "esker.table-cache-evictions") > 0,
        "nothing was ever evicted, so the victim rule was never consulted"
    );
}

#[test]
fn the_cache_never_exceeds_its_capacity() {
    let inner = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = inner.clone();
    let db = open(&fs, 3);
    fill(&db);
    assert!(files_below_l0(&db) >= 4);

    let read = ReadOptions::default();
    for n in (0..KEYS).step_by(3) {
        let _unused = db.get(cf::DEFAULT, &key(n), &read).unwrap();
        assert!(
            counter(&db, "esker.open-tables") <= 3,
            "the cache holds more readers than its capacity; each one is a file descriptor and a \
             resident index, so an unbounded cache is an unbounded process"
        );
    }
}

#[test]
fn a_capacity_of_one_still_serves_reads() {
    // The degenerate case, and the one an "evict everything but what I just inserted" loop can
    // spin on. Asserted because the eviction loop has a branch for exactly this.
    let inner = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = inner.clone();
    let db = open(&fs, 1);
    fill(&db);

    let read = ReadOptions::default();
    for n in (0..KEYS).step_by(101) {
        assert!(
            db.get(cf::DEFAULT, &key(n), &read).unwrap().is_some(),
            "key {n} was not found with a one-reader cache"
        );
        assert!(counter(&db, "esker.open-tables") <= 1);
    }
}
