//! The messages this client sends, and where the rest of them come from.
//!
//! Routing and errors are `esker-proto`'s, re-exported here so that call sites in this crate
//! name one module rather than two: [`Region`], [`Peer`], [`Epoch`] and the typed
//! [`ProtoError`] with its redirect hints all come from the protocol crate, which is their
//! single writer.
//!
//! What is still local is the `RawKv` request and response bodies, transcribed from
//! `docs/DESIGN.md` §9. They become `esker_proto::messages` in one commit when that module
//! lands, and nothing above this file changes when they do.
//!
//! # Invariants this file carries
//!
//! * **Keys are raw user bytes.** The `'r'` namespace of `docs/DESIGN.md` §3 is applied by the
//!   *store*, never by the client — on every path, scan bounds and `DeleteRange` included
//!   (`prompts/02-single-node-server.md`, deliverable 2).
//! * **Every request carries `{ region_id, epoch, peer }`** so a stale epoch is rejected with
//!   a redirect hint rather than served (`CLAUDE.md` invariant 5).
//! * **Byte-opaque.** Nothing here interprets a key or a value (invariant 7).

// TODO(phase-2): replace the message types below with `pub use esker_proto::messages::...`
// once the sibling lane lands them; the swap is meant to be one commit that deletes code.

use bytes::Bytes;

pub use esker_proto::{Epoch, Peer, PeerRole, ProtoError, Region, RequestOutcome};

/// The `{ region_id, epoch, peer }` header every key-value request carries
/// (`docs/DESIGN.md` §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Which region the client believes owns the key.
    pub region_id: u64,
    /// The epoch the client believes that region is at.
    pub epoch: Epoch,
    /// The peer the request is addressed to.
    pub peer: Peer,
}

/// A complete key-value request: the routing header plus the body.
///
/// The two travel together because a store cannot act on either alone — the header decides
/// whether it may answer, the body says what to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `{ region_id, epoch, peer }` (`docs/DESIGN.md` §9).
    pub context: RequestContext,
    /// What to do.
    pub body: RawRequest,
}

/// Which `RawKv` method a request is, without its arguments.
///
/// Useful for logging, for metrics and for matching in tests, none of which want to clone a
/// request's payload to find out what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RawMethod {
    /// [`RawRequest::Get`].
    Get,
    /// [`RawRequest::BatchGet`].
    BatchGet,
    /// [`RawRequest::Put`].
    Put,
    /// [`RawRequest::BatchPut`].
    BatchPut,
    /// [`RawRequest::Delete`].
    Delete,
    /// [`RawRequest::DeleteRange`].
    DeleteRange,
    /// [`RawRequest::Scan`].
    Scan,
    /// [`RawRequest::CompareAndSwap`].
    CompareAndSwap,
}

impl RawMethod {
    /// Whether re-sending the method changes the database differently the second time.
    ///
    /// This is one of the two things a retry decision turns on, and it is **not** the same
    /// question as "is it a read". Under last-write-wins a repeated `Put` of the same bytes
    /// leaves the same state, so `Put` is idempotent here; [`RawMethod::CompareAndSwap`] is
    /// not, because its second attempt sees the state its first attempt created.
    ///
    /// The other thing it turns on is whether the previous attempt reached a commit, which is
    /// [`ProtoError::outcome`]. Both have to say yes.
    #[must_use]
    pub fn is_idempotent(self) -> bool {
        match self {
            Self::Get
            | Self::BatchGet
            | Self::Put
            | Self::BatchPut
            | Self::Delete
            | Self::DeleteRange
            | Self::Scan => true,
            Self::CompareAndSwap => false,
        }
    }

    /// Whether the method can change the database at all.
    #[must_use]
    pub fn is_write(self) -> bool {
        !matches!(self, Self::Get | Self::BatchGet | Self::Scan)
    }
}

/// One `RawKv` request body (`docs/DESIGN.md` §9), keyed by **raw user bytes**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawRequest {
    /// Read one key.
    Get {
        /// The key.
        key: Bytes,
    },
    /// Read several keys in one round trip.
    BatchGet {
        /// The keys, in the order the answers come back.
        keys: Vec<Bytes>,
    },
    /// Write one key.
    Put {
        /// The key.
        key: Bytes,
        /// The value.
        value: Bytes,
    },
    /// Write several keys atomically.
    BatchPut {
        /// Key-value pairs.
        pairs: Vec<(Bytes, Bytes)>,
    },
    /// Remove one key.
    Delete {
        /// The key.
        key: Bytes,
    },
    /// Remove a half-open range. The v1 limitation of `docs/DESIGN.md` §4.7 applies.
    DeleteRange {
        /// Inclusive lower bound.
        start: Bytes,
        /// Exclusive upper bound; empty means unbounded.
        end: Bytes,
    },
    /// Read a bounded run of keys.
    Scan {
        /// Inclusive lower bound.
        start: Bytes,
        /// Exclusive upper bound; empty means unbounded.
        end: Bytes,
        /// Most entries to return. Bounded by the caller so a response fits one frame.
        limit: u32,
        /// Walk from the top of the range down instead of from the bottom up.
        reverse: bool,
    },
    /// Replace a key's value only if it currently holds `expected`.
    CompareAndSwap {
        /// The key.
        key: Bytes,
        /// The value the key must currently hold; `None` means "must be absent".
        expected: Option<Bytes>,
        /// The value to write; `None` deletes.
        new: Option<Bytes>,
    },
}

impl RawRequest {
    /// Which method this is.
    #[must_use]
    pub fn method(&self) -> RawMethod {
        match self {
            Self::Get { .. } => RawMethod::Get,
            Self::BatchGet { .. } => RawMethod::BatchGet,
            Self::Put { .. } => RawMethod::Put,
            Self::BatchPut { .. } => RawMethod::BatchPut,
            Self::Delete { .. } => RawMethod::Delete,
            Self::DeleteRange { .. } => RawMethod::DeleteRange,
            Self::Scan { .. } => RawMethod::Scan,
            Self::CompareAndSwap { .. } => RawMethod::CompareAndSwap,
        }
    }

    /// The key this request routes by: the one the region cache is consulted with.
    ///
    /// For the range methods that is the lower bound, which is the only part of the request a
    /// single-region client can route on. Splitting a range request across regions is
    /// phase 4's problem, not this one's.
    #[must_use]
    pub fn routing_key(&self) -> &[u8] {
        match self {
            Self::Get { key }
            | Self::Put { key, .. }
            | Self::Delete { key }
            | Self::CompareAndSwap { key, .. } => key,
            Self::BatchGet { keys } => keys.first().map_or(&[][..], |key| &key[..]),
            Self::BatchPut { pairs } => pairs.first().map_or(&[][..], |(key, _)| &key[..]),
            // TODO(phase-4): a reverse scan starts at `end` and walks down, so once there is
            // more than one region it routes by `end` rather than by `start`.
            Self::DeleteRange { start, .. } | Self::Scan { start, .. } => start,
        }
    }
    /// A lower bound on what this request encodes to, in bytes.
    ///
    /// Used to refuse an oversized request at the call site rather than have the far end tear
    /// the connection down mid-frame. It counts the payload exactly and the framing loosely:
    /// the payload is what actually gets large, and the check has margin because
    /// `MAX_FRAME_SIZE` sits far above any sane request.
    #[must_use]
    pub fn payload_size(&self) -> usize {
        /// Varint length prefix plus a little slack, per field.
        const PER_FIELD: usize = 6;
        match self {
            Self::Get { key } | Self::Delete { key } => key.len() + PER_FIELD,
            Self::BatchGet { keys } => {
                keys.iter().map(|key| key.len() + PER_FIELD).sum::<usize>() + PER_FIELD
            }
            Self::Put { key, value } => key.len() + value.len() + 2 * PER_FIELD,
            Self::BatchPut { pairs } => {
                pairs
                    .iter()
                    .map(|(key, value)| key.len() + value.len() + 2 * PER_FIELD)
                    .sum::<usize>()
                    + PER_FIELD
            }
            Self::DeleteRange { start, end } | Self::Scan { start, end, .. } => {
                start.len() + end.len() + 4 * PER_FIELD
            }
            Self::CompareAndSwap { key, expected, new } => {
                key.len()
                    + expected.as_ref().map_or(0, Bytes::len)
                    + new.as_ref().map_or(0, Bytes::len)
                    + 3 * PER_FIELD
            }
        }
    }
}

/// One `RawKv` response body. The variant always matches the request's method; a mismatch is
/// a protocol error, not something to be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawResponse {
    /// The value, or nothing if the key is absent.
    Get(Option<Bytes>),
    /// One answer per requested key, in the order they were asked for.
    BatchGet(Vec<Option<Bytes>>),
    /// The write is durable to the extent the request asked for.
    Put,
    /// As [`RawResponse::Put`].
    BatchPut,
    /// The key is gone. Deleting an absent key is not an error.
    Delete,
    /// The range is gone.
    DeleteRange,
    /// The entries found, in key order (reversed for a reverse scan).
    Scan(Vec<(Bytes, Bytes)>),
    /// Whether the swap happened, and what the key held before it.
    CompareAndSwap {
        /// Whether `expected` matched and `new` was written.
        swapped: bool,
        /// What the key held when the comparison was made.
        previous: Option<Bytes>,
    },
}

impl RawResponse {
    /// Which method this answers.
    #[must_use]
    pub fn method(&self) -> RawMethod {
        match self {
            Self::Get(_) => RawMethod::Get,
            Self::BatchGet(_) => RawMethod::BatchGet,
            Self::Put => RawMethod::Put,
            Self::BatchPut => RawMethod::BatchPut,
            Self::Delete => RawMethod::Delete,
            Self::DeleteRange => RawMethod::DeleteRange,
            Self::Scan(_) => RawMethod::Scan,
            Self::CompareAndSwap { .. } => RawMethod::CompareAndSwap,
        }
    }
}

/// What a call to a store returns: an answer, or a typed refusal.
pub type CallResult = Result<RawResponse, ProtoError>;

#[cfg(test)]
mod tests {
    use super::{Bytes, RawMethod, RawRequest};

    /// The retry rules turn on this, so it is pinned rather than assumed. `CompareAndSwap` is
    /// the one method whose second attempt sees what its first attempt did.
    #[test]
    fn compare_and_swap_is_the_one_method_a_retry_can_change() {
        for method in [
            RawMethod::Get,
            RawMethod::BatchGet,
            RawMethod::Put,
            RawMethod::BatchPut,
            RawMethod::Delete,
            RawMethod::DeleteRange,
            RawMethod::Scan,
        ] {
            assert!(method.is_idempotent(), "{method:?}");
        }
        assert!(!RawMethod::CompareAndSwap.is_idempotent());

        for method in [RawMethod::Get, RawMethod::BatchGet, RawMethod::Scan] {
            assert!(!method.is_write(), "{method:?}");
        }
        assert!(RawMethod::Put.is_write());
        assert!(RawMethod::DeleteRange.is_write());
    }

    /// Every request has to name the key it routes by, including the ones whose payload is a
    /// list. An empty list routes to the start of the key space rather than panicking, which
    /// is `CLAUDE.md` invariant 9 in miniature.
    #[test]
    fn every_request_routes_by_a_key() {
        let key = Bytes::from_static(b"k");
        assert_eq!(RawRequest::Get { key: key.clone() }.routing_key(), b"k");
        assert_eq!(
            RawRequest::BatchGet {
                keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
            }
            .routing_key(),
            b"a"
        );
        assert_eq!(RawRequest::BatchGet { keys: vec![] }.routing_key(), b"");
        assert_eq!(RawRequest::BatchPut { pairs: vec![] }.routing_key(), b"");
        assert_eq!(
            RawRequest::Scan {
                start: Bytes::from_static(b"s"),
                end: Bytes::new(),
                limit: 10,
                reverse: false,
            }
            .routing_key(),
            b"s"
        );
    }

    /// The size check guards against a request nobody could send, so it has to grow with the
    /// payload and never under-count the bytes themselves.
    #[test]
    fn the_size_estimate_counts_every_byte_of_the_payload() {
        let big = Bytes::from(vec![0u8; 4096]);
        let put = RawRequest::Put {
            key: Bytes::from_static(b"k"),
            value: big.clone(),
        };
        assert!(put.payload_size() > 4096);

        let batch = RawRequest::BatchPut {
            pairs: vec![
                (Bytes::from_static(b"a"), big.clone()),
                (Bytes::from_static(b"b"), big),
            ],
        };
        assert!(batch.payload_size() > 8192);
        assert!(batch.payload_size() > put.payload_size());

        // An empty request still costs its framing, never zero.
        assert!(RawRequest::BatchGet { keys: vec![] }.payload_size() > 0);
    }
}
