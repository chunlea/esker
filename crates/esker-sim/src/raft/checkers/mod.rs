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
//! The log a node reports is its *whole* log — durable prefix and unstable tail — read through
//! `RawNode::log_entries`. That accessor is why there is no longer a gate here: the harness used
//! to skip the committed record and leader completeness while a node had a disk write
//! outstanding, because the commit index came from the core and the log came from storage, and
//! behind a slow disk the two were from different instants. Both now come from the same place.
//!
//! Records are never retracted. A node that truncates its log does not erase the fact that it
//! once held that `(index, term)`, and a node whose commit index goes backwards across a
//! restart — which is legal, a `HardState` that was never fsynced is simply gone — does not
//! un-commit what a majority had already stored.

use std::collections::{BTreeMap, BTreeSet};

use esker_base::hash::hash64;
use esker_raft::{ConfState, Index, NodeId, Term};

mod observation;

pub use observation::{EntryDigest, NodeSnapshot, Violation};

/// What the run has established about one committed index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Committed {
    entry: EntryDigest,
    /// The voters of the configuration in force when this index was committed. Quorum is
    /// counted against *this*, not against whoever the members happen to be later.
    voters: Vec<NodeId>,
    /// Every node ever seen holding this entry.
    holders: BTreeSet<NodeId>,
    /// The node that first observed this index as committed.
    by: NodeId,
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
    replicated_overwrites: u64,
    /// The configuration each conf-change *entry* produced, and who derived it first.
    configs: BTreeMap<EntryDigest, (ConfState, NodeId)>,
}

impl SafetyChecker {
    /// A checker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports any committed index that was never held by a quorum of the configuration in
    /// force there.
    ///
    /// Deferred rather than instantaneous, because an observation lags the acknowledgement it
    /// followed: asking "is a quorum holding it *right now*" would report the observer being
    /// behind. Asking "did a quorum ever hold it" does not.
    ///
    /// # Where this is sound, and where it is not
    ///
    /// "The configuration in force at index *i*" is only well defined relative to one log.
    /// While membership is changing under partitions, two nodes can be on branches whose
    /// conf-change entries differ, and the voter set this counts against is then whichever
    /// branch the observing node was on. So it is a hard assertion on a cluster with a single
    /// log lineage — which is what `raft_membership.rs`'s quiet test has — and a *report* under
    /// a fault plan that reconfigures and partitions at once. The properties that do not depend
    /// on picking a branch stay hard everywhere.
    pub fn verify_quorums(&self) -> Result<(), Violation> {
        for (index, record) in &self.committed {
            let quorum = record.voters.len() / 2 + 1;
            // Only the voters of that configuration count towards its quorum; a server that was
            // removed afterwards still holds the bytes, but it is not who agreed.
            let holders = record
                .holders
                .iter()
                .filter(|id| record.voters.contains(id))
                .count();
            if holders < quorum {
                return Err(Violation::CommittedWithoutQuorum {
                    index: *index,
                    holders,
                    voters: record.voters.clone(),
                    quorum,
                });
            }
        }
        Ok(())
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

    /// How many times an entry that *another* node had already recorded was overwritten in
    /// some node's log.
    ///
    /// This is the §5.4.2 interleaving, counted: an entry that was replicated beyond the leader
    /// that created it, and then replaced by a later leader. A sweep that never produces one
    /// has not tested the term condition, however many seeds it ran.
    #[must_use]
    pub fn replicated_overwrites(&self) -> u64 {
        self.replicated_overwrites
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
            self.check_membership(node)?;
            self.check_log_matching(node)?;
            self.record_committed(node)?;
            self.check_snapshot(node)?;
            self.count_holders(node);
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

    /// The membership checks: the two derivations agree where they are comparable, nodes that
    /// hold the same conf-change entry derive the same configuration from it, each node's own
    /// configuration moves one server at a time, and a node that is not a voter does not win an
    /// election.
    fn check_membership(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        if node.online && node.comparable_config && node.config != node.core_config {
            return Err(Violation::ConfigDisagreesWithCore {
                node: node.id,
                driver: node.config.voters.clone(),
                core: node.core_config.voters.clone(),
            });
        }
        Self::check_single_server(node)?;

        if node.config_index == 0 {
            return Ok(());
        }
        // Keyed on the *entry*, not the index: two nodes on divergent branches can hold
        // different conf-change entries at one index, and it is only the same entry that must
        // produce the same configuration.
        let first_index = node.compacted_through + 1;
        let Some(entry) = offset_of(node.config_index, first_index).and_then(|at| node.log.get(at))
        else {
            return Ok(());
        };
        match self.configs.get(entry) {
            Some((recorded, other)) if recorded != node.config => {
                Err(Violation::ConfigDivergence {
                    index: node.config_index,
                    node: node.id,
                    other: *other,
                    config: node.config.voters.clone(),
                    recorded: recorded.voters.clone(),
                })
            }
            Some(_) => Ok(()),
            None => {
                self.configs.insert(*entry, (node.config.clone(), node.id));
                Ok(())
            }
        }
    }

    /// Adjacent configurations in one node's own log differ by exactly one server.
    ///
    /// Checked inside one observation, over the lineage the driver reports, because that is the
    /// only place where "adjacent" is unambiguous. Comparing two observations of one node, or
    /// two nodes with each other, compares across log branches — and two branches really can
    /// hold configurations more than one server apart without anything being wrong.
    fn check_single_server(node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        let mut previous = node.base_config;
        for (entry, config) in node.lineage {
            let moved = moved_servers(previous, config);
            if moved > 1 {
                return Err(Violation::ConfigJumped {
                    node: node.id,
                    to_index: entry.index,
                    from: previous.voters.clone(),
                    to: config.voters.clone(),
                    moved,
                });
            }
            previous = config;
        }
        Ok(())
    }

    fn check_election_safety(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        if !node.is_leader {
            return Ok(());
        }
        if !node.config.is_voter(node.id) && !self.leaders.contains_key(&node.term) {
            // A leader that has just appended its *own* removal is legitimately not a voter and
            // keeps leading until the change commits — but it was already recorded as the
            // leader of this term when it won it, so this only catches a node that was never a
            // voter winning in the first place.
            return Err(Violation::NonVoterElected {
                node: node.id,
                term: node.term,
                config: node.config.voters.clone(),
            });
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
        // What is about to be truncated away is an entry this node once held and no longer
        // does. Whether that matters is the §5.4.2 question: an entry that only ever lived on
        // this node is ordinary repair, but one that another node had also recorded was
        // *replicated* before being overwritten, which is the interleaving the term condition
        // exists to make safe. Counting them is how a sweep proves it reached the scenario
        // rather than merely asserting it did.
        let mut replicated_overwrites = 0_u64;
        for (stale, _) in &memo.chain[unchanged..] {
            if let Some(&(_, first_seen)) = self.prefixes.get(&(stale.index, stale.term))
                && first_seen != node.id
            {
                replicated_overwrites += 1;
            }
        }
        self.replicated_overwrites += replicated_overwrites;
        let memo = self.memo.entry(node.id).or_default();
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
                    return Err(Violation::CommittedTwice {
                        index,
                        node: node.id,
                        term: entry.term,
                        payload: entry.payload,
                        other: record.by,
                        other_term: record.entry.term,
                        other_payload: record.entry.payload,
                    });
                }
                Some(_) => {}
                None => {
                    self.committed.insert(
                        index,
                        Committed {
                            entry: *entry,
                            voters: config_in_force(node, index).voters,
                            holders: BTreeSet::new(),
                            by: node.id,
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

    /// A snapshot's metadata is the only claim about a node's state that no log can be
    /// compared against, so it is compared against the committed record instead.
    /// Counts this node among the holders of every committed entry it has.
    fn count_holders(&mut self, node: &NodeSnapshot<'_>) {
        let first_index = node.compacted_through + 1;
        for (index, record) in &mut self.committed {
            let held = if *index < first_index {
                *index <= node.compacted_through
            } else {
                offset_of(*index, first_index)
                    .and_then(|at| node.log.get(at))
                    .is_some_and(|entry| *entry == record.entry)
            };
            if held {
                record.holders.insert(node.id);
            }
        }
    }

    fn check_snapshot(&mut self, node: &NodeSnapshot<'_>) -> Result<(), Violation> {
        if node.compacted_through == 0 {
            return Ok(());
        }
        match self.committed.get(&node.compacted_through) {
            Some(record) if record.entry.term != node.snapshot_term => {
                Err(Violation::SnapshotMismatch {
                    node: node.id,
                    index: node.compacted_through,
                    claimed: node.snapshot_term,
                    committed: record.entry.term,
                })
            }
            _ => Ok(()),
        }
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
            // A gap is a bug unless a snapshot covers it: a node that installs one adopts
            // everything at or below its index without applying the entries one by one.
            let covered =
                entry.index > last_applied + 1 && node.compacted_through + 1 >= entry.index;
            if entry.index != last_applied + 1 && last_applied != 0 && !covered {
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

/// The configuration in force at `index` on this node: its base, folded over every conf-change
/// entry at or below that index.
///
/// Not the node's *current* configuration — quorum for an entry is counted against the members
/// who could have agreed to it, which is who they were then.
fn config_in_force(node: &NodeSnapshot<'_>, index: Index) -> ConfState {
    let mut config = node.base_config.clone();
    for (entry, after) in node.lineage {
        if entry.index > index {
            break;
        }
        config = after.clone();
    }
    config
}

/// How many servers changed status — absent, learner or voter — between two configurations.
/// A single-server change moves exactly one.
fn moved_servers(from: &ConfState, to: &ConfState) -> usize {
    let mut nodes: Vec<NodeId> = from.members().chain(to.members()).collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes
        .into_iter()
        .filter(|id| status_of(from, *id) != status_of(to, *id))
        .count()
}

/// A node's place in a configuration: 0 absent, 1 learner, 2 voter.
fn status_of(config: &ConfState, id: NodeId) -> u8 {
    match (config.is_voter(id), config.is_learner(id)) {
        (true, _) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
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
