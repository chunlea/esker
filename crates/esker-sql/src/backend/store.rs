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
        StoreBackend { client, oracle }
    }
}

impl Backend for StoreBackend {
    fn begin(&self) -> Result<Box<dyn Txn>> {
        Ok(Box::new(StoreTxn {
            inner: Some(self.client.begin().map_err(translate)?),
        }))
    }

    /// **The one stub in this crate, and it refuses rather than approximates.**
    ///
    /// ADR 0021 Decision 1 makes this three lines — `TxnClient::begin_at(start_ts)`, which is
    /// `begin` with the timestamp handed in — and that constructor is another lane's and does not
    /// exist yet (`docs/plans/phase-6d.md` §3). Until it does, a historical read against a real
    /// cluster is `0A000` **naming what is missing**.
    ///
    /// Reading the present instead would be the defect this crate's lowering exists to prevent: a
    /// user asks for an hour ago, gets now, and nothing tells them. `MemoryBackend` implements the
    /// real behaviour, so the feature above this line is fully tested either way.
    fn begin_at(&self, _start_ts: u64) -> Result<Box<dyn Txn>> {
        // TODO(phase-6d): `Ok(Box::new(StoreTxn { inner: Some(self.client.begin_at(start_ts)?) }))`
        // once `esker-client` has the constructor. Nothing else here changes.
        Err(SqlError::FeatureNotSupported(
            "reading as of a past timestamp against a real store".to_owned(),
        ))
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
