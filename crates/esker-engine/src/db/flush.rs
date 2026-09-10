//! Making room for writes: switching memtables, and turning the full ones into L0 files.
//!
//! # The switch
//!
//! When a column family's active memtable passes `write_buffer_size` the group-commit leader
//! makes it immutable, starts a new log segment, and hands the old table to the background
//! thread. The new segment matters: it is what lets the old one be deleted once the table it
//! describes is on disk, and it is why `docs/DESIGN.md` §4.3 says "one WAL segment per
//! memtable generation".
//!
//! # The flush
//!
//! One background thread builds an SST from the oldest immutable memtable, logs a
//! `VersionEdit` adding it at L0, and only **then** drops the table. The order is the whole
//! safety argument: until the manifest edit is durable the memtable is still the only copy of
//! that data, so it stays readable and its log segment stays alive. A crash anywhere before
//! the edit lands replays the log and rebuilds the same memtable.
//!
//! # Stalling, visibly
//!
//! Writes outrun flushes eventually, and when they do the engine has to push back. It slows
//! down at `memtable_slowdown` immutable tables and stops at `memtable_stop` (§4.4). Both are
//! counted and readable as properties, because a database that mysteriously goes slow is a
//! database nobody can operate.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::dbformat::{
    Comparator, EntryKind, InternalKeyComparator, InternalPrefixExtractor, MAX_SEQNO, SeqNo,
    append_internal_key, extract_tag, extract_user_key, tag_seqno,
};
use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::memtable::MemTable;
use crate::range_del::RangeTombstones;
use crate::sst::{TableBuilder, TableOptions};
use crate::version::{FileLocation, FileMeta, VersionEdit};
use crate::wal::LogWriter;

use super::{ColumnFamily, Db, DbInner, MemState, lock, read_lock, write_lock};

/// Grows a file's internal-key bounds to span its range tombstones.
///
/// `Version::overlapping` and the read path's `covers` pick files by these bounds, so a
/// tombstone reaching outside them would be invisible to the reads that need it — and the
/// failure would be silent, the read taking a value from a lower level with nothing reporting
/// an error ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 3).
///
/// The bounds are **internal** keys and a tombstone's are user keys, so each end is given the
/// tag that makes it sort outside every real entry for the same user key: the largest possible
/// tag at the bottom, the smallest at the top. A tombstone's `end` is exclusive, so taking it
/// as the file's largest key overstates the reach by up to one user key — which costs an extra
/// file opened and never a wrong answer.
///
/// Empty bounds mean a file with no entries at all, whose reach is entirely its tombstones.
fn widen(
    comparator: &Arc<InternalKeyComparator>,
    smallest: Vec<u8>,
    largest: Vec<u8>,
    tombstones: &RangeTombstones,
) -> (Vec<u8>, Vec<u8>) {
    let user = comparator.user_comparator();
    let Some((low, high)) = tombstones.key_bounds(user.as_ref()) else {
        return (smallest, largest);
    };

    let mut low_key = Vec::with_capacity(low.len() + 8);
    append_internal_key(low, MAX_SEQNO, EntryKind::MAX, &mut low_key);
    let mut high_key = Vec::with_capacity(high.len() + 8);
    append_internal_key(high, 0, EntryKind::Delete, &mut high_key);

    let smallest = if smallest.is_empty()
        || user.cmp(low, extract_user_key(&smallest)) == std::cmp::Ordering::Less
    {
        low_key
    } else {
        smallest
    };
    let largest = if largest.is_empty()
        || user.cmp(high, extract_user_key(&largest)) == std::cmp::Ordering::Greater
    {
        high_key
    } else {
        largest
    };
    (smallest, largest)
}

/// How long a stalled writer waits before looking again, so that a background thread that has
/// died cannot wedge the process silently.
const STALL_POLL: Duration = Duration::from_millis(50);

impl Db {
    /// Makes the active memtable of `cf` immutable and waits for it to reach L0.
    ///
    /// Mostly for tests and for `esker-cli`: the engine flushes on its own when a memtable
    /// fills up.
    pub fn flush(&self, cf: &str) -> Result<()> {
        let cf = self.inner.cf_by_name(cf)?;
        // **Sampled before anything is signalled**, because the wait below is "one more flush of
        // this family has finished" and a mark taken afterwards could already include the flush
        // it is meant to wait for.
        let (mark, idle) = {
            let mem = read_lock(&cf.mem)?;
            (mem.swept, mem.active.is_empty() && mem.immutable.is_empty())
        };
        // Nothing to write: no job will run, so no completion will ever arrive to wait for.
        if idle {
            return Ok(());
        }
        self.inner.switch_memtable_of(&cf)?;
        self.inner.signal_flush();
        self.inner.wait_for_flush(&cf, mark)
    }

    /// Flushes every column family.
    pub fn flush_all(&self) -> Result<()> {
        for name in self.cf_names() {
            self.flush(&name)?;
        }
        Ok(())
    }
}

impl DbInner {
    /// Called by the group-commit leader before anything is logged.
    ///
    /// Switches any column family whose memtable is full, rolling the log once for all of
    /// them, and stalls the writer if the flush queue is already too deep.
    pub(crate) fn make_room_for_write(&self) -> Result<()> {
        let families: Vec<Arc<ColumnFamily>> = read_lock(&self.cfs)?.values().cloned().collect();
        let mut full = Vec::new();
        for cf in &families {
            let mem = read_lock(&cf.mem)?;
            if mem.active.approximate_size() >= cf.options().write_buffer_size
                && !mem.active.is_empty()
            {
                full.push(Arc::clone(cf));
            }
        }
        if full.is_empty() {
            return Ok(());
        }
        for cf in &full {
            self.wait_for_room(cf)?;
        }
        self.roll_log_and_switch(&full)
    }

    /// Makes one column family's memtable immutable, whatever its size.
    pub(crate) fn switch_memtable_of(&self, cf: &Arc<ColumnFamily>) -> Result<()> {
        if read_lock(&cf.mem)?.active.is_empty() {
            return Ok(());
        }
        self.wait_for_room(cf)?;
        self.roll_log_and_switch(std::slice::from_ref(cf))
    }

    /// Starts a new log segment and moves each family's active memtable onto it.
    fn roll_log_and_switch(&self, families: &[Arc<ColumnFamily>]) -> Result<()> {
        let number = lock(&self.versions)?.new_file_number();
        let path = filename::wal(&self.dir, number);
        let file = self.fs.create(&path).at(&path)?;
        let mut writer = LogWriter::new(file, path.display().to_string());
        // Carried across the roll, or the option would hold only until the log next filled up.
        writer.set_sync_call(self.options.sync_call);

        {
            let mut wal = lock(&self.wal)?;
            // Nothing may stay in the old segment's user-space buffer: from here on it is
            // never written to again.
            wal.writer.flush()?;
            wal.writer = writer;
            wal.number = number;
        }

        for cf in families {
            let mut mem = write_lock(&cf.mem)?;
            let fresh = Arc::new(MemTable::new(Arc::clone(&self.comparator)));
            let retired = std::mem::replace(&mut mem.active, fresh);
            let retired_log = mem.active_log;
            mem.immutable.push((retired, retired_log));
            mem.active_log = number;
        }
        self.signal_flush();
        Ok(())
    }

    /// Blocks while the flush queue is too deep, and slows the writer down before that.
    fn wait_for_room(&self, cf: &Arc<ColumnFamily>) -> Result<()> {
        loop {
            let depth = read_lock(&cf.mem)?.immutable.len();
            if depth < cf.options().memtable_stop {
                if depth >= cf.options().memtable_slowdown {
                    self.slowdowns.fetch_add(1, Ordering::Relaxed);
                    // A brief, deliberate pause: enough for the flush thread to make headway
                    // without turning the write path into a stop-start.
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Ok(());
            }
            self.stalls.fetch_add(1, Ordering::Relaxed);
            if self.shutdown.load(Ordering::Acquire) {
                return Err(Error::ShuttingDown);
            }
            self.check_flush_error()?;
            let state = lock(&self.flush)?;
            // A timeout rather than a plain wait: a background thread that has stopped must
            // not be able to wedge every writer without anyone finding out.
            let _unused = self
                .flush_done
                .wait_timeout(state, STALL_POLL)
                .map_err(|_| Error::Poisoned("the flush lock was poisoned".to_string()))?;
        }
    }

    /// Waits until `cf` has no immutable memtables left.
    pub(crate) fn wait_for_flush(&self, cf: &Arc<ColumnFamily>, mark: u64) -> Result<()> {
        loop {
            // **Both halves, and the second is the one that was missing.** `immutable` empties at
            // `flush_one`'s step 4 and the obsolete-file sweep runs at step 7, with a durable
            // manifest edit in between — so a caller that watched only the memtable was told its
            // flush had finished while the segment that flush reclaims was still on disk. Under
            // load that window holds an fsync, which is why it went red at load 12 and green when
            // idle. `swept` is bumped after the sweep, so waiting for it to pass `mark` is waiting
            // for the whole job.
            {
                let mem = read_lock(&cf.mem)?;
                if mem.immutable.is_empty() && mem.swept > mark {
                    return Ok(());
                }
            }
            self.check_flush_error()?;
            if self.shutdown.load(Ordering::Acquire) {
                return Err(Error::ShuttingDown);
            }
            let state = lock(&self.flush)?;
            let _unused = self
                .flush_done
                .wait_timeout(state, STALL_POLL)
                .map_err(|_| Error::Poisoned("the flush lock was poisoned".to_string()))?;
        }
    }

    /// Wakes the background thread.
    pub(crate) fn signal_flush(&self) {
        if let Ok(mut state) = self.flush.lock() {
            state.wanted = true;
        }
        self.flush_wanted.notify_all();
    }

    /// Reports a background failure to a foreground caller. A flush that cannot write is not
    /// something to keep quiet about while writes pile up behind it.
    fn check_flush_error(&self) -> Result<()> {
        let state = lock(&self.flush)?;
        match &state.error {
            Some(reason) => Err(Error::Poisoned(format!(
                "a background flush failed: {reason}"
            ))),
            None => Ok(()),
        }
    }

    /// The background thread's body.
    pub(crate) fn flush_loop(&self) {
        loop {
            {
                let Ok(mut state) = self.flush.lock() else {
                    return;
                };
                while !state.wanted && !self.shutdown.load(Ordering::Acquire) {
                    // A timeout rather than a plain wait. `Db::drop` sets the shutdown flag
                    // while holding this lock so the wake-up cannot be lost, and the timeout
                    // is the second line of defence: a background thread that slept through a
                    // shutdown would wedge the join in `Drop` with no way to find out why.
                    let Ok((next, _)) = self.flush_wanted.wait_timeout(state, STALL_POLL) else {
                        return;
                    };
                    state = next;
                }
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                state.wanted = false;
            }

            let outcome = self.flush_ready_memtables();
            if let Ok(mut state) = self.flush.lock()
                && let Err(err) = outcome
            {
                tracing::error!(error = %err, "background flush failed");
                state.error.get_or_insert_with(|| err.to_string());
            }
            self.flush_done.notify_all();
        }
    }

    /// Flushes every immutable memtable there is, oldest first.
    fn flush_ready_memtables(&self) -> Result<()> {
        loop {
            let families: Vec<Arc<ColumnFamily>> =
                read_lock(&self.cfs)?.values().cloned().collect();
            let mut work = None;
            for cf in families {
                let mem = read_lock(&cf.mem)?;
                if let Some((table, _)) = mem.immutable.first() {
                    work = Some((Arc::clone(&cf), Arc::clone(table)));
                    break;
                }
            }
            let Some((cf, table)) = work else {
                return Ok(());
            };
            self.flush_one(&cf, &table)?;
        }
    }

    /// Writes one memtable to L0 and retires it.
    fn flush_one(&self, cf: &Arc<ColumnFamily>, table: &Arc<MemTable>) -> Result<()> {
        let number = lock(&self.versions)?.new_file_number();
        let meta = self.build_table(cf, table, number)?;

        #[cfg(any(test, feature = "testing"))]
        self.pause_at(crate::testing::PausePoint::FlushedTableBeforeEdit);

        let mut edit = VersionEdit::new();
        let meta_written = meta.is_some();
        if let Some(meta) = meta {
            tracing::debug!(
                cf = cf.id(),
                file = number,
                entries = table.len(),
                "flushed a memtable to L0"
            );
            edit.add_file(cf.id(), 0, meta);
        }
        self.log_and_apply(&mut edit)?;

        // After the edit and never before it — local durability is what an acknowledgement
        // means, and an upload is not part of it (ADR 0024 decision 1) — but before the
        // immutable memtable is dropped, because that drop is what `wait_for_flush` returns
        // on. A caller that has been told its flush finished must find the file already known
        // to the tier, or "flush then upload" is a race it cannot win.
        if meta_written {
            self.note_durable_sst(number);
        }

        // Only now: until the edit is durable, this memtable is the only copy of that data.
        {
            let mut mem = write_lock(&cf.mem)?;
            if !mem.immutable.is_empty() {
                mem.immutable.remove(0);
            }
        }
        self.advance_log_number()?;
        self.drop_pending(number)?;
        self.purge_and_evict()?;
        // **After the sweep and before the wake-up**: this is what `wait_for_flush` returns on, so
        // it must not be visible until everything a caller is entitled to assume is true.
        write_lock(&cf.mem)?.swept += 1;
        // A new L0 file may have pushed the level over its trigger.
        self.signal_compaction();
        self.flush_done.notify_all();
        Ok(())
    }

    /// Finishes a file that holds nothing but range tombstones.
    ///
    /// A memtable can be exactly this: `delete_range` and nothing else. The file has no data
    /// blocks and no entries, and its whole reach comes from its tombstones — so its bounds
    /// are theirs.
    fn finish_tombstone_only_table(
        &self,
        mut builder: TableBuilder,
        number: u64,
        tombstones: &RangeTombstones,
    ) -> Result<Option<FileMeta>> {
        let (mut smallest_seqno, mut largest_seqno) = (SeqNo::MAX, 0);
        for tombstone in tombstones {
            smallest_seqno = smallest_seqno.min(tombstone.seqno);
            largest_seqno = largest_seqno.max(tombstone.seqno);
        }
        builder.set_seqno_range(smallest_seqno, largest_seqno);
        builder.set_range_tombstones(tombstones.clone());
        let properties = builder.finish()?;
        let (smallest, largest) = widen(&self.comparator, Vec::new(), Vec::new(), tombstones);
        Ok(Some(FileMeta {
            number,
            size: properties.file_size,
            smallest,
            largest,
            smallest_seqno,
            largest_seqno,
            location: FileLocation::Local,
        }))
    }

    /// Builds an SST from `table`. `Ok(None)` means the table was empty and no file was made.
    fn build_table(
        &self,
        cf: &Arc<ColumnFamily>,
        table: &Arc<MemTable>,
        number: u64,
    ) -> Result<Option<FileMeta>> {
        let path = filename::sst(&self.dir, number);
        // Held before the file exists: until the manifest edit names it, it belongs to no
        // version, which is exactly what the obsolete-file sweep deletes.
        self.hold_pending(number)?;
        let file = self.fs.create(&path).at(&path)?;
        let mut builder = TableBuilder::new(self.table_options(cf), file);

        let mut iter = table.iter();
        iter.seek_to_first();
        let mut smallest: Option<Vec<u8>> = None;
        let mut largest: Vec<u8> = Vec::new();
        let (mut smallest_seqno, mut largest_seqno) = (SeqNo::MAX, 0);
        while iter.valid() {
            let key = iter.key();
            if smallest.is_none() {
                smallest = Some(key.to_vec());
            }
            largest.clear();
            largest.extend_from_slice(key);
            if let Some(tag) = extract_tag(key) {
                let seqno = tag_seqno(tag);
                smallest_seqno = smallest_seqno.min(seqno);
                largest_seqno = largest_seqno.max(seqno);
            }
            builder.add(key, iter.value())?;
            iter.next();
        }

        // The ranges deleted while this table was active. They go with it: a flush is the
        // only way a tombstone reaches an SST, because a compaction discharges them rather
        // than propagating them ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)
        // decision 6).
        let tombstones = table.range_tombstones();

        let Some(smallest) = smallest else {
            if tombstones.is_empty() {
                // Nothing to write. Drop the builder without finishing it and take the file
                // away again, rather than leaving a zero-entry SST for compaction to trip
                // over.
                drop(builder);
                let _ = self.fs.delete(&path);
                self.drop_pending(number)?;
                return Ok(None);
            }
            // A memtable holding only range deletes is not empty in the way that matters: its
            // tombstones still have to reach a file, or the deletes are lost at the flush.
            return self.finish_tombstone_only_table(builder, number, &tombstones);
        };

        // The SST layer cannot read a sequence number out of a key — that would mean
        // understanding one, which invariant 7 forbids — so the engine hands them over.
        for tombstone in &tombstones {
            smallest_seqno = smallest_seqno.min(tombstone.seqno);
            largest_seqno = largest_seqno.max(tombstone.seqno);
        }
        builder.set_seqno_range(smallest_seqno, largest_seqno);
        builder.set_range_tombstones(tombstones.clone());
        let properties = builder.finish()?;

        // The file's bounds are widened to span its tombstones, in internal-key space, because
        // `Version::overlapping` and the read path's `covers` pick files by these — and a
        // tombstone reaching outside them would be invisible to exactly the reads that need
        // it, silently ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 3).
        let (smallest, largest) = widen(&self.comparator, smallest, largest, &tombstones);
        Ok(Some(FileMeta {
            number,
            size: properties.file_size,
            smallest,
            largest,
            smallest_seqno,
            largest_seqno,
            location: FileLocation::Local,
        }))
    }

    /// How a column family's tables are built and read. The same options must be used for
    /// both, which is why there is one function producing them.
    ///
    /// This is where [`crate::options::BlockSize::Storage`] becomes a number, and it is the right
    /// place because it is the one function that sees both the family's options and the
    /// filesystem underneath. A reader never consults `block_size` — a table's real block bounds
    /// come from its own index — so resolving it differently than the file was written with
    /// changes nothing about reading that file. Only the *next* table written is affected.
    pub(crate) fn table_options(&self, cf: &ColumnFamily) -> TableOptions {
        let options = cf.options();
        TableOptions {
            block_size: options.block_size.resolve(self.fs.tier().is_some()),
            restart_interval: options.restart_interval,
            bloom_bits_per_key: options.bloom_bits_per_key,
            // The filter goes over the user key inside the internal key; see
            // `InternalPrefixExtractor` for why a filter over raw internal keys is unusable.
            prefix_extractor: Some(Arc::new(InternalPrefixExtractor::new(
                options.prefix_extractor.clone(),
            ))),
            compression: options.compression,
            comparator: Arc::clone(&self.comparator) as Arc<dyn Comparator>,
        }
    }

    /// Moves the log number up to the oldest segment any memtable still depends on.
    fn advance_log_number(&self) -> Result<()> {
        // Captured *before* the scan below, and this ordering is load-bearing. A family that
        // reads as empty may take a write an instant later; that write goes into whatever
        // segment is current *then*, which is this number or a later one. Retiring everything
        // below it is therefore safe. Reading it afterwards would let the log roll in between
        // and retire a segment a family's memtable had just been filled from.
        let current = lock(&self.wal)?.number;

        let families: Vec<Arc<ColumnFamily>> = read_lock(&self.cfs)?.values().cloned().collect();
        let mut oldest = u64::MAX;
        for cf in &families {
            let mem = read_lock(&cf.mem)?;
            oldest = oldest.min(oldest_log(&mem));
        }
        // Every family is empty, so no segment is holding anyone's unflushed data and the
        // whole history behind the current one can go.
        if oldest == u64::MAX {
            oldest = current;
        }
        {
            let mut versions = lock(&self.versions)?;
            if oldest <= versions.log_number() {
                return Ok(());
            }
            versions.set_log_number(oldest);
        }
        let mut edit = VersionEdit::new();
        edit.log_number = Some(oldest);
        self.log_and_apply(&mut edit)
    }
}

/// The oldest log segment a column family's memtables still depend on, or [`u64::MAX`] when it
/// depends on none.
///
/// # Why an empty family must answer "none"
///
/// `roll_log_and_switch` updates `active_log` only for the families it actually switches — the
/// full ones — so a family that goes idle keeps whatever segment it was last switched onto,
/// for ever. Answering `active_log` unconditionally then pins `advance_log_number` at that
/// stale segment and **no WAL segment is ever retired again**.
///
/// That is not a theoretical shape. `esker-store` opens four families and a RawKV-only
/// workload writes to two of them; `lock` and `write` sit empty and pin the log at the segment
/// they were created in. The symptom is a data directory that grows without bound and a
/// recovery that replays the entire history of the database — and, because every write ever
/// made is still in the log, a store that appears not to need its SSTs at all. Found by the
/// phase-6b acceptance test, which deleted a node's SSTs and lost nothing.
///
/// An empty active memtable genuinely depends on no segment: everything the family ever held
/// has been flushed, and a write arriving after this returns lands in whatever segment is
/// current then, which is never one being retired (see `advance_log_number`).
///
/// A **non-empty** active memtable still answers `active_log`, which may be older than the
/// segment its data is really in for the same stale-`active_log` reason. That direction is
/// safe: it retains more than it must, never less.
fn oldest_log(mem: &MemState) -> u64 {
    if let Some((_, log)) = mem.immutable.first() {
        return *log;
    }
    if mem.active.is_empty() {
        return u64::MAX;
    }
    mem.active_log
}
