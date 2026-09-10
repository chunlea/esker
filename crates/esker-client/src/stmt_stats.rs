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
use std::time::Duration;

use crate::wire::Method;

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

    // -- the write side ------------------------------------------------------------------
    //
    // #49's numbers are a *read* statement's. A DDL statement is priced by different things and
    // the round-trip total hides all of them: a `CREATE TABLE` and a `SELECT` that both make six
    // calls are not the same six. What a write pays for is timestamps, the two phases of its
    // commit, the keys it writes, and whatever it spent waiting for somebody else.

    /// Timestamps taken from the oracle. Each is a round trip to the placement driver on a real
    /// cluster, and it is **not** counted in `ROUND_TRIPS`, which counts store calls.
    static TSO: Cell<u64> = const { Cell::new(0) };
    /// `Prewrite` calls — phase one, once per region a transaction's keys fall in, plus one per
    /// range check and one per eager lock.
    static PREWRITES: Cell<u64> = const { Cell::new(0) };
    /// `Commit` calls — phase two, the primary alone and then the secondaries by region.
    static COMMITS: Cell<u64> = const { Cell::new(0) };
    /// Mutations sent in those prewrites: the keys this statement actually wrote or checked.
    static KEYS: Cell<u64> = const { Cell::new(0) };
    /// Time spent asleep waiting for somebody else's lock, in microseconds. Separated because it
    /// is the one part of a statement's cost that is not this statement's own work.
    static WAITED: Cell<u64> = const { Cell::new(0) };
}

/// What one statement cost the cluster, as this crate sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cost {
    /// Wire calls to a store, one per attempt.
    pub round_trips: u64,
    /// Distinct regions those calls addressed.
    pub regions: usize,
    /// Timestamps taken from the oracle.
    pub tso: u64,
    /// `Prewrite` calls.
    pub prewrites: u64,
    /// `Commit` calls.
    pub commits: u64,
    /// Mutations sent in prewrites.
    pub keys: u64,
    /// Time asleep behind somebody else's lock.
    pub waited: Duration,
}

/// How many mutations a body carries, which is zero for everything but a `Prewrite`.
///
/// Here rather than in the router, so that the router goes on knowing only that it is sending a
/// body: what a prewrite is made of is this module's business and the wire's, not routing's.
#[must_use]
pub fn mutations_in(body: &crate::wire::Body) -> usize {
    match body {
        crate::wire::Body::Txn(crate::wire::TxnKvReq::Prewrite { mutations, .. }) => {
            mutations.len()
        }
        _ => 0,
    }
}

/// Records a timestamp taken from the oracle.
pub fn record_tso() {
    if !enabled() {
        return;
    }
    TSO.with(|tso| tso.set(tso.get().saturating_add(1)));
}

/// Records time this statement spent waiting for a lock it did not hold.
pub fn record_wait(waited: Duration) {
    if !enabled() {
        return;
    }
    let micros = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
    WAITED.with(|w| w.set(w.get().saturating_add(micros)));
}

/// Records one wire call to `region_id`.
///
/// Called with the region the attempt was addressed to, which is zero only when the resolver could
/// not name one — and then no wire call happens, so this is not reached with it.
pub fn record_call(region_id: u64, method: Method, keys: usize) {
    if !enabled() {
        return;
    }
    ROUND_TRIPS.with(|trips| trips.set(trips.get().saturating_add(1)));
    REGIONS.with_borrow_mut(|regions| {
        regions.insert(region_id);
    });
    // **By phase, because that is what a write is priced by.** `keys` is meaningful only for a
    // prewrite — it is the mutation count — and is zero everywhere else rather than a guess.
    match method {
        Method::TxnPrewrite => {
            PREWRITES.with(|n| n.set(n.get().saturating_add(1)));
            KEYS.with(|n| n.set(n.get().saturating_add(keys as u64)));
        }
        Method::TxnCommit => COMMITS.with(|n| n.set(n.get().saturating_add(1))),
        _ => {}
    }
}

/// What this thread has done since the last [`reset`].
#[must_use]
pub fn taken() -> Cost {
    Cost {
        round_trips: ROUND_TRIPS.with(Cell::get),
        regions: REGIONS.with_borrow(BTreeSet::len),
        tso: TSO.with(Cell::get),
        prewrites: PREWRITES.with(Cell::get),
        commits: COMMITS.with(Cell::get),
        keys: KEYS.with(Cell::get),
        waited: Duration::from_micros(WAITED.with(Cell::get)),
    }
}

/// Starts a new statement's accounting on this thread.
pub fn reset() {
    ROUND_TRIPS.with(|trips| trips.set(0));
    REGIONS.with_borrow_mut(BTreeSet::clear);
    TSO.with(|n| n.set(0));
    PREWRITES.with(|n| n.set(0));
    COMMITS.with(|n| n.set(0));
    KEYS.with(|n| n.set(0));
    WAITED.with(|n| n.set(0));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Cost, Method, enabled, record_call, record_tso, record_wait, reset, taken};

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
        record_call(7, Method::TxnPrewrite, 3);
        record_tso();
        record_wait(Duration::from_millis(5));
        assert_eq!(
            taken(),
            Cost::default(),
            "every counter, not only the two that existed first — a write-side counter that \
             recorded while the instrument was off would be paid for by every build"
        );
    }
}
