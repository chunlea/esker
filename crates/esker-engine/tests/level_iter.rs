//! A level with N files scans exactly as the same keys do from one file.
//!
//! `db/level_iter.rs` replaced one cursor per file with one cursor per level, opening the file it
//! has reached. That is a pure performance change and the only thing that could go wrong with it
//! is a *correctness* one: a boundary between two files is a place a cursor can lose an entry,
//! return one twice, or stop early — and none of those is visible in a benchmark.
//!
//! So the property is a differential: build the same keys twice, once spread over many files at a
//! level and once in a single file, and require every scan to agree. Forward, backward, and seeks
//! that land inside a file, exactly on a file boundary, in the gap between two files, and outside
//! the level at both ends.
//!
//! # Why this is not covered by `tests/model.rs`
//!
//! The model test compares against a `BTreeMap` and would catch a lost key — but only if its
//! random sequence happened to produce a deep level with many files, which a 24-key space and a
//! default memtable do not. The shape has to be built on purpose.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{CfOptions, Options, ReadOptions};
use esker_engine::{Db, cf};

/// Keys are `key-NNNN`, so their byte order is their numeric order.
fn key(n: u32) -> Vec<u8> {
    format!("key-{n:04}").into_bytes()
}

fn value(n: u32) -> Vec<u8> {
    format!("value-{n}-{}", "x".repeat((n % 17) as usize)).into_bytes()
}

/// A database whose compaction outputs are `target` bytes each.
///
/// `target_file_size` is the knob that decides this, not the memtable: a compaction merges its
/// inputs and cuts a new output every `target_file_size` bytes, so 600 small keys land in one file
/// at the default and in a dozen at 4 KiB. The first version of this test turned `write_buffer_size`
/// down instead and got one file both ways — a differential comparing the level cursor with itself,
/// which the shape assertion below caught.
fn open(target: u64) -> (Arc<MemFileSystem>, Db) {
    let fs = Arc::new(MemFileSystem::new());
    let dynamic: Arc<dyn FileSystem> = fs.clone();
    let options = Options {
        create_if_missing: true,
        cf_options: CfOptions {
            write_buffer_size: 4 * 1024,
            target_file_size: target,
            ..CfOptions::default()
        },
        ..Options::default()
    };
    let db = Db::open_with("/db", options, dynamic, &[cf::DEFAULT]).unwrap();
    (fs, db)
}

/// Every key in the database, in order, read through one full scan.
fn scan_forward(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_to_first();
    let mut out = Vec::new();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    iter.status().unwrap();
    out
}

/// Every key, read backwards, then reversed — which must equal the forward scan.
fn scan_backward(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_to_last();
    let mut out = Vec::new();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    iter.status().unwrap();
    out.reverse();
    out
}

/// What a seek to `target` lands on, and the two entries after it.
fn seek_probe(db: &Db, target: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek(target);
    let mut out = Vec::new();
    for _ in 0..3 {
        if !iter.valid() {
            break;
        }
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    iter.status().unwrap();
    out
}

/// What a reverse seek to `target` lands on, and the two entries before it.
fn seek_for_prev_probe(db: &Db, target: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_for_prev(target);
    let mut out = Vec::new();
    for _ in 0..3 {
        if !iter.valid() {
            break;
        }
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    iter.status().unwrap();
    out
}

/// Writes `count` keys and compacts everything down, leaving one level holding the lot.
///
/// The two databases differ only in memtable size, which is what decides how many files the
/// compaction produces — and therefore whether the level cursor has boundaries to cross.
fn fill(db: &Db, count: u32) {
    for n in 0..count {
        db.put(cf::DEFAULT, &key(n), &value(n)).unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();
}

/// How many files sit below L0, and at which level.
fn files_below_l0(db: &Db) -> usize {
    (1..7)
        .map(|level| {
            db.property(&format!("esker.num-files-at-level{level}.{}", cf::DEFAULT))
                .and_then(|count| count.parse::<usize>().ok())
                .unwrap_or(0)
        })
        .sum()
}

const KEYS: u32 = 600;

#[test]
fn a_level_of_many_files_scans_identically_to_one_file() {
    let (_spread_fs, spread) = open(2 * 1024);
    fill(&spread, KEYS);
    let (_single_fs, single) = open(64 * 1024 * 1024);
    fill(&single, KEYS);

    let many = files_below_l0(&spread);
    let one = files_below_l0(&single);
    assert!(
        many > one,
        "both databases came out the same shape ({many} files vs {one}), so this test is \
         comparing a level cursor with itself. It needs one level split across several files and \
         one that is not."
    );

    let expected = scan_forward(&single);
    assert_eq!(
        expected.len(),
        KEYS as usize,
        "the single-file database did not hold every key, so it is not a specification"
    );
    assert_eq!(
        scan_forward(&spread),
        expected,
        "a level of {many} files did not scan the same as one file. A boundary between two files \
         is where a level cursor loses an entry, repeats one, or stops early."
    );
    assert_eq!(
        scan_backward(&spread),
        expected,
        "the backward scan disagrees with the forward one. `prev` across a file boundary has to \
         land on the *last* entry of the previous file, which is the mirror of the case `next` \
         gets right by accident."
    );
    assert_eq!(scan_backward(&single), expected);
}

#[test]
fn a_seek_agrees_wherever_it_lands_relative_to_a_file_boundary() {
    let (_spread_fs, spread) = open(2 * 1024);
    fill(&spread, KEYS);
    let (_single_fs, single) = open(64 * 1024 * 1024);
    fill(&single, KEYS);
    assert!(files_below_l0(&spread) > files_below_l0(&single));

    // Targets that exist, targets between two existing keys, and targets outside the level at
    // both ends. The between-keys case is the one that matters: a seek can land on a file whose
    // range covers the target and which holds nothing at or after it, and the answer is then the
    // *next* file's first entry rather than "the level ends here".
    let mut targets: Vec<Vec<u8>> = Vec::new();
    for n in (0..KEYS).step_by(7) {
        targets.push(key(n));
        // `key-0007a` sorts between `key-0007` and `key-0008`, so it exists in no file.
        let mut between = key(n);
        between.push(b'a');
        targets.push(between);
    }
    targets.push(b"".to_vec());
    targets.push(b"key".to_vec());
    targets.push(b"key-9999".to_vec());
    targets.push(b"zzzz".to_vec());

    for target in &targets {
        assert_eq!(
            seek_probe(&spread, target),
            seek_probe(&single, target),
            "a forward seek to {:?} disagreed",
            String::from_utf8_lossy(target)
        );
        assert_eq!(
            seek_for_prev_probe(&spread, target),
            seek_for_prev_probe(&single, target),
            "a reverse seek to {:?} disagreed",
            String::from_utf8_lossy(target)
        );
    }
}

#[test]
fn creating_an_iterator_does_not_open_every_file_in_a_level() {
    // The point of the change, measured rather than assumed — and measured at the one instant it
    // is visible. A *finished* scan has touched every file either way, and the table cache holds
    // what it has opened, so counting readers afterwards says nothing. What changed is how many
    // are opened **before the iterator returns**: the old shape pushed one cursor per file for
    // every level, so `Db::iter` paid four reads per file in the database before the caller had
    // seen a key.
    let fs = Arc::new(MemFileSystem::new());
    let dynamic: Arc<dyn FileSystem> = fs.clone();
    let options = || Options {
        create_if_missing: true,
        cf_options: CfOptions {
            write_buffer_size: 4 * 1024,
            target_file_size: 2 * 1024,
            ..CfOptions::default()
        },
        ..Options::default()
    };
    let db = Db::open_with("/db", options(), Arc::clone(&dynamic), &[cf::DEFAULT]).unwrap();
    fill(&db, KEYS);
    let deep = files_below_l0(&db);
    let l0 = db
        .property(&format!("esker.num-files-at-level0.{}", cf::DEFAULT))
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        deep >= 3,
        "only {deep} file(s) below L0, so there is nothing for a level cursor to be lazy about"
    );

    // Reopened, so the table cache starts empty and the count below is what *this* iterator
    // opened rather than what the fill left behind.
    drop(db);
    let db = Db::open_with("/db", options(), dynamic, &[cf::DEFAULT]).unwrap();
    assert_eq!(
        db.property("esker.open-tables").as_deref(),
        Some("0"),
        "the reopened database already has readers open, so the measurement below is not clean"
    );

    let iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    let opened: usize = db
        .property("esker.open-tables")
        .and_then(|count| count.parse().ok())
        .unwrap();
    drop(iter);

    // L0 is still opened file by file and has to be: its files overlap, so any of them can hold
    // the next key, and it is the only level a range tombstone can be in. Everything below it is
    // now nothing until the cursor gets there.
    assert_eq!(
        opened, l0,
        "creating an iterator opened {opened} table(s) against {l0} in L0 and {deep} below it. \
         A level below L0 partitions the key space, so it should cost one cursor and no open \
         file until something seeks into it."
    );
    assert!(
        deep > 0,
        "there was nothing below L0 to leave unopened, so this assertion is vacuous"
    );

    // And the scan still returns everything, which is the thing laziness must not cost.
    assert_eq!(scan_forward(&db).len(), KEYS as usize);
}
