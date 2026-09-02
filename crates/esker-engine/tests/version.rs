//! The manifest's on-disk shapes: `VersionEdit` bytes, and what recovery makes of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::dbformat::{EntryKind, internal_key};
use esker_engine::version::{FileLocation, FileMeta, VersionEdit};
use proptest::prelude::*;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The edit of `docs/DESIGN.md` §4.6, field by field, against a file written by a separate
/// encoder.
#[test]
fn golden_version_edit() {
    let golden = include_str!("golden/version-edit.hex");
    let fields: Vec<&str> = golden
        .lines()
        .filter_map(|line| line.strip_prefix("field "))
        .collect();
    assert_eq!(fields.len(), 9, "the golden file lost a field");

    let mut edit = VersionEdit::new();
    edit.comparator = Some("esker.BytewiseComparator".into());
    edit.log_number = Some(11);
    edit.next_file_number = Some(12);
    edit.last_seqno = Some((1 << 56) - 1);
    edit.cf_added.push((0, "default".into()));
    edit.cf_added.push((7, "write".into()));
    edit.cf_dropped.push(3);
    edit.delete_file(0, 1, 5);
    edit.add_file(
        0,
        0,
        FileMeta {
            number: 8,
            size: 32_768,
            smallest: internal_key(b"apple", 10, EntryKind::Put),
            largest: internal_key(b"pear", 20, EntryKind::Put),
            smallest_seqno: 10,
            largest_seqno: 20,
            location: FileLocation::Local,
        },
    );

    assert_eq!(
        hex(&edit.encode()),
        fields.concat(),
        "the encoded edit is its fields, in the order encode() writes them"
    );
    assert_eq!(VersionEdit::decode(&edit.encode()).unwrap(), edit);
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    (prop::collection::vec(any::<u8>(), 0..16), 0u64..1_000_000)
        .prop_map(|(user, seq)| internal_key(&user, seq, EntryKind::Put))
}

fn file_meta() -> impl Strategy<Value = FileMeta> {
    (
        any::<u64>(),
        any::<u64>(),
        key(),
        key(),
        0u64..(1 << 56),
        0u64..(1 << 56),
    )
        .prop_map(|(number, size, smallest, largest, a, b)| FileMeta {
            number,
            size,
            smallest,
            largest,
            smallest_seqno: a.min(b),
            largest_seqno: a.max(b),
            location: FileLocation::Local,
        })
}

proptest! {
    #[test]
    fn edits_round_trip(
        comparator in prop::option::of("[a-zA-Z.]{1,32}"),
        log_number in prop::option::of(any::<u64>()),
        next_file_number in prop::option::of(any::<u64>()),
        last_seqno in prop::option::of(0u64..(1 << 56)),
        cf_added in prop::collection::vec((any::<u32>(), "[a-z]{1,12}"), 0..4),
        cf_dropped in prop::collection::vec(any::<u32>(), 0..4),
        deleted in prop::collection::vec((any::<u32>(), 0u32..8, any::<u64>()), 0..6),
        added in prop::collection::vec((any::<u32>(), 0u32..8, file_meta()), 0..4),
    ) {
        let mut edit = VersionEdit::new();
        edit.comparator = comparator;
        edit.log_number = log_number;
        edit.next_file_number = next_file_number;
        edit.last_seqno = last_seqno;
        edit.cf_added = cf_added;
        edit.cf_dropped = cf_dropped;
        edit.deleted_files = deleted;
        edit.added_files = added;

        prop_assert_eq!(VersionEdit::decode(&edit.encode())?, edit);
    }

    /// Manifest bytes come off a disk that was being written to when the machine died.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
        if let Ok(edit) = VersionEdit::decode(&bytes) {
            // Anything that decodes must re-encode to the same bytes: there is exactly one
            // encoding of an edit, so a second one would mean the decoder invented something.
            prop_assert_eq!(edit.encode(), bytes);
        }
    }
}

// ---------------------------------------------------------------------------------------
// The manifest: recovery, the CURRENT swap, and what a crash between them leaves behind.
// ---------------------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use esker_engine::dbformat::{BytewiseComparator, Comparator, InternalKeyComparator};
use esker_engine::error::Error;
use esker_engine::filename;
use esker_engine::fs::{FileSystem, RandomAccessFile, WritableFile};
use esker_engine::memfs::MemFileSystem;
use esker_engine::version::{Version, VersionSet};

/// A filesystem operation a test can make fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    Create,
    SyncData,
    Rename,
    FsyncDir,
}

/// Wraps an in-memory filesystem and stops it at a chosen point, the way a crash would.
///
/// `fail_after(op, k)` lets `k` more calls of `op` succeed and fails every one after that. The
/// engine returns the error and stops, which is the part of a crash that is observable from
/// inside the process; what is on disk afterwards is what the test then reopens.
#[derive(Debug, Clone)]
struct CrashFs {
    inner: Arc<MemFileSystem>,
    budget: Arc<Mutex<BTreeMap<Op, usize>>>,
}

impl CrashFs {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemFileSystem::new()),
            budget: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn fail_after(&self, op: Op, calls: usize) {
        self.budget.lock().unwrap().insert(op, calls);
    }

    fn no_faults(&self) {
        self.budget.lock().unwrap().clear();
    }

    fn check(&self, op: Op) -> io::Result<()> {
        let mut budget = self.budget.lock().unwrap();
        match budget.get_mut(&op) {
            None => Ok(()),
            Some(0) => Err(io::Error::other(format!("injected fault on {op:?}"))),
            Some(remaining) => {
                *remaining -= 1;
                Ok(())
            }
        }
    }
}

impl FileSystem for CrashFs {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        self.check(Op::Create)?;
        Ok(Box::new(CrashFile {
            inner: self.inner.create(path)?,
            fs: self.clone(),
        }))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        self.inner.open(path)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(Op::Rename)?;
        self.inner.rename(from, to)
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        self.inner.delete(path)
    }
    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        self.check(Op::FsyncDir)?;
        self.inner.fsync_dir(dir)
    }
    fn size(&self, path: &Path) -> io::Result<u64> {
        self.inner.size(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.inner.create_dir_all(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.inner.remove_dir_all(dir)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.hard_link(from, to)
    }
}

struct CrashFile {
    inner: Box<dyn WritableFile>,
    fs: CrashFs,
}

impl std::fmt::Debug for CrashFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrashFile").finish_non_exhaustive()
    }
}

impl WritableFile for CrashFile {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.append(data)
    }
    fn sync_data(&mut self) -> io::Result<()> {
        self.fs.check(Op::SyncData)?;
        self.inner.sync_data()
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.fs.check(Op::SyncData)?;
        self.inner.sync_all()
    }
}

/// A comparator that is not the default one, for the reopen-mismatch test.
#[derive(Debug)]
struct ReverseComparator;

impl Comparator for ReverseComparator {
    fn cmp(&self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        b.cmp(a)
    }
    fn name(&self) -> &'static str {
        "esker.ReverseComparator"
    }
}

const DIR: &str = "/db";
const LEVELS: usize = 7;

fn bytewise() -> Arc<InternalKeyComparator> {
    Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
}

fn create(fs: &Arc<dyn FileSystem>) -> VersionSet {
    VersionSet::create(
        Arc::clone(fs),
        Path::new(DIR),
        bytewise(),
        LEVELS,
        &["default"],
    )
    .unwrap()
}

fn recover(fs: &Arc<dyn FileSystem>) -> esker_engine::error::Result<VersionSet> {
    VersionSet::recover(Arc::clone(fs), Path::new(DIR), bytewise(), LEVELS)
}

/// Adds one file to L0 and returns the edit's file number.
fn add_file(set: &mut VersionSet, smallest: &[u8], largest: &[u8]) -> u64 {
    let number = set.new_file_number();
    let mut edit = VersionEdit::new();
    edit.add_file(
        0,
        0,
        FileMeta {
            number,
            size: 4096,
            smallest: internal_key(smallest, number, EntryKind::Put),
            largest: internal_key(largest, number, EntryKind::Put),
            smallest_seqno: number,
            largest_seqno: number,
            location: FileLocation::Local,
        },
    );
    set.log_and_apply(&mut edit).unwrap();
    number
}

#[test]
fn a_created_database_recovers_to_what_was_written() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let mut set = create(&fs);
    add_file(&mut set, b"a", b"m");
    add_file(&mut set, b"n", b"z");
    set.set_last_seqno(99);
    set.set_log_number(7);
    let mut edit = VersionEdit::new();
    set.log_and_apply(&mut edit).unwrap();
    let expected = set.current();

    let reopened = recover(&fs).unwrap();
    assert_eq!(*reopened.current(), *expected);
    assert_eq!(reopened.last_seqno(), 99);
    assert_eq!(reopened.log_number(), 7);
    assert_eq!(reopened.cf_id("default"), Some(0));
    assert!(
        reopened.next_file_number() > set.current().live_files().iter().copied().max().unwrap(),
        "file numbers must never be handed out twice"
    );
}

#[test]
fn creating_a_database_where_one_exists_is_refused() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let _set = create(&fs);
    let again = VersionSet::create(
        Arc::clone(&fs),
        Path::new(DIR),
        bytewise(),
        LEVELS,
        &["default"],
    );
    assert!(matches!(again, Err(Error::InvalidArgument(_))));
}

#[test]
fn column_families_survive_a_reopen() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let mut set = create(&fs);
    let lock = set.create_cf("lock").unwrap();
    let write = set.create_cf("write").unwrap();
    set.drop_cf("write").unwrap();

    let reopened = recover(&fs).unwrap();
    assert_eq!(reopened.cf_id("lock"), Some(lock));
    assert_eq!(reopened.cf_id("write"), None, "a dropped cf stays dropped");
    assert_eq!(reopened.cf_name(write), None);
    assert_eq!(reopened.column_families().len(), 2);
}

/// The crash matrix of `prompts/01-engine.md` step 5, one case per point in the protocol.
/// Whichever one it stops at, reopening must succeed and see the old version or the new one —
/// never neither, and never a mixture.
#[test]
fn a_crash_anywhere_in_the_current_swap_leaves_a_readable_database() {
    // Fail the temp file's creation, its sync, the rename, and the directory fsync in turn.
    // In a rolling apply the manifest is created first, so `Create` gets one call of budget.
    let cases = [
        ("manifest sync", Op::SyncData, 0),
        ("CURRENT temp create", Op::Create, 1),
        ("CURRENT temp sync", Op::SyncData, 1),
        ("CURRENT rename", Op::Rename, 0),
        ("directory fsync", Op::FsyncDir, 0),
    ];

    for (name, op, budget) in cases {
        let crash = CrashFs::new();
        let fs: Arc<dyn FileSystem> = Arc::new(crash.clone());
        let mut set = create(&fs);
        let first = add_file(&mut set, b"a", b"m");
        let old = set.current();

        // Force the next edit to roll the manifest, so it has to swap CURRENT.
        set.set_max_manifest_bytes(1);
        crash.fail_after(op, budget);

        let number = set.new_file_number();
        let mut edit = VersionEdit::new();
        edit.add_file(
            0,
            0,
            FileMeta {
                number,
                size: 4096,
                smallest: internal_key(b"n", number, EntryKind::Put),
                largest: internal_key(b"z", number, EntryKind::Put),
                smallest_seqno: number,
                largest_seqno: number,
                location: FileLocation::Local,
            },
        );
        let outcome = set.log_and_apply(&mut edit);
        assert!(
            outcome.is_err(),
            "{name}: the fault should have been reported"
        );

        // Once an update may have half-landed, the set refuses to guess.
        let mut nothing = VersionEdit::new();
        assert!(
            matches!(set.log_and_apply(&mut nothing), Err(Error::Poisoned(_))),
            "{name}: the version set should be poisoned"
        );

        crash.no_faults();
        let reopened = recover(&fs).unwrap_or_else(|err| panic!("{name}: reopen failed: {err}"));
        let files = reopened.current().live_files();
        assert!(
            files == old.live_files() || files == [first, number].into_iter().collect(),
            "{name}: saw neither the old version nor the new one: {files:?}"
        );
        assert!(files.contains(&first), "{name}: the old file was lost");
    }
}

/// After the swap succeeds, the new manifest is the live one and the old one is garbage.
#[test]
fn a_completed_swap_makes_the_new_manifest_live() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let mut set = create(&fs);
    let first = add_file(&mut set, b"a", b"m");
    let old_manifest = set.manifest_number();

    set.set_max_manifest_bytes(1);
    let second = add_file(&mut set, b"n", b"z");
    assert_ne!(set.manifest_number(), old_manifest, "the manifest rolled");

    let reopened = recover(&fs).unwrap();
    assert_eq!(
        reopened.current().live_files(),
        [first, second].into_iter().collect()
    );

    // The old manifest is no longer referenced, so a sweep reclaims it.
    let obsolete = set.obsolete_files().unwrap();
    assert!(
        obsolete.contains(&filename::manifest(Path::new(DIR), old_manifest)),
        "{obsolete:?}"
    );
}

/// A record that was being written when the process died is expected, and costs only the edit
/// that was never acknowledged.
#[test]
fn a_torn_final_manifest_record_is_tolerated() {
    let memfs = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = memfs.clone();
    let mut set = create(&fs);
    let first = add_file(&mut set, b"a", b"m");
    let before = memfs
        .contents(filename::manifest(Path::new(DIR), set.manifest_number()))
        .unwrap()
        .len();
    let second = add_file(&mut set, b"n", b"z");

    let path = filename::manifest(Path::new(DIR), set.manifest_number());
    let bytes = memfs.contents(&path).unwrap();
    assert!(bytes.len() > before);
    for cut in before..bytes.len() {
        memfs.install(&path, bytes[..cut].to_vec()).unwrap();
        let reopened = recover(&fs).unwrap_or_else(|err| panic!("cut at {cut}: {err}"));
        let files = reopened.current().live_files();
        assert!(
            files == [first].into_iter().collect()
                || files == [first, second].into_iter().collect(),
            "cut at {cut}: {files:?}"
        );
    }
}

#[test]
fn a_corrupt_manifest_record_is_an_error() {
    let memfs = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = memfs.clone();
    let mut set = create(&fs);
    add_file(&mut set, b"a", b"m");

    let path = filename::manifest(Path::new(DIR), set.manifest_number());
    let mut bytes = memfs.contents(&path).unwrap();
    bytes[8] ^= 0xFF; // inside the first record's payload
    memfs.install(&path, bytes).unwrap();

    let err = recover(&fs).unwrap_err();
    assert!(err.is_corruption(), "{err}");
}

#[test]
fn a_missing_or_malformed_current_is_not_a_database() {
    let memfs = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = memfs.clone();
    let _set = create(&fs);
    let current = filename::current(Path::new(DIR));

    memfs
        .install(&current, b"MANIFEST-000001".to_vec())
        .unwrap();
    assert!(
        recover(&fs).unwrap_err().is_corruption(),
        "no trailing newline"
    );

    memfs
        .install(&current, b"something else\n".to_vec())
        .unwrap();
    assert!(recover(&fs).unwrap_err().is_corruption());

    memfs.delete(&current).unwrap();
    assert!(matches!(recover(&fs), Err(Error::NotFound(_))));
}

#[test]
fn reopening_with_a_different_comparator_is_refused() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let _set = create(&fs);
    let reversed = Arc::new(InternalKeyComparator::new(Arc::new(ReverseComparator)));
    let err = VersionSet::recover(Arc::clone(&fs), Path::new(DIR), reversed, LEVELS).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    assert!(err.to_string().contains("ReverseComparator"), "{err}");
}

/// A version a reader still holds keeps its files alive, even after a compaction has replaced
/// them. Deleting them early is how a running scan starts reading a file that is not there.
#[test]
fn obsolete_files_respect_pinned_versions() {
    let memfs = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = memfs.clone();
    let mut set = create(&fs);
    let first = add_file(&mut set, b"a", b"m");
    let second = add_file(&mut set, b"n", b"z");
    for number in [first, second] {
        memfs
            .install(filename::sst(Path::new(DIR), number), vec![0u8; 16])
            .unwrap();
    }

    let pinned: Arc<Version> = set.current();
    assert_eq!(pinned.live_files(), [first, second].into_iter().collect());

    // A compaction drops both L0 files for one at L1.
    let merged = set.new_file_number();
    let mut edit = VersionEdit::new();
    edit.delete_file(0, 0, first);
    edit.delete_file(0, 0, second);
    edit.add_file(
        0,
        1,
        FileMeta {
            number: merged,
            size: 8192,
            smallest: internal_key(b"a", merged, EntryKind::Put),
            largest: internal_key(b"z", merged, EntryKind::Put),
            smallest_seqno: 1,
            largest_seqno: merged,
            location: FileLocation::Local,
        },
    );
    set.log_and_apply(&mut edit).unwrap();
    memfs
        .install(filename::sst(Path::new(DIR), merged), vec![0u8; 16])
        .unwrap();

    let obsolete = set.obsolete_files().unwrap();
    assert!(
        !obsolete.contains(&filename::sst(Path::new(DIR), first)),
        "a pinned version still names file {first}: {obsolete:?}"
    );
    assert_eq!(
        set.live_version_count(),
        2,
        "the reader's version is still live"
    );

    drop(pinned);
    let obsolete = set.obsolete_files().unwrap();
    for number in [first, second] {
        assert!(
            obsolete.contains(&filename::sst(Path::new(DIR), number)),
            "file {number} should be reclaimable once nothing pins it: {obsolete:?}"
        );
    }
    assert!(!obsolete.contains(&filename::sst(Path::new(DIR), merged)));
    assert_eq!(set.live_version_count(), 1);

    let deleted = set.purge_obsolete_files().unwrap();
    assert!(deleted.contains(&filename::sst(Path::new(DIR), first)));
    assert!(!memfs.exists(&filename::sst(Path::new(DIR), first)).unwrap());
    assert!(
        memfs
            .exists(&filename::sst(Path::new(DIR), merged))
            .unwrap()
    );
}

/// Log segments below the log number have been flushed; anything at or above it is still
/// needed, and files the engine did not create are not the engine's to delete.
#[test]
fn obsolete_files_keeps_live_logs_and_foreign_files() {
    let memfs = Arc::new(MemFileSystem::new());
    let fs: Arc<dyn FileSystem> = memfs.clone();
    let mut set = create(&fs);
    for number in 1..=4u64 {
        memfs
            .install(filename::wal(Path::new(DIR), number), vec![0u8; 8])
            .unwrap();
    }
    memfs
        .install(Path::new("/db/README"), vec![0u8; 8])
        .unwrap();
    memfs
        .install(filename::temp(Path::new(DIR), 9), vec![0u8; 8])
        .unwrap();

    set.set_log_number(3);
    let mut edit = VersionEdit::new();
    set.log_and_apply(&mut edit).unwrap();

    let obsolete = set.obsolete_files().unwrap();
    assert!(obsolete.contains(&filename::wal(Path::new(DIR), 1)));
    assert!(obsolete.contains(&filename::wal(Path::new(DIR), 2)));
    assert!(!obsolete.contains(&filename::wal(Path::new(DIR), 3)));
    assert!(!obsolete.contains(&filename::wal(Path::new(DIR), 4)));
    assert!(
        obsolete.contains(&filename::temp(Path::new(DIR), 9)),
        "a leftover temp file is always garbage"
    );
    assert!(
        !obsolete.contains(&PathBuf::from("/db/README")),
        "the engine does not delete files it did not create"
    );
}
