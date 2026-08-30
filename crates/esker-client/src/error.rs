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
            Self::NoRegion { .. } | Self::RequestTooLarge { .. } => true,
            // An answer came back, so the store acted; what it did is anybody's guess.
            Self::UnexpectedResponse { .. } => false,
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
