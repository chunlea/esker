//! What [`WalSyncMode`] actually decides, counted rather than described.
//!
//! The mode is a policy with three settings and exactly one line of code reading it
//! (`crates/esker-engine/src/db/write.rs`), so what it does is not obvious from either end. This
//! file counts `sync_data` calls on the write-ahead log through a filesystem that does nothing but
//! count, which is the only way to say what a durability knob is worth: a benchmark can be slow
//! for a dozen reasons and a comment can be wrong for one.
//!
//! It exists because `docs/bench/columnar-learner.md` recorded 4.8 ms per single-row put "with
//! sync switched off" and could not explain it. The switch was not off. See
//! `docs/plans/debt-c3.md` §4.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_engine::memfs::MemFileSystem;
use esker_engine::{
    Db, DirectoryLock, FileSystem, Options, RandomAccessFile, SyncCall, WalSyncMode, WritableFile,
    WriteOptions, cf,
};

/// A filesystem that counts `sync_data` per path and otherwise gets out of the way.
///
/// Counting is the whole of it. A fault injector would answer a different question — what happens
/// when a sync fails — and the question here is whether one happens at all.
#[derive(Debug)]
struct CountingFs {
    inner: Arc<MemFileSystem>,
    syncs: Arc<Mutex<BTreeMap<PathBuf, u64>>>,
    appends: Arc<AtomicU64>,
    /// Of those syncs, how many were the **full** one — `sync_all` rather than `sync_data`.
    full: Arc<AtomicU64>,
}

impl CountingFs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemFileSystem::new()),
            syncs: Arc::new(Mutex::new(BTreeMap::new())),
            appends: Arc::new(AtomicU64::new(0)),
            full: Arc::new(AtomicU64::new(0)),
        })
    }

    /// How many times the write-ahead log — and only the log — was synced.
    ///
    /// By extension, not by exact name: a log segment is `NNNNNN.wal`
    /// ([`esker_engine::filename`]) and rolls, so a test that named one segment would stop
    /// counting the moment the log rolled underneath it.
    fn wal_syncs(&self) -> u64 {
        self.syncs
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "wal"))
            .map(|(_, count)| *count)
            .sum()
    }

    fn wal_appends(&self) -> u64 {
        self.appends.load(Ordering::Relaxed)
    }

    /// How many of the syncs were `sync_all`.
    fn full_syncs(&self) -> u64 {
        self.full.load(Ordering::Relaxed)
    }
}

impl FileSystem for CountingFs {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        Ok(Box::new(CountingFile {
            inner: self.inner.create(path)?,
            path: path.to_path_buf(),
            syncs: Arc::clone(&self.syncs),
            appends: Arc::clone(&self.appends),
            full: Arc::clone(&self.full),
            is_log: path.extension().is_some_and(|ext| ext == "wal"),
        }))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        self.inner.open(path)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        self.inner.delete(path)
    }
    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
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

    fn lock_directory(&self, dir: &Path) -> io::Result<Box<dyn DirectoryLock>> {
        self.inner.lock_directory(dir)
    }
}

struct CountingFile {
    inner: Box<dyn WritableFile>,
    path: PathBuf,
    syncs: Arc<Mutex<BTreeMap<PathBuf, u64>>>,
    appends: Arc<AtomicU64>,
    full: Arc<AtomicU64>,
    is_log: bool,
}

impl std::fmt::Debug for CountingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingFile").finish_non_exhaustive()
    }
}

impl WritableFile for CountingFile {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        if self.is_log {
            self.appends.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.append(data)
    }
    fn sync_data(&mut self) -> io::Result<()> {
        *self
            .syncs
            .lock()
            .unwrap()
            .entry(self.path.clone())
            .or_default() += 1;
        self.inner.sync_data()
    }

    fn sync_all(&mut self) -> io::Result<()> {
        self.full.fetch_add(1, Ordering::Relaxed);
        *self
            .syncs
            .lock()
            .unwrap()
            .entry(self.path.clone())
            .or_default() += 1;
        self.inner.sync_all()
    }
}

fn open(fs: &Arc<CountingFs>, mode: WalSyncMode) -> Db {
    open_with_sync_call(fs, mode, SyncCall::Data)
}

fn open_with_sync_call(fs: &Arc<CountingFs>, mode: WalSyncMode, sync_call: SyncCall) -> Db {
    let options = Options {
        create_if_missing: true,
        wal_sync_mode: mode,
        sync_call,
        ..Options::default()
    };
    Db::open_with(
        "/db",
        options,
        Arc::clone(fs) as Arc<dyn FileSystem>,
        &[cf::DEFAULT],
    )
    .unwrap()
}

/// Twenty puts through one writer, one at a time, and how many log syncs they cost.
fn puts(db: &Db, count: u32, options: WriteOptions) {
    for n in 0..count {
        let id = db.cf_id(cf::DEFAULT).unwrap();
        let mut batch = esker_engine::WriteBatch::new();
        batch.put(id, &Bytes::from(format!("k{n:05}")), b"v");
        db.write(batch, &options).unwrap();
    }
}

const PUTS: u32 = 20;

/// `WalSyncMode::PerWrite` syncs the log once per commit group, which with one writer and one put
/// at a time is once per put. The control: it says the counter works.
#[test]
fn per_write_syncs_every_group() {
    let fs = CountingFs::new();
    let db = open(&fs, WalSyncMode::PerWrite);
    let before = fs.wal_syncs();
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(
        fs.wal_syncs() - before,
        u64::from(PUTS),
        "one sync per group is what this mode names"
    );
}

/// **The finding.** `WalSyncMode::Never` does not stop a default write syncing, because
/// `WriteOptions::default()` is `sync: true` and the write path takes the *union* of the two:
///
/// ```text
/// let sync = options.sync || self.options.wal_sync_mode == WalSyncMode::PerWrite;
/// ```
///
/// The mode can therefore only ever **add** syncing, never remove it, and every caller that has
/// not gone out of its way to say `sync: false` — `put_cf`, `delete_cf`, and every
/// `WriteOptions::default()` in the tree — asks for one. So a database opened `Never` syncs on
/// every single write, which is what `docs/bench/columnar-learner.md` was measuring at 4.8 ms a
/// put while reporting that sync was off.
///
/// This test asserts the behaviour that is **wanted**, so it fails on the code as it was and
/// passes on the code as it is: a mode that says "never" does not sync a write that did not
/// explicitly demand it.
#[test]
fn never_does_not_sync_a_write_that_only_took_the_default() {
    let fs = CountingFs::new();
    let db = open(&fs, WalSyncMode::Never);
    let before = fs.wal_syncs();
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(
        fs.wal_syncs() - before,
        0,
        "a mode that names itself `Never` synced {PUTS} default writes"
    );
    // And the bytes still reached the log: what is disabled is the durability wait, not the write.
    assert!(
        fs.wal_appends() >= u64::from(PUTS),
        "the records never reached the log at all"
    );
}

/// An explicit `sync: true` is still honoured under `Never`. That is the half of the contract the
/// mode's own documentation states — "never sync **except when a write asks**" — and the half
/// invariant 1 rests on: a caller that demanded durability gets it whatever the policy is.
#[test]
fn an_explicit_demand_is_honoured_whatever_the_mode_says() {
    let fs = CountingFs::new();
    let db = open(&fs, WalSyncMode::Never);
    let before = fs.wal_syncs();
    puts(&db, PUTS, WriteOptions::synced());
    assert_eq!(
        fs.wal_syncs() - before,
        u64::from(PUTS),
        "a write that asked for durability did not get it"
    );
}

/// `WalSyncMode::Interval` promises "sync in the background at this interval". Nothing reads it —
/// the one line that reads the mode compares it against `PerWrite` and nothing else — so before
/// this it was `Never` wearing a different name, and a database configured for bounded loss had
/// unbounded loss.
///
/// The interval is now a real background syncer, and this asserts the property that separates the
/// two modes: a write that asked for nothing is not synced on its own thread, and *is* synced
/// without anybody asking again.
/// How long the background syncer gets to make its first sync before that is a failure.
///
/// **A precondition, not a measurement.** The interval under test is ten milliseconds, so what the
/// wait is for takes one of them; the bound exists so that "there is no background syncer at all"
/// fails instead of hanging, and it is sized for a box that cannot schedule a thread for a while
/// rather than for the one it was written on, where it was ten seconds.
const SYNCS_WITHIN: Duration = Duration::from_secs(60);

#[test]
fn interval_syncs_in_the_background_without_the_writer_waiting() {
    let fs = CountingFs::new();
    let db = open(&fs, WalSyncMode::Interval(Duration::from_millis(10)));
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(
        fs.wal_syncs(),
        0,
        "the writer waited for a sync under a mode whose whole point is that it does not"
    );

    let began = Instant::now();
    while fs.wal_syncs() == 0 {
        assert!(
            began.elapsed() < SYNCS_WITHIN,
            "an interval of 10 ms produced no background sync in {:?}",
            began.elapsed()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(db);
}

/// Closing the database syncs the log, whatever the mode.
///
/// Without it, `Never` and `Interval` would lose the tail of an *orderly* shutdown — which is not
/// a crash and is not what either mode trades away. The trade is "a crash may lose recent writes",
/// never "a clean close may".
#[test]
fn a_clean_close_syncs_what_the_mode_deferred() {
    let fs = CountingFs::new();
    let db = open(&fs, WalSyncMode::Never);
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(fs.wal_syncs(), 0, "the writes were synced before the close");
    drop(db);
    assert!(
        fs.wal_syncs() >= 1,
        "a clean close left the log unsynced, so an orderly shutdown can lose writes"
    );
}

/// The `sync_call` option reaches the write-ahead log's sync, and picks the other call.
///
/// # Why this is a wiring test and not a durability one
///
/// It cannot assert durability, because on this platform there is none to tell apart: `std` issues
/// `fcntl(F_FULLFSYNC)` for **both** `sync_data` and `sync_all` on Apple targets
/// (`library/std/src/sys/fs/unix.rs`, cited in `esker_engine::fs`'s module docs), so the two calls
/// are byte-for-byte the same syscall here. On Linux they are `fdatasync` and `fsync`, and the
/// difference is the inode's other metadata.
///
/// So what this pins is the only thing a test on either platform *can* pin, and the thing that
/// actually rots: that the option still reaches the call site and still changes which call is
/// made. An option wired to nothing is what the `TODO` this replaced was afraid of, and it is
/// exactly what would happen silently if the log's sync were ever rewritten.
#[test]
fn a_sync_chooses_by_option() {
    let off = CountingFs::new();
    let db = open_with_sync_call(&off, WalSyncMode::PerWrite, SyncCall::Data);
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(off.wal_syncs(), u64::from(PUTS), "the control did not sync");
    assert_eq!(
        off.full_syncs(),
        0,
        "the log took the stronger call with the option off"
    );
    drop(db);

    let on = CountingFs::new();
    let db = open_with_sync_call(&on, WalSyncMode::PerWrite, SyncCall::All);
    puts(&db, PUTS, WriteOptions::default());
    assert_eq!(
        on.full_syncs(),
        u64::from(PUTS),
        "the option is on and the log still took `sync_data`"
    );
    drop(db);
}
