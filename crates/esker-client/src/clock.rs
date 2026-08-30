//! Time, injected.
//!
//! A retry loop is mostly a claim about time: *this backs off ten milliseconds, then twenty,
//! and gives up after the deadline*. Testing that against the real clock means either a slow
//! test or a weak assertion, and usually both — so the client never calls `Instant::now` or
//! `thread::sleep` directly. It asks a [`Clock`], and its tests hand it a [`FakeClock`] that
//! moves time forward without any passing.
//!
//! This is the same reasoning that keeps `esker-raft` a pure state machine (`CLAUDE.md`
//! invariant 4): time enters through one seam, so a test can drive it.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The passage of time, as the retry loop sees it.
pub trait Clock: fmt::Debug + Send + Sync {
    /// The current instant on a monotonic clock.
    fn now(&self) -> Instant;

    /// Waits for `duration`.
    fn sleep(&self, duration: Duration);
}

/// The real clock: `Instant::now` and `thread::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug)]
struct FakeState {
    /// A real instant captured once, so `now()` can return a genuine monotonic `Instant`
    /// without any real time having passed.
    base: Instant,
    elapsed: Duration,
    slept: Vec<Duration>,
}

/// A clock that jumps instead of waiting, and writes down every jump.
///
/// `sleep` records the duration and advances `now` by it, so a test can assert the exact
/// backoff sequence a retry loop produced — not that it was "roughly exponential" — and a run
/// that would have waited four seconds finishes immediately.
#[derive(Debug)]
pub struct FakeClock {
    state: Mutex<FakeState>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClock {
    /// A clock at zero elapsed time, with nothing recorded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeState {
                base: Instant::now(),
                elapsed: Duration::ZERO,
                slept: Vec::new(),
            }),
        }
    }

    /// Every sleep so far, in order. The assertion a retry test is actually making.
    #[must_use]
    pub fn sleeps(&self) -> Vec<Duration> {
        self.lock().slept.clone()
    }

    /// Every sleep so far in whole milliseconds, which is how backoffs are specified.
    #[must_use]
    pub fn sleeps_ms(&self) -> Vec<u64> {
        self.lock()
            .slept
            .iter()
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .collect()
    }

    /// How far the clock has moved since it was made.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.lock().elapsed
    }

    /// Moves time forward **without** recording a sleep: what a slow server costs a caller,
    /// as opposed to what the client chose to wait.
    pub fn advance(&self, duration: Duration) {
        self.lock().elapsed += duration;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        let state = self.lock();
        state.base + state.elapsed
    }

    fn sleep(&self, duration: Duration) {
        let mut state = self.lock();
        state.elapsed += duration;
        state.slept.push(duration);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Clock, FakeClock, SystemClock};

    #[test]
    fn a_fake_sleep_moves_time_without_spending_any() {
        let clock = FakeClock::new();
        let started = Instant::now();
        let before = clock.now();

        clock.sleep(Duration::from_secs(30));
        clock.sleep(Duration::from_secs(12));

        assert_eq!(clock.sleeps_ms(), vec![30_000, 12_000]);
        assert_eq!(clock.elapsed(), Duration::from_secs(42));
        assert_eq!(clock.now() - before, Duration::from_secs(42));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "forty-two seconds of fake sleep took real time"
        );
    }

    /// A stalled server is not a backoff, and a test that confuses the two would pass while
    /// the client waited the wrong amount.
    #[test]
    fn advancing_is_not_sleeping() {
        let clock = FakeClock::new();
        clock.advance(Duration::from_millis(500));
        assert!(clock.sleeps().is_empty());
        assert_eq!(clock.elapsed(), Duration::from_millis(500));
    }

    #[test]
    fn the_system_clock_moves_forwards() {
        let clock = SystemClock;
        let before = clock.now();
        clock.sleep(Duration::from_millis(1));
        assert!(clock.now() >= before + Duration::from_millis(1));
    }
}
