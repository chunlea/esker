//! The typed error every layer of the protocol returns.
//!
//! `docs/DESIGN.md` §9: "every error is a typed enum with redirect hints". A string error is a
//! build failure of this phase, because the whole point of the enum is that a client can act
//! on it without parsing prose — refresh a region cache on [`ProtoError::EpochNotMatch`],
//! follow the hint on [`ProtoError::NotLeader`], back off on
//! [`ProtoError::ServerIsBusy`] — and each of those is a different decision.
//!
//! Every variant has a **code** that is part of the wire format, and every variant encodes and
//! decodes, including the ones a server never sends. [`ProtoError::Corrupt`],
//! [`ProtoError::Io`] and [`ProtoError::Closed`] are normally raised locally by the transport,
//! but a round trip exercised for only some variants is a golden test with holes in it.
//!
//! [`ProtoError::is_retryable`] lives here rather than in `esker-client` so that the server
//! and the client cannot come to disagree about which errors a retry loop may swallow, and
//! [`ProtoError::outcome`] answers the harder question next to it: whether a failed request
//! may have taken effect, which is what decides if a *write* may be sent again.

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::region::Region;

/// Wire codes for [`ProtoError`] (*fixed*). One `u16` at the head of an `Error` frame's body.
///
/// A code is never reused for a different meaning, and a code this build does not know is a
/// decode error rather than a generic failure: an error whose meaning was guessed is worse
/// than one that could not be read.
pub mod code {
    /// [`super::ProtoError::NotLeader`].
    pub const NOT_LEADER: u16 = 1;
    /// [`super::ProtoError::EpochNotMatch`].
    pub const EPOCH_NOT_MATCH: u16 = 2;
    /// [`super::ProtoError::KeyNotInRegion`].
    pub const KEY_NOT_IN_REGION: u16 = 3;
    /// [`super::ProtoError::ServerIsBusy`].
    pub const SERVER_IS_BUSY: u16 = 4;
    /// [`super::ProtoError::Locked`] — reserved for phase 5.
    pub const LOCKED: u16 = 5;
    /// [`super::ProtoError::RegionNotFound`].
    pub const REGION_NOT_FOUND: u16 = 6;
    /// [`super::ProtoError::WireVersion`].
    pub const WIRE_VERSION: u16 = 7;
    /// [`super::ProtoError::InvalidRequest`].
    pub const INVALID_REQUEST: u16 = 8;
    /// [`super::ProtoError::Unsupported`].
    pub const UNSUPPORTED: u16 = 9;
    /// [`super::ProtoError::Corrupt`].
    pub const CORRUPT: u16 = 10;
    /// [`super::ProtoError::Io`].
    pub const IO: u16 = 11;
    /// [`super::ProtoError::Closed`].
    pub const CLOSED: u16 = 12;
    /// [`super::ProtoError::DuplicateRequestId`].
    pub const DUPLICATE_REQUEST_ID: u16 = 13;
    /// [`super::ProtoError::Internal`].
    pub const INTERNAL: u16 = 14;
    /// [`super::ProtoError::NotSent`].
    pub const NOT_SENT: u16 = 15;
    /// [`super::ProtoError::Timeout`].
    pub const TIMEOUT: u16 = 16;

    /// Every code this version defines, for the tests that sweep them.
    pub const ALL: [u16; 16] = [
        NOT_LEADER,
        EPOCH_NOT_MATCH,
        KEY_NOT_IN_REGION,
        SERVER_IS_BUSY,
        LOCKED,
        REGION_NOT_FOUND,
        WIRE_VERSION,
        INVALID_REQUEST,
        UNSUPPORTED,
        CORRUPT,
        IO,
        CLOSED,
        DUPLICATE_REQUEST_ID,
        INTERNAL,
        NOT_SENT,
        TIMEOUT,
    ];
}

/// What is known about whether a failed request took effect.
///
/// The distinction exists because a retry is only free when the first attempt provably did
/// nothing. Under last-write-wins a repeated `Put` is harmless, but a repeated `Prewrite` is
/// not (`docs/DESIGN.md` §8), so `esker-txn` will need this in phase 5 and the client needs it
/// now to decide whether a write may be sent again.
///
/// When the transport cannot tell, it must say [`RequestOutcome::Unknown`]. That is the safe
/// direction to be wrong in: it costs a failed call, where the other direction costs a
/// silently duplicated write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RequestOutcome {
    /// The request provably did not take effect: the peer answered with a refusal, or the
    /// request never reached the wire. Safe to send again.
    NotApplied,
    /// The request may or may not have taken effect. It went out and no usable answer came
    /// back. **Not** safe to send again unless the operation is idempotent.
    Unknown,
}

/// Everything the protocol can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    /// This peer is not the leader of the region. The hint is the peer it believes is, when it
    /// has an opinion; a client with no hint asks the placement driver.
    #[error("peer is not the leader of region {region_id}")]
    NotLeader {
        /// The region the request named.
        region_id: u64,
        /// Which peer to try instead, if this one knows.
        leader_hint: Option<u64>,
    },

    /// The region's epoch has moved on: it split, merged, or changed membership. The regions
    /// that now cover the range come back with the error so that one round trip refreshes the
    /// client's cache (`CLAUDE.md` invariant 5).
    #[error("region epoch does not match")]
    EpochNotMatch {
        /// The regions that now cover what the request asked for.
        current_regions: Vec<Region>,
    },

    /// The key is outside the range this region owns. Distinct from
    /// [`ProtoError::EpochNotMatch`]: the epoch was right, the key was not.
    #[error("key is not in region {region_id}")]
    KeyNotInRegion {
        /// The key that fell outside.
        key: Bytes,
        /// The region that was asked.
        region_id: u64,
        /// Inclusive start of what it owns.
        start_key: Bytes,
        /// Exclusive end of what it owns; empty means "to the end of the key space".
        end_key: Bytes,
    },

    /// The server is at a limit — in-flight requests, a write stall — and shed this request
    /// rather than queueing it. Retryable after a backoff; queueing it instead is how a
    /// server turns a slow disk into an out-of-memory kill.
    #[error("server is busy: {reason}")]
    ServerIsBusy {
        /// Which limit was hit.
        reason: String,
    },

    /// A transactional lock is in the way. Reserved for phase 5; the payload is opaque here
    /// because `esker-proto` must not learn Percolator's lock layout before `esker-txn`
    /// defines it (`docs/DESIGN.md` §8).
    // TODO(phase-5): replace the opaque bytes with the typed LockInfo of DESIGN §8.
    #[error("key is locked")]
    Locked {
        /// The encoded lock, to be interpreted by `esker-txn`.
        lock_info: Bytes,
    },

    /// This store does not host the region at all.
    #[error("region {region_id} is not on this store")]
    RegionNotFound {
        /// The region the request named.
        region_id: u64,
    },

    /// The peer speaks another version of the protocol. Negotiated once, on connect: there is
    /// no downgrade path, because a protocol that quietly agrees to a lower version is a
    /// protocol whose behaviour nobody can state (`docs/DESIGN.md` §9).
    #[error("wire version mismatch: this build speaks {expected}, the peer speaks {actual}")]
    WireVersion {
        /// The version this build speaks.
        expected: u32,
        /// The version the peer offered.
        actual: u32,
    },

    /// The request could not be decoded, named an unknown method, or was not allowed here.
    #[error("invalid request: {detail}")]
    InvalidRequest {
        /// What was wrong with it.
        detail: String,
    },

    /// A documented limitation of this version, kept distinct from a caller error so that it
    /// can be found and removed when the limitation goes. `DeleteRange` over a large range is
    /// the phase-2 example (ADR 0006).
    #[error("unsupported: {detail}")]
    Unsupported {
        /// What is not supported, and what to do instead.
        detail: String,
    },

    /// Bytes on the wire were not what the framing says they must be: a bad checksum, an
    /// impossible length, an unknown frame kind. Usually raised locally by the frame reader
    /// (`CLAUDE.md` invariant 2, at the network layer).
    #[error("corruption in {context}: {detail}")]
    Corrupt {
        /// Where it was found.
        context: String,
        /// What was wrong.
        detail: String,
    },

    /// The socket failed. Raised locally; the text is `io::Error`'s, because there is no
    /// structure in it worth inventing.
    #[error("io error: {detail}")]
    Io {
        /// What the operating system said.
        detail: String,
    },

    /// The request went out and the connection went away before an answer came back. Its
    /// outcome is [`RequestOutcome::Unknown`]: the store may have applied it and died before
    /// replying. Never retried automatically for a write.
    #[error("connection closed: {detail}")]
    Closed {
        /// Why it closed.
        detail: String,
    },

    /// The request provably never reached the wire — the connection was refused, or the
    /// request was rejected before a byte of its frame went out. Its outcome is
    /// [`RequestOutcome::NotApplied`], so a caller may send it again, to this peer or another,
    /// without risking a duplicate.
    ///
    /// This is the counterpart of [`ProtoError::Closed`], and the pair is the whole point:
    /// collapsing them would leave a client unable to tell a write it can safely repeat from
    /// one it cannot.
    #[error("request not sent: {detail}")]
    NotSent {
        /// Why it never left.
        detail: String,
    },

    /// The deadline passed before the peer answered. Its outcome is
    /// [`RequestOutcome::Unknown`]: giving up on an answer says nothing about whether the peer
    /// applied the request, so a slow store and a dead one look the same from here.
    #[error("timed out: {detail}")]
    Timeout {
        /// What was being waited for.
        detail: String,
    },

    /// A request id that is already in flight on this connection. Request ids are assigned by
    /// the client, so a duplicate is the client's bug; replacing the waiter would leave the
    /// first caller waiting for a response that can never arrive.
    #[error("request id {request_id} is already in flight")]
    DuplicateRequestId {
        /// The id that was reused.
        request_id: u64,
    },

    /// The server failed at something that is neither the caller's fault nor a known
    /// limitation. The detail is for a log, not for a branch.
    #[error("internal error: {detail}")]
    Internal {
        /// What failed.
        detail: String,
    },
}

impl ProtoError {
    /// The wire code for this variant.
    #[must_use]
    pub fn code(&self) -> u16 {
        match self {
            Self::NotLeader { .. } => code::NOT_LEADER,
            Self::EpochNotMatch { .. } => code::EPOCH_NOT_MATCH,
            Self::KeyNotInRegion { .. } => code::KEY_NOT_IN_REGION,
            Self::ServerIsBusy { .. } => code::SERVER_IS_BUSY,
            Self::Locked { .. } => code::LOCKED,
            Self::RegionNotFound { .. } => code::REGION_NOT_FOUND,
            Self::WireVersion { .. } => code::WIRE_VERSION,
            Self::InvalidRequest { .. } => code::INVALID_REQUEST,
            Self::Unsupported { .. } => code::UNSUPPORTED,
            Self::Corrupt { .. } => code::CORRUPT,
            Self::Io { .. } => code::IO,
            Self::Closed { .. } => code::CLOSED,
            Self::DuplicateRequestId { .. } => code::DUPLICATE_REQUEST_ID,
            Self::Internal { .. } => code::INTERNAL,
            Self::NotSent { .. } => code::NOT_SENT,
            Self::Timeout { .. } => code::TIMEOUT,
        }
    }

    /// Whether the request may have taken effect. See [`RequestOutcome`].
    ///
    /// Every error the *peer* sent is [`RequestOutcome::NotApplied`], because sending it is
    /// how the peer says it decided not to serve the request. The unknown ones are the
    /// failures that happened around the answer rather than in it: the connection died, the
    /// socket failed, the response was unreadable, or the server admitted an internal failure
    /// that may have been raised after a write went to the log.
    #[must_use]
    pub fn outcome(&self) -> RequestOutcome {
        match self {
            Self::NotLeader { .. }
            | Self::EpochNotMatch { .. }
            | Self::KeyNotInRegion { .. }
            | Self::ServerIsBusy { .. }
            | Self::Locked { .. }
            | Self::RegionNotFound { .. }
            | Self::WireVersion { .. }
            | Self::InvalidRequest { .. }
            | Self::Unsupported { .. }
            | Self::DuplicateRequestId { .. }
            | Self::NotSent { .. } => RequestOutcome::NotApplied,
            Self::Corrupt { .. }
            | Self::Io { .. }
            | Self::Closed { .. }
            | Self::Timeout { .. }
            | Self::Internal { .. } => RequestOutcome::Unknown,
        }
    }

    /// Shorthand for `self.outcome() == RequestOutcome::Unknown`.
    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        self.outcome() == RequestOutcome::Unknown
    }

    /// Names a request that provably never left this process.
    pub fn not_sent(detail: impl Into<String>) -> Self {
        Self::NotSent {
            detail: detail.into(),
        }
    }

    /// Whether a client may retry the request after a backoff.
    ///
    /// True only for the errors that carry a redirect hint or a "try again later": the request
    /// demonstrably did not take effect, and something about where or when to send it has
    /// changed. Everything else — a decode failure, an unsupported operation, a closed
    /// connection whose request may have been applied — is returned to the caller.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::NotLeader { .. }
                | Self::EpochNotMatch { .. }
                | Self::ServerIsBusy { .. }
                | Self::RegionNotFound { .. }
        )
    }

    /// Names an internal failure, for the many places that have an unexpected `Err` and no
    /// structure to add to it.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// Names corrupt bytes found in `context`.
    pub fn corrupt(context: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Corrupt {
            context: context.into(),
            detail: detail.into(),
        }
    }

    /// Names an invalid request.
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidRequest {
            detail: detail.into(),
        }
    }

    /// Encodes the body of an `Error` frame: `code:u16 ++ fields`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.put_u16(self.code());
        match self {
            Self::NotLeader {
                region_id,
                leader_hint,
            } => {
                out.put_varint(*region_id);
                match leader_hint {
                    Some(peer) => {
                        out.put_bool(true);
                        out.put_varint(*peer);
                    }
                    None => out.put_bool(false),
                }
            }
            Self::EpochNotMatch { current_regions } => {
                out.put_varint(current_regions.len() as u64);
                for region in current_regions {
                    region.encode(&mut out);
                }
            }
            Self::KeyNotInRegion {
                key,
                region_id,
                start_key,
                end_key,
            } => {
                out.put_bytes(key);
                out.put_varint(*region_id);
                out.put_bytes(start_key);
                out.put_bytes(end_key);
            }
            Self::ServerIsBusy { reason } => out.put_str(reason),
            Self::Locked { lock_info } => out.put_bytes(lock_info),
            Self::RegionNotFound { region_id } => out.put_varint(*region_id),
            Self::WireVersion { expected, actual } => {
                out.put_varint(u64::from(*expected));
                out.put_varint(u64::from(*actual));
            }
            Self::InvalidRequest { detail }
            | Self::Unsupported { detail }
            | Self::Io { detail }
            | Self::Closed { detail }
            | Self::NotSent { detail }
            | Self::Timeout { detail }
            | Self::Internal { detail } => out.put_str(detail),
            Self::Corrupt { context, detail } => {
                out.put_str(context);
                out.put_str(detail);
            }
            Self::DuplicateRequestId { request_id } => out.put_varint(*request_id),
        }
        out.finish()
    }

    /// Decodes the body of an `Error` frame. Trailing bytes and unknown codes are errors.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(body);
        let code = input.get_u16("error code")?;
        let error = match code {
            code::NOT_LEADER => Self::NotLeader {
                region_id: input.get_varint("region_id")?,
                leader_hint: input
                    .get_bool("leader_hint")?
                    .then(|| input.get_varint("leader_hint"))
                    .transpose()?,
            },
            code::EPOCH_NOT_MATCH => {
                let count = input.get_count("current_regions")?;
                let mut current_regions = Vec::with_capacity(count);
                for _ in 0..count {
                    current_regions.push(Region::decode(&mut input)?);
                }
                Self::EpochNotMatch { current_regions }
            }
            code::KEY_NOT_IN_REGION => Self::KeyNotInRegion {
                key: Bytes::copy_from_slice(input.get_bytes("key")?),
                region_id: input.get_varint("region_id")?,
                start_key: Bytes::copy_from_slice(input.get_bytes("start_key")?),
                end_key: Bytes::copy_from_slice(input.get_bytes("end_key")?),
            },
            code::SERVER_IS_BUSY => Self::ServerIsBusy {
                reason: input.get_str("reason")?.to_owned(),
            },
            code::LOCKED => Self::Locked {
                lock_info: Bytes::copy_from_slice(input.get_bytes("lock_info")?),
            },
            code::REGION_NOT_FOUND => Self::RegionNotFound {
                region_id: input.get_varint("region_id")?,
            },
            code::WIRE_VERSION => Self::WireVersion {
                expected: input.get_varint_u32("expected")?,
                actual: input.get_varint_u32("actual")?,
            },
            code::INVALID_REQUEST => Self::InvalidRequest {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::UNSUPPORTED => Self::Unsupported {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::CORRUPT => Self::Corrupt {
                context: input.get_str("context")?.to_owned(),
                detail: input.get_str("detail")?.to_owned(),
            },
            code::IO => Self::Io {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::CLOSED => Self::Closed {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::DUPLICATE_REQUEST_ID => Self::DuplicateRequestId {
                request_id: input.get_varint("request_id")?,
            },
            code::INTERNAL => Self::Internal {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::NOT_SENT => Self::NotSent {
                detail: input.get_str("detail")?.to_owned(),
            },
            code::TIMEOUT => Self::Timeout {
                detail: input.get_str("detail")?.to_owned(),
            },
            other => {
                return Err(DecodeError::UnknownTag {
                    what: "error code",
                    tag: u64::from(other),
                });
            }
        };
        input.finish()?;
        Ok(error)
    }
}

impl From<DecodeError> for ProtoError {
    /// A body that would not decode is an invalid request. The frame layer, which is reading
    /// bytes whose checksum already passed, converts to [`ProtoError::Corrupt`] by hand
    /// instead — there the bytes are not the caller's fault.
    fn from(error: DecodeError) -> Self {
        Self::InvalidRequest {
            detail: error.to_string(),
        }
    }
}

impl From<std::io::Error> for ProtoError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            detail: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProtoError, RequestOutcome, code};
    use crate::region::{Epoch, Peer, PeerRole, Region};
    use bytes::Bytes;

    fn one_of_each() -> Vec<ProtoError> {
        vec![
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(7),
            },
            ProtoError::NotLeader {
                region_id: u64::MAX,
                leader_hint: None,
            },
            ProtoError::EpochNotMatch {
                current_regions: vec![
                    Region {
                        id: 1,
                        start_key: Bytes::new(),
                        end_key: Bytes::from_static(b"m"),
                        peers: vec![Peer {
                            store_id: 1,
                            peer_id: 2,
                            role: PeerRole::Voter,
                        }],
                        epoch: Epoch {
                            conf_ver: 1,
                            version: 2,
                        },
                    },
                    Region {
                        id: 2,
                        start_key: Bytes::from_static(b"m"),
                        end_key: Bytes::new(),
                        peers: Vec::new(),
                        epoch: Epoch {
                            conf_ver: 1,
                            version: 2,
                        },
                    },
                ],
            },
            ProtoError::EpochNotMatch {
                current_regions: Vec::new(),
            },
            ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b"z"),
                region_id: 3,
                start_key: Bytes::from_static(b"a"),
                end_key: Bytes::from_static(b"m"),
            },
            ProtoError::ServerIsBusy {
                reason: "write stall".to_owned(),
            },
            ProtoError::Locked {
                lock_info: Bytes::from_static(&[1, 2, 3]),
            },
            ProtoError::RegionNotFound { region_id: 9 },
            ProtoError::WireVersion {
                expected: 1,
                actual: u32::MAX,
            },
            ProtoError::InvalidRequest {
                detail: "unknown method 0x0999".to_owned(),
            },
            ProtoError::Unsupported {
                detail: "DeleteRange over 10000 keys".to_owned(),
            },
            ProtoError::Corrupt {
                context: "frame".to_owned(),
                detail: "checksum mismatch".to_owned(),
            },
            ProtoError::Io {
                detail: "broken pipe".to_owned(),
            },
            ProtoError::Closed {
                detail: "peer went away".to_owned(),
            },
            ProtoError::DuplicateRequestId { request_id: 42 },
            ProtoError::Internal {
                detail: "poisoned lock".to_owned(),
            },
            ProtoError::NotSent {
                detail: "connection refused".to_owned(),
            },
            ProtoError::Timeout {
                detail: "no answer in 30s".to_owned(),
            },
        ]
    }

    #[test]
    fn every_variant_round_trips() {
        for error in one_of_each() {
            let bytes = error.encode();
            let back = ProtoError::decode(&bytes)
                .unwrap_or_else(|failure| panic!("{error:?} did not decode: {failure}"));
            assert_eq!(back, error);
        }
    }

    /// Codes are format. Two variants sharing one, or a code changing meaning, breaks every
    /// peer built before the change.
    #[test]
    fn codes_are_distinct_and_cover_every_variant() {
        let unique: std::collections::BTreeSet<u16> = code::ALL.into_iter().collect();
        assert_eq!(unique.len(), code::ALL.len(), "two error codes collide");
        assert!(!unique.contains(&0), "zero is reserved");

        let used: std::collections::BTreeSet<u16> =
            one_of_each().iter().map(ProtoError::code).collect();
        assert_eq!(used, unique, "a code has no variant, or a variant no code");
    }

    #[test]
    fn an_unknown_code_is_an_error_not_a_guess() {
        let bytes = 0x7FFFu16.to_le_bytes();
        assert!(ProtoError::decode(&bytes).is_err());
    }

    #[test]
    fn trailing_bytes_after_an_error_are_refused() {
        let mut bytes = ProtoError::RegionNotFound { region_id: 1 }.encode();
        bytes.push(0);
        assert!(ProtoError::decode(&bytes).is_err());
    }

    /// Truncating a well-formed error at every offset must produce an error, never a panic
    /// and never a half-decoded value (`CLAUDE.md` invariant 9).
    #[test]
    fn truncation_never_panics() {
        for error in one_of_each() {
            let bytes = error.encode();
            for cut in 0..bytes.len() {
                assert!(
                    ProtoError::decode(&bytes[..cut]).is_err(),
                    "{error:?} truncated to {cut} bytes decoded"
                );
            }
        }
    }

    /// The property the client's write path is built on: exactly the failures that happened
    /// *around* the answer are ambiguous. Every error the peer chose to send means it decided
    /// not to serve the request, and a request that never reached the wire cannot have been
    /// applied. Getting one of these wrong duplicates a write or refuses a safe retry.
    #[test]
    fn only_failures_around_the_answer_are_ambiguous() {
        for error in one_of_each() {
            let expected = matches!(
                error,
                ProtoError::Corrupt { .. }
                    | ProtoError::Io { .. }
                    | ProtoError::Closed { .. }
                    | ProtoError::Timeout { .. }
                    | ProtoError::Internal { .. }
            );
            assert_eq!(error.is_ambiguous(), expected, "{error:?}");
            assert_eq!(
                error.outcome(),
                if expected {
                    RequestOutcome::Unknown
                } else {
                    RequestOutcome::NotApplied
                },
                "{error:?}"
            );
        }
    }

    /// `Closed` and `NotSent` are the pair that carries the distinction. If they ever collapse
    /// into one variant, this is the test that says why they must not.
    #[test]
    fn a_closed_connection_is_ambiguous_and_an_unsent_request_is_not() {
        let closed = ProtoError::Closed {
            detail: "peer went away".to_owned(),
        };
        let not_sent = ProtoError::not_sent("connection refused");
        assert_eq!(closed.outcome(), RequestOutcome::Unknown);
        assert_eq!(not_sent.outcome(), RequestOutcome::NotApplied);
        assert_ne!(closed.code(), not_sent.code());
    }

    /// The retry set is a contract with the client: it decides which failures a retry loop is
    /// allowed to swallow, so it is pinned rather than left to a reader of the enum.
    #[test]
    fn only_redirectable_errors_are_retryable() {
        for error in one_of_each() {
            let expected = matches!(
                error,
                ProtoError::NotLeader { .. }
                    | ProtoError::EpochNotMatch { .. }
                    | ProtoError::ServerIsBusy { .. }
                    | ProtoError::RegionNotFound { .. }
            );
            assert_eq!(error.is_retryable(), expected, "{error:?}");
        }
    }
}
