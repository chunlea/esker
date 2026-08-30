//! The messages nodes exchange, and the two places this set departs from the paper's.
//!
//! **Heartbeats are `AppendEntries` with no entries.** The paper splits them for exposition; the
//! algorithm does not need the split, and folding them means one consistency check, one carrier
//! for the commit index, and one rejection path instead of two of each
//! (`prompts/03-raft.md` 3a). A heartbeat is then simply the degenerate append: `entries` empty,
//! `prev_log_*` pointing at whatever the leader believes the follower has.
//!
//! **A snapshot is acknowledged with [`Message::AppendEntriesResponse`].** Installing a snapshot
//! moves the follower's log to the snapshot's index, and the acknowledgement answers exactly the
//! question an append response answers: what index does this follower now match? A separate
//! response type would carry the same field and take a second code path to the same place.
//!
//! Every message carries `{ from, to, term }`. The term is what makes Raft work at all — a node
//! that sees a higher term steps down before doing anything else, and a node that sees a lower one
//! answers with its own so the sender learns it is behind.

use bytes::Bytes;

use crate::types::{Entry, Index, NodeId, Snapshot, Term};

/// One message between two members of a Raft group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A candidate asking for a vote (§5.2), or a would-be candidate testing the water (§9.6
    /// pre-vote). The `last_log_*` pair is what the receiver checks its own log against before
    /// granting (§5.4.1): a candidate whose log is not at least as up to date as the voter's must
    /// not win, because a leader is never allowed to overwrite a committed entry.
    RequestVote {
        /// The candidate.
        from: NodeId,
        /// The voter being asked.
        to: NodeId,
        /// The term being contested. For a pre-vote this is the *hypothetical* next term, and the
        /// receiver must not adopt it — that is the entire point of pre-vote.
        term: Term,
        /// Index of the candidate's last log entry.
        last_log_index: Index,
        /// Term of the candidate's last log entry.
        last_log_term: Term,
        /// Whether this is a pre-vote probe rather than a real vote request.
        pre_vote: bool,
        /// Set when the campaign was ordered by [`Message::TimeoutNow`]. It tells a voter to
        /// ignore its leader lease: under check-quorum a follower that has heard from a healthy
        /// leader recently refuses votes, which would otherwise make leadership transfer fail
        /// exactly when it is supposed to work.
        force: bool,
    },

    /// A voter's answer.
    RequestVoteResponse {
        /// The voter.
        from: NodeId,
        /// The candidate.
        to: NodeId,
        /// The term the vote was cast in.
        term: Term,
        /// Whether the vote was granted.
        granted: bool,
        /// Echoes the request's `pre_vote`, so a candidate cannot mistake a pre-vote grant for a
        /// real one when both rounds are in flight.
        pre_vote: bool,
    },

    /// The leader replicating its log, or — with `entries` empty — proving it is still there.
    AppendEntries {
        /// The leader.
        from: NodeId,
        /// The follower.
        to: NodeId,
        /// The leader's term.
        term: Term,
        /// Index of the entry immediately preceding `entries`.
        prev_log_index: Index,
        /// Term of the entry at `prev_log_index`. The follower's log must agree on this pair or it
        /// rejects — this is the consistency check that makes Log Matching hold inductively.
        prev_log_term: Term,
        /// The entries to append; empty for a heartbeat.
        entries: Vec<Entry>,
        /// The leader's commit index, capped at the last index this message leaves the follower
        /// with. A follower must never commit past what it actually has.
        leader_commit: Index,
        /// A `ReadIndex` round's tag, echoed in the response. Empty on an ordinary append.
        context: Bytes,
    },

    /// A follower's answer to an append or a snapshot.
    AppendEntriesResponse {
        /// The follower.
        from: NodeId,
        /// The leader.
        to: NodeId,
        /// The follower's term.
        term: Term,
        /// Whether the consistency check failed.
        reject: bool,
        /// On success, the last index the follower now matches. On rejection, the first index the
        /// leader should try instead — see `hint_term`.
        index: Index,
        /// On rejection, the term the follower has at its end of the conflict. It lets the leader
        /// skip a whole term's worth of entries in one step instead of walking back one index per
        /// round trip, which is the difference between a fast catch-up and a linear one.
        hint_term: Term,
        /// Echoes the request's `context`.
        context: Bytes,
    },

    /// The leader telling a follower its log is too far behind to patch, and handing it a
    /// snapshot instead. Sent when the entries the follower needs have been compacted away.
    InstallSnapshot {
        /// The leader.
        from: NodeId,
        /// The follower.
        to: NodeId,
        /// The leader's term.
        term: Term,
        /// The snapshot. The core reads only `meta`.
        snapshot: Snapshot,
    },

    /// The leader ordering `to` to campaign immediately, for leadership transfer (§3.10). The
    /// target skips its election timeout, and the resulting `RequestVote` carries `force`.
    TimeoutNow {
        /// The outgoing leader.
        from: NodeId,
        /// The intended new leader.
        to: NodeId,
        /// The outgoing leader's term.
        term: Term,
    },

    /// A follower forwarding a linearizable read to the leader, because only the leader can
    /// establish the read's index.
    ReadIndex {
        /// The follower that received the read.
        from: NodeId,
        /// The leader.
        to: NodeId,
        /// The follower's term.
        term: Term,
        /// The caller's tag, returned untouched.
        ctx: Bytes,
    },

    /// The leader's answer: the commit index it confirmed it still owned.
    ReadIndexResponse {
        /// The leader.
        from: NodeId,
        /// The follower that asked.
        to: NodeId,
        /// The leader's term.
        term: Term,
        /// Apply through this index before answering the read.
        index: Index,
        /// The caller's tag.
        ctx: Bytes,
    },
}

impl Message {
    /// Who sent it.
    ///
    /// Named `sender` rather than `from` so that `Message::from` keeps meaning what it means
    /// everywhere else in Rust.
    pub fn sender(&self) -> NodeId {
        match self {
            Self::RequestVote { from, .. }
            | Self::RequestVoteResponse { from, .. }
            | Self::AppendEntries { from, .. }
            | Self::AppendEntriesResponse { from, .. }
            | Self::InstallSnapshot { from, .. }
            | Self::TimeoutNow { from, .. }
            | Self::ReadIndex { from, .. }
            | Self::ReadIndexResponse { from, .. } => *from,
        }
    }

    /// Who it is for.
    pub fn recipient(&self) -> NodeId {
        match self {
            Self::RequestVote { to, .. }
            | Self::RequestVoteResponse { to, .. }
            | Self::AppendEntries { to, .. }
            | Self::AppendEntriesResponse { to, .. }
            | Self::InstallSnapshot { to, .. }
            | Self::TimeoutNow { to, .. }
            | Self::ReadIndex { to, .. }
            | Self::ReadIndexResponse { to, .. } => *to,
        }
    }

    /// The term the sender was in.
    pub fn term(&self) -> Term {
        match self {
            Self::RequestVote { term, .. }
            | Self::RequestVoteResponse { term, .. }
            | Self::AppendEntries { term, .. }
            | Self::AppendEntriesResponse { term, .. }
            | Self::InstallSnapshot { term, .. }
            | Self::TimeoutNow { term, .. }
            | Self::ReadIndex { term, .. }
            | Self::ReadIndexResponse { term, .. } => *term,
        }
    }

    /// Whether this is a pre-vote request or response.
    ///
    /// Pre-vote messages are the exception to Raft's "a higher term means step down" reflex: a
    /// pre-vote carries a term the sender has not adopted and is only asking about. Treating one
    /// as a real term is how a partitioned node's repeated campaigns disrupt a healthy cluster —
    /// the disruption pre-vote exists to prevent.
    pub fn is_pre_vote(&self) -> bool {
        match self {
            Self::RequestVote { pre_vote, .. } | Self::RequestVoteResponse { pre_vote, .. } => {
                *pre_vote
            }
            _ => false,
        }
    }

    /// A short name for traces and test failures.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::RequestVote {
                pre_vote: false, ..
            } => "RequestVote",
            Self::RequestVote { pre_vote: true, .. } => "PreVote",
            Self::RequestVoteResponse {
                pre_vote: false, ..
            } => "RequestVoteResponse",
            Self::RequestVoteResponse { pre_vote: true, .. } => "PreVoteResponse",
            Self::AppendEntries { entries, .. } if entries.is_empty() => "Heartbeat",
            Self::AppendEntries { .. } => "AppendEntries",
            Self::AppendEntriesResponse { .. } => "AppendEntriesResponse",
            Self::InstallSnapshot { .. } => "InstallSnapshot",
            Self::TimeoutNow { .. } => "TimeoutNow",
            Self::ReadIndex { .. } => "ReadIndex",
            Self::ReadIndexResponse { .. } => "ReadIndexResponse",
        }
    }
}
