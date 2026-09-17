//! The in-crate property test: no schedule of ticks, deliveries, drops and duplicates ever
//! produces two leaders in one term.
//!
//! This is the fast version of what `esker-sim` does properly. The sibling lane's simulator has a
//! clock, a fault plan, crash-restart over persisted storage and ten thousand seeds; this runs in
//! milliseconds inside `cargo test`, so a change that breaks Election Safety fails at the same
//! moment it is written rather than in a nightly job.
//!
//! Three properties are checked after **every** action, not at the end. A run that ends healthy
//! can still have violated safety in the middle, and a check that only looks at the final state
//! would not know:
//!
//! * **Election Safety** — at most one leader per term, over the whole history. A node can take
//!   office, step down, and be followed by a different node in the same term; only a record of
//!   what has happened catches that, which is why the harness keeps one.
//! * **Log Matching** — two logs holding an entry with the same index and term are identical up to
//!   it.
//! * **State Machine Safety** — everything below two nodes' commit indices is byte-identical.
//!   Those entries have been handed to a state machine and cannot be taken back.
//! * **No two nodes name each other as leader**, which is Election Safety seen from the client's
//!   side rather than a fourth independent claim. `debts-v1.1.md` #107 built a client fixture on
//!   exactly that input — two live peers whose `NotLeader` hints point at each other, which costs
//!   a call its whole retry budget — and the input is unreachable here: `Raft::reset` clears
//!   `leader` on every term change (`core.rs:297`), so a node's belief is only ever about its
//!   *current* term. `P` naming `Q` and `Q` naming `P` therefore needs either one term with two
//!   leaders, or each node to have led a term it never reached. The proof is two lines of code;
//!   this is the machine-checked half of it, and it is here rather than in a test of its own
//!   because a schedule is what would find the exception.
//!
//!   **The proof is universal and this check is a sample of it**, which is the honest way round.
//!   Two lines of `core.rs` settle it for every execution; what runs here is three voters and a
//!   learner over `testkit`'s schedule space — no delay, no crash-restart — so a green run adds
//!   confidence rather than the conclusion. It is worth having because the exception, if there is
//!   one, is a schedule, and this is the only thing in the crate that searches schedules.
//!
//!   **No mutation has yet been found that makes this property fail, and that is recorded rather
//!   than hidden.** Three were tried: removing `reset`'s `self.leader = None`, making the
//!   higher-term path keep the old belief instead of `become_follower(term, None)`, and both at
//!   once — each stayed green, the first also over 20,000 cases. The likely reading is that the
//!   invariant is over-determined: several sites clear the belief and a single-site mutation is
//!   caught by the next one. The other reading is that this check is weaker than it looks. Until
//!   a mutation reddens it, treat it as the invariant written down with a search behind it, not
//!   as a guard proven able to fail.
//!
//! The network here loses, reorders and duplicates messages, which is all a real one does to a
//! correct Raft. What it does not do is crash and restart a node — that needs the storage to be
//! detached and reattached, and it is the simulator's.

use proptest::prelude::*;

use crate::testkit::Harness;
use crate::types::NodeId;

/// One thing a schedule can do. Indices are taken modulo whatever they address, so every generated
/// value is meaningful and proptest never wastes a case on an out-of-range number.
#[derive(Debug, Clone, Copy)]
enum Action {
    /// One tick on one node — the only way time passes.
    Tick(u8),
    /// A tick on every node at once.
    TickAll,
    /// Force a node to stand for election, whatever its timer says.
    Campaign(u8),
    /// Propose on a node; anywhere but the leader this is a no-op, which is the point.
    Propose(u8),
    /// Deliver one queued message, chosen by index — so messages arrive out of order.
    Deliver(u8),
    /// Lose one queued message.
    Drop(u8),
    /// Deliver one queued message and leave it queued, so it arrives twice.
    Duplicate(u8),
    /// Cut every link in and out of a node.
    Isolate(u8),
    /// Restore every link.
    Heal,
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        6 => any::<u8>().prop_map(Action::Tick),
        4 => Just(Action::TickAll),
        2 => any::<u8>().prop_map(Action::Campaign),
        3 => any::<u8>().prop_map(Action::Propose),
        10 => any::<u8>().prop_map(Action::Deliver),
        2 => any::<u8>().prop_map(Action::Drop),
        2 => any::<u8>().prop_map(Action::Duplicate),
        1 => any::<u8>().prop_map(Action::Isolate),
        1 => Just(Action::Heal),
    ]
}

fn run(ids: &[NodeId], seed: u64, actions: &[Action]) -> Result<(), TestCaseError> {
    run_group(Harness::new(ids, seed), ids, &[], actions)
}

/// The same schedule over a group that has a **learner** in it.
///
/// Every checker this project had modelled voters only — the exhaustive model next door, the
/// simulator's five nodes, and the three properties below — so none of them could have found
/// [ADR 0085](../../../docs/adr/0085-a-vote-is-not-granted-to-a-learner.md)'s defect, and none did:
/// a learner whose own configuration wrongly had it as a voter campaigned, was granted votes by a
/// voter that was between leaders, and kept a region leaderless for as long as the load lasted.
/// A property cannot fail on a shape it never builds.
fn run_with_learner(
    voters: &[NodeId],
    learners: &[NodeId],
    seed: u64,
    actions: &[Action],
) -> Result<(), TestCaseError> {
    let ids: Vec<NodeId> = voters.iter().chain(learners).copied().collect();
    // **The confused shape, not the tidy one.** A learner whose own configuration is correct is
    // stopped from campaigning by `Raft::campaign`'s guard, so it never asks for a vote and a
    // property about granting one can never fail — the first version of this was exactly that,
    // and it passed against the code the defect was in. The shape from the field is a peer that
    // believes itself a voter while everyone else holds it as a learner.
    let [confused] = learners else {
        return Err(TestCaseError::fail(
            "one learner, which is the shape from the field",
        ));
    };
    run_group(
        Harness::with_confused_learner(voters, *confused, seed),
        &ids,
        learners,
        actions,
    )
}

/// The first pair naming each other as leader, which should always be `None`.
///
/// A pair and not a single node: a node naming *itself* is what a leader does, and the store's
/// hint filters that out before a client ever sees it
/// (`esker-store/src/peer.rs:1061`, `filter(|id| *id != self.peer_id)`).
fn naming_each_other(group: &Harness, ids: &[NodeId]) -> Option<(NodeId, NodeId)> {
    let believes = |who: NodeId| group.node(who).leader();
    ids.iter().enumerate().find_map(|(at, one)| {
        ids[at + 1..]
            .iter()
            .find(|other| believes(*one) == Some(**other) && believes(**other) == Some(*one))
            .map(|other| (*one, *other))
    })
}

fn run_group(
    mut group: Harness,
    ids: &[NodeId],
    learners: &[NodeId],
    actions: &[Action],
) -> Result<(), TestCaseError> {
    for (at, action) in actions.iter().enumerate() {
        let node = |index: u8| ids[usize::from(index) % ids.len()];
        match *action {
            Action::Tick(index) => group.tick(node(index), 1),
            Action::TickAll => group.tick_all(),
            Action::Campaign(index) => group.campaign(node(index)),
            Action::Propose(index) => {
                // Only the leader accepts one; everywhere else the error is the answer.
                let _ = group
                    .node_mut(node(index))
                    .propose(bytes::Bytes::from_static(b"p"));
            }
            Action::Deliver(index) => {
                group.drain_ready();
                let pending = group.pending();
                if pending > 0 {
                    group.deliver_one(usize::from(index) % pending);
                }
            }
            Action::Drop(index) => {
                group.drain_ready();
                let pending = group.pending();
                if pending > 0 {
                    group.drop_one(usize::from(index) % pending);
                }
            }
            Action::Duplicate(index) => {
                group.drain_ready();
                let pending = group.pending();
                if pending > 0 {
                    group.duplicate_one(usize::from(index) % pending);
                }
            }
            Action::Isolate(index) => group.isolate(node(index)),
            Action::Heal => group.heal(),
        }
        group.drain_ready();

        if let Err(violation) = group.check_election_safety() {
            return Err(TestCaseError::fail(format!(
                "after action {at} ({action:?}): {violation}"
            )));
        }
        if let Err(violation) = group.check_log_matching() {
            return Err(TestCaseError::fail(format!(
                "after action {at} ({action:?}): {violation}"
            )));
        }
        // **A node outside the voter set is never granted a vote** (ADR 0085). It cannot reach a
        // quorum, so a grant buys nothing and costs the granter its vote for the term and, for a
        // real vote, its leader — which is a loop that does not end, because the next round starts
        // from the same place.
        // **Nobody names the peer that names them** (`debts-v1.1.md` #107). Checked after every
        // action rather than at the end, for the reason the header gives: a schedule that ends
        // healthy can pass through the state this rules out.
        if let Some((one, other)) = naming_each_other(&group, ids) {
            return Err(TestCaseError::fail(format!(
                "after action {at} ({action:?}): node {one} names {other} as leader while {other} \
                 names {one} — each belief is about its own term, so this needs two leaders in one \
                 term or a node that led a term it never reached"
            )));
        }
        for message in group.pending_messages() {
            if let crate::message::Message::RequestVoteResponse {
                from,
                to,
                granted: true,
                ..
            } = message
                && learners.contains(to)
            {
                return Err(TestCaseError::fail(format!(
                    "after action {at} ({action:?}): node {from} granted a vote to {to}, which \
                     this configuration holds as a learner"
                )));
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// **Three voters and a learner**, which is the shape every other property here cannot build.
    ///
    /// The learner is in each node's `ConfState`. What is asserted beyond the two safety checks is
    /// ADR 0085's rule: no schedule of ticks, campaigns, deliveries, drops and duplicates ever has
    /// a voter grant it a vote.
    #[test]
    fn a_learner_is_never_granted_a_vote(
        seed in any::<u64>(),
        actions in prop::collection::vec(action(), 1..48),
    ) {
        run_with_learner(&[1, 2, 3], &[4], seed, &actions)?;
    }

    /// Three voters: the smallest group where a split vote is possible.
    #[test]
    fn three_nodes_never_have_two_leaders_in_one_term(
        seed in any::<u64>(),
        actions in prop::collection::vec(action(), 1..160),
    ) {
        run(&[1, 2, 3], seed, &actions)?;
    }

    /// Five voters, where a minority can be two nodes and a partition can be even.
    #[test]
    fn five_nodes_never_have_two_leaders_in_one_term(
        seed in any::<u64>(),
        actions in prop::collection::vec(action(), 1..160),
    ) {
        run(&[1, 2, 3, 4, 5], seed, &actions)?;
    }

    /// Four voters: an even group, where a two-two split has no majority and the cluster must
    /// simply wait rather than electing two leaders to break the tie.
    #[test]
    fn an_even_group_never_breaks_a_tie_by_electing_twice(
        seed in any::<u64>(),
        actions in prop::collection::vec(action(), 1..160),
    ) {
        run(&[1, 2, 3, 4], seed, &actions)?;
    }
}
