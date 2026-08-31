//! Service `0x06`: asking a columnar replica to evaluate a plan fragment.
//!
//! ```text
//! 0x0601 Evaluate   run a fragment against this node's columnar copy of a region
//! ```
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decisions 3 and 4,
//! `docs/plans/phase-8-learner.md` §wire.
//!
//! # A service of its own
//!
//! Not a seventh `TxnKv` method, for the reason [`SERVICE_ADMIN`](crate::messages::SERVICE_ADMIN)
//! gives for itself: these are not key-value work and must not be counted as it by anything
//! watching request rates. A fragment is a *plan* evaluated on a node that may hold no voter at
//! all, and anything routing or metering on the service byte has to tell the two apart without
//! decoding a body.
//!
//! # The fragment is carried, not understood
//!
//! [`FragmentReq::fragment`] is `esker_columnar`'s format — version byte, little-endian body,
//! CRC32C, its own goldens, and 402 million cases through its decoder. This crate carries it as a
//! length-prefixed byte string and has no opinion about its contents, exactly as it carries a
//! transaction's lock payload ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md)).
//!
//! **One definition of fragment bytes, and it is theirs.** A second decoder here would be a
//! second definition of what a filter means, and two nodes disagreeing about that is a wrong
//! answer rather than a protocol error.
//!
//! # Refusal is an answer, not an error
//!
//! ADR 0022's "refuse, never partially honour" crosses the wire as [`FragmentResp::Refused`] — a
//! variant of the *response*, never a [`ProtoError`](crate::ProtoError) frame. A node that does
//! not implement an operator, or that cannot reach [`FragmentReq::min_apply_index`] inside its
//! deadline, is answering correctly: the answer means *fall back to a row scan*, which is a path
//! the planner already has for the cost rule.
//!
//! An error frame would put that on the caller's error path, where retries and fault metrics
//! live, and would make every rolling upgrade look like a fault. The distinction is the same one
//! `esker-columnar` draws between corruption and refusal, and in the same order: intact bytes
//! naming something this build does not implement are a build, not damage.

pub mod result;

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::messages::Method;

pub use result::{
    AggregateKind, Body, Group, Partial, RESULT_FORMAT_VERSION, Value, ValueType,
    decode as decode_result, encode as encode_result,
};

/// Ask a columnar replica to evaluate a fragment.
///
/// Addressed to a region like any other read: the [`RequestHeader`](crate::RequestHeader) that
/// carries it names the region, the epoch and the peer, so `CLAUDE.md` invariant 5 is checked
/// here exactly as it is for a row read. There is deliberately **no epoch in this body** — two
/// epochs on one wire would be two sources of truth about the same fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentReq {
    /// The plan fragment, in `esker_columnar`'s format. Opaque here.
    pub fragment: Bytes,
    /// The reading transaction's timestamp.
    ///
    /// **Two jobs, one number.** It is the snapshot this request is made under, and it is the
    /// MVCC visibility timestamp the evaluator applies while it scans — "the newest version with
    /// `commit_ts <= ts`" means the same thing on a learner as on a row replica, because the
    /// learner has applied the same commit records (ADR 0022 Decision 4, half one).
    ///
    /// Both jobs are the same instant, so they are the same field. A second timestamp for the
    /// second job would be a second answer to "as of when", and the two could differ.
    pub ts: u64,
    /// The apply index this node must reach before it may evaluate.
    ///
    /// ADR 0022 Decision 4, half two, and **separate from `ts` on purpose**. This is a catch-up
    /// bound satisfied by a `ReadIndex` round against the region's leader *before* evaluation
    /// begins; `ts` is visibility applied *during* it. They fail differently — one refuses with
    /// [`RefusalReason::TooFarBehind`], the other silently returns older data — and a build that
    /// derived one from the other would answer from a state it had not reached.
    pub min_apply_index: u64,
}

/// Why a fragment was not evaluated.
///
/// An enum rather than a string because the planner branches on it. `detail` beside it is for a
/// human reading a log and is never matched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// This build cannot evaluate some part of the fragment — an operator, an aggregate, a type,
    /// or a format version it does not know. Retrying here will not help; another replica running
    /// the same build will refuse it too.
    Unsupported,
    /// The node could not reach [`FragmentReq::min_apply_index`] inside its deadline. Another
    /// replica may be closer, and the same node may succeed later.
    TooFarBehind,
    /// This node holds no columnar copy of the region. Placement has changed, or the caller's
    /// routing is stale.
    NotColumnar,
}

impl RefusalReason {
    fn tag(self) -> u8 {
        match self {
            RefusalReason::Unsupported => 1,
            RefusalReason::TooFarBehind => 2,
            RefusalReason::NotColumnar => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, DecodeError> {
        Ok(match tag {
            1 => RefusalReason::Unsupported,
            2 => RefusalReason::TooFarBehind,
            3 => RefusalReason::NotColumnar,
            _ => {
                return Err(DecodeError::invalid(
                    "fragment.refusal",
                    "unknown refusal reason",
                ));
            }
        })
    }
}

/// What a fragment cost, for `EXPLAIN` and for an operator watching pruning work.
///
/// Mirrors `esker_columnar::ScanStats`. It rides beside the result rather than inside the result
/// format because it is not part of the *answer*: a build that ignored every field would still be
/// correct. Carried now rather than at milestone 4 because a field added to a format later costs
/// a version bump, and this one is five varints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Stripes the file has, in the range the fragment asked for.
    pub stripes_considered: u64,
    /// Stripes whose statistics did not rule them out.
    pub stripes_read: u64,
    /// Column chunks decoded.
    pub chunks_decoded: u64,
    /// Rows that reached the filter.
    pub rows_scanned: u64,
    /// Rows the filter kept.
    pub rows_matched: u64,
}

impl ScanStats {
    fn encode(self, out: &mut Encoder) {
        out.put_varint(self.stripes_considered);
        out.put_varint(self.stripes_read);
        out.put_varint(self.chunks_decoded);
        out.put_varint(self.rows_scanned);
        out.put_varint(self.rows_matched);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            stripes_considered: input.get_varint("stats.stripes_considered")?,
            stripes_read: input.get_varint("stats.stripes_read")?,
            chunks_decoded: input.get_varint("stats.chunks_decoded")?,
            rows_scanned: input.get_varint("stats.rows_scanned")?,
            rows_matched: input.get_varint("stats.rows_matched")?,
        })
    }
}

/// A columnar node's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentResp {
    /// The fragment was evaluated. `result` is [`result`]'s format — versioned and checksummed of
    /// its own, so it survives being cached, spilled or forwarded without this frame.
    Result {
        /// The answer, in the fragment result format.
        result: Bytes,
        /// What it cost.
        stats: ScanStats,
    },
    /// The fragment was **not** evaluated, and this is a normal answer. See the module docs.
    Refused {
        /// What the planner should do about it.
        reason: RefusalReason,
        /// For a human. Never matched on.
        detail: String,
    },
}

impl FragmentReq {
    /// The method this request is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        Method::FragmentEvaluate
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        out.put_bytes(&self.fragment);
        out.put_varint(self.ts);
        out.put_varint(self.min_apply_index);
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            fragment: Bytes::copy_from_slice(input.get_bytes("fragment.fragment")?),
            ts: input.get_varint("fragment.ts")?,
            min_apply_index: input.get_varint("fragment.min_apply_index")?,
        })
    }
}

impl FragmentResp {
    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Result { result, stats } => {
                out.put_u8(0);
                out.put_bytes(result);
                stats.encode(out);
            }
            Self::Refused { reason, detail } => {
                out.put_u8(1);
                out.put_u8(reason.tag());
                out.put_str(detail);
            }
        }
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match input.get_u8("fragment.resp.kind")? {
            0 => Self::Result {
                result: Bytes::copy_from_slice(input.get_bytes("fragment.result")?),
                stats: ScanStats::decode(input)?,
            },
            1 => Self::Refused {
                reason: RefusalReason::from_tag(input.get_u8("fragment.refusal.reason")?)?,
                detail: input.get_str("fragment.refusal.detail")?.to_owned(),
            },
            _ => {
                return Err(DecodeError::invalid(
                    "fragment.resp.kind",
                    "unknown response kind",
                ));
            }
        })
    }
}
