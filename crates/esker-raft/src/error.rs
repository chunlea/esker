//! What can go wrong in a state machine that does no I/O.
//!
//! Two kinds of failure live here, and they are worth telling apart. The first is a
//! [`LogStorage`](crate::LogStorage) answering "I cannot give you that": the index is below the
//! compaction boundary, or above what has been written. Those are *normal* — a leader learns a
//! follower needs a snapshot precisely by asking for a compacted index — and the core handles
//! them rather than propagating them. The second is a caller's mistake: proposing to a node that
//! is not the leader, or a second configuration change while one is pending.
//!
//! What is *not* here is an error for a strange message. `step()` never fails on the content of a
//! message (`CLAUDE.md` invariant 9): a stale term, an unknown sender, a vote from a peer that was
//! removed — each is ignored on purpose, because a Raft node cannot control what the network hands
//! it and must not die when the answer is "that is no longer relevant".

use crate::types::{Index, NodeId};

/// The crate's result type.
pub type Result<T> = core::result::Result<T, RaftError>;

/// A failure from the Raft core or from the log storage behind it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RaftError {
    /// The requested index is at or below the compaction boundary; the entries are gone and only
    /// a snapshot can carry that part of the log. The leader turns this into an `InstallSnapshot`.
    #[error("log entries at or below index {0} have been compacted")]
    Compacted(Index),

    /// The requested index is past the end of the log. Either the caller asked too early or the
    /// storage is behind what the core believes it wrote.
    #[error("log index {0} is not available")]
    Unavailable(Index),

    /// Storage cannot produce a snapshot right now (one is being built). The leader retries later
    /// rather than treating the follower as unreachable.
    #[error("snapshot is temporarily unavailable")]
    SnapshotTemporarilyUnavailable,

    /// A snapshot arrived that the log has already moved past. Ignoring it is correct; the error
    /// exists so storage implementations can say so rather than silently rewinding.
    #[error(
        "snapshot at index {index} is out of date; the log is already committed to {committed}"
    )]
    SnapshotOutOfDate {
        /// Index of the snapshot that was offered.
        index: Index,
        /// The commit index that already covers it.
        committed: Index,
    },

    /// A proposal reached a node that cannot order it. The caller should redirect to
    /// [`RawNode::leader`](crate::RawNode::leader), if it knows one.
    #[error("not the leader")]
    NotLeader,

    /// A configuration change was proposed while one may still be uncommitted — either one this
    /// node appended, or anything in the tail a new leader inherited and cannot yet judge.
    /// Single-server changes are only safe one at a time (dissertation §4.1), so the core refuses
    /// rather than queueing — the caller knows better than this layer whether to retry or give up.
    /// The index is the one that has to commit before another change may be proposed.
    #[error("a configuration change may be pending at or below index {0}")]
    ConfChangePending(Index),

    /// A proposal arrived while leadership is being transferred away. The outgoing leader stops
    /// accepting work the moment it sends `TimeoutNow`, so that no proposal is left unowned.
    #[error("leadership transfer to {0} is in progress")]
    LeadershipTransferInProgress(NodeId),

    /// [`Config::validate`](crate::Config::validate) rejected the configuration.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A `ConfChange` entry's payload could not be decoded. On-disk bytes are never trusted
    /// (`CLAUDE.md` invariant 2): corruption is an error value, never a panic.
    #[error("corrupt conf change payload: {0}")]
    CorruptConfChange(String),

    /// The storage implementation failed for a reason of its own — a read error, a poisoned lock.
    /// Kept as text because the core cannot act on the detail, only report it.
    #[error("storage: {0}")]
    Storage(String),
}

impl RaftError {
    /// Whether this is storage saying "ask a snapshot for that part of the log". The leader's
    /// replication path turns exactly this into an `InstallSnapshot`, so it gets a name.
    pub fn is_compacted(&self) -> bool {
        matches!(self, Self::Compacted(_))
    }
}
