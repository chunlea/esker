//! Running compactions against real files.
//!
//! [`crate::compaction`] decides *what* and *how*; this is the half that owns file numbers,
//! the manifest and the threads. Keeping them apart is what lets every rule that can lose data
//! be tested on a list in memory.
//!
//! # Two compactions must not touch one file
//!
//! The pool is bounded but not serial (`docs/DESIGN.md` §4.7: two threads), and a compaction
//! at `L → L+1` reads files at both levels — so one at `L+1 → L+2` can want the same files.
//! Every plan therefore reserves its inputs by file number before it starts, and a plan that
//! cannot have all of them is dropped rather than queued: the picker will produce it again in
//! a moment, against a version that has moved on.
//!
//! # A file being written is not garbage
//!
//! An output file exists on disk before the manifest edit that names it, so for that window it
//! belongs to no version — which is exactly what the obsolete-file sweep deletes. Outputs are
//! registered as pending for that window, by both compaction and flush, and the sweep leaves
//! them alone. Without it a second thread's sweep deletes a file the first is still writing.
//!
//! The register only works if it is read at the same instant as the directory listing it
//! qualifies. Sampled one after the other the two describe different moments, and a flush that
//! installs its edit in between falls through the gap: it was in no version when the directory
//! was read, and is no longer pending by the time the register is. The sweep therefore takes
//! both samples under the version lock, which is the lock installing an edit needs.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::compaction::{Compaction, CompactionJob, CompactionOutput, CompactionStats, Picker};
use crate::dbformat::{SeqNo, extract_tag, tag_seqno};
use crate::error::{Error, IoResultExt, Result};
use crate::filename::{self, FileKind};
use crate::iterator::Cursor;
use crate::sst::{TableBuilder, TableOptions};
use crate::version::{CfVersion, FileMeta, Version, VersionEdit};

use super::iter::table_cursor;
use super::merge::MergeCursor;
use super::{ColumnFamily, Db, DbInner, lock, read_lock};

impl Db {
    /// Compacts everything in `[begin, end]` down through the levels.
    ///
    /// `None` for either bound means unbounded on that side; a range that ends before it
    /// begins is refused. Flushes the column family first, so that "compact this range" means
    /// all of it and not just the part already on disk.
    ///
    /// Synchronous: it returns when the range has been compacted, which is what makes it
    /// usable from a test and from `esker-cli`.
    pub fn compact_range(&self, cf: &str, begin: Option<&[u8]>, end: Option<&[u8]>) -> Result<()> {
        let handle = self.inner.cf_by_name(cf)?;
        // An inverted range is a caller's mistake, and silently compacting the files that
        // happen to span the gap would hide it.
        if let (Some(begin), Some(end)) = (begin, end)
            && self.inner.comparator.user_comparator().cmp(begin, end)
                == std::cmp::Ordering::Greater
        {
            return Err(Error::InvalidArgument(
                "compact_range was given a range that ends before it begins".to_string(),
            ));
        }
        self.flush(cf)?;
        let picker = self.inner.picker(&handle);

        // Each pass moves at least one file out of the level, so the level's overlap with the
        // range strictly shrinks and this terminates.
        for level in 0..picker_levels(&self.inner, &handle)?.saturating_sub(1) {
            loop {
                let version = lock(&self.inner.versions)?.current();
                let Some(cf_version) = version.cf(handle.id()) else {
                    break;
                };
                let Some(compaction) =
                    picker.pick_range(handle.id(), cf_version, level, begin, end)
                else {
                    break;
                };
                if !self.inner.reserve(&compaction)? {
                    // Someone else has these files. Wait for them rather than spin.
                    self.inner.wait_for_compaction()?;
                    continue;
                }
                let outcome = self
                    .inner
                    .run_compaction(&handle, &version, &compaction, &picker);
                self.inner.release(&compaction)?;
                outcome?;
            }
        }
        Ok(())
    }

    /// How many compactions have finished, for tests and for `esker-cli`.
    pub fn compactions_run(&self) -> u64 {
        self.inner.compactions.load(Ordering::Relaxed)
    }
}

/// The number of levels a column family has, from the live version.
fn picker_levels(inner: &DbInner, cf: &Arc<ColumnFamily>) -> Result<usize> {
    let version = lock(&inner.versions)?.current();
    Ok(version.cf(cf.id()).map_or(0, CfVersion::num_levels))
}

impl DbInner {
    /// A picker configured for one column family.
    pub(crate) fn picker(&self, cf: &Arc<ColumnFamily>) -> Picker {
        Picker::new(cf.options().clone(), Arc::clone(&self.comparator))
    }

    /// Runs one compaction if any column family wants one. `Ok(false)` means none did.
    pub(crate) fn maybe_compact(&self) -> Result<bool> {
        let families: Vec<Arc<ColumnFamily>> = read_lock(&self.cfs)?.values().cloned().collect();
        for cf in families {
            let version = lock(&self.versions)?.current();
            let Some(cf_version) = version.cf(cf.id()) else {
                continue;
            };
            let picker = self.picker(&cf);
            let pointers = self.compact_pointers(cf.id())?;
            let Some(compaction) = picker.pick(cf.id(), cf_version, &pointers) else {
                continue;
            };
            if !self.reserve(&compaction)? {
                continue; // Someone else has these files; the picker will offer them again.
            }
            let outcome = self.run_compaction(&cf, &version, &compaction, &picker);
            self.release(&compaction)?;
            outcome?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Claims every input of `compaction`, or nothing at all.
    fn reserve(&self, compaction: &Compaction) -> Result<bool> {
        let mut busy = lock(&self.compacting)?;
        if compaction
            .all_inputs()
            .any(|file| busy.contains(&file.number))
        {
            return Ok(false);
        }
        for file in compaction.all_inputs() {
            busy.insert(file.number);
        }
        Ok(true)
    }

    fn release(&self, compaction: &Compaction) -> Result<()> {
        let mut busy = lock(&self.compacting)?;
        for file in compaction.all_inputs() {
            busy.remove(&file.number);
        }
        drop(busy);
        self.compaction_done.notify_all();
        Ok(())
    }

    /// Blocks briefly until some compaction finishes, so a caller can look again.
    fn wait_for_compaction(&self) -> Result<()> {
        let state = lock(&self.compact)?;
        let _unused = self
            .compaction_done
            .wait_timeout(state, std::time::Duration::from_millis(20))
            .map_err(|_| Error::Poisoned("the compaction lock was poisoned".to_string()))?;
        Ok(())
    }

    fn compact_pointers(&self, cf: u32) -> Result<Vec<Option<Vec<u8>>>> {
        let pointers = lock(&self.compact_pointers)?;
        Ok((0..self.options.num_levels)
            .map(|level| pointers.get(&(cf, level)).cloned())
            .collect())
    }

    /// Does one compaction: merge, write, and install the edit that swaps the files.
    fn run_compaction(
        &self,
        cf: &Arc<ColumnFamily>,
        version: &Version,
        compaction: &Compaction,
        picker: &Picker,
    ) -> Result<()> {
        let mut edit = VersionEdit::new();
        for file in &compaction.inputs {
            edit.delete_file(compaction.cf, level_u32(compaction.level), file.number);
        }
        for file in &compaction.outputs_overlapped {
            edit.delete_file(
                compaction.cf,
                level_u32(compaction.output_level()),
                file.number,
            );
        }

        // A move is only trivial while the output would be byte-identical. A compaction
        // filter is entitled to change what is written, so with one configured the shortcut
        // would quietly skip it — and a caller who asked for a compaction expecting the filter
        // to run would get a no-op.
        let trivial = compaction.is_trivial_move() && cf.options().compaction_filter.is_none();
        let stats = if trivial {
            // The bytes would come out identical under a different number, so only the
            // manifest has any work to do.
            let file = &compaction.inputs[0];
            edit.add_file(
                compaction.cf,
                level_u32(compaction.output_level()),
                (**file).clone(),
            );
            tracing::debug!(
                cf = compaction.cf,
                file = file.number,
                to = compaction.output_level(),
                "moved a file down a level without rewriting it"
            );
            CompactionStats::default()
        } else {
            let (outputs, stats) = self.merge_inputs(cf, version, compaction, picker)?;
            for file in &outputs {
                edit.add_file(
                    compaction.cf,
                    level_u32(compaction.output_level()),
                    file.clone(),
                );
            }
            self.remember_pointer(compaction, &outputs)?;
            stats
        };

        self.log_and_apply(&mut edit)?;
        // The inputs are unreferenced now, and any output that did not make it into the edit
        // is unreferenced too.
        self.forget_pending(compaction)?;
        self.purge_and_evict()?;
        self.compactions.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            cf = compaction.cf,
            level = compaction.level,
            read = stats.entries_read,
            written = stats.entries_written,
            files = stats.files_written,
            "compaction finished"
        );
        Ok(())
    }

    /// Merges the inputs into new files at the output level.
    fn merge_inputs(
        &self,
        cf: &Arc<ColumnFamily>,
        version: &Version,
        compaction: &Compaction,
        picker: &Picker,
    ) -> Result<(Vec<FileMeta>, CompactionStats)> {
        let table_options = self.table_options(cf);
        let mut children: Vec<Box<dyn Cursor + Send>> = Vec::new();
        // A compaction reads every entry of every input, so the bloom filter would not help
        // here even if the cursor consulted it — unlike the point-read path, which has the
        // same gap and does care. See `TODO(post-v1)` in `db/read.rs`.
        for file in compaction.all_inputs() {
            let reader = self.table_cache.get(file.number, &table_options)?;
            children.push(table_cursor(reader.iter()));
        }
        let mut input = MergeCursor::new(children, Arc::clone(&self.comparator));

        let cf_version = version
            .cf(compaction.cf)
            .ok_or_else(|| Error::InvalidArgument("the column family vanished".to_string()))?;
        let is_bottom = |user_key: &[u8]| {
            picker.is_bottom_level_for_key(cf_version, compaction.level, user_key)
        };

        let mut output = TableWriter::new(self, table_options);
        let job = CompactionJob {
            comparator: &self.comparator,
            floor: self.compaction_floor(),
            level: compaction.level,
            target_file_size: compaction.target_file_size,
            filter: cf.options().compaction_filter.as_deref(),
            is_bottom: &is_bottom,
        };
        let stats = job.run(&mut input, &mut output)?;
        Ok((output.finished, stats))
    }

    /// Records where this compaction stopped, so the next one starts after it.
    fn remember_pointer(&self, compaction: &Compaction, outputs: &[FileMeta]) -> Result<()> {
        let Some(last) = outputs.last() else {
            return Ok(());
        };
        let mut pointers = lock(&self.compact_pointers)?;
        pointers.insert(
            (compaction.cf, compaction.level),
            crate::dbformat::extract_user_key(&last.largest).to_vec(),
        );
        Ok(())
    }

    /// Registers `number` as a file being written, so the sweep leaves it alone.
    pub(crate) fn hold_pending(&self, number: u64) -> Result<()> {
        lock(&self.pending_outputs)?.insert(number);
        Ok(())
    }

    /// Stops holding `number`. Safe to call for a number that was never held.
    pub(crate) fn drop_pending(&self, number: u64) -> Result<()> {
        lock(&self.pending_outputs)?.remove(&number);
        Ok(())
    }

    fn forget_pending(&self, compaction: &Compaction) -> Result<()> {
        let mut pending = lock(&self.pending_outputs)?;
        pending.retain(|number| !compaction.all_inputs().any(|file| file.number == *number));
        Ok(())
    }

    /// Deletes what no version needs, keeping files that are still being written.
    pub(crate) fn purge_and_evict(&self) -> Result<()> {
        // Both halves of this decision have to describe the same instant. The listing says
        // which files no live version needs; `pending_outputs` says which of those are outputs
        // that have been created but not yet named by an edit. Sampled one after the other
        // they can disagree: a flush that installs its edit in between was not in the version
        // when the directory was read, and is no longer pending by the time the set is — so
        // its output looks like garbage twice over and the sweep deletes a file the *current*
        // version references. Installing an edit takes the version lock, so holding it across
        // both samples is what makes them one instant.
        let (obsolete, pending) = {
            let mut versions = lock(&self.versions)?;
            let obsolete = versions.obsolete_files()?;
            let pending: BTreeSet<u64> = lock(&self.pending_outputs)?.clone();
            (obsolete, pending)
        };
        for path in obsolete {
            if let Some(FileKind::Sst(number)) = filename::classify_path(&path) {
                if pending.contains(&number) {
                    // Written but not yet named by any version. Deleting it here is how a
                    // second thread's sweep removes a file the first is still writing.
                    continue;
                }
                self.table_cache.evict(number);
            }
            match self.fs.delete(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io(&path, err)),
            }
        }
        Ok(())
    }

    /// The background compaction thread's body. Several run; the reservation set keeps them
    /// off each other's files.
    pub(crate) fn compaction_loop(&self) {
        loop {
            {
                let Ok(mut state) = self.compact.lock() else {
                    return;
                };
                while !state.wanted && !self.shutdown.load(Ordering::Acquire) {
                    let Ok((next, _)) = self
                        .compact_wanted
                        .wait_timeout(state, std::time::Duration::from_millis(50))
                    else {
                        return;
                    };
                    state = next;
                }
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                state.wanted = false;
            }

            match self.maybe_compact() {
                // More may be waiting: ask again rather than sleeping on it.
                Ok(true) => self.signal_compaction(),
                Ok(false) => {}
                Err(err) => {
                    tracing::error!(error = %err, "compaction failed");
                    if let Ok(mut state) = self.compact.lock() {
                        state.error.get_or_insert_with(|| err.to_string());
                    }
                }
            }
            self.compaction_done.notify_all();
        }
    }

    /// Wakes a compaction thread.
    pub(crate) fn signal_compaction(&self) {
        if let Ok(mut state) = self.compact.lock() {
            state.wanted = true;
        }
        self.compact_wanted.notify_all();
    }
}

fn level_u32(level: usize) -> u32 {
    u32::try_from(level).unwrap_or(u32::MAX)
}

/// Writes a compaction's output as SSTs, cutting a new file at the target size.
struct TableWriter<'a> {
    inner: &'a DbInner,
    options: TableOptions,
    builder: Option<(u64, TableBuilder)>,
    smallest: Option<Vec<u8>>,
    largest: Vec<u8>,
    smallest_seqno: SeqNo,
    largest_seqno: SeqNo,
    finished: Vec<FileMeta>,
}

impl<'a> TableWriter<'a> {
    fn new(inner: &'a DbInner, options: TableOptions) -> Self {
        Self {
            inner,
            options,
            builder: None,
            smallest: None,
            largest: Vec::new(),
            smallest_seqno: SeqNo::MAX,
            largest_seqno: 0,
            finished: Vec::new(),
        }
    }
}

impl CompactionOutput for TableWriter<'_> {
    fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.builder.is_none() {
            let number = lock(&self.inner.versions)?.new_file_number();
            // Held before the file exists, so no sweep can see it unreferenced and delete it.
            self.inner.hold_pending(number)?;
            let path = filename::sst(&self.inner.dir, number);
            let file = self.inner.fs.create(&path).at(&path)?;
            self.builder = Some((number, TableBuilder::new(self.options.clone(), file)));
            self.smallest = None;
            self.largest.clear();
            self.smallest_seqno = SeqNo::MAX;
            self.largest_seqno = 0;
        }

        if self.smallest.is_none() {
            self.smallest = Some(key.to_vec());
        }
        self.largest.clear();
        self.largest.extend_from_slice(key);
        if let Some(tag) = extract_tag(key) {
            let seqno = tag_seqno(tag);
            self.smallest_seqno = self.smallest_seqno.min(seqno);
            self.largest_seqno = self.largest_seqno.max(seqno);
        }

        let Some((_, builder)) = self.builder.as_mut() else {
            return Err(Error::Poisoned("the output builder vanished".to_string()));
        };
        builder.add(key, value)
    }

    fn current_file_size(&self) -> u64 {
        self.builder
            .as_ref()
            .map_or(0, |(_, builder)| builder.file_size())
    }

    fn finish_file(&mut self) -> Result<bool> {
        let Some((number, mut builder)) = self.builder.take() else {
            return Ok(false);
        };
        let Some(smallest) = self.smallest.take() else {
            // Nothing was added, so there is no file worth keeping.
            drop(builder);
            let _ = self
                .inner
                .fs
                .delete(&filename::sst(&self.inner.dir, number));
            self.inner.drop_pending(number)?;
            return Ok(false);
        };
        // The SST layer cannot read a sequence number out of a key, so the engine supplies the
        // range (invariant 7). Before `finish`, which is what writes the properties.
        builder.set_seqno_range(self.smallest_seqno, self.largest_seqno);
        let properties = builder.finish()?;
        self.finished.push(FileMeta {
            number,
            size: properties.file_size,
            smallest,
            largest: std::mem::take(&mut self.largest),
            smallest_seqno: self.smallest_seqno,
            largest_seqno: self.largest_seqno,
        });
        Ok(true)
    }
}
