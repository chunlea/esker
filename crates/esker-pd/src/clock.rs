//! The one place in Esker that reads a wall clock.
//!
//! `CLAUDE.md` invariant 6: *timestamps come only from PD's TSO; no node uses its wall clock
//! for ordering.* The oracle is the exception that makes the rule work — something has to turn
//! physical time into the ordering everything else borrows — and this module is the whole of
//! it. Nothing else in the workspace calls [`std::time::SystemTime::now`] for a decision.
//!
//! It is a trait rather than a call because the oracle's hardest property is what it does when
//! the clock **misbehaves**: jumps backwards after an NTP correction, stalls for a second, or
//! comes back from a restart set to last week. A test that had to arrange those with a real
//! clock could not arrange them at all.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Physical time, in milliseconds since the Unix epoch.
pub trait Clock: fmt::Debug + Send + Sync {
    /// Milliseconds since the Unix epoch. Not guaranteed to be monotone — that is the
    /// oracle's job, not the clock's.
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    /// A clock set before 1970 reads as zero rather than panicking: the oracle's restart rule
    /// (`max(clock, mark)`) survives a nonsense clock, and a panic here would take the whole
    /// cluster's timestamp source down for a machine with a dead battery (invariant 9).
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

/// A clock a test sets by hand.
///
/// Behind the `testing` feature, like `esker-engine`'s fault-injecting filesystem: a normal
/// build does not have it. It is the only way to write the tests that matter here — a clock
/// that goes *backwards* across a restart, and one that stands still while a batch is drained.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
pub struct TestClock {
    now_ms: std::sync::atomic::AtomicU64,
}

#[cfg(any(test, feature = "testing"))]
impl TestClock {
    /// A clock reading `now_ms`.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms: std::sync::atomic::AtomicU64::new(now_ms),
        }
    }

    /// Sets the time, forwards or backwards.
    pub fn set(&self, now_ms: u64) {
        self.now_ms
            .store(now_ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// Moves the time forward by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.now_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "testing"))]
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, SystemClock, TestClock};

    /// The system clock has to be somewhere in this century, or every timestamp the cluster
    /// hands out is nonsense that the oracle's restart rule will then preserve for ever.
    #[test]
    fn the_system_clock_reads_a_plausible_millisecond() {
        let now = SystemClock.now_ms();
        assert!(now > 1_700_000_000_000, "{now} is before 2023");
        assert!(now < 4_000_000_000_000, "{now} is after 2096");
    }

    #[test]
    fn a_test_clock_goes_where_it_is_told() {
        let clock = TestClock::new(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        clock.advance(5);
        assert_eq!(clock.now_ms(), 1_005);
        clock.set(7);
        assert_eq!(clock.now_ms(), 7, "a clock must be able to go backwards");
    }
}
