//! Replication: how a leader gets its log onto everyone else's disk, and how it decides that
//! enough of them have it.
//!
//! The consistency check is the whole of Log Matching. An `AppendEntries` names the entry
//! *before* the ones it carries, by index and term; a follower that does not have that exact
//! entry refuses. Induction does the rest: if the logs agreed up to the previous entry and the
//! follower accepts these, they agree up to the new end. Nothing else in Raft has to compare logs.
//!
//! Commit advancement is the other half, and it carries the trap this phase names first. The
//! obvious rule — "an index a majority has replicated is committed" — is wrong, and famously so
//! (§5.4.2). A leader can find an entry from an *earlier* term replicated on a majority, commit
//! it, and then fail; a new leader with a shorter log may legitimately overwrite that entry,
//! because no majority ever promised anything about it *in a term that leader could see*. The fix
//! is one line: only an entry whose term is the **current** term commits by counting, and
//! everything below it commits along with it, transitively. That is why a new leader appends an
//! empty entry of its own term as its first act — it is the lever that lets the backlog commit.
//!
//! Backing off is the third piece. A rejected append could be answered by trying one index
//! earlier, which costs a round trip per entry and turns a long divergence into a long outage. The
//! follower instead reports the term it actually has at the conflict, and the leader skips back
//! past every entry of a higher term in one step — a round trip per *term*, not per entry.

use bytes::Bytes;

use crate::core::{Raft, Role};
use crate::error::Result;
use crate::message::Message;
use crate::progress::ProgressState;
use crate::storage::LogStorage;
use crate::types::{Entry, EntryKind, Index, NodeId, Term};

impl<S: LogStorage> Raft<S> {
    /// Appends `data` as an entry of the current term and starts replicating it.
    pub(crate) fn propose_entry(&mut self, kind: EntryKind, data: Bytes) -> Result<Index> {
        debug_assert_eq!(self.role, Role::Leader, "only a leader appends");
        let index = self.log.last_index()? + 1;
        self.log.append(vec![Entry {
            term: self.term,
            index,
            kind,
            data,
        }])?;
        if let Some(own) = self.progress.get_mut(self.id) {
            own.maybe_update(index);
        }
        // A single-voter group commits its own proposal immediately; anything larger needs the
        // round trip that follows.
        self.maybe_commit()?;
        self.bcast_append()?;
        Ok(index)
    }

    /// Sends whatever each peer is missing.
    pub(crate) fn bcast_append(&mut self) -> Result<()> {
        for peer in self.progress.ids() {
            if peer == self.id {
                continue;
            }
            self.send_append(peer)?;
        }
        Ok(())
    }

    /// Figure 3.1, L1: proves the leader is still there, so followers do not time out.
    ///
    /// `context` carries a [`ReadIndex`](crate::readonly) round's tag when one is outstanding.
    pub(crate) fn bcast_heartbeat(&mut self, context: &Bytes) -> Result<()> {
        for peer in self.progress.ids() {
            if peer == self.id {
                continue;
            }
            self.send_heartbeat(peer, context.clone())?;
        }
        Ok(())
    }

    /// Figure 3.1, L3: sends from `next`, or a snapshot if what the follower needs is gone.
    pub(crate) fn send_append(&mut self, to: NodeId) -> Result<()> {
        let Some(progress) = self.progress.get(to) else {
            return Ok(());
        };
        if progress.is_paused() {
            return Ok(());
        }
        let next = progress.next.max(1);
        let prev_index = next - 1;
        let last = self.log.last_index()?;

        let (Ok(prev_term), Ok(entries)) = (
            self.log.term(prev_index),
            self.log
                .slice(next, last.saturating_add(1), self.max_size_per_msg),
        ) else {
            // What this follower needs has been compacted away.
            return self.send_snapshot(to);
        };

        let last_sent = entries.last().map_or(prev_index, |entry| entry.index);
        let carries_entries = !entries.is_empty();
        let leader_commit = self.log.committed;
        self.send(Message::AppendEntries {
            from: self.id,
            to,
            term: self.term,
            prev_log_index: prev_index,
            prev_log_term: prev_term,
            entries,
            leader_commit,
            context: Bytes::new(),
        });

        if let Some(progress) = self.progress.get_mut(to) {
            match progress.state {
                // In replicate mode the leader pipelines: `next` moves ahead of what is
                // acknowledged, and the in-flight window is what stops it running away. Only a
                // message that actually carries entries counts against the window — an empty one
                // is a heartbeat by another name, and charging it would throttle the very
                // messages that tell a follower its commit index has moved.
                ProgressState::Replicate => {
                    progress.next = last_sent + 1;
                    if carries_entries {
                        progress.inflights.add(last_sent);
                    }
                }
                // In probe mode exactly one message is outstanding, because the leader is
                // guessing and more guesses would not narrow anything down.
                ProgressState::Probe => progress.probe_sent = true,
                ProgressState::Snapshot => {}
            }
        }
        Ok(())
    }

    /// A heartbeat: an append with no entries, anchored at what the follower is already known to
    /// have, so it confirms liveness without ever being rejected.
    fn send_heartbeat(&mut self, to: NodeId, context: Bytes) -> Result<()> {
        let Some(progress) = self.progress.get(to) else {
            return Ok(());
        };
        let anchor = progress.matched;
        let Ok(anchor_term) = self.log.term(anchor) else {
            // Everything this follower has is below our compaction boundary, so no heartbeat can
            // describe its position. Send what it actually needs instead.
            return self.send_append(to);
        };
        // Never advertise a commit index past what this follower holds: it would commit an entry
        // it does not have.
        let leader_commit = self.log.committed.min(anchor);
        self.send(Message::AppendEntries {
            from: self.id,
            to,
            term: self.term,
            prev_log_index: anchor,
            prev_log_term: anchor_term,
            entries: Vec::new(),
            leader_commit,
            context,
        });
        Ok(())
    }

    /// Figure 3.1, A1–A5, from the follower's side.
    pub(crate) fn handle_append_entries(
        &mut self,
        from: NodeId,
        prev_log_index: Index,
        prev_log_term: Term,
        entries: Vec<Entry>,
        leader_commit: Index,
        context: Bytes,
    ) -> Result<()> {
        // Hearing from the leader is what resets the election timer; this is F2's other half.
        self.leader = Some(from);
        self.election_elapsed = 0;

        let appended = self
            .log
            .maybe_append(prev_log_index, prev_log_term, leader_commit, entries);
        match appended {
            Ok(Some(last)) => {
                self.send(Message::AppendEntriesResponse {
                    from: self.id,
                    to: from,
                    term: self.term,
                    reject: false,
                    index: last,
                    hint_term: 0,
                    context,
                });
            }
            Ok(None) => {
                let (index, hint_term) = self.conflict_hint(prev_log_index)?;
                self.send(Message::AppendEntriesResponse {
                    from: self.id,
                    to: from,
                    term: self.term,
                    reject: true,
                    index,
                    hint_term,
                    context,
                });
            }
            Err(error) => {
                // The only way here is an append that would rewrite a *committed* entry, which no
                // correct leader sends. Refusing is the safe answer; the log is untouched.
                tracing::warn!(id = self.id, %error, "refused an append that would rewrite committed history");
                let (index, hint_term) = self.conflict_hint(prev_log_index)?;
                self.send(Message::AppendEntriesResponse {
                    from: self.id,
                    to: from,
                    term: self.term,
                    reject: true,
                    index,
                    hint_term,
                    context,
                });
            }
        }
        Ok(())
    }

    /// What to tell a leader whose append was refused.
    ///
    /// Two cases. If this log is simply shorter, the answer is its end — there is no point in the
    /// leader probing indices that cannot exist. Otherwise the index exists with a different term,
    /// and the useful answer is the *first* index of the term this node has there: everything from
    /// that point on is suspect, so the leader can discard a whole term in one round trip.
    fn conflict_hint(&self, prev_log_index: Index) -> Result<(Index, Term)> {
        let last = self.log.last_index()?;
        if prev_log_index > last {
            return Ok((last, self.log.last_term()));
        }
        let Ok(conflict_term) = self.log.term(prev_log_index) else {
            // Below the compaction boundary: the entry is committed, so it cannot be the conflict.
            // Point at the end of the log and let the leader work it out.
            return Ok((last, self.log.last_term()));
        };
        let first = self.log.first_index()?;
        let mut index = prev_log_index;
        while index > first
            && self
                .log
                .term(index - 1)
                .is_ok_and(|term| term == conflict_term)
        {
            index -= 1;
        }
        Ok((index, conflict_term))
    }

    /// Walks back from `index` while this log's term there is *greater* than `term`.
    ///
    /// The follower said "at my end of the conflict the term is `term`". Every entry this leader
    /// holds above that term is therefore from a leader the follower never accepted, and can be
    /// skipped in one step rather than one round trip each.
    fn find_conflict_by_term(&self, index: Index, term: Term) -> Index {
        let mut at = index.min(self.log.last_index().unwrap_or(0));
        while at > 0 && self.log.term(at).is_ok_and(|own| own > term) {
            at -= 1;
        }
        at
    }

    /// Figure 3.1, L3's second half, and L4.
    pub(crate) fn handle_append_response(
        &mut self,
        from: NodeId,
        reject: bool,
        index: Index,
        hint_term: Term,
        context: &Bytes,
    ) -> Result<()> {
        if self.role != Role::Leader || !self.progress.contains(from) {
            return Ok(());
        }
        // A follower cannot hold more than this leader has sent it, so an index above the leader's
        // own end is a lie or a bug. Clamping it here keeps one bad message from corrupting the
        // progress map — and keeps the arithmetic below inside the index space.
        let index = index.min(self.log.last_index()?);
        if let Some(progress) = self.progress.get_mut(from) {
            progress.recent_active = true;
        }

        if reject {
            let probe = if hint_term > 0 {
                self.find_conflict_by_term(index, hint_term)
                    .saturating_add(1)
            } else {
                index.saturating_add(1)
            };
            let mut retry = false;
            if let Some(progress) = self.progress.get_mut(from) {
                progress.maybe_decr_to(probe);
                // The outstanding probe has been answered, so another is allowed — whether or not
                // the hint moved `next`. A rejection that moves nothing means the leader is
                // already probing as far back as it can, and since index 0 matches every log,
                // that can only mean its own log has been compacted past what this follower
                // holds. The retry is what discovers that and turns into a snapshot; without it
                // the follower is heartbeated forever and never actually repaired.
                progress.probe_sent = false;
                retry = !progress.is_paused();
            }
            if retry {
                tracing::debug!(
                    id = self.id,
                    follower = from,
                    probe,
                    "backing off after a rejected append"
                );
                self.send_append(from)?;
            }
            return Ok(());
        }

        let mut advanced = false;
        if let Some(progress) = self.progress.get_mut(from) {
            advanced = progress.maybe_update(index);
            match progress.state {
                // A successful append tells the leader exactly where the logs agree, so guessing
                // is over and pipelining can start.
                ProgressState::Probe => progress.become_replicate(),
                ProgressState::Replicate => progress.inflights.free_to(index),
                ProgressState::Snapshot => progress.become_probe(),
            }
        }

        let last = self.log.last_index()?;
        if advanced && self.maybe_commit()? {
            // Every follower learns the new commit index from the next message either way; sending
            // it now is what makes a write visible in one round trip rather than two.
            self.bcast_append()?;
        } else if self
            .progress
            .get(from)
            .is_some_and(|progress| progress.matched < last)
        {
            // This one is still behind. It may have been out of contact — its probe dropped in a
            // partition — in which case nothing else will ever prompt the leader to try again.
            self.send_append(from)?;
        }
        self.record_read_ack(from, context);
        Ok(())
    }

    /// Figure 3.1, L4, **including the term condition** (§5.4.2).
    ///
    /// Returns whether the commit index moved.
    pub(crate) fn maybe_commit(&mut self) -> Result<bool> {
        let voters = self.conf.current().voters.clone();
        if voters.is_empty() {
            return Ok(false);
        }
        let quorum = voters.len() / 2 + 1;
        let mut matched: Vec<Index> = voters
            .iter()
            .map(|id| {
                self.progress
                    .get(*id)
                    .map_or(0, |progress| progress.matched)
            })
            .collect();
        // Descending, so the element at `quorum - 1` is the highest index a majority has reached.
        matched.sort_unstable_by(|left, right| right.cmp(left));
        let candidate = matched[quorum - 1];
        if candidate <= self.log.committed {
            return Ok(false);
        }

        // §5.4.2. An entry from an earlier term replicated on a majority is *not* committed: a
        // future leader with a shorter log may still legitimately overwrite it. Only the current
        // term's entries commit by counting — and when one does, everything below it commits with
        // it, which is how the backlog clears.
        match self.log.term(candidate) {
            Ok(term) if term == self.term => {}
            Ok(_) => {
                tracing::trace!(
                    id = self.id,
                    candidate,
                    "a majority holds this index, but it is from an earlier term: not committing"
                );
                return Ok(false);
            }
            // Below the compaction boundary, so already committed by definition.
            Err(_) => return Ok(false),
        }

        self.log.commit_to(candidate)?;
        // The first entry of this leader's term has just committed, which is what a postponed
        // read was waiting for.
        self.flush_postponed_reads()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::config::Config;
    use crate::core::Role;
    use crate::error::RaftError;
    use crate::message::Message;
    use crate::progress::ProgressState;
    use crate::raw_node::RawNode;
    use crate::storage::{LogStorage, MemStorage};
    use crate::testkit::Harness;
    use crate::types::{ConfState, Entry, HardState, Index, NodeId, Term};

    fn storage_with(terms: &[Term], hard_term: Term, voters: &[NodeId]) -> MemStorage {
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(voters.to_vec()));
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
        storage
    }

    /// A node that has already won an election, with the votes fed in directly so the test does
    /// not have to model the other side of the group.
    fn elected(
        voters: &[NodeId],
        storage: MemStorage,
        tweak: impl Fn(&mut Config),
    ) -> RawNode<MemStorage> {
        let mut config = Config::new(voters[0], voters.to_vec(), 5);
        config.pre_vote = false;
        tweak(&mut config);
        let mut node = RawNode::new(config, storage).unwrap();
        node.campaign().unwrap();
        let term = node.term();
        for voter in &voters[1..] {
            node.step(Message::RequestVoteResponse {
                from: *voter,
                to: voters[0],
                term,
                granted: true,
                pre_vote: false,
            })
            .unwrap();
        }
        assert_eq!(node.role(), Role::Leader);
        node
    }

    /// Puts `follower`'s progress into replicate mode by acknowledging everything sent so far.
    fn acknowledge(node: &mut RawNode<MemStorage>, follower: NodeId, index: Index) {
        let term = node.term();
        node.step(Message::AppendEntriesResponse {
            from: follower,
            to: node.status().id,
            term,
            reject: false,
            index,
            hint_term: 0,
            context: Bytes::new(),
        })
        .unwrap();
    }

    fn appends_to(messages: &[Message], to: NodeId) -> Vec<&Message> {
        messages
            .iter()
            .filter(|message| {
                matches!(message, Message::AppendEntries { .. }) && message.recipient() == to
            })
            .collect()
    }

    /// The end-to-end shape: a proposal reaches every log and every commit index.
    #[test]
    fn a_proposal_replicates_and_commits_everywhere() {
        let mut group = Harness::new(&[1, 2, 3], 31);
        group.campaign(1);
        group.settle();
        group.propose(1, b"alpha");
        group.propose(1, b"beta");

        for id in [1, 2, 3] {
            assert_eq!(
                group.commit_of(id),
                3,
                "node {id} did not commit both proposals"
            );
            let log = group.log_of(id);
            assert_eq!(log.len(), 3, "node {id} has the wrong log length");
            assert_eq!(log[1].data.as_ref(), b"alpha");
            assert_eq!(log[2].data.as_ref(), b"beta");
        }
        assert_eq!(group.log_of(1), group.log_of(2));
        assert_eq!(group.log_of(2), group.log_of(3));
    }

    /// **§5.4.2, the trap.** An entry from an *earlier* term sitting on a majority is not
    /// committed. A leader that committed it could then fail, and a new leader with a shorter log
    /// would be entitled to overwrite it — no majority ever promised anything about that entry in
    /// a term the new leader can see.
    ///
    /// The full Figure 8 interleaving that produces this in the wild is the simulator's to find;
    /// this pins the rule itself, which is where an implementation goes wrong.
    #[test]
    fn a_prior_term_entry_on_a_majority_does_not_commit_by_counting() {
        // Indices 1 and 2 are from term 1; this leader is in a later term.
        let mut node = elected(&[1, 2, 3], storage_with(&[1, 1], 1, &[1, 2, 3]), |_| {});
        let own_term = node.term();
        assert!(own_term > 1);
        // Its own no-op sits at index 3.
        assert_eq!(node.status().last_index, 3);

        // A majority (this node and node 2) now holds index 2 — but only index 2.
        node.raft_mut().progress.get_mut(2).unwrap().matched = 2;
        node.raft_mut().progress.get_mut(1).unwrap().matched = 2;
        assert!(!node.raft_mut().maybe_commit().unwrap());
        assert_eq!(
            node.commit_index(),
            0,
            "a prior-term entry must not commit by counting"
        );

        // The moment a majority holds the leader's *own* entry, that entry commits — and the
        // backlog beneath it commits with it, transitively.
        node.raft_mut().progress.get_mut(1).unwrap().matched = 3;
        node.raft_mut().progress.get_mut(2).unwrap().matched = 3;
        assert!(node.raft_mut().maybe_commit().unwrap());
        assert_eq!(
            node.commit_index(),
            3,
            "the current term's entry carries the backlog with it"
        );
    }

    /// The same rule from the other side: a new leader appends a no-op precisely so that the
    /// entries it inherited become committable without waiting for a client.
    #[test]
    fn a_new_leader_commits_its_inherited_entries_behind_its_own_no_op() {
        let mut group = Harness::with_config(&[1, 2, 3], 41, |config| {
            config.pre_vote = false;
            // Node 2 has to take over the instant node 1 goes quiet; check-quorum's lease
            // would (correctly) make it wait out an election timeout first, which this test
            // is not about.
            config.check_quorum = false;
        });
        group.campaign(1);
        group.settle();
        group.propose(1, b"inherited");
        assert_eq!(group.commit_of(1), 2);

        // Node 1 goes away; node 2 takes over and commits what it inherited, plus its own no-op.
        group.isolate(1);
        group.campaign(2);
        group.settle();
        // Node 1 is partitioned and check-quorum is off, so it still believes it leads: a
        // deposed leader that has not found out yet is a normal state, not a violation.
        assert_eq!(group.node(2).role(), Role::Leader);
        assert!(group.node(2).term() > group.node(1).term());
        assert_eq!(group.commit_of(2), 3);
        assert_eq!(group.log_of(2)[1].data.as_ref(), b"inherited");
    }

    /// A2 and A3 from the follower's side: a divergent tail is discarded, not merged.
    #[test]
    fn a_follower_replaces_a_divergent_tail() {
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 7),
            storage_with(&[1, 2, 2], 2, &[1, 2, 3]),
        )
        .unwrap();
        follower
            .step(Message::AppendEntries {
                from: 1,
                to: 2,
                term: 3,
                prev_log_index: 1,
                prev_log_term: 1,
                entries: vec![Entry::empty(3, 2), Entry::empty(3, 3), Entry::empty(3, 4)],
                leader_commit: 4,
                context: Bytes::new(),
            })
            .unwrap();
        let ready = follower.ready();
        assert!(matches!(
            ready.messages.as_slice(),
            [Message::AppendEntriesResponse {
                reject: false,
                index: 4,
                ..
            }]
        ));
        assert_eq!(follower.commit_index(), 4);
        assert_eq!(
            ready
                .entries
                .iter()
                .map(|entry| entry.term)
                .collect::<Vec<_>>(),
            vec![3, 3, 3]
        );
    }

    /// A2: the consistency check refuses what it cannot verify, and the hint says where to look.
    #[test]
    fn an_append_past_the_end_of_the_log_is_refused_with_the_logs_own_end() {
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 7),
            storage_with(&[1, 1], 1, &[1, 2, 3]),
        )
        .unwrap();
        follower
            .step(Message::AppendEntries {
                from: 1,
                to: 2,
                term: 1,
                prev_log_index: 9,
                prev_log_term: 1,
                entries: vec![Entry::empty(1, 10)],
                leader_commit: 0,
                context: Bytes::new(),
            })
            .unwrap();
        assert!(matches!(
            follower.ready().messages.as_slice(),
            [Message::AppendEntriesResponse {
                reject: true,
                index: 2,
                hint_term: 1,
                ..
            }]
        ));
    }

    /// The backoff claim, measured. Figure 7's leader and its follower (f): walking back one index
    /// at a time would take eleven round trips, and the hint takes two.
    #[test]
    fn the_rejection_hint_costs_a_round_trip_per_term_not_per_entry() {
        let leader_terms = [1, 1, 1, 4, 4, 5, 5, 6, 6, 6];
        let follower_terms = [1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3];
        let mut leader = elected(
            &[1, 2, 3],
            storage_with(&leader_terms, 6, &[1, 2, 3]),
            |_| {},
        );
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 7),
            storage_with(&follower_terms, 6, &[1, 2, 3]),
        )
        .unwrap();

        let mut round_trips = 0;
        for _ in 0..16 {
            let ready = leader.ready();
            leader.storage_mut().append(&ready.entries).unwrap();
            leader.advance(&ready);
            let to_follower: Vec<Message> = ready
                .messages
                .into_iter()
                .filter(|message| message.recipient() == 2)
                .collect();
            if to_follower.is_empty() {
                break;
            }
            round_trips += to_follower.len();
            for message in to_follower {
                follower.step(message).unwrap();
            }
            let ready = follower.ready();
            if let Some(hard_state) = ready.hard_state {
                follower.storage_mut().set_hard_state(hard_state);
            }
            follower.storage_mut().append(&ready.entries).unwrap();
            follower.advance(&ready);
            for message in ready.messages {
                leader.step(message).unwrap();
            }
            // Repaired means the logs *agree*, not merely that they are the same length — these
            // two fixtures happen to be, which is exactly the confusion the check has to avoid.
            let last = leader.status().last_index;
            if follower.status().last_index == last
                && follower.storage().entries(1, last + 1, u64::MAX).ok()
                    == leader.storage().entries(1, last + 1, u64::MAX).ok()
            {
                break;
            }
        }

        assert!(
            round_trips <= 3,
            "took {round_trips} round trips to repair eleven divergent entries"
        );
        assert_eq!(
            follower.status().last_index,
            leader.status().last_index,
            "the follower did not catch up"
        );
        let last = follower.status().last_index;
        assert_eq!(
            follower.storage().entries(1, last + 1, u64::MAX).unwrap(),
            leader.storage().entries(1, last + 1, u64::MAX).unwrap(),
        );
    }

    /// One message may be large; it may not be unbounded. The budget stops when the *next* entry
    /// would exceed it, and always carries at least one.
    #[test]
    fn an_append_is_bounded_by_the_byte_budget() {
        let payload = Bytes::from(vec![0_u8; 100]);
        let mut leader = elected(
            &[1, 2, 3],
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
            |config| {
                // Entry::cost is the payload plus a fixed allowance, so 300 admits two of these.
                config.max_size_per_msg = 300;
            },
        );
        for _ in 0..5 {
            leader.propose(payload.clone()).unwrap();
        }
        let _ = leader.ready();

        // Acknowledging the no-op moves node 2 into replicate mode and unpauses it.
        acknowledge(&mut leader, 2, 1);
        let ready = leader.ready();
        let sent = appends_to(&ready.messages, 2);
        assert_eq!(sent.len(), 1);
        let Message::AppendEntries { entries, .. } = sent[0] else {
            unreachable!()
        };
        assert_eq!(
            entries.len(),
            2,
            "the budget admits two entries of this size and no more"
        );
    }

    /// Flow control. Without it a leader with a fast log and a slow follower queues unbounded work
    /// into the transport, and the first casualty is the heartbeat that keeps it in office.
    #[test]
    fn the_in_flight_window_stops_a_leader_running_away_from_a_slow_follower() {
        let mut leader = elected(
            &[1, 2, 3],
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
            |config| {
                config.max_inflight_msgs = 3;
            },
        );
        let _ = leader.ready();
        acknowledge(&mut leader, 2, 1);
        let _ = leader.ready();
        assert_eq!(
            leader.raft_mut().progress.get(2).unwrap().state,
            ProgressState::Replicate
        );

        for _ in 0..10 {
            leader.propose(Bytes::from_static(b"x")).unwrap();
        }
        let ready = leader.ready();
        assert_eq!(
            appends_to(&ready.messages, 2).len(),
            3,
            "the leader sent past its in-flight window"
        );
    }

    /// L1. A follower that keeps hearing from its leader never starts an election, which is the
    /// entire reason heartbeats exist.
    #[test]
    fn heartbeats_keep_a_follower_from_campaigning() {
        let mut group = Harness::new(&[1, 2, 3], 51);
        group.campaign(1);
        group.settle();
        let term = group.node(1).term();

        group.tick_and_settle(300);
        assert_eq!(
            group.leaders(),
            vec![(1, term)],
            "the leader was churned by its own ticks"
        );
        assert_eq!(group.node(2).role(), Role::Follower);
        assert_eq!(group.node(3).role(), Role::Follower);
    }

    /// The other half of `a_partitioned_node_running_pre_votes_never_raises_its_term`, now that
    /// heartbeats exist: with pre-vote *and* check-quorum, a node returning from a partition
    /// rejoins as a follower without costing the cluster a term. This pair is the production
    /// configuration, so it is the pair that gets tested.
    #[test]
    fn a_returning_node_does_not_depose_a_healthy_leader() {
        let mut group = Harness::new(&[1, 2, 3], 61);
        group.campaign(1);
        group.settle();
        let term = group.node(1).term();

        group.isolate(3);
        group.tick_and_settle(120);
        assert_eq!(
            group.node(3).term(),
            term,
            "the isolated node's term never moved"
        );

        group.heal();
        group.tick_and_settle(10);
        assert_eq!(
            group.leaders(),
            vec![(1, term)],
            "the leader kept its office"
        );
        assert_eq!(group.node(3).role(), Role::Follower);
        assert_eq!(group.node(3).leader(), Some(1));
    }

    /// A minority cannot commit. The proposal is appended — a leader always appends — but it stays
    /// uncommitted until a majority has it.
    #[test]
    fn a_leader_without_a_majority_appends_but_does_not_commit() {
        let mut group = Harness::with_config(&[1, 2, 3, 4, 5], 71, |config| {
            config.check_quorum = false;
        });
        group.campaign(1);
        group.settle();
        let committed = group.commit_of(1);

        group.partition(&[1, 2]);
        group.propose(1, b"lonely");
        assert_eq!(group.node(1).status().last_index, committed + 1);
        assert_eq!(
            group.commit_of(1),
            committed,
            "two of five is not a majority"
        );

        group.heal();
        group.tick_and_settle(5);
        assert_eq!(
            group.commit_of(1),
            committed + 1,
            "and it commits once the rest return"
        );
    }

    /// A follower must never be told to commit past what it holds, or it commits an entry it does
    /// not have and can never apply.
    #[test]
    fn a_heartbeat_never_advertises_a_commit_index_past_the_follower() {
        let mut leader = elected(
            &[1, 2, 3],
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
            |_| {},
        );
        for _ in 0..3 {
            leader.propose(Bytes::from_static(b"x")).unwrap();
        }
        // Node 2 has everything and acknowledges; node 3 has nothing.
        acknowledge(&mut leader, 2, 4);
        assert_eq!(leader.commit_index(), 4);
        let _ = leader.ready();

        leader.raft_mut().bcast_heartbeat(&Bytes::new()).unwrap();
        for message in leader.ready().messages {
            if let Message::AppendEntries {
                to,
                leader_commit,
                prev_log_index,
                ..
            } = message
            {
                assert!(
                    leader_commit <= prev_log_index,
                    "told node {to} to commit {leader_commit} past its own {prev_log_index}"
                );
            }
        }
    }

    /// Proposals belong to the leader. Anywhere else the caller is told, so it can redirect rather
    /// than believe a write landed.
    #[test]
    fn a_follower_refuses_a_proposal() {
        let mut group = Harness::new(&[1, 2, 3], 81);
        group.campaign(1);
        group.settle();
        assert!(matches!(
            group.node_mut(2).propose(Bytes::from_static(b"x")),
            Err(RaftError::NotLeader)
        ));
    }

    /// A duplicated or reordered append changes nothing. Idempotence comes from the consistency
    /// check, not from a sequence number — there isn't one.
    #[test]
    fn replaying_an_append_leaves_the_log_alone() {
        let mut follower = RawNode::new(
            Config::new(2, vec![1, 2, 3], 7),
            storage_with(&[1, 1], 1, &[1, 2, 3]),
        )
        .unwrap();
        let append = Message::AppendEntries {
            from: 1,
            to: 2,
            term: 1,
            prev_log_index: 2,
            prev_log_term: 1,
            entries: vec![Entry::empty(1, 3), Entry::empty(1, 4)],
            leader_commit: 4,
            context: Bytes::new(),
        };
        follower.step(append.clone()).unwrap();
        let ready = follower.ready();
        follower.storage_mut().append(&ready.entries).unwrap();
        follower.advance(&ready);
        assert_eq!(follower.status().last_index, 4);

        follower.step(append.clone()).unwrap();
        assert_eq!(follower.status().last_index, 4);

        // And an older one, delivered late, does not shorten it.
        follower
            .step(Message::AppendEntries {
                from: 1,
                to: 2,
                term: 1,
                prev_log_index: 2,
                prev_log_term: 1,
                entries: vec![Entry::empty(1, 3)],
                leader_commit: 3,
                context: Bytes::new(),
            })
            .unwrap();
        assert_eq!(follower.status().last_index, 4);
        assert_eq!(follower.commit_index(), 4);
    }
}
