//! How often a fragment met an unresolved lock, and whether that lock had outlived its lease.
//!
//! [ADR 0118](../../../docs/adr/0118-a-counter-is-placed-at-a-door-and-counts-events.md) decides
//! the shape: a counter goes **at a door** — a place a request crosses a boundary — and counts
//! **events**, never time. This one's door is the fragment service's refusal point, which is the
//! only place that holds both halves of an exact verdict: the read's own `ts` and the lock's own
//! `ttl_ms`.
//!
//! # What the two numbers mean, and what neither of them means
//!
//! `expired` is `esker_txn::is_expired(start_ts, ttl_ms, read_ts)` — the lock has outlived the
//! lease **it** asked for, not a default one. `alive` is the rest.
//!
//! **`expired` is a floor under "transactions that have already finished", not the count of them.**
//! A transaction whose primary has committed while this secondary's lock is still young is
//! finished, and is counted `alive`. Recognising it means reading the primary's `write` record,
//! which is another read per encounter and is not done here. So a near-zero `expired` rules out the
//! expired mechanism and says nothing about the young-but-finished one.
//!
//! # One encounter is one refusal
//!
//! `columnar::region::unresolved_lock` returns the **first** lock it finds and stops, so these
//! count refusals rather than locks. A scan that would have met four locks counts one.
//!
//! # The day comes from the read, not from a clock
//!
//! Buckets are the TSO day of the read's own timestamp: `physical_ms(ts) / 86_400_000`. The number
//! being classified carries its own physical time, so no second clock is introduced — which also
//! keeps this working where a node's `now()` is a frozen constant.
//!
//! **A restart loses the running day's partial counts**, which is the price of a counter that costs
//! nothing to keep, and is stated rather than hidden.
//!
//! **The day's totals are kept in memory as well as logged, and that is not belt-and-braces.** A
//! `tracing` line exists only if the deployment's filter lets it through, so a counter whose only
//! record of yesterday is an INFO line has no record of yesterday on a node configured for WARN.
//! (Seen the hard way on 2026-09-17: a `RUST_LOG` set for a gate was inherited by a child process
//! and filtered out the line a test was waiting for.) So the previous day is readable through
//! [`lock_encounters`] too, and the log line is a convenience rather than the storage. **In an in-process fixture every store shares
//! these statics**; production runs one store per process, so a test must assert on *increments*.

use std::sync::atomic::{AtomicU64, Ordering};

/// Milliseconds in a day, for the TSO-day bucket.
const DAY_MS: u64 = 86_400_000;

static DAY: AtomicU64 = AtomicU64::new(0);
static ALIVE: AtomicU64 = AtomicU64::new(0);
static EXPIRED: AtomicU64 = AtomicU64::new(0);
static LAST_DAY: AtomicU64 = AtomicU64::new(0);
static LAST_ALIVE: AtomicU64 = AtomicU64::new(0);
static LAST_EXPIRED: AtomicU64 = AtomicU64::new(0);

/// What the counters hold right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockEncounters {
    /// The TSO day these counts belong to.
    pub day: u64,
    /// Encounters whose lock was still inside the lease it asked for.
    pub alive: u64,
    /// Encounters whose lock had outlived it.
    pub expired: u64,
    /// The last day that ended, and what it held when it did. Zero before the first rollover.
    pub last_day: u64,
    /// That day's `alive`.
    pub last_alive: u64,
    /// That day's `expired`.
    pub last_expired: u64,
}

/// Records one encounter, judged by the lock's own lease at the timestamp of the read that met it.
pub(crate) fn met(start_ts: u64, ttl_ms: u64, read_ts: u64) {
    let day = esker_txn::physical_ms(read_ts) / DAY_MS;
    let was = DAY.swap(day, Ordering::Relaxed);
    if was != day {
        // The day turned over. Report what the old one held and start the new one empty; a reader
        // that wants the series takes it from these lines.
        let alive = ALIVE.swap(0, Ordering::Relaxed);
        let expired = EXPIRED.swap(0, Ordering::Relaxed);
        LAST_DAY.store(was, Ordering::Relaxed);
        LAST_ALIVE.store(alive, Ordering::Relaxed);
        LAST_EXPIRED.store(expired, Ordering::Relaxed);
        if alive != 0 || expired != 0 {
            tracing::info!(
                day = was,
                alive,
                expired,
                "fragment lock encounters for the day just ended"
            );
        }
    }
    if esker_txn::is_expired(start_ts, ttl_ms, read_ts) {
        EXPIRED.fetch_add(1, Ordering::Relaxed);
    } else {
        ALIVE.fetch_add(1, Ordering::Relaxed);
    }
}

/// The running day's counts, for a test or an in-process reader.
#[must_use]
pub fn lock_encounters() -> LockEncounters {
    LockEncounters {
        day: DAY.load(Ordering::Relaxed),
        alive: ALIVE.load(Ordering::Relaxed),
        expired: EXPIRED.load(Ordering::Relaxed),
        last_day: LAST_DAY.load(Ordering::Relaxed),
        last_alive: LAST_ALIVE.load(Ordering::Relaxed),
        last_expired: LAST_EXPIRED.load(Ordering::Relaxed),
    }
}
