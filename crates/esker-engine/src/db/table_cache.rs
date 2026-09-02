//! Open SSTs, kept open.
//!
//! Opening a table reads its footer, properties, filter and index — four small reads — and the
//! index and filter are then held for the life of the reader. Doing that per lookup would turn
//! a point read into five, so readers are cached by file number.
//!
//! File numbers are never reused and SSTs are immutable, so an entry can never be stale. The
//! only reason to remove one is that the file is being deleted, which is what `evict` is
//! for.
//!
//! # Eviction is by use, and the wrong rule was worse than random
//!
//! This used to evict the **lowest file number**, which is not a neutral choice: file numbers
//! rise monotonically, so the lowest one is the oldest file, which in a levelled engine is the
//! one that has survived the most compactions — the deepest, largest, most-read file in the
//! tree. The cache was systematically throwing away its best entry, and a point read that walked
//! down to L4 reopened the same file every time while a freshly flushed L0 file nothing would
//! ask for again sat resident.
//!
//! So: least-recently-used, exactly. A `BTreeMap` from a monotonic use counter to a file number
//! gives O(log n) eviction of the true least-recently-used entry, and the whole structure is
//! about forty lines of safe code.
//!
//! # Why this is not the sharded LRU the block cache has
//!
//! `cache/lru.rs` is eight shards of an index-linked arena, and it is that because it holds
//! hundreds of thousands of blocks and is on the path of every block read. This holds
//! [`crate::options::defaults::MAX_OPEN_TABLES`] — 256 by default — entries, and its critical section is a map lookup
//! and an `Arc` clone. Sharding it would be optimising before a profile, which `CLAUDE.md`
//! forbids, and it would trade an exact LRU for a per-shard approximation of one. If a profile
//! ever shows this mutex, the shape to copy is next door.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::cache_api::BlockCache;
use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::fs::FileSystem;
use crate::sst::{TableOptions, TableReader};

/// One cached reader and the tick it was last handed out at.
#[derive(Debug)]
struct Open {
    reader: Arc<TableReader>,
    /// This entry's key in `recency`. Rewritten on every hit, which is what makes the cache
    /// least-recently-*used* rather than least-recently-inserted.
    used_at: u64,
}

/// The mutable half, behind one lock so the map and the recency order can never disagree.
///
/// They are two views of one set and an entry present in one and absent from the other is a
/// leak or a panic, so they are not separately lockable.
#[derive(Debug, Default)]
struct State {
    open: HashMap<u64, Open>,
    /// Use tick to file number, so the least-recently-used entry is `first_key_value`.
    recency: BTreeMap<u64, u64>,
}

/// A bounded set of open table readers, evicted least-recently-used.
pub(crate) struct TableCache {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    capacity: usize,
    block_cache: Option<Arc<dyn BlockCache>>,
    state: Mutex<State>,
    /// Ticks handed out. Monotonic for the life of the cache; at one tick per lookup a `u64`
    /// outlasts the hardware.
    clock: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl std::fmt::Debug for TableCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableCache")
            .field("dir", &self.dir)
            .field("capacity", &self.capacity)
            .field("open", &self.len())
            .field("hits", &self.hits.load(Ordering::Relaxed))
            .field("misses", &self.misses.load(Ordering::Relaxed))
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
            state: Mutex::new(State::default()),
            clock: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// The reader for `number`, opening the file if it is not already open.
    pub(crate) fn get(&self, number: u64, options: &TableOptions) -> Result<Arc<TableReader>> {
        {
            let mut state = self.lock()?;
            if let Some(reader) = self.touch(&mut state, number) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(reader);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);

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

        let mut state = self.lock()?;
        // The race above: while this thread was opening, another may have inserted the same
        // file — or *used* it, which is why this goes through `touch` and not a bare `get`.
        if let Some(existing) = self.touch(&mut state, number) {
            return Ok(existing);
        }
        self.insert(&mut state, number, Arc::clone(&reader));
        self.evict_until_within_capacity(&mut state, number);
        Ok(reader)
    }

    /// Marks `number` as just used and returns its reader, if it is open.
    ///
    /// The recency key moves, which costs a `BTreeMap` remove and insert per hit. That is the
    /// price of an exact LRU and it is paid on the read path deliberately: the alternative is
    /// an approximation, and the rule this replaced was an approximation of the *wrong* thing.
    fn touch(&self, state: &mut State, number: u64) -> Option<Arc<TableReader>> {
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let entry = state.open.get_mut(&number)?;
        let previous = entry.used_at;
        entry.used_at = tick;
        let reader = Arc::clone(&entry.reader);
        state.recency.remove(&previous);
        state.recency.insert(tick, number);
        Some(reader)
    }

    fn insert(&self, state: &mut State, number: u64, reader: Arc<TableReader>) {
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        state.open.insert(
            number,
            Open {
                reader,
                used_at: tick,
            },
        );
        state.recency.insert(tick, number);
    }

    /// Drops least-recently-used entries until the cache is within capacity.
    ///
    /// `keep` is never evicted. Without that the caller could be handed a reader the cache had
    /// already decided to drop — harmless today, since the reader is an `Arc` the caller owns,
    /// but it would make "the cache holds what it just returned" false, and every reasoning
    /// about the hit rate starts there.
    fn evict_until_within_capacity(&self, state: &mut State, keep: u64) {
        while state.open.len() > self.capacity {
            let Some((&tick, &victim)) = state.recency.first_key_value() else {
                return;
            };
            if victim == keep {
                // The only entry left is the one just inserted, which means the capacity is one.
                let Some((&next_tick, &next)) = state.recency.iter().nth(1) else {
                    return;
                };
                state.recency.remove(&next_tick);
                state.open.remove(&next);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            state.recency.remove(&tick);
            state.open.remove(&victim);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Forgets `number`, which must be done before its file is deleted.
    pub(crate) fn evict(&self, number: u64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(entry) = state.open.remove(&number) {
                state.recency.remove(&entry.used_at);
            }
        }
    }

    /// How many readers are currently open.
    pub(crate) fn len(&self) -> usize {
        self.state.lock().map_or(0, |state| state.open.len())
    }

    /// Lookups that found an open reader.
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Lookups that had to open the file.
    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Readers dropped to stay within capacity. Does not count [`Self::evict`], which is a
    /// deletion rather than a capacity decision.
    pub(crate) fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| {
            Error::Poisoned("a thread panicked while holding the table cache".to_string())
        })
    }
}
