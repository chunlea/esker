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
use esker_client::wire::{Epoch, Peer, ProtoError, Region};
use esker_client::{ClientOptions, Error, RawClient};

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

fn harness() -> Harness {
    let transport = Arc::new(FakeTransport::new());
    transport.script(names(P.0, Q.1)).script(names(Q.0, P.1));
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

/// **Nine attempts, alternating between two live peers, and the third never asked.**
///
/// The sequence is the assertion, not just the error: a fake scripted wrongly could refuse
/// everything and produce the same `RetriesExhausted` without any ping-pong at all.
#[test]
fn two_peers_naming_each_other_spend_the_whole_budget() {
    let harness = harness();

    let error = harness.client.get(b"k").unwrap_err();
    let Error::RetriesExhausted { attempts, source } = &error else {
        panic!("the budget is what gives up here, not the deadline: {error:?}");
    };
    assert_eq!(*attempts, 9, "one attempt plus MAX_RETRIES: {error:?}");
    assert!(
        source.to_string().contains("not the leader of region 1"),
        "the incident's own sentence: {source}"
    );

    assert_eq!(
        stores(&harness.transport),
        vec![P.0, Q.0, P.0, Q.0, P.0, Q.0, P.0, Q.0, P.0],
        "the cached leader is sent to unconditionally, so the call ping-pongs"
    );
}

/// **The third peer is never asked**, which is the half that says this is not a rotation.
///
/// Stated as its own test rather than folded into the sequence above: if `target_at_skipping` ever
/// starts rotating past a known leader, this is the assertion that should say so, and a reader
/// should not have to work out which element of a nine-long vector carried that meaning.
#[test]
fn a_known_leader_is_never_rotated_past() {
    let harness = harness();
    let _ = harness.client.get(b"k");

    assert!(
        !stores(&harness.transport).contains(&R.0),
        "a client that rotated would reach the third peer within three attempts: {:?}",
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
    assert_eq!(epochs.len(), 9, "every attempt carried a header");
    assert!(
        epochs.iter().all(|epoch| *epoch == Epoch::INITIAL),
        "a NotLeader hint moves no epoch: {epochs:?}"
    );
}

/// **The fix's own test, and it is red today** — a call should not spend its whole budget on two
/// peers that have each already refused it.
///
/// `#[ignore]` everywhere else in this crate means *slow*, so the reason string says what this one
/// is instead: it is a claim about behaviour that does not exist yet. Deleting the ignore is how
/// the next window starts, and the assertion below is what it has to turn green.
///
/// **Why the third peer is the observable.** The client cannot tell a stale hint from a fresh one —
/// a follower naming its believed leader is answering honestly — so the fix is not "distrust the
/// hint". It is that a hint pointing back at a peer this **call** has already heard refuse is not
/// news, and a call with no news left should go back to asking the peers in turn, which is what it
/// already does when the leader is unknown (`region_cache.rs:104`). `R` is then reached, and `R` is
/// the only one of the three that could be leading.
#[test]
#[ignore = "#107: the client has no cycle guard for NotLeader hints; this is next window's red"]
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

/// **The second call is no better off than the first**, which is the thirty seconds.
///
/// A call resets `fruitless` and `corpses`, but the bad leader lives in the `RegionCache` on the
/// `Router` (`region_cache.rs:312`) and no call clears it. That is the only piece of this state
/// that outlives a call, and it is what turns one budget into a wall clock: six of these fill the
/// thirty seconds the incident's `settle` spent.
#[test]
fn the_bad_leader_outlives_the_call_that_learned_it() {
    let harness = harness();

    let first = harness.client.get(b"k").unwrap_err();
    assert!(matches!(first, Error::RetriesExhausted { attempts: 9, .. }));

    harness.transport.clear_log();
    let second = harness.client.get(b"k").unwrap_err();

    let Error::RetriesExhausted { attempts, .. } = &second else {
        panic!("the second call gives up the same way: {second:?}");
    };
    assert_eq!(
        *attempts, 9,
        "a fresh call, the same cached lie: {second:?}"
    );

    let visited = stores(&harness.transport);
    assert_eq!(visited.len(), 9, "{visited:?}");
    assert!(
        visited.iter().all(|store| *store == P.0 || *store == Q.0),
        "still only the two that name each other: {visited:?}"
    );
    assert!(
        visited.windows(2).all(|pair| pair[0] != pair[1]),
        "still alternating: {visited:?}"
    );
}
