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
use crate::range_del::RangeTombstones;
use crate::sst::{TableBuilder, TableOptions};
use crate::version::{CfVersion, FileLocation, FileMeta, Version, VersionEdit};

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
                // The same discharge rule as the background loop: a tombstone in the inputs
                // takes every file it covers ([ADR 0017] decision 6). An explicit
                // `compact_range` is in fact how an operator forces one to happen now.
                let tombstones = self.inner.tombstones_of(&handle, &compaction)?;
                let compaction = if tombstones.is_empty() {
                    compaction
                } else if Picker::can_discharge(&tombstones, self.inner.compaction_floor()) {
                    picker.discharge(cf_version, compaction, &tombstones)
                } else {
                    // The picker would keep offering this same compaction, so leaving the
                    // level is the only way out that is not a spin. The tombstone stays in L0
                    // and is honoured from there.
                    break;
                };
                if !self.inner.reserve(&compaction)? {
                    // Someone else has these files. Wait for them rather than spin.
                    self.inner.wait_for_compaction()?;
                    continue;
                }
                let outcome =
                    self.inner
                        .run_compaction(&handle, &version, &compaction, &picker, &tombstones);
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
            // A tombstone in the inputs turns this into a discharge: it takes every file the
            // tombstone covers, at every level, applies it and drops it
            // ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 6). This is
            // what the *scheduler* does with a `delete_range`; the write that produced the
            // tombstone acknowledged long ago, and reads have been honouring it out of the
            // memtable and L0 ever since (invariant 1 is untouched).
            let tombstones = self.tombstones_of(&cf, &compaction)?;
            let compaction = if tombstones.is_empty() {
                compaction
            } else if Picker::can_discharge(&tombstones, self.compaction_floor()) {
                picker.discharge(cf_version, compaction, &tombstones)
            } else {
                // A snapshot older than the delete is still open. Leave the tombstone in L0,
                // where reads honour it, and try again when that snapshot goes.
                continue;
            };
            if !self.reserve(&compaction)? {
                continue; // Someone else has these files; the picker will offer them again.
            }
            let outcome = self.run_compaction(&cf, &version, &compaction, &picker, &tombstones);
            self.release(&compaction)?;
            outcome?;
            return Ok(true);
        }
        Ok(false)
    }

    /// The range tombstones a compaction's inputs carry.
    ///
    /// Only L0 files can hold any, because a discharge drops them rather than propagating them
    /// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 6) — so this is a
    /// no-op for every compaction below L0, and the `debug_assert` is what keeps the claim
    /// honest rather than assumed.
    fn tombstones_of(
        &self,
        cf: &Arc<ColumnFamily>,
        compaction: &Compaction,
    ) -> Result<RangeTombstones> {
        let table_options = self.table_options(cf);
        let user = self.comparator.user_comparator();
        let mut tombstones = RangeTombstones::new();
        for file in compaction.all_inputs() {
            let reader = self.table_cache.get(file.number, &table_options)?;
            if reader.range_tombstones().is_empty() {
                continue;
            }
            debug_assert_eq!(
                compaction.level, 0,
                "a range tombstone in a file below L0: file {} at level {}",
                file.number, compaction.level
            );
            tombstones.extend(reader.range_tombstones(), user.as_ref());
        }
        Ok(tombstones)
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
        tombstones: &RangeTombstones,
    ) -> Result<()> {
        let mut edit = VersionEdit::new();
        for file in &compaction.inputs {
            edit.delete_file(compaction.cf, level_u32(compaction.level), file.number);
        }
        // `outputs_overlapped` is always `level + 1`, which is the output level only when this
        // is not a discharge — a discharge writes deeper, so the level has to be named rather
        // than inferred.
        for file in &compaction.outputs_overlapped {
            edit.delete_file(compaction.cf, level_u32(compaction.level + 1), file.number);
        }
        for (level, file) in &compaction.discharge {
            edit.delete_file(compaction.cf, level_u32(*level), file.number);
        }

        // A move is only trivial while the output would be byte-identical. A compaction
        // filter is entitled to change what is written, so with one configured the shortcut
        // would quietly skip it — and a caller who asked for a compaction expecting the filter
        // to run would get a no-op.
        // A move is only trivial while the output would be byte-identical, and a tombstone in
        // the inputs guarantees it would not be: the whole point of taking it is to apply it
        // and drop it. Moving the file instead would carry the tombstone to L1 and break the
        // invariant that nothing below L0 holds one.
        let trivial = compaction.is_trivial_move()
            && cf.options().compaction_filter.is_none()
            && tombstones.is_empty();
        // Kept past the branch: once the edit naming them is durable, these are the numbers
        // the register has no further reason to hold.
        let mut outputs: Vec<FileMeta> = Vec::new();
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
            let (produced, stats) =
                self.merge_inputs(cf, version, compaction, picker, tombstones)?;
            for file in &produced {
                edit.add_file(
                    compaction.cf,
                    level_u32(compaction.output_level()),
                    file.clone(),
                );
            }
            self.remember_pointer(compaction, &produced)?;
            outputs = produced;
            stats
        };

        let applied = self.log_and_apply_compaction(compaction, &mut edit)?;
        // Either a version names them now, or nothing ever will: both mean they have stopped
        // being files "being written", and in the second case the sweep is what reclaims them.
        self.forget_pending(&outputs)?;
        if applied {
            // After the edit. A compaction that was dropped produced files no version names,
            // and uploading those would be uploading garbage for the sweep to delete.
            for file in &outputs {
                self.note_durable_sst(file.number);
            }
        }
        self.purge_and_evict()?;
        if !applied {
            tracing::debug!(
                cf = compaction.cf,
                level = compaction.level,
                "dropped a compaction whose inputs had already been compacted away"
            );
            return Ok(());
        }
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

    /// Installs a compaction's edit, unless the plan has gone stale.
    ///
    /// A plan is *picked* against a pinned version and *applied* against `current`, and the two
    /// can differ. The reservation that keeps two compactions off one file is taken after the
    /// pick, so a compaction that finished and released its files in between leaves this plan
    /// naming files `current` no longer holds. Applying it then fails the manifest builder with
    /// `an edit deleted N files that were not there` — a corruption error for what is really
    /// just a lost race, surfacing to whoever called `compact_range`.
    ///
    /// The check and the apply are under one lock acquisition, so nothing can move between
    /// them. `Ok(false)` means the plan was stale and nothing was written; the picker will
    /// offer a fresh one against the version that moved on, which is the same thing `reserve`
    /// returning false already does one step earlier.
    fn log_and_apply_compaction(
        &self,
        compaction: &Compaction,
        edit: &mut VersionEdit,
    ) -> Result<bool> {
        let mut versions = lock(&self.versions)?;
        let current = versions.current();
        let holds = |level: usize, number: u64| {
            current
                .files(compaction.cf, level)
                .iter()
                .any(|file| file.number == number)
        };
        let stale = compaction
            .inputs
            .iter()
            .any(|file| !holds(compaction.level, file.number))
            || compaction
                .outputs_overlapped
                .iter()
                .any(|file| !holds(compaction.level + 1, file.number))
            || compaction
                .discharge
                .iter()
                .any(|(level, file)| !holds(*level, file.number));
        if stale {
            return Ok(false);
        }
        versions.set_last_seqno(self.visible_seqno.load(Ordering::Acquire));
        versions.log_and_apply(edit)?;
        Ok(true)
    }

    /// Merges the inputs into new files at the output level.
    fn merge_inputs(
        &self,
        cf: &Arc<ColumnFamily>,
        version: &Version,
        compaction: &Compaction,
        picker: &Picker,
        tombstones: &RangeTombstones,
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
        // Measured from the *output* level, which a discharge pushes deeper than `level + 1`.
        // Asking about `level` would answer for a level the outputs are not going to, and a
        // tombstone dropped on that answer resurrects a key.
        let is_bottom = |user_key: &[u8]| {
            picker.is_bottom_level_for_key(cf_version, compaction.output_level() - 1, user_key)
        };

        let mut output = TableWriter::new(self, table_options);
        let job = CompactionJob {
            comparator: &self.comparator,
            floor: self.compaction_floor(),
            level: compaction.level,
            target_file_size: compaction.target_file_size,
            filter: cf.options().compaction_filter.as_deref(),
            is_bottom: &is_bottom,
            tombstones,
        };
        match job.run(&mut input, &mut output) {
            Ok(stats) => Ok((output.finished, stats)),
            Err(err) => {
                // No edit will ever name what was written, so stop holding it: reclaiming a
                // half-finished compaction is the sweep's job and the sweep skips anything
                // still registered. The original error is what the caller needs to see, so a
                // failure to release is not allowed to replace it.
                let _unused = output.release_held();
                Err(err)
            }
        }
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

    /// Stops holding a finished compaction's **outputs**.
    ///
    /// Only ever called once the edit naming them is durable: until then they belong to no
    /// version and the sweep would take them for garbage, which is the whole reason the
    /// register exists. Afterwards a version names them, and holding them any longer costs
    /// twice over — the register grows for the life of the process, and the sweep skips every
    /// number in it, so those files are never reclaimed however obsolete they become.
    ///
    /// The *inputs* need no release. They were named by a version from the start and were
    /// never registered, so retaining against them removed nothing and left every output held.
    fn forget_pending(&self, outputs: &[FileMeta]) -> Result<()> {
        let mut pending = lock(&self.pending_outputs)?;
        for file in outputs {
            pending.remove(&file.number);
        }
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
        let (obsolete, live, pending) = {
            let mut versions = lock(&self.versions)?;
            let obsolete = versions.obsolete_files()?;
            // Taken under the same lock as the listing, because object deletion needs the same
            // instant. It cannot be derived from the listing: a file the tier evicted is not
            // in the directory at all, so its object would never be reclaimed
            // (ADR 0024 decision 5).
            let live = versions.live_file_numbers();
            #[cfg(any(test, feature = "testing"))]
            self.pause_at(crate::testing::PausePoint::SweptDirectoryBeforePending);
            let pending: BTreeSet<u64> = lock(&self.pending_outputs)?.clone();
            (obsolete, live, pending)
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
        if let Some(tier) = self.fs.tier()
            && let Err(err) = tier.retain(&live, &pending)
        {
            // An object we failed to delete is leaked, not lost. Failing the sweep over it
            // would turn a storage cost into an availability one.
            tracing::warn!(error = %err, "reclaiming obsolete objects failed");
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
    /// Every number this writer has held, so a compaction that fails part-way can let them go.
    /// Nothing will ever name those files, and a register that keeps them stops the sweep
    /// reclaiming them.
    held: Vec<u64>,
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
            held: Vec::new(),
        }
    }

    /// Releases every number this writer held.
    ///
    /// For the failure path only. On success the outputs are released by
    /// [`DbInner::forget_pending`] *after* the edit that names them is durable, which is
    /// strictly later than this writer is dropped — releasing here instead would hand the
    /// sweep a file the compaction is about to name.
    fn release_held(&self) -> Result<()> {
        let mut pending = lock(&self.inner.pending_outputs)?;
        for number in &self.held {
            pending.remove(number);
        }
        Ok(())
    }
}

impl CompactionOutput for TableWriter<'_> {
    fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.builder.is_none() {
            let number = lock(&self.inner.versions)?.new_file_number();
            // Held before the file exists, so no sweep can see it unreferenced and delete it.
            self.inner.hold_pending(number)?;
            self.held.push(number);
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
            location: FileLocation::Local,
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fs::FileSystem;
    use crate::memfs::MemFileSystem;
    use crate::options::{CfOptions, Options, ReadOptions};
    use crate::{Db, cf};

    /// **Regression.** A finished compaction has to let go of the outputs it held.
    ///
    /// The register of files being written is what stops one thread's sweep deleting another
    /// thread's half-written output, so the sweep skips every number in it. Releasing the
    /// wrong numbers — the *inputs*, which a version named from the start and which were never
    /// registered — removed nothing and left every output held for the life of the process:
    /// the register grew without bound, and the sweep could never reclaim those files however
    /// obsolete they became.
    ///
    /// # Why this drives one compaction by hand
    ///
    /// `compact_range` walks every level, so the file an L0→L1 compaction produces becomes the
    /// *input* of the L1→L2 pass a moment later — and the old code did release input numbers,
    /// so the register drained anyway and the bug stayed invisible. The leak is what happens to
    /// an output that is **not** immediately compacted again, which is the ordinary case for
    /// the background pool: one L0→L1 compaction, and its output sits at L1 until the level
    /// outgrows its target. So this runs exactly that, once.
    #[test]
    fn a_finished_compaction_releases_the_outputs_it_held() {
        let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
        let db = Db::open_with(
            "/db",
            Options {
                create_if_missing: true,
                cf_options: CfOptions {
                    // Out of reach, so the background pool cannot run a compaction of its own
                    // and be mid-flight, holding a number, when the register is read.
                    level0_file_num_compaction_trigger: 100,
                    ..CfOptions::default()
                },
                ..Options::default()
            },
            Arc::clone(&fs),
            &[cf::DEFAULT],
        )
        .expect("a fresh database");

        // Three overlapping generations, so the compaction rewrites rather than moving a file
        // down a level: a trivial move produces no output and would not exercise this at all.
        for generation in 0..3u32 {
            for i in 0..40u32 {
                db.put(
                    cf::DEFAULT,
                    format!("k{i:03}").as_bytes(),
                    format!("g{generation}").as_bytes(),
                )
                .expect("a put");
            }
            db.flush(cf::DEFAULT).expect("a flush");
        }
        drain(&db, "after the flushes");

        // Exactly one L0 → L1 compaction, and nothing after it to take the output as an input.
        let handle = db
            .inner
            .cf_by_name(cf::DEFAULT)
            .expect("the default family");
        let picker = db.inner.picker(&handle);
        let version = lock(&db.inner.versions).expect("the versions").current();
        let cf_version = version.cf(handle.id()).expect("the family's files");
        let compaction = picker
            .pick_range(handle.id(), cf_version, 0, None, None)
            .expect("three L0 files to merge");
        assert!(
            !compaction.is_trivial_move(),
            "the compaction has to write an output for there to be anything to release"
        );
        db.inner
            .run_compaction(
                &handle,
                &version,
                &compaction,
                &picker,
                &RangeTombstones::new(),
            )
            .expect("the compaction");

        drain(&db, "after the compaction");

        // And the data is untouched, which is what says the release was not a sweep quietly
        // deleting live files.
        for i in 0..40u32 {
            let key = format!("k{i:03}");
            assert_eq!(
                db.get(cf::DEFAULT, key.as_bytes(), &ReadOptions::default())
                    .expect("a read")
                    .as_deref(),
                Some(&b"g2"[..]),
                "{key}"
            );
        }
    }

    /// **Regression.** A compaction plan whose inputs have already gone is dropped, not applied.
    ///
    /// A plan is picked against a pinned version and applied against `current`. The reservation
    /// that keeps two compactions off one file is taken *after* the pick, so this order is
    /// reachable with two compaction threads:
    ///
    /// ```text
    ///   T2  pins V, picks a plan over {A, B}          (no reservation yet)
    ///   T1  pins V, picks {A, B}, reserves, compacts, commits, releases
    ///   T2  reserves — and succeeds, because T1 has let go
    ///   T2  applies a plan naming A and B, which `current` no longer has
    /// ```
    ///
    /// The last step used to fail the manifest builder with `an edit deleted 2 files that were
    /// not there`, and that error came back out of whatever called `compact_range` — a
    /// corruption report for what is only a lost race. This drives the same shape directly,
    /// which is exact where two threads racing would be a coin flip.
    #[test]
    fn a_stale_compaction_plan_is_dropped_rather_than_applied() {
        let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
        let db = Db::open_with(
            "/db",
            Options {
                create_if_missing: true,
                cf_options: CfOptions {
                    level0_file_num_compaction_trigger: 100,
                    ..CfOptions::default()
                },
                ..Options::default()
            },
            Arc::clone(&fs),
            &[cf::DEFAULT],
        )
        .expect("a fresh database");

        for generation in 0..3u32 {
            for i in 0..40u32 {
                db.put(
                    cf::DEFAULT,
                    format!("k{i:03}").as_bytes(),
                    format!("g{generation}").as_bytes(),
                )
                .expect("a put");
            }
            db.flush(cf::DEFAULT).expect("a flush");
        }

        // The plan T2 picked, against the version as it stands now.
        let handle = db
            .inner
            .cf_by_name(cf::DEFAULT)
            .expect("the default family");
        let picker = db.inner.picker(&handle);
        let pinned = lock(&db.inner.versions).expect("the versions").current();
        let cf_version = pinned.cf(handle.id()).expect("the family's files");
        let stale = picker
            .pick_range(handle.id(), cf_version, 0, None, None)
            .expect("three L0 files to merge");

        // T1 gets there first and takes those same files away.
        db.compact_range(cf::DEFAULT, None, None)
            .expect("the winning compaction");
        let after_winner = db.compactions_run();

        // T2 applies its plan. Dropping it is the only correct answer: every file it names is
        // gone, so there is nothing left to do and nothing to report.
        db.inner
            .run_compaction(&handle, &pinned, &stale, &picker, &RangeTombstones::new())
            .expect("a stale plan is a lost race, not a corrupt manifest");
        assert_eq!(
            db.compactions_run(),
            after_winner,
            "a dropped plan must not be counted as a compaction that ran"
        );

        drain(&db, "after the stale plan was dropped");

        // The winner's work stands, undisturbed.
        for i in 0..40u32 {
            let key = format!("k{i:03}");
            assert_eq!(
                db.get(cf::DEFAULT, key.as_bytes(), &ReadOptions::default())
                    .expect("a read")
                    .as_deref(),
                Some(&b"g2"[..]),
                "{key}"
            );
        }

        // And the database still opens, which is what says the manifest was left consistent.
        drop(db);
        let reopened = Db::open_with("/db", Options::default(), Arc::clone(&fs), &[cf::DEFAULT])
            .expect("a reopen");
        assert_eq!(
            reopened
                .get(cf::DEFAULT, b"k000", &ReadOptions::default())
                .expect("a read")
                .as_deref(),
            Some(&b"g2"[..])
        );
    }

    /// Waits for the register of files being written to empty.
    ///
    /// A deadline rather than an instant reading, because a flush releases its own number a
    /// moment after `flush` returns and an exact reading would race it. What has to be true is
    /// that the register empties at all.
    fn drain(db: &Db, when: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let held = db.inner.pending_outputs.lock().expect("the register").len();
            if held == 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the register still holds {held} number(s) {when}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
