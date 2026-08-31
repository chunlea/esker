//! SSTs in object storage: the tiered [`FileSystem`].
//!
//! An SST never changes after `TableBuilder::finish` returns. That single fact is what makes
//! tiering possible at all — a file that is immutable can be copied to a bucket once and read
//! from there forever — and the only genuinely hard question is *when a local copy may be
//! thrown away*. [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) answers it and
//! this module implements that answer.
//!
//! # What is tiered, and what is not
//!
//! Only `NNNNNN.sst`. The WAL, the manifest and `CURRENT` go straight to the local filesystem
//! and are never touched, because they are the log: `CLAUDE.md` invariant 1 says a write is
//! acknowledged when its log bytes are durable, and putting a network between the writer and
//! that fsync would either break the invariant or make every write cost a round trip. **S3 is a
//! tier, not the log.**
//!
//! # The database directory is the cache
//!
//! There is no second cache directory and no rename dance. A tiered SST that is resident is
//! simply the local file; evicting it is deleting that file; refilling it is fetching the
//! object back to the same path. The engine is never told which of its files are resident,
//! because it does not have to care — every path it asks for either opens or reports an honest
//! error, which is the contract [`FileSystem`] already had.
//!
//! ```text
//! open(000007.sst)
//!   local file present?   -> open it                         a cache hit
//!   else, object known?   -> a reader that ranges over S3,    a cache miss
//!                            and a whole-file fetch queued
//!   else                  -> NotFound
//! ```
//!
//! Reads of a non-resident file are **ranged** rather than whole-file, because an 8 MiB fetch
//! to answer a 4 KiB point read is a three-orders-of-magnitude amplification and would ruin the
//! p99 the phase asks us to measure. The whole-file fetch happens behind that, so the *second*
//! visit to a file is local.
//!
//! # Deletion
//!
//! An object is deleted only when no live version names its number and it is not a pending
//! output — the phase-1 register discipline, extended by one set. Note that this cannot be
//! driven from a directory listing the way the local sweep is: an evicted file is *absent* from
//! the listing, so its object would never be reclaimed. [`SstTier::retain`] takes the live set
//! directly instead.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use esker_s3::{Error as S3Error, ObjectStore};

use super::claim::{CLAIM_LEN, CLAIM_OBJECT, ClaimError, Identity};
use super::{FileSystem, RandomAccessFile, SstTier, WritableFile};
use crate::filename::{self, FileKind};

/// The suffix a whole-file fetch writes under before renaming into place.
///
/// Not `.tmp`: the obsolete-file sweep deletes those, and a sweep racing a fetch would make the
/// fetch fail for no reason. `classify` returns `None` for this suffix, and `None` means "not
/// the engine's file", so the sweep leaves it alone — which in turn means a crash mid-fetch
/// leaks one, and [`TieredFileSystem::new`] cleans them up at open.
const FETCH_SUFFIX: &str = ".fetching";

/// How the tier behaves.
#[derive(Debug, Clone)]
pub struct TierOptions {
    /// The object key prefix, ending in `/` unless it is empty.
    pub key_prefix: String,

    /// Local SST bytes allowed before the governor starts evicting.
    ///
    /// `None` disables eviction: every uploaded file also stays local. That is the right
    /// setting for a store whose disk is large enough for its data and which wants object
    /// storage as durability rather than as capacity.
    pub local_budget: Option<u64>,

    /// Whether the engine runs the uploader on a thread of its own.
    ///
    /// `true` in production, where [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md)
    /// decision 1 requires that no flush or compaction ever wait on an upload. `false` in
    /// tests, where a background thread turns "has it uploaded yet" into a race and the only
    /// way to answer it is a sleep — which is how a suite becomes flaky on a loaded machine.
    /// With it off, `Db::tier_maintenance` is the only thing that moves any bytes.
    pub background: bool,

    /// How many **distinct** files one [`SstTier::maintain`] pass will upload and fetch.
    ///
    /// Bounded so that a pass cannot become unboundedly long, and *distinct* because that is
    /// the whole of the retry backoff: a file that fails goes to the back of the queue and is
    /// not tried again until the next pass. Passes happen on a new SST or on the uploader's
    /// idle tick, so a bucket that is down is retried at that cadence rather than in a hot
    /// loop — which is what [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md)
    /// decision 2 asks for, without a clock the simulator would have to fake.
    pub batch: usize,

    /// Who this database is, for the prefix's claim marker. `None` skips the claim entirely,
    /// which is what the tier's own unit tests and any single-prefix embedding want.
    ///
    /// See [`claim`](super::claim): a prefix is claimed by the first database to open it, and a
    /// database that is not the claimant is refused rather than left to overwrite the other's
    /// SSTs in silence.
    pub identity: Option<Identity>,

    /// Whether to claim a prefix that already holds objects but carries no marker.
    ///
    /// `false` — refuse — is the default and the only safe one: objects with no marker may
    /// belong to a live database written before markers existed, and adopting them silently is
    /// exactly the corruption the marker is for. `true` is the operator's explicit escape hatch
    /// (`esker server --adopt-sst-store`), which is why it can only be reached by asking.
    pub adopt_unclaimed: bool,
}

impl Default for TierOptions {
    fn default() -> Self {
        Self {
            key_prefix: String::new(),
            // 4 GiB: comfortably more than a handful of levels of an 8 MiB-file database, so
            // eviction is something an operator opts into by lowering it rather than a
            // behaviour that surprises them.
            local_budget: Some(4 * 1024 * 1024 * 1024),
            background: true,
            batch: 8,
            identity: None,
            adopt_unclaimed: false,
        }
    }
}

/// Counters the bench and the tests read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierStats {
    /// Opens served by a local file.
    pub cache_hits: u64,
    /// Opens that had to read the object because the local file was gone.
    pub cache_misses: u64,
    /// Objects successfully uploaded.
    pub uploads: u64,
    /// Upload attempts that failed and will be retried.
    pub upload_failures: u64,
    /// Whole-file fetches that completed.
    pub fetches: u64,
    /// Ranged reads issued against the object store.
    pub ranged_reads: u64,
    /// Local files deleted by the governor to stay inside the budget.
    pub evictions: u64,
    /// Objects deleted because no live version named them.
    pub object_deletes: u64,
}

impl TierStats {
    /// The fraction of opens served locally, or `None` when nothing has been opened.
    ///
    /// This is the "cache hit rate" the phase-6b bench records. It counts *opens*, not blocks:
    /// the block cache in front of this is where block-level hits are counted, and conflating
    /// the two would make the number unreadable.
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.cache_hits + self.cache_misses;
        #[allow(clippy::cast_precision_loss)]
        if total == 0 {
            None
        } else {
            Some(self.cache_hits as f64 / total as f64)
        }
    }
}

/// What the tier knows about one file.
#[derive(Debug, Clone)]
struct Entry {
    /// The `ETag` the object store reported, used to prove a ranged read came from the version
    /// we uploaded (ADR 0024 decision 6).
    etag: Option<String>,
    /// The object's size, which is also the local file's size.
    size: u64,
    /// A monotonic counter of the last open, so eviction can pick the coldest file.
    last_used: u64,
}

/// The mutable half, behind one lock.
#[derive(Debug, Default)]
struct State {
    /// Files known to be in the object store.
    uploaded: BTreeMap<u64, Entry>,
    /// Files durable locally and not yet uploaded, oldest first.
    to_upload: VecDeque<u64>,
    /// Consecutive failures per file, which is what the backoff is computed from.
    attempts: BTreeMap<u64, u32>,
    /// Uploads that completed and have not yet been reported to the engine.
    promotions: Vec<u64>,
    /// Non-resident files that have been opened and should be pulled back local.
    to_fetch: VecDeque<u64>,
    /// A clock for `last_used`; nothing outside this module depends on its units.
    ticks: u64,
}

/// A [`FileSystem`] whose SSTs live in object storage.
pub struct TieredFileSystem {
    local: Arc<dyn FileSystem>,
    store: Arc<dyn ObjectStore>,
    dir: PathBuf,
    options: TierOptions,
    state: Mutex<State>,
    stats: Arc<Stats>,
}

/// The counters, atomic so that reads never contend with the state lock.
#[derive(Debug, Default)]
struct Stats {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    uploads: AtomicU64,
    upload_failures: AtomicU64,
    fetches: AtomicU64,
    ranged_reads: AtomicU64,
    evictions: AtomicU64,
    object_deletes: AtomicU64,
}

impl std::fmt::Debug for TieredFileSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredFileSystem")
            .field("dir", &self.dir)
            .field("key_prefix", &self.options.key_prefix)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl TieredFileSystem {
    /// Wraps `local`, tiering the SSTs under `dir` into `store`.
    ///
    /// Reconciles against the bucket once, here: a `ListObjectsV2` is the only thing that
    /// actually knows what the bucket holds, and starting from it means a manifest whose
    /// location records are stale — because a crash lost the promote edit — costs nothing.
    pub fn new(
        local: Arc<dyn FileSystem>,
        store: Arc<dyn ObjectStore>,
        dir: impl AsRef<Path>,
        options: TierOptions,
    ) -> io::Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        let tier = Arc::new(Self {
            local,
            store,
            dir,
            options,
            state: Mutex::new(State::default()),
            stats: Arc::new(Stats::default()),
        });
        tier.clean_partial_fetches();
        // Before anything is adopted from the bucket: a prefix that is not ours is a startup
        // failure, and a database that listed it, cached what it found and *then* refused would
        // already have taught itself another database's file numbers.
        tier.settle_claim()?;
        tier.adopt_bucket()?;
        Ok(tier)
    }

    /// The claim marker's object key.
    fn claim_key(&self) -> String {
        format!("{}{CLAIM_OBJECT}", self.options.key_prefix)
    }

    /// Establishes that this prefix is this database's, or refuses to open.
    ///
    /// Three outcomes, and the middle one is the whole point:
    ///
    /// * **the marker is ours** — or we just wrote it — and the open proceeds;
    /// * **the marker is somebody else's** — refused, naming both databases
    ///   ([`ClaimError::Claimed`]);
    /// * **there is no marker.** An empty prefix is claimed. A prefix that already holds objects
    ///   is refused ([`ClaimError::Unclaimed`]) unless the operator passed `adopt_unclaimed`,
    ///   because objects with no marker may be a live database's, written before markers
    ///   existed. Adopting them silently is the corruption this whole module exists to stop.
    ///
    /// # Crashing in the middle
    ///
    /// The marker is written before any SST can be uploaded — this runs inside
    /// [`TieredFileSystem::new`], before the engine has the filesystem at all — so a crash
    /// between the two leaves a marker over an empty prefix. The next open reads its own marker,
    /// matches, and carries on. A crash *before* the marker landed leaves an empty prefix, which
    /// the next open claims. Neither needs a recovery path, which is why there is not one.
    ///
    /// # The race this does not win
    ///
    /// Two databases claiming an empty prefix at the same instant cannot be separated by a
    /// `PutObject`, because S3 has no conditional put in the subset
    /// [`ObjectStore`](esker_s3::ObjectStore) exposes. The claim is therefore read back after it
    /// is written: the loser of a simultaneous claim reads the winner's marker and refuses, and
    /// the window narrows to two overlapping round trips rather than the lifetime of a database.
    /// [ADR 0029](../../../docs/adr/0029-the-sst-store-claim.md) records why that is enough and
    /// what would close it.
    fn settle_claim(&self) -> io::Result<()> {
        let Some(ours) = self.options.identity else {
            return Ok(());
        };
        let key = self.claim_key();
        let prefix = self.options.key_prefix.clone();

        match self.store.get(&key) {
            Ok(response) => return Self::verify_claim(&prefix, &ours, &response.body),
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(to_io_error(&error)),
        }

        // No marker. An empty prefix is ours for the taking; one with objects in it is not.
        let listed = self
            .store
            .list(&prefix)
            .map_err(|err| to_io_error(&err))?
            .into_iter()
            .filter(|object| object.key != key)
            .count();
        if listed > 0 && !self.options.adopt_unclaimed {
            return Err(io::Error::other(
                ClaimError::Unclaimed {
                    prefix,
                    objects: listed,
                }
                .to_string(),
            ));
        }
        if listed > 0 {
            tracing::warn!(
                prefix,
                objects = listed,
                identity = %ours,
                "adopting an SST store prefix that holds objects but no claim marker, because \
                 adoption was asked for explicitly"
            );
        }

        self.store
            .put(&key, &ours.encode())
            .map_err(|err| to_io_error(&err))?;
        // Read back, so a simultaneous claim by another database is caught here rather than by
        // whichever of the two later reads the other's SST.
        let written = self.store.get(&key).map_err(|err| to_io_error(&err))?;
        Self::verify_claim(&prefix, &ours, &written.body)?;
        tracing::info!(prefix, identity = %ours, "claimed the SST store prefix");
        Ok(())
    }

    /// Checks a marker's bytes against who we are.
    fn verify_claim(prefix: &str, ours: &Identity, body: &[u8]) -> io::Result<()> {
        let theirs = Identity::decode(body).map_err(|error| {
            // A marker that cannot be read is not a marker that can be overruled: refusing is
            // the same answer as for one that names somebody else, because it might.
            io::Error::other(format!(
                "{error} (the claim marker of the SST store prefix {prefix:?},                  {} bytes; expected {CLAIM_LEN})",
                body.len()
            ))
        })?;
        if theirs.claim == ours.claim {
            return Ok(());
        }
        Err(io::Error::other(
            ClaimError::Claimed {
                prefix: prefix.to_owned(),
                ours: Box::new(*ours),
                theirs: Box::new(theirs),
            }
            .to_string(),
        ))
    }

    /// The object key for a file number.
    fn key(&self, number: u64) -> String {
        format!("{}{number:06}.sst", self.options.key_prefix)
    }

    /// The file number a key ends with, if it names an SST of ours.
    fn number_of(&self, key: &str) -> Option<u64> {
        let rest = key.strip_prefix(&self.options.key_prefix)?;
        match filename::classify(rest) {
            Some(FileKind::Sst(number)) => Some(number),
            _ => None,
        }
    }

    /// Learns what the bucket already holds.
    fn adopt_bucket(&self) -> io::Result<()> {
        let listed = self
            .store
            .list(&self.options.key_prefix)
            .map_err(|err| to_io_error(&err))?;
        let mut state = self.lock()?;
        for object in listed {
            if let Some(number) = self.number_of(&object.key) {
                state.uploaded.insert(
                    number,
                    Entry {
                        etag: Some(object.etag),
                        size: object.size,
                        last_used: 0,
                    },
                );
            }
        }
        tracing::debug!(
            objects = state.uploaded.len(),
            prefix = self.options.key_prefix,
            "adopted the objects already in the bucket"
        );
        Ok(())
    }

    /// Removes fetches a crash left half-written. They are not the engine's files, so nothing
    /// else would ever clean them up.
    fn clean_partial_fetches(&self) {
        let Ok(entries) = self.local.list(&self.dir) else {
            return;
        };
        for path in entries {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(FETCH_SUFFIX))
            {
                let _ = self.local.delete(&path);
            }
        }
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| {
            io::Error::other("a thread panicked while holding the object tier's state")
        })
    }

    /// The path an SST number lives at locally.
    fn sst_path(&self, number: u64) -> PathBuf {
        filename::sst(&self.dir, number)
    }

    /// One attempt at one file. `Ok(true)` means the object is now in the store.
    fn upload_one(&self, number: u64) -> io::Result<bool> {
        let path = self.sst_path(number);
        let body = match read_whole(self.local.as_ref(), &path) {
            Ok(body) => body,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // The file became obsolete and was swept before its upload ran. Nothing to
                // upload and nothing wrong: `retain` will not have an object to delete either.
                tracing::debug!(number, "an SST was deleted before it could be uploaded");
                self.lock()?.attempts.remove(&number);
                return Ok(false);
            }
            Err(err) => return Err(err),
        };
        let size = body.len() as u64;

        match self.store.put(&self.key(number), &body) {
            Ok(etag) => {
                self.stats.uploads.fetch_add(1, Ordering::Relaxed);
                let mut state = self.lock()?;
                state.attempts.remove(&number);
                let last_used = state.ticks;
                state.uploaded.insert(
                    number,
                    Entry {
                        etag,
                        size,
                        last_used,
                    },
                );
                state.promotions.push(number);
                tracing::debug!(number, size, "uploaded an SST");
                Ok(true)
            }
            Err(err) => {
                self.stats.upload_failures.fetch_add(1, Ordering::Relaxed);
                let mut state = self.lock()?;
                let attempts = state.attempts.entry(number).or_insert(0);
                *attempts = attempts.saturating_add(1);
                let attempts = *attempts;
                // Back on the queue, at the end. The file stays local, stays readable, and
                // nothing else fails (ADR 0024 decision 2).
                state.to_upload.push_back(number);
                drop(state);

                if err.is_retryable() {
                    tracing::warn!(
                        number,
                        attempts,
                        error = %err,
                        "an SST upload failed and will be retried; the file stays local"
                    );
                } else {
                    tracing::error!(
                        number,
                        attempts,
                        error = %err,
                        "an SST upload was refused; this is a configuration problem, and \
                         local disk will fill until it is fixed"
                    );
                }
                Ok(false)
            }
        }
    }

    /// Attempts up to `batch` **distinct** files, once each, and reports how many landed.
    ///
    /// Once each is the backoff: a file that fails is behind everything else in the queue and
    /// waits for the next pass, so an endpoint that is refusing everything is retried at the
    /// uploader's cadence instead of spun on.
    fn upload_pass(&self) -> io::Result<usize> {
        let batch: Vec<u64> = {
            let mut state = self.lock()?;
            let take = self.options.batch.min(state.to_upload.len());
            state.to_upload.drain(..take).collect()
        };
        let mut landed = 0;
        for number in batch {
            if self.upload_one(number)? {
                landed += 1;
            }
        }
        Ok(landed)
    }

    /// Pulls one non-resident file back to local disk, so the next open is a cache hit.
    fn fetch_one(&self) -> io::Result<bool> {
        let number = {
            let mut state = self.lock()?;
            match state.to_fetch.pop_front() {
                Some(number) => number,
                None => return Ok(false),
            }
        };
        let path = self.sst_path(number);
        if self.local.exists(&path)? {
            return Ok(true); // Somebody got there first.
        }

        let object = match self.store.get(&self.key(number)) {
            Ok(object) => object,
            Err(err) => {
                tracing::warn!(number, error = %err, "fetching an SST back to local disk failed");
                return Ok(true);
            }
        };

        // Written under another name and renamed, so a reader never sees a partial SST — the
        // same rule the engine applies to every file it builds (invariant 3).
        let staging = self.dir.join(format!("{number:06}.sst{FETCH_SUFFIX}"));
        let mut file = self.local.create(&staging)?;
        file.append(&object.body)?;
        file.sync_data()?;
        drop(file);
        self.local.rename(&staging, &path)?;
        self.local.fsync_dir(&self.dir)?;

        self.stats.fetches.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            number,
            bytes = object.body.len(),
            "fetched an SST back local"
        );
        Ok(true)
    }

    /// Deletes the coldest resident uploaded files until local SST bytes fit the budget.
    ///
    /// Only files the object store *holds* are candidates. A file that is not uploaded is never
    /// evicted, whatever the pressure — deleting the only copy of a live file is the one thing
    /// this whole design exists to make impossible (ADR 0024 decision 3).
    fn evict_to_budget(&self) -> io::Result<()> {
        let Some(budget) = self.options.local_budget else {
            return Ok(());
        };

        let mut resident: Vec<(u64, u64, u64)> = Vec::new(); // (last_used, number, size)
        let mut total = 0u64;
        {
            let state = self.lock()?;
            for (number, entry) in &state.uploaded {
                let path = self.sst_path(*number);
                if self.local.exists(&path)? {
                    total = total.saturating_add(entry.size);
                    resident.push((entry.last_used, *number, entry.size));
                }
            }
        }
        if total <= budget {
            return Ok(());
        }

        resident.sort_unstable(); // Coldest first.
        for (_, number, size) in resident {
            if total <= budget {
                break;
            }
            let path = self.sst_path(number);
            match self.local.delete(&path) {
                Ok(()) => {
                    total = total.saturating_sub(size);
                    self.stats.evictions.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(number, size, "evicted a local SST that is safe in the tier");
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    total = total.saturating_sub(size);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

impl SstTier for TieredFileSystem {
    fn note_durable_sst(&self, number: u64) {
        if let Ok(mut state) = self.lock() {
            if state.uploaded.contains_key(&number) || state.to_upload.contains(&number) {
                return;
            }
            state.to_upload.push_back(number);
        }
    }

    fn drain_promotions(&self) -> Vec<u64> {
        self.lock()
            .map(|mut state| std::mem::take(&mut state.promotions))
            .unwrap_or_default()
    }

    fn retain(&self, live: &BTreeSet<u64>, pending: &BTreeSet<u64>) -> io::Result<()> {
        let doomed: Vec<u64> = {
            let state = self.lock()?;
            state
                .uploaded
                .keys()
                .copied()
                .filter(|number| !live.contains(number) && !pending.contains(number))
                .collect()
        };
        for number in doomed {
            match self.store.delete(&self.key(number)) {
                Ok(()) => {
                    self.stats.object_deletes.fetch_add(1, Ordering::Relaxed);
                    let mut state = self.lock()?;
                    state.uploaded.remove(&number);
                    state.attempts.remove(&number);
                    tracing::debug!(number, "deleted the object of a file no version names");
                }
                // Leaked rather than lost. An object nobody references costs storage; one
                // deleted too early costs data, so the retry is the next sweep and never a
                // reason to fail this one.
                Err(err) => {
                    tracing::warn!(number, error = %err, "deleting an obsolete object failed");
                }
            }
        }
        Ok(())
    }

    fn maintain(&self) -> usize {
        let uploaded = match self.upload_pass() {
            Ok(uploaded) => uploaded,
            Err(err) => {
                tracing::warn!(error = %err, "an upload pass failed");
                0
            }
        };
        for _ in 0..self.options.batch {
            match self.fetch_one() {
                Ok(true) => {}
                Ok(false) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "a fetch pass failed");
                    break;
                }
            }
        }
        if let Err(err) = self.evict_to_budget() {
            tracing::warn!(error = %err, "the local-disk governor failed");
        }
        uploaded
    }

    fn wants_background_thread(&self) -> bool {
        self.options.background
    }

    fn stats(&self) -> TierStats {
        TierStats {
            cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.stats.cache_misses.load(Ordering::Relaxed),
            uploads: self.stats.uploads.load(Ordering::Relaxed),
            upload_failures: self.stats.upload_failures.load(Ordering::Relaxed),
            fetches: self.stats.fetches.load(Ordering::Relaxed),
            ranged_reads: self.stats.ranged_reads.load(Ordering::Relaxed),
            evictions: self.stats.evictions.load(Ordering::Relaxed),
            object_deletes: self.stats.object_deletes.load(Ordering::Relaxed),
        }
    }
}

impl FileSystem for TieredFileSystem {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        // Always local. Durability is local (invariant 1); the tier hears about the file
        // afterwards, through `note_durable_sst`.
        self.local.create(path)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        let Some(number) = sst_number(path) else {
            return self.local.open(path);
        };
        if self.local.exists(path)? {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            // A resident file that is read stays warm, so the governor evicts something else.
            if let Ok(mut state) = self.lock() {
                state.ticks = state.ticks.saturating_add(1);
                let ticks = state.ticks;
                if let Some(entry) = state.uploaded.get_mut(&number) {
                    entry.last_used = ticks;
                }
            }
            return self.local.open(path);
        }

        let entry = {
            let mut state = self.lock()?;
            let Some(entry) = state.uploaded.get(&number).cloned() else {
                // Not local and not in the bucket. That is a missing file, and it is reported
                // exactly as a local-only engine would report it.
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is neither on disk nor in the tier", path.display()),
                ));
            };
            state.ticks = state.ticks.saturating_add(1);
            let ticks = state.ticks;
            if let Some(entry) = state.uploaded.get_mut(&number) {
                entry.last_used = ticks;
            }
            if !state.to_fetch.contains(&number) {
                state.to_fetch.push_back(number);
            }
            entry
        };

        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(TieredFile {
            store: Arc::clone(&self.store),
            key: self.key(number),
            size: entry.size,
            etag: entry.etag,
            stats: Arc::clone(&self.stats),
        }))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.local.list(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.local.rename(from, to)
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        // Local only. The object is deleted by `retain`, which is the one place that knows
        // whether a live version still needs it.
        self.local.delete(path)
    }

    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        self.local.fsync_dir(dir)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        match self.local.size(path) {
            Ok(size) => Ok(size),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let Some(number) = sst_number(path) else {
                    return Err(err);
                };
                let state = self.lock()?;
                state
                    .uploaded
                    .get(&number)
                    .map(|entry| entry.size)
                    .ok_or(err)
            }
            Err(err) => Err(err),
        }
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        if self.local.exists(path)? {
            return Ok(true);
        }
        let Some(number) = sst_number(path) else {
            return Ok(false);
        };
        Ok(self.lock()?.uploaded.contains_key(&number))
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.local.create_dir_all(dir)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.local.hard_link(from, to)
    }

    fn tier(&self) -> Option<&dyn SstTier> {
        Some(self)
    }
}

/// A reader over an object, one range at a time.
///
/// Every `read_at` is a `GetObject` with a `Range`. That is expensive per call and cheap per
/// byte, which is the right trade for an SST: a point read touches an index block, a filter and
/// one data block, and the block cache in front means a hot file costs nothing at all.
struct TieredFile {
    store: Arc<dyn ObjectStore>,
    key: String,
    size: u64,
    etag: Option<String>,
    stats: Arc<Stats>,
}

impl RandomAccessFile for TieredFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0); // End of file, which is a short read and not an error.
        }
        self.stats.ranged_reads.fetch_add(1, Ordering::Relaxed);
        let want = (buf.len() as u64).min(self.size - offset);
        let got = self
            .store
            .get_range(&self.key, offset, want, self.etag.as_deref())
            .map_err(|err| to_io_error(&err))?;
        let n = got.body.len().min(buf.len());
        buf[..n].copy_from_slice(&got.body[..n]);
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }
}

/// The file number of an SST path, or `None` for anything else the engine keeps.
fn sst_number(path: &Path) -> Option<u64> {
    match filename::classify_path(path) {
        Some(FileKind::Sst(number)) => Some(number),
        _ => None,
    }
}

/// Reads a whole file through the filesystem seam, so a fault injector still sees it.
fn read_whole(fs: &dyn FileSystem, path: &Path) -> io::Result<Vec<u8>> {
    let file = fs.open(path)?;
    let size = usize::try_from(file.size()?)
        .map_err(|_| io::Error::other("an SST larger than this machine can address"))?;
    let mut body = vec![0u8; size];
    super::read_exact_at(file.as_ref(), 0, &mut body)?;
    Ok(body)
}

/// An object-store error as an `io::Error`, preserving the one distinction the engine acts on.
fn to_io_error(err: &S3Error) -> io::Error {
    if err.is_not_found() {
        io::Error::new(io::ErrorKind::NotFound, err.to_string())
    } else {
        io::Error::other(err.to_string())
    }
}
