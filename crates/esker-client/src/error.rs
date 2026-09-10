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

/// **What a transaction was doing when a lock refused to clear**, so that a `40001` says which.
///
/// [`Error::LockNotCleared`] has three callers and they are not the same condition. Naming the
/// caller in the error is what lets an operator — or a Rails pass — tell a reader blocked behind
/// somebody's write from a row two writers want, without reading this crate's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// A read: `get`, `scan`, `latest_commit`. It holds no lock and wanted none, so what it met
    /// is somebody else's uncommitted write standing in front of a value it needed.
    Read,
    /// A `SERIALIZABLE` read set, asserting that a range it read has not moved
    /// (ADR 0104 §1). It acquires nothing either, and it may not wound.
    ReadSet,
    /// A transaction **acquiring** a key it means to write — the only one of the three that
    /// wanted the lock.
    Acquire,
}

impl std::fmt::Display for Waiting {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Read => "a read",
            Self::ReadSet => "a read-set range check",
            Self::Acquire => "an acquiring write",
        })
    }
}

/// A failed client call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The store refused, for a reason this client does not retry: it carries no redirect
    /// hint, or retrying it could not help.
    #[error(transparent)]
    Store(#[from] ProtoError),

    /// The call kept failing in a way worth trying again until the retry budget ran out. The
    /// cluster is unhealthy, or the caller's budget is too small for how long it takes to
    /// elect a leader.
    ///
    /// Nothing was written either way, but for two different reasons: a redirectable error is
    /// a refusal, and the only other thing that is retried is a **read** whose answer was lost
    /// ([`crate::retry::may_ask_again`]). `source` still decides
    /// [`Error::changed_nothing`], which is the conservative answer rather than the exact one
    /// — a read that exhausts on lost answers changed nothing, and this reports that it may
    /// have. Callers that need the exact answer have the method and this type does not.
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
    #[error("a lock held by the transaction at {start_ts} did not clear in time for {waiting}")]
    LockNotCleared {
        /// The transaction holding it.
        start_ts: u64,
        /// **What this transaction was doing when it gave up**
        /// ([ADR 0104](../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §4).
        ///
        /// Three different calls reach this error and a client cannot tell them apart from the
        /// message: a read that holds nothing, a read set asserting a range did not move, and a
        /// transaction acquiring a key it means to write. They want different answers from an
        /// operator — the first is a reader blocked behind somebody's write, the last is
        /// contention on a row — and run 114 spent a whole pass unable to say which one had
        /// raised the `40001` it reported.
        waiting: Waiting,
        /// **Which key was locked.** Carried for the same reason [`Error::TxnConflict`] carries
        /// its own: the caller above may need to tell one blocked key from another, and it
        /// cannot if the error names only the transaction.
        ///
        /// Every construction site has it — a `LockInfo` carries the key it describes — so
        /// unlike `TxnConflict`'s this is never `None`. It reaches a client as the key in
        /// `40001`'s message, which is the difference between "a lock did not clear" and
        /// knowing that what was waited on was the catalog's version counter.
        key: Bytes,
    },

    /// A historical read named a timestamp the collector has already passed
    /// ([ADR 0021](../../docs/adr/0021-time-machine.md) decision 1).
    ///
    /// A refusal rather than a clamp, and rather than an approximate answer: below the
    /// safepoint some versions are gone and some are not, so the database *can* answer and the
    /// answer is a state that never existed (`docs/txn-spec.md` §7). The error carries the
    /// floor because the useful reply to "show me 14:00" is how far back the caller may
    /// actually ask.
    #[error("a read at {requested} is below the safepoint {floor}; history that old is collected")]
    SnapshotTooOld {
        /// The timestamp the caller asked to read at.
        requested: u64,
        /// The oldest timestamp that can still be answered — the safepoint now in force.
        floor: u64,
    },

    /// A read named a timestamp that has not happened yet (ADR 0021 decision 1).
    ///
    /// A read there would see a prefix of that instant and call it complete: transactions that
    /// will commit below it have not committed yet.
    #[error("a read at {requested} is above the oracle's high-water mark {now}")]
    SnapshotInTheFuture {
        /// The timestamp the caller asked to read at.
        requested: u64,
        /// The newest timestamp the oracle has handed out.
        now: u64,
    },

    /// A write was attempted on a transaction opened at a past timestamp.
    ///
    /// Read-only is not a policy here, it is the only safe reading: committing at a fresh
    /// `commit_ts` against an old snapshot is a lost update that Percolator's conflict check
    /// **cannot** catch, because the conflicting writer committed after the snapshot and before
    /// the write — the one window snapshot isolation does not close (ADR 0021 decision 1).
    /// PostgreSQL spells the same refusal `25006 read_only_sql_transaction`.
    #[error("the transaction at {start_ts} reads the past and cannot write (key {key:?})")]
    ReadOnlyTransaction {
        /// The historical snapshot it was opened at.
        start_ts: u64,
        /// The first key a write was attempted on.
        key: Bytes,
    },

    /// A named snapshot was read back and is not there
    /// ([ADR 0021](../../docs/adr/0021-time-machine.md) decision 3).
    ///
    /// PostgreSQL's `42704 snapshot "..." does not exist`, and it means what it says: the name
    /// was never exported, or whatever holds the names has lost it. A checkpoint is a claim,
    /// and this is the claim failing before anything is read at it.
    #[error("no snapshot is named {name:?}")]
    NoSuchSnapshot {
        /// The name that was looked up.
        name: Bytes,
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
            // The three time-machine refusals are decided *before* anything is sent, from a
            // timestamp and a safepoint, so there is nothing they could have changed.
            Self::NoRegion { .. }
            | Self::RequestTooLarge { .. }
            | Self::TxnConflict { .. }
            | Self::LockNotCleared { .. }
            | Self::SnapshotTooOld { .. }
            | Self::SnapshotInTheFuture { .. }
            | Self::ReadOnlyTransaction { .. }
            | Self::NoSuchSnapshot { .. } => true,
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
