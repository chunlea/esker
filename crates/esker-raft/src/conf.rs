//! Membership, and the rule that makes single-server changes safe.
//!
//! **A configuration takes effect when its entry is appended, not when it commits**
//! (dissertation §4.1). That is counter-intuitive — everything else in Raft waits for a commit —
//! and it is not an optimisation. A leader that waited for the commit would have to count the
//! quorum for that very entry under the *old* configuration while the new one is what the entry
//! establishes; the two overlap by design for a single-server change, and using the new
//! configuration immediately is what keeps the overlap from mattering.
//!
//! The price is that an uncommitted configuration can be **truncated away**, and the node must
//! then revert to what it had before. So this type is a stack, not a value: appending a change
//! pushes, truncating pops, and committing folds the settled prefix into the base
//! (`docs/plans/phase-3.md` §6 race 3).

use crate::types::{ConfChange, ConfState, Index};

/// The configuration, plus enough history to undo the part that is not committed yet.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfTracker {
    /// The configuration as of `base_index`; committed, or from a snapshot, so never undone.
    base: ConfState,
    /// The index `base` is the configuration as of.
    base_index: Index,
    /// Changes appended above `base_index`, ascending by index. Each entry records the *resulting*
    /// configuration, so reverting is a pop rather than an inverse operation — inverting
    /// "add voter 4" requires knowing whether 4 was previously a learner, and the stack knows.
    appended: Vec<(Index, ConfState)>,
}

impl ConfTracker {
    /// A tracker starting from a known configuration at `index`.
    pub(crate) fn new(base: ConfState, index: Index) -> Self {
        Self {
            base,
            base_index: index,
            appended: Vec::new(),
        }
    }

    /// The configuration in force right now — the latest in the log, committed or not.
    pub(crate) fn current(&self) -> &ConfState {
        self.appended.last().map_or(&self.base, |(_, conf)| conf)
    }

    /// Applies `change` as of the entry at `index`, returning the new configuration.
    ///
    /// **Only for an entry the log has just written.** The stack mirrors the conf-change entries
    /// the log holds above `base_index`, and it can only stay a mirror if it is moved by the same
    /// events the log is: a change that is merely *carried again* by a message the log ignored —
    /// a duplicate, a retransmission whose entries already matched — has not been appended, and
    /// recording it here reads as "the entry at `index` was replaced" and drops every change above
    /// it while those entries stay in the log. That is the shape the simulator found on seed
    /// 41213; see [`crate::log::AppendOutcome`], which is what the callers filter on.
    pub(crate) fn append(&mut self, index: Index, change: &ConfChange) -> &ConfState {
        let mut next = self.current().clone();
        change.apply_to(&mut next);
        // A repeated index means the entry at that position was replaced; drop the old one first.
        self.appended.retain(|(at, _)| *at < index);
        self.appended.push((index, next));
        self.current()
    }

    /// Reverts every change appended at or above `index` — the truncation path.
    ///
    /// Returns whether the configuration actually changed, which is what the leader needs to know:
    /// a reverted membership means the progress map has to be rebuilt.
    pub(crate) fn truncate_from(&mut self, index: Index) -> bool {
        let before = self.current().clone();
        self.appended.retain(|(at, _)| *at < index);
        *self.current() != before
    }

    /// Folds every change at or below `index` into the base, so it can no longer be undone.
    pub(crate) fn commit_to(&mut self, index: Index) {
        while let Some((at, conf)) = self.appended.first() {
            if *at > index {
                break;
            }
            self.base = conf.clone();
            self.base_index = *at;
            self.appended.remove(0);
        }
    }

    /// The index of the first configuration change that is appended but not committed, if any.
    ///
    /// A second single-server change may not be proposed while this is `Some`: overlapping changes
    /// can produce two disjoint majorities, which is exactly the split the one-at-a-time rule
    /// exists to prevent.
    pub(crate) fn pending(&self) -> Option<Index> {
        self.appended.first().map(|(at, _)| *at)
    }

    /// Adopts a configuration wholesale — a snapshot restore, where the log below `index` is gone
    /// and with it any history worth keeping.
    pub(crate) fn reset(&mut self, conf: ConfState, index: Index) {
        self.base = conf;
        self.base_index = index;
        self.appended.clear();
    }
}

use crate::core::{Raft, Role};
use crate::error::{RaftError, Result as RaftResult};
use crate::storage::LogStorage;
use crate::types::{Entry, EntryKind};

impl<S: LogStorage> Raft<S> {
    /// Proposes a single-server membership change, applying it to this node the moment the entry
    /// is appended.
    pub(crate) fn propose_conf_change(&mut self, change: &ConfChange) -> RaftResult<Index> {
        if self.role != Role::Leader {
            return Err(RaftError::NotLeader);
        }
        if let Some(at) = self.conf.pending() {
            return Err(RaftError::ConfChangePending(at));
        }
        // The tracker only knows about changes *this* node appended, and there may be a branch
        // this leader has never seen carrying one that is still in force where it was appended.
        // Committing an entry of its own term is what rules that out: it puts this branch on a
        // quorum, so every future leader has it and no other branch can commit anything again.
        if self.log.committed < self.own_term_index {
            return Err(RaftError::ConfChangePending(self.own_term_index));
        }

        if let Some(target) = self.lead_transferee {
            return Err(RaftError::LeadershipTransferInProgress(target));
        }
        // Removing the last voter would leave a group nothing could ever commit in.
        let mut after = self.conf.current().clone();
        change.apply_to(&mut after);
        if after.voters.is_empty() {
            return Err(RaftError::InvalidConfig(
                "a configuration change may not remove the last voter".into(),
            ));
        }
        self.propose_entry(EntryKind::ConfChange, change.encode())
    }

    /// Applies the configuration carried by any `ConfChange` entries in `entries`.
    ///
    /// Called from both append paths — the leader's and the follower's — because §4.1's rule is
    /// about *appending*, and both of them append. `entries` must be entries the log has just
    /// written, and only those: see [`ConfTracker::append`] for what happens otherwise.
    pub(crate) fn record_conf_changes(&mut self, entries: &[Entry]) -> RaftResult<()> {
        let mut moved = false;
        for entry in entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ConfChange)
        {
            // The bytes came off a log that may have lied; a corrupt payload is an error value,
            // never a panic (`CLAUDE.md` invariant 9).
            let change = ConfChange::decode(&entry.data)?;
            let before = self.conf.current().clone();
            self.conf.append(entry.index, &change);
            moved |= *self.conf.current() != before;
            tracing::debug!(
                id = self.id,
                index = entry.index,
                node = change.node,
                kind = ?change.kind,
                "applied a configuration change at append time"
            );
        }
        if moved {
            self.rebuild_progress()?;
        }
        Ok(())
    }

    /// Rebuilds the stack from the log, at open, on top of the anchor a snapshot provided.
    ///
    /// Every conf-change entry above the snapshot is recorded as though it had just been
    /// appended, and then the committed prefix is folded away — which leaves exactly the
    /// revertible tail. Without this a restarted node holds a configuration it cannot undo: the
    /// entry that established it is still in its log, so a new leader truncating that entry away
    /// leaves the node counting quorums over a membership its own log no longer justifies. The
    /// simulator found that as two leaders in one term, on `ESKER_SIM_SEED=42705`.
    ///
    /// The cost is one pass over the entries a snapshot does not cover, once, at startup.
    pub(crate) fn replay_conf_changes(&mut self) -> RaftResult<()> {
        let first = self.log.first_index()?;
        let last = self.log.last_index()?;
        if last < first {
            return Ok(());
        }
        let entries = self.log.slice(first, last.saturating_add(1), u64::MAX)?;
        self.record_conf_changes(&entries)?;
        self.advance_conf_commit();
        Ok(())
    }

    /// Reverts every configuration change appended at or above `index` — the truncation path.
    pub(crate) fn revert_conf_to(&mut self, index: Index) -> RaftResult<()> {
        if self.conf.truncate_from(index) {
            tracing::debug!(
                id = self.id,
                index,
                "reverted a configuration that was truncated away"
            );
            self.rebuild_progress()?;
        }
        Ok(())
    }

    /// Folds every configuration change at or below the commit index into the base, so it can no
    /// longer be undone.
    pub(crate) fn advance_conf_commit(&mut self) {
        self.conf.commit_to(self.log.committed);
    }

    /// A leader that a committed configuration change has removed steps down (§4.2.2).
    ///
    /// It cannot be a *correct* leader any more: the quorum it would count is a quorum of a group
    /// it is no longer in. Staying would let it serve reads from a cluster that has moved on.
    pub(crate) fn step_down_if_removed(&mut self) {
        if self.role == Role::Leader && !self.is_voter(self.id) {
            tracing::info!(
                id = self.id,
                term = self.term,
                "stepping down: a committed configuration change removed this node"
            );
            self.become_follower(self.term, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::config::Config;
    use crate::core::Role;
    use crate::error::RaftError;
    use crate::message::Message;
    use crate::raw_node::RawNode;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
    use crate::types::{
        ConfChange, ConfChangeKind, ConfState, Entry, EntryKind, HardState, Snapshot, SnapshotMeta,
    };

    fn add_voter(node: u64) -> ConfChange {
        ConfChange::new(ConfChangeKind::AddVoter, node)
    }

    /// **§4.1, the rule that makes single-server changes safe.** The configuration takes effect
    /// when the entry is *appended*, not when it commits — so the change is visible on the leader
    /// before any follower has acknowledged it.
    #[test]
    fn a_configuration_applies_when_its_entry_is_appended() {
        let mut group = Harness::with_config(&[1, 2, 3], 301, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();
        assert_eq!(group.node(1).status().conf.voters, vec![1, 2, 3]);

        // Propose without settling: nothing has acknowledged the entry, so it is not committed.
        group.node_mut(1).propose_conf_change(add_voter(4)).unwrap();
        assert_eq!(
            group.node(1).status().conf.voters,
            vec![1, 2, 3, 4],
            "the configuration must be in force before the entry commits"
        );
        assert!(group.node(1).commit_index() < group.node(1).status().last_index);
    }

    /// **Race 3.** The other side of the same rule: an uncommitted change that gets truncated has
    /// to take its configuration with it, or the node counts a quorum over members that were never
    /// added.
    #[test]
    fn a_truncated_configuration_change_reverts() {
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 302),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();

        // A leader in term 1 appends "add voter 4"; this follower takes it, and the configuration
        // changes on the spot.
        follower
            .step(Message::AppendEntries {
                from: 1,
                to: 2,
                term: 1,
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![Entry::empty(1, 1), Entry::conf_change(1, 2, &add_voter(4))],
                leader_commit: 1,
                context: Bytes::new(),
            })
            .unwrap();
        assert_eq!(follower.status().conf.voters, vec![1, 2, 3, 4]);
        let ready = follower.ready();
        follower.storage_mut().append(&ready.entries).unwrap();
        follower.advance(&ready);

        // That leader loses office. The new one's log has something else at index 2.
        follower
            .step(Message::AppendEntries {
                from: 3,
                to: 2,
                term: 2,
                prev_log_index: 1,
                prev_log_term: 1,
                entries: vec![Entry::empty(2, 2)],
                leader_commit: 1,
                context: Bytes::new(),
            })
            .unwrap();
        assert_eq!(
            follower.status().conf.voters,
            vec![1, 2, 3],
            "a truncated configuration change must revert"
        );
    }

    /// A committed change is settled: nothing can truncate it any more, so it stops being
    /// revertible and a second change becomes proposable.
    #[test]
    fn a_committed_change_settles_and_lets_the_next_one_through() {
        let mut group = Harness::with_config(&[1, 2, 3], 303, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();

        group.node_mut(1).propose_conf_change(add_voter(4)).unwrap();
        assert!(
            matches!(
                group.node_mut(1).propose_conf_change(add_voter(5)),
                Err(RaftError::ConfChangePending(_))
            ),
            "overlapping single-server changes can produce two disjoint majorities"
        );

        group.settle();
        // 4 does not exist in this harness, so the quorum is now 3 of 4: nodes 1, 2 and 3.
        assert_eq!(group.commit_of(1), group.node(1).status().last_index);
        assert!(group.node_mut(1).propose_conf_change(add_voter(5)).is_ok());
    }

    /// A learner replicates without voting, so adding one does not make elections harder while it
    /// catches up (§4.2.1).
    #[test]
    fn a_learner_receives_the_log_without_joining_the_quorum() {
        let mut group = Harness::with_config(&[1, 2, 3], 304, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();
        group
            .node_mut(1)
            .propose_conf_change(ConfChange::new(ConfChangeKind::AddLearner, 4))
            .unwrap();
        group.settle();

        let status = group.node(1).status();
        assert_eq!(status.conf.voters, vec![1, 2, 3]);
        assert_eq!(status.conf.learners, vec![4]);
        assert_eq!(
            status.conf.quorum(),
            2,
            "a learner does not raise the bar for a majority"
        );
    }

    /// §4.2.2. A leader a committed change removed cannot be a correct leader: the quorum it would
    /// count belongs to a group it is not in.
    #[test]
    fn a_leader_removed_by_a_committed_change_steps_down() {
        let mut group = Harness::with_config(&[1, 2, 3], 305, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();

        group
            .node_mut(1)
            .propose_conf_change(ConfChange::new(ConfChangeKind::Remove, 1))
            .unwrap();
        assert_eq!(
            group.node(1).status().conf.voters,
            vec![2, 3],
            "applied at append"
        );
        group.settle();
        assert_ne!(
            group.node(1).role(),
            Role::Leader,
            "a removed leader must step down"
        );
    }

    /// A change that would leave nothing able to commit is refused rather than accepted and
    /// mourned.
    #[test]
    fn removing_the_last_voter_is_refused() {
        let mut group = Harness::new(&[1], 306);
        group.campaign(1);
        group.settle();
        assert!(matches!(
            group
                .node_mut(1)
                .propose_conf_change(ConfChange::new(ConfChangeKind::Remove, 1)),
            Err(RaftError::InvalidConfig(_))
        ));
    }

    /// A restarting node takes its membership from its log — including an uncommitted change,
    /// because §4.1 says the latest configuration in the log is the one in force.
    #[test]
    fn a_restart_recovers_the_configuration_from_storage() {
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        storage.append(&[Entry::empty(1, 1)]).unwrap();
        storage.set_hard_state(HardState {
            term: 1,
            voted_for: None,
            commit: 1,
        });
        storage.set_conf_state(ConfState {
            voters: vec![1, 2, 3, 4],
            learners: vec![5],
        });

        let node = RawNode::new(Config::new(1, vec![9, 9, 9], 307), storage).unwrap();
        assert_eq!(node.status().conf.voters, vec![1, 2, 3, 4]);
        assert_eq!(node.status().conf.learners, vec![5]);
    }

    /// Regression, from the simulator's membership sweep (`ESKER_SIM_SEED=41213`).
    ///
    /// §4.1 says a configuration takes effect when its entry is *appended*. The other half of that
    /// sentence is the one this test is about: an `AppendEntries` that appends nothing changes no
    /// configuration. The message replayed at the end carries a change the log already holds, so
    /// the log ignores it — but re-applying it to the tracker would read as "the entry at index 3
    /// was replaced" and take the change at index 5 off the stack, leaving the node counting
    /// quorums over a configuration its own log contradicts.
    #[test]
    fn a_duplicate_append_does_not_take_a_later_configuration_away() {
        // A correct driver: persist what the `Ready` carries, then advance.
        fn persist(node: &mut RawNode<MemStorage>) {
            let ready = node.ready();
            if let Some(hard_state) = ready.hard_state {
                node.storage_mut().set_hard_state(hard_state);
            }
            node.storage_mut().append(&ready.entries).unwrap();
            node.advance(&ready);
        }

        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 309),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        let append =
            |prev_log_index, prev_log_term, leader_commit, entries| Message::AppendEntries {
                from: 1,
                to: 2,
                term: 1,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                context: Bytes::new(),
            };

        // Add voter 4 at index 2, remove it again at index 3, add it back at index 5 — three
        // proposals, each committed before the next arrives, so each folds into the base in turn.
        follower
            .step(append(
                0,
                0,
                0,
                vec![Entry::empty(1, 1), Entry::conf_change(1, 2, &add_voter(4))],
            ))
            .unwrap();
        persist(&mut follower);
        let remove_four = append(
            2,
            1,
            2,
            vec![Entry::conf_change(
                1,
                3,
                &ConfChange::new(ConfChangeKind::Remove, 4),
            )],
        );
        follower.step(remove_four.clone()).unwrap();
        persist(&mut follower);
        assert_eq!(follower.status().conf.voters, vec![1, 2, 3]);
        follower
            .step(append(
                3,
                1,
                3,
                vec![Entry::empty(1, 4), Entry::conf_change(1, 5, &add_voter(4))],
            ))
            .unwrap();
        persist(&mut follower);
        assert_eq!(follower.status().conf.voters, vec![1, 2, 3, 4]);

        // The network held on to the second message and delivers it again. Every entry it carries
        // is already in the log, byte for byte, so the log does not move.
        follower.step(remove_four).unwrap();
        assert_eq!(
            follower.status().last_index,
            5,
            "a duplicate must not shorten the log"
        );
        assert_eq!(
            follower.status().conf.voters,
            vec![1, 2, 3, 4],
            "index 5 still adds voter 4, so voter 4 is still in the configuration"
        );
    }

    /// **§4.1, and the reason a new leader waits.** A leader may not move a server until it has
    /// committed an entry of its *own term*.
    ///
    /// Everything this node inherited is committed — `commit` and `last` are both 2 when it takes
    /// office, and the change at index 2 is settled — so there is nothing it can see that is
    /// outstanding. It must still wait, because what it cannot see is the point: another node may
    /// hold a branch, higher up than anything here, carrying a configuration change that is in
    /// force where it was appended. Two changes from one committed parent are one server either
    /// side of it and so two servers, and no shared quorum, from each other.
    ///
    /// Committing an entry of its own term is exactly what rules that out. It puts this branch on
    /// a quorum, so every later leader has it (§5.4) and the other branch can never commit
    /// anything again. Waiting only for the *inherited* tail is not the same test and does not
    /// catch this: on `ESKER_SIM_SEED=53017` node 1 took office with `commit == last`, passed
    /// that weaker test on the spot, removed a server — and node 2, still holding an uncommitted
    /// removal of a different server from the same parent, went on to win a later term against a
    /// quorum that shared nobody with the one that had committed node 1's change.
    #[test]
    fn a_new_leader_refuses_a_conf_change_until_it_has_committed_its_own_term() {
        // The anchor is the membership as of index 0; index 2 is how it got to [1, 2, 3, 4].
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        storage
            .append(&[Entry::empty(1, 1), Entry::conf_change(1, 2, &add_voter(4))])
            .unwrap();
        storage.set_hard_state(HardState {
            term: 1,
            voted_for: None,
            commit: 2,
        });

        let mut config = Config::new(1, vec![1, 2, 3, 4], 310);
        config.pre_vote = false;
        let mut node = RawNode::new(config, storage).unwrap();
        assert_eq!(
            node.commit_index(),
            2,
            "everything it inherited is committed"
        );

        // It wins term 2 on votes from 2 and 3 — a quorum of four is three, counting itself.
        node.campaign().unwrap();
        for voter in [2, 3] {
            node.step(Message::RequestVoteResponse {
                from: voter,
                to: 1,
                term: 2,
                granted: true,
                pre_vote: false,
            })
            .unwrap();
        }
        assert_eq!(node.role(), Role::Leader);

        assert!(
            matches!(
                node.propose_conf_change(ConfChange::new(ConfChangeKind::Remove, 2)),
                Err(RaftError::ConfChangePending(3))
            ),
            "nothing of this leader's own term is committed yet, so its branch is not settled"
        );

        // Its own empty entry of term 2 sits at index 3. Committing that settles this branch, and
        // there is nothing left to be uncertain about.
        for follower in [2, 3] {
            node.step(Message::AppendEntriesResponse {
                from: follower,
                to: 1,
                term: 2,
                reject: false,
                index: 3,
                hint_term: 0,
                context: Bytes::new(),
            })
            .unwrap();
        }
        assert_eq!(node.commit_index(), 3, "the leader committed its own term");
        assert!(
            node.propose_conf_change(ConfChange::new(ConfChangeKind::Remove, 2))
                .is_ok()
        );
    }

    /// **Race 3, across a restart.** A configuration applied at append has to be revertible, and
    /// a restarted node can only revert what it can reconstruct.
    ///
    /// `RawNode::new` used to start with a flat stack, so the change at index 10 below — still
    /// uncommitted, still in the log — was in force and could not be taken back. When a new
    /// leader overwrote index 10, the entry went and the configuration stayed, leaving this node
    /// counting quorums of `[1, 2, 3]` while the log says `[1, 2, 3, 4]`. The simulator reached
    /// that on `ESKER_SIM_SEED=42705` and reported it as Leader Completeness: node 2 won a term
    /// with two of its three imagined voters, against a cluster whose quorum did not include
    /// either of them.
    ///
    /// The snapshot is what makes the reconstruction possible: its metadata carries the
    /// membership as of its own index, which the entries above it replay onto.
    #[test]
    fn a_restart_over_a_snapshot_can_still_revert_a_truncated_change() {
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3, 4]));
        storage
            .apply_snapshot(Snapshot {
                meta: SnapshotMeta {
                    index: 6,
                    term: 1,
                    conf: ConfState::from_voters(vec![1, 2, 3, 4]),
                },
                data: Bytes::new(),
            })
            .unwrap();
        storage
            .append(&[
                Entry::empty(1, 7),
                Entry::empty(1, 8),
                Entry::empty(1, 9),
                Entry::conf_change(1, 10, &ConfChange::new(ConfChangeKind::Remove, 4)),
            ])
            .unwrap();
        storage.set_hard_state(HardState {
            term: 1,
            voted_for: None,
            commit: 9,
        });

        let mut node = RawNode::new(Config::new(2, vec![1, 2, 3, 4], 311), storage).unwrap();
        assert_eq!(
            node.status().conf.voters,
            vec![1, 2, 3],
            "the change at index 10 is in force: §4.1 applies it at the append"
        );

        // A leader of term 2 has something else at index 10. The entry goes, and the
        // configuration it established has to go with it.
        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 2,
            prev_log_index: 9,
            prev_log_term: 1,
            entries: vec![Entry::empty(2, 10)],
            leader_commit: 9,
            context: Bytes::new(),
        })
        .unwrap();
        assert_eq!(
            node.status().conf.voters,
            vec![1, 2, 3, 4],
            "a truncated change reverts across a restart, or this node counts a quorum of a \
             membership its own log does not justify"
        );
    }

    /// The same reconstruction with nothing compacted, where the anchor is
    /// [`InitialState::conf_state`](crate::InitialState::conf_state) at index 0 rather than a
    /// snapshot's metadata.
    ///
    /// This is the case a snapshot cannot cover, and leaving it out left the hole open for any
    /// node that restarted before it had ever compacted: `ESKER_SIM_SEED=114249` found two
    /// leaders in term 4 that way, one under `[1, 2, 3]` and one under `[1, 2, 3, 4, 5]`.
    #[test]
    fn a_restart_with_nothing_compacted_can_still_revert_a_truncated_change() {
        // The anchor is the membership as of index 0, not the one in force: the log's own entries
        // say how it got from there to here.
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3, 4]));
        storage
            .append(&[
                Entry::empty(1, 1),
                Entry::conf_change(1, 2, &ConfChange::new(ConfChangeKind::Remove, 4)),
            ])
            .unwrap();
        storage.set_hard_state(HardState {
            term: 1,
            voted_for: None,
            commit: 1,
        });

        let mut node = RawNode::new(Config::new(2, vec![1, 2, 3, 4], 312), storage).unwrap();
        assert_eq!(
            node.status().conf.voters,
            vec![1, 2, 3],
            "replayed from index 0"
        );

        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 2,
            prev_log_index: 1,
            prev_log_term: 1,
            entries: vec![Entry::empty(2, 2)],
            leader_commit: 1,
            context: Bytes::new(),
        })
        .unwrap();
        assert_eq!(
            node.status().conf.voters,
            vec![1, 2, 3, 4],
            "the entry that removed 4 is gone, so 4 is back"
        );
    }

    /// Invariant 9: a corrupt `ConfChange` payload in the log is an error, not a panic.
    #[test]
    fn a_corrupt_configuration_entry_is_an_error() {
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 308),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        let corrupt = Entry {
            term: 1,
            index: 1,
            kind: EntryKind::ConfChange,
            data: Bytes::from_static(b"\xff\x00"),
        };
        let outcome = follower.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 1,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![corrupt],
            leader_commit: 0,
            context: Bytes::new(),
        });
        assert!(matches!(outcome, Err(RaftError::CorruptConfChange(_))));
        assert_eq!(
            follower.status().conf.voters,
            vec![1, 2, 3],
            "and the configuration is intact"
        );
    }
}
