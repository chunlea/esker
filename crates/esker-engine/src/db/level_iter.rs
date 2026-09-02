//! One cursor over a whole level, opening the file it has reached.
//!
//! Every level below L0 **partitions** the key space: its files are sorted by smallest key and
//! none overlaps another (`docs/DESIGN.md` §4.6). So at any moment a scan of that level is inside
//! exactly one file, and the level needs exactly one open reader — which is `LevelDB`'s two-level
//! iterator, an iterator over the file index driving an iterator over the file it has reached.
//!
//! # What this replaces
//!
//! `Db::iter` used to push one cursor per file, for every file, at every level. Opening a table
//! reads its footer, properties, filter and index — four small reads — and holds the index and
//! filter for the reader's life. A scan of a database with a full L4 therefore paid for every L4
//! file before it read a byte of the range it wanted, in both I/O and resident memory, and the
//! block cache spent the whole scan holding index blocks nothing would look at again.
//!
//! L0 keeps the old shape and must: its files overlap, so any of them can hold the next key and
//! all of them are genuinely live at once.
//!
//! # What it does not do
//!
//! It does not skip files by range. A `seek` lands on the right file by binary search, and
//! forward iteration then walks the level in order — which is what a scan asks for. Narrowing the
//! *set* of files to a read's range is `Version::overlapping`'s job and belongs to the caller.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::dbformat::{Comparator, InternalKeyComparator};
use crate::error::{Error, Result};
use crate::iterator::Cursor;
use crate::sst::reader::clone_error;
use crate::sst::{TableIter, TableOptions};
use crate::version::FileMeta;

use super::table_cache::TableCache;

/// A cursor over one sorted, non-overlapping level.
///
/// Holds at most one open table reader, and drops it when the cursor leaves that file.
pub(crate) struct LevelCursor {
    /// The level's files, sorted by smallest key, non-overlapping.
    files: Vec<Arc<FileMeta>>,
    cache: Arc<TableCache>,
    options: TableOptions,
    comparator: Arc<InternalKeyComparator>,
    /// Which file is open, and the cursor inside it.
    current: Option<(usize, TableIter)>,
    /// The first read error, kept because an unreadable block ends iteration exactly as reaching
    /// the last entry does ([`crate::iterator`]).
    status: Option<Error>,
}

impl std::fmt::Debug for LevelCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LevelCursor")
            .field("files", &self.files.len())
            .field("open", &self.current.as_ref().map(|(at, _)| *at))
            .finish_non_exhaustive()
    }
}

impl LevelCursor {
    /// A cursor over `files`, which must be sorted by smallest key and non-overlapping.
    pub(crate) fn new(
        files: Vec<Arc<FileMeta>>,
        cache: Arc<TableCache>,
        options: TableOptions,
        comparator: Arc<InternalKeyComparator>,
    ) -> Self {
        Self {
            files,
            cache,
            options,
            comparator,
            current: None,
            status: None,
        }
    }

    /// Opens `index` and positions the cursor inside it with `position`.
    ///
    /// A file that will not open ends iteration and records why. Returning `false` rather than
    /// leaving a half-open cursor is what keeps `valid()` and `status()` telling the truth
    /// separately.
    fn open(&mut self, index: usize, position: impl FnOnce(&mut TableIter)) -> bool {
        let Some(file) = self.files.get(index) else {
            self.current = None;
            return false;
        };
        let reader = match self.cache.get(file.number, &self.options) {
            Ok(reader) => reader,
            Err(error) => {
                self.status.get_or_insert(error);
                self.current = None;
                return false;
            }
        };
        // The invariant that lets this level be one cursor, checked on every file actually
        // opened. ADR 0017 decision 6: a compaction whose inputs carry a range tombstone becomes
        // a discharge, so no tombstone is ever written below L0 — and a level holding one would
        // not partition the key space the way the read path assumes.
        debug_assert!(
            reader.range_tombstones().is_empty(),
            "a range tombstone in file {} of a level below L0: ADR 0017 decision 6 says a \
             compaction discharges them and never writes one out",
            file.number
        );
        let mut iter = reader.iter();
        position(&mut iter);
        if let Err(error) = iter.status() {
            self.status.get_or_insert(error);
            self.current = None;
            return false;
        }
        let valid = iter.valid();
        self.current = Some((index, iter));
        valid
    }

    /// Walks forward from `index` until a file yields an entry, or the level ends.
    ///
    /// Files are never empty in practice — a compaction that produced no keys writes no file —
    /// but the loop costs nothing and an empty file would otherwise end a scan early, which is a
    /// silently wrong answer rather than a slow one.
    fn open_forward_from(&mut self, mut index: usize) -> bool {
        while index < self.files.len() {
            if self.open(index, TableIter::seek_to_first) {
                return true;
            }
            if self.status.is_some() {
                return false;
            }
            index += 1;
        }
        self.current = None;
        false
    }

    /// Walks backward from `index` until a file yields an entry, or the level ends.
    fn open_backward_from(&mut self, index: usize) -> bool {
        let mut at = index;
        loop {
            if self.open(at, TableIter::seek_to_last) {
                return true;
            }
            if self.status.is_some() || at == 0 {
                self.current = None;
                return false;
            }
            at -= 1;
        }
    }

    /// The first file that could hold an entry at or after `target`: the first whose largest key
    /// is not less than it.
    ///
    /// A partitioned level makes this a binary search, and that is the whole point — the old
    /// shape reached the same file by opening every file before it.
    fn file_at_or_after(&self, target: &[u8]) -> usize {
        self.files
            .partition_point(|file| self.comparator.cmp(&file.largest, target) == Ordering::Less)
    }

    /// The last file that could hold an entry at or before `target`: the last whose smallest key
    /// is not greater than it. `None` when `target` sorts before the level.
    fn file_at_or_before(&self, target: &[u8]) -> Option<usize> {
        let after = self.files.partition_point(|file| {
            self.comparator.cmp(&file.smallest, target) != Ordering::Greater
        });
        after.checked_sub(1)
    }
}

impl Cursor for LevelCursor {
    fn valid(&self) -> bool {
        self.current.as_ref().is_some_and(|(_, iter)| iter.valid())
    }

    fn key(&self) -> &[u8] {
        self.current.as_ref().map_or(&[], |(_, iter)| iter.key())
    }

    fn value(&self) -> &[u8] {
        self.current.as_ref().map_or(&[], |(_, iter)| iter.value())
    }

    fn seek(&mut self, target: &[u8]) {
        let index = self.file_at_or_after(target);
        let target = target.to_vec();
        if self.open(index, |iter| iter.seek(&target)) {
            return;
        }
        if self.status.is_some() {
            return;
        }
        // The file whose range covers `target` can still hold nothing at or after it — `target`
        // falls in a gap between two of its keys and past its last. The answer is then the next
        // file's first entry, not "the level ends here".
        self.open_forward_from(index + 1);
    }

    fn seek_for_prev(&mut self, target: &[u8]) {
        let Some(index) = self.file_at_or_before(target) else {
            self.current = None;
            return;
        };
        let target = target.to_vec();
        if self.open(index, |iter| iter.seek_for_prev(&target)) {
            return;
        }
        if self.status.is_some() || index == 0 {
            self.current = None;
            return;
        }
        self.open_backward_from(index - 1);
    }

    fn seek_to_first(&mut self) {
        self.open_forward_from(0);
    }

    fn seek_to_last(&mut self) {
        match self.files.len().checked_sub(1) {
            Some(last) => {
                self.open_backward_from(last);
            }
            None => self.current = None,
        }
    }

    fn next(&mut self) {
        let Some((index, iter)) = self.current.as_mut() else {
            return;
        };
        iter.next();
        if iter.valid() {
            return;
        }
        if let Err(error) = iter.status() {
            self.status.get_or_insert(error);
            self.current = None;
            return;
        }
        let next = *index + 1;
        self.open_forward_from(next);
    }

    fn prev(&mut self) {
        let Some((index, iter)) = self.current.as_mut() else {
            return;
        };
        iter.prev();
        if iter.valid() {
            return;
        }
        if let Err(error) = iter.status() {
            self.status.get_or_insert(error);
            self.current = None;
            return;
        }
        match index.checked_sub(1) {
            Some(previous) => {
                self.open_backward_from(previous);
            }
            None => self.current = None,
        }
    }

    fn status(&self) -> Result<()> {
        if let Some(error) = &self.status {
            return Err(clone_error(error));
        }
        match &self.current {
            Some((_, iter)) => iter.status(),
            None => Ok(()),
        }
    }
}
