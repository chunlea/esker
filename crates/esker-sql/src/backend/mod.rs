//! The seam between the executor and storage, and an in-memory transaction to test against.
//!
//! [`Backend`] and [`Txn`] are the whole of what the executor may ask of the store. They are shaped
//! against `esker-client`'s real `TxnClient` and `Transaction` rather than against a sketch, so
//! wiring the real one in is a matter of writing the impl and nothing above it changes:
//!
//! * `put` and `delete` return nothing, because writes are buffered on the client until commit —
//!   the executor must not be written as though a write can fail where it is issued;
//! * `commit` yields the commit timestamp, or `None` when the transaction wrote nothing;
//! * `get` and `scan` take `&self`, because read-your-writes is served out of the buffer.
//!
//! [`StoreBackend`] is the real one, over `esker-client`'s `TxnClient`; [`MemoryBackend`] below is
//! the in-process fake every unit test runs against.
//!
//! # How a unique index is enforced, with nothing added here to do it
//!
//! There is deliberately no `put_if_absent`. Uniqueness composes out of the two primitives above,
//! and between them they cover both ways a duplicate can arrive:
//!
//! 1. **The executor reads the index key inside the transaction and requires it absent.** The read
//!    is at the transaction's snapshot, so a duplicate that is *already committed* is visible and
//!    is reported as `23505 unique_violation` before anything is written.
//! 2. **Then it writes the index entry like any other key.** A *concurrent* duplicate needs no
//!    further help: both transactions read the key as absent, both prewrite the same key, and
//!    write-write conflict detection (`docs/DESIGN.md` §8) lets exactly one commit. The loser's
//!    `commit` fails, and the executor reports that as `23505` too.
//!
//! The fake below implements the same conflict rule as the real protocol — a commit fails if any
//! key it wrote gained a version after this transaction's snapshot — so an executor test can
//! exercise the race rather than assume it. `a_concurrent_duplicate_loses_at_commit` is that test.

mod locks;
mod store;

pub use locks::LockView;

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::error::{Result, SqlError};

pub use store::{SchemaLease, StoreBackend};

/// Opens transactions. One per SQL node, shared by every session.
pub trait Backend: fmt::Debug + Send + Sync {
    /// Starts a transaction at a fresh snapshot.
    fn begin(&self) -> Result<Box<dyn Txn>>;

    /// Starts a **read-only** transaction at a snapshot the caller chose.
    ///
    /// The whole of ADR 0021's Decision 1: a historical read is a read timestamp and nothing else,
    /// so this is `begin` with the number handed in rather than allocated. Locks, resolution,
    /// read-your-writes and commit are all indifferent to where it came from.
    ///
    /// **Read-only is enforced by the transaction this returns**, not left to the caller.
    /// Committing at `commit_ts > start_ts` against a snapshot that old is a lost update with
    /// extra steps, and it is the one window snapshot isolation does not close: the conflicting
    /// writer committed *after* the snapshot and *before* the write, so Percolator's conflict
    /// check would not catch it.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>>;

    /// How long this node may still serve **writes** from a cached schema, or `None` when it has
    /// no lease at all.
    ///
    /// [ADR 0028](../../../docs/adr/0028-the-schema-lease.md). `None` is **fail closed**: a node
    /// that cannot reach PD holds no lease and refuses to write, which is what lets PD's step
    /// clock advance on a timer rather than on a poll of nodes it may not be able to reach.
    ///
    /// Reads are never gated by it. A reader's snapshot already agrees with the rows it can see
    /// (ADR 0020), so gating reads would add stalls and close no hole — and it would take a node
    /// that has lost PD from *degraded* to *useless*, which is the wrong trade for a bound that
    /// only writers can violate.
    ///
    /// The default is an unexpired lease of unbounded length, which is what an in-process fake
    /// with no cluster to lose contact with means. `StoreBackend` overrides it.
    fn schema_lease_remaining(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::MAX)
    }

    /// The step interval PD publishes, or `None` when nothing publishes one.
    ///
    /// The number a driver waits between the state transitions of a schema change, and the extra
    /// a *removing* change waits before its last step ([`StepInterval`]). It comes from the same
    /// PD answer the lease does, for the same reason: a cluster-wide bound needs one writer, and
    /// a node that kept its own copy would drift — and an interval short by exactly the drift is
    /// unsafe rather than merely wrong (`docs/plans/phase-6e.md` §10).
    ///
    /// `None` is not a default interval, it is the absence of one, and
    /// [`crate::exec::redrive::ReDriver`] will not step a job without it. A node that cannot be
    /// told how long to wait must not guess: guessing short breaks the two-version invariant the
    /// interval exists for. The in-process fake answers `None` because it has no PD, which is
    /// also why re-driving is off by default in tests.
    fn schema_step_interval(&self) -> Option<StepInterval> {
        None
    }

    /// The oracle's current timestamp.
    ///
    /// `CLAUDE.md` invariant 6: no node uses its wall clock for ordering, so "now" is a number from
    /// the timestamp oracle like every other. This is what bounds a historical read from above —
    /// a read at a timestamp that has not happened would see a prefix of it and call it complete.
    fn now(&self) -> Result<u64>;
}

/// How long a driver waits between the steps of a schema change.
///
/// PD's `SchemaLease` answer, the two fields of it a driver needs. Mirrors
/// `esker_pd::SchemaLease` rather than sharing it: `esker-sql` does not depend on `esker-pd`, and
/// the numbers arrive over the wire in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInterval {
    /// `lease_ms + lock_ttl_ms` — the wait after a state transition, in either direction.
    ///
    /// Each term bounds how stale a *writer* can be: the lease is how long a node may act without
    /// hearing from PD, the lock TTL how long a transaction that has already started may still
    /// commit. Past their sum no writer can be acting on a state two behind, which is ADR 0020's
    /// two-version invariant.
    pub step_ms: u64,
    /// What a **removing** change waits on top of `step_ms`, and only before its final step: the
    /// MVCC retention window.
    ///
    /// Inert for an add, real for a remove — a reader still at `public` reads entries a node at
    /// `absent` has already deleted, and retention is what keeps them readable. Folding it into
    /// `step_ms` would price every `CREATE INDEX` at the retention window
    /// (`crate::exec::verbs`, "why the last step is the expensive one").
    pub removal_extra_ms: u64,
}

/// What [`Txn::lock`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// This transaction holds the key now — or held it already, which is the same answer.
    Taken,
    /// **Waiting for this would close a cycle**: the holder is, directly or through others,
    /// waiting for the asker. One of the two has to die and it is the one that asked, which is the
    /// only one that can be told.
    Deadlock,
    /// Somebody else holds it and is still inside their lease. The caller waits and asks again;
    /// **it may not resolve the lock**, because an owner inside its lease is alive and taking its
    /// row would be a lost update wearing a successful commit (ADR 0057).
    Held {
        /// The holder's `start_ts`, which is what a wait-for edge is drawn between.
        by: u64,
        /// What is left of the holder's lease, in milliseconds. Zero means it has expired and the
        /// next attempt will resolve it rather than wait again.
        lease_ms: u64,
    },
}

/// One transaction's view of storage.
///
/// # Every method is required, on purpose
///
/// **A wrapper must forward every method, and the compiler is the test.** Seven of these once had
/// defaults, so that a backend which had not learned a method stayed honest — `lock` answered
/// `Taken`, `locks` answered empty, `changed_since_statement` answered `false`. A *wrapper* is not
/// such a backend: it has a real transaction one field away, so a default is exactly the wrong
/// answer there, and forgetting one compiles silently.
///
/// This repository paid for that three times. `savepoint::Recording` swallowed `lock` (the holder
/// never held and the waiter never waited), then `validate_reads` and `changed_since_statement`
/// (a SERIALIZABLE transaction with a savepoint open recorded and validated nothing), and then
/// **`locks`, which shipped**: every `pg_locks` read taken while a savepoint was open answered
/// "nothing is held on this node" — a different claim from "I cannot tell you" — and Rails opens a
/// savepoint for every nested `transaction do`. A fourth wrapper, `GatedTxn` in `tests/redrive.rs`,
/// had opted its own tests out of row locking without anyone noticing.
///
/// So there are no defaults left. A backend that takes no locks writes `Ok(Lock::Taken)` and
/// `LockView::default()` itself, in one line, having decided to; and every future wrapper that
/// forgets a method is a compile error rather than a wrong answer. Adding a method here is meant to
/// break all four implementors — that break is the feature.
pub trait Txn: fmt::Debug + Send {
    /// Reads one key at this transaction's snapshot, its own buffered writes merged in.
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;

    /// Reads `[start, end)` at this transaction's snapshot, in key order, buffered writes merged
    /// in. `limit` is applied after the merge; 0 means no limit.
    ///
    /// **An empty range is no rows, not a panic.** `start >= end` names a range that cannot
    /// contain anything, and it is a range a *query* can ask for: `WHERE id BETWEEN 3 AND 2`
    /// bounds a primary-key scan below by 3 and above by 2, which is exactly this. Every
    /// implementation must answer with no rows — `BTreeMap::range` panics on such a pair, so the
    /// check is the implementation's and cannot be left to the caller (invariant 9: never panic on
    /// user input).
    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>>;

    /// Takes a **row lock** for this transaction, or reports who holds it
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    ///
    /// **The thing a READ COMMITTED writer waits on**, and the reason it cannot live in
    /// [`Txn::put`]: that one returns nothing and so can neither wait nor report. Without a lock
    /// taken *at the statement* there is nothing to wait for — two writers both buffer freely and
    /// whichever commits second loses, which is not even the shape first-committer-wins has (the
    /// **first** writer can be the loser). So the wait and `Op::Lock` are one mechanism and this
    /// is it.
    ///
    /// Idempotent for the holder: a transaction that locks a key twice gets [`Lock::Taken`] both
    /// times, which is what makes a statement re-run cost nothing.
    ///
    /// A backend with no notion of locks answers `Ok(Lock::Taken)`, which is what it was already
    /// doing implicitly. It writes that itself — see the trait's own docs for why none of this is
    /// defaulted.
    fn lock(&mut self, key: &[u8]) -> Result<Lock>;

    /// Tells this transaction which session opened it, for `pg_locks.pid` to report.
    ///
    /// **Required rather than defaulted**, for the reason every required method on these traits
    /// is: a default that dropped the pid would make every lock this transaction takes report
    /// `0`, and `0` is indistinguishable from a transaction nobody claimed. It is set once, by
    /// `Executor::begin_txn`, immediately after the transaction is opened — a session's pid never
    /// changes, so there is nothing to keep in step afterwards.
    fn owned_by_session(&mut self, pid: u32);

    /// Every row lock **this node** holds, and every session waiting for one, for `pg_locks`.
    ///
    /// On the trait rather than on the backend because a catalog view is handed a transaction and
    /// nothing else. A backend that takes no locks answers `LockView::default()`, and an empty
    /// `pg_locks` on such a node says "nothing is held here" — **but only a node that decided to
    /// say it**. This was the one defaulted method that shipped a wrong answer, through a wrapper
    /// that never chose anything (`tests/pg_locks.rs`), which is why it is required now.
    fn locks(&self) -> LockView;

    /// Records what this transaction reads, so that its commit can be validated
    /// ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md)).
    ///
    /// **Only SERIALIZABLE asks for this**, and the cost is why: a read set is memory per
    /// transaction and a check per key at commit. The other two levels are snapshot isolation and
    /// pay nothing.
    ///
    /// Turning it on is one-way within a transaction. A transaction that has already recorded reads
    /// cannot un-record them, and a level that moved under it would otherwise validate half of what
    /// it read — which is not a weaker guarantee, it is an arbitrary one.
    fn validate_reads(&mut self, on: bool);

    /// Whether `key` has a committed version this statement's snapshot does not include.
    ///
    /// **PostgreSQL's `EvalPlanQual` question**, and the reason a lock taken without waiting is
    /// not proof that nothing moved: the writer in front may have committed and released between
    /// this statement's read and its lock, in which case there was nothing to wait for and the
    /// value in hand is still stale. Measured — a single `UPDATE … SET n = n + 1` under three
    /// concurrent writers raised `40001` a hundred times in twelve hundred transactions, where
    /// PostgreSQL re-reads and proceeds.
    ///
    /// `false` is right for a backend that takes no locks: nothing there ever waited, so nothing
    /// there has a statement snapshot to be behind. It says so itself.
    fn changed_since_statement(&self, key: &[u8]) -> Result<bool>;

    /// Takes a fresh read timestamp for the statement that is about to be re-run
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    ///
    /// **Two things move and one does not.** Reads made by the re-run see the newer snapshot, and
    /// writes it produces carry that snapshot as their `read_ts` — so a prewrite validates them
    /// against the version they were actually computed from, and the waiter does not die at its
    /// own commit for the transaction it just waited for. What does **not** move is the
    /// transaction's `start_ts`: its data versions and its lock records are still written there,
    /// so waiters classify it exactly as before and every key an *earlier* statement wrote keeps
    /// its own, older stamp and still conflicts.
    ///
    /// Doing nothing is right for a backend with no locks: nothing there ever waits, so nothing
    /// there ever restarts.
    fn restart_statement(&mut self) -> Result<()>;

    /// Begins a statement: a fresh read timestamp, and the previous statement's undo **discarded**
    /// rather than applied.
    ///
    /// The pair to [`Txn::restart_statement`], and the difference is the whole of why they are two
    /// methods: a statement that ends normally keeps its writes, and one that has to be re-run
    /// gives them back. Calling the restart at a statement's *start* undoes the statement before
    /// it, which is how these came to be separate.
    ///
    /// The fresh timestamp is what READ COMMITTED means for reads: each statement sees what was
    /// committed when it began. A transaction at a level that keeps its snapshot never calls this.
    fn begin_statement(&mut self) -> Result<()>;

    /// Gives back every row lock this transaction holds, without ending it.
    ///
    /// **One caller: the deadlock victim.** A real server ends the loser's transaction with the
    /// `40P01`, so its rows are free the instant the survivor asks again; keeping them until this
    /// block's `ROLLBACK` would deadlock the survivor against a transaction that is already dead.
    /// Doing nothing is right for a backend that takes no locks.
    fn abandon_locks(&mut self);

    /// Buffers a write. Nothing can fail here — the conflict, if there is one, comes from
    /// [`Txn::commit`].
    fn put(&mut self, key: &[u8], value: &[u8]);

    /// Buffers a delete, with the same rule.
    fn delete(&mut self, key: &[u8]);

    /// What this transaction has buffered for `key`, in the buffer's three real states.
    ///
    /// **No default.** A backend that quietly answered "nothing" would make every savepoint
    /// rollback on it silently wrong, and a trait default is exactly how this crate's store path
    /// once opted out of half of ADR 0057 without a single test noticing.
    fn buffered(&self, key: &[u8]) -> Buffered;

    /// Puts `key`'s buffer entry back to what [`Txn::buffered`] gave earlier, **removing** it when
    /// that was `None`. Also no default, for the same reason.
    fn restore(&mut self, key: &[u8], prior: Buffered);

    /// Whether this transaction already holds `key`'s row lock.
    ///
    /// [`Lock::Taken`] cannot answer this — it means "holds it now, **or held it already**" — and a
    /// savepoint rollback has to know the difference, or it gives back a lock the transaction took
    /// before the mark and still needs.
    fn holds(&self, key: &[u8]) -> bool;

    /// Gives back one row lock, for a statement the transaction has rolled back.
    ///
    /// PostgreSQL releases a subtransaction's row locks when it aborts, and so must this: a lock
    /// held for a write that no longer exists blocks every other session on that row for the life
    /// of the outer transaction, and leaves the holder in a wait-for graph it has already left.
    fn unlock(&mut self, key: &[u8]);

    /// What this transaction has recorded reading so far (ADR 0062).
    fn read_set(&self) -> ReadSet;

    /// Puts the recorded read set back to a copy taken at a savepoint.
    ///
    /// **A read the rollback discarded cannot have influenced what the transaction commits**, so
    /// validating it would refuse a transaction for a dependency on a statement that no longer
    /// exists — which is PostgreSQL's answer too: its own `transaction_nested_test.rb` commits the
    /// outer transaction after a `SerializationFailure` inside a savepoint, and so must we.
    fn restore_read_set(&mut self, set: ReadSet);

    /// The snapshot this transaction reads at.
    ///
    /// What `pg_export_snapshot()` hands out, and it must be **this** transaction's rather than a
    /// fresh one: PostgreSQL's verb exports what the exporting transaction sees, so that a second
    /// session importing the token reads exactly the state the first one was reading. Allocating a
    /// new timestamp would export a moment nobody had looked at.
    fn start_ts(&self) -> u64;

    /// Whether this transaction has already buffered a write.
    ///
    /// **The one rule in ADR 0022 Decision 2 that is about correctness rather than cost**: a
    /// transaction that has written and then reads cannot be answered from a columnar learner at
    /// all, because the learner has not seen an uncommitted write. Read-your-writes is served out
    /// of the buffer this asks about, and a fragment goes to a different machine, which has none
    /// of it.
    ///
    /// **No default**, for the reason [`Txn::is_read_only`] has none, and it is the same lesson: a
    /// default that is right for the in-memory fake and silently wrong for a real cluster is
    /// exactly the shape of a bug that passes every test in this crate and returns a wrong answer
    /// against a store.
    fn has_written(&self) -> bool;

    /// Whether this transaction may write.
    ///
    /// False for one opened by [`Backend::begin_at`]. The executor asks *before* it plans, so that
    /// a write at a past snapshot is `25006` naming the command rather than a write that is
    /// buffered and then quietly dropped. [`Txn::put`] and [`Txn::delete`] cannot report anything —
    /// they are buffered and return nothing — which is exactly why the check has to be here.
    ///
    /// **No default**, deliberately. It had one — `false` — and `StoreBackend` inherited it, so the
    /// executor's `25006` fired against the fake and not against a real cluster: there the write
    /// reached the store, was buffered, and failed at commit under a different code. A default that
    /// is right for one implementor and silently wrong for the other is the shape of that bug, so
    /// there is none.
    fn is_read_only(&self) -> bool;

    /// Commits, yielding the commit timestamp, or `None` for a transaction that wrote nothing.
    fn commit(self: Box<Self>) -> Result<Option<u64>>;

    /// Abandons the transaction. Buffered writes are discarded and nothing is visible.
    fn rollback(self: Box<Self>) -> Result<()>;
}

/// What a write buffer holds for one key: `None` for nothing at all, `Some(None)` for a buffered
/// delete, `Some(Some(value))` for a buffered value.
///
/// Three states rather than two, because **"this transaction has no opinion on the key" and "this
/// transaction is deleting the key" are different**, and a savepoint that undoes an insert by
/// writing a tombstone has confused them — the rolled-back row stays in the write set and its
/// commit still conflicts with anyone else who touched it.
pub type Buffered = Option<Option<Bytes>>;

/// A copy of what a transaction has recorded reading, taken when a savepoint opens.
///
/// **A whole copy rather than a delta**, for the reason the savepoint's `Parameters` copy is
/// (private to `exec::savepoint`, so named here rather than linked):
/// a savepoint is rare, a read set is small, and a delta is a second thing to get right. The keys
/// are a set, so there is no insertion order to truncate back to — putting the old copy back is
/// the only exact undo available.
#[derive(Debug, Clone, Default)]
pub struct ReadSet {
    keys: std::collections::BTreeSet<Vec<u8>>,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
}

/// A buffered write.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Write {
    Put(Bytes),
    Delete,
}

/// Every committed version of every key, newest last, plus the clock that stamps them.
///
/// **Derived `Default`, and the clock starts at zero on purpose**: zero is below every wall-clock
/// reading, so [`Versions::mark`] lifts the first timestamp to the present without a starting
/// instant having to be chosen — which is what the hardcoded one used to be for.
#[derive(Debug, Default)]
struct Versions {
    /// `key -> [(commit_ts, value)]`, ascending by timestamp. `None` is a tombstone.
    keys: BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>>,
    /// The last timestamp this oracle handed out. Monotone, and the only source of timestamps
    /// here, which is `CLAUDE.md` invariant 6 kept true even in a fake.
    ///
    /// **Shaped like a real one and now paced like one**: `ts = physical_ms << 18 | logical`
    /// (`esker_pd::tso`), with the physical half following the wall clock and the logical half
    /// advancing per commit within a millisecond. [`Versions::mark`] is `esker_pd::tso`'s own
    /// restart rule — the greater of the last timestamp and the clock — which is what keeps the
    /// sequence monotone across a jump in either direction.
    ///
    /// It used to start at a hardcoded instant and advance *only* the logical half, so every
    /// timestamp a process ever handed out named the same millisecond. That is invisible to a test
    /// that compares two versions to each other, and wrong for every client that compares one to
    /// its own clock: the scoreboard node runs this backend, so `now()` and `CURRENT_TIMESTAMP`
    /// were `2026-08-30 14:00:00 UTC` forever, and `fixtures_test#test_insert_with_default_function`
    /// measured a default six days stale — a frozen oracle, not a folded `DEFAULT`.
    clock: u64,
    /// Milliseconds the tests have pushed the clock **forward by**, and it stays pushed.
    ///
    /// An offset rather than a one-shot bump ([`MemoryBackend::advance_ms`]) because the physical
    /// half is the wall clock now: a bump added to `clock` alone would be swallowed the moment the
    /// clock caught up, and a test that asked for two versions in different milliseconds would get
    /// them only when it ran slowly enough.
    skew_ms: u64,
    /// The row locks this node holds ([`crate::backend::locks::RowLocks`]).
    row_locks: locks::RowLocks,
}

/// The wall clock, in Unix milliseconds — read **here and nowhere else in this crate**.
///
/// `CLAUDE.md` invariant 6 is that no node uses its wall clock for ordering, and this does not
/// break it: [`Versions`] is the timestamp oracle's stand-in, and reading the clock is what an
/// oracle is for (`esker_pd::tso` does exactly this, and guards it with the same mark). Every
/// *other* part of this crate takes its instant from a transaction's `start_ts`.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Versions {
    /// The oracle's mark: **the greater of the last timestamp handed out and the wall clock**,
    /// which is `esker_pd::tso`'s restart rule written for one process.
    ///
    /// It is what makes the sequence survive a clock that stands still (the logical half carries
    /// on inside the millisecond), a clock that jumps backwards (the mark wins), and a test that
    /// pushes the clock forward ([`Self::skew_ms`], which is added to the reading rather than to
    /// the mark, so it cannot be overtaken).
    ///
    /// Reading it does **not** consume a timestamp: `begin` and `now` want the current instant and
    /// two transactions starting in the same millisecond share one, exactly as they did when the
    /// clock only moved at commit. Only a commit allocates, and it allocates above this.
    fn mark(&self) -> u64 {
        let wall = esker_client::ts_at_ms(unix_now_ms().saturating_add(self.skew_ms));
        self.clock.max(wall)
    }

    /// The value visible at `ts`: the newest version committed at or before it.
    fn visible(&self, key: &[u8], ts: u64) -> Option<Bytes> {
        self.keys
            .get(key)?
            .iter()
            .rev()
            .find(|(commit_ts, _)| *commit_ts <= ts)
            .and_then(|(_, value)| value.clone())
    }

    /// Whether **anything in `[lo, hi)`** gained a version after `ts` — the phantom test.
    ///
    /// A key that did not exist when a transaction scanned the range is in no read set, so the
    /// range is what names it. `BTreeMap::range` makes this two seeks rather than a walk of the
    /// store, and an empty or crossed range is no rows rather than a panic (invariant 9).
    fn range_written_since(&self, lo: &[u8], hi: &[u8], ts: u64) -> bool {
        if lo >= hi {
            return false;
        }
        self.keys
            .range(lo.to_vec()..hi.to_vec())
            .any(|(_, versions)| versions.iter().any(|(commit_ts, _)| *commit_ts > ts))
    }

    /// Whether `key` gained a version after `ts` — the write-write conflict Percolator's prewrite
    /// detects by checking the `write` column family for a commit newer than the snapshot.
    fn written_since(&self, key: &[u8], ts: u64) -> bool {
        self.keys
            .get(key)
            .is_some_and(|versions| versions.iter().any(|(commit_ts, _)| *commit_ts > ts))
    }
}

/// An in-memory transactional store: snapshot reads, buffered writes, and the one conflict rule
/// that matters.
///
/// Good enough to test an executor against, and honest about the thing an executor can get wrong —
/// it really does refuse a commit whose keys moved underneath it, so a test can watch two
/// transactions race for the same unique index entry and see one of them lose.
#[derive(Debug, Clone, Default)]
pub struct MemoryBackend {
    versions: Arc<Mutex<Versions>>,
    /// The largest page [`Txn::scan`] will answer with, and what a `limit` of 0 becomes. Zero
    /// means no ceiling, which is [`Txn`]'s own contract. See [`MemoryBackend::with_scan_limit`].
    max_scan: u32,
}

impl MemoryBackend {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        MemoryBackend::default()
    }

    /// Caps every scan at `max` pairs, and reads a `limit` of 0 as `max` rather than as "no
    /// limit".
    ///
    /// **This is the real client's behaviour, not a fault injected for fun.**
    /// `esker_client::Transaction::scan` puts every limit through
    /// `Router::bounded_limit(limit, DEFAULT_SCAN_LIMIT)`, which turns 0 into 1024 and then caps
    /// it at `max_scan_limit`. A fake that answered 0 as "everything" let code that asked for a
    /// whole range look correct here and truncate silently against the real backend — a `DROP
    /// TABLE` that left rows, and a `CREATE INDEX` whose index made a query return *fewer* rows
    /// than the same query without it (`docs/plans/phase-6a.md` §10a).
    ///
    /// So this exists to make that difference *visible to a test*: set it to two and any code
    /// that does not page comes back with two rows. Everything that walks a range goes through
    /// the executor's `for_each_page`, and the tests that pin it set this.
    #[must_use]
    pub fn with_scan_limit(mut self, max: u32) -> Self {
        self.max_scan = max;
        self
    }

    /// Moves the clock's **physical** half on by `millis`, the way time passing does, and leaves
    /// it moved.
    ///
    /// Several commits can land in one millisecond — which is what a busy cluster looks like — and
    /// a test about reading *as of an instant* needs its versions in different ones, because an
    /// instant a user can name has millisecond resolution (`crate::time_machine`). This is how it
    /// says so.
    ///
    /// **A persistent offset, not a bump.** The physical half follows the wall clock
    /// (`Versions::mark`), so adding milliseconds to the mark would buy nothing the moment the
    /// clock caught up: two calls a millisecond apart would collapse into one instant and the test
    /// would pass or fail on how fast the machine was. Added to the *reading*, every later
    /// timestamp carries it and each call moves the clock strictly forward.
    pub fn advance_ms(&self, millis: u64) {
        let mut versions = self.lock();
        versions.skew_ms = versions.skew_ms.saturating_add(millis);
    }

    /// The value visible at the newest committed timestamp, for assertions in tests.
    #[must_use]
    pub fn peek(&self, key: &[u8]) -> Option<Bytes> {
        let versions = self.lock();
        versions.visible(key, versions.clock)
    }

    /// A poisoned lock means another thread panicked while holding it. The data behind it is a
    /// `BTreeMap` that is still structurally sound, and taking it back is better than propagating
    /// a panic into a session (`CLAUDE.md` invariant 9).
    fn lock(&self) -> std::sync::MutexGuard<'_, Versions> {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Backend for MemoryBackend {
    fn begin(&self) -> Result<Box<dyn Txn>> {
        let (start_ts, id) = {
            let mut versions = self.lock();
            let id = versions.row_locks.next_id();
            (versions.mark(), id)
        };
        Ok(Box::new(MemoryTxn {
            session: 0,
            versions: Arc::clone(&self.versions),
            start_ts,
            buffer: BTreeMap::new(),
            read_ts: BTreeMap::new(),
            held: Vec::new(),
            statement_undo: BTreeMap::new(),
            id,
            statement_ts: start_ts,
            max_scan: self.max_scan,
            read_only: false,
            validating: false,
            read_keys: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            read_ranges: std::cell::RefCell::new(Vec::new()),
        }))
    }

    /// The fake is a real time machine, which is the point of it.
    ///
    /// Every version here is already filed under its `commit_ts` and `Versions::visible` already
    /// answers "the newest at or before `ts`", so a historical read is the same call with a
    /// different number — the same sentence that is true of the store below. That is what lets
    /// the whole feature, and the whole `.slt` corpus, be exercised before the client half lands.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>> {
        Ok(Box::new(MemoryTxn {
            session: 0,
            versions: Arc::clone(&self.versions),
            start_ts,
            buffer: BTreeMap::new(),
            read_ts: BTreeMap::new(),
            held: Vec::new(),
            statement_undo: BTreeMap::new(),
            // A read-only transaction takes no lock, so its id is never looked at.
            id: 0,
            statement_ts: start_ts,
            max_scan: self.max_scan,
            read_only: true,
            validating: false,
            read_keys: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            read_ranges: std::cell::RefCell::new(Vec::new()),
        }))
    }

    fn now(&self) -> Result<u64> {
        Ok(self.lock().mark())
    }
}

/// What a scan will actually answer with, given what was asked and the store's ceiling.
///
/// Shaped after `esker_client::Router::bounded_limit`: with a ceiling, a `limit` of 0 means the
/// ceiling rather than "everything", and anything above it is capped. With no ceiling — the
/// default — [`Txn`]'s own contract applies and 0 is unlimited.
fn bounded_limit(limit: u32, max: u32) -> u32 {
    if max == 0 {
        return limit;
    }
    if limit == 0 { max } else { limit.min(max) }
}

/// One transaction against a [`MemoryBackend`].
#[derive(Debug)]
struct MemoryTxn {
    versions: Arc<Mutex<Versions>>,
    start_ts: u64,
    buffer: BTreeMap<Vec<u8>, Write>,
    /// `key -> the read timestamp of the statement that produced this write` (ADR 0057 §4).
    ///
    /// **What first-committer-wins is measured against, per key.** A waiter that waits for another
    /// transaction, re-reads at a fresh timestamp and computes the right answer would otherwise
    /// meet that transaction's commit at its own prewrite and fail `40001` — moving the failure
    /// from the `UPDATE` to the `COMMIT` and changing nothing else. A key written by an *earlier*
    /// statement keeps its own, older stamp, so a commit that slipped in between still conflicts:
    /// that is what makes this per key rather than a transaction-wide "latest read ts", and it is
    /// the half that keeps it sound.
    read_ts: BTreeMap<Vec<u8>, u64>,
    /// What [`Txn::lock`] has taken, so commit and rollback can give it all back.
    held: Vec<Vec<u8>>,
    /// This transaction's identity, which `start_ts` is not: see [`Versions::locks`].
    id: u64,
    /// The backend pid of the session that opened it, which `pg_locks.pid` reports. `0` until
    /// somebody says, which is what an internal transaction with no session behind it stays.
    session: u32,
    /// Each key the **current statement** has written, with the buffer entry it replaced.
    ///
    /// **The undo a restart needs, and it is not a savepoint's.** `ROLLBACK TO` restores the
    /// *value* a key had, which is right for a savepoint and wrong here: a re-run must fall
    /// through to the store and see the row the transaction it waited for committed, and a
    /// restored value in the buffer shadows it. So the entry is removed — or put back to what an
    /// *earlier* statement of this transaction had written, which is what the `Option` carries
    /// (ADR 0057).
    statement_undo: BTreeMap<Vec<u8>, Option<Write>>,
    /// The timestamp this transaction's **statements** read at. Equal to `start_ts` until a
    /// statement waits and re-reads.
    statement_ts: u64,
    /// Whether this transaction's reads are being recorded for commit-time validation (ADR 0062).
    validating: bool,
    /// Every key this transaction has **read** and not written, for validation at commit.
    ///
    /// Behind a `RefCell` because [`Txn::get`] takes `&self` — a read is not a mutation of the
    /// database and the trait says so, but it *is* a mutation of the read set.
    read_keys: std::cell::RefCell<std::collections::BTreeSet<Vec<u8>>>,
    /// Every range this transaction has **scanned**, which is what makes a phantom visible: a key
    /// that did not exist when the scan ran is in no read set, and only the range it would have
    /// appeared in can name it.
    read_ranges: std::cell::RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
    /// The store's scan ceiling; see [`MemoryBackend::with_scan_limit`].
    max_scan: u32,
    /// Set by [`Backend::begin_at`]. A write here is dropped rather than buffered, and the
    /// executor is what turns the attempt into `25006` before it gets this far.
    read_only: bool,
}

impl MemoryTxn {
    fn versions(&self) -> std::sync::MutexGuard<'_, Versions> {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records what the current statement is about to replace, once per key.
    ///
    /// Once, because the first write is the one that says what was there before this statement
    /// touched it; a second write in the same statement replaces a value the statement itself put.
    fn remember(&mut self, key: &[u8]) {
        if !self.statement_undo.contains_key(key) {
            self.statement_undo
                .insert(key.to_vec(), self.buffer.get(key).cloned());
        }
    }

    /// Records which snapshot the value about to be written was computed from (ADR 0057 §4).
    ///
    /// **The earliest stamp wins, and that is the whole of it.** A key this transaction has
    /// already written is read back from the *buffer* — read-your-writes — so a later statement
    /// that writes it again is computing from the earlier statement's value, not from anything it
    /// read at its own snapshot. Taking the later timestamp would say the value came from a
    /// snapshot it never saw, and the prewrite check would then look for conflicts after a moment
    /// that is too late to find them.
    ///
    /// That is not a corner: it is `UPDATE t SET n = n + 1` twice in one transaction, which is
    /// three lines of the harness's own probe, and it lost six increments in 240 with **no error
    /// reported** (run 66). ADR 0057 §4 says it in words — "every key an earlier statement wrote
    /// keeps its own, older stamp and still conflicts" — and the code said the opposite.
    fn stamp(&mut self, key: &[u8]) {
        self.read_ts
            .entry(key.to_vec())
            .or_insert(self.statement_ts);
    }

    /// Records a key this transaction read, when it is recording at all.
    ///
    /// **The catalog is not recorded**, and that is a correctness statement rather than a saving:
    /// every statement reads the catalog, so validating it would make every concurrent `CREATE
    /// TABLE` a serialization failure for every transaction in flight. PostgreSQL's own predicate
    /// locking ignores system catalogs for the same reason (ADR 0062 §1).
    fn record_key(&self, key: &[u8]) {
        if !self.validating || key.first() == Some(&esker_keys::prefix::META) {
            return;
        }
        self.read_keys.borrow_mut().insert(key.to_vec());
    }

    /// The same for a range a scan walked, which is what makes a phantom visible.
    fn record_range(&self, start: &[u8], end: &[u8]) {
        if !self.validating || start.first() == Some(&esker_keys::prefix::META) || start >= end {
            return;
        }
        self.read_ranges
            .borrow_mut()
            .push((start.to_vec(), end.to_vec()));
    }

    /// Gives back every lock this transaction took.
    ///
    /// **On every way out**, which is why it is a method and not a line in `commit`: a transaction
    /// that fails its own prewrite still has to release, or the next writer waits for a
    /// transaction that has already given up.
    fn release(&self) {
        // **No early return on an empty list.** A transaction that waited and never acquired
        // anything holds nothing and is still *in the graph*, and skipping the call is what left it
        // there for the life of the process (run 78).
        self.versions().row_locks.release(self.id, &self.held);
    }
}

/// **Every way out includes the one nobody writes down.**
///
/// A session that disconnects mid-transaction drops its `Box<dyn Txn>` without a `commit` or a
/// `rollback` — `Executor::drop` rolls one back only for a session that made a temporary schema —
/// so a lock released only by those two methods stayed held for the life of the process. With
/// `lock_timeout` at PostgreSQL's default of 0 the next writer to that row waits forever, and an
/// abandoned `psql` did exactly that to this project once already.
///
/// Putting it here makes "the lock dies with its transaction" a property of the type rather than a
/// rule every exit has to remember. Releasing twice is safe: `release` removes a lock only if this
/// transaction still holds it, and ids are never reused.
impl Drop for MemoryTxn {
    fn drop(&mut self) {
        self.release();
    }
}

impl Txn for MemoryTxn {
    /// See [`Txn::owned_by_session`]: set once, right after the transaction is opened.
    fn owned_by_session(&mut self, pid: u32) {
        self.session = pid;
    }

    fn start_ts(&self) -> u64 {
        self.start_ts
    }

    fn has_written(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        // Read-your-writes: the buffer wins, and a buffered delete hides a committed value. A read
        // served from here is this transaction's own value, so nobody else can invalidate it and it
        // is not part of the read set (ADR 0062 §1).
        if let Some(write) = self.buffer.get(key) {
            return Ok(match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            });
        }
        self.record_key(key);
        Ok(self.versions().visible(key, self.statement_ts))
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        // A range that cannot contain anything, which `BTreeMap::range` panics on rather than
        // answering empty. See [`Txn::scan`] for why a query can ask for one.
        if start >= end {
            return Ok(Vec::new());
        }
        // **The range, not the keys it answered.** A key that did not exist when this ran is in no
        // read set and is exactly the phantom a range is here to catch (ADR 0062).
        self.record_range(start, end);
        let versions = self.versions();
        let mut merged: BTreeMap<Vec<u8>, Bytes> = BTreeMap::new();
        // **`range`, not a walk with a comparison inside it.** Both maps are ordered by key, so
        // the range is a pair of seeks; iterating every key and testing the bounds costs the whole
        // store per scan, and a catalog read makes one scan per relation. That is what stopped the
        // Rails suite: 866 relations over a suite's worth of rows made a single `::regclass` take
        // two seconds and a fixture load never finish (`a_scan_costs_its_range_and_not_the_store`).
        for (key, _) in versions
            .keys
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
        {
            if let Some(value) = versions.visible(key, self.statement_ts) {
                merged.insert(key.clone(), value);
            }
        }
        drop(versions);
        // The buffer is applied over the snapshot, so a row this transaction wrote is in its own
        // range scan and one it deleted is not — and it is ranged for the same reason: a
        // transaction that has written a hundred thousand rows must not pay for all of them on
        // every unrelated scan.
        for (key, write) in self
            .buffer
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
        {
            match write {
                Write::Put(value) => {
                    merged.insert(key.clone(), value.clone());
                }
                Write::Delete => {
                    merged.remove(key);
                }
            }
        }
        let rows = merged.into_iter().map(|(k, v)| (Bytes::from(k), v));
        // The limit is applied after the merge, or a buffered row could displace a committed one
        // and the scan would return fewer rows than it should.
        let limit = bounded_limit(limit, self.max_scan);
        Ok(if limit == 0 {
            rows.collect()
        } else {
            rows.take(limit as usize).collect()
        })
    }

    fn begin_statement(&mut self) -> Result<()> {
        self.statement_undo.clear();
        // **A read-only transaction never moves.** `begin_at` is the time machine: its whole
        // purpose is a fixed instant, and advancing it would read the present through a statement
        // that asked for the past.
        if !self.read_only {
            let clock = self.versions().clock;
            self.statement_ts = clock;
        }
        Ok(())
    }

    fn abandon_locks(&mut self) {
        self.release();
        self.held.clear();
    }

    fn restart_statement(&mut self) -> Result<()> {
        for (key, before) in std::mem::take(&mut self.statement_undo) {
            if let Some(write) = before {
                self.buffer.insert(key, write);
            } else {
                self.buffer.remove(&key);
                self.read_ts.remove(&key);
            }
        }
        let clock = self.versions().clock;
        self.statement_ts = clock;
        Ok(())
    }

    fn changed_since_statement(&self, key: &[u8]) -> Result<bool> {
        Ok(self.versions().written_since(key, self.statement_ts))
    }

    fn locks(&self) -> LockView {
        self.versions().row_locks.view()
    }

    fn validate_reads(&mut self, on: bool) {
        // One-way: see [`Txn::validate_reads`]. A transaction that has recorded reads keeps
        // recording them.
        self.validating = self.validating || on;
    }

    /// Takes the row lock, or names the holder. See [`Txn::lock`] for why it is here and not in
    /// [`Txn::put`].
    fn lock(&mut self, key: &[u8]) -> Result<Lock> {
        if self.read_only {
            // A time-machine transaction writes nothing, so it needs nothing and must not take a
            // lock a live writer would then wait behind.
            return Ok(Lock::Taken);
        }
        let taken = self
            .versions()
            .row_locks
            .take(key, self.id, self.start_ts, self.session);
        if matches!(taken, Lock::Taken) && !self.held.iter().any(|held| held == key) {
            self.held.push(key.to_vec());
        }
        Ok(taken)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        if self.read_only {
            return;
        }
        self.remember(key);
        self.stamp(key);
        self.buffer
            .insert(key.to_vec(), Write::Put(Bytes::copy_from_slice(value)));
    }

    fn delete(&mut self, key: &[u8]) {
        if self.read_only {
            return;
        }
        self.remember(key);
        self.stamp(key);
        self.buffer.insert(key.to_vec(), Write::Delete);
    }

    fn buffered(&self, key: &[u8]) -> Buffered {
        self.buffer.get(key).map(|write| match write {
            Write::Put(value) => Some(value.clone()),
            Write::Delete => None,
        })
    }

    fn restore(&mut self, key: &[u8], prior: Buffered) {
        match prior {
            Some(Some(value)) => {
                self.buffer.insert(key.to_vec(), Write::Put(value));
            }
            Some(None) => {
                self.buffer.insert(key.to_vec(), Write::Delete);
            }
            None => {
                // The stamp goes with the write: a later statement writing this key again must
                // compute from what it reads then, not from the snapshot of a write that was
                // undone (ADR 0057 §4, and the same rule `restart_statement` follows).
                self.buffer.remove(key);
                self.read_ts.remove(key);
            }
        }
    }

    fn holds(&self, key: &[u8]) -> bool {
        self.held.iter().any(|held| held == key)
    }

    fn unlock(&mut self, key: &[u8]) {
        self.held.retain(|held| held != key);
        // `give_back` and not `release`: this transaction carries on holding whatever the
        // savepoint did not take, so it is still somebody's and still something to wait for.
        self.versions()
            .row_locks
            .give_back(self.id, &[key.to_vec()]);
    }

    fn read_set(&self) -> ReadSet {
        ReadSet {
            keys: self.read_keys.borrow().clone(),
            ranges: self.read_ranges.borrow().clone(),
        }
    }

    fn restore_read_set(&mut self, set: ReadSet) {
        *self.read_keys.borrow_mut() = set.keys;
        *self.read_ranges.borrow_mut() = set.ranges;
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn commit(self: Box<Self>) -> Result<Option<u64>> {
        if self.buffer.is_empty() {
            // Nothing was written, so nothing needs a timestamp — but a `SELECT … FOR UPDATE` took
            // locks and wrote nothing, and they are this transaction's to give back either way.
            self.release();
            return Ok(None);
        }
        let mut versions = self.versions();

        // Prewrite, in one step because there is no network here: every key this transaction wrote
        // must be untouched since its snapshot. This is the check that makes a unique index work
        // without a dedicated method — two inserts of the same index key both reach here, and the
        // second one finds the first one's version.
        for key in self.buffer.keys() {
            // **Against this key's own read timestamp, not the transaction's start** (ADR 0057
            // §4): a value computed from a commit that arrived while this transaction waited has
            // that commit as its *input*, not as its conflict. A key an earlier statement wrote
            // keeps its older stamp and still conflicts, which is the half that stops this being
            // a licence to lose updates.
            let snapshot = self.read_ts.get(key).copied().unwrap_or(self.start_ts);
            if versions.written_since(key, snapshot) {
                drop(versions);
                self.release();
                return Err(SqlError::SerializationFailure {
                    // **The key is not rendered into the text.** It is a raw engine key —
                    // memcomparable, and full of `0x00` — so `from_utf8_lossy` put NULs into a
                    // message that becomes a C string on the wire, and the client stopped reading
                    // at the first one. That is run 54's `message contents do not agree with
                    // length`, and it killed the connection rather than the statement. A real
                    // server's own sentence carries no key either; the key is on the error's own
                    // field below, where the executor reads it to tell this from a `23505`.
                    message: "a key was written after this transaction's snapshot".to_owned(),
                    // The fake answers per key like a real `Prewrite` does, so the executor's
                    // translation of a lost race into a `23505` is exercised here the same way it
                    // will be against the store rather than only there.
                    key: Some(key.clone()),
                });
            }
        }

        // **The read set, validated** (ADR 0062). Every key this transaction read must be
        // untouched since its snapshot, and every range it scanned must have gained nothing — which
        // is the half that catches a phantom. A key it also *wrote* is skipped: its own prewrite
        // above already refused a commit newer than the snapshot the value came from, and checking
        // it twice would answer the same question with a different timestamp.
        if self.validating {
            for key in self.read_keys.borrow().iter() {
                if self.buffer.contains_key(key) {
                    continue;
                }
                if versions.written_since(key, self.start_ts) {
                    drop(versions);
                    self.release();
                    return Err(SqlError::ReadWriteDependency);
                }
            }
            for (start, end) in self.read_ranges.borrow().iter() {
                if versions.range_written_since(start, end, self.start_ts) {
                    drop(versions);
                    self.release();
                    return Err(SqlError::ReadWriteDependency);
                }
            }
        }

        // **Above the mark, never merely above the last commit**: the physical half is the wall
        // clock, so this is `esker_pd::tso`'s allocation — take the greater of the two and add one
        // in the logical bits.
        let commit_ts = versions.mark().saturating_add(1);
        versions.clock = commit_ts;
        for (key, write) in &self.buffer {
            let value = match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            };
            versions
                .keys
                .entry(key.clone())
                .or_default()
                .push((commit_ts, value));
        }
        drop(versions);
        // **After the versions are visible, never before.** A waiter that took the lock between
        // the two would read the row as it was and compute from a value this transaction has
        // already replaced — the lost update the lock exists to stop.
        self.release();
        Ok(Some(commit_ts))
    }

    fn rollback(self: Box<Self>) -> Result<()> {
        self.release();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Backend, Lock, MemoryBackend, Txn};
    use crate::error::SqlError;
    use crate::sqlstate;

    /// The composition the executor performs for a unique index: read, require absent, then write.
    /// Returns the error the executor would report.
    fn insert_unique(txn: &mut Box<dyn Txn>, index_key: &[u8], row: &[u8]) -> crate::Result<()> {
        if txn.get(index_key)?.is_some() {
            return Err(SqlError::UniqueViolation {
                constraint: "g_b_key".into(),
                key: None,
            });
        }
        txn.put(index_key, row);
        Ok(())
    }

    /// A scan costs its **range**, not the store.
    ///
    /// This is the shape of the bug that stopped the Rails suite dead at
    /// `test/cases/custom_locking_test.rb`: `scan` walked every key in the store and compared each
    /// one against the bounds, so the cost of reading a table's sequences — a prefix of a few keys
    /// — was the size of everything anyone had ever written. `Relations::read` does one of those
    /// scans **per relation**, and `Executor::resolve_regclass` does a `Relations::read` per
    /// `::regclass` literal, so a node with 866 relations and a suite's worth of rows spent two
    /// seconds resolving one name and a fixture load never finished.
    ///
    /// **Asserted as a ratio against a control, not as a stopwatch.** The same scan runs against a
    /// small store and against one with two hundred thousand more keys *outside* the range; a
    /// range scan is indifferent to them and a full walk is a thousand times slower. Comparing the
    /// two is what makes this a statement about complexity rather than about this machine.
    #[test]
    fn a_scan_costs_its_range_and_not_the_store() {
        fn elapsed(outside: usize) -> std::time::Duration {
            let backend = MemoryBackend::new();
            let mut writer = backend.begin().unwrap();
            for at in 0..8u32 {
                writer.put(format!("a/{at:08}").as_bytes(), b"in the range");
            }
            for at in 0..outside {
                writer.put(format!("b/{at:08}").as_bytes(), b"not in the range");
            }
            writer.commit().unwrap();

            let reader = backend.begin().unwrap();
            let start = std::time::Instant::now();
            for _ in 0..200 {
                assert_eq!(reader.scan(b"a/", b"a0", 0).unwrap().len(), 8);
            }
            start.elapsed()
        }

        let control = elapsed(8);
        let loaded = elapsed(200_000);
        assert!(
            loaded < control * 50 + std::time::Duration::from_millis(500),
            "a scan of the same eight keys took {loaded:?} in a store of 200_008 and {control:?}              in a store of 16: the scan is walking the store rather than its range"
        );
    }

    /// A transaction dropped without a commit blocks nothing.
    ///
    /// Written to answer the first hypothesis about the suite hang — that a connection killed
    /// mid-transaction left a lock behind — with evidence rather than argument. **There is no lock
    /// to leave**: `FOR UPDATE` and `LOCK TABLE` are both `0A000` on this node, measured against
    /// the live one, and a transaction's writes live in its own buffer until commit. Dropping the
    /// connection drops the buffer, so the next transaction reads the value that was there before
    /// and may write the same key at once.
    #[test]
    fn a_transaction_dropped_without_a_commit_blocks_nothing() {
        let backend = MemoryBackend::new();
        let mut setup = backend.begin().unwrap();
        setup.put(b"k", b"committed");
        setup.commit().unwrap();

        // The connection that dies mid-transaction: it writes and is never heard from again.
        let mut abandoned = backend.begin().unwrap();
        abandoned.put(b"k", b"uncommitted");
        abandoned.put(b"other", b"uncommitted");
        drop(abandoned);

        let mut next = backend.begin().unwrap();
        assert_eq!(next.get(b"k").unwrap().as_deref(), Some(&b"committed"[..]));
        assert_eq!(next.get(b"other").unwrap(), None);
        next.put(b"k", b"after");
        next.commit().unwrap();
        let reader = backend.begin().unwrap();
        assert_eq!(reader.get(b"k").unwrap().as_deref(), Some(&b"after"[..]));
    }

    #[test]
    fn a_committed_value_is_visible_to_the_next_transaction_and_not_to_an_older_one() {
        let backend = MemoryBackend::new();
        let before = backend.now().unwrap();
        let older = backend.begin().unwrap();

        let mut writer = backend.begin().unwrap();
        writer.put(b"k", b"v");
        // A commit takes a timestamp **after** the snapshot every open transaction holds. Written
        // as a relation and not as `before + 1`: the oracle follows the wall clock, so a
        // millisecond passing between the two calls moves the commit further on and an equality
        // here would be a clock race rather than an assertion.
        let commit_ts = writer
            .commit()
            .unwrap()
            .expect("a write commits at a timestamp");
        assert!(commit_ts > before, "{commit_ts} is not after {before}");

        // The transaction that started first still sees its own snapshot.
        assert_eq!(older.get(b"k").unwrap(), None);
        let newer = backend.begin().unwrap();
        assert_eq!(newer.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn a_transaction_reads_its_own_writes_and_its_own_deletes() {
        let backend = MemoryBackend::new();
        let mut setup = backend.begin().unwrap();
        setup.put(b"a", b"1");
        setup.commit().unwrap();

        let mut txn = backend.begin().unwrap();
        txn.put(b"b", b"2");
        txn.delete(b"a");
        assert_eq!(txn.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(txn.get(b"a").unwrap(), None, "a buffered delete hides it");

        let rows = txn.scan(b"a", b"z", 0).unwrap();
        assert_eq!(rows.len(), 1, "the scan sees the buffer too");
        assert_eq!(rows[0].0.as_ref(), b"b");
    }

    #[test]
    fn a_rolled_back_transaction_leaves_nothing_behind() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        txn.put(b"k", b"v");
        txn.rollback().unwrap();
        assert_eq!(backend.peek(b"k"), None);
    }

    #[test]
    fn a_transaction_that_wrote_nothing_needs_no_timestamp() {
        let backend = MemoryBackend::new();
        let txn = backend.begin().unwrap();
        assert_eq!(txn.commit().unwrap(), None);
    }

    /// The first of the two ways a duplicate arrives: it is already committed, so the in-transaction
    /// read finds it and the executor refuses before writing anything.
    #[test]
    fn a_committed_duplicate_is_caught_by_the_read() {
        let backend = MemoryBackend::new();
        let mut first = backend.begin().unwrap();
        insert_unique(&mut first, b"i/alice", b"row1").unwrap();
        first.commit().unwrap();

        let mut second = backend.begin().unwrap();
        let error = insert_unique(&mut second, b"i/alice", b"row2").unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    }

    /// The second way, and the one no read can catch: both transactions look, both see nothing,
    /// both write. Exactly one commits, and the loser's failure is what the executor turns into
    /// `23505`. This is why there is no `put_if_absent` — the conflict check already is one.
    #[test]
    fn a_concurrent_duplicate_loses_at_commit() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();

        // Both read the index key at their own snapshot; neither sees anything.
        assert_eq!(left.get(b"i/alice").unwrap(), None);
        assert_eq!(right.get(b"i/alice").unwrap(), None);
        insert_unique(&mut left, b"i/alice", b"left").unwrap();
        insert_unique(&mut right, b"i/alice", b"right").unwrap();

        assert!(left.commit().is_ok(), "the first to commit wins");
        let loser = right.commit().expect_err(
            "the second must lose the write-write conflict, or the index is not unique",
        );
        assert_eq!(
            loser.sqlstate(),
            sqlstate::SERIALIZATION_FAILURE,
            "a lost race is 40001 here; the executor is what turns it into 23505 for an index key"
        );
        assert_eq!(backend.peek(b"i/alice").as_deref(), Some(&b"left"[..]));
    }

    /// A conflict is about the keys a transaction *wrote*, not the ones it read past. Two
    /// transactions touching different keys must both commit, or every concurrent insert would
    /// fail and the fake would be useless for testing anything else.
    #[test]
    fn transactions_that_write_different_keys_both_commit() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();
        left.put(b"a", b"1");
        right.put(b"b", b"2");
        assert!(left.commit().is_ok());
        assert!(right.commit().is_ok(), "different keys do not conflict");
    }

    /// A scan must not see a version committed after its snapshot, or a query would return rows
    /// that did not exist when it started.
    #[test]
    fn a_scan_reads_only_its_own_snapshot() {
        let backend = MemoryBackend::new();
        let mut setup = backend.begin().unwrap();
        setup.put(b"k1", b"a");
        setup.commit().unwrap();

        let reader = backend.begin().unwrap();
        let mut writer = backend.begin().unwrap();
        writer.put(b"k2", b"b");
        writer.commit().unwrap();

        let rows = reader.scan(b"k", b"l", 0).unwrap();
        assert_eq!(rows.len(), 1, "k2 was committed after the reader started");
    }

    #[test]
    fn a_scan_respects_its_range_and_limit() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        for key in [&b"a"[..], b"b", b"c", b"d"] {
            txn.put(key, b"v");
        }
        txn.commit().unwrap();

        let reader = backend.begin().unwrap();
        assert_eq!(
            reader.scan(b"b", b"d", 0).unwrap().len(),
            2,
            "end is exclusive"
        );
        assert_eq!(
            reader.scan(b"a", b"z", 2).unwrap().len(),
            2,
            "limit applies"
        );
        assert_eq!(reader.scan(b"a", b"z", 0).unwrap().len(), 4);
    }
    /// **A key written twice keeps the stamp of the statement that first wrote it**, and a commit
    /// over a version that landed in between is refused (ADR 0057 §4).
    ///
    /// Written against the backend rather than through SQL because that is where it still bites:
    /// with a row lock held from the first write, nothing can commit in between and the rule is
    /// invisible. A backend whose `lock` takes the trait's default — **`StoreTxn`, today** — has
    /// no such protection, and there this stamp is the only thing standing between two statements
    /// and a silently lost update.
    #[test]
    fn a_second_statement_writing_one_key_keeps_the_first_statement_s_stamp() {
        let backend = MemoryBackend::new();
        let mut seed = backend.begin().unwrap();
        seed.put(b"k", b"0");
        seed.commit().unwrap();

        let mut txn = backend.begin().unwrap();
        txn.begin_statement().unwrap();
        // Statement one computes from what it read and writes it.
        assert_eq!(txn.get(b"k").unwrap().unwrap(), &b"0"[..]);
        txn.put(b"k", b"1");

        // Somebody else commits the same key. No lock was taken, so nothing stopped them.
        let mut other = backend.begin().unwrap();
        other.put(b"k", b"99");
        other.commit().unwrap();

        // Statement two writes it again — reading its own buffered value, not the new commit.
        txn.begin_statement().unwrap();
        assert_eq!(txn.get(b"k").unwrap().unwrap(), &b"1"[..]);
        txn.put(b"k", b"2");

        let refused = txn.commit().unwrap_err();
        assert_eq!(
            refused.sqlstate(),
            "40001",
            "committing 2 over 99 loses an update: the second write's stamp must not say it saw 99"
        );
    }

    /// **A transaction that gives part of its locks back is still there**, and both maps that
    /// answer for it have to say so.
    ///
    /// This is the call `ROLLBACK TO SAVEPOINT` makes: `Savepoints::rollback_to` unlocks the keys
    /// the savepoint took and no others, so a transaction holding a row from before the mark
    /// reaches [`Txn::unlock`] with rows still held. The lock table read it as "this transaction
    /// is over" and forgot its session and every edge pointing at it — which is `pg_locks` losing
    /// the pid of a lock that is still held, and the deadlock detector losing a waiter that is
    /// still waiting.
    #[test]
    fn giving_one_lock_back_leaves_the_transaction_holding_the_rest() {
        let backend = MemoryBackend::new();
        let mut block = backend.begin().unwrap();
        block.owned_by_session(101);
        // One row locked before a savepoint and one inside it. Only the second is the
        // savepoint's to give back.
        assert!(matches!(block.lock(b"before").unwrap(), Lock::Taken));
        assert!(matches!(block.lock(b"inside").unwrap(), Lock::Taken));

        let mut other = backend.begin().unwrap();
        other.owned_by_session(102);
        assert!(
            matches!(other.lock(b"before").unwrap(), Lock::Held { .. }),
            "the other session waits for the row the savepoint did not take"
        );

        // `ROLLBACK TO SAVEPOINT`, one key at a time, which is how the savepoint log replays.
        block.unlock(b"inside");

        let view = block.locks();
        let held: Vec<(Vec<u8>, u32)> = view
            .held
            .iter()
            .map(|(key, _, _, pid)| (key.clone(), *pid))
            .collect();
        assert_eq!(
            held,
            vec![(b"before".to_vec(), 101)],
            "the row taken before the savepoint is still held, and still by session 101"
        );
        let waiting: Vec<u32> = view.waiting.iter().map(|(_, _, pid)| *pid).collect();
        assert_eq!(
            waiting,
            vec![102],
            "the other session is still waiting for a row this transaction still holds"
        );
    }

    /// And the deadlock detector can still see through it: a cycle closed by a lock the
    /// transaction **kept** is a real deadlock.
    ///
    /// Without the edge above, the walk from the holder ends at once and the second waiter is told
    /// to wait — on a transaction that is waiting for it. That is not a spurious `40P01`; it is the
    /// opposite, and worse: two sessions that hang until a `lock_timeout` neither may have set.
    #[test]
    fn a_cycle_through_a_kept_lock_is_still_found() {
        let backend = MemoryBackend::new();
        let mut block = backend.begin().unwrap();
        block.owned_by_session(101);
        assert!(matches!(block.lock(b"before").unwrap(), Lock::Taken));
        assert!(matches!(block.lock(b"inside").unwrap(), Lock::Taken));

        let mut other = backend.begin().unwrap();
        other.owned_by_session(102);
        assert!(matches!(other.lock(b"theirs").unwrap(), Lock::Taken));
        // `other` waits for the row the savepoint will not give back.
        assert!(matches!(other.lock(b"before").unwrap(), Lock::Held { .. }));

        block.unlock(b"inside");

        // Now this block asks for the row `other` holds, and `other` is waiting for this block:
        // a cycle, through a lock the rollback kept.
        assert!(
            matches!(block.lock(b"theirs").unwrap(), Lock::Deadlock),
            "a cycle closed by a lock the transaction kept is a deadlock"
        );
    }
}
