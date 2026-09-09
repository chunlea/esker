//! **Saying that a transaction is still alive**, which is the half of the lock lease nobody sent
//! ([ADR 0088](../../docs/adr/0088-a-row-lock-across-nodes.md)).
//!
//! A Percolator lock carries a lease: `ttl_ms` from its `start_ts`, after which any transaction
//! that wants the key may settle its owner and take it. That is what keeps a crashed client from
//! blocking a row for ever, and the shorter the lease the sooner a dead holder is cleaned up.
//!
//! `TxnKv::Heartbeat` has been the answer to the other side of it since phase 5 — a replicated
//! command, a store handler, a response carrying the lease now in effect — with **nobody sending
//! one**. `docs/DESIGN.md` §8 said so as a known gap, and it cost little while the only locks a
//! transaction held were a commit's: those live for the length of a two-phase commit, which is
//! milliseconds, and a lease of three seconds is never in question.
//!
//! ADR 0088 changed the arithmetic. A `SELECT … FOR UPDATE` lock is taken when the statement runs
//! and held until the transaction ends, and a client that sits for four seconds between two
//! statements is an ordinary client, not a broken one. Under a three-second lease its lock is
//! resolved out from under it by the next session that wants the row, and it finds out at its
//! commit — which is the loudest possible way to lose a promise, and still a promise lost.
//!
//! # Why a thread and not a call per statement
//!
//! The lock has to be renewed while the client is doing *nothing*, which is exactly when no code
//! path of its own runs. Renewing on each statement would leave the gap between two statements
//! open, and that gap is the whole problem.
//!
//! # The cadence is derived, not configured
//!
//! A third of the lease, the rule [ADR 0028](../../docs/adr/0028-the-schema-lease.md) already
//! settled for the schema lease: a lost round trip still leaves two attempts before anything
//! expires. Nothing here reads a wall clock — the lease is measured in the physical part of a
//! timestamp from the oracle (`CLAUDE.md` invariant 6), and the thread's own sleep is only how
//! often it asks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use esker_proto::TxnKvReq;

use crate::router::Router;
use crate::txn::{TimestampOracle, physical_ms};
use crate::wire::Body;

/// What one live transaction needs renewed: the primary is the only lock a resolver consults
/// (`docs/txn-spec.md` §5.5), so it is the only one that has to be told.
#[derive(Debug, Clone)]
struct Lease {
    primary: Bytes,
    ttl_ms: u64,
}

/// The transactions this client is keeping alive.
#[derive(Debug)]
pub(crate) struct Renewals {
    live: Mutex<BTreeMap<u64, Lease>>,
    stop: Arc<AtomicBool>,
    router: Arc<Router>,
    oracle: Arc<dyn TimestampOracle>,
    /// Started on the **first** registration rather than with the client: a client whose
    /// transactions never take an eager lock is every client that only writes, and it should not
    /// pay a thread for a message it will never send.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Renewals {
    pub(crate) fn new(router: Arc<Router>, oracle: Arc<dyn TimestampOracle>) -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(BTreeMap::new()),
            stop: Arc::new(AtomicBool::new(false)),
            router,
            oracle,
            thread: Mutex::new(None),
        })
    }

    /// Starts renewing `start_ts`'s lease. Idempotent: a transaction locks many keys and has one
    /// primary.
    pub(crate) fn register(self: &Arc<Self>, start_ts: u64, primary: &Bytes, ttl_ms: u64) {
        {
            let Ok(mut live) = self.live.lock() else {
                // A poisoned registry means a renewal thread panicked. Not renewing is the safe
                // direction — a lock that expires is resolved, which is the behaviour this whole
                // module is an improvement on rather than a departure from.
                return;
            };
            live.entry(start_ts).or_insert_with(|| Lease {
                primary: primary.clone(),
                ttl_ms,
            });
        }
        self.ensure_running(ttl_ms);
    }

    /// Stops renewing it, which every ending of a transaction must reach — **including the one
    /// that is not a method call.** A renewal that outlived its transaction would make the lock
    /// immortal and a crashed client a permanent one, which is worse than the gap this closes.
    pub(crate) fn forget(&self, start_ts: u64) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&start_ts);
        }
    }

    fn ensure_running(self: &Arc<Self>, ttl_ms: u64) {
        let Ok(mut thread) = self.thread.lock() else {
            return;
        };
        if thread.is_some() {
            return;
        }
        // A third of the lease, and never zero: a cadence of zero is a spin.
        //
        // Taken from the **first** transaction to register, which is exact because `lock_ttl_ms` is
        // the client's and every transaction it opens carries the same one — and the builder that
        // can change it (`with_lock_ttl_ms`) runs before any transaction exists. A per-transaction
        // lease would make this the wrong number for the shortest of them, and the honest fix then
        // is the minimum rather than the first.
        let period = Duration::from_millis((ttl_ms / 3).max(1));
        let renewals = Arc::clone(self);
        let stop = Arc::clone(&self.stop);
        *thread = std::thread::Builder::new()
            .name("esker-lock-renewal".to_owned())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(period);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    renewals.round();
                }
            })
            .ok();
    }

    /// One pass over every live transaction.
    ///
    /// **The lease is absolute, not a delta**: `Heartbeat`'s `ttl_ms` is measured from the lock's
    /// own `start_ts`, so keeping a lock alive means asking for *how long it has been held* plus
    /// another lease. One timestamp per round covers every transaction in it.
    fn round(&self) {
        let leases: Vec<(u64, Lease)> = {
            let Ok(live) = self.live.lock() else {
                return;
            };
            if live.is_empty() {
                return;
            }
            live.iter()
                .map(|(start_ts, lease)| (*start_ts, lease.clone()))
                .collect()
        };
        let Ok(now) = self.oracle.timestamp() else {
            // The oracle is the placement driver; a client that cannot reach it has larger
            // problems than a lease, and the next round asks again.
            return;
        };
        for (start_ts, lease) in leases {
            let held_ms = physical_ms(now).saturating_sub(physical_ms(start_ts));
            let request = TxnKvReq::Heartbeat {
                start_ts,
                primary: lease.primary.clone(),
                ttl_ms: held_ms.saturating_add(lease.ttl_ms),
            };
            // **Best effort, and silent about it.** A failed renewal is not a failed transaction:
            // the lease still has two thirds of itself left by construction, and the next round
            // is inside it. What a failure must not do is stop the loop, which is why nothing here
            // returns and nothing here retries — a transaction whose renewals all fail expires,
            // which is the behaviour that existed before this module.
            let _ = self.router.call(&Body::Txn(request));
        }
    }
}

impl Drop for Renewals {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut thread) = self.thread.lock()
            && let Some(handle) = thread.take()
        {
            // It sleeps in period-length steps and checks the flag on both sides of the sleep, so
            // the join is bounded by one period.
            let _ = handle.join();
        }
    }
}
