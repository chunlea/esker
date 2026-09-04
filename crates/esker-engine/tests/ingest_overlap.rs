//! The ingest overlap rule, as a property: **accepted exactly when the keys are free**.
//!
//! `crates/esker-engine/src/db/ingest.rs` states the rule — an ingest is refused exactly when the
//! file holds a user key the column family already has an entry for, whether that is a value, a
//! point tombstone, or a range tombstone covering it — and the reason: the sequence numbers of two
//! databases are not comparable, so two versions of *one* key have no defensible order, while
//! entries under different keys are never compared at all.
//!
//! A rule stated in a module header is a claim about every input, so it is checked against random
//! ones. Two properties, and the second is the one that would catch a widening that is merely
//! optimistic:
//!
//! 1. **the decision matches the rule** — `ingest` returns `Ok` if and only if the ingested key set
//!    is disjoint from every key the column family has an entry for and no range tombstone covers
//!    any of them. Both directions: a refusal where the rule permits is a regression to the old
//!    range check, and an acceptance where the rule forbids is data whose version is a guess;
//! 2. **an accepted ingest reads back as the union** — every key of the file answers the file's
//!    value, every pre-existing key answers its own, and every deleted key still answers nothing.
//!
//! Property 2 is what makes property 1 worth having. Widening a rule is easy to do in a way that
//! passes every "is it refused" test and answers the wrong value afterwards, because the wrong
//! answer is a *readable* one — which is exactly the failure mode the whole rule exists to prevent.
//!
//! # The generator interleaves rather than partitions
//!
//! Keys are drawn from one small space for both sides, so the ordinary case is ranges that overlap
//! heavily and key sets that may or may not. A generator that put the two sides in separate ranges
//! would test the old rule: it would never produce the case this unit is about, which is a file
//! whose range covers the column family's and whose keys miss every one of them — the shape a
//! bulk load of MVCC versions has, since `esker-txn` puts the version in the key.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_engine::batch::WriteBatch;
use esker_engine::filename;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::{Db, cf};
use proptest::prelude::*;

/// The key space both sides draw from. Small on purpose: collisions have to be common or the
/// refusing half of the rule is never exercised.
const SPACE: u32 = 24;

fn key(n: u32) -> Vec<u8> {
    format!("k{n:03}").into_bytes()
}

fn memfs() -> Arc<dyn FileSystem> {
    Arc::new(MemFileSystem::new())
}

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

/// An SST holding `keys`, built by a database of its own so its sequence numbers are genuinely
/// another database's — which is the whole reason the rule exists.
fn source_file(fs: &Arc<dyn FileSystem>, dir: &str, keys: &BTreeSet<u32>) -> PathBuf {
    let db = Db::open_with(dir, options(), Arc::clone(fs), &[cf::DEFAULT]).unwrap();
    for n in keys {
        db.put(cf::DEFAULT, &key(*n), b"ingested").unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    drop(db);
    let mut ssts: Vec<PathBuf> = fs
        .list(Path::new(dir))
        .unwrap()
        .into_iter()
        .filter(|path| {
            matches!(
                filename::classify_path(path),
                Some(filename::FileKind::Sst(_))
            )
        })
        .collect();
    assert_eq!(ssts.len(), 1, "the source should be exactly one file");
    ssts.pop().unwrap()
}

fn get(db: &Db, k: &[u8]) -> Option<Vec<u8>> {
    db.get(cf::DEFAULT, k, &ReadOptions::default())
        .unwrap()
        .map(|value| value.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// The rule, both directions, and the reads that follow an acceptance.
    #[test]
    fn an_ingest_is_accepted_exactly_when_its_keys_are_free(
        present in proptest::collection::btree_set(0u32..SPACE, 0..8),
        deleted in proptest::collection::btree_set(0u32..SPACE, 0..4),
        ingested in proptest::collection::btree_set(0u32..SPACE, 1..8),
        range_del in proptest::option::of((0u32..SPACE, 0u32..SPACE)),
    ) {
        let fs = memfs();
        let source = source_file(&fs, "/source", &ingested);
        let db = Db::open_with("/db", options(), Arc::clone(&fs), &[cf::DEFAULT]).unwrap();

        // The column family: some values, some point tombstones, and possibly one range delete.
        // A point tombstone is an *entry* under its key, which is why deleting a key does not
        // free it — the sequence number would still have to decide between the delete and the
        // ingested value, and it cannot.
        for n in &present {
            db.put(cf::DEFAULT, &key(*n), b"local").unwrap();
        }
        for n in &deleted {
            db.delete(cf::DEFAULT, &key(*n)).unwrap();
        }
        let covered: BTreeSet<u32> = match range_del {
            Some((low, high)) if low < high => {
                let cf_id = db.cf_id(cf::DEFAULT).unwrap();
                let mut batch = WriteBatch::new();
                batch.delete_range(cf_id, &key(low), &key(high));
                db.write(batch, &WriteOptions::default()).unwrap();
                (low..high).collect()
            }
            _ => BTreeSet::new(),
        };

        // The rule, computed from the inputs rather than from the code under test.
        let occupied: BTreeSet<u32> = present
            .union(&deleted)
            .copied()
            .collect::<BTreeSet<u32>>()
            .union(&covered)
            .copied()
            .collect();
        let free = ingested.is_disjoint(&occupied);

        let outcome = db.ingest(cf::DEFAULT, &[source]);
        prop_assert_eq!(
            outcome.is_ok(),
            free,
            "ingesting {:?} into a family holding {:?} (deleted {:?}, range-deleted {:?}): \
             the rule says free={}, ingest said {:?}",
            ingested, present, deleted, covered, free, outcome.as_ref().err().map(ToString::to_string)
        );

        // Property 2: an acceptance reads back as the union, and a refusal changed nothing.
        // Both are checked against the same expectation, because a refusal that half-applied is
        // the same defect as an acceptance that lost a key.
        let mut expected: BTreeMap<Vec<u8>, &[u8]> = BTreeMap::new();
        for n in &present {
            expected.insert(key(*n), b"local");
        }
        for n in &deleted {
            expected.remove(&key(*n));
        }
        for n in &covered {
            expected.remove(&key(*n));
        }
        if free {
            for n in &ingested {
                expected.insert(key(*n), b"ingested");
            }
        }
        for n in 0..SPACE {
            let answer = get(&db, &key(n));
            prop_assert_eq!(
                answer.as_deref(),
                expected.get(&key(n)).copied(),
                "key {} after {} ingest",
                n,
                if free { "an accepted" } else { "a refused" }
            );
        }
    }

    /// The case the widening is *for*: ranges that interleave completely, keys that never collide.
    ///
    /// This is the shape an MVCC bulk load has — `esker-txn` encodes the version into the key, so
    /// a load of one time range for rows that already exist overlaps every file and shares no key.
    /// Under the old range rule every one of these was refused; under the rule they are all
    /// accepted, and the read afterwards is the union.
    #[test]
    fn interleaved_ranges_that_share_no_key_are_all_accepted(
        rows in proptest::collection::btree_set(0u32..SPACE, 1..10),
    ) {
        let fs = memfs();
        // Two versions per row, one on each side: `<row>@1` is local and `<row>@2` is ingested.
        // Every ingested key sorts strictly between two local ones, so the ranges are as
        // interleaved as they can be and the key sets are disjoint by construction.
        let ingested: BTreeSet<u32> = rows.iter().map(|row| row * 2 + 1).collect();
        let source = source_file(&fs, "/source", &ingested);

        let db = Db::open_with("/db", options(), Arc::clone(&fs), &[cf::DEFAULT]).unwrap();
        for row in &rows {
            db.put(cf::DEFAULT, &key(row * 2), b"local").unwrap();
        }
        db.ingest(cf::DEFAULT, &[source]).unwrap();

        for row in &rows {
            let local = get(&db, &key(row * 2));
            let adopted = get(&db, &key(row * 2 + 1));
            prop_assert_eq!(local.as_deref(), Some(&b"local"[..]));
            prop_assert_eq!(adopted.as_deref(), Some(&b"ingested"[..]));
        }
    }
}
