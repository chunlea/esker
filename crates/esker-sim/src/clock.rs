//! Logical time.
//!
//! The simulator has no wall clock. Time is an integer that only moves when the event loop
//! decides it does, which is what makes a run reproducible and what lets a test compress an
//! hour of Raft elections into a millisecond of real time.

use std::fmt;

/// A point in logical time, in milliseconds since the start of a run.
///
/// Milliseconds are the unit the design already speaks in: a Raft tick is 100 ms, a store
/// heartbeat 10 s, a lock TTL 3 s (`docs/DESIGN.md` §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Millis(pub u64);

impl Millis {
    /// The start of a run.
    pub const ZERO: Millis = Millis(0);

    /// This instant plus `delta`, saturating instead of wrapping. A simulation that ran long
    /// enough to overflow would produce nonsense either way, but saturating keeps time
    /// monotonic, which is the property the event loop relies on.
    #[must_use]
    pub fn saturating_add(self, delta: u64) -> Millis {
        Millis(self.0.saturating_add(delta))
    }
}

impl fmt::Display for Millis {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}ms", self.0)
    }
}

/// A monotonic logical clock.
///
/// Only the event loop advances it, and only forwards: an event scheduled in the past is
/// delivered at the current instant rather than moving time backwards, because a clock that
/// can go backwards makes every timeout in the system unreasonable.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    now: Millis,
}

impl Clock {
    /// A clock at [`Millis::ZERO`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current instant.
    #[must_use]
    pub fn now(&self) -> Millis {
        self.now
    }

    /// Moves time to `instant`, or leaves it alone if that is in the past.
    pub fn advance_to(&mut self, instant: Millis) {
        self.now = self.now.max(instant);
    }

    /// Moves time forward by `delta` milliseconds.
    pub fn advance_by(&mut self, delta: u64) {
        self.now = self.now.saturating_add(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, Millis};

    #[test]
    fn time_never_goes_backwards() {
        let mut clock = Clock::new();
        assert_eq!(clock.now(), Millis::ZERO);

        clock.advance_to(Millis(100));
        assert_eq!(clock.now(), Millis(100));

        clock.advance_to(Millis(50));
        assert_eq!(
            clock.now(),
            Millis(100),
            "an event from the past moved the clock back"
        );

        clock.advance_by(25);
        assert_eq!(clock.now(), Millis(125));
    }

    #[test]
    fn time_saturates_instead_of_wrapping() {
        let mut clock = Clock::new();
        clock.advance_to(Millis(u64::MAX));
        clock.advance_by(1_000);
        assert_eq!(clock.now(), Millis(u64::MAX));
    }
}
