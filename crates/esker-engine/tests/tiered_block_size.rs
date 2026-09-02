//! A tiered database writes 16 KiB blocks; a local one still writes 4 KiB.
//!
//! `docs/bench/phase-11-engine.md` §3 measured why: tiered into object storage a block is **one
//! ranged `GET`** — `TieredFile::read_at` issues exactly one per call — so the block size is the
//! round-trip granularity of every cold read, and 4 KiB makes a scan pay a round trip per 4 KiB
//! of it. A scan's `GET` count fell 1,393 → 349 going from 4 KiB to 16 KiB, and point reads got
//! slightly *faster* rather than slower.
//!
//! Three things need proving, and only the first is about the number:
//!
//! 1. a tiered family resolves to 16 KiB and a local one to 4 KiB;
//! 2. **a caller who asked for a size gets it**, tiered or not — the whole reason
//!    `BlockSize::Storage` is a variant rather than a magic default value;
//! 3. an SST written at 4 KiB is still readable after the default moves, because a block size is
//!    a property of the file that was written and not of the database reading it.
//!
//! `MemoryStore` rather than a container, for the reason `tests/tier.rs` gives: none of this is
//! about HTTP.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::dbformat::{BytewiseComparator, Comparator, InternalKeyComparator};
use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::options::{BlockSize, CfOptions, Options, ReadOptions, defaults};
use esker_engine::sst::{TableOptions, TableReader};
use esker_engine::{Db, cf};
use esker_s3::{MemoryStore, ObjectStore};

fn key(n: u32) -> Vec<u8> {
    format!("key-{n:06}").into_bytes()
}

fn options(block_size: BlockSize) -> Options {
    Options {
        create_if_missing: true,
        compaction_threads: 1,
        cf_options: CfOptions {
            block_size,
            // Small, so a few hundred keys become a real SST rather than staying in the memtable.
            write_buffer_size: 8 * 1024,
            ..CfOptions::default()
        },
        ..Options::default()
    }
}

/// A database on plain local disk.
fn local(dir: &std::path::Path, block_size: BlockSize) -> Db {
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    Db::open_with(dir, options(block_size), fs, &[cf::DEFAULT]).unwrap()
}

/// A database whose SSTs tier into a `MemoryStore`.
fn tiered(dir: &std::path::Path, store: &Arc<MemoryStore>, block_size: BlockSize) -> Db {
    tiered_with_budget(dir, store, block_size, TierOptions::default().local_budget)
}

/// The same, with an explicit local byte budget.
fn tiered_with_budget(
    dir: &std::path::Path,
    store: &Arc<MemoryStore>,
    block_size: BlockSize,
    local_budget: Option<u64>,
) -> Db {
    let fs = TieredFileSystem::new(
        Arc::new(LocalFileSystem::new()),
        Arc::clone(store) as Arc<dyn ObjectStore>,
        dir,
        TierOptions {
            background: false,
            local_budget,
            ..TierOptions::default()
        },
    )
    .unwrap();
    Db::open_with(
        dir,
        options(block_size),
        fs as Arc<dyn FileSystem>,
        &[cf::DEFAULT],
    )
    .unwrap()
}

fn resolved(db: &Db) -> usize {
    db.property(&format!("esker.block-size.{}", cf::DEFAULT))
        .and_then(|value| value.parse().ok())
        .expect("the resolved block size is a property")
}

fn fill(db: &Db, count: u32) {
    for n in 0..count {
        db.put(
            cf::DEFAULT,
            &key(n),
            b"a value long enough to fill a block or two",
        )
        .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
}

#[test]
fn the_storage_decides_and_the_two_answers_differ() {
    let local_dir = tempfile::tempdir().unwrap();
    let tiered_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());

    let on_disk = local(local_dir.path(), BlockSize::Storage);
    let in_object_storage = tiered(tiered_dir.path(), &store, BlockSize::Storage);

    assert_eq!(
        resolved(&on_disk),
        defaults::BLOCK_SIZE,
        "the local default moved; this change was supposed to leave it alone"
    );
    assert_eq!(
        resolved(&in_object_storage),
        defaults::TIERED_BLOCK_SIZE,
        "a tiered family did not pick up the tiered default"
    );
    assert_eq!(defaults::TIERED_BLOCK_SIZE, 16 * 1024);
    assert!(
        defaults::TIERED_BLOCK_SIZE > defaults::BLOCK_SIZE,
        "the point of the tiered default is that a block is a round trip there"
    );
}

#[test]
fn a_caller_who_asked_for_a_size_gets_it_on_either_storage() {
    // **The reason `BlockSize` is an enum.** Had the resolution been "override the value when it
    // equals the local default", this database — which deliberately asked for 4 KiB on tiered
    // storage — would have been silently given 16 KiB, and there would be no way to ask for the
    // small one. That is `dd182cb`'s bug in a different field.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let db = tiered(dir.path(), &store, BlockSize::Fixed(defaults::BLOCK_SIZE));
    assert_eq!(
        resolved(&db),
        defaults::BLOCK_SIZE,
        "an explicit 4 KiB on a tiered database was overridden, so the value the caller wrote is \
         indistinguishable from having written nothing"
    );

    let local_dir = tempfile::tempdir().unwrap();
    let db = local(local_dir.path(), BlockSize::Fixed(64 * 1024));
    assert_eq!(resolved(&db), 64 * 1024);
}

#[test]
fn a_table_written_at_four_kibibytes_still_reads_at_sixteen() {
    // A block size is a property of the file that was written, not of the database reading it: a
    // reader takes every block bound from the table's own index and never consults
    // `TableOptions::block_size` at all. So this is not a compatibility *shim* being tested, it is
    // the claim that no shim is needed — and the way to be sure of that is to write a database at
    // 4 KiB, reopen it at 16 KiB, and read every key back.
    const KEYS: u32 = 400;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    {
        let db = tiered(dir.path(), &store, BlockSize::Fixed(defaults::BLOCK_SIZE));
        assert_eq!(resolved(&db), defaults::BLOCK_SIZE);
        fill(&db, KEYS);
        // Uploaded, so the reopened database really is reading the tiered copy.
        db.tier_maintenance().unwrap();
    }

    let db = tiered(dir.path(), &store, BlockSize::Storage);
    assert_eq!(
        resolved(&db),
        defaults::TIERED_BLOCK_SIZE,
        "the reopened database did not pick up the new default, so this test proves nothing"
    );

    let read = ReadOptions::default();
    for n in 0..KEYS {
        assert!(
            db.get(cf::DEFAULT, &key(n), &read).unwrap().is_some(),
            "key {n}, written into a 4 KiB-block table, was lost when the default moved to 16 KiB"
        );
    }

    // And a scan, because a point read reaches a block through the index while an iterator walks
    // blocks in order — two different paths through the same bounds.
    let mut iter = db.iter(cf::DEFAULT, &read).unwrap();
    iter.seek_to_first();
    let mut seen = 0;
    while iter.valid() {
        seen += 1;
        iter.next();
    }
    iter.status().unwrap();
    assert_eq!(seen, KEYS as usize, "a scan of the old tables lost keys");
}

#[test]
fn a_tiered_database_writes_fewer_and_wider_blocks() {
    // The other half of the compatibility claim: old tables are untouched, and new ones really
    // are written with the new size. Asserted on `data_block_count` read back off the table
    // itself — the number of blocks *is* the number of ranged `GET`s a cold scan will cost,
    // because `TieredFile::read_at` issues exactly one per block read.
    //
    // Two earlier probes were wrong and are worth naming so nobody repeats them. Bytes on disk
    // came out identical to the byte (39,823 both ways): an index entry and a per-block trailer
    // are a rounding error next to the keys, and LZ4 absorbs the rest. `TierStats::ranged_reads`
    // over a scan came out 15 both ways because with the local copies still resident the scan was
    // served locally — 57 cache hits against 3 misses — so it never touched the tier at all.
    const KEYS: u32 = 4_000;

    let blocks = |block_size: BlockSize| -> u64 {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::new());
        let db = tiered(dir.path(), &store, block_size);
        fill(&db, KEYS);
        db.compact_range(cf::DEFAULT, None, None).unwrap();
        data_block_count(dir.path())
    };

    let narrow = blocks(BlockSize::Fixed(defaults::BLOCK_SIZE));
    let wide = blocks(BlockSize::Storage);

    println!("data_block_count: {narrow} at 4 KiB, {wide} at 16 KiB");

    assert!(
        narrow > 4,
        "only {narrow} block(s) at 4 KiB, so there is nothing for a wider block to merge"
    );
    // Four times the block size, so about a quarter of the blocks. Asserted as "fewer than half"
    // rather than the exact ratio, because a block is cut when it *reaches* the size and the last
    // one of each table is short.
    assert!(
        wide * 2 < narrow,
        "the same {KEYS} keys came to {wide} blocks at 16 KiB and {narrow} at 4 KiB. Four times \
         the block size should be about a quarter of the blocks; near parity means the resolved \
         size never reached the table builder."
    );
}

/// Total `data_block_count` across every local `*.sst` under `dir`.
///
/// Opened with `TableReader` directly rather than through the `Db`, because the question is what
/// is *in the file* and the database has no reason to expose that.
fn data_block_count(dir: &std::path::Path) -> u64 {
    let fs = LocalFileSystem::new();
    let mut blocks = 0;
    for path in fs.list(dir).unwrap() {
        let Some(esker_engine::filename::FileKind::Sst(number)) =
            esker_engine::filename::classify_path(&path)
        else {
            continue;
        };
        let file = fs.open(&path).unwrap();
        // The engine's tables are keyed by internal keys, so a reader built with the default
        // (bytewise) comparator refuses them by name — which is the check working, not a problem.
        let options = TableOptions {
            comparator: Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
                as Arc<dyn Comparator>,
            ..TableOptions::default()
        };
        let reader = TableReader::open(file, number, options, None).unwrap();
        blocks += reader.properties().data_block_count;
    }
    blocks
}
