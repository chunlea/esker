//! Row locks, taken at the statement and given back when the transaction ends
//! ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
//!
//! **One mechanism, two backends.** This started inside `MemoryBackend`, where it is what makes
//! READ COMMITTED testable in-process; the real backend needs the same table for the same reason
//! and would otherwise have a second copy of a wait-for graph — the shape that costs a project two
//! sets of rules for one question. What differs between the two is *scope*, and the difference is
//! declared rather than hidden: on `MemoryBackend` this is every session there is, and on
//! `StoreBackend` it is every session **of one node**, with a conflict between nodes still
//! resolving where it always did, at prewrite.
//!
//! # What a lock is not
//!
//! It is not a Percolator lock. Nothing here reaches a store, nothing survives the process, and
//! nothing has a lease — a holder is alive exactly while its transaction is, which is why
//! [`Lock::Held`] reports a lease of `u64::MAX`: the honest answer to "how long before I may take
//! this from you" is *never*, take it up with the holder.

use std::collections::BTreeMap;

use crate::backend::Lock;

/// The row locks one node holds, and the graph of who is waiting for whom.
#[derive(Debug, Default)]
pub(crate) struct RowLocks {
    /// `key -> (the id of the transaction holding it, that transaction's start_ts)`.
    ///
    /// **Keyed by transaction id and not by `start_ts`**, which is the distinction the first
    /// version of this got wrong: `MemoryBackend`'s clock advances on *commit*, so two
    /// transactions that begin before either commits share a `start_ts` — and a lock owned by
    /// "whoever has that stamp" is a lock the second one thinks it already holds. The `start_ts`
    /// is carried beside it because that is what a waiter reports and what a wait-for edge is
    /// drawn between.
    locks: BTreeMap<Vec<u8>, (u64, u64)>,
    /// Hands out the transaction ids above. Never reused, so an id identifies one transaction for
    /// the life of the process — which is what makes releasing twice safe.
    next_txn: u64,
    /// `waiter -> the transaction it is waiting for`, while it waits.
    ///
    /// **The wait-for graph, and a cycle in it is a deadlock.** Without this a wait with no
    /// `lock_timeout` is a wait with no end: two transactions taking two rows in opposite orders
    /// hang, which is worse than the `40001` this replaced. PostgreSQL waits `deadlock_timeout`
    /// and then looks for a cycle; this looks on every attempt, because the graph is a map of the
    /// sessions currently waiting and walking it is cheaper than the sleep that would precede it.
    ///
    /// Node-local, which is every deadlock two sessions of one `esker-sql` process can make. A
    /// cycle across nodes needs a graph both can see — PD's job, and a named follow-on.
    waits_for: BTreeMap<u64, u64>,
}

/// What one node's lock table holds, flattened for `pg_locks`.
///
/// A snapshot rather than a borrow: the view is read under the same mutex every other caller takes,
/// and holding that mutex while a catalog view formats rows would make reading `pg_locks` block
/// every statement on the node — which is the opposite of what a diagnostic is for.
#[derive(Debug, Default)]
pub struct LockView {
    /// `(key, holder transaction id, holder start_ts)` — one per key actually held.
    pub held: Vec<(Vec<u8>, u64, u64)>,
    /// `(waiter transaction id, the id it is waiting for)` — one per session currently blocked.
    pub waiting: Vec<(u64, u64)>,
}

impl RowLocks {
    /// A snapshot of the table, for `pg_locks`.
    pub(crate) fn view(&self) -> LockView {
        LockView {
            held: self
                .locks
                .iter()
                .map(|(key, (holder, start_ts))| (key.clone(), *holder, *start_ts))
                .collect(),
            waiting: self
                .waits_for
                .iter()
                .map(|(waiter, holder)| (*waiter, *holder))
                .collect(),
        }
    }

    /// An id for a new transaction. Ids start at 1 and are never reused.
    pub(crate) fn next_id(&mut self) -> u64 {
        self.next_txn += 1;
        self.next_txn
    }

    /// Takes `key` for `id`, or names the holder.
    ///
    /// Idempotent for the holder: a transaction that locks a key twice gets [`Lock::Taken`] both
    /// times, which is what makes a statement re-run cost nothing.
    pub(crate) fn take(&mut self, key: &[u8], id: u64, start_ts: u64) -> Lock {
        match self.locks.get(key) {
            Some(&(holder, holder_ts)) if holder != id => {
                if self.deadlocks(id, holder) {
                    // **The waiter is the victim**, which is the cheapest correct choice: it is
                    // the one asking, so it is the one that can be told. PostgreSQL picks by age;
                    // the difference a client sees is which of two transactions gets the `40P01`,
                    // and both servers give it to exactly one.
                    self.waits_for.remove(&id);
                    return Lock::Deadlock;
                }
                self.waits_for.insert(id, holder);
                Lock::Held {
                    by: holder_ts,
                    lease_ms: u64::MAX,
                }
            }
            // Ours already, or nobody's.
            Some(_) => Lock::Taken,
            None => {
                self.locks.insert(key.to_vec(), (id, start_ts));
                Lock::Taken
            }
        }
    }

    /// Gives back every one of `held` that `id` still holds, and forgets that `id` was waiting.
    ///
    /// Checking the holder is what makes this safe to call twice, and what makes it safe to call
    /// from a destructor: a key this transaction no longer holds belongs to somebody else.
    pub(crate) fn release(&mut self, id: u64, held: &[Vec<u8>]) {
        self.waits_for.remove(&id);
        for key in held {
            if self.locks.get(key).map(|(holder, _)| *holder) == Some(id) {
                self.locks.remove(key);
            }
        }
    }

    /// Whether `waiter` waiting for `holder` closes a cycle in the wait-for graph.
    ///
    /// Walks from the holder: if the chain of who-waits-for-whom comes back to the waiter, the two
    /// are in a cycle and one of them has to die. Bounded by the number of waiters, so it
    /// terminates even if the map is somehow inconsistent.
    fn deadlocks(&self, waiter: u64, holder: u64) -> bool {
        let mut at = holder;
        for _ in 0..=self.waits_for.len() {
            if at == waiter {
                return true;
            }
            match self.waits_for.get(&at) {
                Some(&next) => at = next,
                None => return false,
            }
        }
        false
    }
}
