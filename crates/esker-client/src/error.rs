//! What a client call can fail with, and what the caller is entitled to conclude from it.
//!
//! The distinctions here are the point of the type. A caller has to be able to tell
//! "the store refused and changed nothing" from "nobody knows whether the write landed",
//! because those demand opposite reactions — retry the first, investigate the second. The
//! transaction layer of phase 5 is built on exactly this split
//! (`prompts/05-txn.md`), so it is a typed error and not a message.

use bytes::Bytes;

use crate::wire::{CallError, RawMethod, ServerError, TransportError};

/// A failed client call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The store refused, for a reason this client does not retry: it carries no redirect
    /// hint, or retrying it could not help. **Nothing was written.**
    #[error(transparent)]
    Server(#[from] ServerError),

    /// The store kept refusing with a redirectable error until the retry budget ran out.
    /// **Nothing was written**; the cluster is unhealthy or the caller's budget is too small.
    #[error("gave up after {attempts} attempts: {source}")]
    RetriesExhausted {
        /// Calls made, including the first.
        attempts: u32,
        /// The last refusal.
        source: Box<ServerError>,
    },

    /// The call's deadline passed. Whether anything was written depends on `source`: a
    /// refusal means no, and no source at all means the deadline passed between attempts.
    #[error("deadline passed after {attempts} attempts")]
    DeadlineExceeded {
        /// Calls made before the deadline stopped the loop.
        attempts: u32,
        /// The last failure seen, if the deadline interrupted a retry rather than a wait.
        source: Option<Box<CallError>>,
    },

    /// **The write may or may not have been applied.** The request went out and no answer
    /// came back, so this client will not send it again: under last-write-wins a repeated
    /// write is harmless *only* when the first one provably did not commit, and here it is
    /// not provable. The caller must decide — read the key back, or fail the operation.
    ///
    /// Reads never produce this: re-reading is always safe, so an unanswered read is a plain
    /// [`Error::Transport`].
    #[error("the {method:?} may or may not have been applied: {source}")]
    AmbiguousResult {
        /// The method whose fate is unknown.
        method: RawMethod,
        /// How the answer was lost.
        source: TransportError,
    },

    /// The call could not be made, and provably changed nothing: the connection was refused,
    /// or the answer was unintelligible.
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// No region in the cache covers the key and none could be fetched. Routing is broken,
    /// not the store.
    #[error("no region covers the key")]
    NoRegion {
        /// The key that could not be routed.
        key: Bytes,
    },

    /// The request would not fit in one frame. Refused here rather than at the far end,
    /// where it would look like a connection failure.
    #[error("request of {bytes} bytes exceeds the {limit}-byte frame limit")]
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
        expected: RawMethod,
        /// What came back.
        actual: RawMethod,
    },
}

impl Error {
    /// Whether the caller may assume the database is untouched.
    ///
    /// True only where the client can *prove* it: the store refused, or the request never
    /// left this process, or it never got as far as being built. Everything else answers
    /// false, including two cases that look like client-side failures and are not —
    /// [`Error::UnexpectedResponse`] and a [`TransportError::Protocol`] failure both mean
    /// bytes came back, which means the store processed something. The safe direction to be
    /// wrong in is "it may have happened", so that is the default.
    ///
    /// Callers that must not double-apply — the transaction layer, a `CompareAndSwap` retry
    /// loop — branch on this rather than on the variant.
    #[must_use]
    pub fn changed_nothing(&self) -> bool {
        match self {
            Self::Server(_)
            | Self::RetriesExhausted { .. }
            | Self::NoRegion { .. }
            | Self::RequestTooLarge { .. } => true,
            // An answer came back, so the store acted; what it did is anybody's guess.
            Self::AmbiguousResult { .. } | Self::UnexpectedResponse { .. } => false,
            Self::Transport(error) => error.is_provably_unsent(),
            // A deadline that fired while a request was in flight leaves the same doubt an
            // ambiguous transport failure does.
            Self::DeadlineExceeded { source, .. } => match source.as_deref() {
                None | Some(CallError::Server(_)) => true,
                Some(CallError::Transport(error)) => error.is_provably_unsent(),
            },
        }
    }
}

/// The result of a client call.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::wire::{CallError, RawMethod, ServerError, TransportError};

    #[test]
    fn only_an_unanswered_write_leaves_the_database_in_doubt() {
        assert!(
            Error::Server(ServerError::Other("disk".to_owned())).changed_nothing(),
            "a refusal is a refusal"
        );
        assert!(Error::Transport(TransportError::NotSent("refused".to_owned())).changed_nothing());
        assert!(
            !Error::Transport(TransportError::Protocol("bad crc".to_owned())).changed_nothing(),
            "bytes that could not be parsed are still bytes the store sent back, so it acted"
        );
        assert!(
            !Error::UnexpectedResponse {
                expected: RawMethod::Put,
                actual: RawMethod::Get,
            }
            .changed_nothing(),
            "answering the wrong method proves the store did something, not nothing"
        );
        assert!(!Error::Transport(TransportError::Ambiguous("reset".to_owned())).changed_nothing());
        assert!(
            !Error::AmbiguousResult {
                method: RawMethod::Put,
                source: TransportError::Ambiguous("reset".to_owned()),
            }
            .changed_nothing()
        );
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
                source: Some(Box::new(CallError::Server(ServerError::ServerIsBusy {
                    reason: "stall".to_owned(),
                    backoff_ms: 1,
                }))),
            }
            .changed_nothing()
        );
        assert!(
            !Error::DeadlineExceeded {
                attempts: 3,
                source: Some(Box::new(CallError::Transport(TransportError::Ambiguous(
                    "reset".to_owned()
                )))),
            }
            .changed_nothing()
        );
    }

    #[test]
    fn messages_name_what_went_wrong() {
        let error = Error::RetriesExhausted {
            attempts: 8,
            source: Box::new(ServerError::NotLeader {
                region_id: 1,
                leader_hint: None,
            }),
        };
        let message = error.to_string();
        assert!(message.contains('8'), "{message}");
        assert!(message.contains("leader"), "{message}");
    }
}
