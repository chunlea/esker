//! The network: a trait the rest of the system talks to, and an in-memory implementation that
//! injects faults deterministically.
//!
//! Determinism is the whole point, so the implementation follows three rules that are easy to
//! break by accident:
//!
//! * **No `HashMap` anywhere that affects ordering.** In-flight messages live in a
//!   [`BTreeMap`] keyed by `(deadline, sequence)`, and inboxes in a `BTreeMap` keyed by node,
//!   so iteration order is the same on every run and on every machine.
//! * **Ties have a total order.** Two messages due at the same instant are separated by a
//!   monotonic sequence number, never by whichever the hasher happened to visit first.
//! * **All randomness comes from one seeded [`Pcg32`]**, drawn in a fixed order per message.
//!   No `Instant`, no OS entropy.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use esker_base::hash::hash64;
use esker_base::rng::Pcg32;
use thiserror::Error;

use crate::clock::{Clock, Millis};
use crate::fault::FaultPlan;

/// Stream selector for the generator that injects faults.
pub const FAULT_STREAM: u64 = 1;

/// Stream selector for a scenario's own choices, kept separate so that adding a coin flip to a
/// scenario does not shift the faults the network injects.
pub const SCENARIO_STREAM: u64 = 2;

/// A generator for a scenario's own decisions, seeded from the run's seed.
#[must_use]
pub fn scenario_rng(seed: u64) -> Pcg32 {
    Pcg32::new(seed, SCENARIO_STREAM)
}

/// Identifies one node — a store, a placement driver, a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Why a message could not be accepted for delivery.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    /// The destination is not a node of this network. A real transport reports the same thing
    /// when it has no address for a store.
    #[error("no route to node {0:?}")]
    Unroutable(NodeId),
}

/// A message in flight or delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Who sent it.
    pub from: NodeId,
    /// Who receives it.
    pub to: NodeId,
    /// The opaque payload. The network never interprets it.
    pub payload: Bytes,
    /// When it was handed to the network.
    pub sent_at: Millis,
    /// When it becomes visible to the receiver.
    pub deliver_at: Millis,
    /// Monotonic per-network counter that gives equal deadlines a total order.
    pub sequence: u64,
}

/// One thing that happened, recorded in order.
///
/// The trace is what a determinism test compares, so it holds a hash of each payload rather
/// than the payload itself: cheap to compare, and still sensitive to a single changed byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceEvent {
    /// A message was accepted and scheduled.
    Sent {
        /// When it was sent.
        at: Millis,
        /// Sender.
        from: NodeId,
        /// Recipient.
        to: NodeId,
        /// Its sequence number.
        sequence: u64,
        /// When it is due.
        deliver_at: Millis,
        /// Hash of the payload.
        payload: u64,
    },
    /// A message was dropped by the fault plan and will never arrive.
    Dropped {
        /// When it was sent.
        at: Millis,
        /// Sender.
        from: NodeId,
        /// Recipient.
        to: NodeId,
        /// Hash of the payload.
        payload: u64,
    },
    /// A copy of an earlier message was scheduled as well.
    Duplicated {
        /// When it was sent.
        at: Millis,
        /// Sender.
        from: NodeId,
        /// Recipient.
        to: NodeId,
        /// The copy's own sequence number.
        sequence: u64,
        /// When the copy is due.
        deliver_at: Millis,
    },
    /// A message reached its recipient's inbox.
    Delivered {
        /// When it arrived.
        at: Millis,
        /// Sender.
        from: NodeId,
        /// Recipient.
        to: NodeId,
        /// Its sequence number.
        sequence: u64,
    },
}

/// What a node can do with a network, implemented here in memory and later by the real TCP
/// transport (`docs/DESIGN.md` §11).
pub trait Network {
    /// Hands a message to the network. Returning `Ok` means it was accepted, not that it will
    /// arrive: the fault plan may already have dropped it.
    fn send(&mut self, to: NodeId, payload: Bytes) -> Result<(), NetworkError>;

    /// Takes the next message from this node's inbox, if one has arrived.
    fn try_recv(&mut self) -> Option<Envelope>;
}

/// An in-memory network with fault injection.
#[derive(Debug)]
pub struct SimNetwork {
    clock: Clock,
    rng: Pcg32,
    plan: FaultPlan,
    /// In-flight messages, keyed by `(deadline, sequence)` so the next delivery is the first
    /// entry and ties are broken by an integer rather than by hash order.
    in_flight: BTreeMap<(Millis, u64), Envelope>,
    /// One inbox per node, in node order.
    inboxes: BTreeMap<NodeId, VecDeque<Envelope>>,
    trace: Vec<TraceEvent>,
    next_sequence: u64,
}

impl SimNetwork {
    /// A network of `nodes`, with faults drawn from `seed`.
    #[must_use]
    pub fn new(seed: u64, plan: FaultPlan, nodes: &[NodeId]) -> Self {
        let mut inboxes = BTreeMap::new();
        for &node in nodes {
            inboxes.insert(node, VecDeque::new());
        }
        Self {
            clock: Clock::new(),
            rng: Pcg32::new(seed, FAULT_STREAM),
            plan,
            in_flight: BTreeMap::new(),
            inboxes,
            trace: Vec::new(),
            next_sequence: 0,
        }
    }

    /// The current instant.
    #[must_use]
    pub fn now(&self) -> Millis {
        self.clock.now()
    }

    /// Everything that has happened, in order.
    #[must_use]
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }

    /// How many messages are still in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// A handle that lets `node` use this network through the [`Network`] trait.
    pub fn node(&mut self, node: NodeId) -> NodeView<'_> {
        NodeView { net: self, node }
    }

    /// Sends `payload` from `from` to `to`, applying the fault plan.
    ///
    /// The draws happen in a fixed order — drop, latency, reorder, duplicate — so that the
    /// stream position after a send depends only on what was sent, never on timing.
    pub fn send(&mut self, from: NodeId, to: NodeId, payload: Bytes) -> Result<(), NetworkError> {
        if !self.inboxes.contains_key(&to) {
            return Err(NetworkError::Unroutable(to));
        }
        let at = self.clock.now();
        let digest = hash64(&payload);

        if self.rng.chance(self.plan.drop) {
            self.trace.push(TraceEvent::Dropped {
                at,
                from,
                to,
                payload: digest,
            });
            return Ok(());
        }

        let (sequence, deliver_at) = self.schedule(from, to, payload.clone(), at);
        self.trace.push(TraceEvent::Sent {
            at,
            from,
            to,
            sequence,
            deliver_at,
            payload: digest,
        });

        if self.rng.chance(self.plan.duplicate) {
            let (sequence, deliver_at) = self.schedule(from, to, payload, at);
            self.trace.push(TraceEvent::Duplicated {
                at,
                from,
                to,
                sequence,
                deliver_at,
            });
        }

        Ok(())
    }

    /// Draws a latency, assigns a sequence number and queues the message, returning both.
    fn schedule(&mut self, from: NodeId, to: NodeId, payload: Bytes, at: Millis) -> (u64, Millis) {
        let (low, high) = self.plan.latency_bounds();
        let mut latency = self.rng.range_inclusive(low, high);
        if self.rng.chance(self.plan.reorder) {
            latency =
                latency.saturating_add(self.rng.range_inclusive(0, self.plan.reorder_extra_ms));
        }

        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let deliver_at = at.saturating_add(latency);
        self.in_flight.insert(
            (deliver_at, sequence),
            Envelope {
                from,
                to,
                payload,
                sent_at: at,
                deliver_at,
                sequence,
            },
        );
        (sequence, deliver_at)
    }

    /// When the next message is due, if any.
    #[must_use]
    pub fn next_delivery(&self) -> Option<Millis> {
        self.in_flight.keys().next().map(|(deadline, _)| *deadline)
    }

    /// Advances to the next delivery deadline and moves every message due then into its
    /// recipient's inbox. Returns `false` when nothing is in flight.
    ///
    /// Messages due at the same instant are delivered in sequence order, which is a total
    /// order, so two runs of the same scenario deliver them the same way.
    pub fn step(&mut self) -> bool {
        let Some(deadline) = self.next_delivery() else {
            return false;
        };
        self.clock.advance_to(deadline);

        while let Some(entry) = self.in_flight.first_entry() {
            if entry.key().0 > deadline {
                break;
            }
            let envelope = entry.remove();
            self.trace.push(TraceEvent::Delivered {
                at: deadline,
                from: envelope.from,
                to: envelope.to,
                sequence: envelope.sequence,
            });
            if let Some(inbox) = self.inboxes.get_mut(&envelope.to) {
                inbox.push_back(envelope);
            }
        }
        true
    }

    /// Steps until nothing is in flight or `deadline` passes.
    pub fn run_until(&mut self, deadline: Millis) {
        while self.next_delivery().is_some_and(|due| due <= deadline) {
            self.step();
        }
        self.clock.advance_to(deadline);
    }

    /// Steps until nothing is left in flight. A scenario with periodic senders would never
    /// finish, so this is for one-shot exchanges.
    pub fn run_to_quiescence(&mut self) {
        while self.step() {}
    }

    /// Takes the next delivered message for `node`.
    pub fn recv(&mut self, node: NodeId) -> Option<Envelope> {
        self.inboxes.get_mut(&node)?.pop_front()
    }
}

/// One node's view of a [`SimNetwork`].
#[derive(Debug)]
pub struct NodeView<'a> {
    net: &'a mut SimNetwork,
    node: NodeId,
}

impl Network for NodeView<'_> {
    fn send(&mut self, to: NodeId, payload: Bytes) -> Result<(), NetworkError> {
        self.net.send(self.node, to, payload)
    }

    fn try_recv(&mut self) -> Option<Envelope> {
        self.net.recv(self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes() -> Vec<NodeId> {
        vec![NodeId(1), NodeId(2)]
    }

    #[test]
    fn a_perfect_network_delivers_exactly_once() {
        let mut net = SimNetwork::new(1, FaultPlan::perfect(), &nodes());
        net.send(NodeId(1), NodeId(2), Bytes::from_static(b"hello"))
            .unwrap();
        assert_eq!(net.in_flight(), 1);
        assert!(
            net.recv(NodeId(2)).is_none(),
            "delivered before time advanced"
        );

        net.run_to_quiescence();
        let envelope = net.recv(NodeId(2)).expect("message was not delivered");
        assert_eq!(envelope.payload, Bytes::from_static(b"hello"));
        assert_eq!(envelope.from, NodeId(1));
        assert_eq!(net.now(), Millis(1));
        assert!(net.recv(NodeId(2)).is_none(), "delivered twice");
    }

    #[test]
    fn sending_to_an_unknown_node_is_an_error() {
        let mut net = SimNetwork::new(1, FaultPlan::perfect(), &nodes());
        assert_eq!(
            net.send(NodeId(1), NodeId(99), Bytes::from_static(b"x")),
            Err(NetworkError::Unroutable(NodeId(99)))
        );
        assert_eq!(net.in_flight(), 0);
    }

    #[test]
    fn the_node_view_routes_through_the_trait() {
        let mut net = SimNetwork::new(1, FaultPlan::perfect(), &nodes());
        net.node(NodeId(1))
            .send(NodeId(2), Bytes::from_static(b"via trait"))
            .unwrap();
        net.run_to_quiescence();
        let received = net.node(NodeId(2)).try_recv().expect("nothing arrived");
        assert_eq!(received.payload, Bytes::from_static(b"via trait"));
    }

    /// Every fault the plan declares has to be reachable, or a scenario that asks for a
    /// hostile network would quietly get a friendly one.
    #[test]
    fn every_declared_fault_actually_happens() {
        let mut net = SimNetwork::new(7, FaultPlan::hostile(), &nodes());
        for i in 0..500u32 {
            net.send(
                NodeId(1),
                NodeId(2),
                Bytes::copy_from_slice(&i.to_le_bytes()),
            )
            .unwrap();
        }
        net.run_to_quiescence();

        let dropped = net
            .trace()
            .iter()
            .filter(|e| matches!(e, TraceEvent::Dropped { .. }))
            .count();
        let duplicated = net
            .trace()
            .iter()
            .filter(|e| matches!(e, TraceEvent::Duplicated { .. }))
            .count();
        assert!(dropped > 0, "the hostile plan dropped nothing");
        assert!(duplicated > 0, "the hostile plan duplicated nothing");

        // Latency spread must actually reorder deliveries relative to send order.
        let delivered: Vec<u64> = net
            .trace()
            .iter()
            .filter_map(|event| match event {
                TraceEvent::Delivered { sequence, .. } => Some(*sequence),
                _ => None,
            })
            .collect();
        assert!(
            delivered.windows(2).any(|pair| pair[0] > pair[1]),
            "no message overtook another"
        );
    }

    /// Equal deadlines must be broken by sequence, not by whatever order a hash map produced.
    #[test]
    fn equal_deadlines_are_delivered_in_send_order() {
        let mut net = SimNetwork::new(3, FaultPlan::perfect(), &nodes());
        for i in 0..16u8 {
            net.send(NodeId(1), NodeId(2), Bytes::copy_from_slice(&[i]))
                .unwrap();
        }
        net.run_to_quiescence();

        let mut received = Vec::new();
        while let Some(envelope) = net.recv(NodeId(2)) {
            received.push(envelope.sequence);
        }
        let mut sorted = received.clone();
        sorted.sort_unstable();
        assert_eq!(
            received, sorted,
            "equal deadlines were delivered out of order"
        );
    }

    #[test]
    fn run_until_stops_at_its_deadline() {
        let mut net = SimNetwork::new(
            5,
            FaultPlan {
                min_latency_ms: 100,
                max_latency_ms: 100,
                ..FaultPlan::perfect()
            },
            &nodes(),
        );
        net.send(NodeId(1), NodeId(2), Bytes::from_static(b"late"))
            .unwrap();
        net.run_until(Millis(50));
        assert_eq!(net.now(), Millis(50));
        assert_eq!(net.in_flight(), 1, "a message arrived before its deadline");
        net.run_until(Millis(100));
        assert_eq!(net.in_flight(), 0);
    }
}
