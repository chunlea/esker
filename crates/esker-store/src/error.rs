//! The store's error type, and how an engine failure reaches the wire.
//!
//! Two audiences. [`StoreError`] is for the process that opened the store — the CLI, a test —
//! and keeps the engine's own error intact so a failure to open a database says what was
//! wrong with it. [`engine_to_proto`] is for a client, and answers a narrower question: whose
//! fault was this, and may the request be sent again?
//!
//! That second question is the one to get right. `esker-proto` splits errors by
//! [`RequestOutcome`](esker_proto::RequestOutcome), and **`Unknown` is the safe direction**:
//! reporting an applied write as not-applied invites a duplicate, while reporting a failed one
//! as unknown costs a retry a client did not have to skip. Every engine error whose effect on
//! the log is uncertain is mapped to the uncertain side.

use esker_engine::Error as EngineError;
use esker_proto::ProtoError;

/// What can go wrong in the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The engine failed. Kept whole, because "could not open the database" is only useful
    /// with the engine's own explanation attached.
    #[error("engine: {0}")]
    Engine(#[from] EngineError),

    /// A request could not be served. Already typed for the wire.
    #[error("{0}")]
    Proto(#[from] ProtoError),

    /// The store could not be brought up: a column family the bootstrap needs is missing, a
    /// region is not what the manifest says it is.
    #[error("bootstrap: {0}")]
    Bootstrap(String),

    /// The set of regions this store holds would stop being a set: a second peer of a region it
    /// already hosts, or a range overlapping one it already claims. Refused rather than
    /// reconciled — either means a routing question has already been answered wrongly, and a map
    /// that cannot be right must not serve.
    #[error("regions: {0}")]
    RegionConflict(String),
}

impl From<StoreError> for ProtoError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Engine(engine) => engine_to_proto(&engine),
            StoreError::Proto(proto) => proto,
            StoreError::Bootstrap(detail) | StoreError::RegionConflict(detail) => {
                Self::internal(detail)
            }
        }
    }
}

/// Turns an engine failure into the typed error a client acts on.
///
/// The mapping is about blame and about repeatability, not about wording:
///
/// * [`EngineError::Unsupported`] is a limitation of this version, and the caller can do
///   something about it — use a smaller range, split the request — so it stays its own kind.
/// * [`EngineError::InvalidArgument`] is the caller's mistake.
/// * [`EngineError::Corruption`] is ours, and it is not the caller's to retry.
/// * [`EngineError::GroupCommit`] and [`EngineError::Poisoned`] become
///   [`ProtoError::Internal`], whose outcome is `Unknown`. A group commit fails the whole
///   group, and a poisoned version set means memory and disk may disagree — in neither case
///   can this layer promise the write did not land, so it does not.
/// * [`EngineError::ShuttingDown`] is [`ProtoError::ServerIsBusy`]: retryable, because the
///   client's next attempt belongs somewhere else and a backoff is how it gets there.
#[must_use]
pub fn engine_to_proto(error: &EngineError) -> ProtoError {
    match error {
        EngineError::Unsupported(detail) => ProtoError::Unsupported {
            detail: detail.clone(),
        },
        EngineError::InvalidArgument(detail) => ProtoError::InvalidRequest {
            detail: detail.clone(),
        },
        EngineError::Corruption { context, detail } => ProtoError::Corrupt {
            context: context.clone(),
            detail: detail.clone(),
        },
        EngineError::Io { path, source } => ProtoError::Io {
            detail: format!("{}: {source}", path.display()),
        },
        EngineError::ShuttingDown => ProtoError::ServerIsBusy {
            reason: "the store is shutting down".to_owned(),
        },
        // A missing file the engine expected is not something a caller did or can fix.
        EngineError::NotFound(detail) => ProtoError::internal(format!("not found: {detail}")),
        EngineError::GroupCommit(detail) => {
            ProtoError::internal(format!("group commit failed: {detail}"))
        }
        EngineError::Poisoned(detail) => {
            ProtoError::internal(format!("the database must be reopened: {detail}"))
        }
    }
}

/// The store's result type.
pub type Result<T> = std::result::Result<T, StoreError>;

#[cfg(test)]
mod tests {
    use super::{StoreError, engine_to_proto};
    use esker_engine::Error as EngineError;
    use esker_proto::{ProtoError, RequestOutcome};

    fn one_of_each() -> Vec<EngineError> {
        vec![
            EngineError::Unsupported("DeleteRange across a boundary".to_owned()),
            EngineError::InvalidArgument("inverted range".to_owned()),
            EngineError::corruption("000001.sst", "checksum mismatch"),
            EngineError::io(
                "/db/CURRENT",
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ),
            EngineError::ShuttingDown,
            EngineError::NotFound("MANIFEST-000004".to_owned()),
            EngineError::GroupCommit("the leader's append failed".to_owned()),
            EngineError::Poisoned("a manifest sync failed".to_owned()),
        ]
    }

    /// The property the client's write path depends on: an engine failure whose effect on the
    /// log is uncertain must not claim the write did not happen.
    #[test]
    fn uncertain_engine_failures_reach_the_client_as_uncertain() {
        for engine in one_of_each() {
            let proto = engine_to_proto(&engine);
            let expected = match engine {
                // The write was refused before it could reach the log, or was never a write.
                EngineError::Unsupported(_)
                | EngineError::InvalidArgument(_)
                | EngineError::ShuttingDown => RequestOutcome::NotApplied,
                // Everything else happened at or after the log, and this layer cannot say
                // which side of it.
                _ => RequestOutcome::Unknown,
            };
            assert_eq!(proto.outcome(), expected, "{engine:?} -> {proto:?}");
        }
    }

    /// A documented limitation must stay distinguishable from a caller's mistake, or the
    /// caller cannot tell "never going to work" from "not like that".
    #[test]
    fn a_limitation_is_not_a_caller_error() {
        let unsupported = engine_to_proto(&EngineError::Unsupported("range too wide".to_owned()));
        assert!(matches!(unsupported, ProtoError::Unsupported { .. }));

        let invalid = engine_to_proto(&EngineError::InvalidArgument("no such cf".to_owned()));
        assert!(matches!(invalid, ProtoError::InvalidRequest { .. }));
    }

    #[test]
    fn a_shutting_down_store_asks_the_client_to_go_elsewhere() {
        let error = engine_to_proto(&EngineError::ShuttingDown);
        assert!(error.is_retryable(), "{error:?}");
    }

    /// The engine's own explanation has to survive the trip, or a corrupt file is reported as
    /// "something went wrong".
    #[test]
    fn corruption_keeps_its_context() {
        let error = engine_to_proto(&EngineError::corruption("000007.sst", "bad magic"));
        match error {
            ProtoError::Corrupt { context, detail } => {
                assert_eq!(context, "000007.sst");
                assert_eq!(detail, "bad magic");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_store_error_converts_to_the_same_wire_error() {
        let store: StoreError = EngineError::ShuttingDown.into();
        assert!(ProtoError::from(store).is_retryable());

        let passed_through = StoreError::Proto(ProtoError::RegionNotFound { region_id: 3 });
        assert_eq!(
            ProtoError::from(passed_through),
            ProtoError::RegionNotFound { region_id: 3 }
        );
    }
}
