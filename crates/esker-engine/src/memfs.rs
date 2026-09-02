//! An in-memory [`FileSystem`], for tests and for the simulator.
//!
//! `docs/DESIGN.md` §13 asks for the filesystem to be a trait partly so that the crash tests
//! have somewhere to inject faults. This is the substrate they inject into: files are
//! `Vec<u8>` behind a mutex, so a test can write a log, truncate it at every byte offset, flip
//! every bit and re-read it thousands of times without touching a disk.
//!
//! It models durability the way a real one does, which is what makes it useful:
//! [`MemFileSystem::sync_len`] remembers how many bytes of each file were durable at the last
//! `sync_data`, and [`MemFileSystem::lose_unsynced`] discards everything after that — a power
//! loss. A `kill -9` loses nothing, because unsynced bytes are already in the page cache; that
//! difference is why the two are modelled separately.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::fs::{FileSystem, RandomAccessFile, WritableFile};

/// One file's bytes, shared by every handle and hard link to it.
type Shared = Arc<Mutex<FileData>>;

#[derive(Debug, Default)]
struct FileData {
    bytes: Vec<u8>,
    /// Bytes durable as of the last `sync_data`.
    synced: usize,
}

#[derive(Debug, Default)]
struct Inner {
    files: BTreeMap<PathBuf, Shared>,
    dirs: BTreeSet<PathBuf>,
}

/// A filesystem that lives in memory.
#[derive(Debug, Clone, Default)]
pub struct MemFileSystem {
    inner: Arc<Mutex<Inner>>,
}

/// Turns a poisoned lock into an I/O error rather than a panic. A poisoned lock means another
/// test thread panicked; that is a test failure to report, not one to add a second panic to.
fn poisoned() -> io::Error {
    io::Error::other("in-memory filesystem lock poisoned")
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", path.display()),
    )
}

impl MemFileSystem {
    /// An empty filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// The full contents of `path`, durable or not.
    pub fn contents(&self, path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        let file = inner
            .files
            .get(path.as_ref())
            .ok_or_else(|| not_found(path.as_ref()))?;
        let data = file.lock().map_err(|_| poisoned())?;
        Ok(data.bytes.clone())
    }

    /// How many bytes of `path` were durable at its last `sync_data`.
    pub fn sync_len(&self, path: impl AsRef<Path>) -> io::Result<usize> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        let file = inner
            .files
            .get(path.as_ref())
            .ok_or_else(|| not_found(path.as_ref()))?;
        let data = file.lock().map_err(|_| poisoned())?;
        Ok(data.synced)
    }

    /// Replaces `path` with exactly these bytes, creating it if needed.
    ///
    /// This is how a test plants a truncated or bit-flipped file: build the good one, take
    /// [`contents`](Self::contents), damage the copy, install it back.
    pub fn install(&self, path: impl AsRef<Path>, bytes: Vec<u8>) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let synced = bytes.len();
        inner.files.insert(
            path.as_ref().to_path_buf(),
            Arc::new(Mutex::new(FileData { bytes, synced })),
        );
        Ok(())
    }

    /// Discards every byte of every file that was not durable — a power loss, not a `kill -9`.
    pub fn lose_unsynced(&self) -> io::Result<()> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        for file in inner.files.values() {
            let mut data = file.lock().map_err(|_| poisoned())?;
            let synced = data.synced;
            data.bytes.truncate(synced);
        }
        Ok(())
    }

    fn shared(&self, path: &Path) -> io::Result<Shared> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        inner
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| not_found(path))
    }
}

impl FileSystem for MemFileSystem {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let shared: Shared = Arc::new(Mutex::new(FileData::default()));
        inner.files.insert(path.to_path_buf(), Arc::clone(&shared));
        Ok(Box::new(MemWritableFile { shared }))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        Ok(Box::new(MemRandomAccessFile {
            shared: self.shared(path)?,
        }))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        // `files` is a BTreeMap, so this is already in a deterministic order.
        Ok(inner
            .files
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .cloned()
            .collect())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let shared = inner.files.remove(from).ok_or_else(|| not_found(from))?;
        inner.files.insert(to.to_path_buf(), shared);
        Ok(())
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        inner.files.remove(path).ok_or_else(|| not_found(path))?;
        Ok(())
    }

    fn fsync_dir(&self, _dir: &Path) -> io::Result<()> {
        // Directory entries are never lost here: there is no metadata to flush.
        Ok(())
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        let shared = self.shared(path)?;
        let data = shared.lock().map_err(|_| poisoned())?;
        Ok(data.bytes.len() as u64)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        Ok(inner.files.contains_key(path) || inner.dirs.contains(path))
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let mut path = dir.to_path_buf();
        loop {
            inner.dirs.insert(path.clone());
            match path.parent() {
                Some(parent) if parent != path => path = parent.to_path_buf(),
                _ => return Ok(()),
            }
        }
    }

    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        // A path is "under `dir`" by ancestry rather than by string prefix, so a sibling whose
        // name merely starts with this one's — `columnar/12` beside `columnar/1` — is not swept
        // up with it.
        inner
            .files
            .retain(|path, _| !path.ancestors().any(|ancestor| ancestor == dir));
        inner
            .dirs
            .retain(|path| !path.ancestors().any(|ancestor| ancestor == dir));
        Ok(())
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let shared = inner
            .files
            .get(from)
            .cloned()
            .ok_or_else(|| not_found(from))?;
        // A hard link shares the bytes; deleting either name leaves the other readable.
        inner.files.insert(to.to_path_buf(), shared);
        Ok(())
    }
}

/// An appendable in-memory file.
#[derive(Debug)]
struct MemWritableFile {
    shared: Shared,
}

impl WritableFile for MemWritableFile {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        let mut file = self.shared.lock().map_err(|_| poisoned())?;
        file.bytes.extend_from_slice(data);
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        let mut file = self.shared.lock().map_err(|_| poisoned())?;
        file.synced = file.bytes.len();
        Ok(())
    }
}

/// A positioned-read in-memory file.
#[derive(Debug)]
struct MemRandomAccessFile {
    shared: Shared,
}

impl RandomAccessFile for MemRandomAccessFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let file = self.shared.lock().map_err(|_| poisoned())?;
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= file.bytes.len() {
            return Ok(0);
        }
        let n = buf.len().min(file.bytes.len() - start);
        buf[..n].copy_from_slice(&file.bytes[start..start + n]);
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        let file = self.shared.lock().map_err(|_| poisoned())?;
        Ok(file.bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::MemFileSystem;
    use crate::fs::FileSystem;
    use std::path::Path;

    #[test]
    fn writes_are_visible_to_readers() {
        let fs = MemFileSystem::new();
        let mut w = fs.create(Path::new("/db/000001.wal")).unwrap();
        w.append(b"hello").unwrap();
        w.sync_data().unwrap();
        w.append(b" world").unwrap();

        let r = fs.open(Path::new("/db/000001.wal")).unwrap();
        let mut buf = [0u8; 11];
        assert_eq!(r.read_at(0, &mut buf).unwrap(), 11);
        assert_eq!(&buf, b"hello world");
        assert_eq!(fs.size(Path::new("/db/000001.wal")).unwrap(), 11);
    }

    /// The point of the model: only synced bytes survive a power loss.
    #[test]
    fn losing_unsynced_bytes_keeps_exactly_what_was_synced() {
        let fs = MemFileSystem::new();
        let path = Path::new("/db/000001.wal");
        let mut w = fs.create(path).unwrap();
        w.append(b"durable").unwrap();
        w.sync_data().unwrap();
        w.append(b"lost").unwrap();
        assert_eq!(fs.sync_len(path).unwrap(), 7);

        fs.lose_unsynced().unwrap();
        assert_eq!(fs.contents(path).unwrap(), b"durable");
    }

    #[test]
    fn install_replaces_a_file_wholesale() {
        let fs = MemFileSystem::new();
        let path = Path::new("/db/f");
        fs.install(path, b"original".to_vec()).unwrap();
        fs.install(path, b"damaged".to_vec()).unwrap();
        assert_eq!(fs.contents(path).unwrap(), b"damaged");
    }

    #[test]
    fn rename_and_list_and_links_behave() {
        let fs = MemFileSystem::new();
        fs.install(Path::new("/db/b"), b"b".to_vec()).unwrap();
        fs.install(Path::new("/db/a"), b"a".to_vec()).unwrap();
        fs.install(Path::new("/other/c"), b"c".to_vec()).unwrap();
        let listed: Vec<_> = fs
            .list(Path::new("/db"))
            .unwrap()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(
            listed,
            ["/db/a", "/db/b"],
            "sorted, and only that directory"
        );

        fs.hard_link(Path::new("/db/a"), Path::new("/db/link"))
            .unwrap();
        fs.delete(Path::new("/db/a")).unwrap();
        assert_eq!(fs.contents(Path::new("/db/link")).unwrap(), b"a");

        fs.rename(Path::new("/db/link"), Path::new("/db/b"))
            .unwrap();
        assert_eq!(fs.contents(Path::new("/db/b")).unwrap(), b"a");
        assert!(!fs.exists(Path::new("/db/link")).unwrap());
    }

    #[test]
    fn missing_files_are_errors_not_panics() {
        let fs = MemFileSystem::new();
        assert!(fs.open(Path::new("/nope")).is_err());
        assert!(fs.size(Path::new("/nope")).is_err());
        assert!(fs.delete(Path::new("/nope")).is_err());
        assert!(!fs.exists(Path::new("/nope")).unwrap());
    }
}
