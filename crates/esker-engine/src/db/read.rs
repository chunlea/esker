//! Point lookups.
//!
//! The order is newest to oldest and it is not negotiable: the active memtable, then the
//! immutable ones newest first, then L0 newest first, then each deeper level by binary search
//! (`docs/DESIGN.md` §4.9). The first entry found for the key at or below the snapshot is the
//! answer, whether it is a value or a tombstone — a delete that stops the search is exactly
//! what makes deletion work in a log-structured store.
//!
//! L0 has to be searched file by file because its files overlap; every level below it
//! partitions the key space, so at most one file per level can contain the key and a binary
//! search finds it.
//!
//! # Why this is a seek and not an exact match
//!
//! An SST stores internal keys, so the key a read is looking for — `user_key` at some
//! snapshot — is almost never present verbatim. The lookup seeks to `(user_key, snapshot)` and
//! takes the first entry at or after it, which is the newest visible version, and then checks
//! that its user key is the one asked for.
//!
//! # The second question
//!
//! A range tombstone does not sit where the key it hides sits, so finding the newest entry is
//! no longer the whole answer: the read must also ask whether a tombstone covers it
//! ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)). This walk collects the tombstones
//! of each source *as it passes it*, newest first, and when a source finally yields an entry it
//! asks the collected set whether anything strictly newer than that entry covers the key. That
//! ordering is what makes one pass enough: every source that could hold a covering tombstone
//! has already been visited by the time the entry is found.
//!
//! Below L0 there is nothing to collect. Range tombstones are **discharged at L0** and never
//! propagated ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 6): a compaction
//! that consumes one takes every lower-level file overlapping it, drops the covered keys, and
//! writes no tombstone into its output. So `search_levels` below L0 is exactly what it was, and
//! the binary search that assumes a level partitions the key space keeps its assumption.

use bytes::Bytes;
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;

use crate::dbformat::{
    Comparator, EntryKind, SeqNo, extract_user_key, lookup_key, split_internal_key,
};
use crate::error::{Error, Result};
use crate::memtable::Lookup;
use crate::options::ReadOptions;
use crate::range_del::RangeTombstones;
use crate::version::{CfVersion, FileMeta, Version};

use super::{ColumnFamily, Db, DbInner, lock, read_lock};

impl Db {
    /// Reads one key, or `None` if it is not there at this snapshot.
    ///
    /// A snapshot from a different open database is refused: it pins nothing here, so
    /// nothing keeps the versions it names readable.
    pub fn get(&self, cf: &str, key: &[u8], options: &ReadOptions) -> Result<Option<Bytes>> {
        let cf = self.inner.cf_by_name(cf)?;
        let snapshot = self.inner.read_seqno(options)?;
        Ok(self.inner.lookup(&cf, key, snapshot)?.and_then(value_of))
    }

    /// Reads at an explicit sequence number.
    ///
    /// Unlike a [`crate::Snapshot`], a bare number pins nothing: it is a request to read history that
    /// may already have been compacted away, and there is no error for that — the read simply
    /// sees whatever survived. Take a snapshot *before* the writes you want to be able to read
    /// back; this is for the engine's own machinery and for tests, which know when nothing has
    /// been collected yet.
    pub fn get_at(&self, cf: &str, key: &[u8], seqno: SeqNo) -> Result<Option<Bytes>> {
        let cf = self.inner.cf_by_name(cf)?;
        // Held for the call so a compaction running alongside it cannot collect mid-read.
        let pinned = self.inner.snapshots.acquire(seqno);
        let found = self.inner.lookup(&cf, key, pinned.seqno());
        drop(pinned);
        Ok(found?.and_then(value_of))
    }
}

impl DbInner {
    /// The sequence number a read should use, refusing a snapshot from another database.
    ///
    /// Sequence numbers survive a reopen, so a handle taken before one is still a plausible
    /// number afterwards and would read as though it were live. It is not: this database's
    /// snapshot list has never heard of it, so the floor it hands compaction can sit above
    /// the number the handle claims, and the versions it pinned can be collected while it
    /// still holds them. A snapshot's whole promise is that what it saw stays readable, and
    /// that is a promise only the database that issued it can keep.
    pub(crate) fn read_seqno(&self, options: &ReadOptions) -> Result<SeqNo> {
        let Some(snapshot) = &options.snapshot else {
            return Ok(self.visible_seqno.load(AtomicOrdering::Acquire));
        };
        if snapshot.instance() != self.snapshots.instance() {
            return Err(Error::InvalidArgument(format!(
                "a snapshot at sequence number {} belongs to database instance {}, not {}; \
                 snapshots do not survive a reopen",
                snapshot.seqno(),
                snapshot.instance(),
                self.snapshots.instance()
            )));
        }
        Ok(snapshot.seqno())
    }

    /// The sequence number below which a shadowed version may be dropped.
    ///
    /// The oldest live snapshot, or the newest visible sequence number when there are none.
    /// Compaction may drop a version only when a newer one for the same key exists at or
    /// below this line, because then every reader that can still ask sees the newer one.
    ///
    /// A snapshot taken *after* this is read is safe without any locking: it can only be at
    /// the sequence number current when it was taken, which is at or above the floor, so the
    /// version that shadowed the dropped one is visible to it. That is the whole argument,
    /// and it holds only because a handle from another database is refused — one of those
    /// could name a number below the floor and this reasoning would not cover it.
    pub(crate) fn compaction_floor(&self) -> SeqNo {
        self.snapshots
            .oldest()
            .unwrap_or_else(|| self.visible_seqno.load(AtomicOrdering::Acquire))
    }

    /// The newest entry for `key` at or below `snapshot`, wherever it lives.
    pub(crate) fn lookup(
        &self,
        cf: &Arc<ColumnFamily>,
        key: &[u8],
        snapshot: SeqNo,
    ) -> Result<Option<Lookup>> {
        // Take a consistent set of memtables and let go of the lock: a read must not block a
        // writer, and holding an `Arc` keeps a table alive across a switch.
        let (active, immutable) = {
            let mem = read_lock(&cf.mem)?;
            let immutable: Vec<Arc<crate::memtable::MemTable>> = mem
                .immutable
                .iter()
                .map(|(table, _)| Arc::clone(table))
                .collect();
            (Arc::clone(&mem.active), immutable)
        };

        // Collected as the walk passes each source, newest first, so that an entry found in
        // an older one is checked against every tombstone that could be newer than it.
        let mut tombstones = RangeTombstones::new();

        if let Some(found) = self.consult(&active, key, snapshot, &mut tombstones) {
            return Ok(Some(found));
        }
        // Newest first: the back of the list is the most recently switched table.
        for table in immutable.iter().rev() {
            if let Some(found) = self.consult(table, key, snapshot, &mut tombstones) {
                return Ok(Some(found));
            }
        }

        // Pinning the version guarantees every file it names still exists for the whole of
        // this read, however many compactions run meanwhile.
        let version = lock(&self.versions)?.current();
        self.search_levels(&version, cf, key, snapshot, &mut tombstones)
    }

    /// Asks one memtable, having first taken its tombstones.
    ///
    /// The order matters: a tombstone in the *same* table as the entry can still hide it, so
    /// the set has to include this table's own before the entry is judged.
    fn consult(
        &self,
        table: &Arc<crate::memtable::MemTable>,
        key: &[u8],
        snapshot: SeqNo,
        tombstones: &mut RangeTombstones,
    ) -> Option<Lookup> {
        let user = self.comparator.user_comparator();
        if table.has_range_tombstones() {
            tombstones.extend(&table.range_tombstones(), user.as_ref());
        }
        match table.get(key, snapshot) {
            Some(Lookup::Found { value, seqno }) => {
                if tombstones.hides(key, seqno, snapshot, user.as_ref()) {
                    Some(Lookup::Deleted)
                } else {
                    Some(Lookup::Found { value, seqno })
                }
            }
            other => other,
        }
    }

    fn search_levels(
        &self,
        version: &Version,
        cf: &Arc<ColumnFamily>,
        key: &[u8],
        snapshot: SeqNo,
        tombstones: &mut RangeTombstones,
    ) -> Result<Option<Lookup>> {
        let target = lookup_key(key, snapshot);
        let user = self.comparator.user_comparator();

        // L0 overlaps, so every file whose range covers the key has to be consulted — newest
        // first, or an older value would shadow a newer one. It is also the only level that
        // can hold a range tombstone (ADR 0017 decision 6), and a file's key bounds are
        // widened to span the ones it holds, so `covers` finds it.
        for file in version.files(cf.id(), 0) {
            if !covers(user.as_ref(), file, key) {
                continue;
            }
            if let Some(found) = self.search_table(
                cf,
                file,
                &target,
                key,
                snapshot,
                &mut TombstoneScan::collecting(tombstones),
            )? {
                return Ok(Some(found));
            }
        }

        // Below L0 the files partition the key space, so at most one per level can hold it.
        let levels = version.cf(cf.id()).map_or(0, CfVersion::num_levels);
        for level in 1..levels {
            let files = version.files(cf.id(), level);
            // The first file whose largest key is not below the target.
            let index = files.partition_point(|file| {
                self.comparator.cmp(&file.largest, &target) == Ordering::Less
            });
            let Some(file) = files.get(index) else {
                continue;
            };
            if !covers(user.as_ref(), file, key) {
                continue;
            }
            // Nothing to *collect* below L0 — there are none — but the set collected above
            // still has to be applied: a tombstone in a memtable or an L0 file is exactly what
            // hides an entry down here.
            if let Some(found) = self.search_table(
                cf,
                file,
                &target,
                key,
                snapshot,
                &mut TombstoneScan::applying(tombstones),
            )? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Seeks one table. `None` means the table has nothing to say about the key.
    fn search_table(
        &self,
        cf: &Arc<ColumnFamily>,
        file: &FileMeta,
        target: &[u8],
        key: &[u8],
        snapshot: SeqNo,
        scan: &mut TombstoneScan<'_>,
    ) -> Result<Option<Lookup>> {
        let reader = self.table_cache.get(file.number, &self.table_options(cf))?;
        let user = self.comparator.user_comparator();
        let TombstoneScan {
            tombstones,
            collect,
        } = scan;
        let collect = *collect;

        // Collecting comes first, and specifically *before* the bloom filter: a table whose
        // filter says it holds no version of this key may still hold the tombstone that hides
        // one further down. Collecting after the filter would skip exactly those.
        if collect {
            if !reader.range_tombstones().is_empty() {
                tombstones.extend(reader.range_tombstones(), user.as_ref());
            }
        } else {
            debug_assert!(
                reader.range_tombstones().is_empty(),
                "a range tombstone below L0: ADR 0017 decision 6 says a compaction discharges \
                 them and never writes one out"
            );
        }

        // Bloom before disk (`docs/DESIGN.md` §4.9). The filter is built over the user key
        // inside the internal key, so probing it with the seek target asks exactly the right
        // question — "does this table hold this user key at any version" — and a miss saves
        // an index lookup and a block read. A filter can only rule a key *out*, so a `true`
        // here means nothing has been decided and the seek proceeds.
        if !reader.may_contain(target) {
            self.bloom_skips.fetch_add(1, AtomicOrdering::Relaxed);
            return Ok(None);
        }
        self.bloom_probes.fetch_add(1, AtomicOrdering::Relaxed);

        let mut iter = reader.iter();
        iter.seek(target);
        // A block that could not be read ends iteration exactly like reaching the end, so the
        // status has to be checked rather than the absence taken at face value.
        iter.status()?;
        if !iter.valid() {
            return Ok(None);
        }

        if user.cmp(extract_user_key(iter.key()), key) != Ordering::Equal {
            return Ok(None);
        }
        match split_internal_key(iter.key()) {
            Some((_, seqno, EntryKind::Put)) => {
                if tombstones.hides(key, seqno, snapshot, user.as_ref()) {
                    return Ok(Some(Lookup::Deleted));
                }
                Ok(Some(Lookup::Found {
                    value: iter.value().to_vec(),
                    seqno,
                }))
            }
            Some((_, _, EntryKind::Delete | EntryKind::DeleteRange)) => Ok(Some(Lookup::Deleted)),
            None => Err(Error::corruption(
                format!("{:06}.sst", file.number),
                "an entry whose tag cannot be decoded".to_string(),
            )),
        }
    }
}

/// The tombstone set a table search works against, and whether that table may add to it.
///
/// The two travel together because they must agree: **collect** at L0, where a table may hold
/// tombstones, and only **apply** below it, where the invariant says none can
/// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 6). Passing them
/// separately is how the pair would come apart.
struct TombstoneScan<'a> {
    tombstones: &'a mut RangeTombstones,
    collect: bool,
}

impl<'a> TombstoneScan<'a> {
    /// At L0: take what the table holds, then apply everything gathered so far.
    fn collecting(tombstones: &'a mut RangeTombstones) -> Self {
        Self {
            tombstones,
            collect: true,
        }
    }

    /// Below L0: apply only. A table here holding a tombstone is a broken invariant, and the
    /// `debug_assert` in `search_table` says so.
    fn applying(tombstones: &'a mut RangeTombstones) -> Self {
        Self {
            tombstones,
            collect: false,
        }
    }
}

/// Whether `file`'s key range could contain `key`.
fn covers(user: &dyn Comparator, file: &FileMeta, key: &[u8]) -> bool {
    user.cmp(key, extract_user_key(&file.smallest)) != Ordering::Less
        && user.cmp(key, extract_user_key(&file.largest)) != Ordering::Greater
}

/// A tombstone is an answer, and the answer is "not there".
fn value_of(found: Lookup) -> Option<Bytes> {
    match found {
        Lookup::Found { value, .. } => Some(Bytes::from(value)),
        Lookup::Deleted => None,
    }
}
