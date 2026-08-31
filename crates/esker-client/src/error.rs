//! What a client call can fail with, and what the caller is entitled to conclude from it.
//!
//! The distinction that matters is not which layer failed, it is what the caller may now
//! assume about the database. `esker-proto` already draws it — every [`ProtoError`] answers
//! [`ProtoError::outcome`] with `NotApplied` or `Unknown` — and this type carries that answer
//! outwards through [`Error::changed_nothing`] rather than re-deriving it. A caller has to be
//! able to tell "the store refused and changed nothing" from "nobody knows whether the write
//! landed", because those demand opposite reactions: retry the first, investigate the second.
//! `prompts/05-txn.md` is built on exactly that split.

use bytes::Bytes;

use crate::wire::{Method, ProtoError, RequestOutcome};

/// A failed client call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The store refused, for a reason this client does not retry: it carries no redirect
    /// hint, or retrying it could not help.
    #[error(transparent)]
    Store(#[from] ProtoError),

    /// The store kept answering with a redirectable error until the retry budget ran out.
    /// Every retried error is a refusal, so nothing was written; the cluster is unhealthy or
    /// the caller's budget is too small for how long it takes to elect a leader.
    #[error("gave up after {attempts} attempts: {source}")]
    RetriesExhausted {
        /// Calls made, the first attempt included.
        attempts: u32,
        /// The last refusal.
        source: Box<ProtoError>,
    },

    /// The call's deadline passed. Whether anything was written follows from `source`.
    #[error("deadline passed after {attempts} attempts")]
    DeadlineExceeded {
        /// Calls made before the deadline stopped the loop.
        attempts: u32,
        /// The last failure seen, if the deadline interrupted a retry rather than a wait.
        source: Option<Box<ProtoError>>,
    },

    /// **The write may or may not have been applied.** The request went out and no usable
    /// answer came back, so this client will not send it again: under last-write-wins a
    /// repeated write is harmless *only* when the first one provably did not commit, and here
    /// it is not provable. The caller decides — read the key back, or fail the operation.
    ///
    /// Reads never produce this. Re-reading is always safe, so an unanswered read is retried
    /// or surfaced as a plain [`Error::Store`].
    #[error("the {method:?} may or may not have been applied: {source}")]
    AmbiguousResult {
        /// The method whose fate is unknown.
        method: Method,
        /// How the answer was lost.
        source: Box<ProtoError>,
    },

    /// No region in the cache covers the key and none could be fetched. Routing is broken,
    /// not the store.
    #[error("no region covers the key")]
    NoRegion {
        /// The key that could not be routed.
        key: Bytes,
    },

    /// The request would not fit in one frame. Refused here rather than at the far end, where
    /// it would look like a connection failure.
    #[error("request of about {bytes} bytes exceeds the {limit}-byte frame limit")]
    RequestTooLarge {
        /// What the request encodes to, at least.
        bytes: usize,
        /// The transport's ceiling.
        limit: usize,
    },

    /// The store answered a different method than the one that was asked. A bug or a version
    /// skew; never something to interpret.
    #[error("asked for {expected:?} and got {actual:?}")]
    UnexpectedResponse {
        /// What was sent.
        expected: Method,
        /// What came back.
        actual: Method,
    },

    /// A transaction lost a write-write race: something committed after its snapshot
    /// (`docs/txn-spec.md` §5.2). Nothing of it was written. First-committer-wins, and only a
    /// **new transaction** at a fresh `start_ts` can succeed — retrying this one cannot.
    #[error("transaction at {start_ts} conflicts with a commit at {commit_ts}")]
    TxnConflict {
        /// The losing transaction's snapshot.
        start_ts: u64,
        /// The winner's commit timestamp.
        commit_ts: u64,
        /// **Which key lost**, when the store said per key — which `Prewrite` does
        /// ([ADR 0016](../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
        ///
        /// A caller above this one may need to know: a lost race on an ordinary row is a
        /// serialization failure and a lost race on a *unique index entry* is a duplicate key,
        /// and only the key tells them apart (`docs/txn-spec.md` §6.1). `None` means the
        /// method that refused does not answer per key, so the store named no key and this
        /// layer will not invent one.
        key: Option<Bytes>,
    },

    /// A transaction was settled by someone else — rolled back because its lock expired, or
    /// found already committed. Its own client is now the one holding a stale belief.
    #[error("transaction at {start_ts} was already settled: {detail}")]
    TxnSettled {
        /// The transaction's snapshot.
        start_ts: u64,
        /// Which way, and by what evidence.
        detail: String,
    },

    /// A lock stood in the way for the whole of a call's budget: its owner kept heartbeating,
    /// or the resolution kept losing a race. Nothing was written.
    #[error("a lock held by the transaction at {start_ts} did not clear in time")]
    LockNotCleared {
        /// The transaction holding it.
        start_ts: u64,
    },

    /// A bug in this crate rather than a failure of the cluster.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Whether the caller may assume the database is untouched.
    ///
    /// Defers to [`ProtoError::outcome`] wherever there is one, so the client and the protocol
    /// can never disagree about what a given failure means. The two variants with no
    /// `ProtoError` behind them answer from their own facts: a request that was never routed
    /// or never built changed nothing, and an answer for the wrong method proves the store
    /// did *something*.
    ///
    /// Callers that must not double-apply — the transaction layer, a `CompareAndSwap` retry
    /// loop — branch on this rather than on the variant.
    #[must_use]
    pub fn changed_nothing(&self) -> bool {
        match self {
            Self::Store(error) => error.outcome() == RequestOutcome::NotApplied,
            Self::RetriesExhausted { source, .. } | Self::AmbiguousResult { source, .. } => {
                source.outcome() == RequestOutcome::NotApplied
            }
            Self::DeadlineExceeded { source, .. } => source
                .as_deref()
                .is_none_or(|error| error.outcome() == RequestOutcome::NotApplied),
            // A request that was never routed or never built changed nothing. So did a
            // transaction that lost a write-write race and one whose lock never cleared: both
            // are the store *saying* it refused, which means its answer arrived.
            Self::NoRegion { .. }
            | Self::RequestTooLarge { .. }
            | Self::TxnConflict { .. }
            | Self::LockNotCleared { .. } => true,
            // An answer came back, so the store acted; what it did is anybody's guess. A
            // transaction settled by someone else is the sharpest case of that — something
            // *was* written, by them — and an internal bug here proves nothing about the
            // cluster, so the safe reading is the pessimistic one.
            Self::UnexpectedResponse { .. } | Self::TxnSettled { .. } | Self::Internal(_) => false,
        }
    }
}

/// The result of a client call.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::wire::{Method, ProtoError};

    #[test]
    fn only_an_unanswered_write_leaves_the_database_in_doubt() {
        assert!(
            Error::Store(ProtoError::not_sent("connection refused")).changed_nothing(),
            "a request that never left changed nothing"
        );
        assert!(
            Error::Store(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            })
            .changed_nothing(),
            "a refusal is a refusal"
        );
        assert!(
            !Error::Store(ProtoError::Closed {
                detail: "reset".to_owned(),
            })
            .changed_nothing(),
            "a connection that died mid-call leaves the write's fate unknown"
        );
        assert!(
            !Error::AmbiguousResult {
                method: Method::RawPut,
                source: Box::new(ProtoError::Closed {
                    detail: "reset".to_owned(),
                }),
            }
            .changed_nothing()
        );
        assert!(
            !Error::UnexpectedResponse {
                expected: Method::RawPut,
                actual: Method::RawGet,
            }
            .changed_nothing(),
            "answering the wrong method proves the store did something, not nothing"
        );
    }

    /// Every error the retry loop can exhaust its budget on is a refusal, so exhausting the
    /// budget must never leave a write in doubt.
    #[test]
    fn a_spent_retry_budget_never_leaves_a_write_in_doubt() {
        for error in [
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(2),
            },
            ProtoError::EpochNotMatch {
                current_regions: vec![],
            },
            ProtoError::ServerIsBusy {
                reason: "l0 stall".to_owned(),
            },
            ProtoError::RegionNotFound { region_id: 1 },
        ] {
            assert!(error.is_retryable(), "{error:?}");
            assert!(
                Error::RetriesExhausted {
                    attempts: 9,
                    source: Box::new(error.clone()),
                }
                .changed_nothing(),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_deadline_inherits_the_doubt_of_whatever_it_interrupted() {
        assert!(
            Error::DeadlineExceeded {
                attempts: 3,
                source: None,
            }
            .changed_nothing()
        );
        assert!(
            Error::DeadlineExceeded {
                attempts: 3,
                source: Some(Box::new(ProtoError::ServerIsBusy {
                    reason: "stall".to_owned(),
                })),
            }
            .changed_nothing()
        );
        assert!(
            !Error::DeadlineExceeded {
                attempts: 3,
                source: Some(Box::new(ProtoError::Closed {
                    detail: "reset".to_owned(),
                })),
            }
            .changed_nothing()
        );
    }

    #[test]
    fn messages_name_what_went_wrong() {
        let error = Error::RetriesExhausted {
            attempts: 8,
            source: Box::new(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            }),
        };
        let message = error.to_string();
        assert!(message.contains('8'), "{message}");
        assert!(message.contains("leader"), "{message}");
    }
}
