//! What a crash costs under each [`WalSyncMode`], and what it may never cost.
//!
//! `tests/wal_sync.rs` counts `sync_data` calls, which says whether the mode is *read*. It cannot
//! say what a crash then loses, and that is the question the mode exists to answer. `dd182cb`
//! turned `WriteOptions`'s `bool` into a [`Durability`] tri-state precisely because a bool could
//! not say "no opinion", and left the crash half unwritten:
//!
//! > `Interval(d)` was worse: nothing read that variant at all, so a database configured for
//! > BOUNDED loss had unbounded loss and said nothing about it.
//!
//! "Bounded" is a claim about a crash. This file is that claim, checked.
//!
//! # The crash model, and why it is not `SIGKILL` here
//!
//! [`MemFileSystem::lose_unsynced`] discards every byte written since the file's last
//! `sync_data` — a **power loss**, and the strictly harsher of the two models `src/fs.rs` names.
//! It is used rather than a subprocess kill because the claim being checked is about an exact
//! set of writes, and a `SIGKILL` lands where the scheduler puts it: under `Interval`, "the last
//! interval's writes" would be whatever happened to be in flight, on a box shared with three
//! other agents. `tests/crash_kill.rs` covers the real-signal side and this covers the exact one,
//! which is the same division of labour `crash_faultfs.rs` describes.
//!
//! A page-cache crash — `kill -9`, where unsynced bytes survive — is the weaker model and every
//! assertion here holds under it a fortiori: it is a superset of what survives a power loss.
//!
//! # The three claims
//!
//! 1. **A `Durable` write survives, whatever the mode.** `CLAUDE.md` invariant 1, and the one
//!    assertion that must hold on `Never` as loudly as on `PerWrite`.
//! 2. **`Interval(d)` bounds the loss.** Everything acknowledged before the log's last periodic
//!    sync is still there. Before `dd182cb` nothing read that variant, so the bound was the whole
//!    database.
//! 3. **A write with no opinion is the one the mode decides.** On a `Never` database it can be
//!    lost — and the test asserts one *was*, because a suite where nothing is ever lost would
//!    pass against a database that syncs everything, which is exactly what this one did before
//!    `dd182cb`. `Policy` and not `Buffered`: an explicit opt-out is a different claim, and it
//!    has its own test.
//!
//! # What is red before `dd182cb`, and what is not
//!
//! Run against the old rule — `sync || mode == PerWrite`, with a default of `sync: true` — three
//! of the five fail: the `Interval` bound, the no-opinion loss, and the explicit `Buffered` write
//! on a `PerWrite` database. The two that stay green are the two that should:
//! `a_durable_write_survives_a_power_loss_under_every_mode` is invariant 1, which the old rule
//! also upheld (it could only ever *add* syncing), and `an_orderly_shutdown_syncs_what_the_mode_
//! deferred` passes trivially on a database that syncs everything. They are guards, not
//! regressions, and they are here because a future change could break either.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esker_engine::memfs::MemFileSystem;
use esker_engine::{
    Db, Durability, FileSystem, Options, RandomAccessFile, ReadOptions, WalSyncMode, WritableFile,
    WriteOptions, cf,
};

const DIR: &str = "/db";

/// A filesystem that counts write-ahead-log syncs and can lose what was never synced.
///
/// The counting is what makes the `Interval` bound checkable rather than timed: the test waits
/// for the sync *count* to move rather than for a duration to elapse, so a starved background
/// thread on a loaded machine makes the test slow instead of making it lie.
struct CrashFs {
    inner: Arc<MemFileSystem>,
    syncs: Arc<Mutex<BTreeMap<PathBuf, u64>>>,
    wal_syncs: Arc<AtomicU64>,
}

impl std::fmt::Debug for CrashFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrashFs")
            .field("wal_syncs", &self.wal_syncs())
            .finish_non_exhaustive()
    }
}

impl CrashFs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemFileSystem::new()),
            syncs: Arc::new(Mutex::new(BTreeMap::new())),
            wal_syncs: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Syncs of any `*.wal`, by extension rather than by name — a log segment rolls, and a test
    /// that named one would stop counting when it did.
    fn wal_syncs(&self) -> u64 {
        self.wal_syncs.load(Ordering::Relaxed)
    }

    /// The power loss. Everything written since each file's last `sync_data` is gone.
    fn power_loss(&self) {
        self.inner.lose_unsynced().unwrap();
    }
}

struct CrashFile {
    inner: Box<dyn WritableFile>,
    path: PathBuf,
    syncs: Arc<Mutex<BTreeMap<PathBuf, u64>>>,
    wal_syncs: Arc<AtomicU64>,
    is_log: bool,
}

impl WritableFile for CrashFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }
    fn sync_data(&mut self) -> io::Result<()> {
        *self
            .syncs
            .lock()
            .unwrap()
            .entry(self.path.clone())
            .or_default() += 1;
        if self.is_log {
            self.wal_syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.sync_data()
    }
    fn sync_all(&mut self) -> io::Result<()> {
        *self
            .syncs
            .lock()
            .unwrap()
            .entry(self.path.clone())
            .or_default() += 1;
        if self.is_log {
            self.wal_syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.sync_all()
    }
}

impl FileSystem for CrashFs {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        Ok(Box::new(CrashFile {
            inner: self.inner.create(path)?,
            path: path.to_path_buf(),
            syncs: Arc::clone(&self.syncs),
            wal_syncs: Arc::clone(&self.wal_syncs),
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
}

fn key(n: u32) -> Vec<u8> {
    format!("key-{n:05}").into_bytes()
}

fn options(mode: WalSyncMode) -> Options {
    Options {
        create_if_missing: true,
        wal_sync_mode: mode,
        // Large, so nothing here is flushed into an SST: a flush syncs the log's successor and
        // would make writes durable for a reason that has nothing to do with the mode.
        cf_options: esker_engine::options::CfOptions {
            write_buffer_size: 64 * 1024 * 1024,
            ..esker_engine::options::CfOptions::default()
        },
        ..Options::default()
    }
}

fn open(fs: &Arc<CrashFs>, mode: WalSyncMode) -> Db {
    Db::open_with(
        DIR,
        options(mode),
        Arc::clone(fs) as Arc<dyn FileSystem>,
        &[cf::DEFAULT],
    )
    .unwrap()
}

fn put(db: &Db, n: u32, durability: Durability) {
    let id = db.cf_id(cf::DEFAULT).unwrap();
    let mut batch = esker_engine::WriteBatch::new();
    batch.put(id, &key(n), b"v");
    db.write(batch, &WriteOptions { durability }).unwrap();
}

/// Which of `0..count` survived, read back from a database reopened on the damaged filesystem.
///
/// **Reopened without closing.** A clean close syncs whatever the mode deferred — deliberately,
/// since these modes trade durability against a crash and an orderly shutdown is not one — so a
/// test that dropped the `Db` first would be measuring the shutdown path and would pass whatever
/// the mode did.
fn survivors(fs: &Arc<CrashFs>, mode: WalSyncMode, count: u32) -> Vec<u32> {
    let db = open(fs, mode);
    let read = ReadOptions::default();
    let alive = (0..count)
        .filter(|n| db.get(cf::DEFAULT, &key(*n), &read).unwrap().is_some())
        .collect();
    std::mem::forget(db);
    alive
}

/// Waits for the log to be synced at least `target` times, or gives up loudly.
///
/// A count rather than a sleep: `Interval(d)` is a background thread, and on a loaded machine it
/// can be late. Waiting for the effect makes lateness slow; waiting for the duration makes it a
/// false failure.
fn wait_for_wal_syncs(fs: &Arc<CrashFs>, target: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while fs.wal_syncs() < target {
        assert!(
            Instant::now() < deadline,
            "the log was synced {} times in 30s, waiting for {target}. Under Interval that is a \
             background thread that never ran; under any other mode this helper is being misused.",
            fs.wal_syncs()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Claim 1: a `Durable` write is durable, whatever the database was opened with.
#[test]
fn a_durable_write_survives_a_power_loss_under_every_mode() {
    for mode in [
        WalSyncMode::PerWrite,
        WalSyncMode::Interval(Duration::from_millis(50)),
        WalSyncMode::Never,
    ] {
        let fs = CrashFs::new();
        {
            let db = open(&fs, mode);
            // Interleaved, so a mode that syncs in lumps cannot make the durable ones survive by
            // accident of position: every `Durable` write has a `Buffered` one on each side.
            for n in 0..30 {
                let durability = match n % 3 {
                    0 => Durability::Durable,
                    1 => Durability::Buffered,
                    _ => Durability::Policy,
                };
                put(&db, n, durability);
            }
            std::mem::forget(db);
        }
        fs.power_loss();

        let alive = survivors(&fs, mode, 30);
        for n in (0..30).filter(|n| n % 3 == 0) {
            assert!(
                alive.contains(&n),
                "{mode:?}: acknowledged Durable write {n} did not survive a power loss. That is \
                 CLAUDE.md invariant 1, and Durability::Durable outranks every mode."
            );
        }
    }
}

/// Claim 2: `Interval(d)` bounds what a crash costs.
#[test]
fn interval_makes_the_loss_bounded_rather_than_total() {
    const EARLY: u32 = 20;
    const LATE: u32 = 20;
    let mode = WalSyncMode::Interval(Duration::from_millis(20));
    let fs = CrashFs::new();
    {
        let db = open(&fs, mode);
        for n in 0..EARLY {
            put(&db, n, Durability::Policy);
        }
        // Wait for the periodic sync to have happened *after* those writes rather than for a
        // duration to have passed. Two, so that one that fired before the batch landed does not
        // count.
        let seen = fs.wal_syncs();
        wait_for_wal_syncs(&fs, seen + 2);

        for n in EARLY..EARLY + LATE {
            put(&db, n, Durability::Policy);
        }
        std::mem::forget(db);
    }
    fs.power_loss();

    let alive = survivors(&fs, mode, EARLY + LATE);
    for n in 0..EARLY {
        assert!(
            alive.contains(&n),
            "write {n} was acknowledged before a periodic sync and did not survive. Interval(d) \
             promises bounded loss; before dd182cb nothing read the variant at all and the bound \
             was the whole database."
        );
    }
    // The late writes may or may not be there — another interval may have elapsed while they were
    // being written — so nothing is asserted about them. What is asserted is that the mode is not
    // secretly PerWrite, which would make the bound meaningless in the other direction.
    assert!(
        fs.wal_syncs() < u64::from(EARLY + LATE),
        "the log was synced {} times for {} writes, which is per-write syncing wearing \
         Interval's name",
        fs.wal_syncs(),
        EARLY + LATE
    );
}

/// Claim 3: on a `Never` database a write with **no opinion** can be lost, and a `Durable` one
/// beside it cannot.
///
/// `Policy` rather than `Buffered` for the writes expected to go, and that choice is the whole
/// red-first value of this test. Before `dd182cb` a caller *could* already say `sync: false` and
/// get a buffered write — what it could not do was leave the decision to the database:
/// `WriteOptions::default()` said `sync: true` and the write path took the union, so
/// `WalSyncMode::Never` "was a no-op for every caller that had not gone out of its way to pass
/// `sync: false`". A test built from explicit `Buffered` writes therefore passes against the bug;
/// one built from `Policy` writes does not.
#[test]
fn a_write_with_no_opinion_can_be_lost_on_a_never_database() {
    const COUNT: u32 = 60;
    let mode = WalSyncMode::Never;
    let fs = CrashFs::new();
    {
        let db = open(&fs, mode);
        for n in 0..COUNT {
            put(
                &db,
                n,
                if n % 10 == 0 {
                    Durability::Durable
                } else {
                    Durability::Policy
                },
            );
        }
        std::mem::forget(db);
    }
    fs.power_loss();

    let alive = survivors(&fs, mode, COUNT);
    let lost: Vec<u32> = (0..COUNT).filter(|n| !alive.contains(n)).collect();

    for n in (0..COUNT).filter(|n| n % 10 == 0) {
        assert!(
            alive.contains(&n),
            "Durable write {n} was lost on a Never database. Never is what the *policy* says, and \
             a caller that asked for durability outranks the policy — invariant 1."
        );
    }
    // **The half that makes the other half worth having.** An assertion that only checked the
    // durable writes would pass against a database that syncs everything, which is exactly what
    // this one did before `dd182cb`.
    assert!(
        !lost.is_empty(),
        "nothing was lost on a Never database whose {} writes expressed no durability preference. \
         Either the mode is being ignored — the bug dd182cb fixed — or something else made the \
         log durable and this test is measuring that instead.",
        COUNT - COUNT / 10
    );
    assert!(
        lost.iter().all(|n| n % 10 != 0),
        "a Durable write is in the lost set: {lost:?}"
    );
}

/// And a write that asked to be buffered is buffered even on a `PerWrite` database.
///
/// This is the other end of the precedence rule, and it is red before `dd182cb` too — which I had
/// written down as green until the red run said otherwise. The old line was
/// `options.sync || mode == PerWrite`, **an OR**, so on a `PerWrite` database the mode added
/// syncing back to a write that had explicitly declined it: `Buffered` was only honoured on the
/// modes that were not going to sync anyway. `dd182cb`'s own summary says it in one clause — "an
/// OR, so the mode could only ever add syncing and never remove it" — and reading that as "an
/// explicit `sync: false` always worked" is a mistake this test now prevents.
#[test]
fn an_explicit_buffered_write_is_still_buffered() {
    const COUNT: u32 = 40;
    let mode = WalSyncMode::PerWrite;
    let fs = CrashFs::new();
    {
        let db = open(&fs, mode);
        for n in 0..COUNT {
            put(&db, n, Durability::Buffered);
        }
        std::mem::forget(db);
    }
    // On `PerWrite`, and still buffered: the mode is what a write with no opinion gets, and these
    // have one. What a `Buffered` write cannot do is un-sync a group it shares with a synced
    // write — hence one writer and one write per group here.
    assert_eq!(
        fs.wal_syncs(),
        0,
        "a Buffered write synced the log on a PerWrite database"
    );
}

/// A clean close is not a crash, and the modes trade durability against crashes only.
#[test]
fn an_orderly_shutdown_syncs_what_the_mode_deferred() {
    let fs = CrashFs::new();
    {
        let db = open(&fs, WalSyncMode::Never);
        for n in 0..20 {
            put(&db, n, Durability::Buffered);
        }
        // Dropped rather than forgotten: this is the one case that closes the database.
        drop(db);
    }
    fs.power_loss();

    let alive = survivors(&fs, WalSyncMode::Never, 20);
    assert_eq!(
        alive.len(),
        20,
        "a database closed cleanly lost writes. These modes trade durability away for a *crash*, \
         and a shutdown is not one — dd182cb: \"a clean close syncs whatever the mode deferred\"."
    );
}
