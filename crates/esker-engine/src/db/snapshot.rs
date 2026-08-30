//! Snapshots: a sequence number, and a promise that nothing below it is thrown away.
//!
//! A read at snapshot `s` sees exactly the writes with a sequence number at or below `s`
//! (`docs/DESIGN.md` §4.1). Taking one is free — it is a `u64` — but *keeping* one is not: a
//! compaction may not drop an old version of a key while some snapshot can still see it, so
//! the engine has to know which are outstanding.
//!
//! That is what [`SnapshotList`] is for. It is a reference-counted multiset of sequence
//! numbers, and [`SnapshotList::oldest`] is the line compaction is not allowed to collect
//! below. A snapshot releases itself on drop, so forgetting one costs space until the handle
//! goes away rather than forever.
//!
//! # A snapshot belongs to one open database
//!
//! Sequence numbers survive a reopen, so a handle taken before one is still a plausible
//! number afterwards — and reading through it would quietly work. It must not. The reopened
//! database's list has never heard of that handle, so the floor it gives compaction can sit
//! above the sequence number the handle still claims to read at, and the versions it pinned
//! would be collected out from under it.
//!
//! Every list therefore carries an instance id, every snapshot remembers its list's, and the
//! read paths refuse a handle from anywhere else. Refusing is the only honest option: the
//! guarantee a snapshot makes is that what it saw stays readable, and a reopened database
//! cannot make that promise about a number it was handed.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::dbformat::SeqNo;

/// Hands each open database a distinct identity. Never repeats within a process, and a
/// snapshot cannot leave one.
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// The sequence numbers readers are currently holding.
#[derive(Debug)]
pub struct SnapshotList {
    /// Which open database this list belongs to.
    instance: u64,
    /// Sequence number to how many live snapshots name it.
    counts: Mutex<BTreeMap<SeqNo, usize>>,
}

impl SnapshotList {
    /// An empty list, with a fresh instance id.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            counts: Mutex::new(BTreeMap::new()),
        })
    }

    /// The open database this list belongs to.
    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// Takes a snapshot at `seqno`.
    pub fn acquire(self: &Arc<Self>, seqno: SeqNo) -> Snapshot {
        self.retain(seqno);
        Snapshot {
            seqno,
            list: Arc::clone(self),
        }
    }

    /// The oldest sequence number any live snapshot names, or `None` if there are none.
    ///
    /// Compaction may not drop a version at or above this, because a reader can still ask for
    /// it. With no snapshots outstanding the only floor is the caller's own read.
    pub fn oldest(&self) -> Option<SeqNo> {
        // A poisoned lock means a reader panicked. Report "no snapshots" rather than panicking
        // again — the conservative direction would be to report the smallest possible, and the
        // safe one is to keep everything, which is what `None` means to the caller here.
        let counts = self.counts.lock().ok()?;
        counts.keys().next().copied()
    }

    /// How many snapshots are outstanding.
    pub fn len(&self) -> usize {
        self.counts.lock().map_or(0, |counts| counts.values().sum())
    }

    /// Whether no snapshot is outstanding.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn retain(&self, seqno: SeqNo) {
        if let Ok(mut counts) = self.counts.lock() {
            *counts.entry(seqno).or_insert(0) += 1;
        }
    }

    fn release(&self, seqno: SeqNo) {
        if let Ok(mut counts) = self.counts.lock()
            && let Some(count) = counts.get_mut(&seqno)
        {
            *count -= 1;
            if *count == 0 {
                counts.remove(&seqno);
            }
        }
    }
}

/// A read position: everything at or below [`Snapshot::seqno`] is visible, nothing above it is.
#[derive(Debug)]
pub struct Snapshot {
    seqno: SeqNo,
    list: Arc<SnapshotList>,
}

impl Snapshot {
    /// The sequence number this snapshot pins.
    pub fn seqno(&self) -> SeqNo {
        self.seqno
    }

    /// The open database that issued it.
    ///
    /// Reads compare this with their own: a snapshot from a database that has since been
    /// closed and reopened pins nothing in the new one, so using it would be a promise
    /// nobody is keeping.
    pub fn instance(&self) -> u64 {
        self.list.instance()
    }
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        // A clone is another holder, so it counts. Deriving `Clone` would let a compaction
        // collect versions a live snapshot can still read.
        self.list.retain(self.seqno);
        Self {
            seqno: self.seqno,
            list: Arc::clone(&self.list),
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.list.release(self.seqno);
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotList;

    #[test]
    fn the_oldest_snapshot_is_the_compaction_floor() {
        let list = SnapshotList::new();
        assert_eq!(list.oldest(), None, "nothing to protect");

        let ten = list.acquire(10);
        let five = list.acquire(5);
        let twenty = list.acquire(20);
        assert_eq!(list.oldest(), Some(5));
        assert_eq!(list.len(), 3);

        drop(five);
        assert_eq!(list.oldest(), Some(10));
        drop(ten);
        drop(twenty);
        assert_eq!(list.oldest(), None);
        assert!(list.is_empty());
    }

    /// Two snapshots at the same sequence number are two holders, and the second one keeps it
    /// alive after the first is dropped.
    #[test]
    fn snapshots_at_the_same_sequence_number_are_counted() {
        let list = SnapshotList::new();
        let first = list.acquire(7);
        let second = list.acquire(7);
        assert_eq!(list.len(), 2);
        drop(first);
        assert_eq!(list.oldest(), Some(7));
        drop(second);
        assert_eq!(list.oldest(), None);
    }

    /// Every open database gets its own identity, so a handle from one can be told from a
    /// handle from another — which is what makes a stale snapshot refusable rather than
    /// silently plausible.
    #[test]
    fn snapshots_carry_the_instance_that_issued_them() {
        let first = SnapshotList::new();
        let second = SnapshotList::new();
        assert_ne!(first.instance(), second.instance());
        assert_eq!(first.acquire(1).instance(), first.instance());
        assert_eq!(first.acquire(9).clone().instance(), first.instance());
        assert_ne!(first.acquire(1).instance(), second.acquire(1).instance());
    }

    /// Cloning has to count too, or a compaction could collect what a clone can still read.
    #[test]
    fn cloning_a_snapshot_holds_it_as_well() {
        let list = SnapshotList::new();
        let original = list.acquire(3);
        let copy = original.clone();
        assert_eq!(list.len(), 2);
        assert_eq!(copy.seqno(), 3);
        drop(original);
        assert_eq!(list.oldest(), Some(3), "the clone still holds it");
        drop(copy);
        assert_eq!(list.oldest(), None);
    }
}
