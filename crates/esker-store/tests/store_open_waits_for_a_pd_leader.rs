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
//! # Why this is three tests and not one
//!
//! The rule has a **boundary**, and a rule tested only from the inside is a rule nothing holds in
//! place. Waiting is right for a refusal an election explains; it is wrong for a placement driver
//! that is simply not there, because `esker cluster start` already orders the driver before the
//! stores on purpose (`cluster/mod.rs`: *"A store whose PD is not up yet fails to open, which is
//! the behaviour that makes a cluster's start order matter here and nowhere else"*), and a
//! mistyped `--pd` should say so in a second rather than in half a minute.
//!
//! So the three tests are: the refusal that is waited out, the one that is not, and the **mixed**
//! sequence a store meets when it is started before the rest of the group — where a member that is
//! down is only worth waiting on once some other member has said an election is running.

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

/// **The boundary.** A placement driver that was never dialled is not an election, and the open
/// fails on the first refusal rather than spending the election budget on a mistyped address.
///
/// Asserted by the **attempt count**, not by a stopwatch: `calls == 1` says no retry happened at
/// all, where an elapsed-time bound would pass just as well on a machine that was merely fast.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_does_not_wait_for_a_driver_that_was_never_there() {
    let pd = Electing::answering(vec![not_dialled(), not_dialled(), not_dialled()]);
    let began = Instant::now();
    let error = open_against(&pd, ENOUGH).unwrap_err();
    assert_eq!(
        pd.calls(),
        1,
        "an undialled driver is not waited on: {error}"
    );
    assert!(
        error.to_string().contains("Connection refused"),
        "the refusal a store exits on must still name itself: {error}"
    );
    assert!(
        began.elapsed() < ENOUGH,
        "it failed, but only after waiting: {:?}",
        began.elapsed()
    );
}

/// **The mixed sequence**, which is what a store started before the rest of its group actually
/// meets: one member up and leaderless, the other two not yet listening.
///
/// Once *any* member has said an election is running, a member that cannot be dialled is one more
/// member to wait on rather than a reason to give up — the group provably exists. This is the
/// second construction the brief named ("store 先于第二个成员起"), and without it the fix is
/// decided by which endpoint the rotation happened to land on last.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_that_is_down_is_waited_out_once_the_group_is_known_to_be_electing() {
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
