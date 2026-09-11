//! A write survives losing the store it was routed to, without the caller being told to.
//!
//! # run 124, and why the budget never got spent
//!
//! A leader-store kill cost a SQL node **184 × `08006` in 0.695 s**, and the number that names the
//! mechanism is the one beside it: the largest gap between consecutive statements in that window
//! was **367 ms**. The statements were not *waiting* out an election — they were being **refused,
//! fast**. `h1-run124-design.md` §2's ~2.5 s leader budget describes a stall, and this was not one.
//! Rails does not retry `08006` in fixture setup, so 92 tests errored on a cluster that was
//! otherwise serving: the control sample on the same binary ran 484 assertions clean.
//!
//! The mechanism is a single predicate. `esker_proto::ProtoError::is_retryable` was
//! `NotLeader | EpochNotMatch | ServerIsBusy | RegionNotFound` — four ways a **store** says "try
//! again". When the store a request is routed to is *gone*, `TcpStores` answers `NotSent`,
//! [`esker_client::retry::classify`] sees an error outside that set, and it surfaces at once.
//! There was no entry for *"the store I was routed to is not reachable"*, so no budget could be
//! spent on it.
//!
//! # Why `NotSent` is the safest possible member of that set
//!
//! The rule the set is derived from is **a write may be re-sent only when the previous attempt
//! provably did not commit**. `NotSent` is the one variant that says exactly that in its own
//! documentation — *"the request never reached the wire. Safe to send again"* — and
//! `ProtoError::outcome` has answered `NotApplied` for it all along. Every other member of the set
//! is a refusal the store *chose to send*; this one never left the client.
//!
//! **`Closed` is deliberately not added.** Its outcome is `Unknown`: the request went out and the
//! answer was lost, so a write may have committed. A read in that position is already re-asked —
//! [`esker_client::retry::may_ask_again`] exists for it and says why — and a write stays
//! `AmbiguousResult`, which is the caller's decision and not this predicate's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "chaos_cluster/mod.rs"]
mod chaos_cluster;

use std::time::{Duration, Instant};

use chaos_cluster::{Cluster, connect};

/// What the coordinator's acceptance asks for: a statement that succeeds *within* this, rather
/// than being refused at once.
///
/// Generous against the cluster's own election — `raft.tick` is 25 ms here, so 10–20 ticks is
/// 250–500 ms plus a pre-vote round — and far under the 10 s a call may take in total
/// (`esker_client::retry::CALL_TIMEOUT_MS`). What it is bounding is the difference between
/// *waiting for an election* and *not waiting at all*.
const WITHIN: Duration = Duration::from_secs(3);

/// **The acceptance.** Three real stores, a long-lived client, and the store that leads taken away
/// underneath a write.
#[test]
fn a_write_outlives_the_store_it_was_routed_to() {
    let cluster = Cluster::start(3);
    assert!(
        cluster.settle(Duration::from_secs(10)),
        "the cluster never elected a leader to take away"
    );
    let client = connect(&cluster.addrs, Instant::now() + Duration::from_secs(10))
        .expect("the client connects to a cluster that is serving");

    // Warm the route, so the client is demonstrably pointed at the leader when it is killed. A
    // client that had never written would learn the route on the attempt below and never meet the
    // failure this is about.
    client
        .put(b"before", b"1")
        .expect("a healthy cluster writes");

    let leader = cluster.leader().expect("somebody leads");
    cluster.kill_node(leader);

    // **One write, and the clock.** Not a loop: the caller gets one answer, and the question is
    // whether that answer is an error handed back in microseconds or a success the client worked
    // for. Rails' fixture setup does not retry, which is why the node has to.
    let began = Instant::now();
    let wrote = client.put(b"after", b"2");
    let took = began.elapsed();

    wrote.unwrap_or_else(|error| {
        panic!(
            "the write was refused {} ms after the store it was routed to was killed, with two \
             live replicas and an election already under way: {error}. This is run 124's 184 \
             refusals in 0.695 s — a client that surfaces `NotSent` instead of re-routing spends \
             no budget at all.",
            took.as_millis()
        )
    });
    assert!(
        took < WITHIN,
        "the write succeeded but took {took:?}, past the {WITHIN:?} a statement may spend on a \
         leader kill"
    );

    // And the cluster really did lose its leader — otherwise this test passes on a kill that took
    // a follower, which is the arithmetic run 123 got wrong and run 124 fixed.
    assert!(
        cluster.leader() != Some(leader),
        "the killed node still leads, so nothing was taken away"
    );

    cluster.shutdown();
}

/// The other side of the same rule, and the reason the write above is allowed to be re-sent: a
/// **read** whose answer was lost is re-asked, and that behaviour is unchanged by this — it was
/// already right, through `may_ask_again`, and is asserted here so that widening the retryable set
/// cannot quietly take it over.
#[test]
fn a_read_still_answers_after_the_leader_is_taken_away() {
    let cluster = Cluster::start(3);
    assert!(cluster.settle(Duration::from_secs(10)), "no leader");
    let client = connect(&cluster.addrs, Instant::now() + Duration::from_secs(10))
        .expect("the client connects");
    client.put(b"k", b"v").expect("a healthy cluster writes");

    let leader = cluster.leader().expect("somebody leads");
    let killed = cluster.addrs[leader];
    cluster.kill_node(leader);

    let began = Instant::now();
    let read = client.get(b"k");
    let took = began.elapsed();
    // **Sampled after the read, because the failure's whole question is which store the client
    // was still dialling.** The error names an address and a store id; on its own that does not
    // say whether it is the corpse this test just made or a live node that had not yet been
    // routed to, and those are two different bugs in two different places. So the panic carries
    // the answer rather than leaving the next reader to work it out from a port number.
    let leads_now = cluster.leader();
    assert_eq!(
        read.unwrap_or_else(|error| panic!(
            "the read was refused after {took:?}.\n  killed: node {leader} = store {} at \
             {killed}\n  leads now: {}\n  error: {error}",
            leader + 1,
            leads_now.map_or_else(
                || "nobody yet".to_owned(),
                |at| format!("node {at} = store {} at {}", at + 1, cluster.addrs[at])
            ),
        )),
        Some(bytes::Bytes::from_static(b"v")),
        "the value written before the kill did not survive it"
    );
    assert!(took < WITHIN, "the read took {took:?}");

    cluster.shutdown();
}
