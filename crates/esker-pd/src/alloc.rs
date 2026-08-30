//! The id allocator: cluster-unique ids for regions and peers, in persisted batches.
//!
//! One rule, and the whole module is it: **the end of a batch is persisted before any id in
//! that batch is handed out.** A crash therefore skips the unused tail of the current batch —
//! a restart resumes at `allocated_end + 1` — and can never hand out an id twice. Ids being
//! cheap and the alternative being two regions with the same id, wasting a thousand of them
//! per crash is not a trade worth thinking about.
//!
//! The persist is a callback rather than a database handle so that this stays a pure state
//! machine: the tests drive it with a `reserve` that records what it was asked to make durable
//! and can be made to fail, which is how "persisted *before* handed out" is checked at all. A
//! version that wrote to the engine itself could only be tested by crashing a process — which
//! `tests/crash_kill.rs` also does, because a rule this load-bearing deserves both.

use crate::error::{PdError, Result};
use crate::record::AllocRecord;

/// Ids reserved per persist. Large enough that a busy PD is not fsyncing per id, small enough
/// that a crash wastes an irrelevant number of them.
pub const ALLOC_BATCH: u64 = 1_000;

/// Hands out monotone ids from a reserved batch.
///
/// `next == 0` means the id space is exhausted. Zero is not an id — nothing in this codebase
/// numbers anything from zero — so it is free to carry that meaning, and it saves a flag that
/// could disagree with the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocator {
    /// The next id to hand out, or zero when there are none left.
    next: u64,
    /// The last id reserved on disk. Ids up to here may be handed out without persisting.
    allocated_end: u64,
    /// How many ids one persist reserves.
    batch: u64,
}

impl Allocator {
    /// The allocator a freshly opened PD starts with.
    ///
    /// `None` is a database that has never allocated: ids start at **1**, because zero is not
    /// an id anywhere in this codebase. A record resumes at `allocated_end + 1`, skipping
    /// whatever the previous process had reserved and not used.
    #[must_use]
    pub fn load(record: Option<AllocRecord>, batch: u64) -> Self {
        let allocated_end = record.map_or(0, |record| record.allocated_end);
        Self {
            // Wrapping is the exhausted sentinel: a database whose last reserved id is u64::MAX
            // has none left, and reopening it must not start again at 1.
            next: allocated_end.wrapping_add(1),
            allocated_end,
            batch: batch.max(1),
        }
    }

    /// The first id of a run of `count` consecutive ids.
    ///
    /// `reserve` is called with the new `allocated_end` **before** any of the ids leave, and
    /// must not return until that value is durable. If it fails, nothing is handed out and the
    /// allocator is unchanged — a caller that retries gets the same ids, because none of them
    /// were given away.
    pub fn allocate(
        &mut self,
        count: u64,
        mut reserve: impl FnMut(u64) -> Result<()>,
    ) -> Result<u64> {
        if count == 0 {
            return Err(PdError::invalid("alloc_id needs a count of at least one"));
        }
        if self.next == 0 {
            return Err(PdError::internal("the id space is exhausted"));
        }
        let last = self
            .next
            .checked_add(count - 1)
            .ok_or_else(|| PdError::internal("the id space is exhausted"))?;

        if last > self.allocated_end {
            // Round the reservation up to a whole batch past what this call needs, so that a
            // caller asking for more ids than a batch holds still gets them in one persist.
            let batches = (last - self.allocated_end).div_ceil(self.batch);
            // Saturating, not checked: rounding the reservation up past the end of the id space
            // must not refuse the ids that are still left below it.
            let end = self
                .allocated_end
                .saturating_add(batches.saturating_mul(self.batch));
            // Durable first. Everything below this line is unreachable until it is.
            reserve(end)?;
            self.allocated_end = end;
        }

        let start = self.next;
        self.next = last.wrapping_add(1);
        Ok(start)
    }

    /// The next id this allocator would hand out, or zero when the space is exhausted.
    #[must_use]
    pub fn next_id(&self) -> u64 {
        self.next
    }

    /// The last id reserved on disk.
    #[must_use]
    pub fn allocated_end(&self) -> u64 {
        self.allocated_end
    }

    /// The record to persist for this allocator's current reservation.
    #[must_use]
    pub fn record(&self) -> AllocRecord {
        AllocRecord {
            allocated_end: self.allocated_end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ALLOC_BATCH, Allocator};
    use crate::error::PdError;
    use crate::record::AllocRecord;
    use std::cell::RefCell;

    /// A `reserve` that records what it was asked to make durable.
    #[derive(Debug, Default)]
    struct Journal {
        reserved: RefCell<Vec<u64>>,
    }

    impl Journal {
        fn reserve(&self) -> impl FnMut(u64) -> crate::error::Result<()> + '_ {
            |end| {
                self.reserved.borrow_mut().push(end);
                Ok(())
            }
        }

        fn last(&self) -> Option<u64> {
            self.reserved.borrow().last().copied()
        }
    }

    #[test]
    fn ids_start_at_one_and_are_consecutive() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(None, ALLOC_BATCH);
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), 1);
        assert_eq!(alloc.allocate(2, journal.reserve()).unwrap(), 2);
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), 4);
    }

    /// The reservation happens once per batch, not once per id: an fsync per region id would
    /// make a split storm PD's problem rather than the store's.
    #[test]
    fn one_persist_covers_a_whole_batch() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(None, 4);
        for expected in 1..=4 {
            assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), expected);
        }
        assert_eq!(journal.reserved.borrow().as_slice(), &[4]);
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), 5);
        assert_eq!(journal.reserved.borrow().as_slice(), &[4, 8]);
    }

    /// A request larger than one batch is still one persist, and still consecutive.
    #[test]
    fn a_request_larger_than_a_batch_is_reserved_in_one_go() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(None, 4);
        assert_eq!(alloc.allocate(10, journal.reserve()).unwrap(), 1);
        assert_eq!(journal.reserved.borrow().as_slice(), &[12]);
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), 11);
        assert_eq!(
            journal.reserved.borrow().as_slice(),
            &[12],
            "still reserved"
        );
    }

    /// The crash-safety property, in the small: a reopen resumes past everything that was
    /// reserved, so the ids the dead process might have handed out are never seen again.
    #[test]
    fn a_reopen_never_reuses_a_reserved_id() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(None, ALLOC_BATCH);
        let first = alloc.allocate(1, journal.reserve()).unwrap();
        let reserved = journal.last().unwrap();

        // The process dies here. Everything between `first` and `reserved` may or may not have
        // been handed out, and the new allocator must assume it was.
        let mut reopened = Allocator::load(
            Some(AllocRecord {
                allocated_end: reserved,
            }),
            ALLOC_BATCH,
        );
        let after = reopened.allocate(1, journal.reserve()).unwrap();
        assert!(
            after > reserved,
            "id {after} was inside the reserved batch [{first}, {reserved}]"
        );
    }

    /// If the persist fails, nothing may be handed out — otherwise a crash right after would
    /// resume below ids that are already in use.
    #[test]
    fn a_failed_reserve_hands_out_nothing() {
        let mut alloc = Allocator::load(None, ALLOC_BATCH);
        let before = alloc;
        let failed = alloc.allocate(1, |_| Err(PdError::internal("disk is on fire")));
        assert!(failed.is_err());
        assert_eq!(alloc, before, "a failed reservation moved the allocator");

        // And the next successful call gets the id the failed one would have.
        let journal = Journal::default();
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), 1);
    }

    #[test]
    fn a_count_of_zero_is_refused() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(None, ALLOC_BATCH);
        assert!(alloc.allocate(0, journal.reserve()).is_err());
    }

    /// The end of the id space is an error, not a wrap. Two regions numbered 1 is worse than
    /// a cluster that stops handing out ids — and the last id in the space is still handed out
    /// rather than lost to the rounding.
    #[test]
    fn the_end_of_the_id_space_is_an_error() {
        let journal = Journal::default();
        let mut alloc = Allocator::load(
            Some(AllocRecord {
                allocated_end: u64::MAX - 1,
            }),
            ALLOC_BATCH,
        );
        assert_eq!(alloc.allocate(1, journal.reserve()).unwrap(), u64::MAX);
        assert_eq!(journal.last(), Some(u64::MAX), "the reservation saturates");
        assert!(alloc.allocate(1, journal.reserve()).is_err());

        // And a database reopened at the very end of the space does not start again at 1.
        let mut reopened = Allocator::load(
            Some(AllocRecord {
                allocated_end: u64::MAX,
            }),
            ALLOC_BATCH,
        );
        assert!(reopened.allocate(1, journal.reserve()).is_err());
    }
}
