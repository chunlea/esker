//! Open SSTs, kept open.
//!
//! Opening a table reads its footer, properties, filter and index — four small reads — and the
//! index and filter are then held for the life of the reader. Doing that per lookup would turn
//! a point read into five, so readers are cached by file number.
//!
//! File numbers are never reused and SSTs are immutable, so an entry can never be stale. The
//! only reason to remove one is that the file is being deleted, which is what `evict` is
//! for.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cache_api::BlockCache;
use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::fs::FileSystem;
use crate::sst::{TableOptions, TableReader};

/// A bounded set of open table readers.
pub(crate) struct TableCache {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    capacity: usize,
    block_cache: Option<Arc<dyn BlockCache>>,
    open: Mutex<BTreeMap<u64, Arc<TableReader>>>,
}

impl std::fmt::Debug for TableCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableCache")
            .field("dir", &self.dir)
            .field("capacity", &self.capacity)
            .field("open", &self.open.lock().map_or(0, |open| open.len()))
            .finish_non_exhaustive()
    }
}

impl TableCache {
    /// A cache holding at most `capacity` open readers.
    pub(crate) fn new(
        fs: Arc<dyn FileSystem>,
        dir: PathBuf,
        capacity: usize,
        block_cache: Option<Arc<dyn BlockCache>>,
    ) -> Self {
        Self {
            fs,
            dir,
            capacity: capacity.max(1),
            block_cache,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    /// The reader for `number`, opening the file if it is not already open.
    pub(crate) fn get(&self, number: u64, options: &TableOptions) -> Result<Arc<TableReader>> {
        {
            let open = self.lock()?;
            if let Some(reader) = open.get(&number) {
                return Ok(Arc::clone(reader));
            }
        }

        // Opened outside the lock: a slow open must not block every other lookup. Two threads
        // racing on the same file both open it and one wins the insert, which costs one extra
        // open and no correctness — the file is immutable.
        let path = filename::sst(&self.dir, number);
        let file = self.fs.open(&path).at(&path)?;
        let reader = Arc::new(TableReader::open(
            file,
            number,
            options.clone(),
            self.block_cache.clone(),
        )?);

        let mut open = self.lock()?;
        if open.len() >= self.capacity
            && let Some(victim) = open.keys().next().copied()
            && victim != number
        {
            // TODO(post-v1): evict by use rather than by number. Readers are cheap to
            // reopen, so a wrong choice costs four reads, not correctness.
            open.remove(&victim);
        }
        Ok(Arc::clone(open.entry(number).or_insert(reader)))
    }

    /// Forgets `number`, which must be done before its file is deleted.
    pub(crate) fn evict(&self, number: u64) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(&number);
        }
    }

    /// How many readers are currently open.
    pub(crate) fn len(&self) -> usize {
        self.open.lock().map_or(0, |open| open.len())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<u64, Arc<TableReader>>>> {
        self.open.lock().map_err(|_| {
            Error::Poisoned("a thread panicked while holding the table cache".to_string())
        })
    }
}
