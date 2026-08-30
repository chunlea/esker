//! Checkpoints and ingest: copying a database without moving its bytes, and adopting files
//! built somewhere else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_engine::filename;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions};
use esker_engine::{CheckpointRange, Db, cf};

const DIR: &str = "/db";
const COPY: &str = "/copy";

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

fn memfs() -> (Arc<MemFileSystem>, Arc<dyn FileSystem>) {
    let inner = Arc::new(MemFileSystem::new());
    let dynamic: Arc<dyn FileSystem> = inner.clone();
    (inner, dynamic)
}

fn open(fs: &Arc<dyn FileSystem>, dir: &str, options: Options, cfs: &[&str]) -> Db {
    Db::open_with(dir, options, Arc::clone(fs), cfs).unwrap()
}

fn get(db: &Db, cf_name: &str, key: &[u8]) -> Option<Vec<u8>> {
    db.get(cf_name, key, &ReadOptions::default())
        .unwrap()
        .map(|value| value.to_vec())
}

fn scan(db: &Db, cf_name: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf_name, &ReadOptions::default()).unwrap();
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    iter.status().unwrap();
    out
}

#[test]
fn a_checkpoint_opens_as_its_own_database() {
    let (_, fs) = memfs();
    let db = open(&fs, DIR, options(), &[cf::DEFAULT, cf::LOCK]);
    for i in 0..100u32 {
        db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"data")
            .unwrap();
    }
    db.put(cf::LOCK, b"lock", b"held").unwrap();
    db.delete(cf::DEFAULT, b"k005").unwrap();

    db.checkpoint(COPY, None).unwrap();

    let copy = open(&fs, COPY, Options::default(), &[cf::DEFAULT, cf::LOCK]);
    assert_eq!(scan(&copy, cf::DEFAULT).len(), 99);
    assert_eq!(
        get(&copy, cf::DEFAULT, b"k000").as_deref(),
        Some(&b"data"[..])
    );
    assert_eq!(
        get(&copy, cf::DEFAULT, b"k005"),
        None,
        "the delete came across"
    );
    assert_eq!(get(&copy, cf::LOCK, b"lock").as_deref(), Some(&b"held"[..]));
    assert!(copy.last_seqno() >= db.last_seqno());
}

/// A checkpoint is a copy, not a view: what happens to the source afterwards is not its
/// business, and compaction deleting the source's files must not empty it.
#[test]
fn a_checkpoint_is_unaffected_by_later_writes_and_compactions() {
    let (_, fs) = memfs();
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    for i in 0..50u32 {
        db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"before")
            .unwrap();
    }
    db.checkpoint(COPY, None).unwrap();

    for i in 0..50u32 {
        db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"after")
            .unwrap();
    }
    db.put(cf::DEFAULT, b"extra", b"after").unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    let copy = open(&fs, COPY, Options::default(), &[cf::DEFAULT]);
    assert_eq!(
        scan(&copy, cf::DEFAULT).len(),
        50,
        "no key the source gained"
    );
    for i in 0..50u32 {
        assert_eq!(
            get(&copy, cf::DEFAULT, format!("k{i:03}").as_bytes()).as_deref(),
            Some(&b"before"[..])
        );
    }
    assert_eq!(get(&copy, cf::DEFAULT, b"extra"), None);
}

/// Hard links, so a checkpoint of a large database costs a manifest and nothing else.
#[test]
fn a_checkpoint_links_rather_than_copies() {
    let (memfs, fs) = memfs();
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    for i in 0..200u32 {
        db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), &[b'v'; 64])
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    db.checkpoint(COPY, None).unwrap();

    let source_files: Vec<PathBuf> = memfs
        .list(Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter(|p| matches!(filename::classify_path(p), Some(filename::FileKind::Sst(_))))
        .collect();
    assert!(!source_files.is_empty());
    for path in &source_files {
        let Some(filename::FileKind::Sst(number)) = filename::classify_path(path) else {
            continue;
        };
        let linked = filename::sst(Path::new(COPY), number);
        assert!(
            memfs.exists(&linked).unwrap(),
            "{} was not linked",
            linked.display()
        );
        assert_eq!(memfs.size(&linked).unwrap(), memfs.size(path).unwrap());
    }
    // Deleting the source's copy leaves the checkpoint's readable, which is what a link means.
    memfs.delete(&source_files[0]).unwrap();
    let copy = open(&fs, COPY, Options::default(), &[cf::DEFAULT]);
    assert_eq!(scan(&copy, cf::DEFAULT).len(), 200);
}

#[test]
fn a_checkpoint_can_take_one_column_family_and_one_range() {
    let (_, fs) = memfs();
    let db = open(&fs, DIR, options(), &[cf::DEFAULT, cf::LOCK]);
    for i in 0..30u32 {
        db.put(cf::DEFAULT, format!("k{i:02}").as_bytes(), b"v")
            .unwrap();
    }
    db.put(cf::LOCK, b"elsewhere", b"v").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    db.checkpoint(
        COPY,
        Some(CheckpointRange {
            cf: cf::DEFAULT,
            begin: Some(b"k00"),
            end: Some(b"k29"),
        }),
    )
    .unwrap();

    let copy = open(&fs, COPY, Options::default(), &[cf::DEFAULT]);
    assert_eq!(
        copy.cf_names(),
        vec![cf::DEFAULT.to_string()],
        "only the named family"
    );
    assert_eq!(scan(&copy, cf::DEFAULT).len(), 30);
    assert!(
        copy.get(cf::LOCK, b"elsewhere", &ReadOptions::default())
            .is_err()
    );
}

#[test]
fn a_checkpoint_refuses_to_overwrite_a_database() {
    let (_, fs) = memfs();
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    db.put(cf::DEFAULT, b"k", b"v").unwrap();
    db.checkpoint(COPY, None).unwrap();
    assert!(
        db.checkpoint(COPY, None).is_err(),
        "twice into the same directory"
    );
}

// ---------------------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------------------

/// Builds a one-file database and returns the path of its only SST, to be ingested elsewhere.
fn build_source(fs: &Arc<dyn FileSystem>, dir: &str, keys: std::ops::Range<u32>) -> PathBuf {
    let db = open(fs, dir, options(), &[cf::DEFAULT]);
    for i in keys {
        db.put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"ingested")
            .unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    drop(db);

    let mut ssts: Vec<PathBuf> = fs
        .list(Path::new(dir))
        .unwrap()
        .into_iter()
        .filter(|p| matches!(filename::classify_path(p), Some(filename::FileKind::Sst(_))))
        .collect();
    assert_eq!(ssts.len(), 1, "the source should be one file");
    ssts.pop().unwrap()
}

#[test]
fn an_ingested_file_reads_like_anything_else() {
    let (_, fs) = memfs();
    let source = build_source(&fs, "/source", 0..40);

    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    db.put(cf::DEFAULT, b"zzz", b"local").unwrap();
    db.ingest(cf::DEFAULT, &[source]).unwrap();

    for i in 0..40u32 {
        assert_eq!(
            get(&db, cf::DEFAULT, format!("k{i:03}").as_bytes()).as_deref(),
            Some(&b"ingested"[..]),
            "k{i:03}"
        );
    }
    assert_eq!(
        get(&db, cf::DEFAULT, b"zzz").as_deref(),
        Some(&b"local"[..])
    );
    assert_eq!(scan(&db, cf::DEFAULT).len(), 41);

    // A write after the ingest must win, which is what raising the sequence number is for.
    db.put(cf::DEFAULT, b"k000", b"newer").unwrap();
    assert_eq!(
        get(&db, cf::DEFAULT, b"k000").as_deref(),
        Some(&b"newer"[..])
    );
    drop(db);

    let db = open(&fs, DIR, Options::default(), &[cf::DEFAULT]);
    assert_eq!(
        get(&db, cf::DEFAULT, b"k000").as_deref(),
        Some(&b"newer"[..])
    );
    assert_eq!(scan(&db, cf::DEFAULT).len(), 41, "and it survives a reopen");
}

/// A bulk load should not pile up at L0 and then compact itself straight back down.
#[test]
fn an_ingested_file_sinks_to_the_deepest_level_it_fits() {
    let (_, fs) = memfs();
    let source = build_source(&fs, "/source", 0..40);
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    db.ingest(cf::DEFAULT, &[source]).unwrap();

    assert_eq!(
        db.property("esker.num-files-at-level0.default").unwrap(),
        "0"
    );
    let bottom = db
        .property("esker.num-files-at-level6.default")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(
        bottom, 1,
        "nothing is in its way, so it goes all the way down"
    );
}

/// The v1 limitation, refused rather than silently reordered: a file built elsewhere carries
/// another database's sequence numbers, so an overlap has no defensible answer.
#[test]
fn ingesting_over_existing_keys_is_refused() {
    let (_, fs) = memfs();
    let source = build_source(&fs, "/source", 0..40);

    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    db.put(cf::DEFAULT, b"k010", b"already here").unwrap();
    let err = db.ingest(cf::DEFAULT, &[source]).unwrap_err();
    assert!(matches!(err, esker_engine::Error::Unsupported(_)), "{err}");
    assert!(err.to_string().contains("overlapping ingest"), "{err}");
    // And nothing changed: the refusal is complete.
    assert_eq!(
        get(&db, cf::DEFAULT, b"k010").as_deref(),
        Some(&b"already here"[..])
    );
    assert_eq!(get(&db, cf::DEFAULT, b"k000"), None);
}

#[test]
fn two_ingested_files_that_overlap_each_other_are_refused() {
    let (_, fs) = memfs();
    let first = build_source(&fs, "/source-a", 0..40);
    let second = build_source(&fs, "/source-b", 20..60);
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    let err = db.ingest(cf::DEFAULT, &[first, second]).unwrap_err();
    assert!(err.to_string().contains("same keys"), "{err}");
}

#[test]
fn disjoint_files_ingest_together() {
    let (_, fs) = memfs();
    let first = build_source(&fs, "/source-a", 0..20);
    let second = build_source(&fs, "/source-b", 40..60);
    let db = open(&fs, DIR, options(), &[cf::DEFAULT]);
    db.ingest(cf::DEFAULT, &[first, second]).unwrap();
    assert_eq!(scan(&db, cf::DEFAULT).len(), 40);
    assert_eq!(
        db.ingest(cf::DEFAULT, &[]).ok(),
        Some(()),
        "nothing to do is not an error"
    );
}

/// The pair the phase-4 snapshot path will use: checkpoint a range on one database, ingest it
/// into another.
#[test]
fn a_checkpoints_files_can_be_ingested_by_another_database() {
    let (_, fs) = memfs();
    let source = open(&fs, "/source", options(), &[cf::DEFAULT]);
    for i in 0..60u32 {
        source
            .put(cf::DEFAULT, format!("k{i:03}").as_bytes(), b"shipped")
            .unwrap();
    }
    source
        .checkpoint(
            COPY,
            Some(CheckpointRange {
                cf: cf::DEFAULT,
                begin: None,
                end: None,
            }),
        )
        .unwrap();
    drop(source);

    let shipped: Vec<PathBuf> = fs
        .list(Path::new(COPY))
        .unwrap()
        .into_iter()
        .filter(|p| matches!(filename::classify_path(p), Some(filename::FileKind::Sst(_))))
        .collect();
    assert!(!shipped.is_empty());

    let target = open(&fs, DIR, options(), &[cf::DEFAULT]);
    target.ingest(cf::DEFAULT, &shipped).unwrap();
    for i in 0..60u32 {
        assert_eq!(
            get(&target, cf::DEFAULT, format!("k{i:03}").as_bytes()).as_deref(),
            Some(&b"shipped"[..])
        );
    }
}
