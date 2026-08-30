//! What can go wrong inside a transaction, as one enum.
//!
//! Two kinds of thing are in here and the split matters. A [`TxnError::Corrupt`] is a byte
//! string that cannot be a record — damaged storage, or something else writing into our column
//! families — and no retry helps. The protocol refusals ([`TxnError::WriteConflict`],
//! [`TxnError::KeyIsLocked`], …) are the transaction losing a race, and every one of them is a
//! thing the client knows how to do something about.
//!
//! Nothing here panics on input from disk or from a socket (`CLAUDE.md` invariant 9); a
//! malformed record is this type, not an `unwrap`.

use bytes::Bytes;
use thiserror::Error;

/// The result of everything in this crate.
pub type Result<T> = std::result::Result<T, TxnError>;

/// Why a transactional operation could not be carried out.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TxnError {
    /// A record's bytes are not a record. Damaged storage, or a foreign writer in one of the
    /// Percolator column families.
    #[error("corrupt {what}: {detail}")]
    Corrupt {
        /// Which record failed to decode — `"lock"`, `"write"`, or a key.
        what: &'static str,
        /// What was wrong with it.
        detail: String,
    },

    /// A commit landed after this transaction's snapshot. First-committer-wins: this one loses
    /// and must start again with a fresh `start_ts` (`docs/txn-spec.md` §5.2).
    #[error(
        "write conflict on a key committed at {commit_ts}, after this transaction's \
         snapshot at {start_ts}"
    )]
    WriteConflict {
        /// The snapshot that lost.
        start_ts: u64,
        /// The commit that beat it.
        commit_ts: u64,
    },

    /// Another transaction holds the key. Not an error the caller should surface: the client
    /// resolves the lock (`docs/txn-spec.md` §5.5) and tries again.
    #[error("key is locked by the transaction at {start_ts}")]
    KeyIsLocked {
        /// The lock, encoded, for `ProtoError::Locked`'s opaque payload.
        lock_info: Bytes,
        /// The transaction holding it, lifted out so a caller can decide without decoding.
        start_ts: u64,
    },

    /// A commit or rollback found no lock of its own and no record saying what happened to it.
    /// Someone else resolved this transaction, or the lock was lost.
    #[error("no lock for the transaction at {start_ts}, and no record of its fate")]
    TxnLockNotFound {
        /// The transaction that has nothing to commit.
        start_ts: u64,
    },

    /// The transaction was rolled back — by a resolver that found its lock expired, or by
    /// itself — and cannot now be committed.
    #[error("the transaction at {start_ts} was already rolled back")]
    AlreadyRolledBack {
        /// The transaction.
        start_ts: u64,
    },

    /// A rollback was asked for a transaction that has already committed. The two answers are
    /// contradictory and the committed one is the one every reader has already seen.
    #[error("the transaction at {start_ts} committed at {commit_ts} and cannot be rolled back")]
    AlreadyCommitted {
        /// The transaction.
        start_ts: u64,
        /// When it committed.
        commit_ts: u64,
    },

    /// A `write` record says `Put` with no inline value, and the `default` column family has
    /// nothing at its `start_ts`. The value is gone.
    ///
    /// Reported rather than answered as "the key does not exist", because those two mean
    /// opposite things and confusing them loses a write silently.
    #[error("the value for the transaction at {start_ts} is missing from the default CF")]
    MissingValue {
        /// The transaction whose value should be there.
        start_ts: u64,
    },

    /// A caller broke one of this crate's ordering rules — committing a secondary under a
    /// token from another transaction, or prewriting a key against a primary it does not name.
    ///
    /// Separate from the protocol refusals because it is a bug in the caller rather than a
    /// race with another transaction, and the only fix is in the calling code.
    #[error("protocol misuse: {0}")]
    Misuse(&'static str),
}

impl TxnError {
    /// A corruption error naming the record kind and what was wrong.
    #[must_use]
    pub fn corrupt(what: &'static str, detail: impl Into<String>) -> Self {
        Self::Corrupt {
            what,
            detail: detail.into(),
        }
    }

    /// Whether retrying the *same* transaction could succeed.
    ///
    /// A lock conflict can: resolve the lock and go again. A write conflict cannot — the
    /// snapshot is stale and only a new `start_ts` helps — and neither can corruption or
    /// misuse. The distinction is the client's whole retry rule
    /// (`docs/DESIGN.md` §10), so it is answered here rather than pattern-matched at each
    /// call site.
    #[must_use]
    pub fn is_resolvable(&self) -> bool {
        matches!(self, Self::KeyIsLocked { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::TxnError;

    /// A lock conflict is the one refusal the client acts on and retries. Everything else
    /// either needs a new snapshot or needs a human.
    #[test]
    fn only_a_lock_conflict_is_resolvable() {
        assert!(
            TxnError::KeyIsLocked {
                lock_info: bytes::Bytes::new(),
                start_ts: 7,
            }
            .is_resolvable()
        );
        for error in [
            TxnError::WriteConflict {
                start_ts: 1,
                commit_ts: 2,
            },
            TxnError::TxnLockNotFound { start_ts: 1 },
            TxnError::AlreadyRolledBack { start_ts: 1 },
            TxnError::MissingValue { start_ts: 1 },
            TxnError::corrupt("lock", "nonsense"),
            TxnError::Misuse("wrong token"),
        ] {
            assert!(!error.is_resolvable(), "{error}");
        }
    }
}
