//! The four safety properties of Raft, checked after every event.
//!
//! `prompts/03-raft.md` (3b) names them: election safety, log matching, leader completeness,
//! state-machine safety. They are checked *after every event* rather than at the end of a run,
//! because an invariant that is violated and then self-heals is still a bug — and the trace
//! that explains it is a hundred events long, not ten thousand.
//!
//! The checkers deliberately do not know about [`esker_raft::RawNode`]. They see a
//! [`NodeSnapshot`]: an identifier, a role, a term, a commit index, a log of digests, and what
//! the state machine has consumed. That is what makes them testable against a hand-written
//! violation — which is the only way to know a checker works, since a checker that has never
//! been shown red is decoration.
//!
//! # What each property means here
//!
//! * **Election safety** — at most one leader per term. Recorded across the whole run, so a
//!   leader that crashed still counts against its term.
//! * **Log matching** — if two logs hold an entry with the same index and term, the logs are
//!   identical in every entry up to it. Checked with a rolling prefix digest: the digest at
//!   `(index, term)` is a function of every entry at or below it, so two nodes that agree on
//!   `(index, term)` and disagree on the digest disagree somewhere in the prefix.
//! * **Leader completeness** — an entry committed in some term is present in the log of every
//!   leader of a higher term. Committed entries are recorded as any node observes them, with
//!   the term it observed them in.
//! * **State-machine safety** — no two nodes apply different entries at the same index, and no
//!   node skips one. Together that is "every applied sequence is a prefix of every longer one".
//!
//! Records are never retracted. A node that truncates its log does not erase the fact that it
//! once held that `(index, term)`, and a node whose commit index goes backwards across a
//! restart — which is legal, a `HardState` that was never fsynced is simply gone — does not
//! un-commit what a majority had already stored.

use std::collections::BTreeMap;

use esker_base::hash::hash64;
use esker_raft::{Index, NodeId, Term};
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
    /// The prefix digest at [`NodeSnapshot::compacted_through`] — `0` for a log that has never
    /// been compacted. A driver that compacts carries the digest forward; a driver that
    /// installs a snapshot sent by a leader seeds it from
    /// [`SafetyChecker::prefix_digest`].
    pub prefix_anchor: u64,
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

/// What the run has established about one committed index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Committed {
    entry: EntryDigest,
    /// The term of the node that first observed this index as committed. A leader of a higher
    /// term must have the entry.
    observed_in: Term,
}

/// Everything the checker remembers about one node between observations.
#[derive(Debug, Clone, Default)]
struct NodeMemo {
    /// First index held in `chain`; `compacted_through + 1` as of the last observation.
    first_index: Index,
    /// The anchor the chain was computed from, so that a changed anchor forces a rebuild.
    anchor: u64,
    /// Per log position: the entry and the prefix digest ending at it.
    chain: Vec<(EntryDigest, u64)>,
    /// How much of the node's `applied` slice has been checked.
    applied_checked: usize,
    /// The index it last applied.
    last_applied: Index,
    /// The highest committed index recorded from this node.
    committed_checked: Index,
    /// While the node leads this term, everything committed at or below this index has been
    /// verified present in its log.
    leading: Option<(Term, Index)>,
}

/// The four safety properties, checked incrementally.
///
/// Feed it a [`SafetyChecker::observe`] after every event. It keeps the record of what the run
/// has established — which terms had which leaders, which prefixes go with which
/// `(index, term)`, what has been committed and what has been applied — and only re-examines
/// the part of a node that changed, so checking after every event costs about what checking
/// once does.
#[derive(Debug, Clone, Default)]
pub struct SafetyChecker {
    leaders: BTreeMap<Term, NodeId>,
    prefixes: BTreeMap<(Index, Term), (u64, NodeId)>,
    committed: BTreeMap<Index, Committed>,
    applied: BTreeMap<Index, (EntryDigest, NodeId)>,
    memo: BTreeMap<NodeId, NodeMemo>,
}

impl SafetyChecker {
    /// A checker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The prefix digest recorded for `(index, term)`, if any node has ever held it.
    ///
    /// A driver that installs a leader's snapshot has no entries to compute an anchor from and
    /// takes it from here (phase 3d).
    #[must_use]
    pub fn prefix_digest(&self, index: Index, term: Term) -> Option<u64> {
        self.prefixes.get(&(index, term)).map(|(digest, _)| *digest)
    }

    /// The leader recorded for `term`, if one has been seen.
    #[must_use]
    pub fn leader_of(&self, term: Term) -> Option<NodeId> {
        self.leaders.get(&term).copied()
    }

    /// The highest index any node has been observed to commit.
    #[must_use]
    pub fn committed_upto(&self) -> Index {
        self.committed.keys().next_back().copied().unwrap_or(0)
    }

    /// Checks every property against `nodes`, the whole cluster as of now.
    ///
    /// The nodes must be in a fixed order — node id order — so that which of two disagreeing
    /// nodes is named in a violation is a function of the seed and not of iteration order.
    pub fn observe(&mut self, nodes: &[NodeSnapshot<'_>]) -> Result<(), Violation> {
        for node in nodes {
            self.check_election_safety(node)?;
            self.check_log_matching(node)?;
            self.record_committed(node)?;
            self.check_state_machine_safety(node)?;
        }
        // Leader completeness is checked last: it reads the committed record that this round's
        // observations have just extended, so a leader elected in the same event as a commit
        // is judged against the commit.
        for node in nodes {
            self.check_leader_completeness(node)?;
        }
        Ok(())
    }

    fn check_election_safety(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        if !node.is_leader {
            return Ok(());
        }
        match self.leaders.get(&node.term) {
            Some(&first) if first != node.id => Err(Violation::ElectionSafety {
                term: node.term,
                first,
                second: node.id,
            }),
            Some(_) => Ok(()),
            None => {
                self.leaders.insert(node.term, node.id);
                Ok(())
            }
        }
    }

    /// Recomputes the changed tail of a node's prefix-digest chain and checks each recomputed
    /// position against the record.
    fn check_log_matching(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        let first_index = node.compacted_through + 1;
        for (expected, entry) in (first_index..).zip(node.log.iter()) {
            if entry.index != expected {
                return Err(Violation::MalformedLog {
                    node: node.id,
                    expected,
                    got: entry.index,
                });
            }
        }

        let memo = self.memo.entry(node.id).or_default();
        // A different starting point or a different anchor means the chain has to be rebuilt
        // from scratch; otherwise only the tail that changed does.
        let rebuild = memo.first_index != first_index || memo.anchor != node.prefix_anchor;
        let unchanged = if rebuild {
            memo.first_index = first_index;
            memo.anchor = node.prefix_anchor;
            memo.chain.clear();
            0
        } else {
            memo.chain
                .iter()
                .zip(node.log.iter())
                .take_while(|((cached, _), current)| cached == *current)
                .count()
        };
        memo.chain.truncate(unchanged);

        let mut previous = memo
            .chain
            .last()
            .map_or(node.prefix_anchor, |(_, digest)| *digest);
        for entry in &node.log[unchanged..] {
            previous = chain_digest(previous, entry);
            memo.chain.push((*entry, previous));

            match self.prefixes.get(&(entry.index, entry.term)) {
                Some(&(expected, other)) if expected != previous => {
                    return Err(Violation::LogMatching {
                        index: entry.index,
                        term: entry.term,
                        node: node.id,
                        other,
                        found: previous,
                        expected,
                    });
                }
                Some(_) => {}
                None => {
                    self.prefixes
                        .insert((entry.index, entry.term), (previous, node.id));
                }
            }
        }
        Ok(())
    }

    /// Records everything this node considers committed, and rejects a second, different
    /// entry at an index that is already committed.
    fn record_committed(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        let last = node.log.last().map_or(node.compacted_through, |e| e.index);
        let top = node.commit.min(last);
        let memo = self.memo.entry(node.id).or_default();
        let from = memo.committed_checked.max(node.compacted_through) + 1;
        if top >= from {
            memo.committed_checked = top;
        }
        let first_index = node.compacted_through + 1;

        for index in from..=top {
            let Some(entry) = offset_of(index, first_index).and_then(|at| node.log.get(at)) else {
                continue;
            };
            match self.committed.get(&index) {
                Some(record) if record.entry != *entry => {
                    // Two different entries committed at one index. That is the sharpest form
                    // of a log-matching failure, so it is reported as one.
                    let (expected, other) = self
                        .prefixes
                        .get(&(index, record.entry.term))
                        .copied()
                        .unwrap_or((0, node.id));
                    return Err(Violation::LogMatching {
                        index,
                        term: record.entry.term,
                        node: node.id,
                        other,
                        found: entry.payload,
                        expected,
                    });
                }
                Some(_) => {}
                None => {
                    self.committed.insert(
                        index,
                        Committed {
                            entry: *entry,
                            observed_in: node.term,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn check_leader_completeness(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        if !node.is_leader {
            if let Some(memo) = self.memo.get_mut(&node.id) {
                memo.leading = None;
            }
            return Ok(());
        }
        let memo = self.memo.entry(node.id).or_default();
        let verified = match memo.leading {
            Some((term, upto)) if term == node.term => upto,
            _ => 0,
        };

        let last = node.log.last().map_or(node.compacted_through, |e| e.index);
        let first_index = node.compacted_through + 1;
        let mut highest = verified;

        for (&index, record) in self.committed.range((verified + 1)..) {
            if record.observed_in >= node.term {
                // Committed in this term or later: this leader is not required to have had it
                // when it was elected, and a later term's commit says nothing about it.
                continue;
            }
            let present = if index < first_index {
                // Inside the node's snapshot.
                index <= node.compacted_through
            } else {
                offset_of(index, first_index)
                    .and_then(|at| node.log.get(at))
                    .is_some_and(|entry| *entry == record.entry)
            };
            if !present {
                let found = if index > last {
                    format!("nothing past index {last}")
                } else if index < first_index {
                    format!("a snapshot through index {}", node.compacted_through)
                } else {
                    offset_of(index, first_index)
                        .and_then(|at| node.log.get(at))
                        .map_or_else(|| "nothing".to_owned(), |entry| format!("{entry:?}"))
                };
                return Err(Violation::LeaderCompleteness {
                    leader: node.id,
                    leader_term: node.term,
                    index,
                    entry_term: record.entry.term,
                    committed_in: record.observed_in,
                    found,
                });
            }
            highest = highest.max(index);
        }
        memo.leading = Some((node.term, highest));
        Ok(())
    }

    fn check_state_machine_safety(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        let memo = self.memo.entry(node.id).or_default();
        if node.applied.len() < memo.applied_checked {
            // The state machine cannot un-apply. A restart rebuilds the driver's record from
            // the snapshot it applied, so the cursor is reset rather than trusted.
            memo.applied_checked = 0;
            memo.last_applied = 0;
        }
        let start = memo.applied_checked;
        memo.applied_checked = node.applied.len();

        let mut last_applied = memo.last_applied;
        for entry in &node.applied[start..] {
            if entry.index != last_applied + 1 && last_applied != 0 {
                return Err(Violation::ApplyOutOfOrder {
                    node: node.id,
                    previous: last_applied,
                    got: entry.index,
                });
            }
            last_applied = entry.index;

            match self.applied.get(&entry.index) {
                Some(&(recorded, other)) if recorded != *entry => {
                    return Err(Violation::StateMachineSafety {
                        index: entry.index,
                        node: node.id,
                        term: entry.term,
                        payload: entry.payload,
                        other,
                        other_term: recorded.term,
                        other_payload: recorded.payload,
                    });
                }
                Some(_) => {}
                None => {
                    self.applied.insert(entry.index, (*entry, node.id));
                }
            }
        }
        if let Some(memo) = self.memo.get_mut(&node.id) {
            memo.last_applied = last_applied;
        }
        Ok(())
    }
}

/// The position of `index` in a log that starts at `first_index`, or `None` if it is not in
/// range. Written out rather than cast, because a `u64 -> usize` cast that truncates would
/// silently read the wrong entry.
fn offset_of(index: Index, first_index: Index) -> Option<usize> {
    usize::try_from(index.checked_sub(first_index)?).ok()
}

/// The prefix digest ending at `entry`: a function of every entry at or below it.
fn chain_digest(previous: u64, entry: &EntryDigest) -> u64 {
    let mut bytes = [0_u8; 32];
    bytes[..8].copy_from_slice(&previous.to_le_bytes());
    bytes[8..16].copy_from_slice(&entry.index.to_le_bytes());
    bytes[16..24].copy_from_slice(&entry.term.to_le_bytes());
    bytes[24..].copy_from_slice(&entry.payload.to_le_bytes());
    hash64(&bytes)
}
