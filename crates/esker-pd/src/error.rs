//! What the placement driver can fail with, and the wire error each failure becomes.
//!
//! The split is the same one every crate here makes: a `PdError` says what went wrong *inside*
//! PD, and [`esker_proto::ProtoError`] says what the caller is told. Two of them are the whole
//! reason this enum exists rather than a string — [`PdError::NotBootstrapped`] and
//! [`PdError::ClusterMismatch`] are decisions a caller acts on (wait for someone to bootstrap;
//! stop, you are talking to the wrong cluster), and prose cannot be acted on.

use esker_proto::ProtoError;

/// Anything the placement driver refuses or fails at.
#[derive(Debug, thiserror::Error)]
pub enum PdError {
    /// PD's own database failed.
    #[error("pd storage: {0}")]
    Engine(#[from] esker_engine::Error),

    /// A record on disk would not decode: an unknown format version, a truncated field, a
    /// trailing byte. Corruption is a value, never a panic (`CLAUDE.md` invariants 2 and 9).
    #[error("pd record `{what}` is corrupt: {detail}")]
    Corrupt {
        /// Which record.
        what: &'static str,
        /// What was wrong with it.
        detail: String,
    },

    /// No store has bootstrapped, so there is no cluster to answer about. Distinct from an
    /// empty answer on purpose: "there is no cluster yet" and "no region owns this key" are
    /// different facts and the caller does different things with them.
    #[error("the cluster is not bootstrapped")]
    NotBootstrapped,

    /// The request carries another cluster's id. A misconfiguration — two clusters sharing an
    /// address — and never something to retry into.
    #[error("request is for cluster {actual}, this is cluster {expected}")]
    ClusterMismatch {
        /// The cluster this PD serves.
        expected: u64,
        /// The cluster the request named.
        actual: u64,
    },

    /// A request PD will not serve as asked: a zero count, a batch larger than the format can
    /// hand out in one go.
    #[error("invalid request: {detail}")]
    Invalid {
        /// What was wrong with it.
        detail: String,
    },

    /// A failure that is neither the caller's fault nor a known limitation — a poisoned lock,
    /// an id space that has run out.
    #[error("internal error: {detail}")]
    Internal {
        /// What failed.
        detail: String,
    },
}

impl PdError {
    /// Names a corrupt record.
    pub fn corrupt(what: &'static str, detail: impl Into<String>) -> Self {
        Self::Corrupt {
            what,
            detail: detail.into(),
        }
    }

    /// Names a request PD will not serve.
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid {
            detail: detail.into(),
        }
    }

    /// Names an internal failure.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }
}

impl From<PdError> for ProtoError {
    /// A storage failure and a corrupt record both become [`ProtoError::Internal`]: from the
    /// caller's side they are the same event — PD could not answer, and the request may or may
    /// not have taken effect, which is exactly what `Internal`'s
    /// [`RequestOutcome::Unknown`](esker_proto::RequestOutcome) says.
    fn from(error: PdError) -> Self {
        match error {
            PdError::NotBootstrapped => Self::NotBootstrapped,
            PdError::ClusterMismatch { expected, actual } => {
                Self::ClusterMismatch { expected, actual }
            }
            PdError::Invalid { detail } => Self::InvalidRequest { detail },
            other => Self::Internal {
                detail: other.to_string(),
            },
        }
    }
}

/// What every PD operation returns.
pub type Result<T> = std::result::Result<T, PdError>;

#[cfg(test)]
mod tests {
    use super::PdError;
    use esker_proto::{ProtoError, RequestOutcome};

    /// The two variants that exist so a caller can branch must survive the trip to the wire as
    /// themselves. Collapsing either into `Internal` would make PD's refusals unreadable.
    #[test]
    fn the_two_actionable_failures_stay_typed_on_the_wire() {
        assert!(matches!(
            ProtoError::from(PdError::NotBootstrapped),
            ProtoError::NotBootstrapped
        ));
        assert!(matches!(
            ProtoError::from(PdError::ClusterMismatch {
                expected: 1,
                actual: 2
            }),
            ProtoError::ClusterMismatch {
                expected: 1,
                actual: 2
            }
        ));
    }

    /// A failure PD could not classify must be ambiguous to the caller: PD may have persisted
    /// the effect before failing to say so.
    #[test]
    fn a_storage_failure_is_ambiguous_to_the_caller() {
        let error = ProtoError::from(PdError::internal("poisoned lock"));
        assert_eq!(error.outcome(), RequestOutcome::Unknown);
    }

    /// A refusal is not ambiguous: PD decided not to serve the request, so nothing happened.
    #[test]
    fn a_refusal_is_not_ambiguous() {
        for error in [
            PdError::NotBootstrapped,
            PdError::ClusterMismatch {
                expected: 1,
                actual: 2,
            },
            PdError::invalid("count is zero"),
        ] {
            let wire = ProtoError::from(error);
            assert_eq!(wire.outcome(), RequestOutcome::NotApplied, "{wire:?}");
        }
    }
}
