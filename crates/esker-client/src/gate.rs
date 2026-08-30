//! A ceiling on how many calls are in flight at once.
//!
//! "Bounded everything" is the rule the rest of the system already follows — bounded frames,
//! bounded retries, bounded write buffers. An unbounded client is the hole in that: sixty-four
//! benchmark threads with no ceiling turn a momentary stall into a queue that grows until the
//! process runs out of memory, and the store sees a load spike it can only answer with
//! `ServerIsBusy`. So a call takes a permit first and gives it back when it finishes,
//! retries included.
//!
//! Waiting is bounded too: a caller that cannot get a permit before its deadline gets a
//! deadline error rather than blocking forever behind a stalled cluster.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// A counting semaphore.
#[derive(Debug)]
pub struct Gate {
    limit: usize,
    held: Mutex<usize>,
    released: Condvar,
}

impl Gate {
    /// A gate that admits `limit` callers at once. A limit of zero admits one, because a gate
    /// that admits nobody is a deadlock rather than a bound.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            held: Mutex::new(0),
            released: Condvar::new(),
        }
    }

    /// Takes a permit, waiting up to `timeout` for one.
    ///
    /// Returns `None` if the wait ran out, which the caller reports as a deadline rather than
    /// as a server error: nothing was sent.
    pub fn acquire(&self, timeout: Duration) -> Option<Permit<'_>> {
        let held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `wait_timeout_while` re-checks the predicate after a spurious wake and subtracts the
        // time already waited, so the total wait really is bounded by `timeout`.
        let (mut held, timing) = self
            .released
            .wait_timeout_while(held, timeout, |held| *held >= self.limit)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if timing.timed_out() {
            return None;
        }
        *held += 1;
        Some(Permit { gate: self })
    }

    /// How many permits are out.
    #[must_use]
    pub fn held(&self) -> usize {
        *self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ceiling.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// One admitted caller. Dropping it lets the next one in.
#[derive(Debug)]
pub struct Permit<'a> {
    gate: &'a Gate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut held = self
            .gate
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = held.saturating_sub(1);
        drop(held);
        self.gate.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::Gate;

    #[test]
    fn permits_are_returned_when_they_are_dropped() {
        let gate = Gate::new(2);
        let first = gate.acquire(Duration::from_secs(1)).expect("room");
        let second = gate.acquire(Duration::from_secs(1)).expect("room");
        assert_eq!(gate.held(), 2);

        assert!(
            gate.acquire(Duration::from_millis(10)).is_none(),
            "a full gate must not admit a third"
        );

        drop(first);
        assert_eq!(gate.held(), 1);
        let third = gate.acquire(Duration::from_millis(50)).expect("room again");
        drop((second, third));
        assert_eq!(gate.held(), 0);
    }

    /// The bound has to be real under contention, not just in a single-threaded test.
    #[test]
    fn the_ceiling_holds_across_threads() {
        let gate = Arc::new(Gate::new(4));
        let peak = Arc::new(std::sync::Mutex::new(0usize));

        let workers: Vec<_> = (0..16)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let permit = gate.acquire(Duration::from_secs(5)).expect("room");
                        let held = gate.held();
                        let mut peak = peak.lock().unwrap();
                        *peak = (*peak).max(held);
                        drop(peak);
                        drop(permit);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("no worker panicked");
        }
        assert!(*peak.lock().unwrap() <= 4, "the gate let too many through");
        assert_eq!(gate.held(), 0);
    }

    #[test]
    fn a_waiting_caller_gives_up_at_its_timeout() {
        let gate = Gate::new(1);
        let _held = gate.acquire(Duration::from_secs(1)).expect("room");
        let started = Instant::now();
        assert!(gate.acquire(Duration::from_millis(30)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(25));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited too long"
        );
    }

    /// A gate that admits nobody is a deadlock dressed up as a bound.
    #[test]
    fn a_zero_limit_still_admits_one() {
        let gate = Gate::new(0);
        assert_eq!(gate.limit(), 1);
        assert!(gate.acquire(Duration::from_millis(10)).is_some());
    }
}
