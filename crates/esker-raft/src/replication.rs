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
        let index = self.log.last_index()?.saturating_add(1);
        let entry = Entry {
            term: self.term,
            index,
            kind,
            data,
        };
        self.log.append(vec![entry.clone()])?;
        // §4.1: a configuration takes effect here, at the append, not when the entry commits.
        self.record_conf_changes(&[entry])?;
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
            //
            // **And send it even though probing is paused.** For this follower the probe *is* the
            // heartbeat: there is no message the leader can anchor at `matched`, so the ordinary
            // heartbeat — which is never subject to flow control — has nowhere else to go. Leaving
            // the pause in place wedges the peer for the rest of the term, because `probe_sent` is
            // cleared only by an answer and the one outstanding probe is the message that did not
            // arrive. Phase-4 acceptance hit exactly this: a learner caught up by snapshot, sent
            // one append, and never heard from again — `matched` 0 and `recent_active` false for
            // four minutes (`docs/plans/phase-4.md` §17).
            //
            // `Snapshot` is deliberately not unpaused here. A snapshot in flight is re-offered on
            // its own timeout (`SNAPSHOT_TIMEOUT_TICKS`), which is a far longer interval than a
            // heartbeat and is the pacing that stops a leader re-announcing megabytes every tick.
            if let Some(progress) = self.progress.get_mut(to)
                && progress.state == ProgressState::Probe
            {
                progress.probe_sent = false;
            }
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

        // **An append below this node's commit index is answered, not rejected.** Everything at or
        // below `committed` is settled — no leader can ever contradict it — so there is nothing to
        // check and the only useful thing to say is where this node actually is.
        //
        // Rejecting instead is a deadlock, and phase-4 acceptance found it. A follower caught up
        // by a snapshot has a log that begins at the snapshot's index, so an append from below it
        // cannot be verified and was refused; the leader's `maybe_decr_to` walks `next` *down* on
        // a rejection and by rule never back up, so once it had probed past the boundary it probed
        // there for ever — 526 back-offs in one run that backed off nothing, against a follower
        // that was answering every one of them. `handle_install_snapshot` already answers this way
        // for the same reason; this is the same rule for the message that carries entries.
        if prev_log_index < self.log.committed {
            self.send(Message::AppendEntriesResponse {
                from: self.id,
                to: from,
                term: self.term,
                reject: false,
                index: self.log.committed,
                hint_term: 0,
                context,
            });
            return Ok(());
        }

        // Kept before the entries are consumed: §4.1 says a configuration takes effect when its
        // entry is *appended*, so these have to be applied the moment the append succeeds. Which
        // of them count is a question only the append can answer, so the filtering waits for it.
        let mut conf_changes: Vec<Entry> = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ConfChange)
            .cloned()
            .collect();
        let appended = self
            .log
            .maybe_append(prev_log_index, prev_log_term, leader_commit, entries);
        match appended {
            Ok(Some(outcome)) => {
                // The configuration follows what the *log* did, not what the message carried.
                // A message that wrote nothing — a duplicate, a retransmission the log already
                // agrees with — moves no configuration: re-applying a change the log already held
                // reads as "the entry at that index was replaced", and would take every change
                // above it away with it while the entries themselves stay in the log.
                if let Some(from_index) = outcome.spliced_from {
                    // A truncated entry takes its configuration with it, or this node would count
                    // a quorum over members that were never added.
                    if outcome.truncated {
                        self.revert_conf_to(from_index)?;
                    }
                    conf_changes.retain(|entry| entry.index >= from_index);
                    self.record_conf_changes(&conf_changes)?;
                }
                self.advance_conf_commit();
                self.send(Message::AppendEntriesResponse {
                    from: self.id,
                    to: from,
                    term: self.term,
                    reject: false,
                    index: outcome.last,
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

        // A transfer target that has just caught up gets its `TimeoutNow` now rather than at the
        // next tick.
        self.maybe_finish_transfer(from)?;

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
        // A committed configuration change can no longer be truncated away, so it stops being
        // revertible — and if it removed this node, this node stops leading (§4.2.2).
        self.advance_conf_commit();
        self.step_down_if_removed();
        // The first entry of this leader's term has just committed, which is what a postponed
        // read was waiting for.
        self.flush_postponed_reads()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
