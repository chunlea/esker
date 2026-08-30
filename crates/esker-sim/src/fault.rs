//! What can go wrong to a message, declared up front.
//!
//! A scenario says which faults it is testing rather than letting them happen by accident, so
//! a failing test names the conditions that produced it and a passing one is evidence about
//! something specific.

/// Everything that can go wrong in a scenario, declared as probabilities.
///
/// Two layers read this. [`SimNetwork`](crate::net::SimNetwork) reads the message-level
/// fields — drop, duplicate, reorder, latency — and applies them to every send. A cluster
/// driver (`crate::raft`'s cluster harness) reads the node-level ones — partition, heal, crash,
/// restart, slow disk — and draws one action per event from them. Both draw from the same
/// seeded generator, so the whole run is still one number.
///
/// Reordering is not a separate mechanism: messages are delivered in the order their
/// deadlines fall, so a wide latency range reorders on its own. [`FaultPlan::reorder`] adds a
/// deliberate extra delay on top, which produces the large, obvious inversions that a
/// scenario wants to test on purpose.
#[derive(Debug, Clone, PartialEq)]
pub struct FaultPlan {
    /// Probability that a message is dropped and never delivered.
    pub drop: f64,
    /// Probability that a message is delivered twice, each copy with its own latency.
    pub duplicate: f64,
    /// Probability that a message is given an extra delay of up to [`FaultPlan::reorder_extra_ms`].
    pub reorder: f64,
    /// Lower bound of the ordinary delivery latency, in milliseconds.
    pub min_latency_ms: u64,
    /// Upper bound of the ordinary delivery latency, in milliseconds.
    pub max_latency_ms: u64,
    /// Largest extra delay a reordered message can be given, in milliseconds.
    pub reorder_extra_ms: u64,

    // --- node-level faults, read by a cluster driver rather than by the network ---
    /// Probability that an event cuts the cluster into two halves, replacing any partition
    /// already in place.
    pub partition: f64,
    /// Probability that an event heals every partition at once.
    pub heal: f64,
    /// Probability that an event kills a running node. A killed node loses everything the
    /// driver had not made durable.
    pub crash: f64,
    /// Probability that an event brings a crashed node back, rebuilt from exactly what was
    /// durable when it died.
    pub restart: f64,
    /// Probability that a node's persistence is slow: the `Ready` it is discharging is held,
    /// and nothing from that `Ready` — no message, no applied entry — happens until the write
    /// completes. This is the fault the driver contract exists for.
    pub slow_disk: f64,
    /// The largest number of events a slow write is held for.
    pub slow_disk_events: u64,
    /// Probability that an event asks the leader to add or remove one server.
    ///
    /// Membership is the fault that is not a fault: the cluster is *supposed* to survive it,
    /// and it changes who a quorum is while everything else is still going wrong.
    pub membership: f64,
    /// How many applied entries a node keeps before compacting its log into a snapshot; `0`
    /// never compacts.
    ///
    /// This is what makes `InstallSnapshot` reachable: a follower that was partitioned away
    /// while the leader compacted past it cannot be repaired by `AppendEntries`, because the
    /// entries it needs are gone.
    pub compact_after: u64,
}

impl FaultPlan {
    /// A network that loses nothing and delivers in 1 ms. The baseline a scenario starts from
    /// when it wants to prove that a protocol works before proving it survives.
    #[must_use]
    pub fn perfect() -> Self {
        Self {
            drop: 0.0,
            duplicate: 0.0,
            reorder: 0.0,
            min_latency_ms: 1,
            max_latency_ms: 1,
            reorder_extra_ms: 0,
            partition: 0.0,
            heal: 0.0,
            crash: 0.0,
            restart: 0.0,
            slow_disk: 0.0,
            slow_disk_events: 0,
            membership: 0.0,
            compact_after: 0,
        }
    }

    /// A plausible local network: a few milliseconds of latency, nothing lost.
    #[must_use]
    pub fn lossless() -> Self {
        Self {
            min_latency_ms: 1,
            max_latency_ms: 10,
            ..Self::perfect()
        }
    }

    /// A deliberately hostile network: one message in ten lost, one in twenty duplicated, one
    /// in five badly delayed. Not realistic, which is the point — a protocol that survives
    /// this survives a bad day. Nodes and disks are healthy; see [`FaultPlan::chaotic`].
    #[must_use]
    pub fn hostile() -> Self {
        Self {
            drop: 0.1,
            duplicate: 0.05,
            reorder: 0.2,
            min_latency_ms: 1,
            max_latency_ms: 50,
            reorder_extra_ms: 500,
            ..Self::perfect()
        }
    }

    /// A hostile network *and* nodes that die, come back and write to slow disks. The plan the
    /// seed sweeps spend most of their time in.
    ///
    /// The probabilities are deliberately lopsided: healing is likelier than partitioning and
    /// restarting likelier than crashing, so the cluster spends most of a run able to make
    /// progress. A plan that is broken more often than not proves only that a broken cluster
    /// does nothing.
    #[must_use]
    pub fn chaotic() -> Self {
        Self {
            partition: 0.02,
            heal: 0.05,
            crash: 0.01,
            restart: 0.06,
            slow_disk: 0.15,
            slow_disk_events: 12,
            ..Self::hostile()
        }
    }

    /// Nodes that crash and restart on a perfect network: the plan that isolates the
    /// persistence boundary from everything else.
    #[must_use]
    pub fn crashy() -> Self {
        Self {
            crash: 0.02,
            restart: 0.08,
            slow_disk: 0.25,
            slow_disk_events: 16,
            ..Self::lossless()
        }
    }

    /// The plan for five nodes, tuned so that an entry can be replicated to a *minority* and
    /// then outrun by a later leader — the interleaving §5.4.2's term condition exists to make
    /// safe, and the one the three-node model checker cannot reach.
    ///
    /// It needs a particular shape, not just more chaos: partitions often enough that a leader
    /// gets isolated with one follower after replicating to it, healing often enough that the
    /// other three elect someone, and crashes rare enough that the minority pair survives to
    /// come back with an entry nobody else has. Slow disks widen every window.
    /// `esker-sim`'s sweep asserts the interleaving was reached, so a plan that stopped
    /// producing it would fail rather than pass quietly.
    #[must_use]
    pub fn figure_eight() -> Self {
        Self {
            partition: 0.06,
            heal: 0.08,
            crash: 0.01,
            restart: 0.08,
            slow_disk: 0.2,
            slow_disk_events: 14,
            drop: 0.02,
            duplicate: 0.02,
            reorder: 0.1,
            min_latency_ms: 1,
            max_latency_ms: 30,
            reorder_extra_ms: 200,
            membership: 0.0,
            compact_after: 0,
        }
    }

    /// Servers joining and leaving while everything else goes wrong, with compaction on so a
    /// joiner may need a snapshot rather than an append (`prompts/03-raft.md` 3d).
    #[must_use]
    pub fn reconfiguring() -> Self {
        Self {
            membership: 0.06,
            compact_after: 4,
            ..Self::chaotic()
        }
    }

    /// Everything [`FaultPlan::chaotic`] does, plus a leader that compacts aggressively — so a
    /// follower that comes back from a partition finds the entries it needs are gone and has to
    /// be repaired with a snapshot (`prompts/03-raft.md` 3d).
    #[must_use]
    pub fn compacting() -> Self {
        Self {
            compact_after: 4,
            ..Self::chaotic()
        }
    }

    /// Whether this plan can ever stop a message from arriving. A scenario that expects
    /// progress within a bound needs to know.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.drop <= 0.0 && self.partition <= 0.0 && self.crash <= 0.0 && self.slow_disk <= 0.0
    }

    /// Latency bounds with `min <= max`, so a mis-ordered plan cannot silently make every
    /// message arrive instantly.
    #[must_use]
    pub fn latency_bounds(&self) -> (u64, u64) {
        let low = self.min_latency_ms.min(self.max_latency_ms);
        let high = self.min_latency_ms.max(self.max_latency_ms);
        (low, high)
    }
}

impl Default for FaultPlan {
    fn default() -> Self {
        Self::lossless()
    }
}

#[cfg(test)]
mod tests {
    use super::FaultPlan;

    #[test]
    fn a_perfect_network_injects_nothing() {
        let plan = FaultPlan::perfect();
        assert!(plan.is_quiet());
        assert!(plan.crash <= 0.0 && plan.partition <= 0.0 && plan.slow_disk <= 0.0);
        // Stated as "no chance of", not as float equality: `chance` clamps and compares
        // against a threshold, so any non-positive probability means the fault never fires.
        assert!(plan.drop <= 0.0);
        assert!(plan.duplicate <= 0.0);
        assert!(plan.reorder <= 0.0);
        assert_eq!(plan.latency_bounds(), (1, 1));
    }

    #[test]
    fn inverted_latency_bounds_are_normalised() {
        let plan = FaultPlan {
            min_latency_ms: 90,
            max_latency_ms: 10,
            ..FaultPlan::lossless()
        };
        assert_eq!(plan.latency_bounds(), (10, 90));
    }

    /// Every node-level fault the chaotic plan declares has to be reachable, and healing has
    /// to be likelier than breaking or a sweep proves nothing but that a broken cluster is
    /// quiet.
    #[test]
    fn the_chaotic_plan_can_still_make_progress() {
        let plan = FaultPlan::chaotic();
        assert!(!plan.is_quiet());
        assert!(plan.partition > 0.0 && plan.crash > 0.0 && plan.slow_disk > 0.0);
        assert!(plan.heal > plan.partition, "partitions would accumulate");
        assert!(plan.restart > plan.crash, "the cluster would bleed nodes");
        assert!(
            plan.slow_disk_events > 0,
            "a slow disk with no delay is a fast disk"
        );
    }

    #[test]
    fn the_hostile_plan_actually_injects_faults() {
        let plan = FaultPlan::hostile();
        assert!(plan.drop > 0.0 && plan.duplicate > 0.0 && plan.reorder > 0.0);
        let (low, high) = plan.latency_bounds();
        assert!(
            high > low,
            "a hostile network with a fixed latency cannot reorder"
        );
    }
}
