//! A prefix proves whose it is.
//!
//! Two databases pointed at one `--sst-store` prefix used to overwrite each other's
//! `000007.sst` in silence — the object key comes from a file number, and file numbers restart
//! at one in every database. These are the cases that must not be silent any more. The format
//! itself is unit-tested next to it (`esker_engine::fs::claim`); this is about what happens at
//! *open*.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use esker_engine::fs::LocalFileSystem;
use esker_engine::fs::claim::{CLAIM_ID_FILE, CLAIM_OBJECT, ClaimId, Identity, id_for_directory};
use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
use esker_s3::{MemoryStore, ObjectStore};

const PREFIX: &str = "shared/";

fn options(identity: Identity, adopt: bool) -> TierOptions {
    TierOptions {
        key_prefix: PREFIX.to_string(),
        background: false,
        identity: Some(identity),
        adopt_unclaimed: adopt,
        ..TierOptions::default()
    }
}

/// Opens a tier over `dir` against `store`, as a database with `identity` would.
fn open(
    store: &Arc<MemoryStore>,
    dir: &Path,
    identity: Identity,
    adopt: bool,
) -> std::io::Result<Arc<TieredFileSystem>> {
    TieredFileSystem::new(
        Arc::new(LocalFileSystem::new()),
        Arc::clone(store) as Arc<dyn ObjectStore>,
        dir,
        options(identity, adopt),
    )
}

/// The claim id a database in `dir` would use, drawn and persisted the first time.
fn identity_of(dir: &Path, cluster: u64, store_id: u64) -> Identity {
    let local = LocalFileSystem::new();
    Identity::new(id_for_directory(&local, dir).unwrap()).of(cluster, store_id)
}

/// **(a)** A second database is refused, and told whose the prefix is.
#[test]
fn a_second_database_is_refused_and_the_error_names_both() {
    let store = Arc::new(MemoryStore::new());
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = identity_of(first_dir.path(), 1, 1);
    let second = identity_of(second_dir.path(), 1, 2);
    assert_ne!(first.claim, second.claim, "two directories, two identities");

    open(&store, first_dir.path(), first, false).expect("the first database claims the prefix");
    let error = open(&store, second_dir.path(), second, false)
        .expect_err("the second database must not share the prefix");

    let text = error.to_string();
    assert!(text.contains(&first.claim.to_string()), "{text}");
    assert!(text.contains(&second.claim.to_string()), "{text}");
    assert!(text.contains("store 1"), "{text}");
    assert!(text.contains("store 2"), "{text}");
    assert!(text.contains(PREFIX), "{text}");
    // And it says what to do about it, because a refusal an operator cannot act on is a stall.
    assert!(text.contains("prefix of its own"), "{text}");
}

/// **(b)** The same database reopening is accepted, as many times as it likes.
#[test]
fn the_same_database_reopens_its_own_prefix() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..3 {
        let identity = identity_of(dir.path(), 4, 9);
        open(&store, dir.path(), identity, false).expect("a database reopening its own prefix");
    }
    // Exactly one marker, rewritten by nobody.
    assert_eq!(
        store
            .keys()
            .iter()
            .filter(|key| key.ends_with(CLAIM_OBJECT))
            .count(),
        1
    );
}

/// The identity is the *directory's*, not the flags'. Two benchmarks share every flag they have
/// and must still not share a prefix — this is the case `(cluster_id, store_id)` alone misses.
#[test]
fn two_databases_with_identical_ids_still_do_not_share_a_prefix() {
    let store = Arc::new(MemoryStore::new());
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    // Same cluster, same store id, no ids at all: exactly what two `esker bench` runs look like.
    open(&store, one.path(), identity_of(one.path(), 0, 0), false).unwrap();
    let error = open(&store, two.path(), identity_of(two.path(), 0, 0), false)
        .expect_err("two benchmarks must not share a prefix either");
    assert!(error.to_string().contains("claimed by"), "{error}");
}

/// **(c)** A crash between the marker and the first SST. The marker is written inside
/// `TieredFileSystem::new`, before the engine ever has the filesystem, so this is the state a
/// `kill -9` at the worst moment leaves: a claimed prefix with nothing in it.
#[test]
fn a_crash_after_the_marker_and_before_any_sst_reopens_cleanly() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let identity = identity_of(dir.path(), 2, 2);

    let tier = open(&store, dir.path(), identity, false).unwrap();
    assert!(store.contains(&format!("{PREFIX}{CLAIM_OBJECT}")));
    drop(tier); // the kill

    // The same database comes back and recognises its own claim.
    open(&store, dir.path(), identity_of(dir.path(), 2, 2), false)
        .expect("a database must recover its own half-claimed prefix");
    // And somebody else still cannot have it.
    let other = tempfile::tempdir().unwrap();
    assert!(
        open(&store, other.path(), identity_of(other.path(), 2, 3), false).is_err(),
        "a marker with no SSTs behind it is still a claim"
    );
}

/// The claim id survives the crash too — it is written before the marker, so the reopen above
/// is recognising a *persisted* identity and not redrawing one that happens to match.
#[test]
fn the_claim_id_is_persisted_in_the_database_directory() {
    let dir = tempfile::tempdir().unwrap();
    let local = LocalFileSystem::new();
    let first = id_for_directory(&local, dir.path()).unwrap();
    assert!(dir.path().join(CLAIM_ID_FILE).exists());
    assert_eq!(first, id_for_directory(&local, dir.path()).unwrap());
    assert_ne!(first, ClaimId(0), "zero is reserved for 'no id'");

    // A different directory is a different database.
    let other = tempfile::tempdir().unwrap();
    assert_ne!(first, id_for_directory(&local, other.path()).unwrap());
}

/// **(e)** A prefix with objects and no marker is the pre-fix world. It is refused by default —
/// those objects may be a live database's — and the message names the way in.
#[test]
fn a_prefix_with_data_and_no_marker_is_refused_by_default() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    store
        .put(&format!("{PREFIX}000007.sst"), b"someone else's")
        .unwrap();

    let error = open(&store, dir.path(), identity_of(dir.path(), 0, 1), false)
        .expect_err("objects with no marker must not be adopted silently");
    let text = error.to_string();
    assert!(text.contains("no claim marker"), "{text}");
    assert!(text.contains("1 object"), "{text}");
    assert!(
        text.contains("--adopt-sst-store"),
        "the refusal must name the escape hatch: {text}"
    );
}

/// And the hatch works, once it is asked for out loud.
#[test]
fn an_unclaimed_prefix_is_adopted_only_when_asked() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    store
        .put(&format!("{PREFIX}000007.sst"), b"ours, really")
        .unwrap();

    let identity = identity_of(dir.path(), 0, 1);
    open(&store, dir.path(), identity, true).expect("--adopt-sst-store claims it");
    assert!(store.contains(&format!("{PREFIX}{CLAIM_OBJECT}")));

    // Having been adopted, it is now claimed like any other: the hatch is not a permanent door.
    let other = tempfile::tempdir().unwrap();
    assert!(
        open(&store, other.path(), identity_of(other.path(), 0, 2), true).is_err(),
        "adoption must not override somebody else's marker"
    );
}

/// An empty prefix needs no hatch: there is nothing there to lose.
#[test]
fn an_empty_prefix_is_claimed_without_being_asked_twice() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    open(&store, dir.path(), identity_of(dir.path(), 0, 1), false).unwrap();
    assert!(store.contains(&format!("{PREFIX}{CLAIM_OBJECT}")));
}

/// A marker that is there and unreadable is refused, not overruled. It might be somebody's.
#[test]
fn a_corrupt_marker_refuses_the_open_rather_than_being_overwritten() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let identity = identity_of(dir.path(), 0, 1);
    open(&store, dir.path(), identity, false).unwrap();

    store.corrupt(&format!("{PREFIX}{CLAIM_OBJECT}"), vec![0xff; 37]);
    let error = open(&store, dir.path(), identity, false)
        .expect_err("a marker that cannot be read cannot be overruled");
    assert!(error.to_string().contains("claim marker"), "{error}");
    // Even with the hatch: the hatch is for *no* marker, not for one that will not parse.
    assert!(open(&store, dir.path(), identity, true).is_err());
}

/// **(d), the engine half.** Distinct prefixes under one bucket are distinct claims, which is
/// what `esker cluster start`'s derived `node-N` prefixes rely on.
#[test]
fn sibling_prefixes_under_one_bucket_are_claimed_independently() {
    let store = Arc::new(MemoryStore::new());
    let mut dirs = Vec::new();
    for node in 1..=3u64 {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity_of(dir.path(), 1, node);
        TieredFileSystem::new(
            Arc::new(LocalFileSystem::new()),
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            dir.path(),
            TierOptions {
                key_prefix: format!("c1/node-{node}/"),
                background: false,
                identity: Some(identity),
                ..TierOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("node {node} could not claim its own prefix: {error}"));
        dirs.push(dir);
    }
    let markers = store
        .keys()
        .iter()
        .filter(|key| key.ends_with(CLAIM_OBJECT))
        .count();
    assert_eq!(markers, 3, "each node claims its own prefix");
}

/// A tier with no identity claims nothing, which is what the engine's own tests and any
/// single-prefix embedding rely on. Opting in is the caller's decision.
#[test]
fn a_tier_without_an_identity_writes_no_marker() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    TieredFileSystem::new(
        Arc::new(LocalFileSystem::new()),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        dir.path(),
        TierOptions {
            key_prefix: PREFIX.to_string(),
            background: false,
            ..TierOptions::default()
        },
    )
    .unwrap();
    assert!(store.keys().is_empty(), "{:?}", store.keys());
}
