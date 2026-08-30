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
    let mut group = Harness::new(ids, seed);
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
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

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
