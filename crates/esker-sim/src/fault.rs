//! What can go wrong to a message, declared up front.
//!
//! A scenario says which faults it is testing rather than letting them happen by accident, so
//! a failing test names the conditions that produced it and a passing one is evidence about
//! something specific.

/// Probabilities and latencies applied to every message sent through a simulated network.
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
    /// this survives a bad day.
    #[must_use]
    pub fn hostile() -> Self {
        Self {
            drop: 0.1,
            duplicate: 0.05,
            reorder: 0.2,
            min_latency_ms: 1,
            max_latency_ms: 50,
            reorder_extra_ms: 500,
        }
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
