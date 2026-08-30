//! The log as the core sees it: a durable prefix in [`LogStorage`], plus the tail that has been
//! decided but not yet written.
//!
//! That seam is where etcd-shaped Raft implementations keep their bugs, so it is one module with
//! its own tests and named helpers instead of index arithmetic spelled out at each call site
//! (`docs/plans/phase-3.md` §7 risk 3). The invariants it maintains:
//!
//! * `unstable` holds entries `[unstable_offset, unstable_offset + unstable.len())`. Everything
//!   below `unstable_offset` is answerable by storage — either as an entry or, at the compaction
//!   boundary, as a term.
//! * `committed <= last_index()` always. A node may not claim to have committed what it does not
//!   have; the leader's `leader_commit` is capped on the way in for exactly this reason.
//! * `applied <= committed`. The state machine never runs ahead of the log.
//! * A pending snapshot in `unstable_snapshot` *replaces* the log below its index. While it is
//!   pending, `first_index` and `term` answer from it, because that is what the node will have
//!   once the driver writes it.

// TODO(step-1): election and replication are the first callers of the read side here.
#![allow(dead_code)]

use crate::error::{RaftError, Result};
use crate::storage::LogStorage;
use crate::types::{Entry, Index, Snapshot, Term, offset};

/// The Raft log: a durable prefix plus an unstable tail.
#[derive(Debug)]
pub(crate) struct RaftLog<S: LogStorage> {
    /// The durable part.
    pub(crate) store: S,
    /// Entries decided but not yet persisted. Handed to the driver in `Ready::entries`.
    unstable: Vec<Entry>,
    /// The index of `unstable[0]`; equals `last durable index + 1` when `unstable` is empty.
    unstable_offset: Index,
    /// A snapshot the driver has not applied yet. It supersedes everything below its index.
    unstable_snapshot: Option<Snapshot>,
    /// The highest index known committed.
    pub(crate) committed: Index,
    /// The highest index handed to the state machine.
    pub(crate) applied: Index,
}

impl<S: LogStorage> RaftLog<S> {
    /// Opens the log over `store`, resuming from `applied`.
    pub(crate) fn new(store: S, applied: Index) -> Result<Self> {
        let last = store.last_index()?;
        let initial = store.initial_state()?;
        let first = store.first_index()?;
        // A restarted node has applied at least everything its snapshot covers, whatever the
        // caller passed: the snapshot *is* applied state.
        let applied = applied.max(first.saturating_sub(1));
        Ok(Self {
            store,
            unstable: Vec::new(),
            unstable_offset: last + 1,
            unstable_snapshot: None,
            committed: initial.hard_state.commit.max(first.saturating_sub(1)),
            applied,
        })
    }

    /// The first index still available as an entry.
    pub(crate) fn first_index(&self) -> Result<Index> {
        if let Some(snapshot) = &self.unstable_snapshot {
            return Ok(snapshot.meta.index + 1);
        }
        self.store.first_index()
    }

    /// The last index in the log, stable or not.
    pub(crate) fn last_index(&self) -> Result<Index> {
        if let Some(last) = self.unstable.last() {
            return Ok(last.index);
        }
        if let Some(snapshot) = &self.unstable_snapshot {
            return Ok(snapshot.meta.index);
        }
        self.store.last_index()
    }

    /// The term at `index`.
    ///
    /// Answers `first_index() - 1` from the snapshot metadata, because the consistency check needs
    /// the term of the entry a compacted log begins after.
    pub(crate) fn term(&self, index: Index) -> Result<Term> {
        if index >= self.unstable_offset {
            let at = offset(index - self.unstable_offset);
            return self
                .unstable
                .get(at)
                .map(|entry| entry.term)
                .ok_or(RaftError::Unavailable(index));
        }
        if let Some(snapshot) = &self.unstable_snapshot {
            if index == snapshot.meta.index {
                return Ok(snapshot.meta.term);
            }
            if index < snapshot.meta.index {
                return Err(RaftError::Compacted(snapshot.meta.index));
            }
        }
        self.store.term(index)
    }

    /// The term of the last entry — what a candidate advertises and a voter compares against
    /// (§5.4.1). An empty log has term 0, which loses every comparison, as it should.
    pub(crate) fn last_term(&self) -> Term {
        self.last_index()
            .and_then(|index| self.term(index))
            .unwrap_or(0)
    }

    /// Whether a candidate advertising `(last_index, last_term)` has a log at least as up to date
    /// as this one — Raft's §5.4.1 restriction, and the reason a leader never has to overwrite a
    /// committed entry.
    ///
    /// "At least as up to date" compares the *term* of the last entry first, and only then its
    /// index. A longer log from an older term loses to a shorter log from a newer one, because the
    /// newer term's entries are the ones that could already be committed.
    pub(crate) fn is_up_to_date(&self, last_index: Index, last_term: Term) -> bool {
        let own_term = self.last_term();
        last_term > own_term
            || (last_term == own_term && last_index >= self.last_index().unwrap_or(0))
    }

    /// Whether `(index, term)` matches this log — the `prev_log_*` check.
    pub(crate) fn matches(&self, index: Index, term: Term) -> bool {
        self.term(index).is_ok_and(|own| own == term)
    }

    /// Appends `entries` to the tail, returning the new last index. The caller has already
    /// numbered them; this is the leader's path, where no conflict is possible.
    pub(crate) fn append(&mut self, entries: Vec<Entry>) -> Result<Index> {
        if entries.is_empty() {
            return self.last_index();
        }
        debug_assert_eq!(
            entries[0].index,
            self.last_index()? + 1,
            "leader append must be contiguous"
        );
        self.truncate_and_append(entries);
        self.last_index()
    }

    /// Splices `entries` in, dropping whatever they replace. One operation, not a truncation
    /// followed by an append.
    ///
    /// Splitting the two is a trap. A truncation that reaches below the unstable tail would leave
    /// `unstable` empty with an offset below what storage still holds, and [`RaftLog::last_index`]
    /// — which falls through to storage when the tail is empty — would then report entries the log
    /// had just discarded. Keeping the replacements in the tail keeps the answer honest until the
    /// driver overwrites the durable ones.
    fn truncate_and_append(&mut self, entries: Vec<Entry>) {
        let Some(first) = entries.first().map(|entry| entry.index) else {
            return;
        };
        if first >= self.unstable_offset + self.unstable.len() as Index {
            // Purely additive.
            if self.unstable.is_empty() {
                self.unstable_offset = first;
            }
            self.unstable.extend(entries);
        } else if first <= self.unstable_offset {
            // Replaces the whole tail, and possibly entries storage still holds.
            self.unstable_offset = first;
            self.unstable = entries;
        } else {
            let keep = offset(first - self.unstable_offset);
            self.unstable.truncate(keep);
            self.unstable.extend(entries);
        }
    }

    /// The follower's append: checks `(prev_index, prev_term)` against this log, splices in
    /// whatever of `entries` is new, and advances the commit index.
    ///
    /// Returns the last index the log now holds, or `None` if the consistency check failed.
    ///
    /// The splice is deliberately conservative. Entries the log already has *with the same term*
    /// are left alone rather than rewritten, because truncating at the first index of the batch
    /// would throw away entries a later message already delivered — a reordered duplicate would
    /// then shorten the log instead of leaving it alone.
    pub(crate) fn maybe_append(
        &mut self,
        prev_index: Index,
        prev_term: Term,
        committed: Index,
        entries: Vec<Entry>,
    ) -> Result<Option<Index>> {
        if !self.matches(prev_index, prev_term) {
            return Ok(None);
        }
        let first_new = entries.first().map_or(prev_index + 1, |entry| entry.index);
        let last_new = entries.last().map_or(prev_index, |entry| entry.index);
        if let Some(conflict) = self.find_conflict(&entries)? {
            if conflict <= self.committed {
                // Rewriting a committed entry would break State Machine Safety. The only way to
                // get here is a bug or a forged message; refuse rather than corrupt.
                return Err(RaftError::Storage(format!(
                    "append conflicts at committed index {conflict} (commit {})",
                    self.committed
                )));
            }
            let already_held = offset(conflict - first_new);
            self.truncate_and_append(entries.into_iter().skip(already_held).collect());
        }
        // §5.3: a follower's commit index is the leader's, but never past what it actually holds.
        self.commit_to(committed.min(last_new))?;
        Ok(Some(last_new))
    }

    /// The first index in `entries` that disagrees with this log, or `None` if all of them either
    /// match or extend it.
    fn find_conflict(&self, entries: &[Entry]) -> Result<Option<Index>> {
        let last = self.last_index()?;
        for entry in entries {
            if entry.index > last {
                return Ok(Some(entry.index));
            }
            match self.term(entry.index) {
                Ok(term) if term == entry.term => {}
                // A compacted index cannot conflict: it is committed, and committed entries are
                // identical everywhere.
                Err(error) if error.is_compacted() => {}
                _ => return Ok(Some(entry.index)),
            }
        }
        Ok(None)
    }

    /// Advances the commit index. Never moves it backwards, and never past the log's end.
    pub(crate) fn commit_to(&mut self, index: Index) -> Result<()> {
        if index <= self.committed {
            return Ok(());
        }
        let last = self.last_index()?;
        if index > last {
            return Err(RaftError::Unavailable(index));
        }
        self.committed = index;
        Ok(())
    }

    /// Entries in `[low, high)`, subject to a byte budget, from wherever they live.
    pub(crate) fn slice(&self, low: Index, high: Index, max_bytes: u64) -> Result<Vec<Entry>> {
        if low >= high {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut budget: u64 = 0;
        if low < self.unstable_offset {
            let stable_high = high.min(self.unstable_offset);
            out = self.store.entries(low, stable_high, max_bytes)?;
            budget = out.iter().map(Entry::cost).sum();
            // Storage stopped early on the budget; do not read past the hole it left.
            if out.len() as u64 != stable_high - low {
                return Ok(out);
            }
        }
        let unstable_low = low.max(self.unstable_offset);
        for index in unstable_low..high {
            let Some(entry) = self.unstable.get(offset(index - self.unstable_offset)) else {
                return Err(RaftError::Unavailable(index));
            };
            budget = budget.saturating_add(entry.cost());
            if budget > max_bytes && !out.is_empty() {
                break;
            }
            out.push(entry.clone());
        }
        Ok(out)
    }

    /// Committed entries the state machine has not been handed yet.
    pub(crate) fn next_committed(&self, max_bytes: u64) -> Result<Vec<Entry>> {
        if self.applied >= self.committed {
            return Ok(Vec::new());
        }
        self.slice(self.applied + 1, self.committed + 1, max_bytes)
    }

    /// The tail the driver still has to persist.
    pub(crate) fn unstable_entries(&self) -> &[Entry] {
        &self.unstable
    }

    /// The snapshot the driver still has to apply.
    pub(crate) fn unstable_snapshot(&self) -> Option<&Snapshot> {
        self.unstable_snapshot.as_ref()
    }

    /// Records that entries through `index` are durable, so they stop being re-offered.
    ///
    /// The term check is what makes a lost race harmless: if the log was truncated and rewritten
    /// between the `Ready` and the `advance`, the entry at `index` no longer has the term the
    /// driver persisted, and nothing is marked stable.
    pub(crate) fn stable_to(&mut self, index: Index, term: Term) {
        if !self.term(index).is_ok_and(|own| own == term) {
            return;
        }
        if index < self.unstable_offset {
            return;
        }
        let consumed = offset(index + 1 - self.unstable_offset);
        if consumed >= self.unstable.len() {
            self.unstable.clear();
            self.unstable_offset = index + 1;
        } else {
            self.unstable.drain(..consumed);
            self.unstable_offset = index + 1;
        }
    }

    /// Records that the pending snapshot is durable.
    pub(crate) fn stable_snapshot_to(&mut self, index: Index) {
        if self
            .unstable_snapshot
            .as_ref()
            .is_some_and(|snap| snap.meta.index == index)
        {
            self.unstable_snapshot = None;
        }
    }

    /// Records that the state machine has applied through `index`.
    pub(crate) fn applied_to(&mut self, index: Index) {
        self.applied = self.applied.max(index).min(self.committed);
    }

    /// Whether a snapshot at `index` is worth installing: only if it carries the log past where
    /// this node already is. A snapshot the log has passed is stale, not an error.
    pub(crate) fn should_restore(&self, snapshot: &Snapshot) -> bool {
        if snapshot.meta.index <= self.committed {
            return false;
        }
        // If this node already has the snapshot's last entry with a matching term, the snapshot
        // tells it nothing new about the log — it can just commit forward instead of throwing the
        // tail away.
        !self.matches(snapshot.meta.index, snapshot.meta.term)
    }

    /// Replaces the log with `snapshot`. Everything the node had is discarded: the snapshot is
    /// from a leader whose log is authoritative, and mixing the two is how a follower ends up with
    /// a log that no leader ever had.
    pub(crate) fn restore(&mut self, snapshot: Snapshot) {
        self.committed = snapshot.meta.index;
        self.applied = self.applied.max(snapshot.meta.index);
        self.unstable.clear();
        self.unstable_offset = snapshot.meta.index + 1;
        self.unstable_snapshot = Some(snapshot);
    }

    /// A snapshot to send to a follower that has fallen behind the compaction boundary.
    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        if let Some(snapshot) = &self.unstable_snapshot {
            return Ok(snapshot.clone());
        }
        self.store.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::RaftLog;
    use crate::storage::MemStorage;
    use crate::types::{ConfState, Entry, Index, Snapshot, SnapshotMeta, Term};

    fn entries(spec: &[(Term, Index)]) -> Vec<Entry> {
        spec.iter()
            .map(|(term, index)| Entry::empty(*term, *index))
            .collect()
    }

    fn log_with(stable: &[(Term, Index)]) -> RaftLog<MemStorage> {
        let mut store = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        store.append(&entries(stable)).unwrap();
        RaftLog::new(store, 0).unwrap()
    }

    /// The seam: `term()` has to answer identically whether the entry is durable or still in the
    /// unstable tail.
    #[test]
    fn the_unstable_tail_reads_the_same_as_the_durable_prefix() {
        let mut log = log_with(&[(1, 1), (1, 2)]);
        log.append(entries(&[(2, 3), (2, 4)])).unwrap();
        assert_eq!(log.last_index().unwrap(), 4);
        assert_eq!(log.term(2).unwrap(), 1);
        assert_eq!(log.term(4).unwrap(), 2);
        assert_eq!(log.last_term(), 2);
        assert_eq!(
            log.slice(1, 5, u64::MAX).unwrap(),
            entries(&[(1, 1), (1, 2), (2, 3), (2, 4)])
        );
    }

    /// `stable_to` is what `advance` calls, and `advance` runs *after* the driver has written the
    /// entries — rule 1 of the driver contract. The test writes them, as a correct driver would.
    #[test]
    fn stabilising_the_tail_stops_it_being_offered_again() {
        let mut log = log_with(&[(1, 1)]);
        log.append(entries(&[(1, 2), (1, 3)])).unwrap();
        assert_eq!(log.unstable_entries().len(), 2);
        log.store.append(&entries(&[(1, 2), (1, 3)])).unwrap();
        log.stable_to(3, 1);
        assert!(log.unstable_entries().is_empty());
        assert_eq!(log.last_index().unwrap(), 3);
    }

    /// Regression. Truncating below the unstable tail used to empty it and lower its offset, which
    /// left `last_index` falling through to storage and reporting entries the log had just
    /// discarded — a follower would then advertise a longer log than it holds, and could win an
    /// election it has no right to.
    #[test]
    fn truncating_into_the_durable_prefix_shortens_the_log() {
        let mut log = log_with(&[(1, 1), (1, 2), (1, 3)]);
        assert_eq!(
            log.maybe_append(1, 1, 0, entries(&[(2, 2)])).unwrap(),
            Some(2)
        );
        assert_eq!(log.last_index().unwrap(), 2);
        assert_eq!(log.last_term(), 2);
    }

    /// If the log was truncated and rewritten between the `Ready` and the `advance`, the entry at
    /// that index no longer has the term the driver persisted, and nothing may be marked stable.
    #[test]
    fn stabilising_an_index_whose_term_changed_marks_nothing() {
        let mut log = log_with(&[(1, 1)]);
        log.append(entries(&[(1, 2)])).unwrap();
        log.maybe_append(1, 1, 0, entries(&[(2, 2)])).unwrap();
        log.stable_to(2, 1);
        assert_eq!(log.unstable_entries().len(), 1);
        assert_eq!(log.term(2).unwrap(), 2);
    }

    #[test]
    fn an_append_whose_previous_entry_does_not_match_is_rejected() {
        let mut log = log_with(&[(1, 1), (1, 2)]);
        assert_eq!(log.maybe_append(2, 5, 0, entries(&[(5, 3)])).unwrap(), None);
        assert_eq!(
            log.maybe_append(9, 1, 0, entries(&[(5, 10)])).unwrap(),
            None
        );
        assert_eq!(log.last_index().unwrap(), 2);
    }

    #[test]
    fn an_append_that_conflicts_truncates_from_the_conflict_and_no_earlier() {
        let mut log = log_with(&[(1, 1), (1, 2), (1, 3)]);
        assert_eq!(
            log.maybe_append(1, 1, 0, entries(&[(1, 2), (3, 3), (3, 4)]))
                .unwrap(),
            Some(4)
        );
        assert_eq!(log.term(2).unwrap(), 1);
        assert_eq!(log.term(3).unwrap(), 3);
        assert_eq!(log.last_index().unwrap(), 4);
    }

    /// Idempotence comes from the consistency check, not from a sequence number: a duplicate
    /// delivery must leave the log exactly as it was, never shorten it.
    #[test]
    fn a_duplicated_append_does_not_shorten_the_log() {
        let mut log = log_with(&[(1, 1)]);
        assert_eq!(
            log.maybe_append(1, 1, 0, entries(&[(1, 2), (1, 3)]))
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            log.maybe_append(1, 1, 0, entries(&[(1, 2)])).unwrap(),
            Some(2)
        );
        assert_eq!(log.last_index().unwrap(), 3);
    }

    /// §5.3: a follower's commit index is the leader's, but never past what it actually holds.
    #[test]
    fn a_follower_never_commits_past_what_it_holds() {
        let mut log = log_with(&[(1, 1)]);
        log.maybe_append(1, 1, 99, entries(&[(1, 2)])).unwrap();
        assert_eq!(log.committed, 2);
    }

    /// §5.4.1, in all four orderings of (term, index). A longer log from an older term loses to a
    /// shorter log from a newer one, because the newer term's entries are the ones that could
    /// already be committed.
    #[test]
    fn the_up_to_date_check_compares_term_before_length() {
        let log = log_with(&[(1, 1), (2, 2), (2, 3)]);
        assert!(log.is_up_to_date(3, 2), "identical logs are up to date");
        assert!(log.is_up_to_date(4, 2), "longer at the same term wins");
        assert!(
            log.is_up_to_date(1, 3),
            "shorter at a newer term still wins"
        );
        assert!(!log.is_up_to_date(9, 1), "longer at an older term loses");
        assert!(!log.is_up_to_date(2, 2), "shorter at the same term loses");
    }

    #[test]
    fn committed_entries_are_handed_out_once_and_in_order() {
        let mut log = log_with(&[(1, 1), (1, 2), (1, 3)]);
        log.commit_to(3).unwrap();
        assert_eq!(
            log.next_committed(u64::MAX).unwrap(),
            entries(&[(1, 1), (1, 2), (1, 3)])
        );
        log.applied_to(3);
        assert!(log.next_committed(u64::MAX).unwrap().is_empty());
    }

    #[test]
    fn the_commit_index_never_moves_backwards_or_past_the_log() {
        let mut log = log_with(&[(1, 1), (1, 2)]);
        log.commit_to(2).unwrap();
        log.commit_to(1).unwrap();
        assert_eq!(log.committed, 2);
        assert!(log.commit_to(5).is_err());
    }

    #[test]
    fn a_snapshot_the_log_has_already_passed_is_not_worth_installing() {
        let mut log = log_with(&[(1, 1), (1, 2), (1, 3)]);
        log.commit_to(3).unwrap();
        let stale = Snapshot {
            meta: SnapshotMeta {
                index: 2,
                term: 1,
                conf: ConfState::from_voters(vec![1]),
            },
            data: bytes::Bytes::new(),
        };
        assert!(!log.should_restore(&stale));
    }

    /// A snapshot whose last entry this node already holds with a matching term tells it nothing
    /// new about the log — it can commit forward instead of throwing its tail away.
    #[test]
    fn a_snapshot_the_log_already_matches_is_not_worth_installing() {
        let mut log = log_with(&[(1, 1), (1, 2), (1, 3)]);
        log.commit_to(1).unwrap();
        let matching = Snapshot {
            meta: SnapshotMeta {
                index: 2,
                term: 1,
                conf: ConfState::from_voters(vec![1]),
            },
            data: bytes::Bytes::new(),
        };
        assert!(!log.should_restore(&matching));
    }

    #[test]
    fn restoring_a_snapshot_replaces_the_log_and_answers_from_its_metadata() {
        let mut log = log_with(&[(1, 1), (1, 2)]);
        let snapshot = Snapshot {
            meta: SnapshotMeta {
                index: 9,
                term: 4,
                conf: ConfState::from_voters(vec![1, 2]),
            },
            data: bytes::Bytes::from_static(b"state"),
        };
        assert!(log.should_restore(&snapshot));
        log.restore(snapshot);
        assert_eq!(log.first_index().unwrap(), 10);
        assert_eq!(log.last_index().unwrap(), 9);
        assert_eq!(log.term(9).unwrap(), 4);
        assert_eq!(log.committed, 9);
        assert!(log.unstable_snapshot().is_some());
        log.stable_snapshot_to(9);
        assert!(log.unstable_snapshot().is_none());
    }

    /// A node restarting over a compacted log has applied everything the snapshot covers,
    /// whatever the caller passed as `applied`.
    #[test]
    fn reopening_a_compacted_log_starts_applied_at_the_boundary() {
        let mut store = MemStorage::new();
        store.append(&entries(&[(1, 1), (1, 2), (1, 3)])).unwrap();
        store.compact(2).unwrap();
        let log = RaftLog::new(store, 0).unwrap();
        assert_eq!(log.applied, 2);
        assert_eq!(log.committed, 2);
    }
}
