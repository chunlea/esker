//! #62 — what a full compaction leaves behind when every key has been deleted.
//!
//! # Where this came from
//!
//! #58's remaining growth is a scan walking further every round with the same work to do: a
//! `DROP TABLE` at round 1000 walked 22,850 stored entries to issue 14 reads, rising +22 a round,
//! and the rise is independent of how much data each round writes. On a real store, flushing and
//! compacting every column family moved the `write` family from 1,773 entries to 604 — the MVCC
//! collector doing its job — and moved the `lock` family from **3,546 to 3,546**.
//!
//! The `lock` family is where that matters most because it has no MVCC in it: a lock key is
//! `'x' ++ enc(user_key)` with no timestamp suffix, put at prewrite and deleted at commit. So its
//! contents are the engine's own superseded entries and tombstones, and nothing can read below the
//! newest entry for such a key — `get_lock` takes no timestamp with which to ask.
//!
//! # What is asserted, and why at this layer
//!
//! The rules are already written down in `compaction::job`'s own header, and they are the standard
//! ones: a version nothing can reach may go; a tombstone may go only where no level below the
//! output can still hold an older value. So the question is not what the rules say — it is whether
//! a compaction of a store in this shape ever reaches them. That is an engine question and it does
//! not need a cluster, a catalog or a transaction to ask, which is why it is asked here on a
//! `Db` with nothing else in it.
//!
//! **No snapshot is held anywhere in this file**, which is the other half of the question: with no
//! reader to protect, the floor is the present and every rule above is free to fire. A test that
//! left one open would be measuring the protection rather than the collection — and there is a
//! test below that holds one on purpose, to prove the protection still works.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions};
use esker_engine::{Db, cf};

/// Enough keys that "all of them" and "none of them" are far apart, and few enough that the whole
/// thing stays in one flush.
const KEYS: usize = 200;

fn open() -> (Arc<dyn FileSystem>, Db) {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let db = Db::open_with(
        "/db",
        Options {
            create_if_missing: true,
            ..Options::default()
        },
        Arc::clone(&fs),
        &[cf::DEFAULT],
    )
    .unwrap();
    (fs, db)
}

fn key(at: usize) -> Vec<u8> {
    format!("k{at:05}").into_bytes()
}

/// Entries the tables hold, summed — what a compaction was supposed to reduce.
fn entries(db: &Db) -> u64 {
    db.sst_entries(cf::DEFAULT)
        .unwrap()
        .iter()
        .map(|(_, _, count)| count)
        .sum()
}

/// Stored entries a full scan walks. The cost side, as `#58` measures it.
fn stepped_by_a_scan(db: &Db) -> u64 {
    let before: u64 = db
        .property("esker.entries-stepped")
        .unwrap()
        .parse()
        .unwrap();
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_to_first();
    while iter.valid() {
        iter.next();
    }
    iter.status().unwrap();
    let after: u64 = db
        .property("esker.entries-stepped")
        .unwrap()
        .parse()
        .unwrap();
    after - before
}

/// **The red one.** Every key written, every key deleted, flushed, and compacted end to end with
/// no reader to protect — and what is left should be nothing.
///
/// A tombstone whose key has no older value anywhere below the output is a tombstone that protects
/// nothing: the rules in `compaction::job` say so, and this asks whether a compaction in the
/// simplest possible shape — one column family, one level's worth of files, no snapshots — ever
/// gets to apply them.
#[test]
fn a_compaction_drops_the_tombstones_of_keys_that_are_gone() {
    let (_fs, db) = open();
    for at in 0..KEYS {
        db.put(cf::DEFAULT, &key(at), b"value").unwrap();
    }
    for at in 0..KEYS {
        db.delete(cf::DEFAULT, &key(at)).unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    let before = entries(&db);
    assert!(
        before >= KEYS as u64,
        "the writes did not reach an SST, so this measures nothing: {before} entries"
    );

    db.compact_range(cf::DEFAULT, None, None).unwrap();
    let after = entries(&db);
    let walked = stepped_by_a_scan(&db);

    assert_eq!(
        after, 0,
        "a full compaction with no reader to protect left {after} of {before} entries, all of \
         them tombstones for keys with nothing beneath them"
    );
    assert!(
        walked <= 1,
        "a scan of an empty column family walked {walked} stored entries"
    );
}

/// **The same shape the `lock` column family is in**: one key put and deleted many times over, so
/// what accumulates is the engine's own superseded entries rather than any kind of version.
///
/// Separate from the test above because the two fail for different reasons if they fail: that one
/// is about tombstones for keys that are gone, this one is about the entries *under* the newest,
/// which rule 1 covers and which no level below can be hiding anything from.
#[test]
fn a_compaction_drops_the_versions_under_the_newest() {
    const ROUNDS: usize = 50;

    let (_fs, db) = open();
    for _ in 0..ROUNDS {
        for at in 0..10 {
            db.put(cf::DEFAULT, &key(at), b"held").unwrap();
            db.delete(cf::DEFAULT, &key(at)).unwrap();
        }
    }
    db.flush(cf::DEFAULT).unwrap();
    let before = entries(&db);
    assert!(
        before >= (ROUNDS * 10) as u64,
        "the writes did not reach an SST: {before} entries"
    );

    db.compact_range(cf::DEFAULT, None, None).unwrap();
    let after = entries(&db);
    assert_eq!(
        after, 0,
        "ten keys put and deleted {ROUNDS} times left {after} of {before} entries after a full \
         compaction; every one of them is unreachable"
    );
}

/// **And the rule that must not be loosened**, which is the counterfactual for both tests above: a
/// reader holding a snapshot from before the deletes must still see what it could see.
///
/// This is the assertion that makes the other two safe to satisfy. A compaction that dropped
/// everything unconditionally would pass them and lose an acknowledged read — so the same workload
/// is run with a snapshot held across it, and the values have to survive.
#[test]
fn a_snapshot_keeps_what_it_can_still_see() {
    let (_fs, db) = open();
    for at in 0..KEYS {
        db.put(cf::DEFAULT, &key(at), b"value").unwrap();
    }
    let snapshot = db.snapshot();
    for at in 0..KEYS {
        db.delete(cf::DEFAULT, &key(at)).unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    let options = ReadOptions {
        snapshot: Some(snapshot),
        ..ReadOptions::default()
    };
    for at in 0..KEYS {
        let seen = db.get(cf::DEFAULT, &key(at), &options).unwrap();
        assert_eq!(
            seen.as_deref(),
            Some(&b"value"[..]),
            "key {at} was collected out from under a snapshot that predates its delete"
        );
    }
    // And the present still sees the deletes, which is the other half of the same claim.
    let now = db
        .get(cf::DEFAULT, &key(0), &ReadOptions::default())
        .unwrap();
    assert_eq!(
        now, None,
        "the delete is still in force for a reader with no snapshot"
    );
}
