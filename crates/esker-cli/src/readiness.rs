//! What a readiness wait is allowed to spend, and on what.
//!
//! # A budget for the whole start is a rate written as a property
//!
//! `cluster start` waits for four stores to answer and used to give them sixty seconds — one
//! clock for the set. Sixty is not a claim about this system: it is a claim about how fast the
//! machine underneath starts four processes, and on a machine running three thousand other tests
//! that claim is false. `.config/nextest.toml` already says so about this crate's tests in
//! particular — "their deadlines are wall-clock budgets rather than assertions about a state" —
//! and `esker-cli::cluster_start` reds under a full gate for exactly that reason, having watched
//! three of its four stores come up.
//!
//! Raising the number is the same claim moved, so the budget is spent **per thing waited on** and
//! starts again each time one of them arrives. A start that is still bringing stores up has not
//! hung, however slowly it is doing it; a start where nothing has arrived for a whole budget has,
//! and that is the verdict worth having. What stays fixed is a ceiling — `each` × the number
//! waited on — so that a set which flaps for ever still ends, and ends saying which limit it hit.
//!
//! This is deliberately a small pure thing with its own tests rather than arithmetic inside the
//! polling loop: the loop cannot be tested without a wall clock, and this can.

use std::time::{Duration, Instant};

/// The budget a readiness wait spends, per thing waited on.
pub(crate) struct Budget {
    /// What one arrival buys. Reaching it with nothing new is the usual failure.
    each: Duration,
    /// The whole wait's limit, however much progress is made inside it.
    ceiling: Duration,
    started: Instant,
    /// When the last thing arrived — the start, until one does.
    progressed: Instant,
}

/// Which limit a spent budget hit. The two are different diagnoses and the message says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spent {
    /// Nothing has arrived for this long. The start is stuck on whatever is still missing.
    Stalled(Duration),
    /// Things kept arriving and the wait still ran past its ceiling: a set that flaps.
    Ceiling(Duration),
}

impl std::fmt::Display for Spent {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stalled(within) => write!(out, "nothing has answered for {within:?}"),
            Self::Ceiling(ceiling) => {
                write!(out, "the whole wait ran past its {ceiling:?} ceiling")
            }
        }
    }
}

impl Budget {
    /// A budget of `each` per thing, for `waited_on` of them.
    ///
    /// `waited_on` of zero is treated as one: a caller with nothing to wait for never asks, and a
    /// ceiling of zero would turn that mistake into an instant failure rather than a trivial
    /// success.
    pub(crate) fn new(now: Instant, each: Duration, waited_on: usize) -> Self {
        let count = u32::try_from(waited_on).unwrap_or(u32::MAX).max(1);
        Self {
            each,
            ceiling: each.saturating_mul(count),
            started: now,
            progressed: now,
        }
    }

    /// One of the things arrived, so the per-thing budget starts again.
    pub(crate) fn progress(&mut self, now: Instant) {
        self.progressed = now;
    }

    /// Why the wait must give up, or `None` while it may keep going.
    ///
    /// The stall is checked first: when both limits are reached at once — a wait that made no
    /// progress at all and whose ceiling is one `each`, which is every single-thing wait — the
    /// useful thing to say is that nothing answered, not that a ceiling was hit.
    pub(crate) fn spent(&self, now: Instant) -> Option<Spent> {
        if now.saturating_duration_since(self.progressed) >= self.each {
            return Some(Spent::Stalled(self.each));
        }
        if now.saturating_duration_since(self.started) >= self.ceiling {
            return Some(Spent::Ceiling(self.ceiling));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten seconds after `at`, for tests that read as a timeline.
    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    /// The whole point: an arrival buys another `each`, so a set that keeps arriving keeps going
    /// past the budget one clock for the set would have ended it at.
    ///
    /// This is the sixty-second start with four stores, spread the way a loaded machine spreads
    /// them: 50 s, 100 s, 150 s. One clock gives up at 60 with three still to come.
    #[test]
    fn a_thing_that_arrives_buys_the_next_one_another_budget() {
        let start = Instant::now();
        let mut budget = Budget::new(start, Duration::from_secs(60), 4);

        assert_eq!(budget.spent(at(start, 59)), None);
        budget.progress(at(start, 50));
        assert_eq!(budget.spent(at(start, 100)), None, "50 s after the first");
        budget.progress(at(start, 100));
        assert_eq!(budget.spent(at(start, 150)), None, "50 s after the second");
        budget.progress(at(start, 150));
        assert_eq!(budget.spent(at(start, 200)), None, "50 s after the third");
    }

    /// And a start where nothing arrives is refused at one `each`, not at the ceiling: the stall
    /// is the diagnosis, and waiting four times as long to reach the same one helps nobody.
    #[test]
    fn a_wait_that_makes_no_progress_gives_up_at_one_budget() {
        let start = Instant::now();
        let budget = Budget::new(start, Duration::from_secs(60), 4);

        assert_eq!(budget.spent(at(start, 59)), None);
        assert_eq!(
            budget.spent(at(start, 60)),
            Some(Spent::Stalled(Duration::from_secs(60)))
        );
    }

    /// The ceiling is what a set that flaps runs into: something arrives often enough that the
    /// stall never fires, and the wait still ends.
    #[test]
    fn a_set_that_keeps_arriving_still_ends_at_the_ceiling() {
        let start = Instant::now();
        let mut budget = Budget::new(start, Duration::from_secs(60), 4);

        // Progress every 30 s, so the stall is never reached. The ceiling is 4 x 60 = 240 s.
        for secs in (30..240).step_by(30) {
            budget.progress(at(start, secs));
            assert_eq!(budget.spent(at(start, secs)), None, "at {secs} s");
        }
        budget.progress(at(start, 239));
        assert_eq!(
            budget.spent(at(start, 240)),
            Some(Spent::Ceiling(Duration::from_secs(240))),
            "a set that keeps arriving must still end"
        );
    }

    /// One thing waited on is the old behaviour exactly: `each` is the whole budget, because the
    /// ceiling is `each` x 1. Every existing caller with a single store depends on this.
    #[test]
    fn one_thing_waited_on_is_the_budget_it_was_given() {
        let start = Instant::now();
        let budget = Budget::new(start, Duration::from_millis(300), 1);

        assert_eq!(budget.spent(start + Duration::from_millis(299)), None);
        assert_eq!(
            budget.spent(start + Duration::from_millis(300)),
            Some(Spent::Stalled(Duration::from_millis(300)))
        );
    }

    /// Nothing to wait for is a trivial success and not an instant failure, which a ceiling of
    /// `each` x 0 would have made it.
    #[test]
    fn nothing_waited_on_still_has_a_budget() {
        let start = Instant::now();
        let budget = Budget::new(start, Duration::from_secs(60), 0);

        assert_eq!(budget.spent(at(start, 59)), None);
    }
}
