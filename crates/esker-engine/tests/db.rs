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

/// Every SST in the database directory, by file number.
///
/// The tables and nothing else: a WAL segment, the manifest and `CURRENT` come and go for reasons
/// that have nothing to do with which column families exist, so a claim about reclamation that
/// counted them would be answered by the wrong files.
fn ssts(memfs: &Arc<MemFileSystem>) -> std::collections::BTreeSet<u64> {
    memfs
        .list(std::path::Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter_map(|path| match filename::classify_path(&path) {
            Some(filename::FileKind::Sst(number)) => Some(number),
            _ => None,
        })
        .collect()
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
    // Four L0 files is exactly the default compaction trigger, so the background pool is
    // entitled to merge them away before the count below is read — and on a loaded machine it
    // does. What this test is about is the order a read consults overlapping L0 files in, not
    // when compaction fires, so the trigger goes out of reach and the files stay put.
    let mut options = options();
    options.cf_options.level0_file_num_compaction_trigger = 100;
    let db = open(&fs, options, &[cf::DEFAULT]).unwrap();
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

// ---------------------------------------------------------------------------------------
// Iteration: many versions in, one entry per user key out.
// ---------------------------------------------------------------------------------------

use esker_engine::options::StripSuffix;

/// Every key and value the iterator yields, walking forward from the start.
fn scan(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    iter.status().unwrap();
    out
}

/// The same, walking backward from the end.
fn scan_back(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    let mut out = Vec::new();
    iter.seek_to_last();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    iter.status().unwrap();
    out
}

fn pairs(entries: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    entries
        .iter()
        .map(|(key, value)| (key.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

#[test]
fn iteration_yields_one_entry_per_key_newest_first() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"b", b"old").unwrap();
    db.put(cf::DEFAULT, b"a", b"a1").unwrap();
    db.put(cf::DEFAULT, b"b", b"new").unwrap();
    db.put(cf::DEFAULT, b"c", b"c1").unwrap();
    db.delete(cf::DEFAULT, b"c").unwrap();

    assert_eq!(scan(&db), pairs(&[("a", "a1"), ("b", "new")]));
    assert_eq!(scan_back(&db), pairs(&[("b", "new"), ("a", "a1")]));
}

/// The same, with the data spread across memtables and L0 files so the merge cursor is
/// actually merging.
#[test]
fn iteration_merges_memtables_and_l0_files() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"a", b"gen0").unwrap();
    db.put(cf::DEFAULT, b"c", b"gen0").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.put(cf::DEFAULT, b"b", b"gen1").unwrap();
    db.put(cf::DEFAULT, b"c", b"gen1").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.put(cf::DEFAULT, b"d", b"memtable").unwrap();
    db.delete(cf::DEFAULT, b"a").unwrap();

    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "2"
    );
    assert_eq!(
        scan(&db),
        pairs(&[("b", "gen1"), ("c", "gen1"), ("d", "memtable")]),
        "the newer file wins, and the tombstone hides the older value"
    );
    assert_eq!(
        scan_back(&db),
        pairs(&[("d", "memtable"), ("c", "gen1"), ("b", "gen1")])
    );
}

#[test]
fn seeking_lands_on_the_right_key_in_both_directions() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for key in ["a", "c", "e"] {
        db.put(cf::DEFAULT, key.as_bytes(), key.as_bytes()).unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();

    iter.seek(b"b");
    assert_eq!(iter.key(), b"c", "seek goes forward");
    iter.seek(b"c");
    assert_eq!(iter.key(), b"c", "an exact hit stays put");
    iter.seek(b"z");
    assert!(!iter.valid());

    iter.seek_for_prev(b"d");
    assert_eq!(iter.key(), b"c", "seek_for_prev goes backward");
    iter.seek_for_prev(b"c");
    assert_eq!(iter.key(), b"c");
    iter.seek_for_prev(b"");
    assert!(!iter.valid());
    iter.status().unwrap();
}

/// Turning around mid-scan must land on the neighbour, not repeat the current key or skip
/// one. This is the case the merge cursor's direction handling exists for.
#[test]
fn changing_direction_mid_scan_lands_on_the_neighbour() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for round in 0..3 {
        for key in ["a", "b", "c", "d"] {
            db.put(cf::DEFAULT, key.as_bytes(), format!("v{round}").as_bytes())
                .unwrap();
        }
        if round == 1 {
            db.flush(cf::DEFAULT).unwrap();
        }
    }

    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_to_first();
    assert_eq!(iter.key(), b"a");
    iter.next();
    iter.next();
    assert_eq!(iter.key(), b"c");
    iter.prev();
    assert_eq!(iter.key(), b"b", "one step back");
    iter.prev();
    assert_eq!(iter.key(), b"a");
    iter.next();
    assert_eq!(iter.key(), b"b", "and forward again");
    iter.next();
    assert_eq!(iter.key(), b"c");
    assert_eq!(iter.value(), b"v2", "still the newest version");
    iter.prev();
    iter.prev();
    assert_eq!(iter.key(), b"a");
    iter.prev();
    assert!(!iter.valid(), "off the front");
}

#[test]
fn an_iterator_reads_at_its_snapshot() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"a", b"first").unwrap();
    db.put(cf::DEFAULT, b"b", b"first").unwrap();
    let snapshot = db.snapshot();

    db.put(cf::DEFAULT, b"a", b"second").unwrap();
    db.delete(cf::DEFAULT, b"b").unwrap();
    db.put(cf::DEFAULT, b"c", b"new").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    let mut iter = db
        .iter(
            cf::DEFAULT,
            &ReadOptions {
                snapshot: Some(snapshot),
                ..ReadOptions::default()
            },
        )
        .unwrap();
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    assert_eq!(out, pairs(&[("a", "first"), ("b", "first")]));
    assert_eq!(scan(&db), pairs(&[("a", "second"), ("c", "new")]));
}

/// `prefix_same_as_start` stops the scan when the prefix changes rather than running to the
/// end of the key space (`docs/DESIGN.md` §4.9).
#[test]
fn prefix_same_as_start_stops_at_the_prefix_boundary() {
    let (_, fs) = memfs();
    let db = open(
        &fs,
        Options {
            create_if_missing: true,
            cf_options: CfOptions {
                // The MVCC shape of DESIGN §3: a user key with a fixed-length version suffix.
                prefix_extractor: Some(Arc::new(StripSuffix::new(4))),
                ..CfOptions::default()
            },
            ..Options::default()
        },
        &[cf::DEFAULT],
    )
    .unwrap();

    for user in ["aaaa", "bbbb", "cccc"] {
        for version in 0..3u32 {
            let mut key = user.as_bytes().to_vec();
            key.extend_from_slice(&version.to_be_bytes());
            db.put(cf::DEFAULT, &key, user.as_bytes()).unwrap();
        }
    }
    db.flush(cf::DEFAULT).unwrap();

    let mut iter = db
        .iter(
            cf::DEFAULT,
            &ReadOptions {
                prefix_same_as_start: true,
                ..ReadOptions::default()
            },
        )
        .unwrap();
    let mut start = b"bbbb".to_vec();
    start.extend_from_slice(&0u32.to_be_bytes());
    iter.seek(&start);

    let mut seen = 0;
    while iter.valid() {
        assert_eq!(&iter.key()[..4], b"bbbb", "the scan left its prefix");
        seen += 1;
        iter.next();
    }
    assert_eq!(seen, 3, "all three versions of bbbb and nothing else");
    iter.status().unwrap();

    // Without the option the same seek runs on into cccc.
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek(&start);
    let mut seen = 0;
    while iter.valid() {
        seen += 1;
        iter.next();
    }
    assert_eq!(seen, 6, "bbbb and cccc");
}

#[test]
fn an_empty_database_iterates_to_nothing() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    assert!(scan(&db).is_empty());
    assert!(scan_back(&db).is_empty());
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.next();
    assert!(!iter.valid(), "next on an invalid cursor is a no-op");
    iter.prev();
    assert!(!iter.valid());
}

/// Random writes across flushes, then the whole key space walked both ways and compared with a
/// `BTreeMap`.
#[test]
fn iteration_matches_a_model() {
    use std::collections::BTreeMap;

    let (_, fs) = memfs();
    let db = open(&fs, small_buffer(8 * 1024), &[cf::DEFAULT]).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = esker_base::rng::Pcg32::from_seed(0x17E5);

    for round in 0..1_200u32 {
        let key = format!("key-{:03}", rng.below(200)).into_bytes();
        if rng.chance(0.3) {
            db.delete(cf::DEFAULT, &key).unwrap();
            model.remove(&key);
        } else {
            let value = format!("v{round}").into_bytes();
            db.put(cf::DEFAULT, &key, &value).unwrap();
            model.insert(key, value);
        }
        if round % 300 == 299 {
            db.flush(cf::DEFAULT).unwrap();
        }
    }

    let expected: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(scan(&db), expected);
    let mut backwards = expected.clone();
    backwards.reverse();
    assert_eq!(scan_back(&db), backwards);
}

// ---------------------------------------------------------------------------------------
// Column families created and dropped while the database is open.
// ---------------------------------------------------------------------------------------

#[test]
fn a_column_family_created_at_runtime_survives_a_reopen() {
    let (_, fs) = memfs();
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        let id = db.create_cf("metrics", CfOptions::default()).unwrap();
        assert_eq!(db.cf_id("metrics"), Some(id));
        db.put("metrics", b"k", b"v").unwrap();
        db.put(cf::DEFAULT, b"k", b"default").unwrap();
        db.flush("metrics").unwrap();
    }

    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert!(db.cf_names().contains(&"metrics".to_string()));
    assert_eq!(
        db.get("metrics", b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"v"[..])
    );
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"default"[..]));
}

#[test]
fn dropping_a_column_family_takes_its_data_with_it() {
    let (memfs, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT, cf::LOCK]).unwrap();
    db.put(cf::LOCK, b"k", b"v").unwrap();
    db.put(cf::DEFAULT, b"keep", b"me").unwrap();
    db.flush(cf::LOCK).unwrap();

    let before = ssts(&memfs);
    db.drop_cf(cf::LOCK).unwrap();
    assert_eq!(db.cf_id(cf::LOCK), None);
    let read = db.get(cf::LOCK, b"k", &ReadOptions::default());
    assert!(
        read.is_err(),
        "a dropped family still answered a read: {read:?}"
    );
    // **The SSTs, not the directory.** Counting every file made this an assertion about the whole
    // directory: a WAL segment rolled or a manifest written between the two listings offsets the
    // reclaimed table and the count does not drop, which has nothing to do with what was reclaimed.
    // The claim is about the dropped family's tables, so it is made about those.
    let after = ssts(&memfs);
    assert!(
        after.is_subset(&before) && after.len() < before.len(),
        "the dropped family's tables should have been reclaimed and nothing else added\n  \
         before: {before:?}\n  after:  {after:?}"
    );
    assert_eq!(get(&db, b"keep").as_deref(), Some(&b"me"[..]));
    drop(db);

    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert!(
        !db.cf_names().contains(&cf::LOCK.to_string()),
        "a dropped family stays dropped"
    );
    assert_eq!(get(&db, b"keep").as_deref(), Some(&b"me"[..]));
}

#[test]
fn creating_a_column_family_twice_is_refused() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.create_cf("twice", CfOptions::default()).unwrap();
    let err = db.create_cf("twice", CfOptions::default()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    assert!(db.drop_cf("nothing-here").is_err());
}

/// The sequence number must survive an edit that rolls the manifest, whichever edit it is.
#[test]
fn a_column_family_edit_carries_the_sequence_number() {
    let (_, fs) = memfs();
    let seqno;
    {
        let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
        for i in 0..20u32 {
            db.put(cf::DEFAULT, format!("k{i}").as_bytes(), b"v")
                .unwrap();
        }
        db.flush(cf::DEFAULT).unwrap();
        seqno = db.last_seqno();
        db.create_cf("later", CfOptions::default()).unwrap();
    }
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert!(
        db.last_seqno() >= seqno,
        "the sequence number went backwards across a column-family edit"
    );
    for i in 0..20u32 {
        assert_eq!(
            get(&db, format!("k{i}").as_bytes()).as_deref(),
            Some(&b"v"[..])
        );
    }
}

// ---------------------------------------------------------------------------------------
// The line compaction will not be allowed to collect below.
// ---------------------------------------------------------------------------------------

/// With no snapshots outstanding the floor is the newest visible sequence number: every
/// version a reader could still ask for is one a fresh read would see. A live snapshot pulls
/// it back to itself and holds it there.
#[test]
fn the_compaction_floor_follows_the_oldest_live_snapshot() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    let floor = || -> u64 {
        db.property("esker.compaction-floor")
            .unwrap()
            .parse()
            .unwrap()
    };

    db.put(cf::DEFAULT, b"k", b"1").unwrap();
    assert_eq!(
        floor(),
        db.last_seqno(),
        "no snapshots: the floor is the present"
    );

    let old = db.snapshot();
    let pinned = old.seqno();
    db.put(cf::DEFAULT, b"k", b"2").unwrap();
    db.put(cf::DEFAULT, b"k", b"3").unwrap();
    assert_eq!(floor(), pinned, "a live snapshot holds the floor at itself");

    // A newer snapshot never moves the floor forward past an older one.
    let newer = db.snapshot();
    assert!(newer.seqno() > pinned);
    assert_eq!(floor(), pinned);

    drop(old);
    assert_eq!(
        floor(),
        newer.seqno(),
        "the floor rises as handles are released"
    );
    drop(newer);
    assert_eq!(floor(), db.last_seqno());
}

/// The other half of the same rule: a snapshot from a database that has been closed pins
/// nothing here, so it is refused rather than allowed to read below a floor it cannot move.
#[test]
fn a_snapshot_from_another_database_is_refused() {
    let (_, fs) = memfs();
    let foreign = {
        let other: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
        let db = open(&other, options(), &[cf::DEFAULT]).unwrap();
        db.put(cf::DEFAULT, b"k", b"elsewhere").unwrap();
        db.snapshot()
    };

    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    db.put(cf::DEFAULT, b"k", b"here").unwrap();
    let read_options = ReadOptions {
        snapshot: Some(foreign.clone()),
        ..ReadOptions::default()
    };
    assert!(matches!(
        db.get(cf::DEFAULT, b"k", &read_options),
        Err(Error::InvalidArgument(_))
    ));
    assert!(db.iter(cf::DEFAULT, &read_options).is_err());
    // And it does not move this database's floor, because it holds nothing in it.
    assert_eq!(
        db.property("esker.compaction-floor").unwrap(),
        db.last_seqno().to_string()
    );
}

// ---------------------------------------------------------------------------------------
// Compaction.
// ---------------------------------------------------------------------------------------

/// The whole point: data goes down the levels and is still all there.
#[test]
fn compaction_moves_data_down_and_keeps_every_key() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for generation in 0..4u32 {
        for i in 0..50u32 {
            db.put(
                cf::DEFAULT,
                format!("key-{i:03}").as_bytes(),
                format!("gen{generation}").as_bytes(),
            )
            .unwrap();
        }
        db.flush(cf::DEFAULT).unwrap();
    }
    db.delete(cf::DEFAULT, b"key-007").unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    assert!(db.compactions_run() > 0, "nothing was compacted");
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "0"
    );
    let deeper: usize = (1..7)
        .map(|level| {
            db.property(&format!("esker.num-files-at-level{level}.default"))
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .sum();
    assert!(deeper > 0, "the data has to have landed somewhere");

    for i in 0..50u32 {
        let key = format!("key-{i:03}");
        let expected = if i == 7 { None } else { Some(&b"gen3"[..]) };
        assert_eq!(get(&db, key.as_bytes()).as_deref(), expected, "{key}");
    }
    assert_eq!(
        scan(&db).len(),
        49,
        "the deleted key is gone from a scan too"
    );
}

/// **Regression.** A newer version of a key must never end up *below* an older one.
///
/// L0 files overlap, so an L0 compaction has to take every L0 file that overlaps its inputs —
/// and that set has to be closed under the overlap, not swept once. Three files, flushed
/// oldest first:
///
/// ```text
///   A = [k15, k15]   the older k15                     (oldest)
///   B = [k09, k20]   the newer k15
///   S = [k05, k09]   the seed                          (newest)
/// ```
///
/// `S` pulls in `B`, which widens the range from `[k05, k09]` to `[k05, k20]` — and only then
/// does `A` overlap. Sweep once and `A` is left at L0 holding k15's older value while the newer
/// one moves to L1; a point read consults every L0 file before it reaches L1, so the older
/// value wins and an acknowledged write is lost.
///
/// The range is bounded at `[k05, k05]` so the compaction is seeded by `S` alone, and the
/// trigger is set out of reach so the only compaction that runs is the one asked for here: the
/// bug is in which files are picked, not in when, and a test of it should not turn on the
/// background pool's timing.
#[test]
fn an_l0_compaction_never_leaves_an_older_version_above_a_newer_one() {
    let (_, fs) = memfs();
    let mut options = options();
    options.cf_options.level0_file_num_compaction_trigger = 100;
    let db = open(&fs, options, &[cf::DEFAULT]).unwrap();

    db.put(cf::DEFAULT, b"k15", b"old").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    db.put(cf::DEFAULT, b"k09", b"v0").unwrap();
    db.put(cf::DEFAULT, b"k15", b"new").unwrap();
    db.put(cf::DEFAULT, b"k20", b"v0").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    db.put(cf::DEFAULT, b"k05", b"v0").unwrap();
    db.put(cf::DEFAULT, b"k09", b"v1").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "3",
        "three L0 files, none of them compacted yet"
    );

    db.compact_range(cf::DEFAULT, Some(b"k05"), Some(b"k05"))
        .unwrap();

    assert_eq!(
        get(&db, b"k15").as_deref(),
        Some(&b"new"[..]),
        "the newer version of k15 was left below the older one"
    );
    for (key, value) in [
        (&b"k05"[..], &b"v0"[..]),
        (&b"k09"[..], &b"v1"[..]),
        (&b"k20"[..], &b"v0"[..]),
    ] {
        assert_eq!(
            get(&db, key).as_deref(),
            Some(value),
            "{}",
            String::from_utf8_lossy(key)
        );
    }
}

/// A compaction may not collect what a snapshot can still read. This is the property the
/// floor exists for, checked end to end rather than in the picker's unit tests.
#[test]
fn a_snapshot_survives_a_compaction_that_would_otherwise_collect_it() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    for i in 0..30u32 {
        db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"first")
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    let snapshot = db.snapshot();

    for i in 0..30u32 {
        db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"second")
            .unwrap();
    }
    db.delete(cf::DEFAULT, b"k05").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    let at_snapshot = ReadOptions {
        snapshot: Some(snapshot.clone()),
        ..ReadOptions::default()
    };
    for i in 0..30u32 {
        let key = format!("k{i:02}");
        assert_eq!(
            db.get(cf::DEFAULT, key.as_bytes(), &at_snapshot)
                .unwrap()
                .as_deref(),
            Some(&b"first"[..]),
            "{key} at the snapshot"
        );
    }
    assert_eq!(
        db.get(cf::DEFAULT, b"k05", &at_snapshot)
            .unwrap()
            .as_deref(),
        Some(&b"first"[..]),
        "the snapshot predates the delete"
    );
    assert_eq!(get(&db, b"k05"), None, "and the delete is visible now");

    // Once nothing pins it, a second compaction is free to collect the old versions.
    drop(snapshot);
    for i in 0..30u32 {
        db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"third")
            .unwrap();
    }
    db.compact_range(cf::DEFAULT, None, None).unwrap();
    for i in 0..30u32 {
        assert_eq!(
            get(&db, format!("k{i:02}").as_bytes()).as_deref(),
            Some(&b"third"[..])
        );
    }
}

/// A compacted database has to reopen as itself.
#[test]
fn a_compacted_database_reopens_unchanged() {
    let (_, fs) = memfs();
    let before;
    {
        let db = open(&fs, small_buffer(4 * 1024), &[cf::DEFAULT]).unwrap();
        for i in 0..300u32 {
            db.put(cf::DEFAULT, format!("key-{i:04}").as_bytes(), &[b'v'; 32])
                .unwrap();
        }
        db.compact_range(cf::DEFAULT, None, None).unwrap();
        before = scan(&db);
        assert_eq!(before.len(), 300);
    }
    let db = open(&fs, Options::default(), &[cf::DEFAULT]).unwrap();
    assert_eq!(scan(&db), before);
}

/// Writing enough to fill several memtables should make the background pool compact without
/// anyone asking, and everything must still be readable while it does.
#[test]
fn the_background_pool_compacts_by_itself() {
    let (_, fs) = memfs();
    let db = open(&fs, small_buffer(2 * 1024), &[cf::DEFAULT]).unwrap();
    for round in 0..6u32 {
        for i in 0..200u32 {
            db.put(
                cf::DEFAULT,
                format!("key-{i:04}").as_bytes(),
                format!("round{round}").as_bytes(),
            )
            .unwrap();
        }
    }
    db.flush(cf::DEFAULT).unwrap();
    // Give the pool a moment; then finish the job synchronously so the test is not a race.
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    // Each of these says what it saw. This test has failed in a full parallel run and never
    // alone, and "assertion failed" told nobody which of the three it was.
    assert!(
        db.compactions_run() > 0,
        "no compaction ran at all: compactions={}, running={:?}",
        db.compactions_run(),
        db.property("esker.compactions-running")
    );
    // **`esker.compactions-running == 0` was asserted here and was never the API's to promise.**
    // Two things were wrong with it. `compact_range` waits for its own work and not for the
    // pool's, which schedules compactions whenever the levels warrant one — so under load the
    // count is routinely non-zero the instant it returns, and it was red in a gate at load ~10
    // for exactly that reason. And the property counts *reserved input files* rather than
    // compactions, so the `running=5` in that failure was very likely one job over five files,
    // not five jobs; the message it printed said "a compaction was still running", which the
    // number could not support either way.
    //
    // What `compact_range` does promise is that this range is compacted, and the two assertions
    // that bracket this comment are that promise: something ran, and every key reads back at its
    // newest version. Both hold under contention.
    for i in 0..200u32 {
        assert_eq!(
            get(&db, format!("key-{i:04}").as_bytes()).as_deref(),
            Some(&b"round5"[..]),
            "key-{i:04} after {} compactions, running={:?}",
            db.compactions_run(),
            db.property("esker.compactions-running")
        );
    }
}

/// The filter is how `esker-txn` will collect MVCC versions below PD's safepoint.
#[test]
fn a_compaction_filter_drops_what_it_refuses() {
    use esker_engine::compaction::{CompactionFilter, FilterDecision};

    #[derive(Debug)]
    struct DropOddKeys;
    // The trait returns `&str` because a real filter may build its name from its
    // configuration; a test one is a literal, which clippy would rather see as `'static`.
    #[allow(clippy::unnecessary_literal_bound)]
    impl CompactionFilter for DropOddKeys {
        fn filter(&self, _level: usize, user_key: &[u8], _value: &[u8]) -> FilterDecision {
            match user_key.last() {
                Some(byte) if (byte - b'0') % 2 == 1 => FilterDecision::Remove,
                _ => FilterDecision::Keep,
            }
        }
        fn name(&self) -> &str {
            "test.DropOddKeys"
        }
    }

    let (_, fs) = memfs();
    let db = open(
        &fs,
        Options {
            create_if_missing: true,
            cf_options: CfOptions {
                compaction_filter: Some(Arc::new(DropOddKeys)),
                ..CfOptions::default()
            },
            ..Options::default()
        },
        &[cf::DEFAULT],
    )
    .unwrap();

    for i in 0..20u32 {
        db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"v")
            .unwrap();
    }
    assert_eq!(
        scan(&db).len(),
        20,
        "the filter runs during compaction, not on write"
    );

    db.compact_range(cf::DEFAULT, None, None).unwrap();
    let survivors: Vec<String> = scan(&db)
        .into_iter()
        .map(|(key, _)| String::from_utf8_lossy(&key).into_owned())
        .collect();
    assert_eq!(
        survivors.len(),
        10,
        "half the keys end in an odd digit: {survivors:?}"
    );
    assert!(
        survivors
            .iter()
            .all(|key| key.ends_with(['0', '2', '4', '6', '8']))
    );
}

/// Compacting a slice of the key space leaves the rest where it was, and an inverted range is
/// a caller's mistake rather than a silent no-op over whatever spans the gap.
#[test]
fn a_bounded_compaction_touches_only_its_range() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    // Three L0 files with disjoint ranges, so a bounded compaction can pick exactly one.
    for (low, high) in [(0u32, 9u32), (10, 19), (20, 29)] {
        for i in low..=high {
            db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"v")
                .unwrap();
        }
        db.flush(cf::DEFAULT).unwrap();
    }
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "3"
    );

    db.compact_range(cf::DEFAULT, Some(b"k10"), Some(b"k19"))
        .unwrap();
    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "2",
        "only the file covering the range should have moved"
    );

    for i in 0..30u32 {
        assert_eq!(
            get(&db, format!("k{i:02}").as_bytes()).as_deref(),
            Some(&b"v"[..]),
            "k{i:02}"
        );
    }

    let err = db
        .compact_range(cf::DEFAULT, Some(b"k20"), Some(b"k10"))
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");

    // A range nothing overlaps is not an error; there is simply nothing to do.
    let before = db.compactions_run();
    db.compact_range(cf::DEFAULT, Some(b"zzz"), Some(b"zzzz"))
        .unwrap();
    assert_eq!(db.compactions_run(), before);
}

/// Bloom before disk (`docs/DESIGN.md` §4.9).
///
/// The absent keys here sit *inside* the key range of every file, so the range check cannot
/// rule them out and the filter is the only thing that can. Without it each of these reads
/// would open a table, walk its index and read a data block to learn nothing.
#[test]
fn a_point_read_consults_the_bloom_filter_before_opening_a_table() {
    let (_, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    // Even keys only, so the odd ones are absent but bracketed by the file's range.
    for i in (0..2_000u32).step_by(2) {
        db.put(cf::DEFAULT, format!("key-{i:05}").as_bytes(), b"v")
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    let skips_before: u64 = db.property("esker.bloom-skips").unwrap().parse().unwrap();
    let probes_before: u64 = db.property("esker.bloom-probes").unwrap().parse().unwrap();

    for i in (1..2_000u32).step_by(2) {
        assert_eq!(
            get(&db, format!("key-{i:05}").as_bytes()),
            None,
            "key-{i:05}"
        );
    }
    let skips: u64 = db
        .property("esker.bloom-skips")
        .unwrap()
        .parse::<u64>()
        .unwrap()
        - skips_before;
    let probes: u64 = db
        .property("esker.bloom-probes")
        .unwrap()
        .parse::<u64>()
        .unwrap()
        - probes_before;

    // 999, not 1000: `key-01999` is past the largest key stored, so the file's range rules it
    // out before the filter is ever needed. The cheap check runs first, which is the point.
    assert_eq!(
        skips + probes,
        999,
        "one table consulted per absent key inside the range"
    );
    // 10 bits/key is a ~1% false-positive rate, so almost every one of these should be a skip.
    assert!(
        skips >= 950,
        "the filter skipped only {skips} of 999 tables; it is not being consulted"
    );

    // And it never rules out a key that is there: a filter may only say "no".
    for i in (0..2_000u32).step_by(2) {
        assert_eq!(
            get(&db, format!("key-{i:05}").as_bytes()).as_deref(),
            Some(&b"v"[..]),
            "key-{i:05}"
        );
    }
}

/// A column family with the filter turned off still reads correctly — the filter is an
/// optimisation, and the read path must not depend on one being there.
#[test]
fn reads_are_correct_without_a_bloom_filter() {
    let (_, fs) = memfs();
    let db = open(
        &fs,
        Options {
            create_if_missing: true,
            cf_options: CfOptions {
                bloom_bits_per_key: 0,
                ..CfOptions::default()
            },
            ..Options::default()
        },
        &[cf::DEFAULT],
    )
    .unwrap();
    for i in (0..200u32).step_by(2) {
        db.put(cf::DEFAULT, format!("key-{i:04}").as_bytes(), b"v")
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();

    assert_eq!(
        db.property("esker.bloom-skips").unwrap(),
        "0",
        "nothing to skip with"
    );
    for i in 0..200u32 {
        let expected = if i % 2 == 0 { Some(&b"v"[..]) } else { None };
        assert_eq!(
            get(&db, format!("key-{i:04}").as_bytes()).as_deref(),
            expected
        );
    }
}

/// A `DeleteRange` deletes its whole range — the thing `docs/DESIGN.md` §4.7 refused to do
/// until [ADR 0017](../../docs/adr/0017-range-tombstones.md) made it real.
///
/// This test used to assert the refusal. It asserts the opposite now, and the two halves it
/// keeps from that version are the ones that mattered: that the range really is a *range*
/// rather than the single key at its start, and that a batch mixing a range delete with a put
/// is atomic either way. `tests/range_del.rs` holds the rest — the discharge, the invariant,
/// and the interaction with snapshots.
#[test]
fn delete_range_deletes_its_whole_range() {
    let (_memfs, fs) = memfs();
    let db = open(&fs, options(), &[cf::DEFAULT]).unwrap();
    let id = db.cf_id(cf::DEFAULT).unwrap();
    for key in [&b"a"[..], b"b", b"c", b"d"] {
        db.put(cf::DEFAULT, key, b"v").unwrap();
    }

    let mut batch = WriteBatch::new();
    batch.delete_range(id, b"b", b"d");
    db.write(batch, &WriteOptions::default())
        .expect("a DeleteRange was refused");

    // The whole range, not the key at its start — which is exactly what the old behaviour
    // would have done while claiming the range was gone.
    assert_eq!(
        get(&db, b"a").as_deref(),
        Some(&b"v"[..]),
        "below the range"
    );
    assert_eq!(get(&db, b"b"), None, "the start is inside");
    assert_eq!(get(&db, b"c"), None, "and so is the middle");
    assert_eq!(
        get(&db, b"d").as_deref(),
        Some(&b"v"[..]),
        "the end is exclusive"
    );

    // An empty or inverted range is a caller error rather than a no-op (ADR 0017 decision 4),
    // and a refused batch changes nothing — including the put that rode with it.
    let mut mixed = WriteBatch::new();
    mixed.put(id, b"new", b"v");
    mixed.delete_range(id, b"z", b"a");
    assert!(db.write(mixed, &WriteOptions::default()).is_err());
    assert_eq!(
        get(&db, b"new"),
        None,
        "a refused batch applied part of itself"
    );
}

/// `approximate_size` is what `esker-store` splits a region on. It has to grow with the bytes in
/// the range, ignore what is outside it, and survive a flush moving those bytes from a memtable
/// into a file — a number that only counted one of the two would report a region shrinking every
/// time it was flushed.
#[test]
fn approximate_size_counts_a_range_across_the_memtable_and_the_files() {
    let (_fs, dynamic) = memfs();
    let db = open(&dynamic, options(), &cf::BUILTIN).unwrap();

    assert_eq!(db.approximate_size(cf::DEFAULT, None, None).unwrap(), 0);

    // A *different* incompressible kilobyte per key, so the file half and the memtable half of
    // the answer are comparable. One repeated value would compress across the entries in a block
    // and the test would be measuring LZ4 rather than the accessor.
    let mut seed = 0x2026_0830_u32;
    let mut value = || -> Vec<u8> {
        (0..1024)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect()
    };
    let mut batch = WriteBatch::new();
    for n in 0..100u32 {
        batch.put(
            db.cf_id(cf::DEFAULT).unwrap(),
            format!("a{n:04}").as_bytes(),
            &value(),
        );
        batch.put(
            db.cf_id(cf::DEFAULT).unwrap(),
            format!("z{n:04}").as_bytes(),
            &value(),
        );
    }
    db.write(batch, &WriteOptions::default()).unwrap();

    let everything = db.approximate_size(cf::DEFAULT, None, None).unwrap();
    assert!(
        everything >= 200 * 1024,
        "200 KiB of values reported as {everything}"
    );

    // Half the keys are under `a` and half under `z`, so a bound between them halves the answer.
    let low = db.approximate_size(cf::DEFAULT, None, Some(b"b")).unwrap();
    let high = db.approximate_size(cf::DEFAULT, Some(b"b"), None).unwrap();
    for (name, half) in [("low", low), ("high", high)] {
        assert!(
            half > everything / 4 && half < everything,
            "the {name} half of {everything} came out as {half}"
        );
    }

    // A range holding nothing is zero while the data is still in a memtable, where the accessor
    // can count entries exactly.
    assert_eq!(
        db.approximate_size(cf::DEFAULT, Some(b"m"), Some(b"n"))
            .unwrap(),
        0
    );

    // And a flush moves the bytes from the memtable into a file without the answer collapsing.
    // It does not stay *equal*: a file's size is compressed and a memtable's is not, which the
    // accessor's documentation names. With incompressible values the two are within a factor.
    db.flush(cf::DEFAULT).unwrap();
    let flushed = db.approximate_size(cf::DEFAULT, None, None).unwrap();
    assert!(
        flushed >= 150 * 1024,
        "a flush lost the size: {everything} became {flushed}"
    );
    // A range beyond every file is still zero once the data is in files: `overlapping` answers
    // by the files' own bounds, so a range past them touches nothing.
    assert_eq!(
        db.approximate_size(cf::DEFAULT, Some(b"zz"), Some(b"zzz"))
            .unwrap(),
        0,
        "a range past the end of the data was charged for it"
    );

    // A range *inside* the one file that now holds everything is charged half of it — the
    // documented coarseness, asserted so that it is a decision rather than a surprise. It must be
    // less than the whole, which is what makes it worth doing at all.
    let straddling = db
        .approximate_size(cf::DEFAULT, Some(b"m"), Some(b"n"))
        .unwrap();
    assert!(
        straddling < flushed && straddling > 0,
        "a straddling range came out as {straddling} of {flushed}"
    );
}

/// **A legal caller must not be told its database is corrupt.**
///
/// Writing while `compact_range` runs is an ordinary supported workload, and it used to come back
/// with `column family 0 level 1: files 133 and 132 overlap` — once in twenty attempts with a
/// writer, and never once in twenty with the writer stopped.
///
/// Two `L0 → L1` plans picked different L0 files, which legitimately overlap each other; with L1
/// empty neither pulled in an L1 file, so their *input* sets were disjoint and both reservations
/// succeeded. Both then wrote into L1 over the same keys. `check_disjoint` refused the resulting
/// edit — so nothing reached disk and this was a failed operation rather than lost data — and the
/// error surfaced to the caller. Reserving the output range as well is what stops the second plan
/// starting ([ADR 0079](../../../docs/adr/0079-compaction-concurrency-reserves-the-output-range.md)).
///
/// **Sixty attempts, and the number is arithmetic rather than taste.** The defect appeared once
/// in twenty, so a twenty-attempt test would catch a regression about two times in three — it
/// would go green against the broken code a third of the time, which is a test that lies at a
/// rate. Sixty puts that at about nineteen in twenty, and the whole loop costs a few seconds
/// because the filesystem is in memory.
#[test]
fn a_writer_alongside_compact_range_never_makes_it_report_corruption() {
    // Sixty attempts, a bounded writer per attempt, and a wall-clock budget over the whole thing:
    // whichever runs out first ends the loop, and falling short of `LEAST` is a failure with a
    // reason rather than a test that quietly ran three attempts and called itself green.
    const ATTEMPTS: usize = 60;
    const LEAST: usize = 20;
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
    const WRITES: u32 = 2_000;

    let started = std::time::Instant::now();
    let mut failures = Vec::new();
    let mut ran = 0usize;
    for attempt in 0..ATTEMPTS {
        if started.elapsed() >= BUDGET {
            break;
        }
        let (_, fs) = memfs();
        let db = Arc::new(open(&fs, small_buffer(2 * 1024), &[cf::DEFAULT]).unwrap());
        // Six rounds over the same 200 keys, so L0 fills with files that overlap each other --
        // which is the shape that made two plans' inputs disjoint and their outputs not.
        for round in 0..6u32 {
            for i in 0..200u32 {
                db.put(
                    cf::DEFAULT,
                    format!("key-{i:04}").as_bytes(),
                    format!("round{round}").as_bytes(),
                )
                .unwrap();
            }
        }

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = Arc::clone(&db);
        let flag = Arc::clone(&stop);
        let hand = std::thread::spawn(move || {
            // **A fixed number of writes, not "until the compaction returns".** Written the other
            // way, an attempt's work is whatever the scheduler allows: a `compact_range` slowed by
            // a loaded box is given a bigger database to compact by a writer that is not slowed
            // with it, and the two chase each other. That is what made this test run past 300 s in
            // a full-workspace gate having taken 1.7 s in a crate-only run.
            for n in 0..WRITES {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let _unused = writer.put(
                    cf::DEFAULT,
                    format!("bg-{n:06}").as_bytes(),
                    b"x".repeat(256).as_slice(),
                );
            }
        });

        db.flush(cf::DEFAULT).unwrap();
        let outcome = db.compact_range(cf::DEFAULT, None, None);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        hand.join().unwrap();
        ran += 1;

        if let Err(why) = outcome {
            failures.push(format!("attempt {attempt}: {why}"));
        }
    }

    assert!(
        failures.is_empty(),
        "compact_range refused a legal concurrent workload {} times in {ran}: {failures:?}",
        failures.len(),
    );
    // **A slow box makes this red with a reason, never a gate with no end.** Below `LEAST` the
    // detection rate this test is sized for is not reached, so passing would be a claim it did not
    // earn: the defect showed once in twenty, and twenty attempts catch a regression about two
    // times in three.
    assert!(
        ran >= LEAST,
        "only {ran} of {ATTEMPTS} attempts fitted in {BUDGET:?}; this box is too slow for this \
         test to mean anything, and a test that cannot reach its own detection rate must say so \
         rather than pass"
    );
}

/// **The same contention, sustained, checked for what it actually wrote.**
///
/// The test above asks whether `compact_range` returns an error. This asks the harder question:
/// after a writer, the background pool and repeated `compact_range` calls have all been working
/// the same levels at once, is every key still readable at its newest value? A reservation rule
/// that serialised too little would corrupt a level; one that serialised the wrong thing could
/// drop an output and lose a key while returning `Ok`.
///
/// **The writer is bounded, and that is not a detail.** The first version of this test let it run
/// unthrottled for the whole loop, so every round had more to compact than the last and
/// `compact_range(None, None)` was chasing a database that grew faster than it drained: 456 other
/// tests finished while this one spun at 350% CPU past seventy seconds. Reserving the output range
/// makes that worse rather than better, because `L0 -> L1` is now one compaction at a time — which
/// is the throughput [ADR 0079](../../../docs/adr/0079-compaction-concurrency-reserves-the-output-range.md)
/// says the invariant costs, observed rather than predicted. A test whose work is unbounded cannot
/// tell that from a livelock, so the writer stops at a fixed count and the contention is what is
/// left.
#[test]
fn a_sustained_writer_and_repeated_compactions_lose_no_key() {
    const WRITES: u32 = 8_000;
    // The wall clock half of the cap below. Declared here with the other item, before any
    // statement: `clippy::items_after_statements` is denied in this workspace, so an item wedged
    // in beside the code it belongs to fails `--all-targets` while reading perfectly well.
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
    let (_, fs) = memfs();
    let db = Arc::new(open(&fs, small_buffer(2 * 1024), &[cf::DEFAULT]).unwrap());

    // The keys whose final value is asserted. Written first and rewritten by the loop below, so
    // they span every level by the time the compactions start moving them.
    for i in 0..300u32 {
        db.put(
            cf::DEFAULT,
            format!("key-{i:04}").as_bytes(),
            b"first".as_slice(),
        )
        .unwrap();
    }

    let compactions_before = db.compactions_run();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = Arc::clone(&db);
    let finished = Arc::clone(&done);
    let hand = std::thread::spawn(move || {
        // Fresh keys, so L0 keeps filling and the pool keeps finding work. Bounded, so the
        // database does not outgrow what the compactions can drain -- see the note above.
        for n in 0..WRITES {
            let _unused = writer.put(
                cf::DEFAULT,
                format!("bg-{n:06}").as_bytes(),
                b"y".repeat(256).as_slice(),
            );
        }
        finished.store(true, std::sync::atomic::Ordering::Release);
    });

    // **The loop ends when the writer does, and that is what makes this concurrent.** Stopping
    // the writer when the loop had had enough was the other way round, and it let the loop
    // finish first: the writer got 718 writes in before it was told to stop, so the compactions
    // it was supposed to contend with ran against a database nobody else was touching. The test
    // passed, in a tenth of a second, having tested nothing. `overlapping` counts the rounds
    // that really did run alongside the writer, and asserting on it is what stops that coming
    // back.
    let mut round = 0u32;
    let mut overlapping = 0u32;
    // **A round ceiling and a wall clock, whichever comes first.** The ceiling alone is a cap on
    // work, not on time: on a box where every round is slow, sixty of them is still unbounded from
    // a gate's point of view. Both, so a slow machine ends this test rather than extending it.
    let started = std::time::Instant::now();
    while round < 4
        || (!done.load(std::sync::atomic::Ordering::Acquire)
            && round < 60
            && started.elapsed() < BUDGET)
    {
        if !done.load(std::sync::atomic::Ordering::Acquire) {
            overlapping += 1;
        }
        for i in 0..300u32 {
            db.put(
                cf::DEFAULT,
                format!("key-{i:04}").as_bytes(),
                format!("round{round}").as_bytes(),
            )
            .unwrap();
        }
        db.compact_range(cf::DEFAULT, None, None)
            .unwrap_or_else(|why| panic!("round {round}: {why}"));
        round += 1;
    }
    let last = round - 1;
    hand.join().unwrap();

    assert!(
        overlapping >= 1,
        "no round ran while the writer was writing, so nothing here was concurrent with anything; \
         {round} rounds in {:?}",
        started.elapsed()
    );
    assert!(
        db.compactions_run() > compactions_before,
        "no compaction ran at all, so there was nothing to contend over"
    );

    let expected = format!("round{last}");
    let missing: Vec<u32> = (0..300u32)
        .filter(|i| {
            get(&db, format!("key-{i:04}").as_bytes()).as_deref() != Some(expected.as_bytes())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "{} keys did not read back their newest value after {round} rounds of concurrent \
         compaction ({overlapping} of them alongside the writer): {missing:?}",
        missing.len(),
    );
}
