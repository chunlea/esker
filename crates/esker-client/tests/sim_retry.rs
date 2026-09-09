//! The real retry loop, run through the model's scripts.
//!
//! `esker_sim::mech::retry` owns the scripts and the arithmetic that says what each one obliged;
//! this file owns the one thing that makes the arithmetic mean something, which is that the loop
//! under test is **the real `RawClient`** and not a transcription of it. Copy this file and
//! `crates/esker-sim/` into a detached worktree at `1502d0f` — `9791e16`'s parent — and the same
//! checker meets the budget as it was, counting every attempt.
//!
//! Everything runs over `FakeTransport` and `FakeClock`: no socket, no wall clock, and therefore a
//! two-second backoff that costs nothing to assert.
//!
//! `docs/plans/phase-11-engine.md` §10 holds the recorded reds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionResolver, Route, StaticRegion};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, Peer, ProtoError, RawKvResp, Region};
use esker_client::{ClientOptions, Error, RawClient};
use esker_sim::mech::retry::{
    Answer, Budget, RetryClient, Script, Verdict, Violation, run, run_endless_progress,
};

/// Seeds every scenario runs, the same list the placement model uses.
const SEEDS: [u64; 24] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584, 4181, 6765, 10946,
    17711, 28657, 46368, 75025,
];

/// Store `n` hosts peer `n * 10`.
fn three_peers() -> Vec<Peer> {
    vec![Peer::voter(1, 10), Peer::voter(2, 20), Peer::voter(3, 30)]
}

fn one_region() -> Arc<dyn RegionResolver> {
    let peers = three_peers();
    let leader = peers.first().copied();
    Arc::new(StaticRegion::new(Route {
        region: Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers,
            epoch: Epoch::INITIAL,
        },
        leader,
    }))
}

/// The real client, wearing the model's clothes.
///
/// The only decisions here are how to say "progress" and "nothing" on the wire; everything after
/// that is `RawClient::get`.
struct RealClient;

impl RealClient {
    /// A refusal that teaches strictly newer routing. `version` rises with every one, so the
    /// cache is left more correct than it was found — which is what makes it progress.
    fn progress(version: u64) -> Outcome {
        Outcome::Fail(ProtoError::EpochNotMatch {
            current_regions: vec![Region {
                id: 1,
                start_key: Bytes::new(),
                end_key: Bytes::new(),
                peers: three_peers(),
                epoch: Epoch::new(1, version),
            }],
        })
    }

    /// A refusal that teaches nothing. A leader hint moves no epoch, so the cache is exactly as
    /// correct after it as before — the loop the budget exists to stop.
    fn fruitless(hint: u64) -> Outcome {
        Outcome::Fail(ProtoError::NotLeader {
            region_id: 1,
            leader_hint: Some(hint * 10),
        })
    }
}

impl RetryClient for RealClient {
    fn budget(&self) -> Budget {
        Budget {
            max_fruitless: esker_client::retry::MAX_RETRIES,
            deadline_ms: esker_client::retry::CALL_TIMEOUT_MS,
        }
    }

    fn call(&self, script: &Script) -> Verdict {
        let transport = Arc::new(FakeTransport::new());
        let mut version = 2_u64;
        let mut hint = 2_u64;
        for answer in &script.answers {
            transport.script(Rule::new(
                Matcher::Any,
                outcome_for(*answer, &mut version, &mut hint),
            ));
        }
        match script.tail {
            // A `forever` rule replays one outcome, and one outcome cannot keep teaching a
            // *newer* epoch — the second time it arrives the client has already learned it, and
            // the run would stop being progress halfway through without saying so. So an endless
            // supply of newer epochs is scripted as a long run of distinct rules. Two thousand is
            // far more than any schedule reaches inside the deadline.
            Answer::Progress => {
                for step in 0..2_000 {
                    transport.script(Rule::new(Matcher::Any, Self::progress(version + step)));
                }
            }
            tail => {
                transport.script(
                    Rule::new(Matcher::Any, outcome_for(tail, &mut version, &mut hint)).forever(),
                );
            }
        }

        let clock = Arc::new(FakeClock::new());
        let client = RawClient::with_options(
            transport.clone(),
            one_region(),
            ClientOptions {
                // Fixed, so a failing seed replays with the same delays.
                jitter_seed: Some(0xE5E5),
                ..ClientOptions::default()
            },
        )
        .with_clock(clock.clone());

        verdict_of(client.get(b"k"), &transport, &clock)
    }
}

fn outcome_for(answer: Answer, version: &mut u64, hint: &mut u64) -> Outcome {
    match answer {
        Answer::Progress => {
            let outcome = RealClient::progress(*version);
            *version += 1;
            outcome
        }
        Answer::Fruitless => {
            let outcome = RealClient::fruitless(*hint);
            // Cycle the hint, so the client is sent somewhere new every time and the refusal is
            // not deduplicated into "the same answer again" by anything.
            *hint = if *hint == 3 { 1 } else { *hint + 1 };
            outcome
        }
        Answer::Answered => Outcome::Reply(RawKvResp::Get { value: None }),
    }
}

fn verdict_of(
    answer: Result<Option<Bytes>, Error>,
    transport: &FakeTransport,
    clock: &FakeClock,
) -> Verdict {
    let calls = u32::try_from(transport.call_count()).unwrap_or(u32::MAX);
    let elapsed_ms = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
    match answer {
        Ok(_) => Verdict::Answered { calls },
        Err(Error::RetriesExhausted { .. }) => Verdict::OutOfAttempts { calls, elapsed_ms },
        Err(Error::DeadlineExceeded { .. }) => Verdict::OutOfTime { calls, elapsed_ms },
        Err(other) => Verdict::Other {
            calls,
            what: other.to_string(),
        },
    }
}

#[test]
fn progress_never_spends_the_budget_and_no_progress_always_does() {
    let report = match run(&SEEDS, &RealClient) {
        Ok(report) => report,
        Err(violation) => panic!(
            "{violation}\n\nRerun with just this seed. 9791e16: the budget counts consecutive \
             attempts that taught this client nothing, and the deadline is what stops a region \
             whose epoch never settles."
        ),
    };
    println!("{} scripts: {report:?}", SEEDS.len());

    // A green run over scripts that all answered inside the old budget would prove nothing, so
    // the two shapes that matter are asserted to have happened. This file is the one that travels
    // to the pre-fix worktree, and it has to be able to say that for itself.
    assert!(
        report.answered_past_the_old_budget >= 8,
        "only {} script(s) obliged an answer past the attempt budget, so most of this run would \
         have passed before 9791e16 too: {report:?}",
        report.answered_past_the_old_budget
    );
    assert!(
        report.obliged_give_up >= 3,
        "only {} script(s) obliged the client to give up, so the other half of the rule — that a \
         store teaching nothing is still stopped — went untested: {report:?}",
        report.obliged_give_up
    );
    assert!(
        report.mixed >= 10,
        "only {} script(s) mixed both kinds of refusal. A budget that resets on progress and one \
         that never resets agree on every uniform script; the mixture is where they differ, and \
         it is what neither hand-written test contains: {report:?}",
        report.mixed
    );
}

#[test]
fn an_epoch_that_never_settles_ends_at_the_deadline() {
    // The half that makes the other one safe. "Progress does not spend the budget" would be a
    // client that never gives up if nothing else counted, so the deadline is pinned: the run must
    // end in `DeadlineExceeded` having actually spent the time, not in an attempt-budget failure
    // wearing a different name.
    let verdict = run_endless_progress(&RealClient)
        .unwrap_or_else(|violation: Violation| panic!("{violation}"));
    let Verdict::OutOfTime { calls, elapsed_ms } = verdict else {
        panic!("an endless supply of newer epochs must end at the deadline, got {verdict:?}");
    };
    println!("endless progress: {calls} calls, {elapsed_ms}ms");
    assert!(
        calls > esker_client::retry::MAX_RETRIES,
        "it stopped after {calls} calls, which is inside the attempt budget — so something other \
         than the deadline ended it"
    );
    assert!(
        elapsed_ms >= esker_client::retry::CALL_TIMEOUT_MS / 2,
        "it gave up after {elapsed_ms}ms of a {}ms deadline",
        esker_client::retry::CALL_TIMEOUT_MS
    );
}

/// **What a leaderless region costs a caller today**, printed rather than argued with.
///
/// The two answers the retry budget was designed around both *say something*: a newer epoch is
/// progress, and a leader hint is a place to try next. A region between leaders says neither —
/// `NotLeader` with **no hint**, at an epoch that is not moving — so the count treats it as a loop
/// to stop, and the count is what ends the call:
///
/// ```text
/// a leaderless region: 9 calls, 2266ms   —  of a 10,000 ms deadline the caller set
/// ```
///
/// **Twenty-three per cent of the time the caller allowed**, answered `gave up after 9 attempts`.
/// [ADR 0100](../../../docs/adr/0100-a-region-between-leaders-waits-on-the-callers-deadline.md)
/// asked whether that count should give way to the caller's deadline, and **the measurement said
/// no**: at 192–325 regions a region that loses its leader does not get one back inside thirty
/// seconds — fourteen sightings in four runs, none recovered — so spending the whole deadline
/// would turn a two-second failure into a ten-second one and not into a success. The count is a
/// real shape and it is not what stands between that load and an answer.
///
/// So this asserts **what the client does today** and prints the three numbers the ADR quotes. If
/// somebody changes the contract, this goes red and the ADR is where the argument is.
#[test]
fn what_a_leaderless_region_costs_a_caller() {
    let transport = Arc::new(FakeTransport::new());
    // No hint, and no new epoch: the honest answer of a peer that is not the leader and does not
    // know who is. A `forever` rule, because an election that is under way keeps saying this.
    transport.script(
        Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            }),
        )
        .forever(),
    );
    let clock = Arc::new(FakeClock::new());
    let client = RawClient::with_options(
        transport.clone(),
        one_region(),
        ClientOptions {
            jitter_seed: Some(0xE5E5),
            ..ClientOptions::default()
        },
    )
    .with_clock(clock.clone());

    let error = client
        .get(b"k")
        .expect_err("a leaderless region answers nothing else");
    let calls = transport.calls().len();
    let elapsed_ms = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
    println!(
        "a leaderless region: {calls} calls, {elapsed_ms}ms of a {}ms deadline",
        esker_client::retry::CALL_TIMEOUT_MS
    );

    assert!(
        matches!(error, Error::RetriesExhausted { .. }),
        "the count is what ends this call today, and the ADR's numbers are read off that: got \
         `{error}` after {elapsed_ms}ms and {calls} calls"
    );
    assert!(
        elapsed_ms < esker_client::retry::CALL_TIMEOUT_MS / 2,
        "it spent {elapsed_ms}ms of a {}ms deadline, which is no longer the shape the ADR \
         describes",
        esker_client::retry::CALL_TIMEOUT_MS
    );
}

#[test]
fn the_two_kinds_of_refusal_are_told_apart_on_the_wire() {
    // A guard on the binding rather than on the client, and it earns its place: if `fruitless`
    // accidentally taught a newer epoch, every "the client kept going" assertion above would pass
    // for the wrong reason and the model would be checking one rule twice.
    let client = RealClient;

    let stall = Script {
        answers: vec![Answer::Fruitless; 20],
        tail: Answer::Fruitless,
    };
    match client.call(&stall) {
        Verdict::OutOfAttempts { calls, .. } => assert_eq!(
            calls,
            esker_client::retry::MAX_RETRIES + 1,
            "a leader hint has to spend the budget; if it reset it, the budget would never stop \
             anything"
        ),
        other => panic!("a store teaching nothing must exhaust the budget, got {other:?}"),
    }

    // Ten refusals each teaching a newer epoch, then an answer: one more than the attempt budget
    // allows, and the shape 9791e16 is named for. Before that fix this stopped at call 9.
    let progress = Script {
        answers: vec![Answer::Progress; 10],
        tail: Answer::Answered,
    };
    assert_eq!(
        client.call(&progress),
        Verdict::Answered { calls: 11 },
        "ten refusals that each taught something must all be spent, and the eleventh call answered"
    );

    // **And the run does not go on for ever.** Twenty of them do not fit inside the call
    // deadline, and what stops them is the deadline — which is the half that makes "progress does
    // not spend the budget" safe. This is also why the model's drawn scripts are ten long: a
    // longer one would let the deadline end a run the model meant to end on attempts, and the
    // checker would have to accept two answers where it should accept one.
    let long = Script {
        answers: vec![Answer::Progress; 20],
        tail: Answer::Answered,
    };
    match client.call(&long) {
        Verdict::OutOfTime { calls, elapsed_ms } => {
            assert!(
                calls > esker_client::retry::MAX_RETRIES + 1,
                "it stopped inside the attempt budget after all: {calls} calls"
            );
            assert!(
                elapsed_ms >= esker_client::retry::CALL_TIMEOUT_MS / 2,
                "it claimed a deadline after only {elapsed_ms}ms"
            );
        }
        other => panic!(
            "twenty refusals cost more than the call deadline, so the deadline is what has to \
             stop them: got {other:?}"
        ),
    }
}

#[test]
fn the_backoff_still_counts_every_attempt() {
    // The conservative half of 9791e16, and the reason no existing budget test changed: the
    // *schedule* still counts total attempts, so a client making progress meets the same
    // exponential curve to the same two-second ceiling. Resetting the backoff on progress as
    // well would turn a splitting cluster into a hot loop against its own stores.
    let script = Script {
        answers: vec![Answer::Progress; 12],
        tail: Answer::Answered,
    };
    let transport = Arc::new(FakeTransport::new());
    let clock = Arc::new(FakeClock::new());
    let client = RawClient::with_options(
        transport.clone(),
        one_region(),
        ClientOptions {
            jitter_seed: Some(0xE5E5),
            ..ClientOptions::default()
        },
    )
    .with_clock(clock.clone());
    let mut version = 2;
    let mut hint = 2;
    for answer in &script.answers {
        transport.script(Rule::new(
            Matcher::Any,
            outcome_for(*answer, &mut version, &mut hint),
        ));
    }
    transport.script(
        Rule::new(
            Matcher::Any,
            outcome_for(script.tail, &mut version, &mut hint),
        )
        .forever(),
    );

    assert_eq!(client.get(b"k"), Ok(None));
    let spent = clock.elapsed();
    // Twelve waits on a 10 ms base doubling to a 2 s cap, minus jitter. Well past the point the
    // schedule would sit at if progress reset it — a reset schedule would never leave 10 ms.
    assert!(
        spent >= Duration::from_secs(2),
        "twelve refusals cost only {spent:?}; the backoff schedule is resetting on progress, \
         which would make a splitting cluster a hot loop"
    );
    assert!(
        spent < Duration::from_millis(esker_client::retry::CALL_TIMEOUT_MS),
        "the call did not finish inside its own deadline: {spent:?}"
    );
}
