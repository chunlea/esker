//! Point lookups.
//!
//! The order is newest to oldest and it is not negotiable: the active memtable, then the
//! immutable ones newest first, then L0 newest first, then each deeper level by index lookup
//! (`docs/DESIGN.md` §4.9). The first entry found for the key at or below the snapshot is the
//! answer, whether it is a value or a tombstone — a delete that stops the search is exactly
//! what makes deletion work in a log-structured store.

use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::dbformat::SeqNo;
use crate::error::Result;
use crate::memtable::Lookup;
use crate::options::ReadOptions;

use super::{Db, Snapshot, read_lock};

impl Db {
    /// Reads one key, or `None` if it is not there at this snapshot.
    pub fn get(&self, cf: &str, key: &[u8], options: &ReadOptions) -> Result<Option<Bytes>> {
        let cf = self.inner.cf_by_name(cf)?;
        let snapshot = options.snapshot.as_ref().map_or_else(
            || self.inner.visible_seqno.load(Ordering::Acquire),
            Snapshot::seqno,
        );

        // Take a consistent set of memtables and let go of the lock: a read must not block a
        // writer, and holding an Arc is enough to keep a table alive after a switch.
        let (active, immutable) = {
            let mem = read_lock(&cf.mem)?;
            (Arc::clone(&mem.active), mem.immutable.clone())
        };

        if let Some(found) = active.get(key, snapshot) {
            return Ok(value_of(found));
        }
        // Newest first: the back of the list is the most recently switched table.
        for table in immutable.iter().rev() {
            if let Some(found) = table.get(key, snapshot) {
                return Ok(value_of(found));
            }
        }

        // TODO(step-6b): L0 newest first, then the deeper levels through the SST lane's
        // TableReader, with the bloom filter consulted before any disk read.
        Ok(None)
    }

    /// Reads at an explicit snapshot, a shorthand for filling in [`ReadOptions`].
    pub fn get_at(&self, cf: &str, key: &[u8], snapshot: SeqNo) -> Result<Option<Bytes>> {
        let options = ReadOptions {
            snapshot: Some(self.inner.snapshots.acquire(snapshot)),
            ..ReadOptions::default()
        };
        self.get(cf, key, &options)
    }
}

/// A tombstone is an answer, and the answer is "not there".
fn value_of(found: Lookup) -> Option<Bytes> {
    match found {
        Lookup::Found(value) => Some(Bytes::from(value)),
        Lookup::Deleted => None,
    }
}
