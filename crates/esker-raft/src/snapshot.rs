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

use crate::core::{Raft, Role};
use crate::error::{RaftError, Result};
use crate::message::Message;
use crate::progress::ProgressState;
use crate::storage::LogStorage;
use crate::types::{NodeId, Snapshot, SnapshotStatus};

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

    /// Takes the driver's word for what became of a transfer, and ends the wait either way.
    ///
    /// Everything about `ProgressState::Snapshot` rests on the follower answering, and a transfer
    /// that never reached it produces no answer — so without this the peer is stranded until the
    /// leader loses office. Only a leader has progress to correct, and only a peer actually in
    /// `Snapshot` has a wait to end: a late report about a transfer the follower has already
    /// acknowledged must not drag a healthy peer back into probing.
    pub(crate) fn report_snapshot(&mut self, to: NodeId, status: SnapshotStatus) {
        if self.role != Role::Leader {
            return;
        }
        let Some(progress) = self.progress.get_mut(to) else {
            return;
        };
        if progress.state != ProgressState::Snapshot {
            return;
        }
        match status {
            SnapshotStatus::Finished => progress.become_probe(),
            SnapshotStatus::Failed => progress.abort_snapshot(),
        }
        tracing::debug!(
            id = self.id,
            follower = to,
            ?status,
            "a snapshot transfer was reported on; probing again"
        );
        // Nothing is sent from here. The retry rides on the next heartbeat or proposal, which is
        // what paces it: a follower that keeps failing costs one announcement per heartbeat
        // interval rather than one per report.
    }

    /// Gives up on snapshots nobody has reported on, one tick at a time.
    ///
    /// The driver that owed a report may be gone — killed mid-transfer, or holding a region this
    /// store no longer has — and a report that will never come cannot be waited for. Called from
    /// the leader's tick, so it costs one pass over the peers per tick and nothing at all for a
    /// group with no snapshot in flight.
    pub(crate) fn expire_pending_snapshots(&mut self, limit: u64) {
        let mut expired: Vec<NodeId> = Vec::new();
        for (id, progress) in self.progress.iter_mut() {
            if progress.snapshot_tick(limit) {
                progress.abort_snapshot();
                expired.push(id);
            }
        }
        for id in expired {
            tracing::warn!(
                id = self.id,
                follower = id,
                limit,
                "a snapshot was never reported on; probing again"
            );
        }
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
    use crate::storage::{LogStorage, MemStorage};
    use crate::testkit::Harness;
    use crate::types::{
        ConfState, Entry, HardState, Index, Snapshot, SnapshotMeta, SnapshotStatus, Term,
    };

    fn voters() -> ConfState {
        ConfState::from_voters(vec![1, 2, 3])
    }

    /// A leader of `{1, 2, 3}` with a log through index 5 and a snapshot at that index in flight
    /// to node 2, and nothing else to distract from it. Every test below starts here because the
    /// state that strands a replica is exactly this one.
    ///
    /// **Check-quorum off.** These tests tick out a whole snapshot timeout without either follower
    /// answering, which is precisely what a leader steps down for. Leaving it on would test the
    /// step-down instead of the thing under test.
    fn leader_awaiting_a_snapshot(seed: u64) -> RawNode<MemStorage> {
        let mut node = RawNode::new(
            Config {
                pre_vote: false,
                check_quorum: false,
                ..Config::new(1, vec![1, 2, 3], seed)
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
        // Four proposals on top of the leader's own empty entry: a log through index 5, so the
        // snapshot in flight names an index the leader actually has.
        for _ in 0..4 {
            node.propose(Bytes::from_static(b"x")).unwrap();
        }
        let ready = node.ready();
        node.storage_mut().append(&ready.entries).unwrap();
        node.advance(&ready);
        assert_eq!(node.status().last_index, 5);

        // Compacted past everything node 2 has, which is what makes a snapshot the only thing left
        // to send it — and what makes the heartbeat no help either, since a heartbeat has to be
        // anchored at an index the leader can still name a term for.
        node.storage_mut().compact(5).unwrap();
        node.step(Message::AppendEntriesResponse {
            from: 2,
            to: 1,
            term: 1,
            reject: true,
            index: 1,
            hint_term: 1,
            context: Bytes::new(),
        })
        .unwrap();

        let progress = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(progress.state, ProgressState::Snapshot);
        assert_eq!(progress.pending_snapshot, 5);
        assert!(
            progress.is_paused(),
            "the state under test is the paused one"
        );
        node
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

    /// The boundary an observer reads has to be the *core's*, and it moves the moment the core
    /// accepts a snapshot — not when the driver writes one.
    ///
    /// For the length of that window the core's commit index is at the snapshot's index and its
    /// log below it is gone, while storage still holds the entries the snapshot replaced. Anything
    /// that took the boundary from storage and the commit index from the core would be reading two
    /// different logs and would find committed entries this node no longer holds any opinion
    /// about. The simulator did exactly that, and reported it as State Machine Safety.
    #[test]
    fn the_snapshot_boundary_moves_when_the_core_accepts_it_not_when_the_driver_writes_it() {
        let mut node = follower(&[1, 1, 2], 5);
        assert_eq!(
            node.snapshot_boundary().unwrap(),
            (0, 0),
            "nothing compacted"
        );

        node.step(install(9, 4)).unwrap();
        assert_eq!(node.snapshot_boundary().unwrap(), (9, 4));
        assert_eq!(node.commit_index(), 9, "and the commit index went with it");
        assert_eq!(
            node.storage().first_index().unwrap(),
            1,
            "while storage still holds the entries it replaced"
        );

        // The driver catches up: now both agree, and they go on agreeing.
        let ready = node.ready();
        let snapshot = ready.snapshot.clone().expect("a snapshot to write");
        node.storage_mut().apply_snapshot(snapshot).unwrap();
        node.advance(&ready);
        assert_eq!(node.snapshot_boundary().unwrap(), (9, 4));
        assert_eq!(node.storage().first_index().unwrap(), 10);
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

    /// The strand itself, stated as a test: nothing the *leader* does ends a wait for a snapshot
    /// that never arrived.
    ///
    /// This is the shape that stranded a replica in `esker-store/tests/balance.rs` — the
    /// announcement was dropped by a transport that did not yet know where the new peer lived, so
    /// no bytes were ever sent, no acknowledgement was ever owed, and the leader sent that peer
    /// nothing for the rest of its term. Heartbeats do not help: a follower below the compaction
    /// boundary has no anchor to heartbeat at, so `send_heartbeat` delegates to `send_append`,
    /// which is paused for exactly this reason.
    ///
    /// The assertion is the strand, and the two tests after it are the two ways out.
    #[test]
    fn a_leader_waiting_on_a_snapshot_sends_that_follower_nothing() {
        let mut node = leader_awaiting_a_snapshot(210);
        let _ = node.ready();

        for _ in 0..(crate::SNAPSHOT_TIMEOUT_TICKS - 1) {
            node.tick();
        }
        node.propose(Bytes::from_static(b"x")).unwrap();

        assert_eq!(
            node.raft_mut().progress.get(2).unwrap().state,
            ProgressState::Snapshot,
            "the leader gave up on its own, which it has no way to do"
        );
        assert!(
            !node
                .ready()
                .messages
                .iter()
                .any(|message| message.recipient() == 2),
            "nothing reaches a follower whose snapshot is in flight"
        );
    }

    /// Way out one: the driver says the transfer failed, and the leader probes from what the
    /// follower is actually known to have.
    ///
    /// `next` is the assertion that matters. Probing from the *promised* index would send an
    /// append the follower must reject — the leader would be asking about entries it just failed
    /// to deliver — so `pending_snapshot` is forgotten before the probe point is worked out.
    #[test]
    fn a_failed_report_probes_from_what_the_follower_has() {
        let mut node = leader_awaiting_a_snapshot(211);
        let _ = node.ready();

        node.report_snapshot(2, SnapshotStatus::Failed);

        let progress = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(progress.state, ProgressState::Probe);
        assert_eq!(progress.pending_snapshot, 0, "the promise was forgotten");
        assert_eq!(
            progress.next,
            progress.matched + 1,
            "the probe restarts from the truth, not from the promise"
        );
        assert!(!progress.is_paused(), "and the leader may send again");

        // And it does: the next proposal reaches the follower rather than being swallowed.
        node.propose(Bytes::from_static(b"x")).unwrap();
        assert!(
            node.ready()
                .messages
                .iter()
                .any(|message| message.recipient() == 2),
            "the follower is still being sent nothing"
        );
    }

    /// Way out two: the driver says the bytes landed, and the leader probes from the index they
    /// carried rather than from scratch.
    ///
    /// `Finished` is a statement about the *transfer*, never about whether the follower is caught
    /// up — that stays the acknowledgement's job. What it buys is the probe point: the follower
    /// holds at least the snapshot's index, so starting below it would re-send entries the
    /// snapshot already carried.
    #[test]
    fn a_finished_report_probes_from_the_snapshot_it_delivered() {
        let mut node = leader_awaiting_a_snapshot(212);
        let _ = node.ready();

        node.report_snapshot(2, SnapshotStatus::Finished);

        let progress = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(progress.state, ProgressState::Probe);
        assert_eq!(
            progress.next, 6,
            "the snapshot at 5 is credited to the peer"
        );
        assert_eq!(
            progress.matched, 0,
            "delivered is not acknowledged; only the follower can say that"
        );
    }

    /// Way out three, for the driver that never got to report at all — a killed process, a store
    /// that lost the region under it. Slower on purpose: it must never cut short a large region's
    /// transfer, so it is the last line rather than the first.
    #[test]
    fn a_snapshot_nobody_reports_on_expires_and_the_leader_probes_again() {
        let mut node = leader_awaiting_a_snapshot(213);
        let _ = node.ready();

        for _ in 0..(crate::SNAPSHOT_TIMEOUT_TICKS - 1) {
            node.tick();
        }
        assert_eq!(
            node.raft_mut().progress.get(2).unwrap().state,
            ProgressState::Snapshot,
            "it gave up early"
        );

        node.tick();

        // The expiry does not merely change a field: the follower is offered the snapshot again,
        // which is the whole point. The leader goes back through `Probe` and straight into a fresh
        // `Snapshot` in the same tick, because a compacted follower has nothing else it can be
        // sent — so what is asserted is the *offer*, and that the clock started over with it.
        assert!(
            node.ready()
                .messages
                .iter()
                .any(|message| matches!(message, Message::InstallSnapshot { to: 2, .. })),
            "the follower was left stranded after the timeout"
        );
        let progress = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(
            progress.pending_snapshot, 5,
            "a fresh offer, not the old one"
        );
        assert_eq!(progress.snapshot_elapsed, 0, "and a fresh clock with it");
    }

    /// An acknowledgement that reaches the pending index ends the snapshot whatever became of the
    /// transfer, because there is no longer anything to wait for.
    ///
    /// This is the case a report cannot cover: the follower was caught up by some other route —
    /// an earlier snapshot it had already installed, a leader change, a duplicate delivery — so
    /// nobody owes a report at all.
    ///
    /// **`Replicate`, not `Probe`, is the assertion.** The follower has just told the leader the
    /// exact index it holds, so there is nothing left to guess at; making it prove the same thing
    /// again through a probe costs a round trip during which the leader sends it nothing. That
    /// only happens because the snapshot is made moot *inside* the acknowledgement, before the
    /// state is read — a peer still in `Snapshot` at that point takes the slower path by design,
    /// because an acknowledgement below the pending index really has not answered the question.
    #[test]
    fn an_acknowledgement_past_the_pending_index_ends_the_snapshot() {
        let mut node = leader_awaiting_a_snapshot(214);
        let _ = node.ready();

        node.step(Message::AppendEntriesResponse {
            from: 2,
            to: 1,
            term: 1,
            reject: false,
            index: 5,
            hint_term: 0,
            context: Bytes::new(),
        })
        .unwrap();

        let progress = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(progress.state, ProgressState::Replicate);
        assert_eq!(progress.pending_snapshot, 0);
        assert_eq!(progress.matched, 5);

        // And one below the pending index does not: that peer is still waiting on state it has
        // not got, so the wait stands.
        let mut node = leader_awaiting_a_snapshot(216);
        let _ = node.ready();
        node.step(Message::AppendEntriesResponse {
            from: 2,
            to: 1,
            term: 1,
            reject: false,
            index: 3,
            hint_term: 0,
            context: Bytes::new(),
        })
        .unwrap();
        assert_eq!(
            node.raft_mut().progress.get(2).unwrap().state,
            ProgressState::Probe,
            "an acknowledgement short of the snapshot goes back to guessing, not to pipelining"
        );
    }

    /// A report is a correction to a wait, so a peer that is not waiting must not be moved by one.
    ///
    /// Both halves matter. A late `Failed` about a transfer the follower has since acknowledged
    /// would drag a replicating peer back into probing and undo its pipelining; and a follower has
    /// no progress to correct at all, so a report reaching the wrong node is a no-op rather than a
    /// panic (invariant 9).
    #[test]
    fn a_report_moves_nothing_that_is_not_waiting_on_a_snapshot() {
        let mut node = leader_awaiting_a_snapshot(215);
        let _ = node.ready();
        node.step(Message::AppendEntriesResponse {
            from: 2,
            to: 1,
            term: 1,
            reject: false,
            index: 5,
            hint_term: 0,
            context: Bytes::new(),
        })
        .unwrap();
        let before = node.raft_mut().progress.get(2).unwrap().clone();

        node.report_snapshot(2, SnapshotStatus::Failed);
        let after = node.raft_mut().progress.get(2).unwrap().clone();
        assert_eq!(after.state, before.state);
        assert_eq!(after.next, before.next);
        assert_eq!(after.matched, before.matched);

        // A peer the leader has never heard of, and a report on a node that leads nothing.
        node.report_snapshot(99, SnapshotStatus::Finished);
        let mut lone = follower(&[1], 5);
        lone.report_snapshot(1, SnapshotStatus::Failed);
        assert_eq!(lone.role(), Role::Follower);
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
