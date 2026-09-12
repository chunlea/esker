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

        // **Nobody reporting is not the same as nobody reading.** A reporter that says `None` has
        // told this registry something — it has nothing open — and the window may apply. An empty
        // registry has told it nothing at all: PD has just restarted, or no client has reached it
        // yet, and a long read could be open behind any of that silence. So the safepoint does not
        // advance, and the store keeps what it has (ADR 0110: every unknown resolves to "collect
        // less").
        if self.reporters.is_empty() {
            return self.published;
        }

        // **Saturating, and that is the CountingOracle case.** A `now` whose physical half is zero
        // is smaller than any retention window expressed in milliseconds, so this floors at zero —
        // a safepoint of zero collects nothing, which is the direction that cannot lose data.
        let window = now.saturating_sub(as_ts(self.retention_ms));
        // **A reporter that said "nothing open" pins its own last word**, not nothing at all. It
        // spoke about the instant it spoke; a transaction may have begun immediately after, and it
        // will not reach PD until that reporter's next round. Treating the silence between rounds
        // as "no constraint" is what let the window walk past a live read — measured on the gate
        // of 2026-09-11 at 598 ms past a fifteen-second-old snapshot, with a one-second window and
        // a report riding the schema lease's refresh period, which is seconds.
        //
        // The cost is paid only where the reports are slower than the window: there the safepoint
        // lags the last round instead of the window, which collects less history and never the
        // wrong history. Where reports are faster — every deployment that has not turned the
        // window down — `heard` is newer than the window and this changes nothing.
        let oldest = self
            .reporters
            .values()
            .map(|reporter| reporter.oldest.unwrap_or(reporter.heard))
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
    ///
    /// **Two reporters, so the TTL is what is being measured.** With one, its timing out empties
    /// the registry and `an_empty_registry_does_not_advance_the_safepoint` takes over — a
    /// different rule, and the test would be passing on it rather than on this one.
    #[test]
    fn a_silent_reporter_stops_pinning_after_its_ttl_and_not_before() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);
        let began = ts(1_000);
        let heard = ts(3_600_000);
        // One holding a long read, and one that keeps reporting with nothing open.
        safepoints.report(1, Some(began), heard);
        safepoints.report(2, None, heard);

        // Quiet for less than the TTL: still holding.
        let inside = heard + ts(TTL_MS - 1);
        safepoints.report(2, None, inside);
        let held = safepoints.safepoint(inside);
        assert!(
            held <= began,
            "a reporter quiet for less than its TTL stopped pinning: {held} > {began}"
        );

        // Past it: gone, and the reporter still talking decides.
        let outside = heard + ts(TTL_MS + 1);
        safepoints.report(2, None, outside);
        assert_eq!(
            safepoints.safepoint(outside),
            outside - ts(WINDOW_MS),
            "a reporter past its TTL is still pinning the safepoint"
        );
    }

    /// **Silence is not "nothing open".** With no reporter at all — a PD that has just restarted,
    /// or a cluster whose clients have not reached it yet — the safepoint does not move, however
    /// long the window says it could.
    ///
    /// The pair to `with_no_reader_the_window_is_the_answer` above, and the distinction is the
    /// whole of it: that one has a reporter *saying* it holds nothing, which is information. This
    /// one has no information, and the direction of an unknown is "collect less".
    #[test]
    fn an_empty_registry_does_not_advance_the_safepoint() {
        let mut safepoints = Safepoints::new(WINDOW_MS, TTL_MS);

        assert_eq!(
            safepoints.safepoint(ts(3_600_000)),
            0,
            "PD published a safepoint without one reader having said anything"
        );

        // And a reporter that times out puts it back into that state rather than releasing.
        safepoints.report(1, Some(ts(1_000)), ts(3_600_000));
        let held = safepoints.safepoint(ts(3_600_000));
        let long_after = ts(3_600_000) + ts(TTL_MS * 10);
        assert_eq!(
            safepoints.safepoint(long_after),
            held,
            "the last reporter timing out advanced the safepoint on nobody's word"
        );
    }

    /// **A reporter's silence is only a statement about the instant it spoke.**
    ///
    /// A reporter that said "nothing open" at `R` has told PD nothing about `R + 1`: a transaction
    /// may have begun the moment after, and it will not be reported until the reporter's next
    /// round. So the window must not walk past `R` on that reporter's word — the same rule
    /// `an_empty_registry_does_not_advance_the_safepoint` applies to a registry with nothing in
    /// it, applied to the gap between one reporter's rounds.
    ///
    /// **This is not hypothetical and the window being short is not the cause.** A node reports on
    /// its schema lease's refresh period, which is seconds; `esker-cli`'s
    /// `safepoint_spares_a_long_read` runs with a one-second window, and on the gate of 2026-09-11
    /// it refused a fifteen-second-old read whose snapshot was **598 ms** below the published
    /// safepoint. A retention window shorter than a report interval is a configuration an operator
    /// may reasonably choose, and it must cost history rather than correctness.
    #[test]
    fn the_window_does_not_walk_past_a_reporters_last_word() {
        // **Its own window**, and a short one: this is about a window shorter than the interval
        // between reports, which is exactly the configuration `esker-cli`'s end-to-end test runs
        // with. The module's `WINDOW_MS` is ten minutes, against which every timestamp here would
        // underflow to zero and the test would pass by measuring nothing.
        let mut safepoints = Safepoints::new(1_000, TTL_MS);

        // It has nothing open, and says so — at 10 s, and then not again.
        safepoints.report(1, None, ts(10_000));

        // Two seconds later, with a one-second window. A read that began at 10.5 s is held by this
        // reporter and has not reached PD yet, so a safepoint above 10 s would collect under it.
        let published = safepoints.safepoint(ts(12_000));
        assert!(
            published <= ts(10_000),
            "the safepoint reached {published} on the word of a reporter that last spoke at \
             {}: anything it has opened since is invisible, and the window is not allowed to \
             assume otherwise",
            ts(10_000)
        );

        // And it still moves when the reporter keeps speaking, which is the half that makes this a
        // constraint rather than a freeze.
        safepoints.report(1, None, ts(12_000));
        assert_eq!(
            safepoints.safepoint(ts(12_000)),
            ts(11_000),
            "a reporter speaking at the same instant leaves the window in charge"
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
