//! Snapshots: what a leader sends when the entries a follower needs no longer exist.
//!
//! A snapshot's *effect on Raft* is small enough to handle in a crate that does no I/O: "your log
//! now starts after `index`, whose term is `term`, and the membership there was this". The bytes
//! of the state machine are the store's to stream (`docs/DESIGN.md` §5); the core reads only
//! [`SnapshotMeta`](crate::SnapshotMeta). That is invariant 7 applied to consensus — Raft moves a
//! snapshot's identity, not its meaning.
//!
//! Three things about installing one are easy to get wrong, and each is a named test.
//!
//! **A snapshot replaces the log; it does not merge with it.** A follower that kept its own tail
//! past the snapshot's index would end up with a log no leader ever had. The one exception is a
//! snapshot the log already *matches* at that index, which is a compaction rather than a
//! replacement and is simply ignored — the follower is not behind, only differently packed.
//!
//! **After installing, the follower's log begins at the snapshot.** Its last index comes from the
//! metadata, not from a now-empty tail, or it would advertise an empty log to the next candidate
//! and vote for someone it should refuse (`docs/plans/phase-3.md` §6 race 4). It also rejects an
//! append whose `prev_log_index` falls below the snapshot, because it can no longer verify one.
//!
//! **A snapshot in flight is not a snapshot delivered.** The leader records the index it sent and
//! stops sending anything else to that follower, but does not count it as replicated until the
//! follower acknowledges — which it does with an ordinary
//! [`AppendEntriesResponse`](crate::Message::AppendEntriesResponse), because the question that
//! answers is exactly "what index do you now match?" (race 5).

use bytes::Bytes;

use crate::core::Raft;
use crate::error::{RaftError, Result};
use crate::message::Message;
use crate::storage::LogStorage;
use crate::types::{NodeId, Snapshot};

impl<S: LogStorage> Raft<S> {
    /// Sends the follower a snapshot, because what it needs has been compacted away.
    pub(crate) fn send_snapshot(&mut self, to: NodeId) -> Result<()> {
        let snapshot = match self.log.snapshot() {
            Ok(snapshot) if snapshot.is_empty() => {
                // Nothing has been compacted, so the follower's gap is not one a snapshot can
                // close. This means the leader's own view of `next` is wrong; probing will find
                // the truth.
                tracing::warn!(
                    id = self.id,
                    follower = to,
                    "a follower needs entries this node does not have, and there is no snapshot"
                );
                if let Some(progress) = self.progress.get_mut(to) {
                    progress.become_probe();
                }
                return Ok(());
            }
            Ok(snapshot) => snapshot,
            Err(RaftError::SnapshotTemporarilyUnavailable) => {
                // One is being built. Retrying later is right; treating the follower as
                // unreachable is not.
                tracing::debug!(id = self.id, follower = to, "snapshot not ready yet");
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        let index = snapshot.meta.index;
        tracing::debug!(id = self.id, follower = to, index, "sending a snapshot");
        self.send(Message::InstallSnapshot {
            from: self.id,
            to,
            term: self.term,
            snapshot,
        });
        if let Some(progress) = self.progress.get_mut(to) {
            // Recorded as in flight, *not* as matched: nothing is replicated until the follower
            // says so.
            progress.become_snapshot(index);
        }
        Ok(())
    }

    /// Installs a snapshot from the leader, or explains why it was not needed.
    pub(crate) fn handle_install_snapshot(
        &mut self,
        from: NodeId,
        snapshot: Snapshot,
    ) -> Result<()> {
        self.leader = Some(from);
        self.election_elapsed = 0;

        let index = snapshot.meta.index;
        let acknowledged = if self.log.should_restore(&snapshot) {
            self.restore(snapshot)?;
            tracing::debug!(id = self.id, index, "installed a snapshot");
            self.log.last_index()?
        } else {
            // Already at or past it, or the log already agrees there. Acknowledging at the commit
            // index tells the leader where this node actually is, so it resumes with entries
            // rather than sending the same snapshot again.
            tracing::debug!(
                id = self.id,
                index,
                "ignored a snapshot the log has already passed"
            );
            self.log.committed
        };

        self.send(Message::AppendEntriesResponse {
            from: self.id,
            to: from,
            term: self.term,
            reject: false,
            index: acknowledged,
            hint_term: 0,
            context: Bytes::new(),
        });
        Ok(())
    }

    /// Adopts a snapshot wholesale: the log below it is gone, and so is the membership history
    /// that produced the configuration it carries.
    pub(crate) fn restore(&mut self, snapshot: Snapshot) -> Result<()> {
        let conf = snapshot.meta.conf.clone();
        let index = snapshot.meta.index;
        self.log.restore(snapshot);
        // A snapshot's configuration is authoritative at its index, and nothing below it can be
        // truncated any more, so the tracker starts again from there rather than keeping a history
        // that no longer has entries behind it.
        self.conf.reset(conf, index);
        self.rebuild_progress()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::config::Config;
    use crate::core::Role;
    use crate::message::Message;
    use crate::progress::ProgressState;
    use crate::raw_node::RawNode;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
    use crate::types::{ConfState, Entry, HardState, Index, Snapshot, SnapshotMeta, Term};

    fn voters() -> ConfState {
        ConfState::from_voters(vec![1, 2, 3])
    }

    fn follower(terms: &[Term], hard_term: Term) -> RawNode<MemStorage> {
        let mut storage = MemStorage::with_conf_state(voters());
        let entries: Vec<Entry> = terms
            .iter()
            .enumerate()
            .map(|(at, term)| Entry::empty(*term, at as Index + 1))
            .collect();
        storage.append(&entries).unwrap();
        storage.set_hard_state(HardState {
            term: hard_term,
            voted_for: None,
            commit: 0,
        });
        RawNode::new(Config::new(2, vec![1, 2, 3], 201), storage).unwrap()
    }

    fn snapshot(index: Index, term: Term) -> Snapshot {
        Snapshot {
            meta: SnapshotMeta {
                index,
                term,
                conf: voters(),
            },
            data: Bytes::from_static(b"machine state"),
        }
    }

    fn install(index: Index, term: Term) -> Message {
        Message::InstallSnapshot {
            from: 1,
            to: 2,
            term: 5,
            snapshot: snapshot(index, term),
        }
    }

    /// A snapshot replaces the log; it does not merge with it. A follower that kept its own tail
    /// would end up with a log no leader ever had.
    #[test]
    fn installing_a_snapshot_replaces_the_log_and_acknowledges_its_index() {
        let mut node = follower(&[1, 1, 2], 5);
        node.step(install(9, 4)).unwrap();

        assert_eq!(node.status().last_index, 9);
        assert_eq!(node.commit_index(), 9);
        assert_eq!(node.leader(), Some(1));
        let ready = node.ready();
        assert_eq!(
            ready.snapshot.as_ref().map(|snapshot| snapshot.meta.index),
            Some(9)
        );
        assert!(
            matches!(
                ready.messages.as_slice(),
                [Message::AppendEntriesResponse {
                    reject: false,
                    index: 9,
                    ..
                }]
            ),
            "a snapshot is acknowledged with the index it left the follower at"
        );
    }

    /// A snapshot that arrives while entries are still unpersisted has to discard them too. They
    /// are part of the log it replaces, and a tail that survived would leave the node claiming a
    /// last index from a history the snapshot says never happened.
    #[test]
    fn a_snapshot_discards_an_unpersisted_tail() {
        let mut node = follower(&[1], 5);
        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 5,
            prev_log_index: 1,
            prev_log_term: 1,
            entries: vec![Entry::empty(5, 2), Entry::empty(5, 3)],
            leader_commit: 0,
            context: Bytes::new(),
        })
        .unwrap();
        assert_eq!(
            node.status().last_index,
            3,
            "the tail is in the log but not yet on disk"
        );

        // A snapshot from a history that never had those entries arrives before the driver writes
        // them.
        node.step(install(9, 4)).unwrap();
        assert_eq!(node.status().last_index, 9);
        let ready = node.ready();
        assert!(
            ready.entries.is_empty(),
            "the unpersisted tail survived a snapshot that replaced it"
        );
        assert_eq!(ready.snapshot.map(|snapshot| snapshot.meta.index), Some(9));
    }

    /// Race 4. After installing, the follower's last index comes from the snapshot's metadata —
    /// its log tail is empty. A follower that reported an empty log would vote for a candidate it
    /// must refuse, and that candidate could then overwrite committed entries.
    #[test]
    fn a_node_that_installed_a_snapshot_still_refuses_a_shorter_candidate() {
        let mut node = follower(&[1], 5);
        node.step(install(9, 4)).unwrap();
        let ready = node.ready();
        node.storage_mut()
            .apply_snapshot(ready.snapshot.clone().unwrap())
            .unwrap();
        node.advance(&ready);

        node.step(Message::RequestVote {
            from: 3,
            to: 2,
            term: 6,
            last_log_index: 4,
            last_log_term: 4,
            pre_vote: false,
            // Forced, so check-quorum's lease — which this node holds, having just heard from its
            // leader — does not veto the request before the log comparison happens. That
            // comparison is what this test is about.
            force: true,
        })
        .unwrap();
        assert!(
            matches!(
                node.ready().messages.as_slice(),
                [Message::RequestVoteResponse { granted: false, .. }]
            ),
            "the snapshot's index is this node's log position, and it is ahead of the candidate"
        );
    }

    /// Race 5. An append the follower can no longer verify — its `prev_log_index` is below the
    /// snapshot — has to be refused, not guessed at.
    #[test]
    fn an_append_below_the_installed_snapshot_is_refused() {
        let mut node = follower(&[1], 5);
        node.step(install(9, 4)).unwrap();
        let ready = node.ready();
        node.storage_mut()
            .apply_snapshot(ready.snapshot.clone().unwrap())
            .unwrap();
        node.advance(&ready);

        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 5,
            prev_log_index: 3,
            prev_log_term: 1,
            entries: vec![Entry::empty(5, 4)],
            leader_commit: 4,
            context: Bytes::new(),
        })
        .unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [Message::AppendEntriesResponse { reject: true, .. }]
        ));
        assert_eq!(node.status().last_index, 9, "and the log was not rewound");
    }

    /// A snapshot the log has already passed says nothing new. Ignoring it is right; the
    /// acknowledgement tells the leader where this node actually is so it resumes with entries.
    #[test]
    fn a_snapshot_the_log_has_passed_is_acknowledged_but_not_installed() {
        let mut node = follower(&[1, 1, 1], 5);
        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 5,
            prev_log_index: 3,
            prev_log_term: 1,
            entries: Vec::new(),
            leader_commit: 3,
            context: Bytes::new(),
        })
        .unwrap();
        let _ = node.ready();
        assert_eq!(node.commit_index(), 3);

        node.step(install(2, 1)).unwrap();
        let ready = node.ready();
        assert!(
            ready.snapshot.is_none(),
            "a stale snapshot must not be installed"
        );
        assert!(matches!(
            ready.messages.as_slice(),
            [Message::AppendEntriesResponse {
                reject: false,
                index: 3,
                ..
            }]
        ));
    }

    /// Invariant 9. A snapshot claiming the end of the index space is not real, and the
    /// arithmetic that follows one would wrap. It is refused as a value, not as a panic.
    #[test]
    fn a_snapshot_at_the_end_of_the_index_space_is_refused() {
        let mut node = follower(&[1], 5);
        node.step(install(Index::MAX, 4)).unwrap();
        assert_eq!(node.status().last_index, 1, "the log was left alone");
        assert!(node.ready().snapshot.is_none());
    }

    /// The leader's half: a follower whose entries have been compacted away is sent a snapshot,
    /// and until it acknowledges, nothing else is sent to it and it counts as replicated to
    /// nothing.
    #[test]
    fn a_compacted_leader_sends_a_snapshot_and_waits_for_it() {
        let mut group = Harness::new(&[1, 2, 3], 202);
        group.campaign(1);
        group.settle();
        group.isolate(3);
        for _ in 0..4 {
            group.propose(1, b"x");
        }

        // The leader compacts past everything node 3 has.
        let compact_to = group.commit_of(1);
        group.node_mut(1).storage_mut().compact(compact_to).unwrap();
        group.heal();
        group.tick_and_settle(6);

        assert_eq!(
            group.commit_of(3),
            compact_to,
            "node 3 caught up through a snapshot"
        );
        assert_eq!(
            group.node(3).status().last_index,
            group.node(1).status().last_index
        );
        assert_eq!(group.log_of(3), group.log_of(1));
    }

    /// A snapshot in flight is not a snapshot delivered: the leader records the index but does not
    /// count it as matched, and sends nothing else until the follower answers.
    #[test]
    fn a_snapshot_in_flight_is_not_counted_as_replicated() {
        let mut leader = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1, 2, 3], 203)
            },
            MemStorage::with_conf_state(voters()),
        )
        .unwrap();
        leader.campaign().unwrap();
        for voter in [2, 3] {
            leader
                .step(Message::RequestVoteResponse {
                    from: voter,
                    to: 1,
                    term: 1,
                    granted: true,
                    pre_vote: false,
                })
                .unwrap();
        }
        let ready = leader.ready();
        leader.storage_mut().append(&ready.entries).unwrap();
        leader.advance(&ready);
        for _ in 0..3 {
            leader.propose(Bytes::from_static(b"x")).unwrap();
        }
        let ready = leader.ready();
        leader.storage_mut().append(&ready.entries).unwrap();
        leader.advance(&ready);
        leader.storage_mut().compact(3).unwrap();

        // Node 2 rejects from below the boundary, so the leader has nothing left to send it.
        leader
            .step(Message::AppendEntriesResponse {
                from: 2,
                to: 1,
                term: 1,
                reject: true,
                index: 1,
                hint_term: 1,
                context: Bytes::new(),
            })
            .unwrap();

        let progress = leader.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(progress.state, ProgressState::Snapshot);
        assert_eq!(progress.pending_snapshot, 3);
        assert_eq!(
            progress.matched, 0,
            "a snapshot in flight replicates nothing yet"
        );
        let ready = leader.ready();
        assert!(
            ready
                .messages
                .iter()
                .any(|message| matches!(message, Message::InstallSnapshot { .. }))
        );

        // While it is in flight, nothing more goes out to that follower.
        leader.propose(Bytes::from_static(b"y")).unwrap();
        let ready = leader.ready();
        assert!(
            !ready
                .messages
                .iter()
                .any(|message| message.recipient() == 2),
            "the leader kept sending to a follower that is installing a snapshot"
        );
    }

    /// A snapshot that does not take puts the follower back into probing rather than leaving the
    /// leader waiting on it forever.
    #[test]
    fn a_rejected_snapshot_returns_the_follower_to_probing() {
        let mut node = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1, 2, 3], 204)
            },
            MemStorage::with_conf_state(voters()),
        )
        .unwrap();
        node.campaign().unwrap();
        for voter in [2, 3] {
            node.step(Message::RequestVoteResponse {
                from: voter,
                to: 1,
                term: 1,
                granted: true,
                pre_vote: false,
            })
            .unwrap();
        }
        node.raft_mut()
            .progress
            .get_mut(2)
            .unwrap()
            .become_snapshot(5);
        assert!(node.raft_mut().progress.get(2).unwrap().is_paused());

        node.step(Message::AppendEntriesResponse {
            from: 2,
            to: 1,
            term: 1,
            reject: true,
            index: 1,
            hint_term: 0,
            context: Bytes::new(),
        })
        .unwrap();
        assert_eq!(
            node.raft_mut().progress.get(2).unwrap().state,
            ProgressState::Probe
        );
    }

    /// A candidate that receives a snapshot concedes, exactly as it does for an append: a snapshot
    /// is an append that could not be expressed as one.
    #[test]
    fn a_candidate_concedes_to_a_snapshot_from_its_own_term() {
        let mut node = follower(&[1], 4);
        node.campaign().unwrap();
        node.step(Message::RequestVoteResponse {
            from: 1,
            to: 2,
            term: 5,
            granted: true,
            pre_vote: true,
        })
        .unwrap();
        assert_eq!(node.role(), Role::Candidate);

        let term = node.term();
        node.step(Message::InstallSnapshot {
            from: 1,
            to: 2,
            term,
            snapshot: snapshot(9, 4),
        })
        .unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.leader(), Some(1));
    }
}
