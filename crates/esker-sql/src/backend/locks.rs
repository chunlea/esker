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
    /// `transaction id -> the backend pid of the session that owns it`.
    ///
    /// **What makes `pg_locks.pid` name a session.** The table is keyed by transaction, which is
    /// the right key for holding and for the wait-for graph and the wrong one for a client: what
    /// it wants to do with a row of `pg_locks` is join it to `pg_stat_activity` or hand it to
    /// `pg_cancel_backend`, and both take a backend pid. Kept beside the ids rather than widening
    /// `locks` and `waits_for`, so the graph and its deadlock walk are untouched.
    sessions: BTreeMap<u64, u32>,
    /// `waiter -> (the transaction it is waiting for, the key it is waiting on)`, while it waits.
    ///
    /// **The edge names the key as well as the holder, and that is what makes a partial release
    /// safe.** Naming only the holder cannot answer "is this edge still true after that
    /// transaction gave one row back", and both wrong answers are real: keeping a stale edge
    /// closes a cycle that does not exist (`40P01` to a transaction that was never in one), and
    /// dropping a live one hides a cycle that does (two sessions hanging until a `lock_timeout`
    /// neither set). With the key here the question is exact — an edge dies with the key it names.
    ///
    /// **The wait-for graph, and a cycle in it is a deadlock.** Without this a wait with no
    /// `lock_timeout` is a wait with no end: two transactions taking two rows in opposite orders
    /// hang, which is worse than the `40001` this replaced. PostgreSQL waits `deadlock_timeout`
    /// and then looks for a cycle; this looks on every attempt, because the graph is a map of the
    /// sessions currently waiting and walking it is cheaper than the sleep that would precede it.
    ///
    /// Node-local, which is every deadlock two sessions of one `esker-sql` process can make. A
    /// cycle across nodes needs a graph both can see — PD's job, and a named follow-on.
    waits_for: BTreeMap<u64, (u64, Vec<u8>)>,
}

/// What one node's lock table holds, flattened for `pg_locks`.
///
/// A snapshot rather than a borrow: the view is read under the same mutex every other caller takes,
/// and holding that mutex while a catalog view formats rows would make reading `pg_locks` block
/// every statement on the node — which is the opposite of what a diagnostic is for.
#[derive(Debug, Default)]
pub struct LockView {
    /// `(key, holder transaction id, holder start_ts, holder backend pid)` — one per key held.
    pub held: Vec<(Vec<u8>, u64, u64, u32)>,
    /// `(waiter transaction id, the id it waits for, waiter backend pid)` — one per blocked
    /// session.
    pub waiting: Vec<(u64, u64, u32)>,
}

impl RowLocks {
    /// A snapshot of the table, for `pg_locks`.
    pub(crate) fn view(&self) -> LockView {
        LockView {
            held: self
                .locks
                .iter()
                .map(|(key, (holder, start_ts))| {
                    (key.clone(), *holder, *start_ts, self.session_of(*holder))
                })
                .collect(),
            waiting: self
                .waits_for
                .iter()
                .map(|(waiter, (holder, _))| (*waiter, *holder, self.session_of(*waiter)))
                .collect(),
        }
    }

    /// The backend pid behind a transaction id, or `0` for one whose session never said.
    ///
    /// `0` is not a pid any session has, so a row carrying it is visibly "nobody's" rather than
    /// quietly somebody's — which is what reporting the *process* id did.
    fn session_of(&self, id: u64) -> u32 {
        self.sessions.get(&id).copied().unwrap_or(0)
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
    pub(crate) fn take(&mut self, key: &[u8], id: u64, start_ts: u64, session: u32) -> Lock {
        // Recorded on every attempt, granted or not: a waiter has a row in `pg_locks` too, and it
        // is the row a stuck client is looking for.
        self.sessions.insert(id, session);
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
                self.waits_for.insert(id, (holder, key.to_vec()));
                Lock::Held {
                    by: holder_ts,
                    lease_ms: u64::MAX,
                }
            }
            // Ours already, or nobody's. **Either way this transaction is no longer waiting**,
            // and the edge has to go with the wait: the graph below is what the deadlock detector
            // walks, so an edge left behind can close a cycle that does not exist and answer
            // `40P01` to a transaction that was never in one. It also outlives its client in
            // `pg_locks`, which is how run 78 found it.
            Some(_) => {
                self.waits_for.remove(&id);
                Lock::Taken
            }
            None => {
                self.locks.insert(key.to_vec(), (id, start_ts));
                self.waits_for.remove(&id);
                Lock::Taken
            }
        }
    }

    /// **`id` has stopped waiting**, whatever it is doing next.
    ///
    /// A waiter is recorded the moment it is refused, because that edge is what another
    /// transaction's deadlock walk reads — and it must come out again the moment the wait ends, not
    /// when the transaction does. A `lock_timeout`, a `statement_timeout`, a cancelled statement and
    /// a `NOWAIT` clause all end a wait while the block lives on, and the edge left behind answers
    /// for a wait that is not happening: the walk goes through a transaction that has given up and
    /// closes a cycle with nobody in it.
    ///
    /// Idempotent, and called on paths that have already cleared it — the acquisition and the
    /// deadlock both do — because one exit that always tidies is cheaper to keep right than five
    /// that each have to remember.
    pub(crate) fn stop_waiting(&mut self, id: u64) {
        self.waits_for.remove(&id);
    }

    /// **The transaction is over**: every key of `held` that `id` still holds, its wait, the
    /// edges pointing at it, and the session behind it.
    ///
    /// Checking the holder is what makes this safe to call twice, and what makes it safe to call
    /// from a destructor: a key this transaction no longer holds belongs to somebody else.
    ///
    /// Use [`RowLocks::give_back`] for a transaction that carries on — the two differ in
    /// everything except which keys they drop, and calling this one for a partial release is what
    /// took a live transaction's pid out of `pg_locks` and a live waiter out of the graph.
    pub(crate) fn release(&mut self, id: u64, held: &[Vec<u8>]) {
        self.waits_for.remove(&id);
        // **And every edge pointing at it**, whatever key it names. `drop` below clears the edges
        // on the keys actually given back, which is the exact rule; this is the sweep that makes
        // the *ended* transaction unconditional, because a caller whose `held` list has drifted
        // from what the table thinks it holds would otherwise leave an edge naming a transaction
        // that is gone for good. `give_back` deliberately does not do this — see there.
        self.waits_for.retain(|_, (holder, _)| *holder != id);
        // Nothing can ask whose the transaction was, so its row goes from here too. Left behind,
        // this would grow by one entry per transaction for the life of the process.
        self.sessions.remove(&id);
        self.drop(id, held);
    }

    /// **Gives back part of what `id` holds and leaves it running**: `ROLLBACK TO SAVEPOINT`, and
    /// a deadlock inside one.
    ///
    /// The keys go and nothing else does. A transaction that still holds a row is still a
    /// transaction to wait for, so the edges pointing at it stand, and it is still somebody's, so
    /// `pg_locks` keeps naming the backend that owns it. Those are the two things
    /// [`RowLocks::release`] deliberately destroys, and they are why this is a second entry point
    /// rather than a flag: the difference is what the caller *means*, and a bool at the call site
    /// says it in the place where it is easiest to get backwards.
    ///
    /// The waiters that were queued on the keys it *does* give back lose their edges, because
    /// `drop` clears an edge with the key it names. That is the whole of what a partial release
    /// may conclude, and it is enough: a deadlock's victim inside a savepoint frees exactly the
    /// rows the survivor is asleep on.
    pub(crate) fn give_back(&mut self, id: u64, keys: &[Vec<u8>]) {
        self.drop(id, keys);
    }

    /// The half the two share: forget the keys of `keys` that `id` actually holds, **and the
    /// edges of the waiters that were waiting on them**.
    ///
    /// A freed key blocks nobody, so every edge naming it is stale the instant it is removed —
    /// whether the transaction that held it has ended or is carrying on with the rest. Clearing
    /// them here rather than leaving them to the waiter's next poll is what closes the two
    /// millisecond window a deadlock's victim retries inside: the survivor's edge still named the
    /// victim, so the retry walked survivor → victim → itself and was told `40P01` for a cycle
    /// that had ended a moment earlier.
    ///
    /// The waiter is still waiting, and that is correct: it is waiting for a key **nobody holds**,
    /// and its next attempt either takes it or draws a new edge to whoever got there first.
    fn drop(&mut self, id: u64, keys: &[Vec<u8>]) {
        for key in keys {
            if self.locks.get(key).map(|(holder, _)| *holder) == Some(id) {
                self.locks.remove(key);
                self.waits_for.retain(|_, (_, on)| on != key);
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
                Some((next, _)) => at = *next,
                None => return false,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::RowLocks;
    use crate::backend::Lock;

    /// **A transaction that stops waiting stops being a waiter**, and both ways out count: it
    /// acquires the key, or it gives up holding nothing.
    ///
    /// The wait-for graph is what the deadlock detector walks, so a stale edge is not merely an
    /// untidy `pg_locks` row — it is an edge that can close a cycle that does not exist, and the
    /// answer to that is `40P01` for a transaction that was never in a deadlock.
    ///
    /// Run 78's capture is the untidy half: a waiter still listed an hour after its client exited,
    /// with a second one accumulated beside it.
    #[test]
    fn a_wait_ends_when_it_stops_waiting() {
        let mut locks = RowLocks::default();
        let (a, b) = (locks.next_id(), locks.next_id());
        assert!(matches!(locks.take(b"k", a, 10, 101), Lock::Taken));
        assert!(matches!(locks.take(b"k", b, 20, 102), Lock::Held { .. }));
        assert_eq!(
            locks.waits_for.get(&b).map(|(holder, _)| *holder),
            Some(a),
            "b is waiting for a"
        );

        // Both sessions are known while both are in the table, which is what `pg_locks.pid`
        // reports: the holder's row and the waiter's row name different backends.
        let view = locks.view();
        assert_eq!(view.held, vec![(b"k".to_vec(), a, 10, 101)]);
        assert_eq!(view.waiting, vec![(b, a, 102)]);

        // A ends; B takes the key it was waiting for.
        locks.release(a, &[b"k".to_vec()]);
        assert_eq!(
            locks.sessions.get(&a),
            None,
            "a transaction that ended is not still somebody's"
        );
        assert!(matches!(locks.take(b"k", b, 20, 102), Lock::Taken));
        assert!(
            locks.waits_for.is_empty(),
            "b holds the key now and is waiting for nobody: {:?}",
            locks.waits_for
        );
    }

    /// **A transaction that has given everything back cannot be waited for**, and the edge saying
    /// so goes with it.
    ///
    /// The deadlock detector walks these edges on every contended attempt, and a deadlock's victim
    /// releases at once and retries — so an edge still naming it makes the retry walk
    /// survivor → victim → itself and answer `40P01` for a cycle that ended a moment ago. That is
    /// `transaction_nested_test.rb`'s recoverable-deadlock test, 15 runs of 15 red before this.
    #[test]
    fn releasing_clears_the_edges_that_pointed_at_the_transaction() {
        let mut locks = RowLocks::default();
        let (a, b) = (locks.next_id(), locks.next_id());
        assert!(matches!(locks.take(b"k", a, 10, 101), Lock::Taken));
        assert!(matches!(locks.take(b"k", b, 20, 102), Lock::Held { .. }));
        assert_eq!(
            locks.waits_for.get(&b).map(|(holder, _)| *holder),
            Some(a),
            "b waits for a"
        );

        // `a` gives everything back — a deadlock's victim does this before it retries.
        locks.release(a, &[b"k".to_vec()]);
        assert!(
            locks.waits_for.is_empty(),
            "nothing waits for a transaction that holds nothing: {:?}",
            locks.waits_for
        );
        // And the retry is a plain acquisition rather than a phantom cycle.
        assert!(matches!(locks.take(b"k", b, 20, 102), Lock::Taken));
    }

    /// The other way out: the waiter gives up — `lock_timeout` — and so holds nothing at all.
    ///
    /// `release` used to return early on an empty list, which is exactly the transaction whose edge
    /// nothing else would ever remove.
    #[test]
    fn a_waiter_that_gave_up_holding_nothing_is_forgotten() {
        let mut locks = RowLocks::default();
        let (a, b) = (locks.next_id(), locks.next_id());
        assert!(matches!(locks.take(b"k", a, 10, 101), Lock::Taken));
        assert!(matches!(locks.take(b"k", b, 20, 102), Lock::Held { .. }));

        locks.release(b, &[]);
        assert!(
            locks.waits_for.is_empty(),
            "the transaction that gave up is not waiting for anything: {:?}",
            locks.waits_for
        );
    }

    /// **A wait ends before its transaction does**, and the graph has to hear about it then.
    ///
    /// `release` is the transaction's end and cannot be the answer: a `lock_timeout` fires, the
    /// statement fails, and the block goes on — with an edge saying it is still waiting. What that
    /// costs is not an untidy `pg_locks` row but a wrong `40P01`: the walk below runs from the
    /// holder, so a transaction that gave up sitting in the middle of the chain closes cycles for
    /// everybody behind it.
    #[test]
    fn a_wait_that_ended_leaves_the_graph_before_the_transaction_does() {
        let mut locks = RowLocks::default();
        let (a, b, c) = (locks.next_id(), locks.next_id(), locks.next_id());
        assert!(matches!(locks.take(b"a", a, 10, 101), Lock::Taken));
        assert!(matches!(locks.take(b"b", b, 20, 102), Lock::Taken));
        // `b` asks for `a`'s row and gives up — `lock_timeout`, and its block lives on.
        assert!(matches!(locks.take(b"a", b, 20, 102), Lock::Held { .. }));
        locks.stop_waiting(b);
        assert!(
            locks.waits_for.is_empty(),
            "the transaction that gave up is not waiting: {:?}",
            locks.waits_for
        );

        // So `a` asking for the row `b` holds is a wait, not a cycle.
        assert!(matches!(locks.take(b"b", a, 10, 101), Lock::Held { .. }));
        // And `b` is still there, holding its row and named in `pg_locks`.
        let held: Vec<(Vec<u8>, u32)> = locks
            .view()
            .held
            .iter()
            .map(|(key, _, _, pid)| (key.clone(), *pid))
            .collect();
        assert_eq!(held, vec![(b"a".to_vec(), 101), (b"b".to_vec(), 102)]);
        // A third transaction behind `b` is a real wait, and still not a cycle.
        assert!(matches!(locks.take(b"b", c, 30, 103), Lock::Held { .. }));
    }
}
