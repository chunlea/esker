//! A removed peer's range is reclaimed exactly once, and never while a hosted region overlaps it.
//!
//! The mechanism is `92a5add` / ADR 0034. Two things were wrong and the first hid the second.
//!
//! A configuration change takes effect when it is **appended**, so the instant a leader appends
//! `Remove(n)` its progress has no entry for `n`, and the entry saying `n` is gone is the first
//! one `n` is not sent. `retire_region` had one caller — a peer applying the conf change that
//! removed it — so on the operator path it never ran at all. The removed peer stays, leaderless,
//! campaigning for ever against a group that has replaced it, and a restart does not heal it. Once
//! it did run, the second half: the range stayed on disk in all three column families, under no
//! region, served by nothing.
//!
//! # What this model explores that the fix's test does not
//!
//! `esker-store/tests/retire.rs` drives one ordering: place a replica, shed it, watch the range
//! go. That is the case where the sweep is *supposed* to fire. Every other answer PD can give is
//! a case where it must not, and each of those is a way to lose acknowledged writes — a store
//! that drops a region it still holds is strictly worse than one that keeps a region it does not.
//!
//! So this enumerates the answers instead of the timings. The safety is entirely in the three
//! conditions the probe applies, and the fail-closed direction is the whole design: `Keep` is the
//! answer to everything that is not positive evidence of a removal.
//!
//! # The ground truth is ownership, not the conditions
//!
//! [`Case::expected`] is written from **who owns the range**, not from what the code checks. "PD
//! holds a newer record for this range and it does not name this store" is evidence of a removal;
//! a record at the same `conf_ver` is not evidence of anything, and a record for a different
//! region is an answer about a different range. Deriving the expectation from the code's own
//! conditions would make this a transcription, and a transcription agrees with the bug.

/// A region, as the model names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionSpan {
    /// Cluster-unique region id.
    pub region_id: u64,
    /// The membership half of the epoch.
    pub conf_ver: u64,
    /// Stores holding a peer of it.
    pub peers_on: Vec<u64>,
}

impl RegionSpan {
    /// Whether this record names a peer on `store_id`.
    #[must_use]
    pub fn names(&self, store_id: u64) -> bool {
        self.peers_on.contains(&store_id)
    }
}

/// What the placement driver says when asked about the range a hosted region covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdAnswer {
    /// PD has never heard of the range, or could not be reached. **Not evidence of anything.**
    Silent,
    /// PD holds this record for the range.
    Holds(RegionSpan),
}

/// One question put to the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// A name a failure can print.
    pub name: &'static str,
    /// The region this store hosts, as this store's own record has it.
    pub hosted: RegionSpan,
    /// What PD answers about its range.
    pub answer: PdAnswer,
    /// Whether this store still hosts some *other* region overlapping the range.
    ///
    /// The second gate, and the one that catches a parent narrowed by a split retiring against
    /// the range it used to have.
    pub overlapping_hosted: bool,
    /// The store the sweep is running on.
    pub store_id: u64,
}

/// What the model says must happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// The region is retired and its range reclaimed.
    Reclaim,
    /// The region and its keys are left exactly as they were, for this reason.
    Keep(&'static str),
}

impl Case {
    /// What must happen, derived from **who owns the range** rather than from what the code
    /// checks.
    ///
    /// The asymmetry is deliberate and is the design: keeping a region this store has been
    /// removed from costs disk and a peer that campaigns for ever; dropping one it still holds
    /// loses acknowledged writes (invariant 5). So `Reclaim` needs positive evidence and
    /// everything else is `Keep`.
    #[must_use]
    pub fn expected(&self) -> Expected {
        if self.overlapping_hosted {
            return Expected::Keep(
                "a region this store still hosts covers the range, so emptying it would delete \
                 that region's keys under its owner",
            );
        }
        let PdAnswer::Holds(record) = &self.answer else {
            return Expected::Keep(
                "PD said nothing about the range, which is not evidence of a removal",
            );
        };
        if record.region_id != self.hosted.region_id {
            return Expected::Keep(
                "PD answered about a different region, which says nothing about this one",
            );
        }
        if record.conf_ver <= self.hosted.conf_ver {
            return Expected::Keep(
                "PD's record is no newer than this store's, so it describes a membership this \
                 store has already applied rather than one it has not heard about",
            );
        }
        if record.names(self.store_id) {
            return Expected::Keep("the newer membership still names a peer on this store");
        }
        Expected::Reclaim
    }
}

/// What the store actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    /// Whether the store still hosts the region.
    pub still_hosted: bool,
    /// Keys left inside the range, summed over every shipped column family.
    pub keys_left: usize,
    /// Keys the range held before the sweep ran. A "gone" assertion against a range that was
    /// never filled is how a test of a deletion passes while deleting nothing.
    pub keys_before: usize,
}

/// A broken case, with everything a reader needs to know which half broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The case's name.
    pub case: &'static str,
    /// What the model required.
    pub expected: Expected,
    /// What happened.
    pub observed: Observed,
    /// Which half of the invariant this is.
    pub half: Half,
}

/// Which half of "reclaimed exactly once, and never while a hosted region overlaps it" broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    /// A range that should have been reclaimed was not. Disk, and a peer campaigning for ever.
    NotReclaimed,
    /// A range that should have been left alone was emptied. **Acknowledged writes.**
    ReclaimedWrongly,
    /// The region was retired or kept when the opposite was required, without the keys moving.
    WrongHosting,
    /// The range held nothing before the sweep, so the case asserts nothing.
    NeverFilled,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "case {:?}: required {:?}, observed {:?} ({:?}). ADR 0034: a removed peer's range is \
             reclaimed exactly once, and never while any region this store still hosts overlaps \
             it",
            self.case, self.expected, self.observed, self.half
        )
    }
}

impl std::error::Error for Violation {}

/// Checks one case against what the store did.
///
/// # Errors
///
/// A [`Violation`] naming which half of the invariant broke.
pub fn check(case: &Case, observed: Observed) -> Result<(), Violation> {
    let expected = case.expected();
    let fail = |half| {
        Err(Violation {
            case: case.name,
            expected,
            observed,
            half,
        })
    };
    if observed.keys_before == 0 {
        return fail(Half::NeverFilled);
    }
    match expected {
        Expected::Reclaim => {
            if observed.keys_left != 0 {
                return fail(Half::NotReclaimed);
            }
            if observed.still_hosted {
                return fail(Half::WrongHosting);
            }
            Ok(())
        }
        Expected::Keep(_) => {
            if observed.keys_left != observed.keys_before {
                return fail(Half::ReclaimedWrongly);
            }
            if !observed.still_hosted {
                return fail(Half::WrongHosting);
            }
            Ok(())
        }
    }
}

/// Every answer PD can give about a range this store hosts, and both states of the second gate.
///
/// Exhaustive over what the decision actually reads. There is no seed here because there is
/// nothing to draw: the state space *is* the answer table, and enumerating it is cheaper and
/// stronger than sampling it.
#[must_use]
pub fn cases(store_id: u64, region_id: u64, hosted_conf_ver: u64) -> Vec<Case> {
    let hosted = RegionSpan {
        region_id,
        conf_ver: hosted_conf_ver,
        // This store's own record still names it — it never applied the change that removed it,
        // and never will. That is why the caller passes the newer record separately.
        peers_on: vec![1, store_id],
    };
    let newer_without_us = RegionSpan {
        region_id,
        conf_ver: hosted_conf_ver + 1,
        peers_on: vec![1],
    };

    vec![
        Case {
            name: "a newer membership that does not name this store",
            hosted: hosted.clone(),
            answer: PdAnswer::Holds(newer_without_us.clone()),
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "a newer membership that still names this store",
            hosted: hosted.clone(),
            answer: PdAnswer::Holds(RegionSpan {
                conf_ver: hosted_conf_ver + 1,
                peers_on: vec![1, store_id],
                ..newer_without_us.clone()
            }),
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "the same membership this store already has",
            hosted: hosted.clone(),
            answer: PdAnswer::Holds(RegionSpan {
                conf_ver: hosted_conf_ver,
                peers_on: vec![1],
                ..newer_without_us.clone()
            }),
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "an older membership than this store's",
            hosted: hosted.clone(),
            answer: PdAnswer::Holds(RegionSpan {
                conf_ver: hosted_conf_ver.saturating_sub(1),
                peers_on: vec![1],
                ..newer_without_us.clone()
            }),
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "a different region at the same start key",
            hosted: hosted.clone(),
            answer: PdAnswer::Holds(RegionSpan {
                region_id: region_id + 100,
                conf_ver: hosted_conf_ver + 1,
                peers_on: vec![1],
            }),
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "PD has never heard of the range",
            hosted: hosted.clone(),
            answer: PdAnswer::Silent,
            overlapping_hosted: false,
            store_id,
        },
        Case {
            name: "a newer membership, but a region this store hosts covers the range",
            hosted,
            answer: PdAnswer::Holds(newer_without_us),
            overlapping_hosted: true,
            store_id,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{Expected, cases};

    #[test]
    fn exactly_one_case_reclaims_and_it_is_the_evidenced_one() {
        let table = cases(2, 1, 3);
        let reclaims: Vec<&'static str> = table
            .iter()
            .filter(|case| case.expected() == Expected::Reclaim)
            .map(|case| case.name)
            .collect();
        assert_eq!(
            reclaims,
            vec!["a newer membership that does not name this store"],
            "the table has to be fail-closed: everything but positive evidence of a removal is a \
             Keep, or this model would be asking the store to lose acknowledged writes"
        );
    }

    #[test]
    fn the_overlap_gate_outranks_the_evidence() {
        let table = cases(2, 1, 3);
        let overlapping = table
            .iter()
            .find(|case| case.overlapping_hosted)
            .expect("the table covers the second gate");
        assert!(
            matches!(overlapping.expected(), Expected::Keep(_)),
            "a hosted region covering the range must stop the clear even when the evidence of \
             removal is perfect: that is the case where a parent narrowed by a split would delete \
             its own child's keys"
        );
    }
}
