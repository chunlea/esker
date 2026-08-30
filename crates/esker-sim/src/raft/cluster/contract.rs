//! The driver contract, and the checks that run after every event.
//!
//! `docs/DESIGN.md` §5 states the order and `docs/plans/phase-3.md` §10 corrects two things
//! about it. Both live here, next to the arithmetic that enforces them, because the order is
//! the part of a Raft driver that is easy to get subtly wrong and impossible to notice:
//!
//! 1. **Persist before send.** A node that tells the cluster "I have this entry" or "I voted for
//!    you" before the bytes are durable can lose both to a kill, and Raft's safety argument
//!    assumed it could not. [`Cluster::violate_persist_order`] breaks it on purpose.
//! 2. **A taken `Ready` is discharged or dies with its node.** `ready()` *moves* the messages
//!    out of the core, so a driver that takes one and drops it loses them — silently, because
//!    the state is offered again and the log still looks right.
//!    [`Cluster::discard_taken_ready`] breaks that one on purpose.
//!
//! A slow disk holds the whole `Ready`: no message, no applied entry, no `advance`. That is
//! what makes a crash during the write worth simulating at all.

use bytes::Bytes;
use esker_raft::{Index, Message, NodeId as RaftId, RawNode, Role};

use super::{Cluster, READY_ROUNDS_PER_EVENT, compaction_point, context_of};
use crate::net::NodeId as WireId;
use crate::raft::checkers::{NodeSnapshot, Violation};
use crate::raft::driver::{DiskWrite, NodeSlot};
use crate::raft::report::{Event, Failure};

impl Cluster {
    // --- the driver contract -------------------------------------------------------------

    /// Gives every node its chance to complete a disk write and to produce a `Ready`.
    pub(super) fn pump(&mut self) -> Result<(), Failure> {
        for id in self.voters.clone() {
            for _ in 0..READY_ROUNDS_PER_EVENT {
                if !self.pump_once(id)? {
                    break;
                }
            }
        }
        Ok(())
    }

    /// One round for one node. Returns whether anything happened.
    ///
    /// The order below *is* the contract: persist, then send, then apply, then advance. Only
    /// [`Cluster::violate_persist_order`] moves the send, and it moves it to before the write
    /// rather than skipping it.
    fn pump_once(&mut self, id: RaftId) -> Result<bool, Failure> {
        let pending = self
            .nodes
            .get(&id)
            .and_then(|slot| slot.pending.as_ref())
            .map(|write| write.due);
        match pending {
            Some(due) if due <= self.event => self.complete_write(id),
            // A write that has not come due blocks everything behind it. That is the point of it.
            Some(_) => Ok(false),
            None => Ok(self.take_ready(id)),
        }
    }

    /// The fsync landed: make it durable, send, apply, advance — in that order.
    fn complete_write(&mut self, id: RaftId) -> Result<bool, Failure> {
        let mut outgoing: Vec<Message> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        let compact_after = self.plan.compact_after;
        let (mut installed, mut compacted) = (false, false);
        let mut reads: Vec<Index> = Vec::new();

        let Some(slot) = self.nodes.get_mut(&id) else {
            return Ok(false);
        };
        // The `Ready` is taken out only once it is certain to be discharged. Taking it first and
        // bailing out on the next guard would drop it, and a dropped `Ready` loses its messages
        // for good (`docs/plans/phase-3.md` §10.2).
        if slot.node.is_none() {
            return Ok(false);
        }
        let Some(write) = slot.pending.take() else {
            return Ok(false);
        };
        let Some(node) = slot.node.as_mut() else {
            return Ok(false);
        };

        // 1. The fsync.
        if let Err(error) = node.storage_mut().persist(&write.ready) {
            return Err(self.driver_error(format!("persist on n{id}: {error}")));
        }
        events.push(Event::Persist {
            node: id,
            entries: write.ready.entries.len(),
            upto: write.ready.entries.last().map_or(0, |entry| entry.index),
            held: write.held,
        });

        // 2. Only now may the messages go — unless they were already sent on purpose.
        if !write.sent_early && !write.ready.messages.is_empty() {
            outgoing.extend(write.ready.messages.iter().cloned());
            events.push(Event::Emit {
                node: id,
                messages: write.ready.messages.len(),
                early: false,
            });
        }

        // 3. A snapshot the write installed is state the machine now holds without having
        //    applied it entry by entry; then apply whatever is left, in order.
        if let Some(snapshot) = &write.ready.snapshot {
            slot.install_snapshot(snapshot.meta.index);
            events.push(Event::Installed {
                node: id,
                through: snapshot.meta.index,
                term: snapshot.meta.term,
            });
            installed = true;
        }
        if !write.ready.committed_entries.is_empty() {
            slot.apply(&write.ready.committed_entries);
            events.push(Event::Apply {
                node: id,
                upto: slot.applied_index,
            });
        }

        // A read index may be ahead of *this* node's commit index — a follower's read index is
        // the leader's — but never ahead of everything the cluster has committed.
        for read in &write.ready.read_states {
            reads.push(read.index);
            events.push(Event::Read {
                node: id,
                index: read.index,
            });
        }

        // 4. Tell the core it may move on.
        if let Some(node) = slot.node.as_mut() {
            node.advance(&write.ready);
        }

        // 5. The store's own housekeeping: fold applied entries into the snapshot so the log
        //    does not grow without bound. This is what makes `InstallSnapshot` reachable — a
        //    follower that fell behind the compaction boundary cannot be repaired by an append,
        //    because the entries it needs no longer exist.
        if compact_after > 0
            && let Some(through) = compaction_point(slot, compact_after)
            && let Some(node) = slot.node.as_mut()
            && node.storage_mut().compact(through).is_ok()
        {
            compacted = true;
            events.push(Event::Compact { node: id, through });
        }
        slot.refresh();

        let high_water = self.commit_high_water();
        for index in reads {
            self.stats.reads_served += 1;
            if index > high_water {
                return Err(Failure::Safety {
                    seed: self.seed,
                    event: self.event,
                    violation: Violation::ReadIndexBeyondCommit {
                        node: id,
                        index,
                        high_water,
                    },
                    trace: self.trace_report(),
                });
            }
        }
        self.stats.readys_discharged += 1;
        if installed {
            self.stats.snapshots_installed += 1;
        }
        if compacted {
            self.stats.compactions += 1;
        }
        self.emit(outgoing);
        for event in events {
            self.record(event);
        }
        Ok(true)
    }

    /// Takes a `Ready` from the core and hands it to the disk. Returns whether the disk was
    /// fast enough that the caller should come straight back for the next round.
    fn take_ready(&mut self, id: RaftId) -> bool {
        let has_ready = self
            .nodes
            .get(&id)
            .and_then(|slot| slot.node.as_ref())
            .is_some_and(RawNode::has_ready);
        if !has_ready {
            return false;
        }

        // The disk's speed is drawn before the `Ready` is taken, so it does not depend on what
        // the `Ready` turned out to contain.
        let held = if self.rng.chance(self.plan.slow_disk) {
            self.rng
                .range_inclusive(1, self.plan.slow_disk_events.max(1))
        } else {
            0
        };
        let violate = self.violate_persist_order;
        let discard = self.discard_taken_ready;
        let mut outgoing: Vec<Message> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        let taken;

        let (leads, term) = {
            let Some(slot) = self.nodes.get_mut(&id) else {
                return false;
            };
            let Some(node) = slot.node.as_mut() else {
                return false;
            };
            let ready = node.ready();
            let leads = node.role() == Role::Leader;
            let term = node.term();
            taken = true;

            if discard {
                // The bug the rule forbids: the `Ready` goes out of scope here, and its
                // messages — which `ready()` moved out of the core — go with it.
                events.push(Event::Discarded {
                    node: id,
                    messages: ready.messages.len(),
                });
            } else {
                if violate && !ready.messages.is_empty() {
                    outgoing.extend(ready.messages.iter().cloned());
                    events.push(Event::Emit {
                        node: id,
                        messages: ready.messages.len(),
                        early: true,
                    });
                }
                slot.pending = Some(DiskWrite {
                    ready,
                    due: self.event + held,
                    held,
                    sent_early: violate,
                });
                slot.refresh();
            }
            (leads, term)
        };

        if taken {
            self.stats.readys_taken += 1;
        }
        if held > 0 {
            self.stats.slow_writes += 1;
        }
        if leads && self.checker.leader_of(term).is_none() {
            events.push(Event::Lead { node: id, term });
            self.stats.elections += 1;
        }
        self.emit(outgoing);
        for event in events {
            self.record(event);
        }
        held == 0
    }

    /// Hands messages to the network, one token each.
    fn emit(&mut self, messages: Vec<Message>) {
        for message in messages {
            let (from, to) = (message.sender(), message.recipient());
            let token = self.next_token;
            self.next_token += 1;
            if let Some(context) = context_of(&message)
                && !context.is_empty()
            {
                self.contexts.insert(token, context);
            }
            self.messages.insert(token, message);
            let _ = self.net.send(
                WireId(from),
                WireId(to),
                Bytes::copy_from_slice(&token.to_le_bytes()),
            );
            self.stats.sent += 1;
        }
    }

    // --- checking ------------------------------------------------------------------------

    /// The highest commit index any node has reported, refreshed from what they hold now.
    /// Monotonic: a node whose commit index goes backwards across a restart — which is legal,
    /// a `HardState` that was never fsynced is gone — does not lower the mark.
    pub(super) fn commit_high_water(&mut self) -> Index {
        let now = self.nodes.values().map(NodeSlot::commit).max().unwrap_or(0);
        self.commit_high_water = self.commit_high_water.max(now);
        self.commit_high_water
    }

    /// The driver rule: a `Ready` that has been taken is discharged, is still on its way to the
    /// disk, or died with the node that took it. Nothing else.
    ///
    /// `docs/plans/phase-3.md` §10.2: `ready()` *moves* the messages out of the core, so a
    /// driver that takes a `Ready` and drops it loses them — silently, because the state will
    /// be offered again and the log will look fine. This is the arithmetic that catches it.
    fn check_driver_rule(&self) -> Result<(), Failure> {
        let outstanding = self
            .nodes
            .values()
            .filter(|slot| slot.pending.is_some())
            .count() as u64;
        let accounted =
            self.stats.readys_discharged + self.stats.readys_lost_to_crash + outstanding;
        if accounted == self.stats.readys_taken {
            return Ok(());
        }
        Err(self.driver_error(format!(
            "{} Ready(s) were taken but only {accounted} are accounted for              ({} discharged, {} lost with a crashed node, {outstanding} still on a disk) —              a taken Ready was dropped, and its messages went with it",
            self.stats.readys_taken, self.stats.readys_discharged, self.stats.readys_lost_to_crash,
        )))
    }

    /// Checks all four properties over every node, dead ones included.
    pub(super) fn check(&mut self) -> Result<(), Failure> {
        self.commit_high_water();
        self.check_driver_rule()?;
        let anchors: Vec<u64> = self
            .nodes
            .values()
            .map(|slot| {
                if slot.compacted_through == 0 {
                    0
                } else {
                    // The anchor goes with the entry the snapshot ends at, whose term the
                    // metadata carries — not with whatever term the node happens to be in now.
                    self.checker
                        .prefix_digest(slot.compacted_through, slot.snapshot_term)
                        .unwrap_or(0)
                }
            })
            .collect();

        let outcome = {
            let snapshots: Vec<NodeSnapshot<'_>> = self
                .nodes
                .values()
                .zip(anchors.iter())
                .map(|(slot, &prefix_anchor)| NodeSnapshot {
                    id: slot.id,
                    online: slot.online(),
                    is_leader: slot.is_leader(),
                    term: slot.term(),
                    commit: slot.commit(),
                    compacted_through: slot.compacted_through,
                    snapshot_term: slot.snapshot_term,
                    prefix_anchor,
                    settled: slot.settled(),
                    log: &slot.log,
                    applied: &slot.applied,
                })
                .collect();
            self.checker.observe(&snapshots)
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(violation) => Err(Failure::Safety {
                seed: self.seed,
                event: self.event,
                violation,
                trace: self.trace_report(),
            }),
        }
    }
}
