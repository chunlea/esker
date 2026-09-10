//! One directory, one writer.
//!
//! The engine had no such rule until [`FileSystem::lock_directory`], and the shape that finds
//! that out is not exotic: a supervisor restarting a store beside an operator restarting the same
//! one. Both binaries **open their database before they bind their port**
//! (`esker-cli/src/server.rs` opens at 230 and binds at 297; `esker-cli/src/pd.rs` at 502 and
//! 516), so the process that loses the port has already opened the tree — and if the two are
//! given different ports, nothing decides between them at all and both write WAL segments and
//! manifests into one directory.
//!
//! The refusal is an error value and never a panic (invariant 9), and never a wait: a node that
//! blocked on a held directory would be a node an operator reads as hung.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::error::Error;
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions};
use esker_engine::{Db, cf};
use tempfile::TempDir;

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

#[test]
fn a_second_open_of_one_directory_is_refused_and_the_first_keeps_working() {
    let dir = TempDir::new().unwrap();
    let first = Db::open(dir.path(), options()).unwrap();

    let refused = Db::open(dir.path(), options());
    match refused {
        Err(Error::InUse { dir: held }) => assert_eq!(held, dir.path()),
        Err(other) => panic!("the second open failed, but not as a claim: {other}"),
        Ok(_) => panic!(
            "two databases are open on {} — this is the two-writers case, and nothing stopped it",
            dir.path().display()
        ),
    }

    // The refusal costs the holder nothing: a claim that broke the database it protects would be
    // worse than the race.
    first.put(cf::DEFAULT, b"k", b"v").unwrap();
    assert_eq!(
        first
            .get(cf::DEFAULT, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"v"[..])
    );
}

#[test]
fn a_closed_database_leaves_its_directory_free() {
    let dir = TempDir::new().unwrap();
    let first = Db::open(dir.path(), options()).unwrap();
    first.put(cf::DEFAULT, b"k", b"v").unwrap();
    drop(first);

    // Not "the lock file is gone" — it is still there, and it is supposed to be. What must be
    // true is that the next open succeeds and reads what the last one wrote.
    let second = Db::open(dir.path(), options()).unwrap();
    assert_eq!(
        second
            .get(cf::DEFAULT, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"v"[..])
    );
}

/// The claim is a file the engine does not otherwise know, and the sweep must leave it alone.
///
/// `filename::classify` answers `None` for `LOCK`, and `obsolete_files` keeps what it cannot
/// classify — *"anything the engine did not create is not the engine's to delete"*. That sentence
/// is what this test pins: deleting the lock out from under its holder would hand the directory
/// to the next process while the first was still writing to it.
#[test]
fn the_sweep_does_not_collect_the_lock() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path(), options()).unwrap();
    for round in 0..64u32 {
        db.put(cf::DEFAULT, &round.to_be_bytes(), b"v").unwrap();
    }
    db.flush(cf::DEFAULT).unwrap();
    let collected = db.purge_obsolete_files().unwrap();
    assert!(
        !collected
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == "LOCK")),
        "the sweep collected the lock file: {collected:?}"
    );
    assert!(dir.path().join("LOCK").exists(), "the lock file is gone");
}

/// The in-memory filesystem claims nothing, and that is the decision rather than the omission.
///
/// **A claim is about processes, and there are none here.** The crash tests share one of these
/// deliberately — `wal_sync_crash.rs` `mem::forget`s a `Db` to model a machine that ran no
/// destructor and then opens the same directory again — and a claim held by a handle nobody will
/// ever drop would make a power loss unmodellable while protecting nothing a deployment has. This
/// test exists so that changing that answer has to change a test that says why.
#[test]
fn the_in_memory_filesystem_hands_a_directory_to_anybody() {
    let fs = Arc::new(MemFileSystem::new());
    let _first = fs.lock_directory(std::path::Path::new("/db")).unwrap();
    fs.lock_directory(std::path::Path::new("/db"))
        .expect("an in-memory filesystem models a disk and not a machine");
}

/// A different directory is a different claim — the obvious half, and the one that would make
/// every other test in this file pass for the wrong reason if it were broken.
#[test]
fn two_directories_are_two_claims() {
    let one = TempDir::new().unwrap();
    let two = TempDir::new().unwrap();
    let _first = Db::open(one.path(), options()).unwrap();
    let _second = Db::open(two.path(), options()).unwrap();
}

/// `LocalFileSystem` is stateless and `Copy`, so two handles to it are the same filesystem.
#[test]
fn two_handles_to_the_local_filesystem_are_one_filesystem() {
    let dir = TempDir::new().unwrap();
    let held = LocalFileSystem::new().lock_directory(dir.path()).unwrap();
    let refused = LocalFileSystem::new().lock_directory(dir.path());
    assert!(
        refused.is_err(),
        "a second handle claimed a directory the first holds"
    );
    drop(held);
}
