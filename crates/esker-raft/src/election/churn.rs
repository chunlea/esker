//! **How long can a group go with nobody in office?** — `debts-v1.1.md` #107's last question.
//!
//! The incident behind #89/#107 was an `INSERT` that spent thirty seconds without reaching a
//! leader of region 1. Thirty seconds is **300 ticks** here (`crate::TICK_MS` is 100), while a
//! normal election finishes inside **one** timeout — `ELECTION_TIMEOUT_MIN_TICKS` to
//! `ELECTION_TIMEOUT_MAX_TICKS`, 10 to 20. So the question is whether a schedule can hold a group
//! vacant for fifteen to thirty consecutive election timeouts.
//!
//! # What is measured, and why it is `role` and not belief
//!
//! The statement that failed was a **write**. A write reaches `RawNode::propose`, and the store
//! turns its refusal into `NotLeader { leader_hint: None }` *unconditionally* —
//! `esker-store/src/peer.rs`'s `propose_error` never consults `node.leader()`. So for a write:
//! whoever is in office serves it, and everyone else answers `None` whatever they believe. Nine
//! refusals in a row therefore mean **nobody was in office**, which is `leaders().is_empty()` —
//! not "nobody had a leader belief", which is the *read* path's shape and would count a
//! partitioned former leader as a vacancy.
//!
//! # What counts as elapsed time
//!
//! Thirty seconds is wall clock, and wall clock advances for everyone at once. Only `tick_all`
//! counts. Ticking one node while its peers stand still is **skew**, not time: a run that ticked
//! one node three hundred times would report a three-hundred-tick vacancy that no clock ever saw.
//! Family C is the one that needs skew, and it bounds it at one election timeout and says so in
//! its own output.
//!
//! # These print rather than assert
//!
//! The assertion here is weak on purpose — the number is the product. A test that asserted "never
//! more than 300" would be green for as long as nothing came close, which is a guarantee about the
//! assertion rather than about the code; `869b2b7f` in this repository is the cautionary case, a
//! property whose three attempted mutations all stayed green. **Anything past 40 ticks is already
//! twice a normal election and worth reporting**, and the pre-registration
//! (`esker-coord/s2-107-churn-design.md` §4) says what each band means before the numbers exist.

use crate::message::Message;
use crate::testkit::Harness;
use crate::types::NodeId;

/// Thirty seconds, in ticks: the incident's span at `crate::TICK_MS` of 100.
const THIRTY_SECONDS: u64 = 300;

/// One election timeout at its widest (`crate::ELECTION_TIMEOUT_MAX_TICKS`), which is both the
/// bound on Family C's skew and the line past which a vacancy stops being ordinary.
const ONE_TIMEOUT: u64 = 20;

/// How long the group went with nobody in office, in whole-group ticks.
#[derive(Default)]
struct Vacancy {
    longest: u64,
    current: u64,
    ticks: u64,
    vacant_ticks: u64,
}

impl Vacancy {
    /// Records one whole-group tick.
    ///
    /// **The longest run and the total are different questions** and both are kept: a group that
    /// changes leader every other tick is vacant half the time and never for long, which is
    /// churn without a stall. The incident needs the *run*.
    fn observe(&mut self, vacant: bool) {
        self.ticks += 1;
        if vacant {
            self.vacant_ticks += 1;
            self.current += 1;
            self.longest = self.longest.max(self.current);
        } else {
            self.current = 0;
        }
    }

    fn report(&self, family: &str, note: &str) {
        // Integer percent: a share of ticks needs no float, and `clippy::cast_precision_loss` is
        // right that a `u64 as f64` is a claim about magnitudes this has no reason to make.
        let share = self.vacant_ticks * 100 / self.ticks.max(1);
        println!(
            "{family}: longest vacancy {} ticks ({} ms), vacant {}% of {} ticks — {note}",
            self.longest,
            self.longest * crate::TICK_MS,
            share,
            self.ticks,
        );
    }
}

/// One whole-group tick, with the intervention applied on **every** delivery round.
///
/// **Two orderings were wrong before this one, and each made a family measure nothing.** Dropping
/// before the tick loses the previous round's traffic and lets the new round through — Family B
/// looked like it was cutting heartbeats while every heartbeat arrived. Dropping once after the
/// tick is no better for anything generated *in reply*: a `RequestVoteResponse` exists only once
/// the `RequestVote` has been delivered, so Family A dropped **zero** vote replies across 300
/// rounds while reporting a tidy vacancy of zero. The predicate has to run each round of the
/// settle, which is why this does not call [`Harness::settle`].
///
/// **The round bound reports instead of panicking.** `Harness::settle` panics at 64 rounds, and a
/// group that will not settle is a fact about the fixture's capacity rather than an answer about
/// churn (`esker-coord/s2-107-churn-design.md` §4, and the coordinator's ruling of 2026-09-17).
fn tick_with(
    group: &mut Harness,
    mut intervene: impl FnMut(&mut Harness) -> u64,
) -> (bool, u64, bool) {
    group.tick_all();
    let mut dropped = 0;
    let mut settled = false;
    for _ in 0..64 {
        group.drain_ready();
        dropped += intervene(group);
        if group.pending() == 0 {
            settled = true;
            break;
        }
        while group.pending() > 0 {
            group.deliver_one(0);
        }
    }
    (group.leaders().is_empty(), dropped, settled)
}

/// Nothing is dropped; the shape the families that only bend time need.
fn nothing(_: &mut Harness) -> u64 {
    0
}

/// Makes `id` stand for election unless it already holds office.
///
/// `Raft::become_candidate` carries `debug_assert_ne!(role, Leader, "a leader does not campaign")`
/// and tests are debug builds, so an unguarded `campaign` turns "a leader emerged" into a panic
/// that looks like a defect and is not one.
fn campaign_unless_leading(group: &mut Harness, id: NodeId) -> bool {
    if group.leaders().iter().any(|(who, _)| *who == id) {
        return false;
    }
    group.campaign(id);
    true
}

/// Drops every queued message the predicate picks.
///
/// An index walk rather than a filter because `Harness` owns the queue and offers `drop_one` by
/// index; `at` only advances when the message at it survives.
fn drop_matching(group: &mut Harness, mut doomed: impl FnMut(&Message) -> bool) -> u64 {
    let mut at = 0;
    let mut dropped = 0;
    while at < group.pending() {
        if doomed(&group.pending_messages()[at]) {
            group.drop_one(at);
            dropped += 1;
        } else {
            at += 1;
        }
    }
    dropped
}

/// **Family A — a split vote, over and over.** Two candidates in the same term, the third peer's
/// replies lost, so neither reaches a majority and both must time out again.
///
/// The randomised timeout is the defence being tested: `ELECTION_TIMEOUT_MAX_TICKS`'s own doc says
/// *"the spread is what stops two followers from campaigning in lockstep forever"*. Pre-vote is on
/// by default, which should blunt this further — how much is the number below.
#[test]
fn family_a_a_repeated_split_vote() {
    let ids: Vec<NodeId> = vec![1, 2, 3];
    let mut group = Harness::new(&ids, 0x00C0_FFEE);
    let mut vacancy = Vacancy::default();
    let (mut campaigned, mut dropped, mut unsettled) = (0_u64, 0_u64, 0_u64);

    for round in 0..THIRTY_SECONDS {
        if round % 10 == 0 {
            campaigned += u64::from(campaign_unless_leading(&mut group, 1));
            campaigned += u64::from(campaign_unless_leading(&mut group, 2));
        }
        let (vacant, lost, settled) = tick_with(&mut group, |group| {
            drop_matching(group, |message| {
                matches!(message, Message::RequestVoteResponse { from: 3, .. })
            })
        });
        dropped += lost;
        unsettled += u64::from(!settled);
        vacancy.observe(vacant);
    }

    // **A family that intervened in nothing has measured nothing.** The first version of this
    // dropped zero vote replies across all 300 rounds and reported a vacancy of zero, which read
    // like an answer and was a fact about the probe — the shape `s1-unit-q/q107-probe.log`
    // recorded when a load produced no refusals at all. The counter is here so that cannot recur.
    println!(
        "  family A fired: {campaigned} campaigns, {dropped} vote replies dropped, {unsettled} rounds hit the bound"
    );
    vacancy.report(
        "family A (split vote)",
        "two candidates, the third peer's votes lost",
    );
    assert!(
        vacancy.longest <= THIRTY_SECONDS,
        "{} ticks",
        vacancy.longest
    );
}

/// **Family B — the leader's heartbeats never land.** A leader takes office and every
/// `AppendEntries` it sends is lost, so its followers time out and depose it; the next leader is
/// treated the same way.
///
/// The prediction on record is that this produces *frequent changes of leader* rather than a long
/// vacancy — each gap should be about one timeout. The total and the longest run are reported
/// separately precisely so that prediction can be checked rather than assumed.
#[test]
fn family_b_heartbeats_that_never_land() {
    let ids: Vec<NodeId> = vec![1, 2, 3];
    let mut group = Harness::new(&ids, 0x5EED);
    let mut vacancy = Vacancy::default();
    let (mut dropped, mut unsettled) = (0_u64, 0_u64);

    group.campaign(1);
    group.settle();

    for _ in 0..THIRTY_SECONDS {
        let (vacant, lost, settled) = tick_with(&mut group, |group| {
            drop_matching(group, |message| {
                matches!(message, Message::AppendEntries { .. })
            })
        });
        dropped += lost;
        unsettled += u64::from(!settled);
        vacancy.observe(vacant);
    }

    println!("  family B fired: {dropped} AppendEntries dropped, {unsettled} rounds hit the bound");
    vacancy.report("family B (lost heartbeats)", "every AppendEntries dropped");
    assert!(
        vacancy.longest <= THIRTY_SECONDS,
        "{} ticks",
        vacancy.longest
    );
}

/// **Family C — timers pulled out of step.** The one family that attacks the spread from the side:
/// if the peers' election timers are kept apart, someone always times out first, raises the term,
/// and scatters a majority that was about to form.
///
/// **The skew is bounded at one timeout and reported**, because skew is not time. Ticking one node
/// while its peers stand still would produce a vacancy no wall clock ever saw
/// (`s2-107-churn-design.md` §2).
#[test]
fn family_c_timers_pulled_out_of_step() {
    let ids: Vec<NodeId> = vec![1, 2, 3];
    let mut group = Harness::new(&ids, 0xBEEF);
    let mut vacancy = Vacancy::default();
    let mut skew = [0_u64; 3];
    let mut unsettled = 0_u64;

    for round in 0..THIRTY_SECONDS {
        let at = usize::try_from(round % 3).expect("three peers");
        if skew[at] < ONE_TIMEOUT {
            group.tick(ids[at], 1);
            skew[at] += 1;
        }
        let (vacant, _, settled) = tick_with(&mut group, nothing);
        unsettled += u64::from(!settled);
        vacancy.observe(vacant);
    }

    println!(
        "  family C fired: skew {skew:?} ticks (bounded at {ONE_TIMEOUT} each), {unsettled} rounds hit the bound"
    );
    vacancy.report(
        "family C (skewed timers)",
        "one extra tick per node, rotating",
    );
    assert!(
        vacancy.longest <= THIRTY_SECONDS,
        "{} ticks",
        vacancy.longest
    );
}

/// **Family D — duplicated vote traffic, against Family B as its control.**
///
/// A vote is idempotent, so duplicating one should change nothing; the point of the family is the
/// case where it *does*, because that would not be a churn finding but a hole in the idempotence,
/// which matters more than this whole unit.
///
/// **It is Family B plus duplication, and that is the fix for its first version.** Written alone
/// it campaigned once in three hundred rounds — a leader took office at the start and
/// `campaign_unless_leading` correctly refused to disturb it, so there were two vote messages in
/// the whole run and nothing to duplicate. Elections have to keep happening for duplication to
/// have anything to act on, and dropping the heartbeats is what makes them. The number to compare
/// is Family B's: same schedule, same seed, one difference.
///
/// **And it is not one difference, which the numbers say out loud.** Family B drops 262
/// `AppendEntries` over its run and this one drops 466, because duplicating vote traffic adds
/// delivery rounds and every round is another chance to drop an append. So a longer vacancy here
/// is **not** evidence that duplication lengthens an election: the two runs differ in the thing
/// being tested *and* in how much else was lost. Reading the gap as a hole in vote idempotence
/// would be reading a two-variable experiment as a one-variable one. To make it an A/B, the drop
/// count would have to be held equal — drop a fixed budget per tick rather than everything each
/// round — and that is a different unit, not a tweak to this one.
#[test]
fn family_d_duplicated_votes() {
    let ids: Vec<NodeId> = vec![1, 2, 3];
    let mut group = Harness::new(&ids, 0x5EED);
    let mut vacancy = Vacancy::default();
    let (mut dropped, mut duplicated, mut unsettled) = (0_u64, 0_u64, 0_u64);

    group.campaign(1);
    group.settle();

    for _ in 0..THIRTY_SECONDS {
        let (vacant, lost, settled) = tick_with(&mut group, |group| {
            // Duplicate first, drop second: a copy of a message this round is about to lose is
            // not a duplicate, and the two orders measure different things.
            let votes: Vec<usize> = group
                .pending_messages()
                .iter()
                .enumerate()
                .filter(|(_, message)| {
                    matches!(
                        message,
                        Message::RequestVote { .. } | Message::RequestVoteResponse { .. }
                    )
                })
                .map(|(at, _)| at)
                .collect();
            let copies = votes.len() as u64;
            for at in votes.into_iter().rev() {
                group.duplicate_one(at);
            }
            duplicated += copies;
            copies
                + drop_matching(group, |message| {
                    matches!(message, Message::AppendEntries { .. })
                })
        });
        dropped += lost;
        unsettled += u64::from(!settled);
        vacancy.observe(vacant);
    }

    println!(
        "  family D fired: {duplicated} vote messages duplicated, {dropped} AppendEntries dropped, {unsettled} rounds hit the bound"
    );
    vacancy.report(
        "family D (duplicated votes)",
        "family B's schedule and seed, plus every vote message delivered twice",
    );
    assert!(
        vacancy.longest <= THIRTY_SECONDS,
        "{} ticks",
        vacancy.longest
    );
}
