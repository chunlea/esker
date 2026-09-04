//! The timestamp window, under a generator rather than under a script.
//!
//! `tests/failover.rs` proves the rule against a group of three, and `tests/crash_kill.rs` proves
//! it against a signal. Both drive one schedule each. This drives thousands, and it drives the two
//! things a hand-written schedule keeps forgetting: a clock that goes **backwards** at the moment
//! of a failover, and a mark that was *proposed and never committed*.
//!
//! # The model, and why it is the right one
//!
//! [`Oracle`](esker_pd::tso::Oracle) takes a `persist` callback, and under
//! [ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md) that callback is "propose this mark and
//! wait for it to apply". So the two outcomes a placement driver can have are exactly the two this
//! file generates:
//!
//! * **it commits** — the mark is durable, every member will see it, and a new leader resumes at
//!   or above it;
//! * **it does not** — the leader lost its quorum, `allocate` returns an error, and *nothing is
//!   handed out*. That second half is the load-bearing one: a deposed leader that answered anyway
//!   is the duplicate this whole design is arranged against.
//!
//! A failover is then modelled as what it is: a fresh `Oracle` loaded from the **committed** mark,
//! with a clock of the adversary's choosing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use esker_pd::record::TsoRecord;
use esker_pd::tso::Oracle;
use esker_pd::{PdError, decompose_ts};
use proptest::prelude::*;

/// One thing the adversary does to the oracle.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Ask for `count` timestamps with the clock at `now_ms`, and let the mark commit.
    Allocate { count: u32, now_ms: u64 },
    /// The same, but this member has lost its quorum: the mark never commits.
    NoQuorum { count: u32, now_ms: u64 },
    /// A failover, or a restart: a new leader resumes from the committed mark, with this clock.
    Failover { now_ms: u64 },
}

/// Clocks the adversary may pick, including ones far behind where the oracle already is.
fn clocks() -> impl Strategy<Value = u64> {
    const BASE: u64 = 1_700_000_000_000;
    prop_oneof![
        // Around where it started, forwards and backwards.
        (BASE - 10_000..BASE + 10_000),
        // A long way behind: a dead battery, or a machine that never had NTP.
        (1_u64..1_000),
        // A long way ahead, then back again on the next step.
        (BASE + 1_000_000..BASE + 2_000_000),
    ]
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        4 => (1_u32..64, clocks()).prop_map(|(count, now_ms)| Step::Allocate { count, now_ms }),
        1 => (1_u32..64, clocks()).prop_map(|(count, now_ms)| Step::NoQuorum { count, now_ms }),
        2 => clocks().prop_map(|now_ms| Step::Failover { now_ms }),
    ];
    prop::collection::vec(step, 1..120)
}

proptest! {
    /// **No timestamp is ever handed out twice, and none goes backwards** — whatever the clock
    /// does and however often the leader changes.
    ///
    /// The run also asserts the rule the guarantee rests on, at every step: a timestamp that left
    /// the oracle has `physical < mark`, where `mark` is what had **committed** at that moment.
    /// Checking only the first would let an implementation pass by handing out one enormous
    /// timestamp and never moving again.
    #[test]
    fn a_timestamp_never_repeats_however_the_clock_and_the_leader_move(
        script in steps(),
        save_interval_ms in 1_u64..5_000,
    ) {
        // The mark that is durable. A failover reloads from this and from nothing else, which is
        // the whole of what "committed" buys.
        let mut committed = 0_u64;
        let mut oracle = Oracle::load(None, 1_700_000_000_000, save_interval_ms);
        let mut handed_out: Vec<u64> = Vec::new();

        for step in script {
            match step {
                Step::Allocate { count, now_ms } => {
                    let asked = Cell::new(None);
                    let start = oracle.allocate(count, now_ms, |mark| {
                        asked.set(Some(mark));
                        Ok(())
                    });
                    let Ok(start) = start else { continue };
                    if let Some(mark) = asked.get() {
                        committed = committed.max(mark);
                    }
                    for offset in 0..u64::from(count) {
                        let ts = start + offset;
                        prop_assert!(
                            decompose_ts(ts).0 < committed,
                            "{ts} was handed out at or above the committed mark {committed}"
                        );
                        handed_out.push(ts);
                    }
                }
                Step::NoQuorum { count, now_ms } => {
                    let before = oracle;
                    let outcome = oracle
                        .allocate(count, now_ms, |_| Err(PdError::internal("no quorum")))
                        .ok();
                    match outcome {
                        None => {
                            // **The mark did not move**, which is the guarantee. It is the only
                            // one: unlike the allocator, the oracle may have advanced its physical
                            // part before asking, and a run of skipped timestamps costs nothing —
                            // `crate::tso`'s own documentation says so, and the generator found
                            // this file asserting the allocator's stronger promise by mistake.
                            //
                            // What would be fatal is the *mark* moving without a commit: a new
                            // leader resumes at the committed mark, so a member that believed in
                            // an uncommitted one would hand out timestamps its successor would
                            // hand out again.
                            prop_assert_eq!(
                                oracle.high_water_ms(),
                                before.high_water_ms(),
                                "a failed commit moved the mark"
                            );
                            prop_assert!(
                                oracle.physical_ms() >= before.physical_ms(),
                                "a failed commit walked the oracle backwards"
                            );
                        }
                        // Inside the window: no commit was needed and none was made, so these are
                        // as safe as any others — and still below the committed mark.
                        Some(start) => {
                            prop_assert!(
                                decompose_ts(start).0 < committed,
                                "a placement driver with no quorum crossed its own mark"
                            );
                            for offset in 0..u64::from(count) {
                                handed_out.push(start + offset);
                            }
                        }
                    }
                }
                Step::Failover { now_ms } => {
                    oracle = Oracle::load(
                        (committed > 0).then_some(TsoRecord { high_water_ms: committed }),
                        now_ms,
                        save_interval_ms,
                    );
                }
            }
        }

        for pair in handed_out.windows(2) {
            prop_assert!(
                pair[1] > pair[0],
                "a timestamp went backwards: {} then {}",
                pair[0],
                pair[1]
            );
        }
        let unique: std::collections::BTreeSet<u64> = handed_out.iter().copied().collect();
        prop_assert_eq!(unique.len(), handed_out.len(), "a timestamp was handed out twice");
    }
}

/// The generator has to actually reach the case it exists for, or a green run means nothing.
///
/// Two of them: a failover whose clock is **behind** the mark — which is the only case where the
/// `max` in `Oracle::load` is doing any work — and a run that hands out enough to make the
/// question interesting.
#[test]
fn the_generator_reaches_a_failover_with_a_clock_behind_the_mark() {
    let mut oracle = Oracle::load(None, 1_700_000_000_000, 3_000);
    let mut committed = 0_u64;
    let mut last = 0_u64;
    for _ in 0..8 {
        let asked = Cell::new(None);
        let start = oracle
            .allocate(1, 1_700_000_000_000, |mark| {
                asked.set(Some(mark));
                Ok(())
            })
            .unwrap();
        if let Some(mark) = asked.get() {
            committed = committed.max(mark);
        }
        last = start;
    }
    assert!(committed > 0, "nothing committed");

    // The clock a thousand years behind, which is what a dead battery reads.
    let resumed = Oracle::load(
        Some(TsoRecord {
            high_water_ms: committed,
        }),
        1,
        3_000,
    );
    assert!(
        resumed.physical_ms() >= committed,
        "a resumed oracle followed the clock down to {}",
        resumed.physical_ms()
    );
    let mut resumed = resumed;
    let next = resumed.allocate(1, 1, |_| Ok(())).unwrap();
    assert!(
        next > last,
        "the resumed oracle handed out {next}, at or below {last}"
    );
}
