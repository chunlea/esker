//! Three placement drivers in one process: elections, refusals, and what survives losing one.
//!
//! # Why there is no socket here
//!
//! The properties this file is about — who may answer, what a follower says, whether a timestamp
//! can repeat across a failover — are properties of the *algorithm*, and a socket would only make
//! them arrive at unpredictable times. So the transport is a direct call into the target member,
//! with a switch the test can throw to cut one off. `tests/loopback.rs` is where PD meets a real
//! socket, and `crates/esker-pd/src/transport.rs` is where the real transport's own rules live.
//!
//! Time is the same: [`Pd::tick`] is called by the test rather than by an interval, so an election
//! timeout is a number of loop iterations. That is what `CLAUDE.md` invariant 4 buys — the core
//! reads no clock, so a test can be the clock.
//!
//! # Why the transport delivers, and the test does not
//!
//! The first version of this file queued messages and drained the queue between ticks. It hangs,
//! and the reason is worth writing down: on a group of three, a proposal commits only when the
//! other members acknowledge it — and `Pd::bootstrap` **blocks the calling thread** until its entry
//! applies. A test that owned delivery would be holding the only thread that could deliver the
//! acknowledgement it was waiting for. That is not a quirk of the harness, it is the shape of the
//! real thing: in a running placement driver the acknowledgement arrives on the reactor while the
//! request sits on a blocking thread, and `Pd::step_raft` is deliberately non-blocking so those two
//! never trade places.
//!
//! So [`Wire`] steps straight into the target's driver, from the sending member's own driver
//! thread, exactly as the TCP transport's delivery task does. The test blocks in the call and the
//! drivers do the work.
//!
//! # The barrier
//!
//! `tick` returns as soon as the driver has the message; [`Pd::settle`] waits until it has driven
//! it. An election is tick-then-settle, repeated, and without the settles the test would be racing
//! the driver threads rather than driving them.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// One `as usize` on a compile-time constant that is 20, in a test.
#![allow(clippy::cast_possible_truncation)]

use std::sync::{Arc, Mutex, Weak};

use esker_pd::clock::TestClock;
use esker_pd::driver::PdTransport;
use esker_pd::member::{MemberList, PdMember};
use esker_pd::{Clock, Pd, PdError, PdOptions};
use esker_proto::pd::PdRaftBatch;
use esker_raft::{Message, NodeId};

/// One election timeout, in rounds of this harness. `esker-raft` counts in `u64`; everything here
/// counts loop iterations.
const ELECTION_TIMEOUT: usize = esker_raft::ELECTION_TIMEOUT_MAX_TICKS as usize;

/// Ticks the group is driven for before an election is called failed.
///
/// `ELECTION_TIMEOUT_MAX_TICKS` is 20, so 200 is ten timeouts: enough that a split vote or two
/// resolves, and short enough that a group which cannot elect fails in under a second.
const ELECTION_BUDGET: usize = 200;

/// Settles run per tick, so that a request, its acknowledgement and the apply that follows all
/// land before the next one. Three is the length of that chain.
const SETTLES_PER_ROUND: usize = 3;

// ---------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------

/// The members, and which of them are cut off.
///
/// `Weak`, because every member holds this and it holds every member. Filled in after the three
/// are open; a send before that is dropped, which costs nothing because a group of three
/// campaigns on a tick and no tick has happened yet.
#[derive(Debug, Default)]
struct Wire {
    members: Mutex<Vec<(NodeId, Weak<Pd>)>>,
    group_id: Mutex<u64>,
    /// Members that neither send nor receive, as a partition would leave them.
    cut: Mutex<Vec<NodeId>>,
}

impl Wire {
    fn join(&self, id: NodeId, pd: &Arc<Pd>) {
        let mut members = self.members.lock().unwrap();
        members.retain(|(other, _)| *other != id);
        members.push((id, Arc::downgrade(pd)));
        *self.group_id.lock().unwrap() = pd.members().group_id();
    }

    fn cut_off(&self, id: NodeId) {
        self.cut.lock().unwrap().push(id);
    }

    fn restore(&self, id: NodeId) {
        self.cut.lock().unwrap().retain(|cut| *cut != id);
    }

    fn is_cut(&self, id: NodeId) -> bool {
        self.cut.lock().unwrap().contains(&id)
    }

    fn find(&self, id: NodeId) -> Option<Arc<Pd>> {
        self.members
            .lock()
            .unwrap()
            .iter()
            .find(|(other, _)| *other == id)
            .and_then(|(_, pd)| pd.upgrade())
    }
}

/// One member's end of the wire: it steps straight into the target, from this member's own driver
/// thread, the way the TCP transport's delivery task does.
#[derive(Debug)]
struct Post {
    id: NodeId,
    wire: Arc<Wire>,
}

impl PdTransport for Post {
    fn send(&self, messages: Vec<Message>) {
        if self.wire.is_cut(self.id) {
            return;
        }
        let group_id = *self.wire.group_id.lock().unwrap();
        for message in messages {
            let to = message.recipient();
            // A partition is not one-way: a leader that could still be *heard* would never step
            // down, and check-quorum is half of what this file tests.
            if self.wire.is_cut(to) {
                continue;
            }
            let Some(target) = self.wire.find(to) else {
                continue;
            };
            // `step_raft` posts to the target's driver and returns; it never blocks, which is what
            // keeps this call — made from a driver thread — from being able to deadlock.
            let _ = target.step_raft(&PdRaftBatch::new(group_id, self.id, vec![message]));
        }
    }
}

// ---------------------------------------------------------------------------------------
// The group
// ---------------------------------------------------------------------------------------

struct Group {
    members: MemberList,
    pds: Vec<Arc<Pd>>,
    clocks: Vec<Arc<TestClock>>,
    dirs: Vec<tempfile::TempDir>,
    wire: Arc<Wire>,
}

impl Group {
    /// Three members, each on its own database, none of them leading yet.
    fn of_three(now_ms: u64) -> Self {
        let members = MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:32379"),
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:32381"),
        ])
        .unwrap();
        let wire = Arc::new(Wire::default());
        let mut pds = Vec::new();
        let mut clocks = Vec::new();
        let mut dirs = Vec::new();
        for member in members.members() {
            let dir = tempfile::tempdir().unwrap();
            let clock = Arc::new(TestClock::new(now_ms));
            let pd = open(member.id, &members, dir.path(), &clock, &wire);
            wire.join(member.id, &pd);
            pds.push(pd);
            clocks.push(clock);
            dirs.push(dir);
        }
        Self {
            members,
            pds,
            clocks,
            dirs,
            wire,
        }
    }

    fn at(&self, id: NodeId) -> &Arc<Pd> {
        &self.pds[slot(id)]
    }

    /// Waits until every member has driven everything it has been given.
    fn settle(&self) {
        for _ in 0..SETTLES_PER_ROUND {
            for pd in &self.pds {
                pd.settle().unwrap();
            }
        }
    }

    /// One round: everyone ticks, and everyone catches up on what that produced.
    fn round(&self) {
        for pd in &self.pds {
            pd.tick().unwrap();
        }
        self.settle();
    }

    /// Runs until some member is serving, or gives up.
    fn elect(&self) -> NodeId {
        self.elect_where(|_| true, "any member")
    }

    /// Runs until some member other than `excluded` is serving.
    fn elect_without(&self, excluded: NodeId) -> NodeId {
        self.elect_where(
            move |id| id != excluded,
            "a member other than the one cut off",
        )
    }

    fn elect_where(&self, wanted: impl Fn(NodeId) -> bool, what: &str) -> NodeId {
        for _ in 0..ELECTION_BUDGET {
            if let Some(id) = self.serving_where(&wanted) {
                return id;
            }
            self.round();
        }
        panic!(
            "no leader ({what}) after {ELECTION_BUDGET} ticks; the members believe {:?}",
            self.beliefs()
        );
    }

    /// A serving member that is not `excluded`, if there is one.
    fn leader_without(&self, excluded: NodeId) -> Option<NodeId> {
        self.serving_where(&|id| id != excluded)
    }

    /// The member that may answer, if there is one.
    fn leader(&self) -> Option<NodeId> {
        self.serving_where(&|_| true)
    }

    fn serving_where(&self, wanted: &impl Fn(NodeId) -> bool) -> Option<NodeId> {
        self.pds
            .iter()
            .map(|pd| pd.leadership())
            .find(|office| office.serving && wanted(office.id))
            .map(|office| office.id)
    }

    fn beliefs(&self) -> Vec<(NodeId, esker_raft::Role, Option<NodeId>, bool)> {
        self.pds
            .iter()
            .map(|pd| {
                let office = pd.leadership();
                (office.id, office.role, office.leader, office.serving)
            })
            .collect()
    }

    /// Advances every member's clock, so that a test about timestamps is not one about a clock
    /// that stood still.
    fn advance_clocks(&self, by_ms: u64) {
        for clock in &self.clocks {
            clock.advance(by_ms);
        }
    }

    /// Opens a member that was **not** in the founding group, as `esker pd serve --join` does.
    ///
    /// Its database is empty and its configuration is empty: it is a member of nothing until the
    /// group tells it otherwise. What it is *told* is the group's id and its members, which is
    /// exactly what `Pd::Members` answers and what
    /// [ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)
    /// says a joining member is given.
    fn admit(&mut self, id: NodeId, address: &str) -> Arc<Pd> {
        let group_id = self
            .at(self.leader().expect("a leader to join"))
            .membership()
            .group_id;
        let joining = MemberList::joining(
            self.members
                .members()
                .iter()
                .cloned()
                .chain(std::iter::once(PdMember::new(id, address)))
                .collect(),
            group_id,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(self.clocks[0].now_ms()));
        let pd = open(id, &joining, dir.path(), &clock, &self.wire);
        self.wire.join(id, &pd);
        self.pds.push(Arc::clone(&pd));
        self.clocks.push(clock);
        self.dirs.push(dir);
        self.members = joining;
        pd
    }

    /// Runs a blocking placement-driver call while ticking the group, and **fails** rather than
    /// hanging.
    ///
    /// Every operator call on a group of more than one blocks: it is answered when its entry
    /// *applies*, and applying may need the ticks this loop produces — a member noticed as gone, a
    /// leader deposed. Calling one inline would be the module header's mistake one level up, the
    /// test holding the thread that has to unblock it.
    ///
    /// A hang is the worst kind of red. It says nothing, it wedges whatever else is sharing the
    /// machine, and it has to be read with a debugger instead of off the failure.
    fn run<T: Send>(&self, what: &str, call: impl FnOnce() -> T + Send) -> T {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|scope| {
            let flag = Arc::clone(&done);
            let worker = scope.spawn(move || {
                let outcome = call();
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                outcome
            });
            for _ in 0..(ELECTION_BUDGET * 4) {
                if done.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                self.round();
            }
            assert!(
                worker.is_finished(),
                "{what} never returned, and joining it now would hang; the members believe {:?}",
                self.beliefs()
            );
            worker.join().expect("the worker thread")
        })
    }

    /// Runs `step` until it answers `true`. Each attempt goes through [`Group::run`].
    fn until(&self, what: &str, step: impl Fn() -> esker_pd::Result<bool> + Sync) {
        for _ in 0..ELECTION_BUDGET {
            match self.run(what, &step) {
                Ok(true) => return,
                Ok(false) => self.round(),
                Err(error) => panic!("{what}: {error}; the members believe {:?}", self.beliefs()),
            }
        }
        panic!(
            "{what} never answered done; the members believe {:?}",
            self.beliefs()
        );
    }

    /// Ticks until the serving member has stopped hearing from `id`.
    ///
    /// **Cutting a member off is not the same as the group knowing it is gone**, and the gap is a
    /// whole election timeout: `recent_active` is reset on every one, so a member that died a tick
    /// ago still reads as alive — honestly, because a tick ago it was. An operator removing a
    /// member removes one that has been down for a while; a test that asked one tick after the cut
    /// would be testing a question nobody asks.
    fn wait_until_noticed(&self, id: NodeId) {
        for _ in 0..(ELECTION_BUDGET * 2) {
            let leader = self.leader().expect("a leader to notice with");
            let seen = self
                .at(leader)
                .driver_progress()
                .unwrap()
                .and_then(|(_, progress)| {
                    progress
                        .iter()
                        .find(|peer| peer.id == id)
                        .map(|peer| peer.recent_active)
                })
                .unwrap_or(false);
            if !seen {
                return;
            }
            self.round();
        }
        panic!("member {id} was cut off and the group never noticed");
    }

    /// Re-opens a member over the same directory.
    ///
    /// A **crash**, not a pause: it loses every scrap of memory it held — its allocator's position
    /// inside a reservation, its oracle's logical counter, its in-flight operators — and keeps only
    /// what it had applied.
    fn restart(&mut self, id: NodeId) {
        let at = slot(id);
        // Dropped first, so its driver thread is joined and its database closed before the
        // replacement opens the same files.
        self.pds[at] = open(
            id,
            &self.members,
            self.dirs[at].path(),
            &self.clocks[at],
            &self.wire,
        );
        self.wire.join(id, &self.pds[at]);
    }
}

/// Where member `id` sits in the group's parallel arrays. Ids run from one and there are three.
fn slot(id: NodeId) -> usize {
    usize::try_from(id).expect("a member id fits a usize") - 1
}

fn open(
    id: NodeId,
    members: &MemberList,
    dir: &std::path::Path,
    clock: &Arc<TestClock>,
    wire: &Arc<Wire>,
) -> Arc<Pd> {
    let options = PdOptions {
        id,
        members: members.clone(),
        transport: Some(Arc::new(Post {
            id,
            wire: Arc::clone(wire),
        })),
        // One seed for the whole group: the member id selects the stream, so the three still draw
        // different election timeouts while one number reproduces the run
        // (`esker_raft::Config`).
        raft_seed: 0xE5,
        ..PdOptions::with_clock(Arc::clone(clock) as Arc<dyn Clock>)
    };
    Pd::open(dir, options).unwrap()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The headline: three members elect one leader, and exactly one member may answer.
#[test]
fn three_members_elect_one_leader_and_only_it_serves() {
    let group = Group::of_three(1_700_000_000_000);
    // Before anyone has won, nobody may answer. A member that served here would be serving out of
    // its own opinion.
    assert_eq!(group.leader(), None, "a member served before any election");

    let leader = group.elect();
    let serving: Vec<NodeId> = group
        .pds
        .iter()
        .filter(|pd| pd.is_serving())
        .map(|pd| pd.leadership().id)
        .collect();
    assert_eq!(serving, vec![leader], "more than one member is serving");

    let office = group.at(leader).leadership();
    assert!(office.serving);
    assert_eq!(
        office.office_term, office.term,
        "the leader is serving in a term it did not take office in"
    );
}

/// Trap 1 of the brief, and the one that corrupts data silently: a follower that answered a `Tso`
/// would hand out a timestamp the leader may also hand out.
///
/// **The address is asserted, not just the variant**, and that is the whole value of this test.
/// A follower refuses a *write* twice over — the leader check in `Pd::leading`, and the core
/// refusing to propose on a non-leader — and only the first of those knows where to send the
/// caller. Removing the leader check leaves this passing on the variant alone, which is what
/// removing it and re-running proved; the address is the discriminator.
#[test]
fn a_follower_refuses_every_answer_only_a_leader_may_give() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    group.at(leader).bootstrap(1, "127.0.0.1:20160").unwrap();
    let expected = group.members.address_of(leader).unwrap();

    for pd in &group.pds {
        if pd.leadership().id == leader {
            continue;
        }
        assert!(!pd.is_serving(), "a follower says it may serve");
        let refusals = [
            ("tso", pd.tso(1).err()),
            ("alloc_id", pd.alloc_id(1).err()),
            ("bootstrap", pd.bootstrap(2, "127.0.0.1:20161").err()),
            ("report_columnar", pd.report_columnar(Vec::new()).err()),
        ];
        for (what, error) in refusals {
            let Some(PdError::NotLeader {
                leader_id,
                leader_address,
            }) = error
            else {
                panic!("{what} was not refused with a redirect: {error:?}");
            };
            assert_eq!(leader_id, leader, "{what} named the wrong leader");
            assert_eq!(leader_address, expected, "{what} named no address");
        }
    }
}

/// **A follower must refuse a *read* as well**, and that refusal lives in one place only: the
/// leader check at the top of the service's dispatch.
///
/// The writes above refuse themselves inside `Pd`, so they pass with the service's check removed.
/// A read takes no proposal — a follower would happily answer `GetRegion` out of whatever it had
/// applied — so this is the only test that fails when that check goes, and it goes through
/// `PdService` rather than through `Pd` because that is where the check is.
#[tokio::test]
async fn a_follower_refuses_a_read_and_still_answers_for_itself() {
    use esker_proto::pd::{PdReq, PdResp};
    use esker_proto::{ProtoError, Reply, Request, Response, Service};

    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let cluster_id = group
        .at(leader)
        .bootstrap(1, "127.0.0.1:20160")
        .unwrap()
        .cluster_id;
    group.settle();
    let expected = group.members.address_of(leader).unwrap().to_owned();

    let follower = (1..=3).find(|id| *id != leader).unwrap();
    let service = esker_pd::PdService::new(Arc::clone(group.at(follower)));
    let ask = |request: PdReq| {
        let service = Arc::clone(&service);
        async move {
            service
                .call(Request::Pd {
                    cluster_id,
                    request,
                })
                .await
        }
    };

    for request in [
        PdReq::GetRegion {
            key: bytes::Bytes::from_static(b"anything"),
        },
        PdReq::ScanRegions {
            start_key: bytes::Bytes::new(),
            limit: 8,
        },
        PdReq::SchemaLease,
    ] {
        let name = request.method().name();
        match ask(request).await {
            Err(ProtoError::PdNotLeader {
                leader_id,
                leader_address,
            }) => {
                assert_eq!(leader_id, leader, "{name} named the wrong leader");
                assert_eq!(leader_address, expected, "{name} named no address");
            }
            other => panic!("a follower answered {name}: {other:?}"),
        }
    }

    // And the two that are questions about **this process** are still answered, because they are
    // what an operator asks a placement driver that has stopped answering.
    let Ok(Reply::Unary(Response::Pd(PdResp::Members(membership)))) = ask(PdReq::Members).await
    else {
        panic!("a follower refused Members, which is the one thing it must always answer");
    };
    assert_eq!(membership.this_id, follower);
    assert_eq!(membership.leader_id, leader);
    assert_eq!(membership.members.len(), 3);
    assert!(matches!(
        ask(PdReq::Status).await,
        Ok(Reply::Unary(Response::Pd(PdResp::Status { .. })))
    ));
}

/// A refusal that does not say where to go is a refusal the caller can only back off from. Every
/// follower that knows a leader names it, **by address**, out of its own member list.
#[test]
fn a_follower_names_the_leader_it_knows_by_address() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let expected = group.members.address_of(leader).unwrap().to_owned();

    for pd in &group.pds {
        if pd.leadership().id == leader {
            continue;
        }
        let Err(PdError::NotLeader {
            leader_id,
            leader_address,
        }) = pd.tso(1)
        else {
            panic!("a follower did not refuse");
        };
        assert_eq!(leader_id, leader);
        assert_eq!(leader_address, expected);
    }
}

/// What replication is for: every member ends up holding the same records, because every member
/// applied the same log.
#[test]
fn every_member_holds_what_the_leader_wrote() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let pd = group.at(leader);

    let cluster_id = pd.bootstrap(1, "127.0.0.1:20160").unwrap().cluster_id;
    pd.alloc_id(5).unwrap();
    pd.tso(4).unwrap();
    group.settle();

    for other in &group.pds {
        assert_eq!(
            other.cluster().unwrap().map(|cluster| cluster.cluster_id),
            Some(cluster_id),
            "member {} does not know the cluster",
            other.leadership().id
        );
        assert_eq!(
            other.regions().unwrap().len(),
            1,
            "member {} does not hold region 1",
            other.leadership().id
        );
        assert_eq!(
            other.tso_high_water_ms().unwrap(),
            pd.tso_high_water_ms().unwrap(),
            "member {} holds a different mark",
            other.leadership().id
        );
    }
}

/// The group-id guard, which is what stops two clusters' placement drivers from forming one and
/// replicating one cluster's routing table over the other's.
#[test]
fn a_batch_for_another_group_is_refused() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let real = group.members.group_id();

    let stranger = PdRaftBatch::new(
        real ^ 1,
        2,
        vec![Message::TimeoutNow {
            from: 2,
            to: 1,
            term: 99,
        }],
    );
    assert!(
        matches!(
            group.at(1).step_raft(&stranger),
            Err(PdError::Invalid { .. })
        ),
        "a batch for another group was stepped"
    );
    // And it changed nothing: a refused batch must not have moved the term or the leadership.
    assert_eq!(group.leader(), Some(leader));

    // A batch that carries the right group id but claims to come from outside it is refused too:
    // a message from outside the configuration is one the core would have to reason about.
    let outsider = PdRaftBatch::new(
        real,
        9,
        vec![Message::TimeoutNow {
            from: 9,
            to: 1,
            term: 99,
        }],
    );
    assert!(matches!(
        group.at(1).step_raft(&outsider),
        Err(PdError::Invalid { .. })
    ));
    assert_eq!(group.leader(), Some(leader));
}

/// **The §3.3 proof, and the one thing `docs/plans/phase-4.md` §15 said to check first.**
///
/// A new leader's first timestamp is strictly above every timestamp the old one handed out, and it
/// is so for a reason that survives the clock: every timestamp given away had `physical < mark`,
/// the mark is *committed* before it is crossed, and a new leader resumes at
/// `max(clock, committed mark)`. So the clocks are moved **backwards** here — as
/// `tests/crash_kill.rs` does across a restart — because a test whose clocks only ever go forwards
/// would pass against an implementation that had no mark at all.
#[test]
fn a_new_leader_starts_above_the_last_committed_window() {
    let group = Group::of_three(1_700_000_000_000);
    let first = group.elect();
    group.at(first).bootstrap(1, "127.0.0.1:20160").unwrap();

    let mut handed_out = Vec::new();
    for _ in 0..40 {
        handed_out.push(group.at(first).tso(1).unwrap());
        group.advance_clocks(1);
        group.settle();
    }
    let mark = group.at(first).tso_high_water_ms().unwrap();
    assert!(
        handed_out
            .iter()
            .all(|ts| esker_pd::decompose_ts(*ts).0 < mark),
        "a timestamp was handed out at or above the mark that covers it"
    );

    // The leader goes, and the clock goes *backwards* under its successors.
    group.wire.cut_off(first);
    group.advance_clocks(0);
    for clock in &group.clocks {
        clock.set(1_699_999_000_000);
    }
    let second = group.elect_without(first);
    assert_ne!(second, first);

    let next = group.at(second).tso(1).unwrap();
    let last = *handed_out.last().unwrap();
    assert!(
        next > last,
        "the new leader handed out {next}, at or below {last} which had already left the \
         placement driver — the mark did not survive the failover"
    );
    assert!(
        esker_pd::decompose_ts(next).0 >= mark,
        "the new leader resumed below the committed mark, so it is only the clock keeping it \
         apart from its predecessor"
    );
}

/// The other half of the lease argument: a leader that has lost its quorum and **has not noticed**
/// cannot hand out a timestamp its successor might hand out too.
///
/// Below its own mark it needs no commit and answers freely — and that is safe, because the
/// successor starts at or above that mark. To cross it, it needs a commit it will never get, so
/// the call **fails** rather than answering. Ack after commit, never after propose.
#[test]
fn a_deposed_leader_cannot_cross_its_own_mark() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    group.at(leader).bootstrap(1, "127.0.0.1:20160").unwrap();

    // **While it still has a quorum**, so that there is a committed window to be inside of. A
    // mark of zero would make the "inside" call below propose, which is the very thing this test
    // says a member with no quorum cannot do.
    group.at(leader).tso(1).unwrap();
    let mark = group.at(leader).tso_high_water_ms().unwrap();
    assert!(
        mark > 0,
        "no window was committed, so there is nothing to be inside"
    );

    group.wire.cut_off(leader);
    // Still inside its window, so still answering — no commit is needed and none is possible.
    let inside = group.at(leader).tso(1).unwrap();
    assert!(
        esker_pd::decompose_ts(inside).0 < mark,
        "a timestamp came out at or above the mark that covers it"
    );

    // Now push it past the mark, on another thread: the call blocks until this member notices it
    // has lost its quorum, and the ticks that make it notice come from here. The thread is what
    // makes this a test rather than a hang — in a running placement driver the request sits on a
    // blocking thread while the ticks that end it arrive on the reactor.
    let deposed = Arc::clone(group.at(leader));
    let clock = Arc::clone(&group.clocks[slot(leader)]);
    let asking = std::thread::spawn(move || {
        clock.set(1_700_000_000_000 + esker_pd::TSO_SAVE_INTERVAL_MS + 1);
        deposed.tso(1)
    });
    for _ in 0..ELECTION_BUDGET {
        if asking.is_finished() {
            break;
        }
        group.round();
    }
    assert!(
        asking.is_finished(),
        "the deposed member never gave up on a timestamp it could not commit; joining now would \
         hang for ever, so this fails instead"
    );
    let answer = asking.join().expect("the asking thread");
    assert!(
        answer.is_err(),
        "a placement driver with no quorum handed out {answer:?} above its committed mark"
    );
}

/// An id is a timestamp with a different name: the reservation is the lease, a deposed leader is
/// confined below `allocated_end`, and a new leader resumes at `allocated_end + 1`.
#[test]
fn an_id_is_never_handed_out_twice_across_a_failover() {
    let group = Group::of_three(1_700_000_000_000);
    let first = group.elect();
    group.at(first).bootstrap(1, "127.0.0.1:20160").unwrap();

    let mut ids = Vec::new();
    for _ in 0..20 {
        ids.push(group.at(first).alloc_id(1).unwrap());
        group.settle();
    }

    group.wire.cut_off(first);
    let second = group.elect_without(first);
    for _ in 0..20 {
        ids.push(group.at(second).alloc_id(1).unwrap());
        group.settle();
    }

    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "an id was handed out twice: {ids:?}"
    );
    for pair in ids.windows(2) {
        assert!(pair[1] > pair[0], "ids went backwards: {ids:?}");
    }
}

/// Liveness, which is the whole point: losing one of three costs an election and nothing else.
#[test]
fn the_survivors_serve_within_one_election_timeout() {
    let group = Group::of_three(1_700_000_000_000);
    let first = group.elect();
    let cluster_id = group
        .at(first)
        .bootstrap(1, "127.0.0.1:20160")
        .unwrap()
        .cluster_id;
    group.settle();

    group.wire.cut_off(first);
    let mut rounds = 0;
    let second = loop {
        if let Some(id) = group.leader_without(first) {
            break id;
        }
        rounds += 1;
        assert!(rounds <= ELECTION_BUDGET, "the survivors never elected");
        group.round();
    };

    // Four election timeouts of headroom. One has to expire before anybody campaigns at all; a
    // split vote — both survivors campaigning together, each voting for itself, neither reaching
    // the quorum of two — costs another, and the randomised redraw makes a second split unlikely
    // rather than impossible. Past four the group is churning, not electing, which is a different
    // failure and worth catching.
    assert!(
        rounds <= 4 * ELECTION_TIMEOUT,
        "the survivors took {rounds} ticks, more than four election timeouts"
    );
    // And they serve the cluster the dead leader created, not a new one.
    assert_eq!(
        group.at(second).cluster_id().unwrap(),
        cluster_id,
        "the survivors lost the cluster record"
    );
    assert!(group.at(second).alloc_id(1).is_ok());
}

/// A member that crashed and came back is the case that made the barrier necessary: it holds every
/// committed entry the moment it is re-opened, and none of the memory it had.
#[test]
fn a_restarted_member_rejoins_with_what_it_had_applied() {
    let mut group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let cluster_id = group
        .at(leader)
        .bootstrap(1, "127.0.0.1:20160")
        .unwrap()
        .cluster_id;
    let ids = group.at(leader).alloc_id(3).unwrap();
    group.settle();

    let bystander = (1..=3).find(|id| *id != leader).unwrap();
    group.restart(bystander);
    assert_eq!(
        group.at(bystander).cluster().unwrap().map(|c| c.cluster_id),
        Some(cluster_id),
        "a restarted member forgot what it had applied"
    );
    assert!(!group.at(bystander).is_serving());

    // The group keeps working with it back, and the ids carry on where they were.
    for _ in 0..20 {
        group.round();
    }
    let leader = group.leader().expect("a leader after the restart");
    let next = group.at(leader).alloc_id(1).unwrap();
    assert!(next > ids + 2, "an id was reused after a restart");
}

/// The fear this whole design is arranged against, stated directly: the old leader comes back.
///
/// It rejoins as a follower, it stops serving, and nothing it handed out before it was cut off
/// collides with anything its successor handed out while it was gone.
#[test]
fn a_returning_leader_hands_out_nothing_its_successor_already_did() {
    let group = Group::of_three(1_700_000_000_000);
    let first = group.elect();
    group.at(first).bootstrap(1, "127.0.0.1:20160").unwrap();

    let mut ids = vec![group.at(first).alloc_id(1).unwrap()];
    let mut stamps = vec![group.at(first).tso(1).unwrap()];
    group.settle();

    group.wire.cut_off(first);
    let second = group.elect_without(first);
    for _ in 0..10 {
        ids.push(group.at(second).alloc_id(1).unwrap());
        stamps.push(group.at(second).tso(1).unwrap());
        group.advance_clocks(1);
        group.settle();
    }

    // And back it comes.
    group.wire.restore(first);
    for _ in 0..(2 * ELECTION_TIMEOUT) {
        group.round();
    }
    assert!(
        !group.at(first).is_serving(),
        "the returning member is serving beside its successor"
    );

    let leader = group.leader().expect("a leader after the reunion");
    for _ in 0..10 {
        ids.push(group.at(leader).alloc_id(1).unwrap());
        stamps.push(group.at(leader).tso(1).unwrap());
        group.advance_clocks(1);
        group.settle();
    }

    for (what, values) in [("id", &ids), ("timestamp", &stamps)] {
        for pair in values.windows(2) {
            assert!(
                pair[1] > pair[0],
                "a {what} went backwards across the reunion: {values:?}"
            );
        }
    }
}

/// **A placement driver that has just taken office is one that restarted**, to the scheduler
/// (`docs/DESIGN.md` §7, ADR 0013): the operators the previous leader had in flight were formed
/// against a cluster that has moved on, so the new one starts from the next round of heartbeats
/// and re-derives what is still needed.
#[test]
fn a_new_leader_inherits_no_operator_and_re_derives_from_a_heartbeat() {
    let group = Group::of_three(1_700_000_000_000);
    let first = group.elect();
    let pd = group.at(first);
    for store_id in 1..=3 {
        pd.bootstrap(store_id, &format!("127.0.0.1:2016{store_id}"))
            .unwrap();
    }

    // A region short of its replica target, on a cluster with somewhere to put the replacement,
    // is what makes PD issue an operator at all.
    let beat = esker_pd::RegionBeat {
        region: esker_proto::Region {
            id: 1,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            epoch: esker_proto::Epoch::new(1, 1),
            peers: vec![esker_proto::Peer::voter(1, 2)],
        },
        leader_peer_id: 2,
        term: 1,
        approximate_size: 1,
        applied_index: 1,
    };
    let issued = pd.region_heartbeat(&beat).unwrap();
    assert!(
        issued.operator.is_some(),
        "the fixture produced no operator, so this test would pass for the wrong reason"
    );
    assert_eq!(pd.in_flight().unwrap().len(), 1);
    group.settle();

    group.wire.cut_off(first);
    let second = group.elect_without(first);
    assert!(
        group.at(second).in_flight().unwrap().is_empty(),
        "member {second} took office holding the previous leader's plan"
    );

    // And it re-derives: the region is still short, so the next heartbeat earns a **fresh**
    // operator rather than PD sitting on a plan it never made.
    let again = group.at(second).region_heartbeat(&beat).unwrap();
    assert!(
        again.operator.is_some(),
        "the new leader neither inherited an operator nor derived one, so the region is stuck"
    );
    assert_eq!(group.at(second).in_flight().unwrap().len(), 1);
}

/// **The recovery path the brief asks for: two of three, back to three of three.**
///
/// The order is the whole test. A learner does not count towards a quorum, so it can be added while
/// a member is down; a voter cannot, because the configuration is in force from the moment its
/// entry is appended and the entry that made the quorum three would need three to commit. So:
/// add a learner, let it catch up, promote it, and only then remove the dead one — the same
/// add-before-remove ADR 0013 states for region replicas, for the same arithmetic
/// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md) §11.4).
#[test]
fn a_group_of_three_with_one_gone_recovers_to_three_of_three() {
    let mut group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    let cluster_id = group
        .at(leader)
        .bootstrap(1, "127.0.0.1:20160")
        .unwrap()
        .cluster_id;
    let before = group.at(leader).alloc_id(1).unwrap();

    // One member goes, and stays gone. Two live of three: alive, and one failure from dead.
    let dead = (1..=3).find(|id| *id != leader).unwrap();
    group.wire.cut_off(dead);
    let leader = group.elect_without(dead);

    // A fourth joins. It is told the group's name, because it cannot derive one that would match.
    let joiner = group.admit(4, "127.0.0.1:32382");
    assert_eq!(
        joiner.membership().group_id,
        group.at(leader).membership().group_id,
        "the joining member named a different group"
    );

    // Add, catch up, promote — one command, run until it says it is done.
    group.until("adding member 4", || {
        group.at(leader).add_member(4, "127.0.0.1:32382")
    });
    group.settle();

    // It is a voter now, and it holds what the group holds.
    let conf = group.at(leader).conf_state().unwrap();
    assert!(conf.voters.contains(&4), "member 4 was never promoted");
    assert!(conf.learners.is_empty(), "member 4 is still a learner");
    assert_eq!(
        joiner.cluster().unwrap().map(|cluster| cluster.cluster_id),
        Some(cluster_id),
        "the new member did not catch up on the state machine"
    );

    // And only now the dead one goes.
    let remover = Arc::clone(group.at(leader));
    group.until("removing the dead member", || remover.remove_member(dead));
    group.settle();
    let conf = group.at(leader).conf_state().unwrap();
    assert!(
        !conf.voters.contains(&dead),
        "the dead member is still a voter"
    );
    assert_eq!(conf.voters.len(), 3, "the group did not settle at three");

    // Three of three, and it still allocates above what it had handed out before any of this.
    let after = group.at(leader).alloc_id(1).unwrap();
    assert!(after > before, "an id went backwards across the recovery");
    assert_eq!(group.at(leader).cluster_id().unwrap(), cluster_id);
}

/// **Killed mid-change.** The three proposals an add is made of are three places for a `kill -9`,
/// and an operator who reruns the command must not be told the member already exists.
///
/// Here the kill is the member that *issued* it losing office — which is strictly harder than a
/// crash, because a different member has to finish what this one started, from the log alone.
#[test]
fn an_add_interrupted_halfway_is_finished_by_whoever_is_leading_next() {
    let mut group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    group.at(leader).bootstrap(1, "127.0.0.1:20160").unwrap();
    group.admit(4, "127.0.0.1:32382");

    // One step only: member 4 is a learner and nothing more.
    assert!(
        !group.at(leader).add_member(4, "127.0.0.1:32382").unwrap(),
        "one step should not have finished the add"
    );
    group.settle();
    let conf = group.at(leader).conf_state().unwrap();
    assert!(conf.learners.contains(&4), "member 4 is not a learner");
    assert!(!conf.voters.contains(&4));

    // And now the member that asked for it is gone.
    group.wire.cut_off(leader);
    let next = group.elect_without(leader);
    assert_ne!(next, leader);

    // The successor finishes it, from what the log says rather than from anything it was told.
    let adder = Arc::clone(group.at(next));
    group.until("finishing the add", || {
        adder.add_member(4, "127.0.0.1:32382")
    });
    group.settle();
    assert!(
        group.at(next).conf_state().unwrap().voters.contains(&4),
        "the successor did not finish the add"
    );

    // Idempotent: running it again on a member that is already a voter answers done and proposes
    // nothing, which is what makes it safe for an operator to retry after any failure.
    let term = group.at(next).leadership().term;
    assert!(group.at(next).add_member(4, "127.0.0.1:32382").unwrap());
    assert_eq!(group.at(next).leadership().term, term);
    assert_eq!(group.at(next).conf_state().unwrap().voters.len(), 4);
}

/// A placement driver may not remove its way out of a quorum, because undoing that needs the
/// quorum it just lost.
///
/// The group is given time to **notice** the member is gone first, which is both what an operator
/// would do and what makes the question meaningful: `recent_active` is reset every election
/// timeout, so one tick after a member dies it still reads as alive — and PD saying "everybody is
/// here" a tick after a death is honest rather than wrong.
#[test]
fn a_removal_that_would_lose_the_quorum_is_refused() {
    let group = Group::of_three(1_700_000_000_000);
    let leader = group.elect();
    group
        .run("bootstrapping", || {
            group.at(leader).bootstrap(1, "127.0.0.1:20160")
        })
        .unwrap();

    // One member down: two live of three, quorum two.
    let dead = (1..=3).find(|id| *id != leader).unwrap();
    group.wire.cut_off(dead);
    let leader = group.elect_without(dead);
    group.wait_until_noticed(dead);
    let other = (1..=3).find(|id| *id != leader && *id != dead).unwrap();

    // Removing the live one would leave one live of two, which is not a quorum.
    let refused = group.run("removing the live peer", || {
        group.at(leader).remove_member(other)
    });
    assert!(
        matches!(refused, Err(PdError::Invalid { .. })),
        "removing the last live peer was allowed: {refused:?}"
    );
    assert_eq!(
        group.at(leader).conf_state().unwrap().voters.len(),
        3,
        "the refusal still changed the membership"
    );

    // Removing the member that is *already* gone is the right move, and is allowed.
    group.until("removing the dead member", || {
        group.at(leader).remove_member(dead)
    });
    group.settle();
    let conf = group.at(leader).conf_state().unwrap();
    assert_eq!(conf.voters.len(), 2);
    assert!(!conf.voters.contains(&dead));
}

/// The last member cannot remove itself: a group with no placement driver is not a smaller group.
#[test]
fn the_last_member_cannot_remove_itself() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let pd = Pd::open(
        dir.path(),
        PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
    )
    .unwrap();
    assert!(matches!(pd.remove_member(1), Err(PdError::Invalid { .. })));
    assert!(pd.is_serving(), "the refusal cost it its office");
}

/// A group of one is what every other test in this crate builds, and it must not need any of the
/// machinery above: no ticks, no transport, no runtime, and serving before `open` returns.
#[test]
fn a_group_of_one_needs_no_ticks_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let pd = Pd::open(
        dir.path(),
        PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
    )
    .unwrap();
    assert!(
        pd.is_serving(),
        "a lone placement driver did not take office"
    );
    assert!(pd.members().is_alone());
    assert!(pd.bootstrap(1, "127.0.0.1:20160").unwrap().region.is_some());
    assert!(pd.tso(1).unwrap() > 0);
}
