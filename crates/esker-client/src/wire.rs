//! **A stand-in for `esker-proto`.**
//!
//! The wire types belong to `esker-proto`, which has exactly one writer this phase. This
//! module is the shape this client codes against until that crate lands: the request and
//! response bodies of `RawKv`, the typed server error with its redirect hints, and the
//! routing types they carry — all transcribed from `docs/DESIGN.md` §6 and §9 rather than
//! invented here.
//!
//! When `esker-proto` lands, this file becomes a set of `pub use` re-exports and nothing
//! above it changes. That is the whole reason it exists as one module with no logic in it.
//!
//! # Invariants this file carries
//!
//! * **Keys are raw user bytes.** The `'r'` namespace of `docs/DESIGN.md` §3 is applied by the
//!   *store*, never by the client. A key that goes into a [`RawRequest`] is exactly what the
//!   caller passed (`prompts/02-single-node-server.md`, deliverable 2).
//! * **Every request carries `{ region_id, epoch, peer }`** so a stale epoch is rejected with
//!   a redirect hint rather than served (`CLAUDE.md` invariant 5).
//! * **Byte-opaque.** Nothing here interprets a key or a value (invariant 7).

// TODO(phase-2): replace the bodies of this module with `pub use esker_proto::...` once the
// sibling lane lands the crate; the swap is meant to be one commit that deletes code.

use bytes::Bytes;

/// A region's version pair: `conf_ver` bumps on a membership change, `version` on a split
/// (`docs/DESIGN.md` §6).
///
/// Requests carry it and stores compare it. Two epochs are ordered componentwise; a request
/// whose epoch is behind the store's is rejected with [`ServerError::EpochNotMatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionEpoch {
    /// Bumped by every configuration change.
    pub conf_ver: u64,
    /// Bumped by every split.
    pub version: u64,
}

/// What a peer is allowed to do in its Raft group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Votes and may become leader.
    Voter,
    /// Receives the log but neither votes nor campaigns.
    Learner,
}

/// One replica of one region on one store (`docs/DESIGN.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The store hosting this replica.
    pub store_id: u64,
    /// Unique within the region; a peer id is never reused after removal.
    pub peer_id: u64,
    /// Voter or learner.
    pub role: PeerRole,
}

impl Peer {
    /// A voting peer, the only kind phase 2 creates.
    #[must_use]
    pub fn voter(store_id: u64, peer_id: u64) -> Self {
        Self {
            store_id,
            peer_id,
            role: PeerRole::Voter,
        }
    }
}

/// A contiguous key range replicated by one Raft group (`docs/DESIGN.md` §6).
///
/// `end_key` is exclusive, and **empty means unbounded** — the first region is `["", "")`, so
/// an empty `end_key` sorts after every key rather than before it. Every comparison against it
/// has to say so explicitly, which is why [`Region::contains`] exists rather than callers
/// writing the check themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// Cluster-unique region id.
    pub id: u64,
    /// Inclusive lower bound.
    pub start_key: Bytes,
    /// Exclusive upper bound; empty means "no upper bound".
    pub end_key: Bytes,
    /// Every replica of this region.
    pub peers: Vec<Peer>,
    /// Membership and split versions.
    pub epoch: RegionEpoch,
}

impl Region {
    /// Whether `key` falls inside this region.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= &self.start_key[..] && (self.end_key.is_empty() || key < &self.end_key[..])
    }

    /// The peer on `store_id`, if this region has one.
    #[must_use]
    pub fn peer_on(&self, store_id: u64) -> Option<&Peer> {
        self.peers.iter().find(|peer| peer.store_id == store_id)
    }
}

/// The `{ region_id, epoch, peer }` header every key-value request carries
/// (`docs/DESIGN.md` §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Which region the client believes owns the key.
    pub region_id: u64,
    /// The epoch the client believes that region is at.
    pub epoch: RegionEpoch,
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
    /// This is the property the retry rules turn on, and it is **not** the same question as
    /// "is it a read". Under last-write-wins a repeated `Put` of the same bytes leaves the
    /// same state, so `Put` is idempotent here; [`RawMethod::CompareAndSwap`] is not, because
    /// its second attempt sees the state its first attempt created.
    ///
    /// It is deliberately a property of the *method*, not of a delivery outcome: whether a
    /// retry is safe also needs to know that the previous attempt did not reach a commit. See
    /// [`crate::Error::AmbiguousResult`], which is what the client returns when it cannot
    /// prove that.
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
        /// Inclusive lower bound of a forward scan; exclusive upper bound of a reverse one.
        start: Bytes,
        /// The other end of the range; empty means unbounded.
        end: Bytes,
        /// Most entries to return. Bounded by the caller so a response fits one frame.
        limit: u32,
        /// Walk from `end` down to `start` instead.
        reverse: bool,
        /// Return keys without their values.
        keys_only: bool,
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

/// A Percolator lock standing between a reader and a value (`docs/DESIGN.md` §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// The transaction's primary key, which decides whether it committed.
    pub primary: Bytes,
    /// The transaction's start timestamp.
    pub start_ts: u64,
    /// Milliseconds the lock lives for without a heartbeat.
    pub ttl: u64,
    // TODO(phase-5): `kind` and `short_value` from the `lock` CF layout.
}

/// What the server refused to do, and what the client should do about it
/// (`docs/DESIGN.md` §9).
///
/// Every variant is a *refusal*: the server rejected the request before changing anything.
/// That is what makes retrying the redirectable ones safe for a write as well as a read, and
/// it is why a connection that died mid-call is **not** in this enum — see
/// [`TransportError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerError {
    /// This peer is not the leader. The hint, when present, is where to go next.
    #[error("peer is not the leader of region {region_id}")]
    NotLeader {
        /// The region asked about.
        region_id: u64,
        /// Where the leader was last seen, if the peer knows.
        leader_hint: Option<Peer>,
    },
    /// The request's epoch is behind the store's: the region split or changed membership.
    /// The regions now covering the old range come back so the cache can be repaired.
    #[error("epoch of region {region_id} has moved on")]
    EpochNotMatch {
        /// The region asked about.
        region_id: u64,
        /// The regions that now cover the range the request aimed at.
        current_regions: Vec<Region>,
    },
    /// The key is outside the range this region owns.
    #[error("key is outside region {region_id}")]
    KeyNotInRegion {
        /// The region asked about.
        region_id: u64,
        /// The key that missed.
        key: Bytes,
    },
    /// The store is shedding load — a write stall, a full queue. Back off and come back.
    #[error("server is busy: {reason}")]
    ServerIsBusy {
        /// What is congested, for a human reading a log.
        reason: String,
        /// How long the server suggests waiting, in milliseconds. Advisory.
        backoff_ms: u64,
    },
    /// A transaction holds the key (phase 5).
    #[error("key is locked by transaction at {}", .lock.start_ts)]
    Locked {
        /// Which transaction, and how to resolve it.
        lock: Box<LockInfo>,
    },
    /// The store failed for a reason that has no redirect hint: a corrupt file, an I/O error.
    #[error("store failed: {0}")]
    Other(String),
}

/// Why a call did not produce an answer, from the transport's point of view.
///
/// The split between [`TransportError::NotSent`] and [`TransportError::Ambiguous`] is the
/// whole point of this enum: it is the difference between "the write provably did not happen"
/// and "nobody can say". A transport that cannot tell the two apart must report
/// `Ambiguous`, which is the safe direction to be wrong in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The request never reached the wire: the connection could not be established, or the
    /// transport rejected it before writing a byte of the request frame. The server cannot
    /// have seen it, so re-sending it is safe for any method.
    #[error("request was not sent: {0}")]
    NotSent(String),
    /// The request went out and no answer came back — the connection died, or the deadline
    /// passed while waiting. Whether the server applied it is **unknown**.
    #[error("no answer came back: {0}")]
    Ambiguous(String),
    /// Bytes came back that this build cannot make sense of: a bad checksum, an unknown
    /// method, a response whose kind does not match the request. A bug or a version skew,
    /// never something to retry.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl TransportError {
    /// Whether the server provably never saw the request.
    #[must_use]
    pub fn is_provably_unsent(&self) -> bool {
        matches!(self, Self::NotSent(_))
    }
}

/// What a call to a store returns: an answer, or a typed refusal.
pub type CallResult = Result<RawResponse, CallError>;

/// Either end of the failure story: the server refused, or the transport could not ask.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The store answered with a refusal.
    #[error(transparent)]
    Server(#[from] ServerError),
    /// No answer was obtained.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[cfg(test)]
mod tests {
    use super::{Bytes, Peer, RawMethod, RawRequest, Region, RegionEpoch, TransportError};

    fn region(start: &[u8], end: &[u8]) -> Region {
        Region {
            id: 1,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, 1)],
            epoch: RegionEpoch::default(),
        }
    }

    /// An empty `end_key` means unbounded, and a byte comparison would get that backwards:
    /// `b""` sorts before everything. Getting this wrong routes every key to the wrong region
    /// the moment a second one exists.
    #[test]
    fn an_empty_end_key_is_the_end_of_the_key_space() {
        let whole = region(b"", b"");
        assert!(whole.contains(b""));
        assert!(whole.contains(b"\xff\xff\xff\xff"));

        let first_half = region(b"", b"m");
        assert!(first_half.contains(b"a"));
        assert!(!first_half.contains(b"m"), "end_key is exclusive");
        assert!(!first_half.contains(b"z"));

        let second_half = region(b"m", b"");
        assert!(!second_half.contains(b"a"));
        assert!(second_half.contains(b"m"));
        assert!(second_half.contains(b"\xff"));
    }

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
                keys_only: false,
            }
            .routing_key(),
            b"s"
        );
    }

    /// The one distinction the whole retry story rests on.
    #[test]
    fn only_a_never_sent_request_is_provably_unsent() {
        assert!(TransportError::NotSent("refused".into()).is_provably_unsent());
        assert!(!TransportError::Ambiguous("reset".into()).is_provably_unsent());
        assert!(!TransportError::Protocol("bad crc".into()).is_provably_unsent());
    }

    #[test]
    fn a_region_finds_its_peer_by_store() {
        let mut region = region(b"", b"");
        region.peers = vec![Peer::voter(1, 11), Peer::voter(2, 12)];
        assert_eq!(region.peer_on(2).map(|peer| peer.peer_id), Some(12));
        assert_eq!(region.peer_on(3), None);
    }
}
