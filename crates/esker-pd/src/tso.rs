//! The timestamp oracle: the clock the whole cluster borrows.
//!
//! `CLAUDE.md` invariant 6 — *timestamps come only from PD's TSO; no node uses its wall clock
//! for ordering* — makes this the single point of correctness for every ordering decision above
//! it. `esker-txn` will take a `start_ts` and a `commit_ts` from here for every transaction and
//! compare them across machines (`docs/DESIGN.md` §8); a timestamp that repeats or goes
//! backwards is a lost update that no test in phase 5 will reproduce, because it needs a crash
//! at one particular instant to appear at all.
//!
//! So the ordering rules are written here, in one place, and both are checked by killing a
//! process rather than only by unit tests (`tests/crash_kill.rs`).
//!
//! # The mark
//!
//! `ts = physical_ms << 18 | logical` (the layout is pinned in [`crate`]). PD keeps a
//! **high-water mark** on disk, and the rule is:
//!
//! > every timestamp ever handed out has `physical < mark`, and the mark is fsynced before the
//! > first timestamp that would break that leaves the process.
//!
//! A restart resumes at `max(clock, mark)`. Since every timestamp already given away had a
//! physical part strictly below the mark, and the new physical part is at least the mark,
//! **nothing can repeat** — whatever the clock says. That is the property, and it is worth
//! stating what it survives:
//!
//! * a clock that jumps **backwards** across the restart (NTP correction, a dead battery): the
//!   mark wins, and time carries on from where it was;
//! * a clock that stands still: allocations continue in the logical bits of the same
//!   millisecond, and roll into the next one when those run out;
//! * a crash between the mark going down and the timestamp going out: the timestamps that were
//!   never handed out are skipped, which costs nothing.
//!
//! The mark is written `save_interval` milliseconds *ahead* — 3 s by default
//! (`docs/DESIGN.md` §7) — so the fsync happens once every 3 s of issued time rather than once
//! per batch. Making the interval smaller costs throughput; making it larger costs a longer
//! jump forward after a crash, and nothing else.
//!
//! # Why the persist is a callback
//!
//! Same reason as [`crate::alloc`]: it makes the *ordering* testable. A test can hand this an
//! [`Oracle::allocate`] whose persist records the marks it was asked for, and assert that no
//! timestamp was ever returned above one — which is the invariant, stated directly.

use crate::error::{PdError, Result};
use crate::record::TsoRecord;
use crate::{TSO_LOGICAL_BITS, TSO_MAX_LOGICAL, compose_ts};

/// Timestamps in one millisecond: the whole logical space.
pub const LOGICAL_SPACE: u64 = TSO_MAX_LOGICAL + 1;

/// The largest physical millisecond the layout can carry.
///
/// Above this the shift would drop the high bits and two different instants would compose to
/// the same timestamp, so a clock reading past it is refused rather than truncated. It is
/// somewhere around the year 4200; a machine reading that is broken, and PD saying so is much
/// better than PD quietly handing out colliding timestamps.
pub const MAX_PHYSICAL_MS: u64 = (1 << (u64::BITS - TSO_LOGICAL_BITS)) - 1;

/// Hands out ordered timestamps, and keeps the mark that makes a restart safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oracle {
    physical_ms: u64,
    logical: u64,
    high_water_ms: u64,
    save_interval_ms: u64,
}

impl Oracle {
    /// The oracle a freshly opened PD starts with: `max(clock, mark)`.
    ///
    /// The maximum is the whole restart rule. Taking the clock alone would repeat timestamps
    /// after a backwards jump; taking the mark alone would freeze time on a PD that was down
    /// for a week.
    #[must_use]
    pub fn load(record: Option<TsoRecord>, now_ms: u64, save_interval_ms: u64) -> Self {
        let high_water_ms = record.map_or(0, |record| record.high_water_ms);
        Self {
            physical_ms: now_ms.max(high_water_ms),
            logical: 0,
            high_water_ms,
            save_interval_ms: save_interval_ms.max(1),
        }
    }

    /// A run of `count` consecutive timestamps, starting at the returned one.
    ///
    /// They are consecutive as **integers**: `start + i` for `i < count` is the timestamp with
    /// logical part `logical + i`, because the batch is never allowed to straddle a
    /// millisecond. That is what lets a caller hold one number and count.
    ///
    /// `persist` is called with a new mark *before* any timestamp above the old one is
    /// returned, and must not come back until that mark is durable.
    pub fn allocate(
        &mut self,
        count: u32,
        now_ms: u64,
        mut persist: impl FnMut(u64) -> Result<()>,
    ) -> Result<u64> {
        let count = u64::from(count);
        if count == 0 {
            return Err(PdError::invalid("tso needs a count of at least one"));
        }
        if count > LOGICAL_SPACE {
            return Err(PdError::invalid(format!(
                "tso batch of {count} is larger than a millisecond's {LOGICAL_SPACE} timestamps"
            )));
        }

        if now_ms > self.physical_ms {
            // The normal case: physical time has moved on, and the logical counter restarts.
            // A clock that went *backwards* takes the other branch and changes nothing, which
            // is exactly the point — the oracle never follows a clock down.
            self.physical_ms = now_ms;
            self.logical = 0;
        }
        if self.logical + count > LOGICAL_SPACE {
            // This millisecond is full. Borrowing from the next one is safe because the mark
            // is checked afterwards, so the borrowed millisecond is covered too.
            self.physical_ms = self
                .physical_ms
                .checked_add(1)
                .ok_or_else(|| PdError::internal("the timestamp space is exhausted"))?;
            self.logical = 0;
        }
        if self.physical_ms > MAX_PHYSICAL_MS {
            return Err(PdError::internal(format!(
                "the clock reads {} ms, past the end of the timestamp space",
                self.physical_ms
            )));
        }

        if self.physical_ms >= self.high_water_ms {
            let mark = self
                .physical_ms
                .checked_add(self.save_interval_ms)
                .ok_or_else(|| PdError::internal("the timestamp space is exhausted"))?;
            // Durable first. Nothing below this line runs until the mark is on disk.
            persist(mark)?;
            self.high_water_ms = mark;
        }

        let start = compose_ts(self.physical_ms, self.logical);
        self.logical += count;
        Ok(start)
    }

    /// The mark on disk. Every timestamp handed out is strictly below it.
    #[must_use]
    pub fn high_water_ms(&self) -> u64 {
        self.high_water_ms
    }

    /// The millisecond the next timestamp will come from.
    #[must_use]
    pub fn physical_ms(&self) -> u64 {
        self.physical_ms
    }

    /// The record for the current mark.
    #[must_use]
    pub fn record(&self) -> TsoRecord {
        TsoRecord {
            high_water_ms: self.high_water_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LOGICAL_SPACE, MAX_PHYSICAL_MS, Oracle};
    use crate::record::TsoRecord;
    use crate::{TSO_SAVE_INTERVAL_MS, decompose_ts};
    use std::cell::RefCell;

    /// A persist that records the marks it was asked to make durable, and the timestamps that
    /// were handed out afterwards — which is what the invariant is stated in terms of.
    #[derive(Debug, Default)]
    struct Disk {
        marks: RefCell<Vec<u64>>,
    }

    impl Disk {
        fn persist(&self) -> impl FnMut(u64) -> crate::error::Result<()> + '_ {
            |mark| {
                self.marks.borrow_mut().push(mark);
                Ok(())
            }
        }

        fn mark(&self) -> u64 {
            self.marks.borrow().last().copied().unwrap_or(0)
        }
    }

    fn take(oracle: &mut Oracle, count: u32, now_ms: u64, disk: &Disk) -> u64 {
        let ts = oracle.allocate(count, now_ms, disk.persist()).unwrap();
        // The invariant, checked on every single allocation this module's tests make.
        let (physical, _) = decompose_ts(ts);
        assert!(
            physical < oracle.high_water_ms(),
            "handed out {physical} at or above the mark {}",
            oracle.high_water_ms()
        );
        assert!(
            physical < disk.mark(),
            "handed out {physical} at or above the mark {} that reached the disk",
            disk.mark()
        );
        ts
    }

    #[test]
    fn timestamps_are_consecutive_integers_within_a_batch() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, 1_000, TSO_SAVE_INTERVAL_MS);
        let start = take(&mut oracle, 4, 1_000, &disk);
        let next = take(&mut oracle, 1, 1_000, &disk);
        assert_eq!(next, start + 4, "a batch of 4 owns start..start+4");
        let (physical, logical) = decompose_ts(start);
        assert_eq!((physical, logical), (1_000, 0));
    }

    /// The mark is written ahead, not per batch: the fsync is amortised over the interval.
    #[test]
    fn the_mark_is_persisted_ahead_and_not_once_per_batch() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, 1_000, TSO_SAVE_INTERVAL_MS);
        take(&mut oracle, 1, 1_000, &disk);
        assert_eq!(
            disk.marks.borrow().as_slice(),
            &[1_000 + TSO_SAVE_INTERVAL_MS]
        );

        // Everything inside the interval is free.
        for now in [1_001, 2_000, 1_000 + TSO_SAVE_INTERVAL_MS - 1] {
            take(&mut oracle, 1, now, &disk);
        }
        assert_eq!(disk.marks.borrow().len(), 1, "the mark was rewritten early");

        // Reaching the mark writes the next one.
        take(&mut oracle, 1, 1_000 + TSO_SAVE_INTERVAL_MS, &disk);
        assert_eq!(disk.marks.borrow().len(), 2);
        assert_eq!(disk.mark(), 1_000 + 2 * TSO_SAVE_INTERVAL_MS);
    }

    /// The property the whole module exists for. A clock that goes *backwards* across a
    /// restart — the case that actually happens, after an NTP correction or on a machine with
    /// a dead battery — must not produce a timestamp that was already handed out.
    #[test]
    fn a_restart_with_a_backwards_clock_never_repeats_a_timestamp() {
        const NOW: u64 = 1_700_000_000_000;
        const A_DAY_BEHIND: u64 = NOW - 86_400_000;

        let disk = Disk::default();
        let mut before = Vec::new();
        let mut oracle = Oracle::load(None, NOW, TSO_SAVE_INTERVAL_MS);
        for now in [NOW, NOW + 1, NOW + 2] {
            before.push(take(&mut oracle, 8, now, &disk));
        }
        let mark = disk.mark();

        // The process dies. It comes back with a clock a full day behind.
        let mut after = Oracle::load(
            Some(TsoRecord {
                high_water_ms: mark,
            }),
            A_DAY_BEHIND,
            TSO_SAVE_INTERVAL_MS,
        );
        let resumed = take(&mut after, 1, A_DAY_BEHIND, &disk);

        let highest = before.iter().copied().max().unwrap_or(0);
        assert!(
            resumed > highest,
            "restart handed out {resumed}, at or below {highest} from before the crash"
        );
        assert!(decompose_ts(resumed).0 >= mark, "the mark did not win");
    }

    /// A clock standing still is a clock that has stopped moving *forwards*; the oracle keeps
    /// going in the logical bits and rolls the millisecond when they run out.
    #[test]
    fn a_stalled_clock_rolls_the_millisecond_rather_than_repeating() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, 500, TSO_SAVE_INTERVAL_MS);
        let batch = u32::try_from(LOGICAL_SPACE / 4).unwrap();
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(take(&mut oracle, batch, 500, &disk));
        }
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "timestamps went backwards under a stalled clock: {seen:?}"
        );
        assert!(
            decompose_ts(*seen.last().unwrap()).0 > 500,
            "the millisecond never rolled"
        );
    }

    /// A batch may not straddle a millisecond, or `start + i` would not be the i-th timestamp.
    #[test]
    fn a_batch_never_straddles_a_millisecond() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, 500, TSO_SAVE_INTERVAL_MS);
        let batch = u32::try_from(LOGICAL_SPACE - 2).unwrap();
        let first = take(&mut oracle, batch, 500, &disk);
        let second = take(&mut oracle, 4, 500, &disk);
        assert_eq!(decompose_ts(first), (500, 0));
        assert_eq!(
            decompose_ts(second),
            (501, 0),
            "the batch that did not fit should have started the next millisecond"
        );
    }

    #[test]
    fn a_count_of_zero_or_more_than_a_millisecond_holds_is_refused() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, 500, TSO_SAVE_INTERVAL_MS);
        assert!(oracle.allocate(0, 500, disk.persist()).is_err());
        let too_many = u32::try_from(LOGICAL_SPACE + 1).unwrap();
        assert!(oracle.allocate(too_many, 500, disk.persist()).is_err());
        // The largest legal batch is a whole millisecond.
        let all = u32::try_from(LOGICAL_SPACE).unwrap();
        assert!(oracle.allocate(all, 500, disk.persist()).is_ok());
    }

    /// If the mark cannot be made durable, no timestamp is handed out. Otherwise a crash right
    /// after would resume below timestamps that are already in use.
    #[test]
    fn a_failed_persist_hands_out_nothing() {
        let mut oracle = Oracle::load(None, 500, TSO_SAVE_INTERVAL_MS);
        let before = oracle;
        let failed = oracle.allocate(1, 500, |_| {
            Err(crate::error::PdError::internal("the disk is gone"))
        });
        assert!(failed.is_err());
        assert_eq!(oracle, before, "a failed persist moved the oracle");
    }

    /// A clock past the end of the layout is refused rather than truncated: a shift that drops
    /// the high bits would compose two different instants to the same timestamp.
    #[test]
    fn a_clock_past_the_end_of_the_space_is_refused() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(None, MAX_PHYSICAL_MS + 1, TSO_SAVE_INTERVAL_MS);
        assert!(
            oracle
                .allocate(1, MAX_PHYSICAL_MS + 1, disk.persist())
                .is_err()
        );
        assert!(
            disk.marks.borrow().is_empty(),
            "a refused clock wrote a mark"
        );
    }

    /// A mark from the future — the state a crash right after a persist leaves — is not a
    /// reason to hand out a timestamp below it.
    #[test]
    fn a_mark_ahead_of_the_clock_wins() {
        let disk = Disk::default();
        let mut oracle = Oracle::load(
            Some(TsoRecord {
                high_water_ms: 9_000,
            }),
            1_000,
            TSO_SAVE_INTERVAL_MS,
        );
        let ts = take(&mut oracle, 1, 1_000, &disk);
        assert_eq!(decompose_ts(ts).0, 9_000);
        assert_eq!(disk.mark(), 9_000 + TSO_SAVE_INTERVAL_MS);
    }
}
