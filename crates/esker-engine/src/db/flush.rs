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

use crate::dbformat::{Comparator, InternalPrefixExtractor, SeqNo, extract_tag, tag_seqno};
use crate::error::{Error, IoResultExt, Result};
use crate::filename::{self, FileKind};
use crate::memtable::MemTable;
use crate::sst::{TableBuilder, TableOptions};
use crate::version::{FileMeta, VersionEdit};
use crate::wal::LogWriter;

use super::{ColumnFamily, Db, DbInner, MemState, lock, read_lock, write_lock};

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
        self.inner.switch_memtable_of(&cf)?;
        self.inner.signal_flush();
        self.inner.wait_for_flush(&cf)
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
        let writer = LogWriter::new(file, path.display().to_string());

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
    pub(crate) fn wait_for_flush(&self, cf: &Arc<ColumnFamily>) -> Result<()> {
        loop {
            if read_lock(&cf.mem)?.immutable.is_empty() {
                return Ok(());
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

        let mut edit = VersionEdit::new();
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

        // Only now: until the edit is durable, this memtable is the only copy of that data.
        {
            let mut mem = write_lock(&cf.mem)?;
            if !mem.immutable.is_empty() {
                mem.immutable.remove(0);
            }
        }
        self.advance_log_number()?;
        self.purge_and_evict()?;
        self.flush_done.notify_all();
        Ok(())
    }

    /// Builds an SST from `table`. `Ok(None)` means the table was empty and no file was made.
    fn build_table(
        &self,
        cf: &Arc<ColumnFamily>,
        table: &Arc<MemTable>,
        number: u64,
    ) -> Result<Option<FileMeta>> {
        let path = filename::sst(&self.dir, number);
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

        let Some(smallest) = smallest else {
            // Nothing to write. Drop the builder without finishing it and take the file away
            // again, rather than leaving a zero-entry SST for compaction to trip over.
            drop(builder);
            let _ = self.fs.delete(&path);
            return Ok(None);
        };

        // The SST layer cannot read a sequence number out of a key — that would mean
        // understanding one, which invariant 7 forbids — so the engine hands them over.
        builder.set_seqno_range(smallest_seqno, largest_seqno);
        let properties = builder.finish()?;
        Ok(Some(FileMeta {
            number,
            size: properties.file_size,
            smallest,
            largest,
            smallest_seqno,
            largest_seqno,
        }))
    }

    /// How a column family's tables are built and read. The same options must be used for
    /// both, which is why there is one function producing them.
    pub(crate) fn table_options(&self, cf: &ColumnFamily) -> TableOptions {
        let options = cf.options();
        TableOptions {
            block_size: options.block_size,
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
        let families: Vec<Arc<ColumnFamily>> = read_lock(&self.cfs)?.values().cloned().collect();
        let mut oldest = u64::MAX;
        for cf in &families {
            let mem = read_lock(&cf.mem)?;
            oldest = oldest.min(oldest_log(&mem));
        }
        if oldest == u64::MAX {
            return Ok(());
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

    /// Deletes what no version needs, forgetting any table reader for a file that goes.
    fn purge_and_evict(&self) -> Result<()> {
        let deleted = lock(&self.versions)?.purge_obsolete_files()?;
        for path in &deleted {
            if let Some(FileKind::Sst(number)) = filename::classify_path(path) {
                self.table_cache.evict(number);
            }
        }
        Ok(())
    }
}

/// The oldest log segment a column family's memtables still depend on.
fn oldest_log(mem: &MemState) -> u64 {
    mem.immutable
        .first()
        .map_or(mem.active_log, |(_, log)| *log)
}
