//! The log the core reads, and an in-memory implementation of it.
//!
//! [`LogStorage`] is how a state machine that does no I/O reads a log that lives on a disk. It is
//! read-only on purpose: writes leave through [`Ready`](crate::Ready) and the driver performs
//! them, which is what makes the ordering rule in `docs/plans/phase-3.md` §4 enforceable at all.
//! If the core could write, "persist before send" would be an internal detail nobody could test.
//!
//! Two things are worth stating precisely, because every off-by-one in an etcd-shaped Raft lives
//! here:
//!
//! * Ranges are **half-open**: `entries(lo, hi)` returns `[lo, hi)`.
//! * [`LogStorage::first_index`] is the first index still *available as an entry*. Everything
//!   below it has been compacted into a snapshot. [`LogStorage::term`] still answers for
//!   `first_index() - 1`, because the snapshot records that entry's term and the consistency
//!   check needs it — a follower whose log begins at a snapshot must still be able to say what
//!   term its predecessor had.
//!
//! An empty log has `first_index() == 1` and `last_index() == 0`, so `last_index()` is "how many
//! entries exist" only when nothing has been compacted.

use crate::error::{RaftError, Result};
use crate::types::{ConfState, Entry, HardState, Index, Snapshot, Term, offset};

/// What a node reloads when it restarts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitialState {
    /// Term, vote and commit index as of the last fsync.
    pub hard_state: HardState,
    /// The membership as of the last entry in the log — not the last *committed* one, because a
    /// configuration takes effect when its entry is appended (dissertation §4.1).
    pub conf_state: ConfState,
}

/// Read access to the Raft log and the durable state beside it.
///
/// Implementations must be deterministic: two calls with the same argument on the same state
/// return the same answer. Nothing here may block on I/O that the core cannot tolerate — a real
/// implementation reads from a page cache or an already-open file, and answers
/// [`RaftError::SnapshotTemporarilyUnavailable`] rather than waiting for a snapshot to be built.
pub trait LogStorage {
    /// Term, vote, commit index and membership, as of the last durable write.
    fn initial_state(&self) -> Result<InitialState>;

    /// Entries in `[low, high)`, stopping early once their total [`Entry::cost`] would exceed
    /// `max_bytes`.
    ///
    /// **At least one entry is always returned** when the range is non-empty, even if that entry
    /// alone exceeds the budget. A limit that could return nothing would stall replication on the
    /// first oversized proposal instead of merely making one message large.
    ///
    /// Returns [`RaftError::Compacted`] if `low` is below [`LogStorage::first_index`], and
    /// [`RaftError::Unavailable`] if `high` exceeds `last_index() + 1`.
    fn entries(&self, low: Index, high: Index, max_bytes: u64) -> Result<Vec<Entry>>;

    /// The term of the entry at `index`.
    ///
    /// Answers for `first_index() - 1` as well, from the snapshot's metadata. Below that the
    /// answer is genuinely gone: [`RaftError::Compacted`].
    fn term(&self, index: Index) -> Result<Term>;

    /// The first index available as an entry. `1` for a log that has never been compacted.
    fn first_index(&self) -> Result<Index>;

    /// The last index in the log; `0` when the log is empty and nothing is compacted.
    fn last_index(&self) -> Result<Index>;

    /// The most recent snapshot, for a leader to send to a follower that has fallen too far
    /// behind. An implementation with nothing compacted returns an empty snapshot
    /// ([`Snapshot::is_empty`]).
    fn snapshot(&self) -> Result<Snapshot>;
}

/// A [`LogStorage`] in memory: the one the unit tests and the simulator use.
///
/// It is `Clone` because that is how a simulator models a crash and a restart — clone the storage
/// at the moment of the kill, throw the node away, build a new one over the clone. Anything the
/// driver had not fsynced is simply not in the clone, which is exactly the failure the driver
/// contract is about.
#[derive(Debug, Clone, Default)]
pub struct MemStorage {
    hard_state: HardState,
    conf_state: ConfState,
    /// The snapshot, if any. Its `meta.index` is the compaction boundary.
    snapshot: Snapshot,
    /// Entries strictly above the compaction boundary, in index order.
    entries: Vec<Entry>,
}

impl MemStorage {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty log whose membership is `conf` — how a bootstrapped group starts.
    pub fn with_conf_state(conf: ConfState) -> Self {
        Self {
            conf_state: conf,
            ..Self::default()
        }
    }

    /// The compaction boundary: the index of the last entry folded into the snapshot.
    fn compacted_index(&self) -> Index {
        self.snapshot.meta.index
    }

    fn compacted_term(&self) -> Term {
        self.snapshot.meta.term
    }

    fn last(&self) -> Index {
        self.compacted_index() + self.entries.len() as Index
    }

    /// Appends `entries`, truncating any existing suffix that conflicts with them.
    ///
    /// Truncation is not an edge case: it is what a follower does every time a new leader replaces
    /// the tail its predecessor left behind. Entries at or below the compaction boundary are
    /// silently dropped — they are already in the snapshot, and a leader that resends them is
    /// merely behind on where this node is, not wrong.
    ///
    /// Returns [`RaftError::Unavailable`] if the batch would leave a hole in the log.
    pub fn append(&mut self, entries: &[Entry]) -> Result<()> {
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let last = entries[entries.len() - 1].index;
        if last <= self.compacted_index() {
            return Ok(());
        }
        if first.index > self.last() + 1 {
            return Err(RaftError::Unavailable(self.last() + 1));
        }
        // Skip whatever the snapshot already covers, then replace from the first index that
        // remains. An entry at exactly the boundary is covered too, hence the `+ 1`.
        let skip = if self.compacted_index() >= first.index {
            offset(self.compacted_index() - first.index + 1)
        } else {
            0
        };
        let kept = &entries[skip.min(entries.len())..];
        let Some(first_kept) = kept.first() else {
            return Ok(());
        };
        let at = offset(first_kept.index - self.compacted_index() - 1);
        self.entries.truncate(at);
        self.entries.extend_from_slice(kept);
        Ok(())
    }

    /// Records the durable state. The driver calls this with
    /// [`Ready::hard_state`](crate::Ready::hard_state) before sending anything from that `Ready`.
    pub fn set_hard_state(&mut self, hard_state: HardState) {
        self.hard_state = hard_state;
    }

    /// The durable state as last recorded.
    pub fn hard_state(&self) -> HardState {
        self.hard_state
    }

    /// Records the membership. In a real store this rides along with the entry that changed it.
    pub fn set_conf_state(&mut self, conf_state: ConfState) {
        self.conf_state = conf_state;
    }

    /// Replaces the log with a snapshot: everything up to `snapshot.meta.index` is now covered by
    /// it, and any entry the snapshot does not cover is kept.
    ///
    /// Returns [`RaftError::SnapshotOutOfDate`] for a snapshot the log has already passed.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) -> Result<()> {
        if snapshot.meta.index <= self.compacted_index() {
            return Err(RaftError::SnapshotOutOfDate {
                index: snapshot.meta.index,
                committed: self.compacted_index(),
            });
        }
        // Entries the snapshot does not cover survive it; a snapshot that matches the log's own
        // term at that index is a compaction, not a replacement.
        let keeps_tail = self
            .term(snapshot.meta.index)
            .is_ok_and(|term| term == snapshot.meta.term);
        if keeps_tail {
            let covered = offset(snapshot.meta.index - self.compacted_index());
            self.entries.drain(..covered.min(self.entries.len()));
        } else {
            self.entries.clear();
        }
        self.hard_state.commit = self.hard_state.commit.max(snapshot.meta.index);
        self.conf_state = snapshot.meta.conf.clone();
        self.snapshot = snapshot;
        Ok(())
    }

    /// Discards entries at or below `to_index`, which must already be covered by a snapshot the
    /// caller is about to install or has installed.
    ///
    /// The snapshot's `data` is not this type's concern; `compact` moves the boundary and keeps
    /// the term at it, which is all the consistency check needs.
    pub fn compact(&mut self, to_index: Index) -> Result<()> {
        if to_index <= self.compacted_index() {
            return Err(RaftError::Compacted(self.compacted_index()));
        }
        if to_index > self.last() {
            return Err(RaftError::Unavailable(to_index));
        }
        let term = self.term(to_index)?;
        let covered = offset(to_index - self.compacted_index()).min(self.entries.len());
        self.entries.drain(..covered);
        self.snapshot.meta.index = to_index;
        self.snapshot.meta.term = term;
        self.snapshot.meta.conf = self.conf_state.clone();
        Ok(())
    }

    /// Replaces the snapshot's payload, for a test that wants a snapshot with recognisable bytes.
    pub fn set_snapshot_data(&mut self, data: bytes::Bytes) {
        self.snapshot.data = data;
    }
}

impl LogStorage for MemStorage {
    fn initial_state(&self) -> Result<InitialState> {
        Ok(InitialState {
            hard_state: self.hard_state,
            conf_state: self.conf_state.clone(),
        })
    }

    fn entries(&self, low: Index, high: Index, max_bytes: u64) -> Result<Vec<Entry>> {
        if low <= self.compacted_index() {
            return Err(RaftError::Compacted(self.compacted_index()));
        }
        if high > self.last() + 1 {
            return Err(RaftError::Unavailable(high.saturating_sub(1)));
        }
        if low >= high {
            return Ok(Vec::new());
        }
        let start = offset(low - self.compacted_index() - 1);
        let end = offset(high - self.compacted_index() - 1);
        let window = self
            .entries
            .get(start..end)
            .ok_or(RaftError::Unavailable(high - 1))?;
        let mut out = Vec::with_capacity(window.len());
        let mut budget: u64 = 0;
        for entry in window {
            budget = budget.saturating_add(entry.cost());
            if budget > max_bytes && !out.is_empty() {
                break;
            }
            out.push(entry.clone());
        }
        Ok(out)
    }

    fn term(&self, index: Index) -> Result<Term> {
        if index == self.compacted_index() {
            return Ok(self.compacted_term());
        }
        if index < self.compacted_index() {
            return Err(RaftError::Compacted(self.compacted_index()));
        }
        if index > self.last() {
            return Err(RaftError::Unavailable(index));
        }
        self.entries
            .get(offset(index - self.compacted_index() - 1))
            .map(|entry| entry.term)
            .ok_or(RaftError::Unavailable(index))
    }

    fn first_index(&self) -> Result<Index> {
        Ok(self.compacted_index() + 1)
    }

    fn last_index(&self) -> Result<Index> {
        Ok(self.last())
    }

    fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.snapshot.clone())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{LogStorage, MemStorage};
    use crate::error::RaftError;
    use crate::types::{ConfState, Entry, Index, Snapshot, SnapshotMeta, Term};

    fn entries(spec: &[(Term, Index)]) -> Vec<Entry> {
        spec.iter()
            .map(|(term, index)| Entry::empty(*term, *index))
            .collect()
    }

    fn snapshot(index: Index, term: Term) -> Snapshot {
        Snapshot {
            meta: SnapshotMeta {
                index,
                term,
                conf: ConfState::from_voters(vec![1, 2, 3]),
            },
            data: Bytes::from_static(b"state"),
        }
    }

    /// The empty log is the one every off-by-one starts from: no entries, but the next one goes
    /// at index 1.
    #[test]
    fn an_empty_log_starts_at_one_and_ends_at_zero() {
        let store = MemStorage::new();
        assert_eq!(store.first_index().unwrap(), 1);
        assert_eq!(store.last_index().unwrap(), 0);
        assert_eq!(store.term(0).unwrap(), 0);
        assert!(store.entries(1, 1, u64::MAX).unwrap().is_empty());
        assert!(store.snapshot().unwrap().is_empty());
    }

    #[test]
    fn appended_entries_come_back_by_index_and_term() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (2, 3)])).unwrap();
        assert_eq!(store.last_index().unwrap(), 3);
        assert_eq!(store.term(2).unwrap(), 1);
        assert_eq!(store.term(3).unwrap(), 2);
        assert_eq!(
            store.entries(2, 4, u64::MAX).unwrap(),
            entries(&[(1, 2), (2, 3)])
        );
        assert!(matches!(store.term(4), Err(RaftError::Unavailable(4))));
    }

    /// What a follower does every time a new leader replaces the tail its predecessor left.
    #[test]
    fn appending_over_an_existing_suffix_replaces_it() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        store.append(&entries(&[(2, 2), (2, 3)])).unwrap();
        assert_eq!(store.last_index().unwrap(), 3);
        assert_eq!(store.term(1).unwrap(), 1);
        assert_eq!(store.term(2).unwrap(), 2);
        assert_eq!(store.term(3).unwrap(), 2);
    }

    /// A leader that resends entries the snapshot already covers is behind on where this node is,
    /// not wrong. Dropping them must not shorten the log.
    #[test]
    fn appending_below_the_compaction_boundary_drops_only_the_covered_prefix() {
        let mut store = MemStorage::new();
        store
            .append(&entries(&[(1, 1), (1, 2), (1, 3), (1, 4)]))
            .unwrap();
        store.compact(2).unwrap();
        store
            .append(&entries(&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5)]))
            .unwrap();
        assert_eq!(store.first_index().unwrap(), 3);
        assert_eq!(store.last_index().unwrap(), 5);
    }

    #[test]
    fn a_batch_entirely_below_the_boundary_is_ignored() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        store.compact(3).unwrap();
        store.append(&entries(&[(1, 1), (1, 2)])).unwrap();
        assert_eq!(store.last_index().unwrap(), 3);
    }

    #[test]
    fn a_batch_that_would_leave_a_hole_is_refused() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1)])).unwrap();
        assert!(matches!(
            store.append(&entries(&[(1, 3)])),
            Err(RaftError::Unavailable(2))
        ));
    }

    /// A budget that could return nothing would stall replication on the first oversized proposal
    /// instead of merely making one message large.
    #[test]
    fn a_byte_budget_always_yields_at_least_one_entry() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        assert_eq!(store.entries(1, 4, 0).unwrap().len(), 1);
        assert_eq!(store.entries(1, 4, u64::MAX).unwrap().len(), 3);
    }

    #[test]
    fn compaction_keeps_the_term_at_the_boundary_and_loses_everything_below() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (2, 2), (2, 3)])).unwrap();
        store.compact(2).unwrap();
        // The consistency check needs the term of the entry a compacted log begins after.
        assert_eq!(store.term(2).unwrap(), 2);
        assert_eq!(store.first_index().unwrap(), 3);
        assert!(matches!(store.term(1), Err(RaftError::Compacted(2))));
        assert!(matches!(
            store.entries(1, 3, u64::MAX),
            Err(RaftError::Compacted(2))
        ));
        assert!(matches!(store.compact(2), Err(RaftError::Compacted(2))));
        assert!(matches!(store.compact(9), Err(RaftError::Unavailable(9))));
    }

    /// A snapshot from a leader whose log disagrees is authoritative: mixing the two is how a
    /// follower ends up with a log no leader ever had.
    #[test]
    fn a_snapshot_that_disagrees_with_the_log_replaces_it_entirely() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        store.apply_snapshot(snapshot(2, 5)).unwrap();
        assert_eq!(store.first_index().unwrap(), 3);
        assert_eq!(store.last_index().unwrap(), 2);
        assert_eq!(store.term(2).unwrap(), 5);
        assert_eq!(
            store.initial_state().unwrap().conf_state,
            ConfState::from_voters(vec![1, 2, 3])
        );
    }

    /// A snapshot that agrees with the log at its own index is a compaction, not a replacement:
    /// the entries above it are still valid and throwing them away would cost a needless catch-up.
    #[test]
    fn a_snapshot_that_agrees_with_the_log_keeps_the_tail() {
        let mut store = MemStorage::new();
        store
            .append(&entries(&[(1, 1), (1, 2), (2, 3), (2, 4)]))
            .unwrap();
        store.apply_snapshot(snapshot(2, 1)).unwrap();
        assert_eq!(store.first_index().unwrap(), 3);
        assert_eq!(store.last_index().unwrap(), 4);
        assert_eq!(store.term(4).unwrap(), 2);
    }

    #[test]
    fn a_stale_snapshot_is_refused_rather_than_rewinding_the_log() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        store.apply_snapshot(snapshot(3, 1)).unwrap();
        assert!(matches!(
            store.apply_snapshot(snapshot(2, 1)),
            Err(RaftError::SnapshotOutOfDate {
                index: 2,
                committed: 3
            })
        ));
    }

    /// How the simulator models a crash: clone the durable state, throw the node away, rebuild.
    /// Whatever the driver had not written is simply not in the clone.
    #[test]
    fn a_clone_carries_exactly_the_durable_state() {
        let mut store = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        store.append(&entries(&[(1, 1), (1, 2)])).unwrap();
        store.set_hard_state(crate::types::HardState {
            term: 4,
            voted_for: Some(2),
            commit: 1,
        });
        let restarted = store.clone();
        store.append(&entries(&[(1, 3)])).unwrap();

        let state = restarted.initial_state().unwrap();
        assert_eq!(state.hard_state.term, 4);
        assert_eq!(state.hard_state.voted_for, Some(2));
        assert_eq!(restarted.last_index().unwrap(), 2);
        assert_eq!(store.last_index().unwrap(), 3);
    }
}
