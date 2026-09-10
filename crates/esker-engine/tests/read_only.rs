//! An open that reads, and does nothing else.
//!
//! An ordinary `Db::open` is not a read: it replays the log, **creates a fresh write-ahead
//! segment**, appends a manifest edit, claims the directory and starts the flusher and the
//! compactors. That is right for a process about to serve and wrong for a tool about to print —
//! and it is why `esker pd inspect`, whose own module doc says it reads *"a **stopped** placement
//! driver's files"*, was writing into the database of a running one every 200 ms for a minute in
//! `cluster_start.rs`.
//!
//! The two properties below are the whole contract, and each is asserted against the arrangement
//! it exists for: a **live writer holding the directory**, and a **before-and-after listing** of
//! every file in it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_engine::error::Error;
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::options::{CfOptions, OpenMode, Options, ReadOptions};
use esker_engine::{Db, cf};
use tempfile::TempDir;

fn writable() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

fn reading() -> Options {
    Options {
        mode: OpenMode::ReadOnly,
        ..Options::default()
    }
}

/// Every name in the directory, so "wrote nothing" is a set difference and not a guess.
fn names(dir: &Path) -> BTreeSet<PathBuf> {
    LocalFileSystem::new()
        .list(dir)
        .unwrap()
        .into_iter()
        .collect()
}

#[test]
fn a_read_only_open_succeeds_while_a_writer_holds_the_directory() {
    let dir = TempDir::new().unwrap();
    let writer = Db::open(dir.path(), writable()).unwrap();
    writer.put(cf::DEFAULT, b"k", b"v").unwrap();
    writer.flush(cf::DEFAULT).unwrap();
    writer.put(cf::DEFAULT, b"logged", b"only").unwrap();

    // The control, first: a *writable* open of the same directory is still refused, so what the
    // read-only open below proves is about the mode and not about the claim having gone away.
    match Db::open(dir.path(), writable()) {
        Err(Error::InUse { .. }) => {}
        other => panic!("a second writer was not refused: {other:?}"),
    }

    let reader = Db::open_with(
        dir.path(),
        reading(),
        Arc::new(LocalFileSystem::new()),
        &[cf::DEFAULT],
    )
    .expect("a reader must be able to open a directory a writer holds");

    // What was flushed, and what was only in the log — both are the writer's committed data, and
    // a reader that replayed the log sees both.
    assert_eq!(
        reader
            .get(cf::DEFAULT, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"v"[..])
    );
    assert_eq!(
        reader
            .get(cf::DEFAULT, b"logged", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"only"[..])
    );

    // And it cannot write, however it is asked.
    let refused = reader.put(cf::DEFAULT, b"reader", b"no").unwrap_err();
    assert!(
        matches!(refused, Error::Unsupported(_)),
        "a write through a read-only database must be refused, and it said: {refused}"
    );

    // The writer is untouched by having been read.
    writer.put(cf::DEFAULT, b"after", b"still-writing").unwrap();
}

#[test]
fn a_read_only_open_leaves_no_new_file_behind() {
    let dir = TempDir::new().unwrap();
    {
        let writer = Db::open(dir.path(), writable()).unwrap();
        writer.put(cf::DEFAULT, b"k", b"v").unwrap();
        writer.flush(cf::DEFAULT).unwrap();
    }

    let before = names(dir.path());
    {
        let reader = Db::open_with(
            dir.path(),
            reading(),
            Arc::new(LocalFileSystem::new()),
            &[cf::DEFAULT],
        )
        .unwrap();
        assert_eq!(
            reader
                .get(cf::DEFAULT, b"k", &ReadOptions::default())
                .unwrap()
                .as_deref(),
            Some(&b"v"[..])
        );
    }
    let after = names(dir.path());

    assert_eq!(
        before,
        after,
        "a read-only open changed the directory: added {:?}, removed {:?}",
        after.difference(&before).collect::<Vec<_>>(),
        before.difference(&after).collect::<Vec<_>>()
    );
}

/// **And it deletes nothing either.** A reader that swept obsolete files would delete them out
/// from under the process that owns them — the sweep is the writer's, and `purge_obsolete_files`
/// runs at the end of a writable open and nowhere in a read-only one.
#[test]
fn a_read_only_open_sweeps_nothing() {
    let dir = TempDir::new().unwrap();
    {
        let writer = Db::open(dir.path(), writable()).unwrap();
        for round in 0..64_u32 {
            writer.put(cf::DEFAULT, &round.to_be_bytes(), b"v").unwrap();
        }
        writer.flush(cf::DEFAULT).unwrap();
    }
    // **A sorted string table no live version references**, which is precisely what the sweep
    // exists to collect. Its contents are never read — an SST is opened when something reads a key
    // in its range, and nothing here does — so garbage is enough to make it a file with a number
    // and no owner. A `.wal` would not do: a segment above the log number is *replayed* at open,
    // and this would become a test about parsing rubbish.
    let orphan = dir.path().join("000999.sst");
    std::fs::write(&orphan, b"not a real table").unwrap();

    let before = names(dir.path());
    drop(
        Db::open_with(
            dir.path(),
            reading(),
            Arc::new(LocalFileSystem::new()),
            &[cf::DEFAULT],
        )
        .unwrap(),
    );
    assert_eq!(
        before,
        names(dir.path()),
        "a read-only open swept the directory"
    );

    // **The control, and it is what makes the assertion above about the mode.** The same file, a
    // writable open, and it is collected — so the reader kept something genuinely sweepable
    // rather than something nothing would ever have taken.
    drop(Db::open(dir.path(), writable()).unwrap());
    assert!(
        !names(dir.path()).contains(&orphan),
        "a writable open did not sweep an orphaned table, so this test's control proves nothing"
    );
}

/// A family the caller names and the database does not have is **absent**, not created.
///
/// `Db::open_with` creates any family it is named, which is the behaviour that would make an
/// inspector answer "this database has a `write` column family" because it had just made one.
#[test]
fn a_read_only_open_creates_no_column_family() {
    let dir = TempDir::new().unwrap();
    drop(Db::open(dir.path(), writable()).unwrap());

    let before = names(dir.path());
    let reader = Db::open_with(
        dir.path(),
        reading(),
        Arc::new(LocalFileSystem::new()),
        &[cf::DEFAULT, "a-family-that-is-not-there"],
    )
    .unwrap();
    assert!(
        reader.cf_id("a-family-that-is-not-there").is_none(),
        "the reader created the family it was asked about"
    );
    drop(reader);
    assert_eq!(before, names(dir.path()));
}

/// **Every way of changing a byte is refused**, and not only the obvious one.
///
/// A flush writes a table and a manifest edit; a compaction writes tables; the sweep *deletes*
/// them; `create_cf` and `drop_cf` write edits; a checkpoint pins the source's live files and
/// links them, which a process that does not own the source cannot do safely. A read-only handle
/// that could do any of those would be doing it inside a database somebody else owns.
///
/// The list is this file's job to keep: a new mutator added without a guard passes every other
/// test here.
#[test]
fn every_way_of_writing_is_refused() {
    let dir = TempDir::new().unwrap();
    {
        let writer = Db::open(dir.path(), writable()).unwrap();
        writer.put(cf::DEFAULT, b"k", b"v").unwrap();
    }
    let reader = Db::open_with(
        dir.path(),
        reading(),
        Arc::new(LocalFileSystem::new()),
        &[cf::DEFAULT],
    )
    .unwrap();

    let refused = |what: &str, result: Result<(), Error>| match result {
        Err(Error::Unsupported(detail)) => assert!(
            detail.contains("read-only"),
            "{what} was refused, but not as a read-only database: {detail}"
        ),
        Err(other) => panic!("{what} was refused for the wrong reason: {other}"),
        Ok(()) => panic!("{what} was allowed on a read-only database"),
    };

    refused("put", reader.put(cf::DEFAULT, b"a", b"b").map(|_| ()));
    refused("delete", reader.delete(cf::DEFAULT, b"a").map(|_| ()));
    refused("flush", reader.flush(cf::DEFAULT));
    refused("flush_all", reader.flush_all());
    refused(
        "compact_range",
        reader.compact_range(cf::DEFAULT, None, None),
    );
    refused(
        "purge_obsolete_files",
        reader.purge_obsolete_files().map(|_| ()),
    );
    refused(
        "create_cf",
        reader.create_cf("new", CfOptions::default()).map(|_| ()),
    );
    refused("drop_cf", reader.drop_cf(cf::DEFAULT));
    let elsewhere = TempDir::new().unwrap();
    refused("checkpoint", reader.checkpoint(elsewhere.path(), None));
}

/// A directory with no database is still `NotFound`, read-only or not — a mistyped path must not
/// read as an empty cluster.
#[test]
fn a_read_only_open_of_nothing_is_not_found() {
    let dir = TempDir::new().unwrap();
    let error = Db::open_with(
        dir.path(),
        reading(),
        Arc::new(LocalFileSystem::new()),
        &[cf::DEFAULT],
    )
    .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error}");
}
