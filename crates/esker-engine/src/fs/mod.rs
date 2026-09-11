//! The filesystem seam.
//!
//! Every file the engine touches goes through [`FileSystem`] (`docs/DESIGN.md` §13). Three
//! things depend on that being true:
//!
//! * **Tiering.** Phase 6b puts SSTs in object storage. An S3-backed implementation replaces
//!   this one for SST reads; the WAL and the Raft log stay local.
//! * **Fault injection.** The crash tests need a filesystem that fails a rename, truncates a
//!   write or drops an fsync on demand. A real one cannot be asked to do that on purpose.
//! * **Simulation.** The same reason `esker-raft` takes its time and its messages from the
//!   caller: an engine that reaches for `std::fs` directly cannot be replayed.
//!
//! The three traits return [`io::Result`] rather than the crate's [`crate::Result`] so that an
//! implementation owes nothing to the engine's error type. Call sites attach the path with
//! [`crate::error::IoResultExt::at`].
//!
//! # Durability, stated honestly
//!
//! [`WritableFile::sync_data`] calls `fdatasync` (`File::sync_data`). On Linux with a
//! well-behaved device that is power-loss durability. **On macOS it is not**: `fsync` there
//! flushes to the drive without forcing the drive's own write cache, and only
//! `fcntl(F_FULLFSYNC)` does. v1 therefore targets **process-crash (`kill -9`) durability**,
//! which `fdatasync` does give on both platforms, and says so rather than pretending
//! otherwise. `F_FULLFSYNC` is a named knob to add when power-loss durability is a stated
//! goal — see `TODO(full-fsync)` below.
//!
//! # Why POSIX only
//!
//! The engine's atomicity story is `rename(2)` replacing a file atomically plus `fsync` on the
//! containing directory making that replacement durable (invariant 3). Windows has neither
//! shape, so a Windows port is a design change, not a `cfg` — and `deny.toml` builds exactly
//! two targets, both of them unix.

pub mod claim;
pub mod tier;

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(not(unix))]
compile_error!(
    "esker-engine requires POSIX rename and directory fsync semantics; \
     see the module documentation in src/fs.rs"
);

use std::os::unix::fs::FileExt;

/// The set of file operations the engine performs.
///
/// Implementations are `Send + Sync` because compaction, flush and foreground writers all use
/// one instance concurrently, and `Debug` because everything holding one — options, the
/// version set, the log writer — derives `Debug`.
pub trait FileSystem: Send + Sync + fmt::Debug {
    /// Creates `path`, truncating it if it exists, and opens it for appending.
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>>;

    /// Opens `path` for positioned reads.
    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>>;

    /// Lists `dir`, returning full paths in a deterministic order.
    ///
    /// The order is part of the contract: recovery lists a directory to find WAL segments,
    /// and a run that depends on readdir order is a run that cannot be replayed.
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;

    /// Renames `from` over `to`, atomically replacing any existing `to`.
    ///
    /// This is the engine's only mutable pointer operation (invariant 3). A reader either
    /// sees the old file or the new one, never a mixture and never neither.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Removes `path`.
    fn delete(&self, path: &Path) -> io::Result<()>;

    /// Flushes `dir`'s own metadata, making a create, rename or delete inside it durable.
    ///
    /// Renaming a file is not durable until the *directory* is synced. Skipping this is the
    /// classic way a crash loses a file that was definitely written.
    fn fsync_dir(&self, dir: &Path) -> io::Result<()>;

    /// The current size of `path` in bytes.
    fn size(&self, path: &Path) -> io::Result<u64>;

    /// Whether `path` exists. An I/O failure while looking is an error, not `false`.
    fn exists(&self, path: &Path) -> io::Result<bool>;

    /// Creates `dir` and any missing parents. Succeeds if it already exists.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;

    /// Removes `dir` and everything under it. Succeeds if it is already gone.
    ///
    /// The inverse of [`create_dir_all`](Self::create_dir_all), and the one operation that cannot
    /// be built out of [`list`](Self::list) and [`delete`](Self::delete): `delete` takes a file,
    /// `list` takes a directory, and nothing here says which a path is. It exists because a
    /// *region's* files are a tree — the columnar copy of one lives under a directory of its own —
    /// so reclaiming a region that has gone away needs a tree to go with it
    /// ([ADR 0034](../../../../docs/adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md)).
    ///
    /// **Idempotent, deliberately.** The one caller runs after a crash may have interrupted it, so
    /// "already gone" is the ordinary case and not a failure. Every other error is returned.
    ///
    /// No implementation may treat this as "empty the directory": the directory goes too, because
    /// "does this exist" is how a caller asks whether the reclamation happened.
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()>;

    /// Creates a hard link at `to` pointing at `from`.
    ///
    /// This is what makes `checkpoint` cheap: SSTs are immutable, so a checkpoint links them
    /// instead of copying them (`docs/DESIGN.md` §4.1).
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Claims `dir` for this process, for as long as the returned value lives.
    ///
    /// A data directory has exactly one writer. Nothing in the engine enforced that until this
    /// existed, and the shape that finds it is a supervisor and an operator restarting the same
    /// node: both binaries **open their database before they bind their port**, so the one that
    /// loses the port has already opened the database — two writers on one LSM tree, for as long
    /// as it takes the loser to exit.
    ///
    /// `io::ErrorKind::WouldBlock` means somebody else holds it. Any other error is the
    /// filesystem's own.
    ///
    /// No default implementation, for the reason [`WritableFile::sync_all`] gives: a default that
    /// answered `Ok` would be a claim nobody checked, in a trait whose implementations are the
    /// simulator's and the fault injector's — exactly the ones where a silent opt-out survives.
    fn lock_directory(&self, dir: &Path) -> io::Result<Box<dyn DirectoryLock>>;

    /// The object-storage tier behind this filesystem, if it has one.
    ///
    /// `None` for every implementation that is only a filesystem, which is the default and
    /// therefore the answer for [`LocalFileSystem`], the simulator's memory filesystem and the
    /// fault injector. [`tier::TieredFileSystem`] returns itself.
    ///
    /// The engine calls this at the three moments a tier needs to hear about: an SST has been
    /// written and is durable, an edit is about to be written and may carry promotions, and the
    /// obsolete-file sweep has computed which files are still live. Everything else — when to
    /// upload, what to evict, when to fetch — is the tier's own business.
    fn tier(&self) -> Option<&dyn SstTier> {
        None
    }
}

/// The object-storage tier's side of the seam.
///
/// Kept deliberately narrow. The engine knows four facts a tier cannot work out for itself —
/// a new SST exists, an edit is being written, which files are live, and that some time has
/// passed — and the tier knows everything else.
pub trait SstTier: Send + Sync + fmt::Debug {
    /// A new SST is complete and durable on local disk, and may be uploaded.
    ///
    /// Called **after** the manifest edit that names it, never before: an upload is not on the
    /// write path ([ADR 0024](../../../../docs/adr/0024-tiering-failure-semantics.md) decision 1).
    fn note_durable_sst(&self, number: u64);

    /// File numbers whose upload has completed since the last call, taken from the tier.
    ///
    /// The engine folds these into whatever edit it is about to write, as promotions. Draining
    /// is destructive: a number reported once is not reported again, and a crash before the
    /// edit lands simply means the file stays recorded `Local` — which is safe, because the
    /// read path does not consult the field (ADR 0024 decision 4).
    fn drain_promotions(&self) -> Vec<u64>;

    /// Deletes the objects of files no live version names.
    ///
    /// `live` is the union of every pinned version's file set and `pending` is the register of
    /// outputs being written. An object is deleted only when its number is in neither — the
    /// phase-1 rule, extended by one set, because an evicted file is absent from the directory
    /// listing the local sweep uses and its object would otherwise never be reclaimed.
    fn retain(&self, live: &BTreeSet<u64>, pending: &BTreeSet<u64>) -> io::Result<()>;

    /// Does one bounded pass of whatever is outstanding: uploads, fetches, eviction.
    ///
    /// Returns how many uploads succeeded. Called by the tier's own thread when it has one, and
    /// directly by tests when it does not.
    fn maintain(&self) -> usize;

    /// Whether the engine should run [`maintain`](Self::maintain) on a thread of its own.
    ///
    /// `false` makes the tier entirely caller-driven, which is what the tests want: a
    /// background thread makes "has it uploaded yet" a question only a sleep can answer.
    fn wants_background_thread(&self) -> bool;

    /// What has happened so far, for the bench and for `tracing`.
    fn stats(&self) -> tier::TierStats;
}

/// An append-only file. The WAL, the manifest and every SST are written through one.
///
/// There is no `flush` distinct from [`sync_data`](WritableFile::sync_data): callers buffer in
/// user space and hand over whole records, so an `append` that returns `Ok` has reached the
/// kernel. Whether it has reached the device is what `sync_data` answers.
pub trait WritableFile: Send {
    /// Appends `data` to the end of the file.
    fn append(&mut self, data: &[u8]) -> io::Result<()>;

    /// Makes every previously appended byte durable, at the level the module docs describe.
    fn sync_data(&mut self) -> io::Result<()>;

    /// Makes every previously appended byte **and all of this file's metadata** durable.
    ///
    /// `File::sync_all`. On Apple targets this is byte-for-byte what
    /// [`sync_data`](Self::sync_data) does — both are `fcntl(F_FULLFSYNC)`, see the module docs —
    /// and on Linux it is `fsync` where the other is `fdatasync`. Selected by
    /// [`Options::sync_call`](crate::Options::sync_call), which is off by default.
    ///
    /// No default implementation, deliberately: one that answered this by quietly doing the
    /// weaker thing would make the option a lie in the direction that costs data, and the
    /// compiler is the only reviewer that reads every implementation.
    fn sync_all(&mut self) -> io::Result<()>;
}

/// A claim on a data directory, released when it is dropped.
///
/// **Released by the kernel too.** [`LocalFileSystem`]'s holds an open file whose lock lives on
/// the open file description, so a `kill -9` releases it with every other descriptor the process
/// had. That is what makes this safe in a system whose acceptance run is fifty `SIGKILL`s: a
/// claim that outlived its holder would turn one crash into a node that can never start again.
///
/// **Within one process it is only released when the last holder lets go**, which is a different
/// and sharper rule. A `kill -9` and the restart that follows it are two processes, so the kernel
/// has already released the claim before the new one asks — `esker cluster start`'s supervisor
/// spawns children and never opens a store itself, so respawning is unaffected. What *is* affected
/// is any caller that closes a database and immediately reopens the same directory in the same
/// process: it gets `InUse` unless everything holding the engine has actually let go first, and
/// "I called `stop()`" is not the same claim — `esker-store`'s `Store::wait_for_tasks` exists
/// because an aborted `tokio` task holds what it held until the runtime drops it.
pub trait DirectoryLock: Send + Sync + fmt::Debug {}

/// A file read by position, concurrently, without a shared cursor.
pub trait RandomAccessFile: Send + Sync {
    /// Reads into `buf` starting at `offset`, returning the number of bytes read.
    ///
    /// A short read is not an error: it means end of file. Callers that need exactly `n`
    /// bytes loop, or treat a short read as corruption when the format promised more.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// The file's size in bytes.
    fn size(&self) -> io::Result<u64>;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalFileSystem;

impl LocalFileSystem {
    /// A handle to the local filesystem. Stateless, so this is free.
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for LocalFileSystem {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Box::new(LocalWritableFile { file }))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        let file = File::open(path)?;
        Ok(Box::new(LocalRandomAccessFile { file }))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(dir)? {
            paths.push(entry?.path());
        }
        // readdir order is whatever the filesystem feels like; recovery must not depend on it.
        paths.sort();
        Ok(paths)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        // Opening a directory read-only and syncing it is the portable POSIX way to make the
        // renames and creations inside it durable.
        File::open(dir)?.sync_all()
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.len())
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }

    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        match fs::remove_dir_all(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(from, to)
    }

    fn lock_directory(&self, dir: &Path) -> io::Result<Box<dyn DirectoryLock>> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        // Not truncated on the way in: whoever holds it is still holding it, and emptying their
        // record before finding out whether we can have the lock would erase the one thing that
        // says who to look for. `try_lock` and not `lock`, because a second node blocking for
        // ever on a directory reads as a hang.
        match file.try_lock() {
            Ok(()) => {
                write_holder(&file);
                Ok(Box::new(LocalDirectoryLock { _file: file }))
            }
            Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is held by another process", path.display()),
            )),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

/// The name of the file [`LocalFileSystem::lock_directory`] locks, and the note in it.
///
/// Uppercase like `CURRENT`, and — like every name the engine does not recognise —
/// [`crate::filename::classify`] answers `None` for it, which is what keeps the obsolete-file
/// sweep from deleting the lock out from under its holder.
///
/// Its **contents** are one line naming the holder — `pid=… exe=… since_unix=…` — written after
/// the lock is taken and read by whoever is refused. A note and never an authority: the lock
/// decides, and this only answers the question a refusal used to leave open.
pub const LOCK_FILE: &str = "LOCK";

/// Records who holds the claim, in the file the claim is held on.
///
/// **The lock is the authority and this is only a note**, which is what makes it safe: nothing
/// reads this to decide anything, so a stale or truncated line costs a reader nothing but the
/// diagnosis. It is written after the lock is taken, so no two processes can be writing it, and a
/// failure to write it is ignored — a claim that failed because its note could not be written
/// would be a worse trade than an unexplained refusal.
///
/// It exists because a gate archived `InUse { dir: "/tmp/.tmpe1uNGX" }` and nothing more, and the
/// next question — *who had it* — had no answer anywhere.
fn write_holder(file: &File) {
    use std::io::{Seek, SeekFrom, Write};

    let pid = std::process::id();
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "?".to_owned());
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let line = format!("pid={pid} exe={exe} since_unix={since}\n");
    let mut file = file;
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

/// A claim on a local directory: an open file whose kernel lock is released when it closes.
#[derive(Debug)]
struct LocalDirectoryLock {
    _file: File,
}

impl DirectoryLock for LocalDirectoryLock {}

/// An appendable file on the local filesystem.
#[derive(Debug)]
struct LocalWritableFile {
    file: File,
}

impl WritableFile for LocalWritableFile {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        // On Apple targets `std` makes this `fcntl(F_FULLFSYNC)` itself, so this *is* the full
        // fsync the module header used to say was missing. See there for the citation.
        self.file.sync_data()
    }

    fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// A positioned-read file on the local filesystem.
#[derive(Debug)]
struct LocalRandomAccessFile {
    file: File,
}

impl RandomAccessFile for LocalRandomAccessFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        // `pread`: no shared cursor, so concurrent readers of one SST do not interfere.
        self.file.read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

/// Reads exactly `buf.len()` bytes at `offset`, or reports how far it got.
///
/// Most engine reads know the length they want from an index or a footer, and a short read
/// there means the file is truncated — corruption, not end of input.
pub fn read_exact_at(file: &dyn RandomAccessFile, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(offset + done as u64, &mut buf[done..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("wanted {} bytes at offset {offset}, got {done}", buf.len()),
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FileSystem, LocalFileSystem, read_exact_at};

    #[test]
    fn write_sync_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        let path = dir.path().join("000001.wal");

        let mut w = fs.create(&path).unwrap();
        w.append(b"hello ").unwrap();
        w.append(b"world").unwrap();
        w.sync_data().unwrap();
        drop(w);

        assert_eq!(fs.size(&path).unwrap(), 11);
        let r = fs.open(&path).unwrap();
        assert_eq!(r.size().unwrap(), 11);

        let mut buf = [0u8; 5];
        read_exact_at(r.as_ref(), 6, &mut buf).unwrap();
        assert_eq!(&buf, b"world");
    }

    #[test]
    fn create_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        let path = dir.path().join("f");

        let mut w = fs.create(&path).unwrap();
        w.append(b"0123456789").unwrap();
        drop(w);
        let mut w = fs.create(&path).unwrap();
        w.append(b"ab").unwrap();
        drop(w);

        assert_eq!(fs.size(&path).unwrap(), 2);
    }

    #[test]
    fn short_read_at_eof_is_not_an_error_but_read_exact_at_fails() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        let path = dir.path().join("f");
        let mut w = fs.create(&path).unwrap();
        w.append(b"abc").unwrap();
        drop(w);

        let r = fs.open(&path).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(r.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(r.read_at(3, &mut buf).unwrap(), 0);
        assert!(read_exact_at(r.as_ref(), 0, &mut buf).is_err());
    }

    #[test]
    fn rename_replaces_and_list_is_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        for name in ["c", "a", "b"] {
            let mut w = fs.create(&dir.path().join(name)).unwrap();
            w.append(name.as_bytes()).unwrap();
        }
        let listed: Vec<_> = fs
            .list(dir.path())
            .unwrap()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(listed, ["a", "b", "c"]);

        fs.rename(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap();
        assert!(!fs.exists(&dir.path().join("a")).unwrap());
        assert_eq!(fs.size(&dir.path().join("b")).unwrap(), 1);
        fs.fsync_dir(dir.path()).unwrap();
    }

    #[test]
    fn hard_link_shares_content_and_survives_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        let src = dir.path().join("000007.sst");
        let dst = dir.path().join("checkpoint-000007.sst");
        let mut w = fs.create(&src).unwrap();
        w.append(b"immutable").unwrap();
        drop(w);

        fs.hard_link(&src, &dst).unwrap();
        fs.delete(&src).unwrap();
        assert_eq!(fs.size(&dst).unwrap(), 9);
    }

    #[test]
    fn create_dir_all_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new();
        let nested = dir.path().join("a/b/c");
        fs.create_dir_all(&nested).unwrap();
        fs.create_dir_all(&nested).unwrap();
        assert!(fs.exists(&nested).unwrap());
    }
}
