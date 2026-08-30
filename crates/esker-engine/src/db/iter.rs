//! Iterating a column family: many versions in, one per key out.
//!
//! Underneath is a [`MergeCursor`] over every memtable and every SST that could hold the
//! range, walking **internal** keys: several versions of a key, tombstones included, in
//! newest-first order. This turns that into the sequence a caller expects — one entry per user
//! key, at the snapshot, with deleted keys absent.
//!
//! # The rule
//!
//! Within a user key, entries are ordered newest first. So going forward, the first entry at
//! or below the snapshot decides the key: a value is yielded, a tombstone hides it, and every
//! older entry for that key is skipped either way. Going backward they arrive oldest first, so
//! the decision is the *last* one seen before the user key changes. That asymmetry is why the
//! two directions are separate code and why `LevelDB` writes them separately too.
//!
//! # Changing direction
//!
//! The underlying cursor sits on the entry that produced the current answer, which is in the
//! middle of that key's versions. Turning around therefore has to step past all of them first,
//! or the same key would be returned twice.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::dbformat::{
    EntryKind, InternalKeyComparator, MAX_SEQNO, SeqNo, extract_user_key, internal_key, lookup_key,
    split_internal_key,
};
use crate::error::{Error, Result};
use crate::iterator::Cursor;
use crate::options::{PrefixExtractor, ReadOptions};
use crate::sst::TableIter;
use crate::version::Version;

use super::merge::MergeCursor;
use super::{Db, Snapshot, lock, read_lock};

/// Which way the iterator is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
}

/// Adapts the SST lane's table cursor to [`Cursor`].
///
/// A wrapper rather than an implementation on `TableIter` itself, because `src/sst/` belongs
/// to the other lane. `TODO(sibling)`: once `TableIter` implements `Cursor` directly this type
/// can go.
#[derive(Debug)]
struct TableCursor(TableIter);

impl Cursor for TableCursor {
    fn valid(&self) -> bool {
        self.0.valid()
    }
    fn key(&self) -> &[u8] {
        self.0.key()
    }
    fn value(&self) -> &[u8] {
        self.0.value()
    }
    fn seek(&mut self, target: &[u8]) {
        self.0.seek(target);
    }
    fn seek_for_prev(&mut self, target: &[u8]) {
        self.0.seek_for_prev(target);
    }
    fn seek_to_first(&mut self) {
        self.0.seek_to_first();
    }
    fn seek_to_last(&mut self) {
        self.0.seek_to_last();
    }
    fn next(&mut self) {
        self.0.next();
    }
    fn prev(&mut self) {
        self.0.prev();
    }
    fn status(&self) -> Result<()> {
        self.0.status()
    }
}

/// A cursor over one column family's user keys, at one snapshot.
#[derive(Debug)]
pub struct DbIterator {
    merger: MergeCursor,
    comparator: Arc<InternalKeyComparator>,
    snapshot: SeqNo,
    direction: Direction,
    /// The user key and value the cursor is on.
    current: Option<(Vec<u8>, Vec<u8>)>,
    /// Set by [`ReadOptions::prefix_same_as_start`]: iteration ends when the prefix changes.
    prefix: Option<Vec<u8>>,
    extractor: Option<Arc<dyn PrefixExtractor>>,
    status: Option<Error>,
    /// Pins every file the merger reads. Dropping it is what lets them be deleted.
    _version: Arc<Version>,
    /// Pins the sequence number, so a compaction cannot collect versions this iterator needs.
    _snapshot: Option<Snapshot>,
}

impl DbIterator {
    /// Whether the cursor is on an entry.
    pub fn valid(&self) -> bool {
        self.current.is_some()
    }

    /// The user key under the cursor.
    pub fn key(&self) -> &[u8] {
        self.current.as_ref().map_or(&[], |(key, _)| key.as_slice())
    }

    /// The value under the cursor.
    pub fn value(&self) -> &[u8] {
        self.current
            .as_ref()
            .map_or(&[], |(_, value)| value.as_slice())
    }

    /// Whether everything read so far was readable.
    ///
    /// Checked after iteration ends: an unreadable block stops the cursor exactly as reaching
    /// the last key does, and treating the first as the second turns a damaged file into an
    /// empty range.
    pub fn status(&self) -> Result<()> {
        if let Some(error) = &self.status {
            return Err(Error::corruption("iterator", error.to_string()));
        }
        self.merger.status()
    }

    /// Positions on the first key.
    pub fn seek_to_first(&mut self) {
        self.prefix = None;
        self.merger.seek_to_first();
        self.direction = Direction::Forward;
        self.find_next(None);
    }

    /// Positions on the last key.
    pub fn seek_to_last(&mut self) {
        self.prefix = None;
        self.merger.seek_to_last();
        self.direction = Direction::Reverse;
        self.find_prev();
    }

    /// Positions on the first key at or after `target`.
    pub fn seek(&mut self, target: &[u8]) {
        self.set_prefix(target);
        // The largest tag for the user key, which sorts before every stored version of it.
        self.merger.seek(&lookup_key(target, MAX_SEQNO));
        self.direction = Direction::Forward;
        self.find_next(None);
    }

    /// Positions on the last key at or before `target`.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.set_prefix(target);
        // Tag zero sorts after every stored version of the user key, so this lands on the
        // oldest one; the backward scan then walks up to the newest.
        self.merger
            .seek_for_prev(&internal_key(target, 0, EntryKind::Delete));
        self.direction = Direction::Reverse;
        self.find_prev();
    }

    /// Moves to the next key. A no-op when the cursor is not valid.
    pub fn next(&mut self) {
        let Some((user, _)) = self.current.clone() else {
            return;
        };
        if self.direction == Direction::Reverse {
            self.step_forward_past(&user);
            self.direction = Direction::Forward;
        } else {
            self.merger.next();
        }
        self.find_next(Some(user));
    }

    /// Moves to the previous key. A no-op when the cursor is not valid.
    pub fn prev(&mut self) {
        let Some((user, _)) = self.current.clone() else {
            return;
        };
        if self.direction == Direction::Forward {
            self.step_back_past(&user);
            self.direction = Direction::Reverse;
        }
        self.find_prev();
    }

    /// Scans forward for the next visible value, skipping older versions of `skipping`.
    fn find_next(&mut self, mut skipping: Option<Vec<u8>>) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        while self.merger.valid() {
            let key = self.merger.key().to_vec();
            let Some((user, seqno, kind)) = split_internal_key(&key) else {
                self.fail("an entry whose tag cannot be decoded");
                return;
            };
            if seqno <= self.snapshot {
                // An entry at or below the key we have already decided is an older version of
                // it, whatever it says.
                let decided = skipping
                    .as_ref()
                    .is_some_and(|key| user_order.cmp(user, key) != Ordering::Greater);
                if !decided {
                    if !self.in_prefix(user) {
                        self.current = None;
                        return;
                    }
                    match kind {
                        EntryKind::Put => {
                            self.current = Some((user.to_vec(), self.merger.value().to_vec()));
                            return;
                        }
                        EntryKind::Delete | EntryKind::DeleteRange => {
                            skipping = Some(user.to_vec());
                        }
                    }
                }
            }
            self.merger.next();
        }
        self.current = None;
    }

    /// Scans backward. Versions arrive oldest first, so the answer is the last one seen before
    /// the user key changes.
    fn find_prev(&mut self) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        let mut found: Option<(Vec<u8>, Vec<u8>)> = None;
        while self.merger.valid() {
            let key = self.merger.key().to_vec();
            let Some((user, seqno, kind)) = split_internal_key(&key) else {
                self.fail("an entry whose tag cannot be decoded");
                return;
            };
            if seqno <= self.snapshot {
                if let Some((decided, _)) = &found
                    && user_order.cmp(user, decided) == Ordering::Less
                {
                    // Moved off the key we had settled on; that one is the answer.
                    break;
                }
                match kind {
                    EntryKind::Put => {
                        found = Some((user.to_vec(), self.merger.value().to_vec()));
                    }
                    // A tombstone cancels the older versions seen so far for this key.
                    EntryKind::Delete | EntryKind::DeleteRange => found = None,
                }
            }
            self.merger.prev();
        }
        self.current = found.filter(|(user, _)| self.in_prefix(user));
    }

    /// Moves the underlying cursor past every version of `user`, going forward.
    fn step_forward_past(&mut self, user: &[u8]) {
        // Tag zero is smaller than any real tag, so it sorts after every version of the key.
        self.merger.seek(&internal_key(user, 0, EntryKind::Delete));
    }

    /// Moves the underlying cursor before every version of `user`, going backward.
    fn step_back_past(&mut self, user: &[u8]) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        self.merger.seek_for_prev(&lookup_key(user, MAX_SEQNO));
        while self.merger.valid() {
            let key_user = extract_user_key(self.merger.key()).to_vec();
            if user_order.cmp(&key_user, user) == Ordering::Less {
                return;
            }
            self.merger.prev();
        }
    }

    fn set_prefix(&mut self, target: &[u8]) {
        self.prefix = self.extractor.as_ref().map(|extractor| {
            if extractor.in_domain(target) {
                extractor.prefix(target).to_vec()
            } else {
                target.to_vec()
            }
        });
    }

    /// Whether `user` still shares the prefix iteration started at.
    fn in_prefix(&self, user: &[u8]) -> bool {
        let Some(prefix) = &self.prefix else {
            return true;
        };
        let Some(extractor) = &self.extractor else {
            return true;
        };
        extractor.in_domain(user) && extractor.prefix(user) == prefix.as_slice()
    }

    fn fail(&mut self, detail: &str) {
        self.current = None;
        self.status
            .get_or_insert_with(|| Error::corruption("iterator", detail.to_string()));
    }
}

impl Cursor for DbIterator {
    fn valid(&self) -> bool {
        Self::valid(self)
    }
    fn key(&self) -> &[u8] {
        Self::key(self)
    }
    fn value(&self) -> &[u8] {
        Self::value(self)
    }
    fn seek(&mut self, target: &[u8]) {
        Self::seek(self, target);
    }
    fn seek_for_prev(&mut self, target: &[u8]) {
        Self::seek_for_prev(self, target);
    }
    fn seek_to_first(&mut self) {
        Self::seek_to_first(self);
    }
    fn seek_to_last(&mut self) {
        Self::seek_to_last(self);
    }
    fn next(&mut self) {
        Self::next(self);
    }
    fn prev(&mut self) {
        Self::prev(self);
    }
    fn status(&self) -> Result<()> {
        Self::status(self)
    }
}

impl Db {
    /// A cursor over `cf`, positioned nowhere until it is seeked.
    ///
    /// The iterator pins a version and a snapshot for its whole life, so every file it can
    /// read stays on disk and every version it can see survives compaction. Holding one for a
    /// long time therefore holds disk space; that is the trade a consistent scan costs.
    pub fn iter(&self, cf: &str, options: &ReadOptions) -> Result<DbIterator> {
        let cf = self.inner.cf_by_name(cf)?;
        let snapshot = self.inner.read_seqno(options)?;

        let mut children: Vec<Box<dyn Cursor + Send>> = Vec::new();
        {
            let mem = read_lock(&cf.mem)?;
            children.push(Box::new(mem.active.iter()));
            for (table, _) in &mem.immutable {
                children.push(Box::new(table.iter()));
            }
        }

        let version = lock(&self.inner.versions)?.current();
        let table_options = self.inner.table_options(&cf);
        // TODO(post-v1): one cursor per file opens every file in a level. LevelDB uses a
        // two-level iterator that opens a level's files as it reaches them; that matters once
        // compaction fills the deeper levels.
        let levels = version
            .cf(cf.id())
            .map_or(0, crate::version::CfVersion::num_levels);
        for level in 0..levels {
            for file in version.files(cf.id(), level) {
                let reader = self.inner.table_cache.get(file.number, &table_options)?;
                children.push(Box::new(TableCursor(reader.iter())));
            }
        }

        Ok(DbIterator {
            merger: MergeCursor::new(children, Arc::clone(&self.inner.comparator)),
            comparator: Arc::clone(&self.inner.comparator),
            snapshot,
            direction: Direction::Forward,
            current: None,
            prefix: None,
            extractor: options
                .prefix_same_as_start
                .then(|| cf.options().prefix_extractor.clone())
                .flatten(),
            status: None,
            _version: version,
            _snapshot: options.snapshot.clone(),
        })
    }
}
