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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::wire::Method;

/// Whether the instrument is on: read once from the environment, or turned on by [`force_on`].
///
/// One relaxed load once it is settled, which is the whole of what "off" costs.
#[must_use]
pub fn enabled() -> bool {
    match ON.load(Ordering::Relaxed) {
        UNKNOWN => {
            let on = if std::env::var_os("ESKER_STMT_STATS").is_some() {
                YES
            } else {
                NO
            };
            ON.store(on, Ordering::Relaxed);
            on == YES
        }
        settled => settled == YES,
    }
}

/// Turns the instrument on for the rest of this process.
///
/// **For a test that has to read its own counters.** A run turns this on with `ESKER_STMT_STATS`;
/// a test cannot, because setting an environment variable is `unsafe` in this edition and racy in
/// fact — another thread may be reading one as it is written. This is the same switch without the
/// race, and it only ever moves one way.
pub fn force_on() {
    ON.store(YES, Ordering::Relaxed);
}

const UNKNOWN: u8 = 0;
const NO: u8 = 1;
const YES: u8 = 2;

/// [`UNKNOWN`] until the first [`enabled`] settles it, or [`force_on`] does.
static ON: AtomicU8 = AtomicU8::new(UNKNOWN);

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

    /// **Where the reads went**, as the opaque head of the key each addressed.
    ///
    /// A count of scans says a `DROP TABLE` makes nine of them and says nothing about what they
    /// are for. The head of a key is what tells one record kind from another, and grouping by it
    /// turns "nine scans" into "nine scans of *these*".
    ///
    /// **Opaque here.** This crate does not know what a key means (`CLAUDE.md` invariant 7): it
    /// records a fixed window of bytes and `esker-sql` names them. `HEAD` is wider than any
    /// namespace's kind marker so that no length here encodes a layout, and a short key is padded
    /// rather than skipped — a key too short to have a kind is a fact worth seeing.
    static READ_HEADS: RefCell<BTreeMap<[u8; HEAD], u64>> = const { RefCell::new(BTreeMap::new()) };
    /// The same for range scans, kept apart because they are the number `DROP` is priced by.
    static SCAN_HEADS: RefCell<BTreeMap<[u8; HEAD], u64>> = const { RefCell::new(BTreeMap::new()) };

    // -- what this statement met when a store was not there ------------------------------
    //
    // **A statement that met a dead store and recovered looks exactly like one that did not.**
    // A failed attempt is a round trip, so `ROUND_TRIPS` counts both and tells them apart from
    // neither. run 124 asks a question the difference is the whole of: with a store killed every
    // sixty seconds under a real workload, was any store ever unreachable, and was every such
    // moment recovered from? A green run without these two numbers cannot separate "nothing was
    // ever unreachable" from "everything unreachable was silently recovered", and those are
    // different claims about the redial `102aef93` added.

    /// Calls that never left this client because the connection to that store was closed, by
    /// store. `NotSent` and nothing else: an error the store answered is not this.
    static NOT_SENT: RefCell<BTreeMap<u64, u64>> = const { RefCell::new(BTreeMap::new()) };
    /// Connections rebuilt to a store that had gone away, by store — the recovery itself, and
    /// the only place a run can see that one happened at all.
    static REDIALS: RefCell<BTreeMap<u64, u64>> = const { RefCell::new(BTreeMap::new()) };
}

/// How many leading bytes of a key are kept to tell record kinds apart. Opaque to this crate.
pub const HEAD: usize = 8;

/// What one statement cost the cluster, as this crate sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// Key heads the point reads addressed, and how many each took.
    pub read_heads: BTreeMap<[u8; HEAD], u64>,
    /// Key heads the range scans addressed.
    pub scan_heads: BTreeMap<[u8; HEAD], u64>,
    /// Calls that never left the client because that store's connection was closed, by store.
    pub not_sent: BTreeMap<u64, u64>,
    /// Connections rebuilt to a store that had gone away, by store.
    pub redials: BTreeMap<u64, u64>,
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
pub fn record_call(region_id: u64, body: &crate::wire::Body) {
    if !enabled() {
        return;
    }
    ROUND_TRIPS.with(|trips| trips.set(trips.get().saturating_add(1)));
    REGIONS.with_borrow_mut(|regions| {
        regions.insert(region_id);
    });
    // **By phase, because that is what a write is priced by.** The mutation count is meaningful
    // only for a prewrite, and is not counted anywhere else rather than guessed at.
    match body.method() {
        Method::TxnPrewrite => {
            PREWRITES.with(|n| n.set(n.get().saturating_add(1)));
            KEYS.with(|n| n.set(n.get().saturating_add(mutations_in(body) as u64)));
        }
        Method::TxnCommit => COMMITS.with(|n| n.set(n.get().saturating_add(1))),
        // **Where a read went**, by the head of the key it routed by — which for a scan is its
        // lower bound, and is exactly the prefix the scan walks from.
        Method::TxnGet => bump(&READ_HEADS, body.routing_key()),
        Method::TxnScan => bump(&SCAN_HEADS, body.routing_key()),
        _ => {}
    }
}

/// Records a call that never left this client, because `store_id`'s connection was closed.
///
/// **Not every `NotSent`** — only the one this crate can attribute to a store that went away.
/// A request with nowhere to go at all is a different fact and is not counted here.
pub fn record_not_sent(store_id: u64) {
    if !enabled() {
        return;
    }
    NOT_SENT.with_borrow_mut(|counts| *counts.entry(store_id).or_default() += 1);
}

/// Records a connection rebuilt to `store_id`.
pub fn record_redial(store_id: u64) {
    if !enabled() {
        return;
    }
    REDIALS.with_borrow_mut(|counts| *counts.entry(store_id).or_default() += 1);
}

/// Adds one to `map`'s count for the head of `key`, padded when the key is shorter than [`HEAD`].
fn bump(map: &'static std::thread::LocalKey<RefCell<BTreeMap<[u8; HEAD], u64>>>, key: &[u8]) {
    let mut head = [0u8; HEAD];
    let take = key.len().min(HEAD);
    head[..take].copy_from_slice(&key[..take]);
    map.with_borrow_mut(|counts| *counts.entry(head).or_default() += 1);
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
        read_heads: READ_HEADS.with_borrow(Clone::clone),
        scan_heads: SCAN_HEADS.with_borrow(Clone::clone),
        not_sent: NOT_SENT.with_borrow(Clone::clone),
        redials: REDIALS.with_borrow(Clone::clone),
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
    READ_HEADS.with_borrow_mut(BTreeMap::clear);
    SCAN_HEADS.with_borrow_mut(BTreeMap::clear);
    NOT_SENT.with_borrow_mut(BTreeMap::clear);
    REDIALS.with_borrow_mut(BTreeMap::clear);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Cost, enabled, record_call, record_tso, record_wait, reset, taken};

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
        record_call(
            7,
            &crate::wire::Body::Txn(crate::wire::TxnKvReq::Get {
                key: bytes::Bytes::from_static(b"k"),
                ts: 1,
            }),
        );
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
