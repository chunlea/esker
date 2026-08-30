//! What a checker looks at, and what it says when something is wrong.
//!
//! The observation ([`NodeSnapshot`]) is deliberately not a `RawNode`: it is an identifier, a
//! role, a term, a commit index, a membership, a log of digests, and what the state machine has
//! consumed. That is what makes every property testable against a hand-written violation, which
//! is the only way to know a checker works.

use esker_base::hash::hash64;
use esker_raft::{ConfState, Index, NodeId, Term};
use thiserror::Error;

/// One log entry as a checker sees it: enough to tell two entries apart, and nothing about
/// what is inside them (`CLAUDE.md` invariant 7 — the sim is byte-opaque too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryDigest {
    /// Its position in the log.
    pub index: Index,
    /// The term of the leader that created it.
    pub term: Term,
    /// A hash of its payload, so that two entries with the same index and term but different
    /// contents are distinguishable.
    pub payload: u64,
}

impl EntryDigest {
    /// A digest of `data` at `index` in `term`.
    #[must_use]
    pub fn of(index: Index, term: Term, data: &[u8]) -> Self {
        Self {
            index,
            term,
            payload: hash64(data),
        }
    }
}

/// What one node looked like at one instant.
///
/// Borrowed rather than owned: this is built after every event, and copying a log per node per
/// event would dominate the run.
#[derive(Debug, Clone, Copy)]
pub struct NodeSnapshot<'a> {
    /// Which node.
    pub id: NodeId,
    /// Whether it is running. A crashed node is still checked against the record of what it
    /// did before it died, but contributes nothing new.
    pub online: bool,
    /// Whether it believes it is the leader of [`NodeSnapshot::term`].
    pub is_leader: bool,
    /// The term it is in.
    pub term: Term,
    /// Its commit index.
    pub commit: Index,
    /// The last index covered by an installed snapshot; entries at or below it are absent from
    /// [`NodeSnapshot::log`] but are still present in the node's state.
    pub compacted_through: Index,
    /// The term the snapshot's metadata claims for the entry at
    /// [`NodeSnapshot::compacted_through`]. A snapshot that disagrees with what was committed
    /// there is a violation in its own right: it is the one part of a node's state the log
    /// cannot be checked against, so its metadata is.
    pub snapshot_term: Term,
    /// The prefix digest at [`NodeSnapshot::compacted_through`] — `0` for a log that has never
    /// been compacted. A driver that compacts carries the digest forward; a driver that
    /// installs a snapshot sent by a leader seeds it from
    /// [`super::SafetyChecker::prefix_digest`].
    pub prefix_anchor: u64,
    /// The membership the *driver* has derived from the log, which is what a restart would
    /// recover. Independent of [`NodeSnapshot::core_config`] on purpose: two derivations of one
    /// fact, and a check that they agree.
    pub config: &'a ConfState,
    /// The index of the last conf-change entry folded into [`NodeSnapshot::config`].
    pub config_index: Index,
    /// The configuration after each conf-change entry in this node's log, in log order. The
    /// single-server rule is checked inside this, where there are no branches to confuse it.
    pub lineage: &'a [(EntryDigest, ConfState)],
    /// The configuration in force before the first entry of [`NodeSnapshot::lineage`].
    pub base_config: &'a ConfState,
    /// What the core itself believes the membership is.
    pub core_config: &'a ConfState,
    /// Whether [`NodeSnapshot::config`] and [`NodeSnapshot::core_config`] were derived from the
    /// same base, and so can be compared at all: true while the node has never restarted and
    /// nothing has been folded into a snapshot.
    pub comparable_config: bool,
    /// The node's log, ascending and contiguous, starting at `compacted_through + 1`.
    pub log: &'a [EntryDigest],
    /// Everything the node's state machine has consumed, in the order it consumed it.
    pub applied: &'a [EntryDigest],
}

/// A safety property that does not hold.
///
/// Every variant names the nodes and indices involved, because the message is what a failing
/// seed sweep prints and it has to be enough to start debugging from.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Violation {
    /// Two different nodes were leaders of the same term.
    #[error("election safety: term {term} had two leaders, node {first} and node {second}")]
    ElectionSafety {
        /// The term with two leaders.
        term: Term,
        /// The node recorded first.
        first: NodeId,
        /// The node that also claimed it.
        second: NodeId,
    },
    /// Two logs hold the same index and term but disagree somewhere at or below it.
    #[error(
        "log matching: node {node} has (index {index}, term {term}) with prefix digest \
         {found:#018x}, but node {other} had the same index and term with {expected:#018x} — \
         the two logs differ at or below index {index}"
    )]
    LogMatching {
        /// The index they disagree about.
        index: Index,
        /// The term both entries claim.
        term: Term,
        /// The node observed now.
        node: NodeId,
        /// The node that established the record.
        other: NodeId,
        /// This node's prefix digest.
        found: u64,
        /// The recorded one.
        expected: u64,
    },
    /// A node became leader without an entry that a previous term had already committed.
    #[error(
        "leader completeness: node {leader} leads term {leader_term} but its log lacks the \
         entry committed at index {index} in term {entry_term} (observed committed in term \
         {committed_in}); its log holds {found}"
    )]
    LeaderCompleteness {
        /// The offending leader.
        leader: NodeId,
        /// Its term.
        leader_term: Term,
        /// The index of the missing entry.
        index: Index,
        /// The term of the missing entry.
        entry_term: Term,
        /// The term in which the entry was observed committed.
        committed_in: Term,
        /// What the leader has at that index instead.
        found: String,
    },
    /// Two different entries were committed at one index.
    #[error(
        "committed twice: node {node} has index {index} committed as term {term}, payload \
         {payload:#018x}, but node {other} committed term {other_term}, payload \
         {other_payload:#018x} there"
    )]
    CommittedTwice {
        /// The index committed twice.
        index: Index,
        /// The node observed now.
        node: NodeId,
        /// Its entry's term.
        term: Term,
        /// Its entry's payload digest.
        payload: u64,
        /// The node that established the record.
        other: NodeId,
        /// The recorded term.
        other_term: Term,
        /// The recorded payload digest.
        other_payload: u64,
    },
    /// Two nodes applied different entries at the same position.
    #[error(
        "state machine safety: node {node} applied (index {index}, term {term}, \
         payload {payload:#018x}) where node {other} applied term {other_term}, payload \
         {other_payload:#018x}"
    )]
    StateMachineSafety {
        /// The index they disagree about.
        index: Index,
        /// The node observed now.
        node: NodeId,
        /// Its entry's term.
        term: Term,
        /// Its entry's payload digest.
        payload: u64,
        /// The node that established the record.
        other: NodeId,
        /// The recorded term.
        other_term: Term,
        /// The recorded payload digest.
        other_payload: u64,
    },
    /// A node's state machine skipped an index or went backwards.
    #[error(
        "state machine safety: node {node} applied index {got} straight after index \
         {previous} (nothing covers the gap)"
    )]
    ApplyOutOfOrder {
        /// The node.
        node: NodeId,
        /// The last index it had applied.
        previous: Index,
        /// The index it applied next.
        got: Index,
    },
    /// A snapshot claims a term that disagrees with what was committed at that index.
    #[error(
        "snapshot metadata: node {node} has a snapshot through index {index} claiming term \
         {claimed}, but term {committed} was committed there"
    )]
    SnapshotMismatch {
        /// The node.
        node: NodeId,
        /// The snapshot's last index.
        index: Index,
        /// What the metadata says.
        claimed: Term,
        /// What was committed.
        committed: Term,
    },
    /// A `ReadIndex` was answered with an index no node has ever committed.
    ///
    /// Note what this does *not* say. A follower's read index is the *leader's* commit index,
    /// so it is routinely ahead of the follower's own — that is the whole point, and it is why
    /// the driver contract says to answer a read only once the state machine has applied the
    /// index. What must never happen is a read index beyond anything the cluster committed at
    /// all: a read served there would return state no quorum agreed on.
    #[error(
        "read index: node {node} answered a ReadIndex with index {index}, but no node has \
         committed past {high_water}"
    )]
    ReadIndexBeyondCommit {
        /// The node that answered.
        node: NodeId,
        /// The index it answered with.
        index: Index,
        /// The highest commit index any node has reported, at any point in the run.
        high_water: Index,
    },
    /// The driver's configuration and the core's disagree.
    ///
    /// Two derivations of one fact, compared — but only where they are derived the same way.
    /// The core folds changes forward from the configuration it was *built* with and reverts
    /// them on a truncation; the driver refolds the log from the base that goes with it. Those
    /// agree exactly while a node has never restarted and never compacted. Once it has, the
    /// bases differ — the core's is whatever storage held when it started, the driver's is the
    /// snapshot's — and comparing them stops being a property of the system and starts being a
    /// property of the two definitions. So [`NodeSnapshot::comparable_config`] says when the
    /// comparison means something, and the check is skipped when it does not.
    #[error(
        "membership: node {node} has derived voters {driver:?} from its log, but its core \
         believes {core:?}"
    )]
    ConfigDisagreesWithCore {
        /// The node.
        node: NodeId,
        /// What the driver derived.
        driver: Vec<NodeId>,
        /// What the core believes.
        core: Vec<NodeId>,
    },
    /// Two nodes with the same log prefix derived different configurations from it.
    #[error(
        "membership: node {node} and node {other} agree on the log through index {index} but \
         derived different configurations from it: {config:?} against {recorded:?}"
    )]
    ConfigDivergence {
        /// The conf-change index they agree on.
        index: Index,
        /// The node observed now.
        node: NodeId,
        /// The node that established the record.
        other: NodeId,
        /// This node's voters.
        config: Vec<NodeId>,
        /// The recorded voters.
        recorded: Vec<NodeId>,
    },
    /// Two consecutive configurations differ by more than one server.
    ///
    /// Single-server change is what makes every pair of consecutive configurations share a
    /// quorum without joint consensus (dissertation §4.1). Two servers moving at once is how
    /// a cluster ends up with two disjoint majorities and two leaders.
    #[error(
        "membership: node {node}'s configuration moved {moved} servers in one change, from \
         {from:?} to {to:?}, at log index {to_index}"
    )]
    ConfigJumped {
        /// The node whose lineage jumped.
        node: NodeId,
        /// The index of the entry that made the jump.
        to_index: Index,
        /// The earlier members.
        from: Vec<NodeId>,
        /// The later members.
        to: Vec<NodeId>,
        /// How many servers changed status.
        moved: usize,
    },
    /// A node won an election while its own configuration did not list it as a voter.
    #[error(
        "membership: node {node} became leader of term {term} without being a voter in {config:?}"
    )]
    NonVoterElected {
        /// The node.
        node: NodeId,
        /// The term it won.
        term: Term,
        /// Its own voter set.
        config: Vec<NodeId>,
    },
    /// A committed entry was never held by a quorum of the configuration in force at its index.
    #[error(
        "quorum: index {index} was committed, but only {holders} of {voters:?} ever held it — \
         a quorum there is {quorum}"
    )]
    CommittedWithoutQuorum {
        /// The index.
        index: Index,
        /// How many nodes were ever seen holding the recorded entry.
        holders: usize,
        /// The voters of the configuration in force at that index.
        voters: Vec<NodeId>,
        /// How many of them a quorum is.
        quorum: usize,
    },
    /// A node reported a log that is not a contiguous ascending run.
    #[error("malformed log: node {node} has index {got} where index {expected} was due")]
    MalformedLog {
        /// The node.
        node: NodeId,
        /// The index the log should have held.
        expected: Index,
        /// The index it held.
        got: Index,
    },
}

impl Violation {
    /// What kind of failure this is, for grouping a sweep's failing seeds.
    ///
    /// A census wants the property, not the instance: two seeds that break Leader Completeness in
    /// different terms are one thing to go and fix, and a sweep that lists thirty seeds without
    /// saying which are the same bug has told the reader nothing they can act on.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::ElectionSafety { .. } => "election safety",
            Self::LogMatching { .. } => "log matching",
            Self::LeaderCompleteness { .. } => "leader completeness",
            Self::CommittedTwice { .. } => "committed twice",
            Self::StateMachineSafety { .. } => "state machine safety",
            Self::ApplyOutOfOrder { .. } => "apply out of order",
            Self::SnapshotMismatch { .. } => "snapshot metadata",
            Self::ReadIndexBeyondCommit { .. } => "read index beyond commit",
            Self::ConfigDisagreesWithCore { .. } => "membership: core against its log",
            Self::ConfigDivergence { .. } => "membership: two nodes, one log",
            Self::ConfigJumped { .. } => "membership: more than one server at a time",
            Self::NonVoterElected { .. } => "a non-voter was elected",
            Self::CommittedWithoutQuorum { .. } => "committed without a quorum",
            Self::MalformedLog { .. } => "malformed log",
        }
    }
}
