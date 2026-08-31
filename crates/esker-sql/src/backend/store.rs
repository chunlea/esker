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
use esker_client::{Error as ClientError, Transaction, TxnClient};

use crate::backend::{Backend, Txn};
use crate::error::{Result, SqlError};

/// Opens transactions against a real cluster.
#[derive(Debug)]
pub struct StoreBackend {
    client: Arc<TxnClient>,
}

impl StoreBackend {
    /// A backend over a client. One per SQL node, shared by every session — the client holds the
    /// region cache, and a cache warmed by one session is warm for all of them.
    #[must_use]
    pub fn new(client: Arc<TxnClient>) -> Self {
        StoreBackend { client }
    }
}

impl Backend for StoreBackend {
    fn begin(&self) -> Result<Box<dyn Txn>> {
        Ok(Box::new(StoreTxn {
            inner: Some(self.client.begin().map_err(translate)?),
        }))
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
