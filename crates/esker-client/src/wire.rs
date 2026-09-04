//! `esker-proto`, re-exported, plus the three questions a client asks about a message that
//! the protocol crate has no reason to answer.
//!
//! Everything here that is a *type* belongs to `esker-proto`, whose single writer defines it:
//! [`Region`], [`Peer`], [`Epoch`], [`Method`], [`RawKvReq`], [`RawKvResp`], [`RequestHeader`]
//! and the typed [`ProtoError`] with its redirect hints. Call sites in this crate name one
//! module rather than two, and the stand-in this lane started from is gone.
//!
//! What is left is three free functions. They are here rather than in `esker-proto` because
//! each is about *routing and retrying* a request rather than about encoding one, and the
//! protocol crate has no region cache to route with.
//!
//! # Invariants
//!
//! * **Keys are raw user bytes.** The `'r'` namespace of `docs/DESIGN.md` §3 is applied by the
//!   *store*, never by the client — on every path, scan bounds and `DeleteRange` included
//!   (`prompts/02-single-node-server.md`, deliverable 2).
//! * **Every request carries `{ region_id, epoch, peer }`** so a stale epoch is rejected with
//!   a redirect hint rather than served (`CLAUDE.md` invariant 5).
//! * **Byte-opaque.** Nothing here interprets a key or a value (invariant 7).

use bytes::Bytes;

pub use esker_proto::messages::{
    DEFAULT_SCAN_LIMIT, Method, RawKvReq, RawKvResp, Request, RequestHeader, Response,
};
pub use esker_proto::txn::{LockInfo, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
pub use esker_proto::{Epoch, MAX_FRAME_SIZE, Peer, PeerRole, ProtoError, Region, RequestOutcome};

/// What a call to a store returns: an answer, or a typed refusal.
///
/// The whole [`Response`] rather than a `RawKvResp`, because two services now go through the
/// same routing and retry loop and unwrapping one of them at the transport would mean a
/// second loop for the other.
pub type CallResult = Result<Response, ProtoError>;

/// A request before it has been addressed to a region.
///
/// The router routes, retries and backs off identically for both services, so the loop takes
/// one of these rather than being written twice ([`crate::router`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A `RawKv` request (namespace `'r'`).
    Raw(RawKvReq),
    /// A `TxnKv` request (namespace `'x'`, `docs/DESIGN.md` §8).
    Txn(TxnKvReq),
}

impl Body {
    /// The method this will be sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Raw(request) => request.method(),
            Self::Txn(request) => request.method(),
        }
    }

    /// The key the region cache is consulted with.
    #[must_use]
    pub fn routing_key(&self) -> &[u8] {
        match self {
            Self::Raw(request) => routing_key(request),
            Self::Txn(request) => request.routing_key(),
        }
    }

    /// A lower bound on what this encodes to, in bytes.
    #[must_use]
    pub fn payload_size(&self) -> usize {
        match self {
            Self::Raw(request) => payload_size(request),
            Self::Txn(request) => txn_payload_size(request),
        }
    }

    /// The wire request, addressed to a region.
    #[must_use]
    pub fn into_request(self, header: RequestHeader) -> Request {
        match self {
            Self::Raw(request) => Request::raw_kv(header, request),
            Self::Txn(request) => Request::txn_kv(header, request),
        }
    }
}

impl From<RawKvReq> for Body {
    fn from(request: RawKvReq) -> Self {
        Self::Raw(request)
    }
}

impl From<TxnKvReq> for Body {
    fn from(request: TxnKvReq) -> Self {
        Self::Txn(request)
    }
}

/// A lower bound on what a `TxnKv` request encodes to, in bytes. See [`payload_size`].
#[must_use]
pub fn txn_payload_size(request: &TxnKvReq) -> usize {
    /// Varint length prefix plus a little slack, per field.
    const PER_FIELD: usize = 6;
    let keys =
        |keys: &[Bytes]| keys.iter().map(|key| key.len() + PER_FIELD).sum::<usize>() + PER_FIELD;
    match request {
        TxnKvReq::Get { key, .. } => key.len() + 2 * PER_FIELD,
        TxnKvReq::Scan { start, end, .. } => start.len() + end.len() + 5 * PER_FIELD,
        TxnKvReq::Prewrite {
            primary, mutations, ..
        } => {
            primary.len()
                + mutations
                    .iter()
                    .map(|mutation| match mutation {
                        TxnMutation::Put { key, value } => key.len() + value.len() + 2 * PER_FIELD,
                        TxnMutation::Delete { key } => key.len() + PER_FIELD,
                    })
                    .sum::<usize>()
                + 4 * PER_FIELD
        }
        TxnKvReq::Commit { keys: k, .. }
        | TxnKvReq::Rollback { keys: k, .. }
        | TxnKvReq::ResolveLock { keys: k, .. } => keys(k) + 2 * PER_FIELD,
        TxnKvReq::Heartbeat { primary, .. } => primary.len() + 3 * PER_FIELD,
        TxnKvReq::GcSafepoint { .. } => PER_FIELD,
    }
}

/// The key a request routes by: the one the region cache is consulted with.
///
/// For the range methods that is the lower bound, which is the only part of a request a
/// single-region client can route on. Splitting a range request across regions is phase 4's
/// problem, not this one's.
///
/// A batch with no keys routes to the start of the key space rather than panicking
/// (`CLAUDE.md` invariant 9).
#[must_use]
pub fn routing_key(request: &RawKvReq) -> &[u8] {
    match request {
        RawKvReq::Get { key }
        | RawKvReq::Put { key, .. }
        | RawKvReq::Delete { key, .. }
        | RawKvReq::CompareAndSwap { key, .. } => key,
        RawKvReq::BatchGet { keys } => keys.first().map_or(&[][..], |key| &key[..]),
        RawKvReq::BatchPut { pairs, .. } => pairs.first().map_or(&[][..], |(key, _)| &key[..]),
        // `TODO(debt-c6 #2)`: for a reverse scan `start` is the **exclusive** upper bound, so a
        // `start` sitting exactly on a region boundary routes to the region above the one holding
        // every key the scan should return, and the answer is an empty page that reads like the
        // end of the range (`docs/plans/debt-c6.md` §4).
        RawKvReq::DeleteRange { start, .. } | RawKvReq::Scan { start, .. } => start,
    }
}

/// A lower bound on what a request encodes to, in bytes.
///
/// Lets the client refuse an oversized request at the call site rather than have the far end
/// tear the connection down mid-frame. It counts the payload exactly and the framing loosely:
/// the payload is what actually gets large, and the check has margin because
/// [`MAX_FRAME_SIZE`] sits far above any sane request.
#[must_use]
pub fn payload_size(request: &RawKvReq) -> usize {
    /// Varint length prefix plus a little slack, per field.
    const PER_FIELD: usize = 6;
    match request {
        RawKvReq::Get { key } | RawKvReq::Delete { key, .. } => key.len() + PER_FIELD,
        RawKvReq::BatchGet { keys } => {
            keys.iter().map(|key| key.len() + PER_FIELD).sum::<usize>() + PER_FIELD
        }
        RawKvReq::Put { key, value, .. } => key.len() + value.len() + 2 * PER_FIELD,
        RawKvReq::BatchPut { pairs, .. } => {
            pairs
                .iter()
                .map(|(key, value)| key.len() + value.len() + 2 * PER_FIELD)
                .sum::<usize>()
                + PER_FIELD
        }
        RawKvReq::DeleteRange { start, end, .. } | RawKvReq::Scan { start, end, .. } => {
            start.len() + end.len() + 4 * PER_FIELD
        }
        RawKvReq::CompareAndSwap {
            key,
            expected,
            value,
            ..
        } => {
            key.len()
                + expected.as_ref().map_or(0, Bytes::len)
                + value.as_ref().map_or(0, Bytes::len)
                + 3 * PER_FIELD
        }
    }
}

/// Whether re-sending a method changes the database differently the second time.
///
/// This is **not** the same question as [`Method::is_mutation`], and the difference is the
/// point. Under last-write-wins a repeated `Put` of the same bytes leaves the same state, so
/// `Put` is a mutation but is idempotent; `CompareAndSwap` is neither, because its second
/// attempt sees the state its first attempt created.
///
/// Idempotence is only half of what makes a retry safe. The other half is that the previous
/// attempt provably did not commit, which is [`ProtoError::outcome`]. Both have to say yes —
/// which is why this client, whose retries are all triggered by refusals, never needs to ask
/// this to be correct today, and why `esker-txn` will (`prompts/05-txn.md`).
#[must_use]
pub fn is_idempotent(method: Method) -> bool {
    method != Method::RawCompareAndSwap
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{Method, RawKvReq, is_idempotent, payload_size, routing_key};

    /// `CompareAndSwap` is the one method whose second attempt sees what its first attempt
    /// did. Pinned rather than assumed, because phase 5 turns on it.
    #[test]
    fn compare_and_swap_is_the_one_method_a_retry_can_change() {
        for method in Method::ALL {
            assert_eq!(
                is_idempotent(method),
                method != Method::RawCompareAndSwap,
                "{}",
                method.name()
            );
        }
        // A mutation is not the same thing as a non-idempotent operation, and conflating the
        // two would either forbid safe retries or allow unsafe ones.
        assert!(Method::RawPut.is_mutation());
        assert!(is_idempotent(Method::RawPut));
    }

    /// Every request has to name the key it routes by, including the ones whose payload is a
    /// list. An empty list routes to the start of the key space rather than panicking.
    #[test]
    fn every_request_routes_by_a_key() {
        assert_eq!(routing_key(&RawKvReq::get(b"k".as_slice())), b"k");
        assert_eq!(
            routing_key(&RawKvReq::BatchGet {
                keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
            }),
            b"a"
        );
        assert_eq!(routing_key(&RawKvReq::BatchGet { keys: vec![] }), b"");
        assert_eq!(
            routing_key(&RawKvReq::batch_put(vec![])),
            b"",
            "an empty batch must not panic"
        );
        assert_eq!(
            routing_key(&RawKvReq::scan(b"s".as_slice(), b"".as_slice(), 10)),
            b"s"
        );
        assert_eq!(
            routing_key(&RawKvReq::delete_range(b"lo".as_slice(), b"hi".as_slice())),
            b"lo"
        );
        assert_eq!(
            routing_key(&RawKvReq::compare_and_swap(
                b"c".as_slice(),
                None,
                Some(Bytes::from_static(b"v")),
            )),
            b"c"
        );
    }

    /// The size check guards against a request nobody could send, so it has to grow with the
    /// payload and never under-count the bytes themselves.
    #[test]
    fn the_size_estimate_counts_every_byte_of_the_payload() {
        let big = Bytes::from(vec![0u8; 4096]);
        let put = RawKvReq::put(Bytes::from_static(b"k"), big.clone());
        assert!(payload_size(&put) > 4096);

        let batch = RawKvReq::batch_put(vec![
            (Bytes::from_static(b"a"), big.clone()),
            (Bytes::from_static(b"b"), big),
        ]);
        assert!(payload_size(&batch) > 8192);
        assert!(payload_size(&batch) > payload_size(&put));

        // An empty request still costs its framing, never zero.
        assert!(payload_size(&RawKvReq::BatchGet { keys: vec![] }) > 0);
    }
}
