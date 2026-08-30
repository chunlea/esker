//! A minimal, *correct* driver, for the crate's own tests.
//!
//! The point of this file is that it obeys [`Ready`](crate::Ready)'s contract exactly — persist
//! `hard_state` and `entries` before sending the messages from the same `Ready`, apply the
//! snapshot first, then advance. A test that used a sloppier driver would pass while the real one
//! failed, so this one is written to the rule and nothing else.
//!
//! It is deliberately not a network simulator. There is no delay, no duplication, no reordering
//! and no crash-restart here: those live in `esker-sim`, where they belong, and the properties
//! they find are the sibling lane's. What this gives is a fast, exact answer to "given these
//! nodes and these deliveries, who leads and what did they agree on".

// TODO(step-2): the replication tests are the first callers of a few of these helpers.
#![allow(dead_code)]

use crate::config::Config;
use crate::message::Message;
use crate::raw_node::RawNode;
use crate::storage::{LogStorage, MemStorage};
use crate::types::{ConfState, Entry, Index, NodeId};

/// A group of nodes, a message queue, and a set of severed links.
pub(crate) struct Harness {
    nodes: Vec<(NodeId, RawNode<MemStorage>)>,
    queue: Vec<Message>,
    /// Directed links that drop everything, as `(from, to)`.
    severed: Vec<(NodeId, NodeId)>,
}

impl Harness {
    /// A group of voters, all starting empty.
    pub(crate) fn new(ids: &[NodeId], seed: u64) -> Self {
        Self::with_config(ids, seed, |_| {})
    }

    /// A group whose nodes are configured by `tweak` before they are built.
    pub(crate) fn with_config(ids: &[NodeId], seed: u64, tweak: impl Fn(&mut Config)) -> Self {
        let conf = ConfState::from_voters(ids.to_vec());
        let nodes = ids
            .iter()
            .map(|id| {
                let mut config = Config::new(*id, ids.to_vec(), seed);
                tweak(&mut config);
                let storage = MemStorage::with_conf_state(conf.clone());
                (
                    *id,
                    RawNode::new(config, storage).expect("valid test configuration"),
                )
            })
            .collect();
        Self {
            nodes,
            queue: Vec::new(),
            severed: Vec::new(),
        }
    }

    pub(crate) fn node(&self, id: NodeId) -> &RawNode<MemStorage> {
        &self
            .nodes
            .iter()
            .find(|(node, _)| *node == id)
            .expect("unknown node")
            .1
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> &mut RawNode<MemStorage> {
        &mut self
            .nodes
            .iter_mut()
            .find(|(node, _)| *node == id)
            .expect("unknown node")
            .1
    }

    /// One tick on every node.
    pub(crate) fn tick_all(&mut self) {
        for (_, node) in &mut self.nodes {
            node.tick();
        }
    }

    /// `count` ticks on one node.
    pub(crate) fn tick(&mut self, id: NodeId, count: u64) {
        for _ in 0..count {
            self.node_mut(id).tick();
        }
    }

    /// Makes `id` campaign now, skipping its timeout.
    pub(crate) fn campaign(&mut self, id: NodeId) {
        self.node_mut(id)
            .campaign()
            .expect("campaigning must not fail in memory");
    }

    /// Cuts every link in and out of `id`.
    pub(crate) fn isolate(&mut self, id: NodeId) {
        let ids: Vec<NodeId> = self.nodes.iter().map(|(node, _)| *node).collect();
        for other in ids {
            if other == id {
                continue;
            }
            self.severed.push((id, other));
            self.severed.push((other, id));
        }
    }

    /// Restores every link.
    pub(crate) fn heal(&mut self) {
        self.severed.clear();
    }

    /// Drains every node's `Ready` — persisting exactly as the contract requires — and queues the
    /// messages. Returns how many were queued.
    fn drain_ready(&mut self) -> usize {
        let mut outgoing = Vec::new();
        for (_, node) in &mut self.nodes {
            if !node.has_ready() {
                continue;
            }
            let ready = node.ready();

            // Rule 2: the snapshot replaces the prefix the entries continue.
            if let Some(snapshot) = &ready.snapshot {
                let _ = node.storage_mut().apply_snapshot(snapshot.clone());
            }
            // Rule 1: hard state and entries are durable *before* a single message leaves.
            if let Some(hard_state) = ready.hard_state {
                node.storage_mut().set_hard_state(hard_state);
            }
            node.storage_mut()
                .append(&ready.entries)
                .expect("test appends are contiguous");

            outgoing.extend(ready.messages.iter().cloned());
            node.advance(&ready);
        }
        let queued = outgoing.len();
        self.queue.extend(outgoing);
        queued
    }

    /// Delivers everything queued, dropping what crosses a severed link.
    fn deliver(&mut self) {
        for message in core::mem::take(&mut self.queue) {
            let link = (message.sender(), message.recipient());
            if self.severed.contains(&link) {
                continue;
            }
            if let Some((_, node)) = self.nodes.iter_mut().find(|(id, _)| *id == link.1) {
                node.step(message)
                    .expect("step must not fail on a well-formed message");
            }
        }
    }

    /// Runs deliveries until nothing is left to do, or until the bound is hit.
    ///
    /// The bound is a test failure waiting to happen rather than a silent truncation: a group that
    /// will not settle is a bug, and the assertion says which.
    pub(crate) fn settle(&mut self) {
        for _ in 0..64 {
            let queued = self.drain_ready();
            if queued == 0 && self.queue.is_empty() {
                return;
            }
            self.deliver();
        }
        panic!("the group did not settle within 64 rounds");
    }

    /// Ticks every node `count` times, settling after each tick.
    pub(crate) fn tick_and_settle(&mut self, count: u64) {
        for _ in 0..count {
            self.tick_all();
            self.settle();
        }
    }

    /// Proposes on `id` and settles.
    pub(crate) fn propose(&mut self, id: NodeId, data: &'static [u8]) {
        self.node_mut(id)
            .propose(bytes::Bytes::from_static(data))
            .expect("propose");
        self.settle();
    }

    /// Severs every link between `side` and the rest, leaving each side able to talk to itself.
    pub(crate) fn partition(&mut self, side: &[NodeId]) {
        let ids: Vec<NodeId> = self.nodes.iter().map(|(id, _)| *id).collect();
        for near in &ids {
            for far in &ids {
                if side.contains(near) != side.contains(far) {
                    self.severed.push((*near, *far));
                }
            }
        }
    }

    /// Every node that currently believes it is the leader, with its term.
    pub(crate) fn leaders(&self) -> Vec<(NodeId, crate::types::Term)> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.role() == crate::core::Role::Leader)
            .map(|(id, node)| (*id, node.term()))
            .collect()
    }

    /// The single leader, or a panic naming what it found instead.
    pub(crate) fn leader(&self) -> NodeId {
        match self.leaders().as_slice() {
            [(id, _)] => *id,
            other => panic!("expected exactly one leader, found {other:?}"),
        }
    }

    /// Every entry in a node's log, durable or not.
    pub(crate) fn log_of(&self, id: NodeId) -> Vec<Entry> {
        let node = self.node(id);
        let last = node.status().last_index;
        let storage = node.storage();
        let first = storage.first_index().unwrap_or(1);
        if last < first {
            return Vec::new();
        }
        storage
            .entries(first, last + 1, u64::MAX)
            .unwrap_or_default()
    }

    /// A node's commit index.
    pub(crate) fn commit_of(&self, id: NodeId) -> Index {
        self.node(id).commit_index()
    }
}
