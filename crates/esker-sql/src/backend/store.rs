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

use crate::backend::{Backend, Txn};
use crate::error::{Result, SqlError};

/// Opens transactions against a real cluster.
#[derive(Debug)]
pub struct StoreBackend {
    client: Arc<TxnClient>,
    oracle: Arc<dyn TimestampOracle>,
    /// Where this node's schema lease comes from, or `None` for a node with no placement driver.
    lease: Option<Arc<dyn SchemaLease>>,
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
        Ok(Box::new(StoreTxn {
            inner: Some(self.client.begin().map_err(translate)?),
        }))
    }

    /// ADR 0021 Decision 1, and it really is three lines.
    ///
    /// The client's `begin_at` is `begin` with the timestamp handed in rather than allocated, and
    /// it enforces the two bounds this layer can only *advise* on: the future, and the **real**
    /// safepoint, which is PD's and not something the SQL layer can compute from retention. Both
    /// come back as `22023` here, carrying the number the store named — so a user who asked too
    /// far back is told the floor that is actually in force rather than the one this node guessed.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>> {
        Ok(Box::new(StoreTxn {
            inner: Some(self.client.begin_at(start_ts).map_err(translate)?),
        }))
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

    fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.open()?.get(key).map_err(translate)
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
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
        self.take()?.commit().map_err(translate)
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
