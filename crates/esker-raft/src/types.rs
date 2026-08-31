//! The values Raft moves around: identities, log entries, the durable state, configurations and
//! snapshots.
//!
//! Everything here is plain data with no behaviour beyond what keeps it honest. Two shapes carry
//! a rule rather than a value, and they are the ones to read carefully:
//!
//! * [`HardState`] is the state that must survive a crash — term, vote, commit index. It leaves
//!   the core through [`Ready::hard_state`](crate::Ready::hard_state) and the driver fsyncs it
//!   *before* sending the messages from the same `Ready`. A vote that is answered but not durable
//!   is a vote cast twice in one term after a restart, which is two leaders.
//! * [`ConfState`] is a *sorted, deduplicated* membership list. Sorted because every decision the
//!   core makes from it must be identical on two runs of the same seed; a set with an arbitrary
//!   iteration order would make the simulator's traces irreproducible
//!   (`docs/plans/phase-3.md` §7 risk 2).

use bytes::Bytes;

use crate::error::{RaftError, Result};

/// A node's identity within one Raft group. Assigned by the placement driver; never reused.
pub type NodeId = u64;

/// A Raft term. Monotonic, and the only ordering the algorithm trusts.
pub type Term = u64;

/// A position in the Raft log. The first real entry is at index 1; index 0 is the empty log.
pub type Index = u64;

/// Narrows a log offset to a machine index.
///
/// A log with more entries than `usize::MAX` cannot exist on the machine holding it, so the
/// saturating branch is unreachable rather than lossy — but it is a value and not a panic, and
/// every caller bounds-checks the result against the collection it indexes anyway
/// (`CLAUDE.md` invariant 9).
pub(crate) fn offset(delta: Index) -> usize {
    usize::try_from(delta).unwrap_or(usize::MAX)
}

/// What an [`Entry`] means to the layer above.
///
/// The core reads this for exactly one reason: a [`EntryKind::ConfChange`] entry changes the
/// membership **when it is appended**, not when it commits (dissertation §4.1), so the core has to
/// recognise one. A [`EntryKind::Normal`] entry's bytes are never inspected — invariant 7 applied
/// to consensus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// An opaque proposal from the state machine above.
    Normal,
    /// A single-server membership change, encoded by [`ConfChange::encode`].
    ConfChange,
}

/// One entry in the replicated log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The term of the leader that created this entry. Together with `index` it identifies the
    /// entry across the whole cluster: Raft's Log Matching property says that if two logs contain
    /// an entry with the same index and term, the logs are identical up to that point.
    pub term: Term,
    /// This entry's position in the log.
    pub index: Index,
    /// Whether the core has to interpret `data`.
    pub kind: EntryKind,
    /// The payload. Opaque for [`EntryKind::Normal`].
    pub data: Bytes,
}

impl Entry {
    /// An ordinary proposal.
    pub fn normal(term: Term, index: Index, data: Bytes) -> Self {
        Self {
            term,
            index,
            kind: EntryKind::Normal,
            data,
        }
    }

    /// An empty entry, which a new leader appends to its own term so that §5.4.2's commit rule
    /// lets earlier terms' entries commit behind it.
    pub fn empty(term: Term, index: Index) -> Self {
        Self::normal(term, index, Bytes::new())
    }

    /// A membership change, ready to append.
    pub fn conf_change(term: Term, index: Index, change: &ConfChange) -> Self {
        Self {
            term,
            index,
            kind: EntryKind::ConfChange,
            data: change.encode(),
        }
    }

    /// What this entry costs against a batching budget: the payload plus a fixed allowance for the
    /// term, index and kind the wire format carries beside it. Approximate on purpose — it bounds
    /// a message, it does not describe an on-disk layout.
    pub fn cost(&self) -> u64 {
        const OVERHEAD: u64 = 24;
        OVERHEAD + self.data.len() as u64
    }
}

/// The state a node must have on stable storage before it acts on it.
///
/// The three fields are the ones Raft's Figure 3.1 marks as persistent. `commit` is included
/// because replaying it saves a restarted node from re-deriving what it already knew; losing it is
/// survivable, losing `term` or `voted_for` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HardState {
    /// The latest term this node has seen.
    pub term: Term,
    /// Who this node voted for in `term`, if anyone.
    pub voted_for: Option<NodeId>,
    /// The highest index known to be committed.
    pub commit: Index,
}

impl HardState {
    /// Whether this differs from `other` in a way that has to reach stable storage. A `Ready` only
    /// carries a `HardState` when the answer is yes, so an idle node produces no writes.
    pub fn differs_from(&self, other: &Self) -> bool {
        self != other
    }
}

/// Who is in the group, and in what role.
///
/// Both lists are kept sorted and deduplicated by [`ConfState::normalize`]. Learners receive the
/// log but do not vote and do not count toward a quorum; they exist so a new replica can catch up
/// without making elections harder while it does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfState {
    /// Members that vote and count toward quorum.
    pub voters: Vec<NodeId>,
    /// Members that replicate but do not vote.
    pub learners: Vec<NodeId>,
}

impl ConfState {
    /// A voter-only configuration.
    pub fn from_voters(voters: Vec<NodeId>) -> Self {
        let mut state = Self {
            voters,
            learners: Vec::new(),
        };
        state.normalize();
        state
    }

    /// Sorts and deduplicates both lists, and drops from `learners` anything that is also a voter
    /// — a node is one or the other, and "voter" wins because it is the stronger claim.
    pub fn normalize(&mut self) {
        self.voters.sort_unstable();
        self.voters.dedup();
        self.learners.sort_unstable();
        self.learners.dedup();
        self.learners
            .retain(|id| self.voters.binary_search(id).is_err());
    }

    /// Whether `id` votes in this configuration.
    pub fn is_voter(&self, id: NodeId) -> bool {
        self.voters.binary_search(&id).is_ok()
    }

    /// Whether `id` replicates without voting.
    pub fn is_learner(&self, id: NodeId) -> bool {
        self.learners.binary_search(&id).is_ok()
    }

    /// Whether `id` is a member at all.
    pub fn contains(&self, id: NodeId) -> bool {
        self.is_voter(id) || self.is_learner(id)
    }

    /// How many votes make a majority. Learners are not counted, which is their entire purpose.
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// Every member, voters first, in sorted order. The iteration order of this is a decision
    /// input, so it is defined rather than incidental.
    pub fn members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.voters.iter().chain(self.learners.iter()).copied()
    }
}

/// What kind of single-server membership change an [`EntryKind::ConfChange`] entry asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfChangeKind {
    /// Add `node` as a voter, or promote it from learner.
    AddVoter,
    /// Add `node` as a learner, or demote it from voter.
    AddLearner,
    /// Remove `node` from the group entirely.
    Remove,
}

impl ConfChangeKind {
    /// The byte this kind occupies in the encoded payload. Part of a persisted format: these
    /// numbers are fixed.
    fn tag(self) -> u8 {
        match self {
            Self::AddVoter => 1,
            Self::AddLearner => 2,
            Self::Remove => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::AddVoter),
            2 => Some(Self::AddLearner),
            3 => Some(Self::Remove),
            _ => None,
        }
    }
}

/// A single-server membership change.
///
/// # Format (*fixed*, version 1)
///
/// This is the payload of a `ConfChange` entry, so it is a persisted format and its bytes cannot
/// move without an ADR and a format version (`CLAUDE.md`). Little-endian, no varints — the payload
/// is nine bytes plus a context, and a fixed header reads the same in a hex dump as in code:
///
/// ```text
/// kind     u8        1 = AddVoter, 2 = AddLearner, 3 = Remove
/// node     u64 LE    the node this change is about
/// context  bytes     the rest of the payload, opaque to this crate
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfChange {
    /// What to do.
    pub kind: ConfChangeKind,
    /// Who to do it to.
    pub node: NodeId,
    /// Caller data carried through the log — a store id, a peer address. Never interpreted here.
    pub context: Bytes,
}

impl ConfChange {
    /// Bytes an encoded change occupies before its context.
    pub const HEADER_LEN: usize = 9;

    /// A change with no context.
    pub fn new(kind: ConfChangeKind, node: NodeId) -> Self {
        Self {
            kind,
            node,
            context: Bytes::new(),
        }
    }

    /// Encodes this change into an entry payload. See the type's format documentation.
    pub fn encode(&self) -> Bytes {
        let mut buf = Vec::with_capacity(Self::HEADER_LEN + self.context.len());
        buf.push(self.kind.tag());
        buf.extend_from_slice(&self.node.to_le_bytes());
        buf.extend_from_slice(&self.context);
        Bytes::from(buf)
    }

    /// Decodes an entry payload written by [`ConfChange::encode`].
    ///
    /// These bytes come off a disk that may have lied, so every failure is a value and none is a
    /// panic (`CLAUDE.md` invariant 9).
    pub fn decode(data: &Bytes) -> Result<Self> {
        if data.len() < Self::HEADER_LEN {
            return Err(RaftError::CorruptConfChange(format!(
                "payload is {} bytes, needs at least {}",
                data.len(),
                Self::HEADER_LEN
            )));
        }
        let kind = ConfChangeKind::from_tag(data[0]).ok_or_else(|| {
            RaftError::CorruptConfChange(format!("unknown kind byte {}", data[0]))
        })?;
        let mut node_bytes = [0_u8; 8];
        node_bytes.copy_from_slice(&data[1..Self::HEADER_LEN]);
        Ok(Self {
            kind,
            node: u64::from_le_bytes(node_bytes),
            context: data.slice(Self::HEADER_LEN..),
        })
    }

    /// Applies this change to `conf`, returning whether anything moved.
    ///
    /// Idempotent: adding a voter that is already a voter is a no-op, not an error. A membership
    /// change can be appended twice — a leader retries, a log is replayed — and the result has to
    /// be the same configuration either way.
    pub fn apply_to(&self, conf: &mut ConfState) -> bool {
        let before = conf.clone();
        conf.voters.retain(|id| *id != self.node);
        conf.learners.retain(|id| *id != self.node);
        match self.kind {
            ConfChangeKind::AddVoter => conf.voters.push(self.node),
            ConfChangeKind::AddLearner => conf.learners.push(self.node),
            ConfChangeKind::Remove => {}
        }
        conf.normalize();
        *conf != before
    }
}

/// A leader's view of one of its peers (`RawNode::progress`).
///
/// A **snapshot** of what the leader believed when it was asked, not a live view: every field
/// moves as acknowledgements arrive. Callers use it to report, or to make a decision they can
/// afford to be a moment late on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerProgress {
    /// The peer.
    pub id: NodeId,
    /// The highest index the leader knows is replicated on it. This is the "has it caught up"
    /// number: a peer whose `matched` is near the leader's last index is one whose promotion to
    /// voter will not stall a quorum.
    pub matched: Index,
    /// The next index the leader will send. A guess while the leader is probing for the peer's
    /// tail, a fact once it is replicating.
    pub next: Index,
    /// Whether the peer replicates without voting.
    pub is_learner: bool,
    /// Whether the peer has been heard from within the current election timeout. A leader that
    /// cannot see a quorum of `recent_active` peers steps down (§6.2), and a peer that is `false`
    /// is one a scheduler should not move work onto.
    pub recent_active: bool,
    /// The index of a snapshot in flight to this peer, or `0`. Non-zero means the peer is being
    /// caught up by state rather than by log, and its `matched` will jump rather than climb.
    pub pending_snapshot: Index,
}

/// What became of a snapshot transfer the driver was running.
///
/// The core sends an `InstallSnapshot` and then waits: `ProgressState::Snapshot` is paused
/// unconditionally, and only the follower can end it. That is sound as long as the follower
/// eventually answers — and a snapshot the network lost, the receiver refused, or a killed process
/// abandoned produces no answer at all, so the replica is stranded for the leader's whole term.
///
/// The bytes are the driver's business (invariant 4), so only the driver knows. Reporting is how
/// it says. Neither answer is a promise the follower is caught up — that is still an
/// `AppendEntriesResponse`'s job — only a statement about the *transfer*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotStatus {
    /// The bytes were delivered. The leader may probe from the snapshot's index: the follower
    /// holds at least that much, even though it has not said so yet.
    Finished,
    /// The transfer did not complete. The leader forgets the index it promised and probes from
    /// what the follower is actually known to have, which is what makes the next attempt start
    /// from the truth rather than from the promise.
    Failed,
}

/// What a snapshot says about the log it replaces.
///
/// The core reads only this. It is the whole reason `InstallSnapshot` is safe to handle in a
/// crate that does no I/O: a snapshot's *effect* on Raft is "your log now starts at `index`, whose
/// term is `term`, with this membership" — the bytes are the state machine's business.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotMeta {
    /// The last index included in the snapshot.
    pub index: Index,
    /// The term of the entry at `index`.
    pub term: Term,
    /// The membership as of `index`. A restoring node adopts it wholesale.
    pub conf: ConfState,
}

/// A snapshot of the state machine, plus the metadata that places it in the log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Where this snapshot sits in the log, and what the membership was there.
    pub meta: SnapshotMeta,
    /// The state machine's bytes. Opaque here; `esker-store` streams them (`docs/DESIGN.md` §5).
    pub data: Bytes,
}

impl Snapshot {
    /// Whether this snapshot carries no state at all. `LogStorage::snapshot` returns one of these
    /// when nothing has been compacted yet, and the core must not try to install it.
    pub fn is_empty(&self) -> bool {
        self.meta.index == 0
    }
}

/// A read whose linearizability point has been established.
///
/// The index is the commit index the leader confirmed it still owned; the driver may answer the
/// read once it has applied through that index, and not before (`docs/plans/phase-3.md` §4 rule 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadState {
    /// Apply through this index before answering.
    pub index: Index,
    /// The caller's tag, returned untouched so it can match the answer to the request.
    pub ctx: Bytes,
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{ConfChange, ConfChangeKind, ConfState, Entry};

    /// The `ConfChange` payload is persisted in the Raft log, so its bytes are a format. This is
    /// the golden: a change to it is a format change, and needs an ADR and a version.
    #[test]
    fn a_conf_change_encodes_to_the_documented_bytes() {
        let change = ConfChange {
            kind: ConfChangeKind::AddLearner,
            node: 0x0102_0304_0506_0708,
            context: Bytes::from_static(b"store-7"),
        };
        assert_eq!(
            change.encode().as_ref(),
            &[
                2, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, b's', b't', b'o', b'r', b'e',
                b'-', b'7'
            ],
        );
        assert_eq!(ConfChange::decode(&change.encode()).unwrap(), change);
    }

    #[test]
    fn a_conf_change_round_trips_for_every_kind() {
        for kind in [
            ConfChangeKind::AddVoter,
            ConfChangeKind::AddLearner,
            ConfChangeKind::Remove,
        ] {
            let change = ConfChange::new(kind, 42);
            assert_eq!(ConfChange::decode(&change.encode()).unwrap(), change);
        }
    }

    /// Invariant 9: bytes off a disk that may have lied come back as errors, never panics.
    #[test]
    fn a_corrupt_conf_change_is_an_error_and_not_a_panic() {
        assert!(ConfChange::decode(&Bytes::from_static(b"")).is_err());
        assert!(ConfChange::decode(&Bytes::from_static(b"\x01\x00\x00")).is_err());
        assert!(ConfChange::decode(&Bytes::from_static(&[9, 0, 0, 0, 0, 0, 0, 0, 0])).is_err());
    }

    #[test]
    fn a_configuration_is_sorted_deduplicated_and_never_both_roles() {
        let mut conf = ConfState {
            voters: vec![3, 1, 3, 2],
            learners: vec![5, 2, 5],
        };
        conf.normalize();
        assert_eq!(conf.voters, vec![1, 2, 3]);
        // 2 was listed as both; "voter" is the stronger claim and wins.
        assert_eq!(conf.learners, vec![5]);
        assert!(conf.is_voter(2));
        assert!(!conf.is_learner(2));
    }

    /// Learners are the whole point of the distinction: they replicate without making elections
    /// harder while a new replica catches up.
    #[test]
    fn learners_do_not_count_toward_a_quorum() {
        let conf = ConfState {
            voters: vec![1, 2, 3],
            learners: vec![4, 5, 6, 7],
        };
        assert_eq!(conf.quorum(), 2);
        assert_eq!(ConfState::from_voters(vec![1, 2, 3, 4]).quorum(), 3);
        assert_eq!(ConfState::from_voters(vec![1]).quorum(), 1);
    }

    /// A membership change can be appended twice — a leader retries, a log is replayed — and both
    /// paths have to reach the same configuration.
    #[test]
    fn applying_a_conf_change_twice_changes_nothing_the_second_time() {
        let mut conf = ConfState::from_voters(vec![1, 2, 3]);
        let add = ConfChange::new(ConfChangeKind::AddVoter, 4);
        assert!(add.apply_to(&mut conf));
        assert!(!add.apply_to(&mut conf));
        assert_eq!(conf.voters, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_learner_is_promoted_rather_than_duplicated() {
        let mut conf = ConfState {
            voters: vec![1, 2],
            learners: vec![3],
        };
        assert!(ConfChange::new(ConfChangeKind::AddVoter, 3).apply_to(&mut conf));
        assert_eq!(conf.voters, vec![1, 2, 3]);
        assert!(conf.learners.is_empty());
    }

    #[test]
    fn an_entry_costs_its_payload_plus_a_fixed_allowance() {
        let small = Entry::empty(1, 1);
        let large = Entry::normal(1, 2, Bytes::from(vec![0_u8; 100]));
        assert!(large.cost() > small.cost());
        assert_eq!(large.cost() - small.cost(), 100);
    }
}
