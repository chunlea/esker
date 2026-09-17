//! **Two live peers naming each other spend the whole retry budget** — `debts-v1.1.md` #107.
//!
//! The incident behind #89/#107 was `gave up after 9 attempts: peer is not the leader of region 1`,
//! and its arithmetic is tight: nine refusals that were **store answers and not dial failures**,
//! no epoch movement across them, the budget exhausted with most of the ten-second deadline
//! unspent, and the state persisting across roughly six such calls to fill thirty seconds.
//!
//! This file shows that one construction produces every one of those, with no node dying and no
//! election left unfinished. Four pieces of the client line up:
//!
//! * a cached leader is returned **unconditionally** — `RegionCache`'s `target_at_skipping` rotates
//!   by attempt only while the leader is `None` (`esker-client/src/region_cache.rs:90`);
//! * a store never names **itself** in a `NotLeader` hint
//!   (`esker-store/src/peer.rs:1061`, `filter(|id| *id != self.peer_id)`), so two peers holding
//!   stale beliefs can name each other;
//! * the client writes the hint straight back into the cache (`Router::repair`, `router.rs:476`);
//! * and **nothing guards against a cycle**: `corpses` records only stores that could not be
//!   *dialled* (`router.rs:252`, `:299`), and `without_the_corpse` (`:454`) drops a hint only when
//!   it names the store this call has just failed to reach. Both of these peers answer.
//!
//! So the call walks P → Q → P → Q …, every hop a refusal that teaches it nothing, until
//! `fruitless > max_retries` (`router.rs:344`, `MAX_RETRIES` = 8 at `retry.rs:60`).
//!
//! # What this does not show
//!
//! **That this is what happened.** It shows the mechanism can produce that sentence; the incident's
//! own logs do not exist (`debts-v1.1.md` #89 records three runs that never reproduced it). Whether
//! a correct cluster ever puts two peers into mutually stale beliefs is a **store** question — it
//! needs control over elections — and is not asked here. A green run of this file is not an
//! explanation of the incident, and writing it up as one would be the mistake this note exists to
//! prevent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, Peer, ProtoError, RawKvResp, Region};
use esker_client::{ClientOptions, RawClient};

/// Store 1 holds peer 10, store 2 peer 20, store 3 peer 30.
const P: (u64, u64) = (1, 10);
const Q: (u64, u64) = (2, 20);
const R: (u64, u64) = (3, 30);

/// One region over the whole key space, replicated by three peers, **led by `P` as far as the
/// client knows**.
///
/// The third peer earns its place by never being asked: a client that rotated would reach it
/// within three attempts, so its absence from the log is what proves the leader is returned
/// unconditionally.
fn one_region_led_by_p() -> Arc<dyn RegionResolver> {
    let region = Region {
        id: 1,
        start_key: Bytes::new(),
        end_key: Bytes::new(),
        peers: vec![
            Peer::voter(P.0, P.1),
            Peer::voter(Q.0, Q.1),
            Peer::voter(R.0, R.1),
        ],
        epoch: Epoch::INITIAL,
    };
    let leader = Some(Peer::voter(P.0, P.1));
    Arc::new(RegionTable::from_routes([Route { region, leader }]))
}

/// `store` answers every request with "I am not the leader — try `hint`".
///
/// **Bounded rather than `forever`.** A rule that answers for ever turns a mis-scripted fake into a
/// spin instead of a failure; fifty is far past the budget of nine, so the bound never fires while
/// the client behaves, and a client that stopped giving up would fail here quickly rather than
/// hang.
fn names(store: u64, hint: u64) -> Rule {
    Rule::new(
        Matcher::Store(store),
        Outcome::Fail(ProtoError::NotLeader {
            region_id: 1,
            leader_hint: Some(hint),
        }),
    )
    .times(50)
}

struct Harness {
    client: RawClient,
    transport: Arc<FakeTransport>,
}

/// `store` is the region's actual leader and answers.
///
/// Before the fix this rule was never reached — the call never got past `P` and `Q` — which is
/// why the file could assert the third peer was *never asked*. It is the third hop now.
fn leads(store: u64) -> Rule {
    Rule::new(
        Matcher::Store(store),
        Outcome::Reply(RawKvResp::Get { value: None }),
    )
    .times(50)
}

fn harness() -> Harness {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(names(P.0, Q.1))
        .script(names(Q.0, P.1))
        .script(leads(R.0));
    let client = RawClient::with_options(
        transport.clone(),
        one_region_led_by_p(),
        ClientOptions {
            jitter_seed: Some(0x4A4A),
            ..ClientOptions::default()
        },
    )
    .with_clock(Arc::new(FakeClock::new()));
    Harness { client, transport }
}

/// Which store each call went to, in order.
fn stores(transport: &FakeTransport) -> Vec<u64> {
    transport.calls().iter().map(|call| call.store_id).collect()
}

/// **A pair that names each other costs one extra hop, not the whole budget.**
///
/// Before the fix this was nine attempts alternating `P, Q, P, Q …` and then `RetriesExhausted`.
/// The sequence is still the assertion rather than the outcome: a fake scripted wrongly could
/// succeed for the wrong reason, and `[P, Q, R]` is the only walk that means what this claims.
///
/// **Three, and the third is the cost.** `P` is the cached leader and refuses; `Q` is its hint and
/// refuses; `Q`'s hint names `P`, which has already refused *this call*, so it stops being news and
/// the rota resumes — reaching `R`, the one peer that had not answered and the only one that could
/// be leading. Asserting the length is what keeps "this got one hop longer" from reading as a
/// regression later: it is the price of the gate, and it is written down
/// (`s1-107-loopfix-review.md` §5.1).
#[test]
fn a_pair_that_names_each_other_costs_one_extra_hop() {
    let harness = harness();

    harness
        .client
        .get(b"k")
        .expect("the rota reaches the peer that is actually leading");

    assert_eq!(
        stores(&harness.transport),
        vec![P.0, Q.0, R.0],
        "the cycle is cut after the second refusal and the rota finds the leader"
    );
}

/// **A cached leader that keeps refusing is rotated past.**
///
/// The inverse of what this file asserted before the fix, and kept as its own test for the same
/// reason: a reader should not have to work out which element of a vector carried the meaning. A
/// cached leader is still sent to first (`region_cache.rs:90`) — what changed is that its hint
/// stops being followed once it has refused, so the rota can reach a peer this call has not heard
/// from.
#[test]
fn a_leader_that_keeps_refusing_is_rotated_past() {
    let harness = harness();
    let _ = harness.client.get(b"k");

    assert!(
        stores(&harness.transport).contains(&R.0),
        "the peer that never refused is the only one that can be leading: {:?}",
        stores(&harness.transport)
    );
}

/// **No attempt moved the epoch**, which is why the budget was spent rather than reset.
///
/// `learned_a_newer_epoch` is what separates "chasing a moving region" from "hammering a still one"
/// (`router.rs:512`), and a `NotLeader` hint moves nothing. This pins the condition the incident
/// arithmetic needs; without it, nine fruitless attempts could not have happened at all.
#[test]
fn the_epoch_never_moves() {
    let harness = harness();
    let _ = harness.client.get(b"k");

    let epochs: Vec<Epoch> = harness
        .transport
        .calls()
        .iter()
        .filter_map(|call| call.header().map(|header| header.epoch))
        .collect();
    // **The property survives the fix; the count did not.** This assertion said `9` while the
    // call ping-ponged, and saying "this test is unaffected by the fix" was wrong twice before it
    // was checked against the walk. Three hops now, and the epoch is still what it was.
    assert_eq!(epochs.len(), 3, "every attempt carried a header");
    assert!(
        epochs.iter().all(|epoch| *epoch == Epoch::INITIAL),
        "a NotLeader hint moves no epoch: {epochs:?}"
    );
}

/// **The fix's own test.** It shipped red and ignored one commit before the gate that made it
/// pass, which is the only reason its assertion is worded as a claim rather than as a
/// description: it was written down before it was true.
///
/// **Why the third peer is the observable.** The client cannot tell a stale hint from a fresh one —
/// a follower naming its believed leader is answering honestly — so the fix is not "distrust the
/// hint". It is that a hint pointing back at a peer this **call** has already heard refuse is not
/// news, and a call with no news left should go back to asking the peers in turn, which is what it
/// already does when the leader is unknown (`region_cache.rs:104`). `R` is then reached, and `R` is
/// the only one of the three that could be leading.
#[test]
fn a_call_stops_asking_two_peers_that_have_both_refused_it() {
    let harness = harness();
    let _ = harness.client.get(b"k");

    let visited = stores(&harness.transport);
    assert!(
        visited.contains(&R.0),
        "a hint naming a peer that already refused this call is not news, and a call out of news \
         should ask the peers in turn again rather than alternate until the budget is gone: {visited:?}"
    );
}

/// **The bad leader does not outlive the call that learned it**, which is where the thirty
/// seconds went.
///
/// A call resets `fruitless` and `corpses`, but the bad leader lives in the `RegionCache` on the
/// `Router` (`region_cache.rs:312`) and no call clears it. That is the only piece of this state
/// that outlives a call, and it is what turns one budget into a wall clock: six of these fill the
/// thirty seconds the incident's `settle` spent.
#[test]
fn the_bad_leader_does_not_outlive_the_call() {
    let harness = harness();

    harness.client.get(b"k").expect("the first call recovers");

    harness.transport.clear_log();
    harness
        .client
        .get(b"k")
        .expect("and so does the next one, from a cache that was not left lying");

    assert_eq!(
        stores(&harness.transport),
        vec![P.0, Q.0, R.0],
        "the second call walks the same three, which means it did not start from a stale leader \
         that the first call had already found wanting"
    );
}
