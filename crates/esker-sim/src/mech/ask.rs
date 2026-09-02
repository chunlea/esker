//! A snapshot is never served to a peer the applied record does not name — and a peer the core
//! names is waited for rather than refused on sight.
//!
//! The mechanism is `1502d0f` / ADR 0035, and the interesting thing about it is that it is a fix
//! for a fix. A conf change takes effect in the core when it is **appended** and in the region
//! record when it is **applied**, and between those two a leader knows about a peer its own record
//! does not list. That gap is exactly where a new peer asks, because the traffic telling it the
//! region exists is the traffic the leader started sending the moment it appended. Reading only
//! the record answered "peer N is not a member of region M" — a correct sentence about the wrong
//! membership.
//!
//! `phase-8-learner` §close bullet 4 suggested the sender read the **core's** membership instead.
//! Implemented exactly as written, the new test went green and `tests/promotion.rs` failed 3 of 3
//! with a learner stranded for the whole deadline: a snapshot's header carries the *applied*
//! record, so serving on the strength of the core ships a header that does not list the peer
//! receiving it. The receiver writes the record, writes every byte of the region, and then
//! declines to start it — correctly, because a record that does not name this store is one it must
//! not serve. A retry that cost one heartbeat became a stall with no end.
//!
//! So the ask is **held** instead: the core's membership decides whether the caller is a stranger,
//! the record decides when it is served, and the wait is bounded.
//!
//! # What this model explores that the fix's test does not
//!
//! `tests/snapshot.rs` holds one ordering still — a voter added on a store that does not exist, so
//! the change can never commit — and asserts which refusal comes back. This enumerates the answer
//! table around it: every combination of "does the region exist here", "does the core know this
//! peer", "does the applied record know it", each with the timing class that tells the two
//! refusals apart. The timing is not decoration: "not a member, on sight" and "not applied here,
//! after the wait" are the pre-fix and post-fix answers to the *same* question, and a test that
//! reads only the identity of the refusal cannot tell a waited answer from an immediate one.
//!
//! # The ground truth is the header the snapshot would carry
//!
//! [`AskCase::expected`] is written from what a served snapshot would *contain* — a header naming
//! the applied record — rather than from what the sender checks. Serving a peer the record does
//! not name ships a copy of a range with no owner, and it does so whether the sender arrived there
//! by reading the record, the core, or a coin.

/// What the two membership views say about the peer asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Membership {
    /// Neither the core's configuration nor the applied record has heard of it. A stranger.
    Stranger,
    /// The core's configuration has it and the applied record does not: the gap between append
    /// and apply, which is where a new peer asks.
    InCoreOnly {
        /// Whether the change that put it there will ever commit. A change that cannot commit
        /// can still be rolled back, and a region shipped to a peer a rollback removes is a copy
        /// of a range with no owner.
        commits: bool,
    },
    /// The applied record names it. This is the only state in which a snapshot may be served.
    InRecord,
}

/// One question put to the snapshot sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskCase {
    /// A name a failure can print.
    pub name: &'static str,
    /// Whether this store hosts the region at all.
    pub region_hosted: bool,
    /// What the two views say about the asking peer.
    pub membership: Membership,
    /// Whether a real store can be driven into this state; see `sim_snapshot_ask.rs`.
    pub constructible: bool,
}

/// What the sender answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A snapshot stream was opened.
    Served,
    /// Refused, saying the peer is not a member.
    NotAMember,
    /// Refused, saying the change that placed the peer has not applied here.
    NotAppliedHere,
    /// Refused, saying this store does not host the region.
    NoSuchRegion,
    /// Anything else.
    Other(String),
}

/// What the sender did, and how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The answer.
    pub answer: Answer,
    /// Milliseconds between the ask and the answer.
    pub waited_ms: u64,
}

/// What the model requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Required {
    /// The answer that must come back.
    pub answer: RequiredAnswer,
    /// Whether the sender must have waited for the record before answering.
    pub waits: bool,
}

/// The answer the model requires, named rather than compared, so a failure says what it wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredAnswer {
    /// A snapshot stream, because the applied record names the peer.
    Served,
    /// Refused as a stranger, because neither view has the peer.
    NotAMember,
    /// Refused after the wait, because the core has the peer and the record never got it.
    NotAppliedHere,
    /// Refused because this store does not host the region.
    NoSuchRegion,
}

impl AskCase {
    /// What must come back, derived from **what a served snapshot would carry**.
    ///
    /// A snapshot's header is the applied record, so the only state in which serving is safe is
    /// `InRecord`. Everything else is a refusal, and which refusal it is says whether the sender
    /// treated a member as a stranger — the failure `1502d0f` is about.
    #[must_use]
    pub fn expected(&self) -> Required {
        if !self.region_hosted {
            return Required {
                answer: RequiredAnswer::NoSuchRegion,
                waits: false,
            };
        }
        match self.membership {
            Membership::Stranger => Required {
                answer: RequiredAnswer::NotAMember,
                // A stranger is refused on sight. Waiting for one would make every wrong ask cost
                // the bound, and there is nothing to wait for: no change is in flight for it.
                waits: false,
            },
            Membership::InCoreOnly { commits: true } => Required {
                answer: RequiredAnswer::Served,
                waits: true,
            },
            Membership::InCoreOnly { commits: false } => Required {
                answer: RequiredAnswer::NotAppliedHere,
                waits: true,
            },
            Membership::InRecord => Required {
                answer: RequiredAnswer::Served,
                waits: false,
            },
        }
    }
}

/// Which half of the rule broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    /// **A snapshot was served to a peer the applied record does not name.** A copy of a range
    /// with no owner: the receiver writes every byte and then declines to start it.
    ServedAStranger,
    /// A member in the gap was refused as if it were a stranger — the pre-`1502d0f` answer, and
    /// the one that strands the learner it was meant to place.
    RefusedAMember,
    /// The right answer, arrived at the wrong way: on sight when it should have waited, or after
    /// the bound when it should have been immediate.
    WrongTiming,
    /// Some other answer entirely.
    WrongAnswer,
}

/// A broken case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The case's name.
    pub case: &'static str,
    /// What the model required.
    pub required: Required,
    /// What came back.
    pub outcome: Outcome,
    /// Which half broke.
    pub half: Half,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "case {:?}: required {:?}, got {:?} ({:?}). ADR 0035: the core's membership decides \
             whether the caller is a stranger, the record decides when it is served",
            self.case, self.required, self.outcome, self.half
        )
    }
}

impl std::error::Error for Violation {}

/// Checks one answer.
///
/// `wait_floor_ms` is what "waited" means observably: a sender that answered faster than this did
/// not consult the record more than once, whatever it said.
///
/// # Errors
///
/// A [`Violation`] naming which half broke.
pub fn check(case: &AskCase, outcome: &Outcome, wait_floor_ms: u64) -> Result<(), Violation> {
    let required = case.expected();
    let fail = |half| {
        Err(Violation {
            case: case.name,
            required,
            outcome: outcome.clone(),
            half,
        })
    };

    // The safety half first, and on its own, because it is the one that costs data. Everything
    // below is about *which* refusal; this is about serving at all.
    if outcome.answer == Answer::Served && required.answer != RequiredAnswer::Served {
        return fail(Half::ServedAStranger);
    }

    let matched = matches!(
        (required.answer, &outcome.answer),
        (RequiredAnswer::Served, Answer::Served)
            | (RequiredAnswer::NotAMember, Answer::NotAMember)
            | (RequiredAnswer::NotAppliedHere, Answer::NotAppliedHere)
            | (RequiredAnswer::NoSuchRegion, Answer::NoSuchRegion)
    );
    if !matched {
        // The one worth naming separately: the sender said "not a member" about a peer the core
        // has. That is a correct sentence about the wrong membership.
        if required.answer == RequiredAnswer::NotAppliedHere && outcome.answer == Answer::NotAMember
        {
            return fail(Half::RefusedAMember);
        }
        return fail(Half::WrongAnswer);
    }

    if required.waits && outcome.waited_ms < wait_floor_ms {
        return fail(Half::WrongTiming);
    }
    if !required.waits && outcome.waited_ms >= wait_floor_ms {
        return fail(Half::WrongTiming);
    }
    Ok(())
}

/// Every combination of the two membership views, with a note on which a real store can be driven
/// into.
///
/// Two of the six are not constructible and are marked rather than dropped, because the reason is
/// the interesting part: the record is applied *from* the log, so the core can never know less
/// than the record, and a change that commits closes the gap in microseconds rather than holding
/// it open. `sim_snapshot_ask.rs` says the same at its skips.
#[must_use]
pub fn cases() -> Vec<AskCase> {
    vec![
        AskCase {
            name: "the applied record names the peer",
            region_hosted: true,
            membership: Membership::InRecord,
            constructible: true,
        },
        AskCase {
            name: "neither view has heard of the peer",
            region_hosted: true,
            membership: Membership::Stranger,
            constructible: true,
        },
        AskCase {
            name: "the core has the peer and the change can never commit",
            region_hosted: true,
            membership: Membership::InCoreOnly { commits: false },
            constructible: true,
        },
        AskCase {
            name: "this store does not host the region",
            region_hosted: false,
            membership: Membership::Stranger,
            constructible: true,
        },
        AskCase {
            name: "this store does not host the region, and the peer is in it elsewhere",
            region_hosted: false,
            membership: Membership::InRecord,
            constructible: true,
        },
        AskCase {
            // A learner's addition commits on the existing voters alone, so this state exists for
            // microseconds and cannot be held open from outside the store.
            name: "the core has the peer and the change commits",
            region_hosted: true,
            membership: Membership::InCoreOnly { commits: true },
            constructible: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{Answer, AskCase, Membership, Outcome, RequiredAnswer, cases, check};

    #[test]
    fn only_the_applied_record_permits_serving() {
        for case in cases() {
            let serves = case.expected().answer == RequiredAnswer::Served;
            let record_has_it = matches!(
                case.membership,
                Membership::InRecord | Membership::InCoreOnly { commits: true }
            ) && case.region_hosted;
            assert_eq!(
                serves, record_has_it,
                "case {:?} would serve a snapshot whose header does not name the peer receiving \
                 it, which is a copy of a range with no owner",
                case.name
            );
        }
    }

    #[test]
    fn refusing_a_member_as_a_stranger_is_named_as_its_own_failure() {
        // The pre-1502d0f answer, and the reason the checker distinguishes the two refusals: both
        // are "no", and only one of them strands the learner the change was placing.
        let case = AskCase {
            name: "the core has the peer and the change can never commit",
            region_hosted: true,
            membership: Membership::InCoreOnly { commits: false },
            constructible: true,
        };
        let violation = check(
            &case,
            &Outcome {
                answer: Answer::NotAMember,
                waited_ms: 0,
            },
            400,
        )
        .expect_err("reading only the record must be a violation");
        assert_eq!(violation.half, super::Half::RefusedAMember);
    }

    #[test]
    fn an_answer_that_did_not_wait_is_a_different_answer() {
        let case = AskCase {
            name: "the core has the peer and the change can never commit",
            region_hosted: true,
            membership: Membership::InCoreOnly { commits: false },
            constructible: true,
        };
        assert!(
            check(
                &case,
                &Outcome {
                    answer: Answer::NotAppliedHere,
                    waited_ms: 0,
                },
                400,
            )
            .is_err(),
            "the right words with no wait behind them is a sender that refused on sight and \
             happened to phrase it well"
        );
        assert!(
            check(
                &case,
                &Outcome {
                    answer: Answer::NotAppliedHere,
                    waited_ms: 480,
                },
                400,
            )
            .is_ok()
        );
    }
}
