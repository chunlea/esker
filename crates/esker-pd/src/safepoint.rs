//! What the cluster's readers are holding, and the garbage-collection safepoint that follows.
//!
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md) decided the
//! number: **`min(now − retention window, the oldest active read)`**. The active-read floor is the
//! load-bearing half — it is what the safepoint *means* — and the window is the safety net that
//! stops an abandoned reporter pinning history for ever. The window-only version was refused
//! because it produces a number that looks right, is published, is honoured, and is wrong exactly
//! when a transaction is long.
//!
//! # One oracle, one scale
//!
//! Every number here is a **TSO timestamp** — `physical_ms << 18 | logical` — including the
//! deadlines the TTL is measured against. That is not tidiness: the ADR's one arithmetic rule is
//! that a safepoint may only be computed from values a single oracle issued, because mixing a wall
//! clock into a counted sequence silently deletes data. So the window and the TTL are both
//! expressed as milliseconds and shifted onto the oracle's scale, and a `now` that is not a real
//! millisecond count — `CountingOracle`'s, whose physical half is zero — makes both of them
//! underflow to zero, which collects **nothing**. That is the safe direction and it is asserted.
//!
//! # Silence
//!
//! A reporter that stops reporting keeps its last value until its TTL passes, and then stops
//! pinning anything. Both halves matter: releasing instantly would collect history a live-but-quiet
//! reader still needs, and never releasing would let one crashed client keep every version for ever.

use std::collections::BTreeMap;

use crate::TSO_LOGICAL_BITS;

/// One reporter's last word.
#[derive(Debug, Clone, Copy)]
struct Reporter {
    /// The oldest `start_ts` it was holding, or `None` for "nothing open".
    oldest: Option<u64>,
    /// The `now` at which it said so.
    heard: u64,
}

/// The safepoint PD publishes, and the reports it is computed from.
#[derive(Debug, Clone)]
pub struct Safepoints {
    /// How far behind `now` the window alone would allow, in milliseconds.
    retention_ms: u64,
    /// How long a silent reporter keeps pinning, in milliseconds.
    ttl_ms: u64,
    reporters: BTreeMap<u64, Reporter>,
    /// Never moves backwards, which is the rule every store already enforces for itself.
    published: u64,
}

/// Milliseconds as a timestamp delta on the oracle's scale.
fn as_ts(ms: u64) -> u64 {
    ms.checked_shl(TSO_LOGICAL_BITS).unwrap_or(u64::MAX)
}

impl Safepoints {
    /// A registry with a retention window and a reporter TTL, both in milliseconds.
    ///
    /// The TTL is two report intervals by default at the call site, so one lost message never
    /// moves the safepoint — a reporter has to be gone, not merely unlucky.
    #[must_use]
    pub fn new(retention_ms: u64, ttl_ms: u64) -> Self {
        Self {
            retention_ms,
            ttl_ms,
            reporters: BTreeMap::new(),
            published: 0,
        }
    }

    /// Records what one reporter is holding, as of `now`.
    ///
    /// `oldest` is `None` when it has nothing open — which is a report like any other, and the
    /// thing that lets a client that has finished stop holding the floor down without waiting for
    /// its TTL.
    pub fn report(&mut self, reporter: u64, oldest: Option<u64>, now: u64) {
        self.reporters
            .insert(reporter, Reporter { oldest, heard: now });
    }

    /// Forgets a reporter outright — a clean disconnection, rather than silence.
    pub fn forget(&mut self, reporter: u64) {
        self.reporters.remove(&reporter);
    }

    /// The safepoint as of `now`: the minimum of the window and every live reporter's oldest read.
    ///
    /// Reporters past their TTL are dropped here rather than on a timer, so the answer depends only
    /// on `now` and what has been reported — which is what makes it testable without a clock.
    pub fn safepoint(&mut self, now: u64) -> u64 {
        let ttl = as_ts(self.ttl_ms);
        self.reporters
            .retain(|_, reporter| now.saturating_sub(reporter.heard) <= ttl);

        // **Saturating, and that is the CountingOracle case.** A `now` whose physical half is zero
        // is smaller than any retention window expressed in milliseconds, so this floors at zero —
        // a safepoint of zero collects nothing, which is the direction that cannot lose data.
        let window = now.saturating_sub(as_ts(self.retention_ms));
        let oldest = self
            .reporters
            .values()
            .filter_map(|reporter| reporter.oldest)
            .min();
        let computed = oldest.map_or(window, |oldest| window.min(oldest));

        self.published = self.published.max(computed);
        self.published
    }

    /// The last published value, without recomputing.
    #[must_use]
    pub fn published(&self) -> u64 {
        self.published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A millisecond on the oracle's scale, which is the only scale here.
    fn ts(ms: u64) -> u64 {
        ms << TSO_LOGICAL_BITS
    }

    const WINDOW_MS: u64 = 600_000;
    const TTL_MS: u64 = 20_000;

    /// **The discriminator.** A transaction older than the retention window is still open, and the
    /// safepoint may not step over it.
    ///
    /// This is the one test the window-only design fails and every other test here passes, which
    /// is what makes it worth writing first: `now − window` alone answers a number well above the
    /// read's `start_ts`, and a store that honoured it would collect the versions that read is
    /// entitled to. ADR 0110 refuses that design on the strength of exactly this case.
    #[test]
    fn a_read_older_than_the_window_is_not_stepped_over() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        let began = ts(1_000);
        // Reported now, and still open: an hour of `now` moving does not change that.
        safepoints.report(1, Some(began), ts(3_600_000));

        let safepoint = safepoints.safepoint(ts(3_600_000));

        assert!(
            safepoint <= began,
            "the safepoint stepped over a read that is still open: {safepoint} > {began}"
        );
    }

    /// And with nothing open, the window is what is left — otherwise the floor would be zero for
    /// ever and nothing would ever be collected.
    #[test]
    fn with_no_reader_the_window_is_the_answer() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        safepoints.report(1, None, ts(3_600_000));

        assert_eq!(
            safepoints.safepoint(ts(3_600_000)),
            ts(3_600_000 - WINDOW_MS),
            "a cluster with no open read should collect up to the window"
        );
    }

    /// **A reporter that goes quiet keeps pinning until its TTL, and not one moment longer.**
    ///
    /// Two assertions and not one, because each side is a different bug: releasing early collects
    /// history a live-but-quiet reader still needs, and never releasing lets one crashed client
    /// keep every version for ever.
    #[test]
    fn a_silent_reporter_stops_pinning_after_its_ttl_and_not_before() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        let began = ts(1_000);
        let heard = ts(3_600_000);
        safepoints.report(1, Some(began), heard);

        // Quiet for less than the TTL: still holding.
        let inside = safepoints.safepoint(heard + ts(TTL_MS - 1));
        assert!(
            inside <= began,
            "a reporter quiet for less than its TTL stopped pinning: {inside} > {began}"
        );

        // Past it: gone, and the window is the answer again.
        let outside = heard + ts(TTL_MS + 1);
        assert_eq!(
            safepoints.safepoint(outside),
            outside - ts(WINDOW_MS),
            "a reporter past its TTL is still pinning the safepoint"
        );
    }

    /// **A counting oracle collects nothing**, rather than everything.
    ///
    /// `CountingOracle`'s timestamps have a zero physical half, so a window in milliseconds is
    /// larger than any `now` it will ever issue. The subtraction saturates at zero and zero
    /// collects nothing — the direction that cannot lose data. Asserted because the other
    /// direction is silent: an unchecked subtraction would wrap to a safepoint above every
    /// timestamp in the cluster and collect all of it.
    #[test]
    fn a_counting_oracle_collects_nothing() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        // What a counting oracle hands out: small integers, no physical half at all.
        safepoints.report(1, None, 42);

        assert_eq!(
            safepoints.safepoint(42),
            0,
            "a counting oracle's timestamps are not milliseconds, and the window must not pretend"
        );
    }

    /// The safepoint never moves backwards, which every store already enforces for itself.
    #[test]
    fn the_safepoint_never_moves_backwards() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        safepoints.report(1, None, ts(3_600_000));
        let high = safepoints.safepoint(ts(3_600_000));

        // A long reader arrives *after* history above it is already collectable.
        safepoints.report(2, Some(ts(1_000)), ts(3_600_001));

        assert_eq!(
            safepoints.safepoint(ts(3_600_001)),
            high,
            "the published safepoint went backwards, which promises history that may be gone"
        );
    }
}
