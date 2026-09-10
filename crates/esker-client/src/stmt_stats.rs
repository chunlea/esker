//! **What one statement costs in round trips and regions**, counted at the only place that knows.
//!
//! `docs/plans/debts-v1.1.md` #49 opened on three numbers nobody could produce: how many KV reads
//! one statement makes, how many **RPC round trips** those become, and **how many regions** they
//! land on. The first is `esker-sql`'s to count — it is the caller. The other two are this crate's,
//! and they are one hook: [`crate::router::Router::call`] resolves a region and then makes exactly
//! one wire call per attempt, so the region and the round trip are known in the same three lines.
//!
//! **A retry is a round trip.** The count is per *attempt*, not per call: a request that is
//! refused for a stale epoch and sent again cost the cluster two, and a number that hid the second
//! would be measuring the API rather than the wire.
//!
//! **Off unless `ESKER_STMT_STATS` is set**, an environment variable rather than a feature for the
//! reason `esker_sql::catalog::stats` gives: the run that produces these numbers is a suite against
//! a released binary, and a feature would mean a special build nobody has. Reading the variable
//! happens once; when it is off, this costs one relaxed load and nothing else.
//!
//! **Per thread, because a statement is a thread.** `esker-sql` hands each statement to one
//! blocking thread and the client's calls are synchronous, so what this thread did since the last
//! reset is what this statement did. A global would mix two sessions' work into one statement's
//! number.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Whether the instrument is on, read once from the environment.
#[must_use]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ESKER_STMT_STATS").is_some())
}

thread_local! {
    /// Wire calls this thread has made since the last reset — **one per attempt**.
    static ROUND_TRIPS: Cell<u64> = const { Cell::new(0) };
    /// Which regions those calls addressed. A set, because the number #49 wants is *how many
    /// distinct* regions a statement touches: a scan that walks four regions is a different shape
    /// from a point read that retries four times on one.
    static REGIONS: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
}

/// Records one wire call to `region_id`.
///
/// Called with the region the attempt was addressed to, which is zero only when the resolver could
/// not name one — and then no wire call happens, so this is not reached with it.
pub fn record_call(region_id: u64) {
    if !enabled() {
        return;
    }
    ROUND_TRIPS.with(|trips| trips.set(trips.get().saturating_add(1)));
    REGIONS.with_borrow_mut(|regions| {
        regions.insert(region_id);
    });
}

/// What this thread has done since the last [`reset`]: `(round trips, distinct regions)`.
#[must_use]
pub fn taken() -> (u64, usize) {
    (
        ROUND_TRIPS.with(Cell::get),
        REGIONS.with_borrow(BTreeSet::len),
    )
}

/// Starts a new statement's accounting on this thread.
pub fn reset() {
    ROUND_TRIPS.with(|trips| trips.set(0));
    REGIONS.with_borrow_mut(BTreeSet::clear);
}

#[cfg(test)]
mod tests {
    use super::{enabled, record_call, reset, taken};

    /// **Off by default, and off records nothing.** The instrument sits on the path of every wire
    /// call, so a build nobody switched on must not pay for it or count for it.
    #[test]
    fn it_is_off_unless_the_environment_says_otherwise() {
        if std::env::var_os("ESKER_STMT_STATS").is_some() {
            assert!(enabled());
            return;
        }
        assert!(!enabled());
        reset();
        record_call(7);
        assert_eq!(taken(), (0, 0));
    }
}
