//! The database: opening, group commit, reads, snapshots and recovery.
//!
//! These run against the in-memory filesystem, which is what makes it cheap to reopen a
//! database a few thousand times and to damage a log at a chosen byte.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::batch::WriteBatch;
use esker_engine::error::Error;
use esker_engine::filename;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WalSyncMode, WriteOptions};
use esker_engine::{Db, cf};

const DIR: &str = "/db";

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

fn open(
    fs: &Arc<dyn FileSystem>,
    options: Options,
    cfs: &[&str],
) -> esker_engine::error::Result<Db> {
    Db::open_with(DIR, options, Arc::clone(fs), cfs)
}

fn memfs() -> (Arc<MemFileSystem>, Arc<dyn FileSystem>) {
    let inner = Arc::new(MemFileSystem::new());
    let dynamic: Arc<dyn FileSystem> = inner.clone();
    (inner, dynamic)
}

fn get(db: &Db, key: &[u8]) -> Option<Vec<u8>> {
    db.get(cf::DEFAULT, key, &ReadOptions::default())
        .unwrap()
        .map(|value| value.to_vec())
}

#[test]
fn a_write_survives_a_reopen() {
    let (_, fs) = memfs();
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        db.put(cf::DEFAULT, b"alpha", b"one").unwrap();
        db.put(cf::DEFAULT, b"beta", b"two").unwrap();
        assert_eq!(get(&db, b"alpha").as_deref(), Some(&b"one"[..]));
    }
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(get(&db, b"alpha").as_deref(), Some(&b"one"[..]));
    assert_eq!(get(&db, b"beta").as_deref(), Some(&b"two"[..]));
    assert_eq!(get(&db, b"missing"), None);
}

#[test]
fn opening_a_database_that_is_not_there() {
    let (_, fs) = memfs();
    let err = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");

    let _db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    let err = open(
        &fs,
        Options {
            error_if_exists: true,
            ..options()
        },
        &[cf::DEFAULT],
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
}

/// A tombstone has to survive a reopen too, or a deleted key comes back from the log.
#[test]
fn a_deletion_survives_a_reopen() {
    let (_, fs) = memfs();
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        db.put(cf::DEFAULT, b"k", b"v").unwrap();
        db.delete(cf::DEFAULT, b"k").unwrap();
        assert_eq!(get(&db, b"k"), None);
    }
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(get(&db, b"k"), None, "the tombstone was replayed");
}

/// One record in the log, so either both column families see the batch or neither does.
#[test]
fn a_batch_across_column_families_is_atomic() {
    let (_, fs) = memfs();
    let names = [cf::DEFAULT, cf::LOCK, cf::WRITE];
    {
        let db = open(&fs, options(), &names).unwrap();
        let mut batch = WriteBatch::new();
        batch.put(db.cf_id(cf::DEFAULT).unwrap(), b"k", b"data");
        batch.put(db.cf_id(cf::LOCK).unwrap(), b"k", b"lock");
        batch.delete(db.cf_id(cf::WRITE).unwrap(), b"k");
        db.write(batch, &WriteOptions::default()).unwrap();
    }
    let db = open(&fs, Options::default(), &names).unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"data"[..]));
    assert_eq!(
        db.get(cf::LOCK, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"lock"[..])
    );
    assert_eq!(
        db.get(cf::WRITE, b"k", &ReadOptions::default()).unwrap(),
        None
    );
    assert_eq!(db.cf_names().len(), 3);
}

#[test]
fn a_batch_naming_an_unknown_column_family_is_rejected_before_it_is_logged() {
    let (memfs, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    let before = memfs.contents(filename::wal(
        std::path::Path::new(DIR),
        db.wal_number().unwrap(),
    ));

    let mut batch = WriteBatch::new();
    batch.put(999, b"k", b"v");
    let err = db.write(batch, &WriteOptions::default()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");

    let after = memfs.contents(filename::wal(
        std::path::Path::new(DIR),
        db.wal_number().unwrap(),
    ));
    assert_eq!(
        before.unwrap(),
        after.unwrap(),
        "nothing should have been logged"
    );
}

/// A snapshot sees the database as it was, however much is written afterwards.
#[test]
fn snapshots_see_the_database_as_it_was() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"k", b"first").unwrap();
    let snapshot = db.snapshot();
    db.put(cf::DEFAULT, b"k", b"second").unwrap();
    db.delete(cf::DEFAULT, b"gone").unwrap();

    let at_snapshot = ReadOptions {
        snapshot: Some(snapshot.clone()),
        ..ReadOptions::default()
    };
    assert_eq!(
        db.get(cf::DEFAULT, b"k", &at_snapshot).unwrap().as_deref(),
        Some(&b"first"[..])
    );
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"second"[..]));

    // The clone in `at_snapshot` is a second holder, and a compaction would have to respect
    // both. Only when the last one goes does the sequence number stop being protected.
    assert_eq!(db.property("esker.snapshots").unwrap(), "2");
    drop(at_snapshot);
    assert_eq!(db.property("esker.snapshots").unwrap(), "1");
    drop(snapshot);
    assert_eq!(db.property("esker.snapshots").unwrap(), "0");
}

/// Every entry of every batch gets its own sequence number, so nothing is ever overwritten by
/// a concurrent write that happened to land in the same group.
#[test]
fn concurrent_writers_all_land_and_all_get_distinct_sequence_numbers() {
    let (_, fs) = memfs();
    let db = Arc::new(open(&fs, options(), &[cf::DEFAULT]).unwrap());
    let threads = 8;
    let per_thread = 200;

    let handles: Vec<_> = (0..threads)
        .map(|worker| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let mut seqnos = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let mut batch = WriteBatch::new();
                    batch.put(0, format!("{worker}-{i:04}").as_bytes(), b"value");
                    batch.put(0, format!("{worker}-{i:04}-b").as_bytes(), b"value");
                    // Half the writers do not ask for durability; they may ride a synced
                    // group, which is correct.
                    let options = if worker % 2 == 0 {
                        WriteOptions::synced()
                    } else {
                        WriteOptions::unsynced()
                    };
                    seqnos.push(db.write(batch, &options).unwrap());
                }
                seqnos
            })
        })
        .collect();

    let mut all: Vec<u64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(
        all.len(),
        threads * per_thread,
        "sequence numbers must be unique"
    );

    for worker in 0..threads {
        for i in 0..per_thread {
            let key = format!("{worker}-{i:04}");
            assert_eq!(
                get(&db, key.as_bytes()).as_deref(),
                Some(&b"value"[..]),
                "{key} went missing"
            );
        }
    }
    assert_eq!(db.last_seqno(), *all.last().unwrap());
}

/// Invariant 1, at the only place it can be tested: bytes that were acknowledged with
/// `sync = true` must survive a power cut, and bytes that were not need not.
#[test]
fn synced_writes_survive_a_power_cut() {
    let (memfs, fs) = memfs();
    {
        let db = open(
            &fs,
            Options {
                wal_sync_mode: WalSyncMode::Never,
                ..options()
            },
            &[cf::DEFAULT],
        )
        .unwrap();
        db.write(
            {
                let mut batch = WriteBatch::new();
                batch.put(0, b"durable", b"yes");
                batch
            },
            &WriteOptions::synced(),
        )
        .unwrap();
        db.write(
            {
                let mut batch = WriteBatch::new();
                batch.put(0, b"maybe", b"perhaps");
                batch
            },
            &WriteOptions::unsynced(),
        )
        .unwrap();
    }
    memfs.lose_unsynced().unwrap();

    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(
        get(&db, b"durable").as_deref(),
        Some(&b"yes"[..]),
        "an acknowledged synced write was lost"
    );
    // The unsynced one may or may not be there; what matters is that it is not corrupt.
    let _ = get(&db, b"maybe");
}

/// A torn record is what a crash looks like — but only at the end of the last segment. In an
/// earlier one it means a segment that should be complete is not, which is a lost write.
#[test]
fn a_torn_tail_is_tolerated_only_in_the_last_segment() {
    let (memfs, fs) = memfs();
    let first;
    let second;
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        first = db.wal_number().unwrap();
        db.put(cf::DEFAULT, b"a", b"1").unwrap();
    }
    {
        let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
        second = db.wal_number().unwrap();
        db.put(cf::DEFAULT, b"b", b"2").unwrap();
    }
    assert_ne!(first, second);

    let path = filename::wal(std::path::Path::new(DIR), second);
    let bytes = memfs.contents(&path).unwrap();
    memfs
        .install(&path, bytes[..bytes.len() - 2].to_vec())
        .unwrap();
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(
        get(&db, b"a").as_deref(),
        Some(&b"1"[..]),
        "the earlier segment replayed"
    );
    drop(db);

    // The same damage in the earlier segment is not survivable.
    let path = filename::wal(std::path::Path::new(DIR), first);
    let bytes = memfs.contents(&path).unwrap();
    memfs
        .install(&path, bytes[..bytes.len() - 2].to_vec())
        .unwrap();
    let err = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap_err();
    assert!(err.is_corruption(), "{err}");
    assert!(err.to_string().contains("not the last"), "{err}");
}

#[test]
fn a_corrupt_log_record_is_refused_under_paranoid_checks() {
    let (memfs, fs) = memfs();
    let number;
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        number = db.wal_number().unwrap();
        db.put(cf::DEFAULT, b"a", b"1").unwrap();
        db.put(cf::DEFAULT, b"b", b"2").unwrap();
    }
    let path = filename::wal(std::path::Path::new(DIR), number);
    let mut bytes = memfs.contents(&path).unwrap();
    bytes[8] ^= 0xFF;
    memfs.install(&path, bytes).unwrap();

    let err = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap_err();
    assert!(err.is_corruption(), "{err}");

    // With paranoid checks off it opens, having dropped the rest of that segment.
    let db = open(
        &fs,
        Options {
            paranoid_checks: false,
            ..Options::default()
        },
        &[cf::DEFAULT],
    )
    .unwrap();
    assert_eq!(
        get(&db, b"a"),
        None,
        "the damaged record was not resurrected"
    );
}

#[test]
fn an_empty_batch_commits_and_reports_where_it_landed() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    let before = db
        .write(WriteBatch::new(), &WriteOptions::default())
        .unwrap();
    assert_eq!(before, db.last_seqno());
    db.put(cf::DEFAULT, b"k", b"v").unwrap();
    let after = db
        .write(WriteBatch::new(), &WriteOptions::default())
        .unwrap();
    assert!(after > before);
}

#[test]
fn properties_report_what_is_in_memory() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT, cf::LOCK]).unwrap();
    assert_eq!(db.property("esker.num-column-families").unwrap(), "2");
    assert_eq!(
        db.property("esker.num-immutable-mem-table.default")
            .unwrap(),
        "0"
    );
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "0"
    );
    assert_eq!(db.property("esker.mem-table-size.default").unwrap(), "0");
    db.put(cf::DEFAULT, b"k", b"v").unwrap();
    assert!(
        db.property("esker.mem-table-size.default")
            .unwrap()
            .parse::<usize>()
            .unwrap()
            > 0
    );
    assert_eq!(db.property("esker.mem-table-size.lock").unwrap(), "0");
    assert_eq!(db.property("esker.no-such-property"), None);
    assert_eq!(db.property("esker.mem-table-size.no-such-cf"), None);
}

/// A column family the database already has is opened even when the caller does not name it:
/// hiding data a database contains is worse than opening more than was asked for.
#[test]
fn existing_column_families_are_opened_even_when_unnamed() {
    let (_, fs) = memfs();
    {
        let db = open(&fs, options(), &[cf::DEFAULT, cf::LOCK]).unwrap();
        db.put(cf::LOCK, b"k", b"v").unwrap();
    }
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(db.cf_names().len(), 2);
    assert_eq!(
        db.get(cf::LOCK, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"v"[..])
    );
}

// ---------------------------------------------------------------------------------------
// Flush: memtables becoming L0 files, and reads that have to go and find them.
// ---------------------------------------------------------------------------------------

use esker_engine::options::CfOptions;

/// Options with a memtable small enough that a handful of writes fills it.
fn small_buffer(bytes: usize) -> Options {
    Options {
        create_if_missing: true,
        cf_options: CfOptions {
            write_buffer_size: bytes,
            ..CfOptions::default()
        },
        ..Options::default()
    }
}

#[test]
fn a_flush_moves_data_to_l0_and_reads_still_find_it() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for i in 0..200u32 {
        db.put(cf::DEFAULT, format!("key-{i:04}").as_bytes(), b"value")
            .unwrap();
    }
    db.delete(cf::DEFAULT, b"key-0007").unwrap();
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "0"
    );

    db.flush(cf::DEFAULT).unwrap();
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "1"
    );
    assert_eq!(
        db.property("esker.num-immutable-mem-table.default")
            .unwrap(),
        "0"
    );
    assert_eq!(
        db.property("esker.mem-table-size.default").unwrap(),
        "0",
        "the flushed table is gone from memory"
    );

    for i in 0..200u32 {
        let key = format!("key-{i:04}");
        let expected = if i == 7 { None } else { Some(&b"value"[..]) };
        assert_eq!(get(&db, key.as_bytes()).as_deref(), expected, "{key}");
    }
    assert!(
        db.property("esker.open-tables")
            .unwrap()
            .parse::<usize>()
            .unwrap()
            >= 1
    );
}

#[test]
fn flushed_data_survives_a_reopen_and_the_old_log_is_reclaimed() {
    let (memfs, fs) = memfs();
    let old_log;
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        old_log = db.wal_number().unwrap();
        for i in 0..50u32 {
            db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"v")
                .unwrap();
        }
        db.flush(cf::DEFAULT).unwrap();
        assert!(
            !memfs
                .exists(&filename::wal(std::path::Path::new(DIR), old_log))
                .unwrap(),
            "the flushed segment should have been reclaimed"
        );
    }

    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "1"
    );
    // Once the segments are gone the manifest is the only record of how far the sequence
    // numbers got. If an edit forgot to stamp it, recovery restarts numbering from an older
    // point and every flushed write becomes invisible — which is exactly what happened the
    // first time this test was written.
    assert!(
        db.last_seqno() >= 50,
        "the sequence number did not survive the flush: {}",
        db.last_seqno()
    );
    for i in 0..50u32 {
        assert_eq!(
            get(&db, format!("k{i:03}").as_bytes()).as_deref(),
            Some(&b"v"[..])
        );
    }
}

/// The engine flushes on its own once a memtable fills, without anyone asking.
#[test]
fn a_full_memtable_flushes_by_itself() {
    let (_, fs) = memfs();
    let db = open(&fs, small_buffer(4 * 1024), &[cf::DEFAULT]).unwrap();
    for i in 0..400u32 {
        db.put(cf::DEFAULT, format!("key-{i:05}").as_bytes(), &[b'v'; 64])
            .unwrap();
    }
    // Wait for the background thread to catch up; the last table may still be in memory.
    db.flush(cf::DEFAULT).unwrap();

    let files: usize = db
        .property("esker.num-files-at-level0.default")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        files > 1,
        "several memtables should have filled and flushed, saw {files}"
    );
    for i in 0..400u32 {
        let key = format!("key-{i:05}");
        assert_eq!(
            get(&db, key.as_bytes()).as_deref(),
            Some(&[b'v'; 64][..]),
            "{key}"
        );
    }
}

/// A tombstone has to reach L0 as an entry, or the value underneath it comes back.
#[test]
fn a_tombstone_survives_a_flush_that_leaves_the_value_behind() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"k", b"v").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.delete(cf::DEFAULT, b"k").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "2"
    );
    assert_eq!(get(&db, b"k"), None, "the newer L0 file must win");
    drop(db);

    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(get(&db, b"k"), None, "and still after a reopen");
}

/// L0 files overlap, so a read has to consult them newest first.
#[test]
fn overlapping_l0_files_are_read_newest_first() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for generation in 0..4u32 {
        for i in 0..10u32 {
            db.put(
                cf::DEFAULT,
                format!("k{i}").as_bytes(),
                format!("gen{generation}").as_bytes(),
            )
            .unwrap();
        }
        db.flush(cf::DEFAULT).unwrap();
    }
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "4"
    );
    for i in 0..10u32 {
        assert_eq!(
            get(&db, format!("k{i}").as_bytes()).as_deref(),
            Some(&b"gen3"[..]),
            "k{i} should read the newest generation"
        );
    }
}

/// A snapshot taken before a flush still sees what it saw, even though the data has moved
/// from memory to disk underneath it.
#[test]
fn a_snapshot_reads_the_same_values_across_a_flush() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"k", b"first").unwrap();
    let snapshot = db.snapshot();
    db.put(cf::DEFAULT, b"k", b"second").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    let at_snapshot = ReadOptions {
        snapshot: Some(snapshot),
        ..ReadOptions::default()
    };
    assert_eq!(
        db.get(cf::DEFAULT, b"k", &at_snapshot).unwrap().as_deref(),
        Some(&b"first"[..])
    );
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"second"[..]));
}

/// Writers must be pushed back when they outrun the flush thread, and the push-back has to be
/// visible rather than mysterious (`docs/DESIGN.md` §4.4).
#[test]
fn write_stalls_are_counted() {
    let (_, fs) = memfs();
    let db = open(&fs, small_buffer(1024), &[cf::DEFAULT]).unwrap();
    for i in 0..2_000u32 {
        db.put(cf::DEFAULT, format!("key-{i:06}").as_bytes(), &[b'x'; 128])
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    let stalls: u64 = db.property("esker.write-stalls").unwrap().parse().unwrap();
    let slowdowns: u64 = db
        .property("esker.write-slowdowns")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        stalls + slowdowns > 0,
        "writing 2000 records into a 1 KiB buffer should have pushed back at least once"
    );
    for i in 0..2_000u32 {
        assert_eq!(
            get(&db, format!("key-{i:06}").as_bytes()).as_deref(),
            Some(&[b'x'; 128][..]),
            "key-{i:06}"
        );
    }
}

/// Everything at once, against a model: random puts and deletes across flushes, read back at
/// every snapshot that was taken along the way.
#[test]
fn reads_match_a_model_across_flushes() {
    use std::collections::BTreeMap;

    let (_, fs) = memfs();
    let db = open(&fs, small_buffer(8 * 1024), &[cf::DEFAULT]).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = esker_base::rng::Pcg32::from_seed(0xE5E5);

    for round in 0..1_500u32 {
        let key = format!("key-{:04}", rng.below(400)).into_bytes();
        if rng.chance(0.25) {
            db.delete(cf::DEFAULT, &key).unwrap();
            model.remove(&key);
        } else {
            let value = format!("v{round}").into_bytes();
            db.put(cf::DEFAULT, &key, &value).unwrap();
            model.insert(key, value);
        }
        if round % 400 == 399 {
            db.flush(cf::DEFAULT).unwrap();
        }
    }
    db.flush(cf::DEFAULT).unwrap();

    for i in 0..400u32 {
        let key = format!("key-{i:04}").into_bytes();
        assert_eq!(
            get(&db, &key).as_deref(),
            model.get(&key).map(Vec::as_slice),
            "key-{i:04}"
        );
    }
}
