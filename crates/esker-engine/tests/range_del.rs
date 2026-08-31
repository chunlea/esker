//! `DeleteRange` end to end: reads, flush, compaction, recovery, and the invariant that keeps
//! it sound ([ADR 0017](../../docs/adr/0017-range-tombstones.md)).
//!
//! The three things this file exists to hold down, in the order the ruling on decision 6 put
//! them:
//!
//! 1. **The discharge is scheduled, not synchronous.** `write` acknowledges as it always did,
//!    and reads honour the tombstone out of the memtable and L0 for as long as it takes a
//!    compaction to come round. `CLAUDE.md` invariant 1 is untouched.
//! 2. **The forced lower-level inclusion is provable.** A tombstone routinely reaches past the
//!    key range ordinary input selection would pick — a `DROP TABLE` deletes far more than the
//!    one file recording the delete — and the picker has to take every file it covers anyway.
//! 3. **No SST below L0 ever holds a range tombstone.** That is what makes it sound to leave
//!    `search_levels`' binary search alone, so it is asserted rather than assumed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::batch::WriteBatch;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::sst::{TableOptions, TableReader};
use esker_engine::{Db, cf};
use tempfile::TempDir;

fn open(dir: &TempDir) -> Db {
    let options = Options {
        create_if_missing: true,
        ..Options::default()
    };
    Db::open(dir.path(), options).unwrap()
}

fn put(db: &Db, key: &[u8], value: &[u8]) {
    db.put(cf::DEFAULT, key, value).unwrap();
}

fn delete_range(db: &Db, begin: &[u8], end: &[u8]) -> esker_engine::Result<()> {
    let mut batch = WriteBatch::new();
    batch.delete_range(0, begin, end);
    db.write(batch, &WriteOptions::default()).map(|_| ())
}

fn get(db: &Db, key: &[u8]) -> Option<Vec<u8>> {
    db.get(cf::DEFAULT, key, &ReadOptions::default())
        .unwrap()
        .map(|value| value.to_vec())
}

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

/// Every key of the alphabet, one letter each.
fn fill(db: &Db) {
    for letter in b'a'..=b'z' {
        put(db, &[letter], &[letter, letter]);
    }
}

// -- the refusal is gone, and the range is honoured ---------------------------------------

/// The headline: a range delete removes every key in `[begin, end)` and nothing outside it.
///
/// This is what `docs/DESIGN.md` §4.7 refused to do until now, and the refusal is gone in the
/// same commit that makes this pass.
#[test]
fn a_range_delete_removes_exactly_its_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);

    delete_range(&db, b"d", b"h").unwrap();

    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()), "below the range");
    for letter in b'd'..b'h' {
        assert_eq!(
            get(&db, &[letter]),
            None,
            "{} is inside the range",
            letter as char
        );
    }
    assert_eq!(get(&db, b"h"), Some(b"hh".to_vec()), "the end is exclusive");
    assert_eq!(get(&db, b"i"), Some(b"ii".to_vec()), "above the range");

    // And the same through a scan, which is a different code path entirely.
    let keys: Vec<Vec<u8>> = scan(&db).into_iter().map(|(key, _)| key).collect();
    assert!(
        !keys
            .iter()
            .any(|key| key.as_slice() >= &b"d"[..] && key.as_slice() < &b"h"[..])
    );
    assert_eq!(keys.len(), 22, "26 letters less the four deleted");
}

/// A write *after* a range delete survives it. Without the `tombstone.seqno > entry.seqno`
/// bound one `delete_range` would swallow every later write to that range for ever, which is
/// the way a range tombstone goes most catastrophically wrong.
#[test]
fn a_write_after_the_delete_survives_it() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    put(&db, b"e", b"before");
    delete_range(&db, b"d", b"h").unwrap();
    assert_eq!(get(&db, b"e"), None);

    put(&db, b"e", b"after");
    assert_eq!(get(&db, b"e"), Some(b"after".to_vec()));
    // …and again, because the second write must not be treated as re-exposing the first.
    put(&db, b"f", b"later");
    assert_eq!(get(&db, b"f"), Some(b"later".to_vec()));
}

/// A snapshot taken before the delete still sees what it deleted — the ordinary MVCC rule,
/// which a range tombstone must obey like everything else.
#[test]
fn a_snapshot_older_than_the_delete_still_sees_the_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);
    let before = db.snapshot();

    delete_range(&db, b"d", b"h").unwrap();

    let options = ReadOptions {
        snapshot: Some(before.clone()),
        ..ReadOptions::default()
    };
    assert_eq!(
        db.get(cf::DEFAULT, b"e", &options)
            .unwrap()
            .map(|v| v.to_vec()),
        Some(b"ee".to_vec()),
        "the snapshot predates the delete"
    );
    assert_eq!(get(&db, b"e"), None, "and the live read does not");
    drop(before);
}

/// An empty or inverted range is a caller error, not a no-op
/// ([ADR 0017](../../docs/adr/0017-range-tombstones.md) decision 4). A refused batch changes
/// nothing.
#[test]
fn an_empty_or_inverted_range_is_refused() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);

    assert!(delete_range(&db, b"d", b"d").is_err(), "empty");
    assert!(delete_range(&db, b"h", b"d").is_err(), "inverted");
    assert_eq!(get(&db, b"e"), Some(b"ee".to_vec()), "nothing was written");
}

// -- requirement 1: the discharge is scheduled, not synchronous ----------------------------

/// The write acknowledges without waiting for any compaction, and the range reads as deleted
/// from that instant — out of the memtable, then out of L0 after a flush, and only eventually
/// because a compaction discharged it.
///
/// `CLAUDE.md` invariant 1 is about the *log*, and a range delete is one log entry like any
/// other; the discharge is bookkeeping that happens later and changes no answer.
#[test]
fn the_range_reads_as_deleted_before_any_compaction_runs() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);

    let before = db.compactions_run();
    delete_range(&db, b"d", b"h").unwrap();
    assert_eq!(
        db.compactions_run(),
        before,
        "the write did not wait for a compaction"
    );
    assert_eq!(get(&db, b"e"), None, "honoured out of the memtable");

    // After a flush it is an L0 file's tombstone, and the answer does not change.
    db.flush(cf::DEFAULT).unwrap();
    assert_eq!(get(&db, b"e"), None, "honoured out of L0");
    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()));
    assert_eq!(get(&db, b"h"), Some(b"hh".to_vec()));

    // And after the compaction that discharges it, still.
    db.compact_range(cf::DEFAULT, None, None).unwrap();
    assert_eq!(get(&db, b"e"), None, "honoured after the discharge");
    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()));
    assert_eq!(get(&db, b"h"), Some(b"hh".to_vec()));
}

/// The delete survives a crash: it is in the log like any other entry, and replay puts it back
/// in the tombstone list rather than in the map.
#[test]
fn a_range_delete_survives_a_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let db = open(&dir);
        fill(&db);
        delete_range(&db, b"d", b"h").unwrap();
        // No flush: the tombstone exists only in the log and the memtable.
    }
    let db = open(&dir);
    assert_eq!(get(&db, b"e"), None, "replayed as a tombstone");
    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()));
    assert_eq!(get(&db, b"h"), Some(b"hh".to_vec()));
}

/// A memtable holding *only* range deletes still has to reach a file. Flushing it as "empty"
/// would drop the delete on the floor.
#[test]
fn a_memtable_of_nothing_but_deletes_still_flushes() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);
    db.flush(cf::DEFAULT).unwrap();

    delete_range(&db, b"d", b"h").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    assert_eq!(get(&db, b"e"), None);

    // And it is still gone after a reopen, which reads it back out of the file rather than
    // the log.
    drop(db);
    let db = open(&dir);
    assert_eq!(get(&db, b"e"), None, "the tombstone reached a file");
    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()));
}

// -- requirement 3: no SST below L0 ever holds a range tombstone ---------------------------

/// Walks every SST the database currently holds and reports `(level, has tombstones)`.
fn tombstones_by_level(db: &Db, dir: &TempDir) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    for (level, number) in db.files_by_level(cf::DEFAULT).unwrap() {
        let path = dir.path().join(format!("{number:06}.sst"));
        let file =
            esker_engine::fs::FileSystem::open(&esker_engine::fs::LocalFileSystem, &path).unwrap();
        // The engine's own comparator: a table built with one and read with another is
        // refused, which is the check working rather than a problem here.
        let options = TableOptions {
            comparator: db.comparator().clone(),
            ..TableOptions::default()
        };
        let reader = TableReader::open(file, number, options, None).unwrap();
        out.push((level, !reader.range_tombstones().is_empty()));
    }
    out
}

/// The standing invariant. A tombstone lives in a memtable and in L0 and nowhere else: a
/// compaction discharges it rather than propagating it, which is what lets `search_levels`
/// keep the binary search that assumes a level partitions the key space
/// ([ADR 0017](../../docs/adr/0017-range-tombstones.md) decision 6).
#[test]
fn no_sst_below_l0_ever_holds_a_range_tombstone() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    fill(&db);
    db.flush(cf::DEFAULT).unwrap();
    delete_range(&db, b"d", b"h").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    let before = tombstones_by_level(&db, &dir);
    assert!(
        before.iter().any(|(level, has)| *level == 0 && *has),
        "the flush should have put a tombstone in L0: {before:?}"
    );

    db.compact_range(cf::DEFAULT, None, None).unwrap();

    for (level, has) in tombstones_by_level(&db, &dir) {
        assert!(
            level == 0 || !has,
            "a range tombstone reached level {level}, which ADR 0017 decision 6 forbids"
        );
    }
    // And the data is still right, which is the point of the invariant rather than the
    // invariant itself.
    assert_eq!(get(&db, b"e"), None);
    assert_eq!(get(&db, b"c"), Some(b"cc".to_vec()));
    assert_eq!(get(&db, b"h"), Some(b"hh".to_vec()));
}

/// The discharge really removes the covered keys rather than merely hiding them: after it, the
/// tombstone is gone and the keys stay gone, which is only true if every version of them was
/// dropped.
#[test]
fn a_discharge_drops_the_covered_keys_rather_than_hiding_them() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);

    // Three generations, so the covered keys exist at several levels before the delete.
    for round in 0..3u8 {
        for letter in b'a'..=b'z' {
            put(&db, &[letter], &[letter, round]);
        }
        db.flush(cf::DEFAULT).unwrap();
    }
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    delete_range(&db, b"d", b"h").unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    for letter in b'd'..b'h' {
        assert_eq!(get(&db, &[letter]), None, "{}", letter as char);
    }
    for (level, has) in tombstones_by_level(&db, &dir) {
        assert!(!has || level == 0, "a tombstone at level {level}");
    }
    // Reopening reads only what is on disk, so a key that survived the discharge would show
    // up here even if a stale in-memory tombstone had been hiding it.
    drop(db);
    let db = open(&dir);
    for letter in b'd'..b'h' {
        assert_eq!(
            get(&db, &[letter]),
            None,
            "after reopen: {}",
            letter as char
        );
    }
    assert_eq!(get(&db, b"c"), Some(vec![b'c', 2]));
}
