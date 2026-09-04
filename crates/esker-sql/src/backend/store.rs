//! The real backend: [`crate::backend::Backend`] over `esker-client`'s `TxnClient`.
//!
//! This is the whole of the wiring. Nothing above it changes — the executor, the planner and the
//! session were written against the trait, and the trait was shaped against this client rather
//! than against a sketch (`docs/plans/phase-6a.md` §5), so what is left is the impl and the
//! translation of one crate's errors into another's.
//!
//! # Two things this file exists to get right
//!
//! **A scan's `limit` means different things on the two sides.** [`crate::backend::Txn`] says a
//! `limit` of 0 is no limit; `Transaction::scan` reads 0 as the protocol's default and then caps
//! it. Nothing here papers over that — the executor already walks a range a page at a time
//! (`crate::exec::for_each_page`) precisely because it cannot ask for one in a single call, and
//! this backend passes the limit through unchanged. A backend that quietly looped to satisfy a
//! `limit` of 0 would put an unbounded read behind a call that looks bounded.
//!
//! **A conflict has to arrive as a conflict, carrying its key.** `Error::TxnConflict` names the
//! key that lost when the store answered per key, and that is what lets the executor report the
//! `23505` a user actually caused rather than the `40001` the storage layer saw
//! (`docs/txn-spec.md` §6.1). Losing the key here would silently turn every duplicate-key error
//! into a retryable-looking race.

use std::sync::Arc;

use bytes::Bytes;
use esker_client::{Error as ClientError, TimestampOracle, Transaction, TxnClient};

use crate::backend::locks::RowLocks;
use crate::backend::{Backend, Lock, Txn};
use crate::error::{Result, SqlError};

/// Opens transactions against a real cluster.
#[derive(Debug)]
pub struct StoreBackend {
    client: Arc<TxnClient>,
    oracle: Arc<dyn TimestampOracle>,
    /// Where this node's schema lease comes from, or `None` for a node with no placement driver.
    lease: Option<Arc<dyn SchemaLease>>,
    /// The row locks **this node** holds
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    ///
    /// The same table `MemoryBackend` uses, and deliberately not a second implementation of a
    /// wait-for graph. What differs is its *scope*, and the difference is declared rather than
    /// hidden: two sessions of **one** `esker-sql` process see each other's locks and block; two
    /// sessions of different nodes do not, and their conflict resolves where it always did, at
    /// prewrite, with the loser told `40001`.
    ///
    /// That is strictly better than what was here — a node that took no locks at all — and it is
    /// not the end state: a lock every node can see is a store-side operation, and §5 of the ADR
    /// says what it costs.
    locks: Arc<std::sync::Mutex<RowLocks>>,
}

/// Where a node's schema lease comes from
/// ([ADR 0028](../../../docs/adr/0028-the-schema-lease.md)).
///
/// A trait rather than a `PdClient` field for one reason that matters: **the test that proves fail
/// closed has to be able to stop answering.** A lease source that could only be a live PD would
/// leave "PD is unreachable" untestable, and untested fail-closed is fail-open with good
/// intentions.
pub trait SchemaLease: std::fmt::Debug + Send + Sync {
    /// How long this node may still serve writes, or `None` if it cannot say.
    ///
    /// `None` is the whole safety property: a node that cannot renew must **stop writing**, so
    /// that PD's step clock can advance on a timer rather than on a poll of nodes it may not be
    /// able to reach.
    fn remaining(&self) -> Option<std::time::Duration>;

    /// The step interval from the same PD answer, or `None` if it cannot say.
    ///
    /// One source rather than two, because the numbers arrive together: PD computes the interval
    /// *from* the lease (`step_interval_ms = lease_ms + lock_ttl_ms`), so a node holding one
    /// without the other would be holding half an arithmetic. A source that has lost PD answers
    /// `None` to both.
    fn step_interval(&self) -> Option<crate::backend::StepInterval>;
}

impl StoreBackend {
    /// A backend over a client. One per SQL node, shared by every session — the client holds the
    /// region cache, and a cache warmed by one session is warm for all of them.
    ///
    /// The oracle is passed alongside rather than reached through the client because the caller
    /// already built one to construct the client, and because a timestamp is the *only* thing
    /// this crate wants from it (`CLAUDE.md` invariant 6: never a wall clock).
    #[must_use]
    pub fn new(client: Arc<TxnClient>, oracle: Arc<dyn TimestampOracle>) -> Self {
        StoreBackend {
            client,
            oracle,
            lease: None,
            locks: Arc::new(std::sync::Mutex::new(RowLocks::default())),
        }
    }

    /// One transaction, given this node's clock and its lock table.
    ///
    /// The id comes from the table rather than from `start_ts`, for the reason `RowLocks` records:
    /// two transactions can share a timestamp, and a lock owned by "whoever has that stamp" is a
    /// lock the second one believes it already holds.
    fn wrap(&self, inner: Transaction) -> StoreTxn {
        let id = self.locks.lock().map_or(0, |mut locks| locks.next_id());
        StoreTxn {
            inner: Some(inner),
            oracle: Arc::clone(&self.oracle),
            locks: Arc::clone(&self.locks),
            id,
            held: Vec::new(),
            validating: false,
            read_keys: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            read_ranges: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The same backend, holding a schema lease from PD.
    ///
    /// Without one this node writes freely, which is right for a cluster with no placement driver
    /// — every test cluster in this crate, and the in-process fake — and wrong for one with a
    /// schema change in flight. `esker-sql`'s binary attaches it when it is told PD's address.
    #[must_use]
    pub fn with_schema_lease(mut self, lease: Arc<dyn SchemaLease>) -> Self {
        self.lease = Some(lease);
        self
    }
}

impl Backend for StoreBackend {
    fn begin(&self) -> Result<Box<dyn Txn>> {
        Ok(Box::new(self.wrap(self.client.begin().map_err(translate)?)))
    }

    /// ADR 0021 Decision 1, and it really is three lines.
    ///
    /// The client's `begin_at` is `begin` with the timestamp handed in rather than allocated, and
    /// it enforces the two bounds this layer can only *advise* on: the future, and the **real**
    /// safepoint, which is PD's and not something the SQL layer can compute from retention. Both
    /// come back as `22023` here, carrying the number the store named — so a user who asked too
    /// far back is told the floor that is actually in force rather than the one this node guessed.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>> {
        Ok(Box::new(
            self.wrap(self.client.begin_at(start_ts).map_err(translate)?),
        ))
    }

    /// This node's lease, or an unbounded one when nothing is publishing them.
    ///
    /// **`None` only when a lease source exists and has run out**, which is the difference between
    /// "no schema changes are being coordinated here" and "I have lost the thing that coordinates
    /// them". A cluster with no placement driver has no staged schema change to be behind on; a
    /// node that *had* a lease and lost it does.
    fn schema_lease_remaining(&self) -> Option<std::time::Duration> {
        match &self.lease {
            Some(lease) => lease.remaining(),
            None => Some(std::time::Duration::MAX),
        }
    }

    /// The interval this node's lease source publishes, and `None` when there is no source.
    ///
    /// Note that this defaults the *opposite* way to the lease above, on purpose. A node with no
    /// lease source writes freely — "nobody is coordinating" is not a reason to stop. A node with
    /// no interval source does not **re-drive**, because re-driving without knowing the wait
    /// would mean inventing one, and an invented interval that is short is exactly the unsafety
    /// the number exists to prevent. Not writing is a stall; stepping early is wrong.
    fn schema_step_interval(&self) -> Option<crate::backend::StepInterval> {
        self.lease.as_ref()?.step_interval()
    }

    fn now(&self) -> Result<u64> {
        self.oracle
            .timestamp()
            .map_err(|error| SqlError::StoreUnavailable(error.to_string()))
    }
}

/// One transaction against a real cluster.
///
/// The `Option` is what lets `commit` and `rollback` take `self: Box<Self>` and hand the
/// transaction on by value, which is how the client's API is shaped: a transaction is consumed by
/// its own end, so that using one afterwards is a compile error there and an internal error here.
#[derive(Debug)]
struct StoreTxn {
    inner: Option<Transaction>,
    /// Where a statement's read timestamp comes from (`CLAUDE.md` invariant 6: never a wall clock).
    oracle: Arc<dyn TimestampOracle>,
    /// This node's lock table, shared with every other session on it.
    locks: Arc<std::sync::Mutex<RowLocks>>,
    /// This transaction's identity in that table, which `start_ts` is not.
    id: u64,
    /// The keys this transaction has locked, so they can all be given back at once.
    held: Vec<Vec<u8>>,
    /// Whether this transaction's reads are recorded for commit-time validation (ADR 0062).
    validating: bool,
    /// The keys it has read, and the ranges it has scanned. Behind `RefCell` because a read takes
    /// `&self` — a read is not a mutation of the database, and it *is* a mutation of the read set.
    read_keys: std::cell::RefCell<std::collections::BTreeSet<Vec<u8>>>,
    read_ranges: std::cell::RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
}

/// **A lock dies with its transaction, including the way out nobody writes down.**
///
/// The same reason `MemoryTxn` has one: a session that disconnects mid-transaction takes neither
/// `commit` nor `rollback`, so a lock released only by those two would be held for the life of the
/// process — and with `lock_timeout` at PostgreSQL's default of 0, the next writer to that row
/// waits forever. That cost four suite files a round (run 66).
impl Drop for StoreTxn {
    fn drop(&mut self) {
        self.release();
    }
}

impl StoreTxn {
    fn open(&self) -> Result<&Transaction> {
        self.inner
            .as_ref()
            .ok_or_else(|| SqlError::Internal("a transaction was used after it ended".into()))
    }

    fn open_mut(&mut self) -> Option<&mut Transaction> {
        self.inner.as_mut()
    }

    fn take(&mut self) -> Result<Transaction> {
        self.inner
            .take()
            .ok_or_else(|| SqlError::Internal("a transaction ended twice".into()))
    }

    /// Records a key this transaction read, when it is recording at all.
    ///
    /// **The catalog is excluded**, and that is a correctness statement rather than a saving: every
    /// statement reads the catalog, so validating it would make every concurrent `CREATE TABLE` a
    /// serialization failure for every transaction in flight (ADR 0062 §1). The decision is here,
    /// in the SQL layer, because the client is byte-opaque (`CLAUDE.md` invariant 7).
    fn record_key(&self, key: &[u8]) {
        if !self.validating || key.first() == Some(&esker_keys::prefix::META) {
            return;
        }
        if let Ok(mut keys) = self.read_keys.try_borrow_mut() {
            keys.insert(key.to_vec());
        }
    }

    /// The same for a range a scan walked, which is what makes a phantom visible.
    fn record_range(&self, start: &[u8], end: &[u8]) {
        if !self.validating || start.first() == Some(&esker_keys::prefix::META) || start >= end {
            return;
        }
        if let Ok(mut ranges) = self.read_ranges.try_borrow_mut() {
            ranges.push((start.to_vec(), end.to_vec()));
        }
    }

    /// Gives back every row lock this transaction took on this node.
    ///
    /// A poisoned lock table is ignored rather than panicked on: this runs from a destructor, and
    /// a panic there while another thread already panicked holding the table would abort the
    /// process (`CLAUDE.md` invariant 9).
    fn release(&self) {
        if self.held.is_empty() {
            return;
        }
        if let Ok(mut locks) = self.locks.lock() {
            locks.release(self.id, &self.held);
        }
    }
}

impl Txn for StoreTxn {
    fn start_ts(&self) -> u64 {
        // A transaction that has ended has no snapshot to report; every caller reaches this
        // through a live one, and zero is a timestamp no oracle hands out.
        self.inner.as_ref().map_or(0, Transaction::start_ts)
    }

    /// The client's own write buffer, which is where read-your-writes is served from.
    ///
    /// A transaction that has ended answers `true`, which is the conservative reading: nothing
    /// reaches this through one, and "may have written" is the safe direction for a caller
    /// deciding whether a *different machine* can answer its read.
    fn has_written(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|txn| !Transaction::is_empty(txn))
    }

    /// **Not the trait's default.** The default is `false`, and taking it here was a real defect
    /// for as long as it stood: the executor's `25006` check asks this, so a write at a past
    /// snapshot was refused against the fake and not against a real cluster — where it instead
    /// reached the store, was buffered, and failed at commit with a different code. A test over
    /// the fake could not have caught it, which is why `tests/real_backend.rs` asks.
    fn is_read_only(&self) -> bool {
        self.inner.as_ref().is_some_and(Transaction::is_read_only)
    }

    /// Takes this node's row lock, or names the holder
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    ///
    /// **Node-local, and that is a declared scope rather than an approximation.** Two sessions of
    /// one `esker-sql` process block on each other exactly as they do on the in-process backend;
    /// two sessions of different nodes do not see each other's locks, and their conflict resolves
    /// where it always did — at prewrite, with the loser told `40001`. Nothing is weakened by that:
    /// what a cross-node pair gets is what *every* pair got before this, and the per-key read
    /// timestamp (§4, already on the wire) is what keeps it honest.
    ///
    /// A cluster-wide lock is a store-side operation with a wire and a log change behind it, and
    /// the ADR asks the question rather than half-answering it here.
    fn lock(&mut self, key: &[u8]) -> Result<Lock> {
        // A read-only transaction writes nothing, so it needs nothing — and must not take a lock a
        // live writer would then wait behind.
        if self.is_read_only() {
            return Ok(Lock::Taken);
        }
        let start_ts = self.start_ts();
        let Ok(mut locks) = self.locks.lock() else {
            // A poisoned table means another session panicked holding it. Refusing to lock is the
            // safe direction: the caller proceeds without the wait, which is what this node did
            // before locks existed at all.
            return Ok(Lock::Taken);
        };
        let taken = locks.take(key, self.id, start_ts);
        drop(locks);
        if matches!(taken, Lock::Taken) && !self.held.iter().any(|held| held == key) {
            self.held.push(key.to_vec());
        }
        Ok(taken)
    }

    /// A fresh read timestamp for the statement beginning now, from the oracle.
    ///
    /// **This is what makes READ COMMITTED real against a cluster**: without it every statement of
    /// a transaction read at `BEGIN`'s snapshot, which is REPEATABLE READ wearing another name. The
    /// timestamp costs one TSO call per statement, which is what a real server pays for a snapshot
    /// per statement too.
    ///
    /// A read-only transaction is left alone: it was opened at a timestamp the caller chose — a
    /// time-machine read — and moving its snapshot forward would answer a different past.
    fn begin_statement(&mut self) -> Result<()> {
        if self.is_read_only() {
            return Ok(());
        }
        // The oracle's refusal is a `ProtoError` rather than the client's error type, so it is
        // named here rather than run through `translate`: a statement that cannot get a timestamp
        // is a statement that cannot read, and saying which is more use than a generic internal.
        let at = self.oracle.timestamp().map_err(|error| {
            SqlError::Internal(format!("no timestamp for this statement: {error}"))
        })?;
        if let Some(txn) = self.open_mut() {
            txn.begin_statement(at);
        }
        Ok(())
    }

    /// The same, for a statement that waited and must now run again — and the writes of the
    /// attempt that waited are **given back** first.
    ///
    /// A re-run is not a second statement. Its predecessor's writes were computed from a snapshot
    /// this transaction has now left behind, so keeping them would validate the re-run against the
    /// moment *before* the wait and kill it with a `40001` naming the commit it waited for — which
    /// is what three real stores answered before this existed: `a commit at 1007 beat this
    /// transaction at 1004`.
    fn restart_statement(&mut self) -> Result<()> {
        let at = self.oracle.timestamp().map_err(|error| {
            SqlError::Internal(format!("no timestamp for this statement: {error}"))
        })?;
        if let Some(txn) = self.open_mut() {
            txn.restart_statement(at);
        }
        Ok(())
    }

    /// Gives the deadlock victim's rows back at once, without ending the transaction.
    fn abandon_locks(&mut self) {
        self.release();
        self.held.clear();
    }

    fn locks(&self) -> crate::backend::LockView {
        // A poisoned table answers "nothing held" rather than panicking: `pg_locks` is a
        // diagnostic, and a diagnostic that takes the node down when the node is already in
        // trouble is worse than one that says less.
        self.locks
            .lock()
            .map(|locks| locks.view())
            .unwrap_or_default()
    }

    fn validate_reads(&mut self, on: bool) {
        self.validating = self.validating || on;
    }

    /// **The question ADR 0066 added, asked** — the store path's answer is exact now.
    ///
    /// One round trip per locked key per statement, and only for a write statement under READ
    /// COMMITTED whose lock was taken without waiting: the writer in front may have committed and
    /// released between this statement's read and its lock, and a lock taken at once cannot say.
    fn changed_since_statement(&self, key: &[u8]) -> Result<bool> {
        let txn = self.open()?;
        Ok(txn
            .latest_commit(key)
            .map_err(translate)?
            .is_some_and(|newest| newest > txn.reading_ts()))
    }

    fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.record_key(key);
        self.open()?.get(key).map_err(translate)
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        self.record_range(start, end);
        self.open()?.scan(start, end, limit).map_err(translate)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        // Buffered on the client, so there is nothing to fail and nothing to report. A
        // transaction that has already ended cannot be written to; dropping the write is right
        // because the only caller that could do it is one that ignored an error from `commit`.
        if let Some(txn) = self.open_mut() {
            txn.put(key, value);
        }
    }

    fn delete(&mut self, key: &[u8]) {
        if let Some(txn) = self.open_mut() {
            txn.delete(key);
        }
    }

    fn commit(mut self: Box<Self>) -> Result<Option<u64>> {
        // **The read set is handed over here, at the last moment**, because a transaction records
        // right up to its commit and the client only needs it once (ADR 0062 §1).
        let keys: Vec<Bytes> = self
            .read_keys
            .borrow()
            .iter()
            .map(|key| Bytes::copy_from_slice(key))
            .collect();
        let ranges: Vec<(Bytes, Bytes)> = self
            .read_ranges
            .borrow()
            .iter()
            .map(|(start, end)| (Bytes::copy_from_slice(start), Bytes::copy_from_slice(end)))
            .collect();
        let mut inner = self.take()?;
        if !keys.is_empty() || !ranges.is_empty() {
            inner.checking(keys, ranges);
        }
        inner.commit().map_err(translate)
    }

    fn rollback(mut self: Box<Self>) -> Result<()> {
        self.take()?.rollback().map_err(translate)
    }
}

/// One crate's errors as another's, and the four that are not obvious.
///
/// * **A conflict keeps its key.** Everything in `docs/txn-spec.md` §6.1 rests on it, and the
///   executor's `23505` depends on it.
/// * **An ambiguous result is its own condition.** A request went out and no usable answer came
///   back, so whether it was applied is unknown. PostgreSQL has `40003
///   statement_completion_unknown` for exactly this, because two-phase commit has the same
///   problem — reporting success would be a lie and reporting failure would be a different one.
/// * **A lock that could not be cleared is a conflict**, not an outage: some other transaction
///   holds it and this one made no progress. `40001` is retryable and this is retryable.
/// * **Everything else that is the network is `08006`**, and everything else that is not is
///   internal. A store that answered something impossible is this crate's bug or the store's, and
///   dressing it up as a user error would send the user looking in the wrong place.
fn translate(error: ClientError) -> SqlError {
    match error {
        // The time machine's three refusals, which the client raises and this layer renames. Each
        // keeps the number the store named: a floor this node guessed from retention would be a
        // different number, and telling a user the wrong one is worse than telling them none.
        ClientError::SnapshotTooOld { requested, floor } => SqlError::ParameterOutOfRange {
            value: crate::time_machine::render(requested),
            name: crate::time_machine::READ_AS_OF,
            low: crate::time_machine::render(floor),
            high: "now".to_owned(),
        },
        ClientError::SnapshotInTheFuture { requested, now } => SqlError::ParameterOutOfRange {
            value: crate::time_machine::render(requested),
            name: crate::time_machine::READ_AS_OF,
            low: "the safepoint".to_owned(),
            high: crate::time_machine::render(now),
        },
        // Unreachable through SQL — the executor refuses a write at a past snapshot before it
        // plans, naming the command, which is the message PostgreSQL sends. Translated anyway,
        // because a `25006` that arrived from below is still a `25006` and must not become an
        // internal error if some path ever reaches it.
        ClientError::ReadOnlyTransaction { .. } => SqlError::ReadOnlyTransaction("this statement"),
        ClientError::NoSuchSnapshot { name } => {
            SqlError::SnapshotDoesNotExist(String::from_utf8_lossy(&name).into_owned())
        }
        ClientError::TxnConflict {
            start_ts,
            commit_ts,
            key,
        } => SqlError::SerializationFailure {
            message: format!("a commit at {commit_ts} beat this transaction at {start_ts}"),
            key: key.map(|key| key.to_vec()),
        },
        ClientError::LockNotCleared { start_ts } => SqlError::SerializationFailure {
            message: format!("a lock from the transaction at {start_ts} could not be cleared"),
            key: None,
        },
        error @ ClientError::AmbiguousResult { .. } => SqlError::OutcomeUnknown(error.to_string()),
        error @ (ClientError::RetriesExhausted { .. }
        | ClientError::DeadlineExceeded { .. }
        | ClientError::NoRegion { .. }
        | ClientError::Store(_)) => SqlError::StoreUnavailable(error.to_string()),
        other => SqlError::Internal(other.to_string()),
    }
}
