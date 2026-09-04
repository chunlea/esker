//! Every message, and the method numbers that name them (*fixed*) — `docs/DESIGN.md` §9.
//!
//! A body is `tag:u16 ++ fields`. In a request or a response the tag is the [`Method`]; in an
//! error frame it is the code from [`crate::error::code`]. Ping, pong and stream frames carry
//! no tag at all — a stream chunk's body *is* the chunk.
//!
//! Two rules hold for every message here, and both are about refusing to guess:
//!
//! * **An unknown method is an error.** Not a skipped frame, not a default. Compatibility is
//!   negotiated once through [`crate::WIRE_VERSION`] on connect, so by the time a method
//!   arrives both ends have already agreed on which ones exist.
//! * **Trailing bytes are an error.** A body longer than its fields is a different message.
//!
//! Every key-value request carries a [`RequestHeader`], so a store can refuse a stale epoch
//! with a redirect hint even when — as in this phase — there is one region and its epoch
//! never moves (`CLAUDE.md` invariant 5).
//!
//! Keys and values are **raw user bytes**. The `'r'` namespace of `docs/DESIGN.md` §3 is added
//! by the store on every path, including scan and delete-range bounds; a client that could put
//! a prefix on the wire could address a namespace that is not its own (invariant 7).

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::raft::RaftBatch;
use crate::region::{Epoch, Region};

/// The largest number of key-value pairs a `Scan` returns when the caller names no limit.
///
/// A limit of zero on the wire means "as many as the server will give", and this is that
/// number. It exists so an unbounded scan of a whole region is many bounded answers rather
/// than one frame that cannot fit.
pub const DEFAULT_SCAN_LIMIT: u32 = 1024;

/// Which request or response a body holds (*fixed*).
///
/// The high byte is the service and the low byte the method, so each service's numbers stay
/// contiguous and a whole service can be reserved before it is written. Zero is not a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Method {
    /// Version negotiation, sent once when a connection opens.
    Hello = 0x0001,

    /// `RawKv::Get`.
    RawGet = 0x0101,
    /// `RawKv::BatchGet`.
    RawBatchGet = 0x0102,
    /// `RawKv::Put`.
    RawPut = 0x0103,
    /// `RawKv::BatchPut`.
    RawBatchPut = 0x0104,
    /// `RawKv::Delete`.
    RawDelete = 0x0105,
    /// `RawKv::DeleteRange`.
    RawDeleteRange = 0x0106,
    /// `RawKv::Scan`.
    RawScan = 0x0107,
    /// `RawKv::CompareAndSwap`.
    RawCompareAndSwap = 0x0108,

    /// `Pd::Bootstrap` — register a store, and create the cluster if it is the first
    /// (`docs/DESIGN.md` §7, [`crate::pd`]).
    PdBootstrap = 0x0301,
    /// `Pd::StoreHeartbeat`.
    PdStoreHeartbeat = 0x0302,
    /// `Pd::RegionHeartbeat`.
    PdRegionHeartbeat = 0x0303,
    /// `Pd::GetRegion`.
    PdGetRegion = 0x0304,
    /// `Pd::AllocId`.
    PdAllocId = 0x0305,
    /// `Pd::Tso`.
    PdTso = 0x0306,
    /// `Pd::SchemaLease` — how long a node may act on a cached schema before it must ask again
    /// ([ADR 0028](../../docs/adr/0028-the-schema-lease.md)).
    PdSchemaLease = 0x0307,
    /// `Pd::ReportColumnar` — a SQL node telling PD which key ranges want columnar replicas
    /// ([ADR 0022](../../docs/adr/0022-columnar-learner-replica.md), [`crate::pd`]).
    PdReportColumnar = 0x0308,
    /// `Pd::Status` — what a **running** placement driver is doing right now: the operators it
    /// has in flight, which are memory and are therefore invisible to `esker pd inspect`
    /// ([`crate::pd`]).
    PdStatus = 0x0309,
    /// `Pd::ScanRegions` — a page of the routing table in **key** order, so a tool that wants
    /// every region does not ask `GetRegion` once per region ([`crate::pd`]).
    PdScanRegions = 0x030a,
    /// `Pd::Raft` — a tick's worth of Raft messages between two placement drivers
    /// ([ADR 0059](../../docs/adr/0059-pd-is-a-raft-group.md), [`crate::pd::PdRaftBatch`]).
    ///
    /// On the `Pd` service rather than on `RaftTransport`, because the two are addressed
    /// differently and a store must not be able to receive one: `RaftTransport` carries a
    /// **region's** messages, with a region id and an epoch, and a placement driver's group is not
    /// a region. Anything routing or metering on the service byte can therefore tell a cluster's
    /// consensus traffic from its placement driver's without decoding a body.
    PdRaft = 0x030b,
    /// `Pd::Members` — who is in this placement driver's group and which member leads
    /// ([ADR 0059](../../docs/adr/0059-pd-is-a-raft-group.md)).
    ///
    /// Answered by **any** member, leader or not, which is the whole point: an operator reaches
    /// for it exactly when the leader is the thing that is missing. Its own method rather than a
    /// field added to `Pd::Status`, because that message's bytes are frozen by a golden and this
    /// is an addition rather than a change to what is already on the wire.
    PdMembers = 0x030c,
    /// `Pd::MemberChange` — add or remove a placement driver, one step at a time
    /// ([ADR 0060](../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
    ///
    /// **One step**, and the caller loops. Adding a member is three things — propose a learner,
    /// wait for it to catch up, promote it — and a single call that did all three would hold a
    /// request open across a catch-up that a snapshot may be part of, well past any sensible
    /// deadline. So each call does what is missing and says whether more is needed, which is also
    /// what makes an operator's retry after a `kill -9` a reconciliation rather than a mistake.
    PdMemberChange = 0x030d,

    /// `RaftTransport::Batch` — a tick's worth of Raft messages between two stores
    /// (`docs/DESIGN.md` §6, [ADR 0009](../../docs/adr/0009-the-wire-carries-the-raft-message.md)).
    RaftBatch = 0x0401,
    /// `Admin::Split` — split a region at a chosen key (`esker-cli region split`).
    AdminSplit = 0x0501,
    /// `Admin::TransferLeader` — move a region's leadership (`esker-cli region transfer-leader`).
    AdminTransferLeader = 0x0502,
    /// `Admin::Regions` — what this store hosts, for `esker-cli region ls`.
    AdminRegions = 0x0503,

    /// `RaftTransport::Snapshot` — a follower asking a leader for a region's contents.
    ///
    /// The only **streamed** method: its answer is a run of `Stream` frames rather than one
    /// `Response`. The receiver asks; the leader does not push. `esker-proto`'s streaming is a
    /// reply shape, and a pulling receiver controls its own retries
    /// (`docs/plans/phase-4.md` §13.4).
    RaftSnapshot = 0x0402,

    /// `TxnKv::Get` — read one key at a timestamp (`docs/DESIGN.md` §8, [`crate::txn`]).
    TxnGet = 0x0201,
    /// `TxnKv::Scan`.
    TxnScan = 0x0202,
    /// `TxnKv::Prewrite`.
    TxnPrewrite = 0x0203,
    /// `TxnKv::Commit`.
    TxnCommit = 0x0204,
    /// `TxnKv::Rollback`.
    TxnRollback = 0x0205,
    /// `TxnKv::ResolveLock`.
    TxnResolveLock = 0x0206,
    /// `TxnKv::Heartbeat`.
    TxnHeartbeat = 0x0207,
    /// `TxnKv::GcSafepoint`.
    TxnGcSafepoint = 0x0208,

    /// `Fragment::Evaluate` — run a plan fragment against a node's columnar copy of a region
    /// ([ADR 0022](../../docs/adr/0022-columnar-learner-replica.md), [`crate::fragment`]).
    FragmentEvaluate = 0x0601,

    /// `Schema::Fetch` — ask a store for a table's columnar record, because the asker does not
    /// host the region that holds it ([`crate::schema`],
    /// [ADR 0037](../../docs/adr/0037-a-columnar-learner-fetches-the-schema-it-cannot-read.md)).
    SchemaFetch = 0x0701,
}

/// Service byte of the system methods — version negotiation and, later, connection control.
pub const SERVICE_SYSTEM: u8 = 0x00;
/// Service byte of `RawKv` (`docs/DESIGN.md` §9, namespace `'r'`).
pub const SERVICE_RAW_KV: u8 = 0x01;
/// Service byte of `TxnKv` (`docs/DESIGN.md` §8 and §9, namespace `'x'`).
pub const SERVICE_TXN_KV: u8 = 0x02;
/// Service byte reserved for the placement driver — phase 4.
pub const SERVICE_PD: u8 = 0x03;
/// Service byte reserved for the Raft transport — phase 3.
pub const SERVICE_RAFT: u8 = 0x04;
/// Service byte of `Fragment` — asking a columnar replica to evaluate a plan fragment.
///
/// Its own service rather than a seventh `TxnKv` method, for the reason [`SERVICE_ADMIN`] gives
/// for itself: a fragment is a *plan* run on a node that may hold no voter, and anything routing
/// or metering on the service byte has to tell it from key-value work without decoding a body.
pub const SERVICE_FRAGMENT: u8 = 0x06;

/// Service byte of `Schema` — asking a store for a table's columnar record ([`crate::schema`]).
///
/// Its own service for the reason [`SERVICE_ADMIN`] gives for itself, and one more: this is the
/// only store-to-store request addressed to a **store** rather than to a region, so anything that
/// routes or meters on the service byte must be able to see that it carries no
/// [`RequestHeader`] without decoding a body.
pub const SERVICE_SCHEMA: u8 = 0x07;

/// Service byte of `Admin` — the operator-facing requests `esker-cli region` sends.
///
/// Separate from `Pd` because these are addressed to a **store**: the placement driver schedules,
/// and an operator asking for one specific thing on one specific region talks to the store that
/// leads it. Separate from `RawKv` because they are not key-value work and must not be counted as
/// it by anything watching request rates.
pub const SERVICE_ADMIN: u8 = 0x05;

impl Method {
    /// Every method this version defines.
    pub const ALL: [Self; 37] = [
        Self::Hello,
        Self::RawGet,
        Self::RawBatchGet,
        Self::RawPut,
        Self::RawBatchPut,
        Self::RawDelete,
        Self::RawDeleteRange,
        Self::RawScan,
        Self::RawCompareAndSwap,
        Self::PdBootstrap,
        Self::PdStoreHeartbeat,
        Self::PdRegionHeartbeat,
        Self::PdGetRegion,
        Self::PdAllocId,
        Self::PdTso,
        Self::PdSchemaLease,
        Self::PdReportColumnar,
        Self::PdStatus,
        Self::PdScanRegions,
        Self::PdRaft,
        Self::PdMembers,
        Self::PdMemberChange,
        Self::RaftBatch,
        Self::RaftSnapshot,
        Self::TxnGet,
        Self::TxnScan,
        Self::TxnPrewrite,
        Self::TxnCommit,
        Self::TxnRollback,
        Self::TxnResolveLock,
        Self::TxnHeartbeat,
        Self::TxnGcSafepoint,
        Self::AdminSplit,
        Self::AdminTransferLeader,
        Self::AdminRegions,
        Self::FragmentEvaluate,
        Self::SchemaFetch,
    ];

    /// The wire tag.
    #[must_use]
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    /// The method for a wire tag, or `None` for one this version does not define.
    #[must_use]
    pub fn from_u16(tag: u16) -> Option<Self> {
        match tag {
            0x0001 => Some(Self::Hello),
            0x0101 => Some(Self::RawGet),
            0x0102 => Some(Self::RawBatchGet),
            0x0103 => Some(Self::RawPut),
            0x0104 => Some(Self::RawBatchPut),
            0x0105 => Some(Self::RawDelete),
            0x0106 => Some(Self::RawDeleteRange),
            0x0107 => Some(Self::RawScan),
            0x0108 => Some(Self::RawCompareAndSwap),
            0x0301 => Some(Self::PdBootstrap),
            0x0302 => Some(Self::PdStoreHeartbeat),
            0x0303 => Some(Self::PdRegionHeartbeat),
            0x0304 => Some(Self::PdGetRegion),
            0x0305 => Some(Self::PdAllocId),
            0x0306 => Some(Self::PdTso),
            0x0307 => Some(Self::PdSchemaLease),
            0x0308 => Some(Self::PdReportColumnar),
            0x0309 => Some(Self::PdStatus),
            0x030a => Some(Self::PdScanRegions),
            0x030b => Some(Self::PdRaft),
            0x030c => Some(Self::PdMembers),
            0x030d => Some(Self::PdMemberChange),
            0x0601 => Some(Self::FragmentEvaluate),
            0x0701 => Some(Self::SchemaFetch),
            0x0401 => Some(Self::RaftBatch),
            0x0402 => Some(Self::RaftSnapshot),
            0x0201 => Some(Self::TxnGet),
            0x0202 => Some(Self::TxnScan),
            0x0203 => Some(Self::TxnPrewrite),
            0x0204 => Some(Self::TxnCommit),
            0x0205 => Some(Self::TxnRollback),
            0x0206 => Some(Self::TxnResolveLock),
            0x0207 => Some(Self::TxnHeartbeat),
            0x0208 => Some(Self::TxnGcSafepoint),
            0x0501 => Some(Self::AdminSplit),
            0x0502 => Some(Self::AdminTransferLeader),
            0x0503 => Some(Self::AdminRegions),
            _ => None,
        }
    }

    /// Whether this method's answer is a run of `Stream` frames rather than one `Response`.
    ///
    /// Exactly one method is, and the distinction is on the type rather than in a caller's head:
    /// a caller that used [`crate::Transport::call`] on a streamed method would wait for a
    /// `Response` frame that is never sent.
    #[must_use]
    pub fn is_streamed(self) -> bool {
        matches!(self, Self::RaftSnapshot)
    }

    /// Which service this method belongs to.
    #[must_use]
    pub fn service(self) -> u8 {
        (self.as_u16() >> 8) as u8
    }

    /// The name a log line or an error message uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hello => "Hello",
            Self::RawGet => "RawKv::Get",
            Self::RawBatchGet => "RawKv::BatchGet",
            Self::RawPut => "RawKv::Put",
            Self::RawBatchPut => "RawKv::BatchPut",
            Self::RawDelete => "RawKv::Delete",
            Self::RawDeleteRange => "RawKv::DeleteRange",
            Self::RawScan => "RawKv::Scan",
            Self::RawCompareAndSwap => "RawKv::CompareAndSwap",
            Self::PdBootstrap => "Pd::Bootstrap",
            Self::PdStoreHeartbeat => "Pd::StoreHeartbeat",
            Self::PdRegionHeartbeat => "Pd::RegionHeartbeat",
            Self::PdGetRegion => "Pd::GetRegion",
            Self::PdAllocId => "Pd::AllocId",
            Self::PdTso => "Pd::Tso",
            Self::PdReportColumnar => "Pd::ReportColumnar",
            Self::PdStatus => "Pd::Status",
            Self::PdScanRegions => "Pd::ScanRegions",
            Self::PdRaft => "Pd::Raft",
            Self::PdMembers => "Pd::Members",
            Self::PdMemberChange => "Pd::MemberChange",
            Self::FragmentEvaluate => "Fragment::Evaluate",
            Self::SchemaFetch => "Schema::Fetch",
            Self::PdSchemaLease => "Pd::SchemaLease",
            Self::AdminSplit => "Admin::Split",
            Self::AdminTransferLeader => "Admin::TransferLeader",
            Self::AdminRegions => "Admin::Regions",
            Self::RaftBatch => "RaftTransport::Batch",
            Self::RaftSnapshot => "RaftTransport::Snapshot",
            Self::TxnGet => "TxnKv::Get",
            Self::TxnScan => "TxnKv::Scan",
            Self::TxnPrewrite => "TxnKv::Prewrite",
            Self::TxnCommit => "TxnKv::Commit",
            Self::TxnRollback => "TxnKv::Rollback",
            Self::TxnResolveLock => "TxnKv::ResolveLock",
            Self::TxnHeartbeat => "TxnKv::Heartbeat",
            Self::TxnGcSafepoint => "TxnKv::GcSafepoint",
        }
    }

    /// Whether this method may change stored state.
    ///
    /// The client needs it to decide whether an ambiguous failure may be retried: repeating a
    /// read costs a round trip, repeating a write may duplicate it
    /// ([`crate::RequestOutcome`]).
    #[must_use]
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::RawPut
                | Self::RawBatchPut
                | Self::RawDelete
                | Self::RawDeleteRange
                | Self::RawCompareAndSwap
                | Self::PdBootstrap
                | Self::PdStoreHeartbeat
                | Self::PdRegionHeartbeat
                | Self::PdAllocId
                | Self::PdTso
                | Self::TxnPrewrite
                | Self::TxnCommit
                | Self::TxnRollback
                | Self::TxnResolveLock
                | Self::TxnHeartbeat
                | Self::TxnGcSafepoint
        )
    }

    /// Whether this method belongs to the placement driver's service.
    #[must_use]
    pub fn is_pd(self) -> bool {
        self.service() == SERVICE_PD
    }

    /// Whether this method belongs to the fragment service ([`crate::fragment`]).
    #[must_use]
    pub fn is_fragment(self) -> bool {
        self.service() == SERVICE_FRAGMENT
    }

    /// Whether this method belongs to the transaction service.
    #[must_use]
    pub fn is_txn_kv(self) -> bool {
        self.service() == SERVICE_TXN_KV
    }

    /// Whether this method belongs to the schema service ([`crate::schema`]).
    #[must_use]
    pub fn is_schema(self) -> bool {
        self.service() == SERVICE_SCHEMA
    }
}

/// `{ region_id, epoch, peer }` — on every key-value request (`docs/DESIGN.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestHeader {
    /// The region the client believes owns the key.
    pub region_id: u64,
    /// The epoch the client believes that region is at.
    pub epoch: Epoch,
    /// The peer the client believes is the leader. Zero means "no opinion".
    pub peer: u64,
}

impl RequestHeader {
    /// A header addressed to one region at one epoch.
    #[must_use]
    pub fn new(region_id: u64, epoch: Epoch, peer: u64) -> Self {
        Self {
            region_id,
            epoch,
            peer,
        }
    }

    fn encode(self, out: &mut Encoder) {
        out.put_varint(self.region_id);
        self.epoch.encode(out);
        out.put_varint(self.peer);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            region_id: input.get_varint("header.region_id")?,
            epoch: Epoch::decode(input)?,
            peer: input.get_varint("header.peer")?,
        })
    }
}

/// The first message on a connection: which protocol version the caller speaks.
///
/// **This layout can never change.** It is the one message a peer at any version must be able
/// to read, because reading it is how a version mismatch is turned into
/// [`crate::ProtoError::WireVersion`] rather than into a hang or a corruption error. The
/// version is a fixed four-byte little-endian integer for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// The version the caller speaks — [`crate::WIRE_VERSION`] for this build.
    pub version: u32,
}

impl Hello {
    /// A hello announcing this build's version.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: crate::WIRE_VERSION,
        }
    }
}

/// The server's answer to a [`Hello`] it accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloAck {
    /// The version the server speaks, which equals the one the client offered.
    pub version: u32,
    /// Which store answered. A client logs it, and from phase 4 checks it against what the
    /// placement driver said.
    pub store_id: u64,
    /// The largest frame this server will accept, so a client can refuse an oversized request
    /// before spending a round trip on it.
    pub max_frame_size: u64,
}

/// A `RawKv` request (`docs/DESIGN.md` §9, namespace `'r'`).
///
/// The constructors set `sync: true`. `CLAUDE.md` invariant 1 makes the un-durable
/// acknowledgement the thing a caller opts into, so the flag is explicit on the wire and
/// durable by default; [`RawKvReq::unsynced`] is the opt-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawKvReq {
    /// One key.
    Get {
        /// The key, without a namespace prefix.
        key: Bytes,
    },
    /// Several keys in one round trip.
    BatchGet {
        /// The keys, in the order the answers come back in.
        keys: Vec<Bytes>,
    },
    /// Write one key.
    Put {
        /// The key.
        key: Bytes,
        /// The value.
        value: Bytes,
        /// Wait for the write to be durable before answering.
        sync: bool,
    },
    /// Write several keys atomically — one engine `WriteBatch`.
    BatchPut {
        /// The pairs to write.
        pairs: Vec<(Bytes, Bytes)>,
        /// Wait for the write to be durable before answering.
        sync: bool,
    },
    /// Remove one key.
    Delete {
        /// The key.
        key: Bytes,
        /// Wait for the write to be durable before answering.
        sync: bool,
    },
    /// Remove everything in `[start, end)`. See ADR 0006 for what this version can promise.
    DeleteRange {
        /// Inclusive start.
        start: Bytes,
        /// Exclusive end; empty means "to the end of the region".
        end: Bytes,
        /// Wait for the write to be durable before answering.
        sync: bool,
    },
    /// Read a range of keys in order.
    Scan {
        /// Inclusive start of the range, or — when `reverse` — the exclusive upper bound to
        /// walk down from.
        start: Bytes,
        /// Exclusive end; empty means "to the end of the region".
        end: Bytes,
        /// Most pairs to return. Zero means [`DEFAULT_SCAN_LIMIT`]; the server caps it either
        /// way, so a scan is always a bounded answer.
        limit: u32,
        /// Walk from the high end of the range towards the low one.
        reverse: bool,
    },
    /// Write `value` only if the key currently holds `expected`.
    ///
    /// `expected: None` means "only if the key is absent"; `value: None` means "delete it".
    CompareAndSwap {
        /// The key.
        key: Bytes,
        /// What the key must currently hold, or `None` for "must be absent".
        expected: Option<Bytes>,
        /// What to write, or `None` to delete.
        value: Option<Bytes>,
        /// Wait for the write to be durable before answering.
        sync: bool,
    },
}

impl RawKvReq {
    /// A durable single-key write.
    #[must_use]
    pub fn put(key: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        Self::Put {
            key: key.into(),
            value: value.into(),
            sync: true,
        }
    }

    /// A durable atomic multi-key write.
    #[must_use]
    pub fn batch_put(pairs: Vec<(Bytes, Bytes)>) -> Self {
        Self::BatchPut { pairs, sync: true }
    }

    /// A durable single-key delete.
    #[must_use]
    pub fn delete(key: impl Into<Bytes>) -> Self {
        Self::Delete {
            key: key.into(),
            sync: true,
        }
    }

    /// A durable range delete.
    #[must_use]
    pub fn delete_range(start: impl Into<Bytes>, end: impl Into<Bytes>) -> Self {
        Self::DeleteRange {
            start: start.into(),
            end: end.into(),
            sync: true,
        }
    }

    /// A durable compare-and-swap.
    #[must_use]
    pub fn compare_and_swap(
        key: impl Into<Bytes>,
        expected: Option<Bytes>,
        value: Option<Bytes>,
    ) -> Self {
        Self::CompareAndSwap {
            key: key.into(),
            expected,
            value,
            sync: true,
        }
    }

    /// A point read.
    #[must_use]
    pub fn get(key: impl Into<Bytes>) -> Self {
        Self::Get { key: key.into() }
    }

    /// A forward scan.
    #[must_use]
    pub fn scan(start: impl Into<Bytes>, end: impl Into<Bytes>, limit: u32) -> Self {
        Self::Scan {
            start: start.into(),
            end: end.into(),
            limit,
            reverse: false,
        }
    }

    /// The same request, acknowledged before its bytes are durable.
    ///
    /// The deliberate opt-out of `CLAUDE.md` invariant 1. On a read it does nothing.
    #[must_use]
    pub fn unsynced(mut self) -> Self {
        match &mut self {
            Self::Put { sync, .. }
            | Self::BatchPut { sync, .. }
            | Self::Delete { sync, .. }
            | Self::DeleteRange { sync, .. }
            | Self::CompareAndSwap { sync, .. } => *sync = false,
            Self::Get { .. } | Self::BatchGet { .. } | Self::Scan { .. } => {}
        }
        self
    }

    /// Whether this request waits for durability. Reads are never `sync`.
    #[must_use]
    pub fn is_sync(&self) -> bool {
        match self {
            Self::Put { sync, .. }
            | Self::BatchPut { sync, .. }
            | Self::Delete { sync, .. }
            | Self::DeleteRange { sync, .. }
            | Self::CompareAndSwap { sync, .. } => *sync,
            Self::Get { .. } | Self::BatchGet { .. } | Self::Scan { .. } => false,
        }
    }

    /// The method this request is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Get { .. } => Method::RawGet,
            Self::BatchGet { .. } => Method::RawBatchGet,
            Self::Put { .. } => Method::RawPut,
            Self::BatchPut { .. } => Method::RawBatchPut,
            Self::Delete { .. } => Method::RawDelete,
            Self::DeleteRange { .. } => Method::RawDeleteRange,
            Self::Scan { .. } => Method::RawScan,
            Self::CompareAndSwap { .. } => Method::RawCompareAndSwap,
        }
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Get { key } => out.put_bytes(key),
            Self::BatchGet { keys } => {
                out.put_varint(keys.len() as u64);
                for key in keys {
                    out.put_bytes(key);
                }
            }
            Self::Put { key, value, sync } => {
                out.put_bytes(key);
                out.put_bytes(value);
                out.put_bool(*sync);
            }
            Self::BatchPut { pairs, sync } => {
                out.put_varint(pairs.len() as u64);
                for (key, value) in pairs {
                    out.put_bytes(key);
                    out.put_bytes(value);
                }
                out.put_bool(*sync);
            }
            Self::Delete { key, sync } => {
                out.put_bytes(key);
                out.put_bool(*sync);
            }
            Self::DeleteRange { start, end, sync } => {
                out.put_bytes(start);
                out.put_bytes(end);
                out.put_bool(*sync);
            }
            Self::Scan {
                start,
                end,
                limit,
                reverse,
            } => {
                out.put_bytes(start);
                out.put_bytes(end);
                out.put_varint(u64::from(*limit));
                out.put_bool(*reverse);
            }
            Self::CompareAndSwap {
                key,
                expected,
                value,
                sync,
            } => {
                out.put_bytes(key);
                out.put_opt_bytes(expected.as_deref());
                out.put_opt_bytes(value.as_deref());
                out.put_bool(*sync);
            }
        }
    }

    fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let request = match method {
            Method::RawGet => Self::Get {
                key: take(input, "key")?,
            },
            Method::RawBatchGet => {
                let count = input.get_count("keys")?;
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(take(input, "key")?);
                }
                Self::BatchGet { keys }
            }
            Method::RawPut => Self::Put {
                key: take(input, "key")?,
                value: take(input, "value")?,
                sync: input.get_bool("sync")?,
            },
            Method::RawBatchPut => {
                let count = input.get_count("pairs")?;
                let mut pairs = Vec::with_capacity(count);
                for _ in 0..count {
                    pairs.push((take(input, "key")?, take(input, "value")?));
                }
                Self::BatchPut {
                    pairs,
                    sync: input.get_bool("sync")?,
                }
            }
            Method::RawDelete => Self::Delete {
                key: take(input, "key")?,
                sync: input.get_bool("sync")?,
            },
            Method::RawDeleteRange => Self::DeleteRange {
                start: take(input, "start")?,
                end: take(input, "end")?,
                sync: input.get_bool("sync")?,
            },
            Method::RawScan => Self::Scan {
                start: take(input, "start")?,
                end: take(input, "end")?,
                limit: input.get_varint_u32("limit")?,
                reverse: input.get_bool("reverse")?,
            },
            Method::RawCompareAndSwap => Self::CompareAndSwap {
                key: take(input, "key")?,
                expected: take_opt(input, "expected")?,
                value: take_opt(input, "value")?,
                sync: input.get_bool("sync")?,
            },
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a RawKv method", other.name()),
                ));
            }
        };
        Ok(request)
    }
}

/// The answer to a [`RawKvReq`]. One variant per method, and the method is echoed in the
/// response's tag, so a response decodes without looking up what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawKvResp {
    /// The value, or `None` when the key is absent.
    Get {
        /// What was stored.
        value: Option<Bytes>,
    },
    /// One answer per requested key, in the order they were asked.
    BatchGet {
        /// The values, `None` where the key was absent.
        values: Vec<Option<Bytes>>,
    },
    /// The write is done.
    Put,
    /// The batch is done.
    BatchPut,
    /// The delete is done.
    Delete,
    /// The range is gone.
    DeleteRange {
        /// How many keys were removed.
        deleted: u64,
    },
    /// The pairs found, in key order — reversed when the scan was.
    Scan {
        /// Key-value pairs.
        pairs: Vec<(Bytes, Bytes)>,
    },
    /// Whether the swap happened, and what the key held when it was read.
    CompareAndSwap {
        /// True when `expected` matched and the write went through.
        swapped: bool,
        /// What the key actually held, which is why the swap failed when it did.
        previous: Option<Bytes>,
    },
}

impl RawKvResp {
    /// The method this is a response to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Get { .. } => Method::RawGet,
            Self::BatchGet { .. } => Method::RawBatchGet,
            Self::Put => Method::RawPut,
            Self::BatchPut => Method::RawBatchPut,
            Self::Delete => Method::RawDelete,
            Self::DeleteRange { .. } => Method::RawDeleteRange,
            Self::Scan { .. } => Method::RawScan,
            Self::CompareAndSwap { .. } => Method::RawCompareAndSwap,
        }
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Get { value } => out.put_opt_bytes(value.as_deref()),
            Self::BatchGet { values } => {
                out.put_varint(values.len() as u64);
                for value in values {
                    out.put_opt_bytes(value.as_deref());
                }
            }
            Self::Put | Self::BatchPut | Self::Delete => {}
            Self::DeleteRange { deleted } => out.put_varint(*deleted),
            Self::Scan { pairs } => {
                out.put_varint(pairs.len() as u64);
                for (key, value) in pairs {
                    out.put_bytes(key);
                    out.put_bytes(value);
                }
            }
            Self::CompareAndSwap { swapped, previous } => {
                out.put_bool(*swapped);
                out.put_opt_bytes(previous.as_deref());
            }
        }
    }

    fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let response = match method {
            Method::RawGet => Self::Get {
                value: take_opt(input, "value")?,
            },
            Method::RawBatchGet => {
                let count = input.get_count("values")?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(take_opt(input, "value")?);
                }
                Self::BatchGet { values }
            }
            Method::RawPut => Self::Put,
            Method::RawBatchPut => Self::BatchPut,
            Method::RawDelete => Self::Delete,
            Method::RawDeleteRange => Self::DeleteRange {
                deleted: input.get_varint("deleted")?,
            },
            Method::RawScan => {
                let count = input.get_count("pairs")?;
                let mut pairs = Vec::with_capacity(count);
                for _ in 0..count {
                    pairs.push((take(input, "key")?, take(input, "value")?));
                }
                Self::Scan { pairs }
            }
            Method::RawCompareAndSwap => Self::CompareAndSwap {
                swapped: input.get_bool("swapped")?,
                previous: take_opt(input, "previous")?,
            },
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a RawKv method", other.name()),
                ));
            }
        };
        Ok(response)
    }
}

/// Anything a client sends as a `Request` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Version negotiation. Handled by the connection itself and never reaches a service.
    Hello(Hello),
    /// A key-value request, with the region it is addressed to.
    RawKv {
        /// Which region, at which epoch, on which peer.
        header: RequestHeader,
        /// What to do.
        request: RawKvReq,
    },
    /// A transactional request, with the region it is addressed to
    /// (`docs/DESIGN.md` §8, [`crate::txn`]).
    TxnKv {
        /// Which region, at which epoch, on which peer.
        header: RequestHeader,
        /// What to do.
        request: crate::txn::TxnKvReq,
    },
    /// A question for the placement driver. It carries a cluster id rather than a
    /// [`RequestHeader`]: PD's answers are about the routing table itself, so there is no
    /// region to address, and the id is what stops one cluster answering for another
    /// ([`crate::pd`]).
    Pd {
        /// The cluster the caller believes it is talking to. Zero means "not known yet",
        /// which only `Bootstrap` may send.
        cluster_id: u64,
        /// What to ask.
        request: crate::pd::PdReq,
    },
    /// A plan fragment for a columnar replica, with the region it is addressed to
    /// ([`crate::fragment`], [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md)).
    Fragment {
        /// Which region, at which epoch, on which peer. Invariant 5 applies here as to a row
        /// read, which is why the fragment body carries no epoch of its own.
        header: RequestHeader,
        /// What to evaluate.
        request: crate::fragment::FragmentReq,
    },
    /// Raft traffic between two stores. It carries no [`RequestHeader`], because one batch may
    /// hold messages for many regions and each carries its own (`docs/DESIGN.md` §6).
    Raft(RaftBatch),
    /// A follower asking for a region's contents, having been told by an `InstallSnapshot`
    /// message that its log no longer reaches back far enough.
    ///
    /// The answer is a **stream**, not a response frame: a region is megabytes and a Raft
    /// message is not where megabytes go.
    Snapshot(SnapshotRequest),
    /// An operator asking a store to do one specific thing (`esker-cli region`).
    ///
    /// It carries no [`RequestHeader`]: an operator names a region by id and does not hold an
    /// epoch to be checked against — the store checks what it can and refuses what it cannot.
    Admin(AdminReq),
    /// One store asking another for a table's columnar record ([`crate::schema`]).
    ///
    /// It carries no [`RequestHeader`] either, and for a sharper reason than `Admin`'s: the asker
    /// does not know which region covers the record — that is what it is asking about — so an
    /// epoch it invented would be checked against a region it never routed to.
    Schema(crate::schema::SchemaReq),
}

/// What an operator asks a store to do (`docs/DESIGN.md` §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminReq {
    /// Split a region at a chosen key, rather than waiting for it to grow past the threshold.
    Split {
        /// Which region.
        region_id: u64,
        /// Where to cut. Refused if it is not strictly inside the region.
        split_key: Bytes,
    },
    /// Move a region's leadership to one of its peers.
    TransferLeader {
        /// Which region.
        region_id: u64,
        /// Which peer should take office.
        to_peer_id: u64,
    },
    /// Every region this store hosts, with what it knows about each.
    Regions,
}

impl AdminReq {
    /// The method this is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Split { .. } => Method::AdminSplit,
            Self::TransferLeader { .. } => Method::AdminTransferLeader,
            Self::Regions => Method::AdminRegions,
        }
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Split {
                region_id,
                split_key,
            } => {
                out.put_varint(*region_id);
                out.put_bytes(split_key);
            }
            Self::TransferLeader {
                region_id,
                to_peer_id,
            } => {
                out.put_varint(*region_id);
                out.put_varint(*to_peer_id);
            }
            Self::Regions => {}
        }
    }

    fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match method {
            Method::AdminSplit => Self::Split {
                region_id: input.get_varint("admin.region_id")?,
                split_key: Bytes::copy_from_slice(input.get_bytes("admin.split_key")?),
            },
            Method::AdminTransferLeader => Self::TransferLeader {
                region_id: input.get_varint("admin.region_id")?,
                to_peer_id: input.get_varint("admin.to_peer_id")?,
            },
            Method::AdminRegions => Self::Regions,
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not an Admin method", other.name()),
                ));
            }
        })
    }
}

/// One region as a store reports it to an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionStatus {
    /// The region: range, peers and epoch.
    pub region: Region,
    /// The peer this store believes leads it; `0` for "no opinion".
    pub leader_peer_id: u64,
    /// Whether *this* store leads it.
    pub is_leader: bool,
    /// Roughly how many bytes it holds.
    pub approximate_size: u64,
    /// How far its state machine has applied.
    pub applied_index: u64,
}

/// A follower's request for a region's contents (`docs/DESIGN.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRequest {
    /// The region wanted.
    pub region_id: u64,
    /// The snapshot index the asking peer was told about. The sender refuses if it cannot
    /// produce one at least this recent — a snapshot older than the announcement would leave
    /// the follower's log with a hole between the two.
    pub index: u64,
    /// Which peer is asking, so a store that is not a member of the region is refused rather
    /// than served a copy of data it has no claim to.
    pub peer_id: u64,
}

impl Request {
    /// A `RawKv` request addressed to a region.
    #[must_use]
    pub fn raw_kv(header: RequestHeader, request: RawKvReq) -> Self {
        Self::RawKv { header, request }
    }

    /// A `TxnKv` request addressed to a region.
    #[must_use]
    pub fn txn_kv(header: RequestHeader, request: crate::txn::TxnKvReq) -> Self {
        Self::TxnKv { header, request }
    }

    /// The method this request is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::RawKv { request, .. } => request.method(),
            Self::TxnKv { request, .. } => request.method(),
            Self::Pd { request, .. } => request.method(),
            Self::Fragment { request, .. } => request.method(),
            Self::Raft(_) => Method::RaftBatch,
            Self::Snapshot(_) => Method::RaftSnapshot,
            Self::Admin(request) => request.method(),
            Self::Schema(_) => Method::SchemaFetch,
        }
    }

    /// The region header, for the requests that have one.
    #[must_use]
    pub fn header(&self) -> Option<RequestHeader> {
        match self {
            Self::Hello(_)
            | Self::Raft(_)
            | Self::Snapshot(_)
            | Self::Admin(_)
            | Self::Schema(_)
            | Self::Pd { .. } => None,
            Self::RawKv { header, .. }
            | Self::TxnKv { header, .. }
            | Self::Fragment { header, .. } => Some(*header),
        }
    }

    /// The body of a `Request` frame: `method:u16 ++ header ++ fields`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.put_u16(self.method().as_u16());
        match self {
            Self::Hello(hello) => out.put_u32(hello.version),
            Self::RawKv { header, request } => {
                header.encode(&mut out);
                request.encode(&mut out);
            }
            Self::TxnKv { header, request } => {
                header.encode(&mut out);
                request.encode(&mut out);
            }
            Self::Pd {
                cluster_id,
                request,
            } => {
                out.put_varint(*cluster_id);
                request.encode(&mut out);
            }
            Self::Fragment { header, request } => {
                header.encode(&mut out);
                request.encode(&mut out);
            }
            Self::Raft(batch) => batch.encode(&mut out),
            Self::Snapshot(request) => {
                out.put_varint(request.region_id);
                out.put_varint(request.index);
                out.put_varint(request.peer_id);
            }
            Self::Admin(request) => request.encode(&mut out),
            Self::Schema(request) => request.encode(&mut out),
        }
        out.finish()
    }

    /// Reads a `Request` frame's body. An unknown method and trailing bytes are both errors.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(body);
        let method = read_method(&mut input)?;
        let request = match method {
            Method::Hello => Self::Hello(Hello {
                version: input.get_u32("hello.version")?,
            }),
            Method::RaftBatch => Self::Raft(RaftBatch::decode(&mut input)?),
            method if method.service() == SERVICE_ADMIN => {
                Self::Admin(AdminReq::decode(method, &mut input)?)
            }
            Method::SchemaFetch => Self::Schema(crate::schema::SchemaReq::decode(&mut input)?),
            Method::RaftSnapshot => Self::Snapshot(SnapshotRequest {
                region_id: input.get_varint("snapshot.region_id")?,
                index: input.get_varint("snapshot.index")?,
                peer_id: input.get_varint("snapshot.peer_id")?,
            }),
            other if other.is_pd() => Self::Pd {
                cluster_id: input.get_varint("pd.cluster_id")?,
                request: crate::pd::PdReq::decode(other, &mut input)?,
            },
            other if other.is_fragment() => {
                let header = RequestHeader::decode(&mut input)?;
                Self::Fragment {
                    header,
                    request: crate::fragment::FragmentReq::decode(&mut input)?,
                }
            }
            other if other.is_txn_kv() => {
                let header = RequestHeader::decode(&mut input)?;
                Self::TxnKv {
                    header,
                    request: crate::txn::TxnKvReq::decode(other, &mut input)?,
                }
            }
            other => {
                let header = RequestHeader::decode(&mut input)?;
                Self::RawKv {
                    header,
                    request: RawKvReq::decode(other, &mut input)?,
                }
            }
        };
        input.finish()?;
        Ok(request)
    }
}

/// Anything a server sends as a `Response` frame. Failures are `Error` frames instead
/// ([`crate::ProtoError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The accepted version negotiation.
    Hello(HelloAck),
    /// The answer to a key-value request.
    RawKv(RawKvResp),
    /// The answer to a transactional request ([`crate::txn`]).
    TxnKv(crate::txn::TxnKvResp),
    /// The placement driver's answer ([`crate::pd`]).
    Pd(crate::pd::PdResp),
    /// A Raft batch was received. It carries nothing: Raft's own retries are what make a lost
    /// message survivable, so there is no outcome for the sender to act on
    /// ([`RaftTransport`](crate::raft) is fire-and-forget by design).
    Raft,
    /// A columnar replica's answer to a fragment ([`crate::fragment`]).
    Fragment(crate::fragment::FragmentResp),
    /// The answer to an operator's request.
    Admin(AdminResp),
    /// A store's answer to a schema fetch: the record's bytes, or nothing ([`crate::schema`]).
    Schema(crate::schema::SchemaResp),
}

/// What a store answers an operator with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminResp {
    /// The split was proposed and applied. It carries the two halves, so an operator sees what it
    /// got rather than having to ask again.
    Split {
        /// The parent, narrowed.
        left: Region,
        /// The half that was created.
        right: Region,
    },
    /// Leadership was *asked* to move. It carries nothing, because what completes a transfer is
    /// an election and the answer would be a guess (`docs/DESIGN.md` §6).
    TransferLeader,
    /// What this store hosts.
    Regions {
        /// One entry per region, in key order.
        regions: Vec<RegionStatus>,
    },
}

impl AdminResp {
    /// The method this answers.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Split { .. } => Method::AdminSplit,
            Self::TransferLeader => Method::AdminTransferLeader,
            Self::Regions { .. } => Method::AdminRegions,
        }
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Split { left, right } => {
                left.encode(out);
                right.encode(out);
            }
            Self::TransferLeader => {}
            Self::Regions { regions } => {
                out.put_varint(regions.len() as u64);
                for status in regions {
                    status.region.encode(out);
                    out.put_varint(status.leader_peer_id);
                    out.put_bool(status.is_leader);
                    out.put_varint(status.approximate_size);
                    out.put_varint(status.applied_index);
                }
            }
        }
    }

    fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match method {
            Method::AdminSplit => Self::Split {
                left: Region::decode(input)?,
                right: Region::decode(input)?,
            },
            Method::AdminTransferLeader => Self::TransferLeader,
            Method::AdminRegions => {
                let count = input.get_count("admin.regions")?;
                let mut regions = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    regions.push(RegionStatus {
                        region: Region::decode(input)?,
                        leader_peer_id: input.get_varint("admin.leader")?,
                        is_leader: input.get_bool("admin.is_leader")?,
                        approximate_size: input.get_varint("admin.size")?,
                        applied_index: input.get_varint("admin.applied")?,
                    });
                }
                Self::Regions { regions }
            }
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not an Admin method", other.name()),
                ));
            }
        })
    }
}

impl Response {
    /// The method this is a response to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::RawKv(response) => response.method(),
            Self::TxnKv(response) => response.method(),
            Self::Pd(response) => response.method(),
            Self::Fragment(_) => Method::FragmentEvaluate,
            Self::Schema(_) => Method::SchemaFetch,
            Self::Raft => Method::RaftBatch,
            Self::Admin(response) => response.method(),
        }
    }

    /// The `RawKv` answer, or an [`crate::ProtoError::InvalidRequest`] naming what came back
    /// instead. Every caller that asked a `RawKv` question wants exactly this.
    pub fn into_raw_kv(self) -> Result<RawKvResp, crate::ProtoError> {
        match self {
            Self::RawKv(response) => Ok(response),
            other => Err(crate::ProtoError::invalid(format!(
                "expected a RawKv response, got {}",
                other.method().name()
            ))),
        }
    }

    /// The `TxnKv` answer, or an [`crate::ProtoError::InvalidRequest`] naming what came back
    /// instead.
    pub fn into_txn_kv(self) -> Result<crate::txn::TxnKvResp, crate::ProtoError> {
        match self {
            Self::TxnKv(response) => Ok(response),
            other => Err(crate::ProtoError::invalid(format!(
                "expected a TxnKv response, got {}",
                other.method().name()
            ))),
        }
    }

    /// The body of a `Response` frame: `method:u16 ++ fields`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.put_u16(self.method().as_u16());
        match self {
            Self::Hello(ack) => {
                out.put_u32(ack.version);
                out.put_varint(ack.store_id);
                out.put_varint(ack.max_frame_size);
            }
            Self::RawKv(response) => response.encode(&mut out),
            Self::TxnKv(response) => response.encode(&mut out),
            Self::Pd(response) => response.encode(&mut out),
            Self::Fragment(response) => response.encode(&mut out),
            Self::Schema(response) => response.encode(&mut out),
            // The acknowledgement carries nothing: Raft's own retries are what make a lost
            // message survivable, so there is no outcome for the sender to act on.
            Self::Raft => {}
            Self::Admin(response) => response.encode(&mut out),
        }
        out.finish()
    }

    /// Reads a `Response` frame's body.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(body);
        let method = read_method(&mut input)?;
        let response = match method {
            Method::RaftBatch => Self::Raft,
            method if method.service() == SERVICE_ADMIN => {
                Self::Admin(AdminResp::decode(method, &mut input)?)
            }
            Method::Hello => Self::Hello(HelloAck {
                version: input.get_u32("hello.version")?,
                store_id: input.get_varint("hello.store_id")?,
                max_frame_size: input.get_varint("hello.max_frame_size")?,
            }),
            other if other.is_pd() => Self::Pd(crate::pd::PdResp::decode(other, &mut input)?),
            other if other.is_fragment() => {
                Self::Fragment(crate::fragment::FragmentResp::decode(&mut input)?)
            }
            other if other.is_schema() => {
                Self::Schema(crate::schema::SchemaResp::decode(&mut input)?)
            }
            other if other.is_txn_kv() => {
                Self::TxnKv(crate::txn::TxnKvResp::decode(other, &mut input)?)
            }
            other => Self::RawKv(RawKvResp::decode(other, &mut input)?),
        };
        input.finish()?;
        Ok(response)
    }
}

fn read_method(input: &mut Decoder<'_>) -> Result<Method, DecodeError> {
    let tag = input.get_u16("method")?;
    Method::from_u16(tag).ok_or(DecodeError::UnknownTag {
        what: "method",
        tag: u64::from(tag),
    })
}

/// A length-prefixed byte string, copied out of the body.
///
/// The copy is what turns a borrowed decode into an owned message. It could be avoided by
/// slicing the frame's `Bytes`, which would need the decoder to carry one; that is a change to
/// make with a profile in hand, not before (`CLAUDE.md`, "we optimize after a profile").
fn take(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, DecodeError> {
    Ok(Bytes::copy_from_slice(input.get_bytes(field)?))
}

fn take_opt(input: &mut Decoder<'_>, field: &'static str) -> Result<Option<Bytes>, DecodeError> {
    Ok(input.get_opt_bytes(field)?.map(Bytes::copy_from_slice))
}

#[cfg(test)]
mod tests {
    use super::{
        Hello, HelloAck, Method, RawKvReq, RawKvResp, Request, RequestHeader, Response,
        SERVICE_ADMIN, SERVICE_RAW_KV, SERVICE_SYSTEM, SERVICE_TXN_KV,
    };
    use crate::region::Epoch;
    use bytes::Bytes;

    fn header() -> RequestHeader {
        RequestHeader::new(1, Epoch::new(2, 3), 4)
    }

    fn requests() -> Vec<Request> {
        let header = header();
        vec![
            Request::Hello(Hello::current()),
            Request::raw_kv(header, RawKvReq::get(b"key".as_slice())),
            Request::raw_kv(header, RawKvReq::Get { key: Bytes::new() }),
            Request::raw_kv(
                header,
                RawKvReq::BatchGet {
                    keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
                },
            ),
            Request::raw_kv(header, RawKvReq::BatchGet { keys: Vec::new() }),
            Request::raw_kv(header, RawKvReq::put(b"k".as_slice(), b"v".as_slice())),
            Request::raw_kv(
                header,
                RawKvReq::put(b"k".as_slice(), b"v".as_slice()).unsynced(),
            ),
            Request::raw_kv(
                header,
                RawKvReq::batch_put(vec![(Bytes::from_static(b"a"), Bytes::from_static(b"1"))]),
            ),
            Request::raw_kv(header, RawKvReq::delete(b"k".as_slice())),
            Request::raw_kv(
                header,
                RawKvReq::delete_range(b"a".as_slice(), b"".as_slice()),
            ),
            Request::raw_kv(
                header,
                RawKvReq::scan(b"a".as_slice(), b"z".as_slice(), 100),
            ),
            Request::raw_kv(
                header,
                RawKvReq::Scan {
                    start: Bytes::from_static(b"z"),
                    end: Bytes::new(),
                    limit: 0,
                    reverse: true,
                },
            ),
            Request::raw_kv(
                header,
                RawKvReq::compare_and_swap(
                    b"k".as_slice(),
                    Some(Bytes::from_static(b"old")),
                    Some(Bytes::from_static(b"new")),
                ),
            ),
            Request::raw_kv(
                header,
                RawKvReq::compare_and_swap(b"k".as_slice(), None, None),
            ),
        ]
    }

    fn responses() -> Vec<Response> {
        vec![
            Response::Hello(HelloAck {
                version: crate::WIRE_VERSION,
                store_id: 1,
                max_frame_size: crate::MAX_FRAME_SIZE as u64,
            }),
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::from_static(b"v")),
            }),
            Response::RawKv(RawKvResp::Get { value: None }),
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::new()),
            }),
            Response::RawKv(RawKvResp::BatchGet {
                values: vec![Some(Bytes::from_static(b"a")), None],
            }),
            Response::RawKv(RawKvResp::Put),
            Response::RawKv(RawKvResp::BatchPut),
            Response::RawKv(RawKvResp::Delete),
            Response::RawKv(RawKvResp::DeleteRange { deleted: 17 }),
            Response::RawKv(RawKvResp::Scan {
                pairs: vec![(Bytes::from_static(b"a"), Bytes::from_static(b"1"))],
            }),
            Response::RawKv(RawKvResp::Scan { pairs: Vec::new() }),
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: false,
                previous: Some(Bytes::from_static(b"actual")),
            }),
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: true,
                previous: None,
            }),
        ]
    }

    #[test]
    fn every_request_round_trips() {
        for request in requests() {
            let bytes = request.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), request, "{request:?}");
        }
    }

    #[test]
    fn every_response_round_trips() {
        for response in responses() {
            let bytes = response.encode();
            assert_eq!(Response::decode(&bytes).unwrap(), response, "{response:?}");
        }
    }

    /// A response is self-describing: its tag is the method it answers, so a demultiplexer
    /// does not have to remember what was asked in order to read what came back.
    #[test]
    fn a_response_carries_the_method_it_answers() {
        for response in responses() {
            let bytes = response.encode();
            let tag = u16::from_le_bytes([bytes[0], bytes[1]]);
            assert_eq!(Method::from_u16(tag), Some(response.method()));
        }
    }

    /// Method numbers are format. A collision, or a service byte moving, breaks every peer.
    #[test]
    fn methods_are_distinct_nonzero_and_in_their_services() {
        let unique: std::collections::BTreeSet<u16> =
            Method::ALL.into_iter().map(Method::as_u16).collect();
        assert_eq!(unique.len(), Method::ALL.len(), "two methods share a tag");
        assert!(!unique.contains(&0), "zero is not a method");

        for method in Method::ALL {
            assert_eq!(Method::from_u16(method.as_u16()), Some(method));
            let service = match method {
                Method::Hello => SERVICE_SYSTEM,
                Method::RaftBatch | Method::RaftSnapshot => crate::messages::SERVICE_RAFT,
                Method::AdminSplit | Method::AdminTransferLeader | Method::AdminRegions => {
                    SERVICE_ADMIN
                }
                Method::PdBootstrap
                | Method::PdStoreHeartbeat
                | Method::PdRegionHeartbeat
                | Method::PdGetRegion
                | Method::PdAllocId
                | Method::PdTso
                | Method::PdSchemaLease
                | Method::PdReportColumnar
                | Method::PdStatus
                | Method::PdScanRegions
                | Method::PdRaft
                | Method::PdMembers
                | Method::PdMemberChange => crate::messages::SERVICE_PD,
                Method::TxnGet
                | Method::TxnScan
                | Method::TxnPrewrite
                | Method::TxnCommit
                | Method::TxnRollback
                | Method::TxnResolveLock
                | Method::TxnHeartbeat
                | Method::TxnGcSafepoint => SERVICE_TXN_KV,
                Method::FragmentEvaluate => crate::messages::SERVICE_FRAGMENT,
                Method::SchemaFetch => crate::messages::SERVICE_SCHEMA,
                _ => SERVICE_RAW_KV,
            };
            assert_eq!(method.service(), service, "{method:?}");
        }

        // `docs/DESIGN.md` §9: a service's numbers stay contiguous from 1, so a gap means a
        // method was removed rather than reserved — and a reserved number has to stay reserved.
        for service in [
            SERVICE_SYSTEM,
            SERVICE_RAW_KV,
            SERVICE_TXN_KV,
            crate::messages::SERVICE_PD,
            crate::messages::SERVICE_RAFT,
            crate::messages::SERVICE_FRAGMENT,
        ] {
            let mut numbers: Vec<u16> = Method::ALL
                .into_iter()
                .filter(|method| method.service() == service)
                .map(|method| method.as_u16() & 0x00FF)
                .collect();
            numbers.sort_unstable();
            let count = u16::try_from(numbers.len()).expect("a service has few methods");
            let expected: Vec<u16> = (1..=count).collect();
            assert_eq!(numbers, expected, "service {service:#04x} has a gap");
        }
    }

    #[test]
    fn an_unknown_method_is_an_error_not_a_skipped_frame() {
        for tag in [0x0000u16, 0x0109, 0x0200, 0xFFFF] {
            assert_eq!(Method::from_u16(tag), None, "{tag:#06x}");
            assert!(Request::decode(&tag.to_le_bytes()).is_err(), "{tag:#06x}");
            assert!(Response::decode(&tag.to_le_bytes()).is_err(), "{tag:#06x}");
        }
    }

    /// A body longer than its fields is a different message, not this one with something on
    /// the end (`docs/DESIGN.md` §9).
    #[test]
    fn trailing_bytes_are_refused() {
        for request in requests() {
            let mut bytes = request.encode();
            bytes.push(0);
            assert!(Request::decode(&bytes).is_err(), "{request:?}");
        }
        for response in responses() {
            let mut bytes = response.encode();
            bytes.push(0);
            assert!(Response::decode(&bytes).is_err(), "{response:?}");
        }
    }

    /// Whatever a socket delivers, a decode is an error or a value — never a panic
    /// (`CLAUDE.md` invariant 9).
    #[test]
    fn truncation_never_panics() {
        for request in requests() {
            let bytes = request.encode();
            for cut in 0..bytes.len() {
                assert!(Request::decode(&bytes[..cut]).is_err(), "{cut}");
            }
        }
        for response in responses() {
            let bytes = response.encode();
            for cut in 0..bytes.len() {
                assert!(Response::decode(&bytes[..cut]).is_err(), "{cut}");
            }
        }
    }

    /// `CLAUDE.md` invariant 1: the un-durable acknowledgement is what a caller opts into.
    #[test]
    fn every_write_constructor_defaults_to_durable() {
        let writes = [
            RawKvReq::put(b"k".as_slice(), b"v".as_slice()),
            RawKvReq::batch_put(vec![(Bytes::from_static(b"k"), Bytes::from_static(b"v"))]),
            RawKvReq::delete(b"k".as_slice()),
            RawKvReq::delete_range(b"a".as_slice(), b"b".as_slice()),
            RawKvReq::compare_and_swap(b"k".as_slice(), None, None),
        ];
        for write in writes {
            assert!(write.is_sync(), "{write:?} is not durable by default");
            assert!(!write.clone().unsynced().is_sync(), "{write:?}");
            assert!(write.method().is_mutation(), "{write:?}");
        }

        // Reads have no durability to wait for, and opting out of it changes nothing.
        for read in [
            RawKvReq::get(b"k".as_slice()),
            RawKvReq::BatchGet { keys: Vec::new() },
            RawKvReq::scan(b"a".as_slice(), b"z".as_slice(), 10),
        ] {
            assert!(!read.is_sync());
            assert!(!read.method().is_mutation(), "{read:?}");
            assert_eq!(read.clone().unsynced(), read);
        }
    }

    /// The `sync` flag is on the wire, not implied. A peer must not have to know a default in
    /// order to agree about durability.
    #[test]
    fn the_sync_flag_changes_the_bytes() {
        let durable = Request::raw_kv(header(), RawKvReq::put(b"k".as_slice(), b"v".as_slice()));
        let fast = Request::raw_kv(
            header(),
            RawKvReq::put(b"k".as_slice(), b"v".as_slice()).unsynced(),
        );
        assert_ne!(durable.encode(), fast.encode());
    }

    /// An absent value and an empty one are different answers to "is this key there?".
    #[test]
    fn an_absent_value_is_not_an_empty_one() {
        let absent = Response::RawKv(RawKvResp::Get { value: None }).encode();
        let empty = Response::RawKv(RawKvResp::Get {
            value: Some(Bytes::new()),
        })
        .encode();
        assert_ne!(absent, empty);
        assert_eq!(
            Response::decode(&absent).unwrap(),
            Response::RawKv(RawKvResp::Get { value: None })
        );
    }

    /// Hello is the one message every version must be able to read, because reading it is how
    /// a mismatch becomes a typed error instead of a hang.
    #[test]
    fn hello_is_a_method_tag_and_a_fixed_width_version() {
        let bytes = Request::Hello(Hello { version: 7 }).encode();
        assert_eq!(bytes.len(), 6);
        assert_eq!(&bytes[..2], &Method::Hello.as_u16().to_le_bytes());
        assert_eq!(&bytes[2..], &7u32.to_le_bytes());
    }

    #[test]
    fn a_request_reports_its_header_and_a_hello_has_none() {
        assert_eq!(
            Request::raw_kv(header(), RawKvReq::get(b"k".as_slice())).header(),
            Some(header())
        );
        assert_eq!(Request::Hello(Hello::current()).header(), None);
    }
}
