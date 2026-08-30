//! What a run says about itself: the compact event trace, and the shape of a failure.
//!
//! A sweep that fails has to hand back enough to start from and no more. That is the seed —
//! printed as `ESKER_SIM_SEED=n`, which is the form the environment variable takes, so the line
//! can be pasted back — the event it failed at, what the violation was, and the last hundred
//! events. Not ten thousand events: the ones that mattered.

use std::fmt;

use esker_raft::{Index, NodeId as RaftId, Term};
use thiserror::Error;

use crate::clock::Millis;

use super::checkers::Violation;

/// One thing the event loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Every online node was ticked.
    Tick {
        /// Logical time after the tick.
        at: Millis,
    },
    /// One message was taken out of an inbox and stepped into its recipient.
    Deliver {
        /// Sender.
        from: RaftId,
        /// Recipient.
        to: RaftId,
        /// Which kind of message.
        kind: &'static str,
        /// Its term.
        term: Term,
        /// Whether it was thrown away because the recipient was dead.
        lost: bool,
    },
    /// A proposal was offered to a node.
    Propose {
        /// Which node.
        node: RaftId,
        /// Whether it took it. A follower does not.
        accepted: bool,
    },
    /// A node was killed.
    Crash {
        /// Which node.
        node: RaftId,
    },
    /// A node was rebuilt from what it had made durable.
    Restart {
        /// Which node.
        node: RaftId,
    },
    /// The cluster was cut in two.
    Partition {
        /// One side of the cut.
        side: Vec<RaftId>,
    },
    /// Every link came back.
    Heal,
    /// A disk write completed.
    Persist {
        /// Which node.
        node: RaftId,
        /// How many entries it wrote.
        entries: usize,
        /// The highest index it wrote.
        upto: Index,
        /// How many events the write was held for.
        held: u64,
    },
    /// A node handed messages to the network.
    Emit {
        /// Which node.
        node: RaftId,
        /// How many.
        messages: usize,
        /// Whether they went out before the write that should have preceded them.
        early: bool,
    },
    /// A `Ready` was thrown away without being discharged — the driver bug this harness has a
    /// switch for, so the rule that forbids it can be shown red.
    Discarded {
        /// Which node's `Ready`.
        node: RaftId,
        /// How many messages went with it.
        messages: usize,
    },
    /// A node answered a `ReadIndex`.
    Read {
        /// Which node.
        node: RaftId,
        /// The index the read may be served at.
        index: Index,
    },
    /// A node adopted a leader's snapshot.
    Installed {
        /// Which node.
        node: RaftId,
        /// The index the snapshot covers.
        through: Index,
        /// The term at that index.
        term: Term,
    },
    /// A node folded applied entries into its snapshot and deleted them.
    Compact {
        /// Which node.
        node: RaftId,
        /// The new compaction boundary.
        through: Index,
    },
    /// A node's state machine consumed entries.
    Apply {
        /// Which node.
        node: RaftId,
        /// The index it has now applied up to.
        upto: Index,
    },
    /// A node became the leader of a term.
    Lead {
        /// Which node.
        node: RaftId,
        /// Which term.
        term: Term,
    },
    /// A node's core rejected a message. Not a failure by itself; a count that will not go down
    /// is worth looking at.
    Rejected {
        /// Which node.
        node: RaftId,
        /// What it said.
        reason: String,
    },
}

impl fmt::Display for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Tick { at } => write!(formatter, "tick                     {at}"),
            Event::Deliver {
                from,
                to,
                kind,
                term,
                lost,
            } => write!(
                formatter,
                "deliver  {from} -> {to}  {kind}(t{term}){}",
                if *lost { "  LOST (node down)" } else { "" }
            ),
            Event::Propose { node, accepted } => write!(
                formatter,
                "propose  n{node}{}",
                if *accepted { "" } else { "  refused" }
            ),
            Event::Crash { node } => write!(formatter, "CRASH    n{node}"),
            Event::Restart { node } => write!(formatter, "restart  n{node}"),
            Event::Partition { side } => write!(formatter, "PARTITION {side:?} | rest"),
            Event::Heal => write!(formatter, "heal     every link"),
            Event::Persist {
                node,
                entries,
                upto,
                held,
            } => write!(
                formatter,
                "persist  n{node}  {entries} entries, through {upto}{}",
                if *held > 0 {
                    format!("  (disk held {held} events)")
                } else {
                    String::new()
                }
            ),
            Event::Emit {
                node,
                messages,
                early,
            } => write!(
                formatter,
                "send     n{node}  {messages} messages{}",
                if *early { "  BEFORE PERSISTING" } else { "" }
            ),
            Event::Installed {
                node,
                through,
                term,
            } => write!(
                formatter,
                "SNAPSHOT n{node}  installed through {through} (term {term})"
            ),
            Event::Read { node, index } => {
                write!(formatter, "read     n{node}  answered at index {index}")
            }
            Event::Discarded { node, messages } => write!(
                formatter,
                "DISCARD  n{node}  a taken Ready thrown away, {messages} messages lost with it"
            ),
            Event::Compact { node, through } => {
                write!(
                    formatter,
                    "compact  n{node}  log now starts after {through}"
                )
            }
            Event::Apply { node, upto } => write!(formatter, "apply    n{node}  through {upto}"),
            Event::Lead { node, term } => write!(formatter, "LEADER   n{node} of term {term}"),
            Event::Rejected { node, reason } => {
                write!(formatter, "rejected n{node}  {reason}")
            }
        }
    }
}

/// A run that ended badly. Every variant prints the seed first, in the form the environment
/// variable takes, so a failing line can be pasted back to reproduce it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Failure {
    /// A safety property does not hold.
    #[error("ESKER_SIM_SEED={seed}  event {event}\n  VIOLATION: {violation}\n{trace}")]
    Safety {
        /// The seed that produced it.
        seed: u64,
        /// The event it was found at.
        event: u64,
        /// Which property, and how.
        violation: Violation,
        /// The last events, most recent last.
        trace: String,
    },
    /// The cluster failed to make progress after everything was healed.
    #[error("ESKER_SIM_SEED={seed}  event {event}\n  NO PROGRESS: {what}\n{trace}")]
    Liveness {
        /// The seed that produced it.
        seed: u64,
        /// The event it gave up at.
        event: u64,
        /// What did not happen.
        what: String,
        /// The last events, most recent last.
        trace: String,
    },
    /// The harness itself could not do something it should have been able to.
    #[error("ESKER_SIM_SEED={seed}  event {event}\n  DRIVER: {what}\n{trace}")]
    Driver {
        /// The seed that produced it.
        seed: u64,
        /// The event it happened at.
        event: u64,
        /// What went wrong.
        what: String,
        /// The last events, most recent last.
        trace: String,
    },
}

/// What a settled cluster looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// How many tick rounds it took.
    pub ticks: u64,
    /// Who leads.
    pub leader: RaftId,
    /// The index the proposal committed at.
    pub index: Index,
}

/// Counters worth asserting on: a sweep that injected no faults proves nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events run.
    pub events: u64,
    /// Leaders elected, counting each new term once.
    pub elections: u64,
    /// Nodes killed.
    pub crashes: u64,
    /// Nodes brought back.
    pub restarts: u64,
    /// Times the cluster was cut.
    pub partitions: u64,
    /// Disk writes that were held for at least one event.
    pub slow_writes: u64,
    /// Messages handed to the network.
    pub sent: u64,
    /// Messages stepped into a node.
    pub delivered: u64,
    /// Proposals a node accepted.
    pub proposals: u64,
    /// Messages a core refused.
    pub rejected: u64,
    /// The highest index any node was observed to commit.
    ///
    /// A sweep asserts this is non-zero, and that assertion earns its keep: the committed
    /// record and leader completeness are only evaluated on a node whose disk is idle, so a
    /// change that left every node permanently unsettled would switch two of the four
    /// properties off without failing anything. This is what would notice.
    pub committed: Index,
    /// The highest index any state machine has applied.
    pub applied: Index,
    /// `Ready`s taken from a core. Each one must be discharged or die with its node: a
    /// `Ready`'s messages are *taken*, not re-offered, so a driver that inspects one and throws
    /// it away silently loses them (`docs/plans/phase-3.md` §10.2).
    pub readys_taken: u64,
    /// `Ready`s fully discharged: persisted, sent, applied, advanced.
    pub readys_discharged: u64,
    /// `Ready`s that died with the node holding them. That is not a lost `Ready` — the process
    /// that would have sent the messages no longer exists.
    pub readys_lost_to_crash: u64,
    /// Messages delivered a second or later time, because the plan duplicated them.
    pub duplicate_deliveries: u64,
    /// Deliveries carrying a non-empty `context` — the field `ReadIndex` rides on, which the
    /// fault model has to carry verbatim through a duplicate or a reorder.
    pub contexts_delivered: u64,
    /// `ReadIndex` requests answered.
    pub reads_served: u64,
    /// Snapshots a follower adopted from a leader.
    pub snapshots_installed: u64,
    /// Times a node folded applied entries into its snapshot.
    pub compactions: u64,
}
