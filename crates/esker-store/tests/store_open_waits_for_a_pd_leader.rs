//! #59 — a store's first open waits out a placement-driver election instead of exiting.
//!
//! A three-member driver group has no leader for the first one to two seconds of its life, and a
//! member with no leader answers [`ProtoError::PdNotLeader`] to everything
//! ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)). A store that treated that as fatal
//! turned a one-second election into a dead node — which is what four ADR 0108 tests hit on a
//! loaded gate, each after its own `esker cluster start` reported
//! `node 1 exited with exit status: 1`:
//!
//! ```text
//! esker pd: member 1 of group 0xa9d2660b5a6b5463 (3 members)
//! esker pd: listening on 127.0.0.1:31101 … member 2 … member 3 …
//! esker server: opening /tmp/.tmphHIWR6/node-1: placement driver is not the leader
//! esker cluster: node 1 exited with exit status: 1
//! ```
//!
//! On a quiet machine the election wins the race and nothing is seen. That is the whole shape of
//! the bug: a start-order race that only a loaded machine loses, so the fix has to be a rule and
//! not a longer sleep somewhere.
//!
//! # The rule was narrower, and the narrow version cost two gates
//!
//! It waited on an unreachable member only **after** some other member had answered, on the
//! reasoning that `esker cluster start` orders the driver before the stores on purpose — so a
//! refused connection had to be a mistyped `--pd` and should fail in a second rather than in half
//! a minute.
//!
//! A refused connection is also what a store sees when a placement driver's **process is up and
//! its port is not listening yet**. The two share a wire error and nothing inside the startup
//! window tells them apart, so the narrow rule silently took the wrong reading whenever the store
//! won the race — which is decided by the machine's load, so it failed only under load. Two of
//! three gates on 2026-09-11 went red on `cluster_pd_member_change` and `durability_pd_failover`
//! with `the store exited with exit status: 1`, at 2.4 s, on an otherwise quiet machine.
//!
//! **So both refusals are waited on, under one budget.** The cost is real and is stated rather than
//! hidden: a genuinely wrong `--pd` now fails after the budget instead of in a second, and the
//! message it fails with names the endpoint it last tried.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esker_proto::ProtoError;
use esker_store::pd::{FakePd, PdClient, RegionRoute, StoreInfo};
use esker_store::{Bootstrapped, RegionHeartbeat, Store, StoreHeartbeat, StoreOptions};

/// A placement driver that answers a **script** of refusals and then behaves.
///
/// Scripted rather than timed, because what is under test is which refusals are waited on, and a
/// double that answered by the clock would make the test measure the poll interval instead.
#[derive(Debug)]
struct Electing {
    /// Answered in order, one per call, until it is empty.
    script: Mutex<VecDeque<ProtoError>>,
    /// Answered once the script is spent, for a driver that never settles at all. `None` is the
    /// group finishing its election.
    after: Option<fn() -> ProtoError>,
    /// Answers once the script is spent and `after` is `None`.
    settled: FakePd,
    /// Every call to [`PdClient::bootstrap`], so a test can say how many attempts really happened
    /// rather than inferring it from the wall clock.
    calls: AtomicUsize,
}

impl Electing {
    fn answering(script: Vec<ProtoError>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            after: None,
            settled: FakePd::new(),
            calls: AtomicUsize::new(0),
        })
    }

    /// A driver that answers `refusal` for ever — an election that never ends.
    fn always(refusal: fn() -> ProtoError) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(VecDeque::new()),
            after: Some(refusal),
            settled: FakePd::new(),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl PdClient for Electing {
    fn bootstrap(&self, store: &StoreInfo) -> Result<Bootstrapped, ProtoError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.script.lock().unwrap().pop_front() {
            Some(refusal) => Err(refusal),
            None => match self.after {
                Some(refusal) => Err(refusal()),
                None => self.settled.bootstrap(store),
            },
        }
    }

    fn alloc_id(&self, count: u64) -> Result<u64, ProtoError> {
        self.settled.alloc_id(count)
    }

    fn get_region(&self, key: &[u8]) -> Result<Option<RegionRoute>, ProtoError> {
        self.settled.get_region(key)
    }

    fn store_heartbeat(&self, beat: &StoreHeartbeat) -> Result<(), ProtoError> {
        self.settled.store_heartbeat(beat)
    }

    fn region_heartbeat(
        &self,
        beat: &RegionHeartbeat,
    ) -> Result<Option<esker_proto::Operator>, ProtoError> {
        self.settled.region_heartbeat(beat)
    }
}

/// A member that does not know who leads: the honest answer during an election, and the one the
/// whole file is about.
fn electing() -> ProtoError {
    ProtoError::PdNotLeader {
        leader_id: 0,
        leader_address: String::new(),
    }
}

/// A member that could not be dialled at all. `NotSent` is the one failure that provably never
/// left this process, which is why it is the only unreachable the rule below will wait on.
fn not_dialled() -> ProtoError {
    ProtoError::NotSent {
        detail: "connecting to 127.0.0.1:31102: Connection refused".to_owned(),
    }
}

/// Opens against `pd`, with a startup budget short enough that a test can spend all of it.
fn open_against(pd: &Arc<Electing>, wait: Duration) -> Result<Arc<Store>, esker_store::StoreError> {
    let dir = tempfile::tempdir().unwrap();
    Store::open(
        dir.path(),
        StoreOptions {
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:7001".to_owned(),
            pd_leader_wait: wait,
            ..StoreOptions::new()
        },
    )
}

/// Long enough that the scripted refusals below are all waited out, short enough that a test
/// which stopped waiting would not sit for half a minute before saying so.
const ENOUGH: Duration = Duration::from_secs(5);

/// **The bug, as a rule**: three refusals from a group mid-election, and the store comes up.
///
/// The attempt count is the assertion that matters. A store that opened because the double
/// happened to answer on the first call would pass a test that only checked it opened, so what is
/// asserted is that it asked **four** times — three refusals waited out, then the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_waits_out_a_placement_driver_election() {
    let pd = Electing::answering(vec![electing(), electing(), electing()]);
    let store =
        open_against(&pd, ENOUGH).expect("a store opens against a group that is still electing");
    assert_eq!(pd.calls(), 4, "three refusals waited out, then the answer");
    store.stop();
}

/// **The window this file is named for**: a driver whose port is not up yet, and the store waits
/// rather than exiting.
///
/// This is the case the narrow rule got wrong. Nothing has answered, so there is no evidence a
/// group exists — and the store waits anyway, because inside the startup budget "the port is not
/// listening yet" and "the address is wrong" are the same bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_waits_for_a_driver_whose_port_is_not_up_yet() {
    let pd = Electing::answering(vec![not_dialled(), not_dialled(), not_dialled()]);
    let store = open_against(&pd, ENOUGH).expect("a store opens against a driver that is starting");
    assert_eq!(
        pd.calls(),
        4,
        "three refused dials waited out, then the answer"
    );
    store.stop();
}

/// **The cost of that, stated rather than hidden.** A driver that is genuinely not there fails the
/// open — after the budget, not in a second, and with the endpoint it last tried in the message.
///
/// The wording matters and is asserted: *answered* is a different diagnosis from *led the group*.
/// A group that never elected is up and undecided; one that never answered may not be there at all,
/// and an operator reading the second needs the address, which is why the refusal is carried
/// through.
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_that_never_answers_fails_after_the_budget_and_names_it() {
    let pd = Electing::always(not_dialled);
    let began = Instant::now();
    let error = open_against(&pd, Duration::from_millis(600)).unwrap_err();
    assert!(
        error.to_string().contains("no placement driver answered"),
        "a driver that never spoke is not an election that never ended: {error}"
    );
    assert!(
        error.to_string().contains("127.0.0.1:31102"),
        "the endpoint it could not reach has to be in the message: {error}"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(600),
        "it gave up before its own budget: {:?}",
        began.elapsed()
    );
}

/// **The mixed sequence**, which is what a store started before the rest of its group actually
/// meets: one member up and leaderless, the other two not yet listening.
///
/// It matters that the two refusals **interleave**. The rotation visits endpoints in turn, so which
/// one a given attempt lands on is not something the store chooses — and a rule that treated the
/// two differently would make the outcome depend on the order they happened to arrive in. Here they
/// alternate, and the store comes up regardless.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_comes_up_whichever_refusal_the_rotation_lands_on() {
    let pd = Electing::answering(vec![electing(), not_dialled(), not_dialled(), electing()]);
    let store =
        open_against(&pd, ENOUGH).expect("a store opens while its group is still assembling");
    assert_eq!(pd.calls(), 5, "four refusals waited out, then the answer");
    store.stop();
}

/// **The bound is a bound.** A group that never elects is not waited on for ever — the open ends,
/// and the error says it *waited* rather than repeating the refusal as though it had come back at
/// once.
///
/// The budget is a field on `StoreOptions` for exactly this: at its default of thirty seconds
/// this test would not be run, and a branch nothing runs is a branch nothing holds in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_that_never_elects_ends_the_wait_and_says_it_waited() {
    let pd = Electing::always(electing);
    let began = Instant::now();
    let error = open_against(&pd, Duration::from_millis(600)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no placement driver led the group"),
        "the bound must name itself, not echo the refusal: {error}"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(600),
        "it gave up before its own budget: {:?}",
        began.elapsed()
    );
    assert!(pd.calls() > 1, "it never retried at all");
}
