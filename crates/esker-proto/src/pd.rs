//! Service `0x03`: the placement driver's six methods, and the channel a caller drives them
//! through (`docs/DESIGN.md` §7 and §9).
//!
//! ```text
//! 0x0301 Bootstrap        register a store; create the cluster if this is the first
//! 0x0302 StoreHeartbeat   capacity and load, every 10 s
//! 0x0303 RegionHeartbeat  one region's leader reporting; the answer may carry an Operator
//! 0x0304 GetRegion        where a key lives, who leads it, and how to reach its stores
//! 0x0305 AllocId          a block of cluster-unique ids
//! 0x0306 Tso              a batch of timestamps
//! ```
//!
//! # Every request carries the cluster id
//!
//! Two clusters sharing an address is a misconfiguration whose symptom, unchecked, is one
//! cluster quietly answering questions about the other's regions — so the id is on every
//! request and PD refuses a mismatch with [`ProtoError::ClusterMismatch`]. `Bootstrap` may
//! send **zero**, meaning "I do not know it yet", because asking is how a caller learns it; a
//! caller that does know it sends it and has it checked like everything else.
//!
//! # The channel is deliberately thin
//!
//! [`PdChannel`] encodes, sends, and checks that the answer is the one that was asked for. It
//! does not cache regions, retry, or schedule: the store lane's `PdClient`
//! (`docs/plans/phase-4.md` §3.2) wraps it and that is where policy belongs. For a caller with
//! its own transport — a blocking one, or a scripted fake in a test — [`encode`] and
//! [`decode`] are the same protocol with nothing wrapped around it at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::messages::Method;
use crate::region::{Epoch, Region};
use crate::{ProtoError, Request, Response, Transport};

/// One key range that wants columnar replicas, and how many.
///
/// The unit of [`PdReq::ReportColumnar`]. A **range** rather than a table, because a range is
/// what PD already reasons in and a table id would make PD a reader of SQL semantics
/// (`CLAUDE.md` invariant 7). It is also split-safe by construction: a table that splits into
/// four regions is still one range, and every region overlapping it inherits the wish without
/// anybody re-reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnarWish {
    /// Inclusive start of the range.
    pub start_key: Bytes,
    /// Exclusive end, or empty for "to the end of the key space".
    pub end_key: Bytes,
    /// How many columnar replicas the range wants. Never zero: a range that wants none is absent
    /// from the report, which is how a cleared flag travels.
    pub replicas: u8,
}

/// A store, and where to reach it.
///
/// The address is here because a client addresses a store by **id** and "resolving one to a
/// socket is PD's job" (`docs/DESIGN.md` §10). It is a string rather than a parsed
/// `SocketAddr` so that a host name — or, later, something that is not an IP at all — does not
/// need a format change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    /// The store's cluster-unique id.
    pub store_id: u64,
    /// Where it listens, as `host:port`.
    pub address: String,
}

impl StoreInfo {
    /// A store at an address.
    pub fn new(store_id: u64, address: impl Into<String>) -> Self {
        Self {
            store_id,
            address: address.into(),
        }
    }

    fn encode(&self, out: &mut Encoder) {
        out.put_varint(self.store_id);
        out.put_str(&self.address);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            store_id: input.get_varint("store.id")?,
            address: input.get_str("store.address")?.to_owned(),
        })
    }
}

/// One membership change PD wants a region's leader to propose (`docs/DESIGN.md` §7).
///
/// An operator is a **request, not a command**. It rides on the answer to that region's
/// heartbeat, and PD re-sends it on every later heartbeat until a heartbeat shows it happened
/// or it times out — so a lost response costs a heartbeat interval and never a stuck region.
/// The receiving store checks the epoch and its own state before proposing anything, which is
/// what makes re-sending safe: a duplicate is refused rather than applied twice.
///
/// The **epoch is part of the operator** for that reason. A store that has split or changed
/// membership since PD last heard from it will refuse an operator addressed to the old shape
/// (`CLAUDE.md` invariant 5), and PD will re-derive from the next heartbeat rather than insist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operator {
    /// Add a replica of `region_id` on `store_id`, numbered `peer_id`.
    ///
    /// The peer id comes from PD's allocator, so it is cluster-unique and is *not* reused when
    /// an operator is re-issued after a PD restart — the store's own epoch check absorbs the
    /// stale one.
    AddPeer {
        /// The region to grow.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// Where the new replica goes.
        store_id: u64,
        /// What to number it.
        peer_id: u64,
    },

    /// Remove the replica `peer_id` from `region_id`.
    ///
    /// Only ever issued once the region already has enough live replicas without it: removing
    /// first and adding second is how a repair takes a region below quorum
    /// (`docs/DESIGN.md` §7).
    RemovePeer {
        /// The region to shrink.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// Which replica to drop.
        peer_id: u64,
    },

    /// Add a replica of `region_id` on `store_id` that **stays** a learner.
    ///
    /// [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md) Decision 1: a columnar replica
    /// is a Raft learner whose apply writes columns instead of rows, and it is never promoted.
    ///
    /// **This is not a new membership concept.** The peer it asks for is an ordinary
    /// [`PeerRole::Learner`](crate::PeerRole::Learner) — the same one [`Operator::AddPeer`]
    /// creates on its way to a voter — and nothing in Raft, in the region record or in a quorum
    /// calculation tells the two apart, because there is nothing to tell apart. What differs is
    /// what **done** means, and that is why it cannot be `AddPeer` with a flag: `AddPeer`
    /// completes when the peer becomes a voter, and this one completes when it exists at all. An
    /// `AddPeer` that stopped at a learner is a repair still in progress; an `AddLearner` that
    /// stopped at a learner is finished.
    ///
    /// A separate kind byte rather than a field on `AddPeer`, so no existing operator's bytes
    /// move — the same additive shape [ADR 0028](../../docs/adr/0028-the-schema-lease.md) chose
    /// for the schema lease, and for the same reason.
    AddLearner {
        /// The region to grow.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// Where the learner goes.
        store_id: u64,
        /// What to number it.
        peer_id: u64,
    },

    /// Move leadership of `region_id` to `to_peer_id`.
    ///
    /// **Reserved for 4d.** It is on the wire now so that the operator encoding does not change
    /// when leader balance arrives; nothing in 4c issues one, and a test pins that.
    TransferLeader {
        /// The region whose leadership moves.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// The peer that should take office.
        to_peer_id: u64,
    },
}

/// Wire tag for an operator's kind (*fixed*). Zero is reserved, as everywhere in this format.
mod operator_kind {
    pub(super) const ADD_PEER: u8 = 1;
    pub(super) const REMOVE_PEER: u8 = 2;
    pub(super) const TRANSFER_LEADER: u8 = 3;
    pub(super) const ADD_LEARNER: u8 = 4;
}

impl Operator {
    /// The region this operator is about.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        match self {
            Self::AddPeer { region_id, .. }
            | Self::AddLearner { region_id, .. }
            | Self::RemovePeer { region_id, .. }
            | Self::TransferLeader { region_id, .. } => *region_id,
        }
    }

    /// The epoch PD believed the region was at when it issued this.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        match self {
            Self::AddPeer { epoch, .. }
            | Self::AddLearner { epoch, .. }
            | Self::RemovePeer { epoch, .. }
            | Self::TransferLeader { epoch, .. } => *epoch,
        }
    }

    /// The name a log line or an error message uses.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddPeer { .. } => "AddPeer",
            Self::AddLearner { .. } => "AddLearner",
            Self::RemovePeer { .. } => "RemovePeer",
            Self::TransferLeader { .. } => "TransferLeader",
        }
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Self::AddPeer {
                region_id,
                epoch,
                store_id,
                peer_id,
            } => {
                out.put_u8(operator_kind::ADD_PEER);
                out.put_varint(*region_id);
                epoch.encode(out);
                out.put_varint(*store_id);
                out.put_varint(*peer_id);
            }
            Self::AddLearner {
                region_id,
                epoch,
                store_id,
                peer_id,
            } => {
                out.put_u8(operator_kind::ADD_LEARNER);
                out.put_varint(*region_id);
                epoch.encode(out);
                out.put_varint(*store_id);
                out.put_varint(*peer_id);
            }
            Self::RemovePeer {
                region_id,
                epoch,
                peer_id,
            } => {
                out.put_u8(operator_kind::REMOVE_PEER);
                out.put_varint(*region_id);
                epoch.encode(out);
                out.put_varint(*peer_id);
            }
            Self::TransferLeader {
                region_id,
                epoch,
                to_peer_id,
            } => {
                out.put_u8(operator_kind::TRANSFER_LEADER);
                out.put_varint(*region_id);
                epoch.encode(out);
                out.put_varint(*to_peer_id);
            }
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let kind = input.get_u8("operator.kind")?;
        let region_id = input.get_varint("operator.region_id")?;
        let epoch = Epoch::decode(input)?;
        Ok(match kind {
            operator_kind::ADD_PEER => Self::AddPeer {
                region_id,
                epoch,
                store_id: input.get_varint("operator.store_id")?,
                peer_id: input.get_varint("operator.peer_id")?,
            },
            operator_kind::ADD_LEARNER => Self::AddLearner {
                region_id,
                epoch,
                store_id: input.get_varint("operator.store_id")?,
                peer_id: input.get_varint("operator.peer_id")?,
            },
            operator_kind::REMOVE_PEER => Self::RemovePeer {
                region_id,
                epoch,
                peer_id: input.get_varint("operator.peer_id")?,
            },
            operator_kind::TRANSFER_LEADER => Self::TransferLeader {
                region_id,
                epoch,
                to_peer_id: input.get_varint("operator.to_peer_id")?,
            },
            other => {
                return Err(DecodeError::UnknownTag {
                    what: "operator kind",
                    tag: u64::from(other),
                });
            }
        })
    }
}

/// Anything a caller asks the placement driver.
///
/// The heartbeat field sets are the ones `docs/plans/phase-4.md` §3.2 pins, so the store's
/// `PdClient` maps onto them without a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdReq {
    /// Register this store, and create the cluster if it is the first.
    Bootstrap {
        /// Who is registering, and where to reach it.
        store: StoreInfo,
    },

    /// One store's capacity and load.
    StoreHeartbeat {
        /// Which store is reporting.
        store_id: u64,
        /// Bytes of storage it has.
        capacity: u64,
        /// Bytes still free.
        available: u64,
        /// Regions with a peer on it.
        region_count: u64,
        /// Regions it leads.
        leader_count: u64,
        /// Bytes of user data applied.
        applied_bytes: u64,
    },

    /// One region's leader reporting the region as it sees it.
    RegionHeartbeat {
        /// Range, peers and epoch.
        region: Region,
        /// The peer sending this; zero if it does not claim to lead.
        leader_peer_id: u64,
        /// The Raft term it leads in. The tiebreaker within one epoch.
        term: u64,
        /// Approximate bytes of user data in the region.
        approximate_size: u64,
        /// The leader's apply index.
        applied_index: u64,
    },

    /// Where a key lives.
    GetRegion {
        /// The key, in *user* key space — PD never sees a namespace prefix
        /// (`CLAUDE.md` invariant 7).
        key: Bytes,
    },

    /// A block of cluster-unique ids.
    AllocId {
        /// How many, consecutive.
        count: u64,
    },

    /// A batch of timestamps.
    Tso {
        /// How many, consecutive as integers.
        count: u32,
    },

    /// How long a node may act on a cached schema before it must ask again
    /// ([ADR 0028](../../docs/adr/0028-the-schema-lease.md)).
    ///
    /// Asked by a SQL node, which is the first thing above the store that is neither a store nor a
    /// region and therefore has nothing else to say to PD. It carries no arguments: the answer is
    /// a cluster-wide number with one writer, exactly like the GC safepoint.
    SchemaLease,

    /// **The whole** set of key ranges that want columnar replicas, as one SQL node sees them.
    ///
    /// [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md) Decision 5. PD acts on the
    /// per-table columnar setting, and **cannot read it**: the setting lives in the catalog, in
    /// the cluster's own key space, and PD links neither `esker-sql` nor a client — every method
    /// on this service is inbound, so PD is told things and asks for nothing. So the SQL node
    /// that ran the `ALTER` reports, and re-reports whenever it refreshes its lease.
    ///
    /// **Key ranges, not table ids**, and that is what keeps `CLAUDE.md` invariant 7 intact: a
    /// range is PD's own vocabulary — it is what a region *is* — so PD acts on this without ever
    /// learning that a table exists. A table id would have made PD a reader of SQL semantics,
    /// which is the line phase 6e drew when it took the schema-step drive away from PD.
    ///
    /// **A full assertion, never a delta.** Every SQL node reads the same catalog, so every
    /// report has the same content and the last writer is right whoever it was; a delta would
    /// need an order this service does not impose. It also makes the re-report on lease refresh
    /// an anti-entropy sweep rather than a duplicate: a report lost to a restart is repaired by
    /// the next one, and PD needs no acknowledgement protocol to notice.
    ReportColumnar {
        /// Every range that wants columnar replicas, with how many. A range that wants none is
        /// simply absent, which is how a cleared flag arrives.
        wishes: Vec<ColumnarWish>,
    },
}

impl PdReq {
    /// The method this request is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Bootstrap { .. } => Method::PdBootstrap,
            Self::StoreHeartbeat { .. } => Method::PdStoreHeartbeat,
            Self::RegionHeartbeat { .. } => Method::PdRegionHeartbeat,
            Self::GetRegion { .. } => Method::PdGetRegion,
            Self::AllocId { .. } => Method::PdAllocId,
            Self::Tso { .. } => Method::PdTso,
            Self::SchemaLease => Method::PdSchemaLease,
            Self::ReportColumnar { .. } => Method::PdReportColumnar,
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Bootstrap { store } => store.encode(out),
            Self::StoreHeartbeat {
                store_id,
                capacity,
                available,
                region_count,
                leader_count,
                applied_bytes,
            } => {
                out.put_varint(*store_id);
                out.put_varint(*capacity);
                out.put_varint(*available);
                out.put_varint(*region_count);
                out.put_varint(*leader_count);
                out.put_varint(*applied_bytes);
            }
            Self::RegionHeartbeat {
                region,
                leader_peer_id,
                term,
                approximate_size,
                applied_index,
            } => {
                region.encode(out);
                out.put_varint(*leader_peer_id);
                out.put_varint(*term);
                out.put_varint(*approximate_size);
                out.put_varint(*applied_index);
            }
            Self::GetRegion { key } => out.put_bytes(key),
            Self::AllocId { count } => out.put_varint(*count),
            Self::Tso { count } => out.put_varint(u64::from(*count)),
            // No fields, so nothing to write. The method is the whole request.
            Self::SchemaLease => {}
            Self::ReportColumnar { wishes } => {
                out.put_varint(wishes.len() as u64);
                for wish in wishes {
                    out.put_bytes(&wish.start_key);
                    out.put_bytes(&wish.end_key);
                    out.put_u8(wish.replicas);
                }
            }
        }
    }

    pub(crate) fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match method {
            Method::PdBootstrap => Self::Bootstrap {
                store: StoreInfo::decode(input)?,
            },
            Method::PdStoreHeartbeat => Self::StoreHeartbeat {
                store_id: input.get_varint("beat.store_id")?,
                capacity: input.get_varint("beat.capacity")?,
                available: input.get_varint("beat.available")?,
                region_count: input.get_varint("beat.region_count")?,
                leader_count: input.get_varint("beat.leader_count")?,
                applied_bytes: input.get_varint("beat.applied_bytes")?,
            },
            Method::PdRegionHeartbeat => Self::RegionHeartbeat {
                region: Region::decode(input)?,
                leader_peer_id: input.get_varint("beat.leader_peer_id")?,
                term: input.get_varint("beat.term")?,
                approximate_size: input.get_varint("beat.approximate_size")?,
                applied_index: input.get_varint("beat.applied_index")?,
            },
            Method::PdGetRegion => Self::GetRegion {
                key: Bytes::copy_from_slice(input.get_bytes("get_region.key")?),
            },
            Method::PdAllocId => Self::AllocId {
                count: input.get_varint("alloc_id.count")?,
            },
            Method::PdTso => Self::Tso {
                count: input.get_varint_u32("tso.count")?,
            },
            Method::PdSchemaLease => Self::SchemaLease,
            Method::PdReportColumnar => {
                let count = input.get_count("columnar.wishes")?;
                let mut wishes = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    wishes.push(ColumnarWish {
                        start_key: Bytes::copy_from_slice(input.get_bytes("columnar.start_key")?),
                        end_key: Bytes::copy_from_slice(input.get_bytes("columnar.end_key")?),
                        replicas: input.get_u8("columnar.replicas")?,
                    });
                }
                Self::ReportColumnar { wishes }
            }
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a Pd method", other.name()),
                ));
            }
        })
    }
}

/// Anything the placement driver answers with. Failures are `Error` frames instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdResp {
    /// The cluster this store now belongs to.
    Bootstrap {
        /// The cluster's id, to be sent on every later request.
        cluster_id: u64,
        /// The region to create, when this call is the one that bootstrapped the cluster.
        /// `None` means the cluster already existed and this was a registration.
        region: Option<Region>,
    },

    /// Recorded.
    StoreHeartbeat,

    /// Recorded, or dropped as stale — which is not something the sender acts on, so the
    /// answer does not say which. PD logs the drop; the next beat supersedes it either way.
    ///
    /// What the answer *does* carry is at most one [`Operator`] for this region: the membership
    /// change PD wants its leader to propose. **At most one, ever** — PD never has two in
    /// flight for one region (`docs/DESIGN.md` §7) — and the same one comes back on every
    /// heartbeat until a heartbeat shows it happened or it times out.
    RegionHeartbeat {
        /// What PD wants this region's leader to do, if anything.
        operator: Option<Operator>,
    },

    /// Where the key lives.
    GetRegion {
        /// The region covering the key, or `None` if no region does — which cannot happen
        /// while the table is a contiguous partition, and is a fact rather than an error.
        region: Option<Region>,
        /// The peer PD last heard was leading it; zero for "no opinion", the same convention
        /// [`crate::RequestHeader::peer`] uses.
        leader_peer_id: u64,
        /// The stores hosting the region's peers, so the caller can reach them.
        stores: Vec<StoreInfo>,
    },

    /// The block of ids.
    AllocId {
        /// The first id.
        start: u64,
        /// How many were granted; always what was asked for. Echoed so that a caller can
        /// check rather than assume.
        count: u64,
    },

    /// The batch of timestamps.
    Tso {
        /// The first timestamp; the batch is `start_ts .. start_ts + count`.
        start_ts: u64,
        /// How many were granted.
        count: u32,
    },

    /// The report was recorded. It carries nothing: a full assertion has no partial outcome, and
    /// the next report repairs anything this one lost.
    ReportColumnar,

    /// The schema lease, and the step interval derived from it.
    SchemaLease {
        /// How long a node may serve **writes** from a cached schema before asking again.
        ///
        /// Writes only: a reader's snapshot already agrees with the rows it can see, so gating
        /// reads would add stalls and close no hole (ADR 0020, as amended).
        lease_ms: u64,
        /// How long a schema-change step must wait before the next one, for a change in the
        /// **adding** direction: `lease_ms + lock_ttl_ms`.
        ///
        /// Sent rather than derived by the caller so that the arithmetic has one home — PD's — and
        /// a node cannot hold a different opinion about how long it is safe to be behind.
        step_interval_ms: u64,
        /// The extra a **removing** step must wait on top of [`PdResp::SchemaLease::step_interval_ms`],
        /// which is the MVCC retention window: a reader at `public` reads entries a node at
        /// `absent` has already deleted, and retention is what keeps them readable.
        ///
        /// Zero when nothing is being removed, which is why it is separate rather than folded in:
        /// retention is an hour by default, and adding it to every step would make every schema
        /// change take one.
        removal_extra_ms: u64,
    },
}

impl PdResp {
    /// The method this is a response to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Bootstrap { .. } => Method::PdBootstrap,
            Self::StoreHeartbeat => Method::PdStoreHeartbeat,
            Self::RegionHeartbeat { .. } => Method::PdRegionHeartbeat,
            Self::GetRegion { .. } => Method::PdGetRegion,
            Self::AllocId { .. } => Method::PdAllocId,
            Self::Tso { .. } => Method::PdTso,
            Self::SchemaLease { .. } => Method::PdSchemaLease,
            Self::ReportColumnar => Method::PdReportColumnar,
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Bootstrap { cluster_id, region } => {
                out.put_varint(*cluster_id);
                encode_opt_region(region.as_ref(), out);
            }
            Self::StoreHeartbeat | Self::ReportColumnar => {}
            Self::RegionHeartbeat { operator } => match operator {
                Some(operator) => {
                    out.put_bool(true);
                    operator.encode(out);
                }
                None => out.put_bool(false),
            },
            Self::GetRegion {
                region,
                leader_peer_id,
                stores,
            } => {
                encode_opt_region(region.as_ref(), out);
                out.put_varint(*leader_peer_id);
                out.put_varint(stores.len() as u64);
                for store in stores {
                    store.encode(out);
                }
            }
            Self::AllocId { start, count } => {
                out.put_varint(*start);
                out.put_varint(*count);
            }
            Self::Tso { start_ts, count } => {
                out.put_varint(*start_ts);
                out.put_varint(u64::from(*count));
            }
            Self::SchemaLease {
                lease_ms,
                step_interval_ms,
                removal_extra_ms,
            } => {
                out.put_varint(*lease_ms);
                out.put_varint(*step_interval_ms);
                out.put_varint(*removal_extra_ms);
            }
        }
    }

    pub(crate) fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match method {
            Method::PdBootstrap => Self::Bootstrap {
                cluster_id: input.get_varint("bootstrap.cluster_id")?,
                region: decode_opt_region(input)?,
            },
            Method::PdStoreHeartbeat => Self::StoreHeartbeat,
            Method::PdReportColumnar => Self::ReportColumnar,
            Method::PdRegionHeartbeat => Self::RegionHeartbeat {
                operator: input
                    .get_bool("operator.present")?
                    .then(|| Operator::decode(input))
                    .transpose()?,
            },
            Method::PdGetRegion => {
                let region = decode_opt_region(input)?;
                let leader_peer_id = input.get_varint("get_region.leader_peer_id")?;
                let count = input.get_count("get_region.stores")?;
                let mut stores = Vec::with_capacity(count);
                for _ in 0..count {
                    stores.push(StoreInfo::decode(input)?);
                }
                Self::GetRegion {
                    region,
                    leader_peer_id,
                    stores,
                }
            }
            Method::PdAllocId => Self::AllocId {
                start: input.get_varint("alloc_id.start")?,
                count: input.get_varint("alloc_id.count")?,
            },
            Method::PdTso => Self::Tso {
                start_ts: input.get_varint("tso.start_ts")?,
                count: input.get_varint_u32("tso.count")?,
            },
            Method::PdSchemaLease => Self::SchemaLease {
                lease_ms: input.get_varint("schema_lease.lease_ms")?,
                step_interval_ms: input.get_varint("schema_lease.step_interval_ms")?,
                removal_extra_ms: input.get_varint("schema_lease.removal_extra_ms")?,
            },
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a Pd method", other.name()),
                ));
            }
        })
    }
}

fn encode_opt_region(region: Option<&Region>, out: &mut Encoder) {
    match region {
        Some(region) => {
            out.put_bool(true);
            region.encode(out);
        }
        None => out.put_bool(false),
    }
}

fn decode_opt_region(input: &mut Decoder<'_>) -> Result<Option<Region>, DecodeError> {
    if input.get_bool("region.present")? {
        Ok(Some(Region::decode(input)?))
    } else {
        Ok(None)
    }
}

/// The request frame for one PD call. Public so that a caller with its own transport — a
/// blocking one, or a fake in a test — can drive the protocol without a [`PdChannel`].
#[must_use]
pub fn encode(cluster_id: u64, request: PdReq) -> Request {
    Request::Pd {
        cluster_id,
        request,
    }
}

/// The PD answer inside a response, or an error naming what came back instead.
pub fn decode(response: Response) -> Result<PdResp, ProtoError> {
    match response {
        Response::Pd(response) => Ok(response),
        other => Err(ProtoError::invalid(format!(
            "expected a Pd response, got {}",
            other.method().name()
        ))),
    }
}

/// A caller's side of the `Pd` service: encode, send, check the answer is the one asked for.
///
/// Holds the cluster id and stamps it on every request. `Bootstrap` latches the id the cluster
/// answers with, so the usual sequence — construct, bootstrap, then everything else — needs no
/// configuration at all.
#[derive(Debug)]
pub struct PdChannel {
    transport: Arc<dyn Transport>,
    cluster_id: AtomicU64,
}

impl PdChannel {
    /// A channel over `transport`, with the cluster id not yet known.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            cluster_id: AtomicU64::new(0),
        }
    }

    /// A channel that already knows which cluster it is talking to.
    #[must_use]
    pub fn with_cluster(transport: Arc<dyn Transport>, cluster_id: u64) -> Self {
        Self {
            transport,
            cluster_id: AtomicU64::new(cluster_id),
        }
    }

    /// The cluster id being stamped on requests; zero if it is not known yet.
    #[must_use]
    pub fn cluster_id(&self) -> u64 {
        self.cluster_id.load(Ordering::Relaxed)
    }

    /// Sets the cluster id, for a caller that learned it elsewhere.
    pub fn set_cluster_id(&self, cluster_id: u64) {
        self.cluster_id.store(cluster_id, Ordering::Relaxed);
    }

    /// The transport underneath.
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    /// Registers `store`, and creates the cluster if it is the first one.
    ///
    /// Latches the cluster id for every later call on this channel. Meant to be called on
    /// **every** store start: it is idempotent, and it is what refreshes the store's address.
    pub async fn bootstrap(&self, store: StoreInfo) -> Result<(u64, Option<Region>), ProtoError> {
        let response = self.call(PdReq::Bootstrap { store }).await?;
        match response {
            PdResp::Bootstrap { cluster_id, region } => {
                self.set_cluster_id(cluster_id);
                Ok((cluster_id, region))
            }
            other => Err(mismatch("Bootstrap", &other)),
        }
    }

    /// Reports this store's capacity and load.
    pub async fn store_heartbeat(
        &self,
        store_id: u64,
        capacity: u64,
        available: u64,
        region_count: u64,
        leader_count: u64,
        applied_bytes: u64,
    ) -> Result<(), ProtoError> {
        let response = self
            .call(PdReq::StoreHeartbeat {
                store_id,
                capacity,
                available,
                region_count,
                leader_count,
                applied_bytes,
            })
            .await?;
        match response {
            PdResp::StoreHeartbeat => Ok(()),
            other => Err(mismatch("StoreHeartbeat", &other)),
        }
    }

    /// Reports one region, as its leader sees it.
    pub async fn region_heartbeat(
        &self,
        region: Region,
        leader_peer_id: u64,
        term: u64,
        approximate_size: u64,
        applied_index: u64,
    ) -> Result<Option<Operator>, ProtoError> {
        let response = self
            .call(PdReq::RegionHeartbeat {
                region,
                leader_peer_id,
                term,
                approximate_size,
                applied_index,
            })
            .await?;
        match response {
            PdResp::RegionHeartbeat { operator } => Ok(operator),
            other => Err(mismatch("RegionHeartbeat", &other)),
        }
    }

    /// Where `key` lives: the region, the leader PD last heard about, and the stores its peers
    /// are on.
    pub async fn get_region(
        &self,
        key: impl Into<Bytes>,
    ) -> Result<Option<(Region, Option<u64>, Vec<StoreInfo>)>, ProtoError> {
        let response = self.call(PdReq::GetRegion { key: key.into() }).await?;
        match response {
            PdResp::GetRegion {
                region,
                leader_peer_id,
                stores,
            } => Ok(region.map(|region| {
                (
                    region,
                    (leader_peer_id != 0).then_some(leader_peer_id),
                    stores,
                )
            })),
            other => Err(mismatch("GetRegion", &other)),
        }
    }

    /// The first of `count` consecutive cluster-unique ids.
    pub async fn alloc_id(&self, count: u64) -> Result<u64, ProtoError> {
        let response = self.call(PdReq::AllocId { count }).await?;
        match response {
            PdResp::AllocId {
                start,
                count: granted,
            } if granted == count => Ok(start),
            PdResp::AllocId { count: granted, .. } => Err(ProtoError::invalid(format!(
                "asked for {count} ids and was granted {granted}"
            ))),
            other => Err(mismatch("AllocId", &other)),
        }
    }

    /// The first of `count` consecutive timestamps.
    pub async fn tso(&self, count: u32) -> Result<u64, ProtoError> {
        let response = self.call(PdReq::Tso { count }).await?;
        match response {
            PdResp::Tso {
                start_ts,
                count: granted,
            } if granted == count => Ok(start_ts),
            PdResp::Tso { count: granted, .. } => Err(ProtoError::invalid(format!(
                "asked for {count} timestamps and was granted {granted}"
            ))),
            other => Err(mismatch("Tso", &other)),
        }
    }

    /// How long this node may act on a cached schema before asking again, and the step arithmetic
    /// that depends on it ([ADR 0028](../../docs/adr/0028-the-schema-lease.md)).
    ///
    /// A SQL node calls this when its lease is running out. **Not being able to call it is the
    /// point**: a node that cannot reach PD holds no lease, and a node holding no lease refuses to
    /// write, which is how the step clock can advance on a timer rather than on a poll of nodes it
    /// may not be able to reach.
    pub async fn schema_lease(&self) -> Result<(u64, u64, u64), ProtoError> {
        let response = self.call(PdReq::SchemaLease).await?;
        match response {
            PdResp::SchemaLease {
                lease_ms,
                step_interval_ms,
                removal_extra_ms,
            } => Ok((lease_ms, step_interval_ms, removal_extra_ms)),
            other => Err(mismatch("SchemaLease", &other)),
        }
    }

    async fn call(&self, request: PdReq) -> Result<PdResp, ProtoError> {
        let response = self
            .transport
            .call(encode(self.cluster_id(), request))
            .await?;
        decode(response)
    }
}

fn mismatch(asked: &str, got: &PdResp) -> ProtoError {
    ProtoError::invalid(format!(
        "asked Pd::{asked} and got a {} answer",
        got.method().name()
    ))
}

#[cfg(test)]
mod tests {
    use super::{Operator, PdReq, PdResp, StoreInfo};
    use crate::messages::{Method, SERVICE_PD};
    use crate::region::{Epoch, Peer, Region};
    use crate::{Request, Response};
    use bytes::Bytes;

    fn region() -> Region {
        Region {
            id: 7,
            start_key: Bytes::from_static(b"a"),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, 10), Peer::voter(2, 11)],
            epoch: Epoch::new(2, 3),
        }
    }

    fn requests() -> Vec<PdReq> {
        vec![
            PdReq::Bootstrap {
                store: StoreInfo::new(1, "127.0.0.1:20160"),
            },
            PdReq::StoreHeartbeat {
                store_id: 1,
                capacity: 1 << 40,
                available: 1 << 39,
                region_count: 3,
                leader_count: 1,
                applied_bytes: 99,
            },
            PdReq::RegionHeartbeat {
                region: region(),
                leader_peer_id: 10,
                term: 4,
                approximate_size: 1 << 20,
                applied_index: 77,
            },
            PdReq::GetRegion {
                key: Bytes::from_static(b"key"),
            },
            PdReq::GetRegion { key: Bytes::new() },
            PdReq::AllocId { count: 2 },
            PdReq::Tso { count: 16 },
            PdReq::SchemaLease,
        ]
    }

    fn responses() -> Vec<PdResp> {
        vec![
            PdResp::Bootstrap {
                cluster_id: 0x0123_4567,
                region: Some(region()),
            },
            PdResp::Bootstrap {
                cluster_id: 0x0123_4567,
                region: None,
            },
            PdResp::StoreHeartbeat,
            PdResp::RegionHeartbeat { operator: None },
            PdResp::RegionHeartbeat {
                operator: Some(Operator::AddPeer {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    store_id: 4,
                    peer_id: 41,
                }),
            },
            PdResp::RegionHeartbeat {
                operator: Some(Operator::RemovePeer {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    peer_id: 11,
                }),
            },
            PdResp::RegionHeartbeat {
                operator: Some(Operator::TransferLeader {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    to_peer_id: 10,
                }),
            },
            PdResp::GetRegion {
                region: Some(region()),
                leader_peer_id: 10,
                stores: vec![
                    StoreInfo::new(1, "127.0.0.1:20160"),
                    StoreInfo::new(2, "h:1"),
                ],
            },
            PdResp::GetRegion {
                region: None,
                leader_peer_id: 0,
                stores: Vec::new(),
            },
            PdResp::AllocId {
                start: 1_000,
                count: 2,
            },
            PdResp::Tso {
                start_ts: 0x1234_5678_9ABC,
                count: 16,
            },
            PdResp::SchemaLease {
                lease_ms: 5_000,
                step_interval_ms: 8_000,
                removal_extra_ms: 3_600_000,
            },
        ]
    }

    #[test]
    fn every_pd_request_round_trips_through_a_frame_body() {
        for request in requests() {
            let framed = Request::Pd {
                cluster_id: 42,
                request: request.clone(),
            };
            let back = Request::decode(&framed.encode()).unwrap();
            assert_eq!(back, framed);
        }
    }

    #[test]
    fn every_pd_response_round_trips_through_a_frame_body() {
        for response in responses() {
            let framed = Response::Pd(response.clone());
            assert_eq!(Response::decode(&framed.encode()).unwrap(), framed);
        }
    }

    /// A zero cluster id is a caller that does not know it yet, and must survive the trip: it
    /// is what `Bootstrap` sends the first time.
    #[test]
    fn an_unknown_cluster_id_round_trips() {
        let framed = Request::Pd {
            cluster_id: 0,
            request: PdReq::Bootstrap {
                store: StoreInfo::new(1, "a"),
            },
        };
        assert_eq!(Request::decode(&framed.encode()).unwrap(), framed);
    }

    #[test]
    fn every_method_belongs_to_the_pd_service_and_is_contiguous() {
        let numbers: Vec<u16> = requests()
            .iter()
            .map(|request| request.method().as_u16())
            .collect();
        for number in &numbers {
            assert_eq!((number >> 8) as u8, SERVICE_PD);
        }
        for (offset, method) in [
            Method::PdBootstrap,
            Method::PdStoreHeartbeat,
            Method::PdRegionHeartbeat,
            Method::PdGetRegion,
            Method::PdAllocId,
            Method::PdTso,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                method.as_u16(),
                0x0301 + u16::try_from(offset).unwrap(),
                "{} is out of order",
                method.name()
            );
        }
    }

    /// A zero kind byte, and one this version does not define, are both errors — the rule
    /// every tag in this format follows. A zeroed operator must not decode as "add a peer".
    #[test]
    fn an_unknown_operator_kind_is_refused() {
        let good = Response::Pd(PdResp::RegionHeartbeat {
            operator: Some(Operator::RemovePeer {
                region_id: 7,
                epoch: Epoch::new(2, 3),
                peer_id: 11,
            }),
        })
        .encode();
        // method:u16 ++ present:u8 ++ kind:u8
        let kind_at = 3;
        assert_eq!(good[kind_at], 2, "the kind byte moved");
        for kind in [0u8, 4, 9, 255] {
            let mut bytes = good.clone();
            bytes[kind_at] = kind;
            assert!(
                Response::decode(&bytes).is_err(),
                "operator kind {kind} decoded"
            );
        }
    }

    /// Trailing bytes are a different message, here as everywhere else in this format.
    #[test]
    fn trailing_bytes_after_a_pd_body_are_refused() {
        let mut bytes = Request::Pd {
            cluster_id: 1,
            request: PdReq::AllocId { count: 1 },
        }
        .encode();
        bytes.push(0);
        assert!(Request::decode(&bytes).is_err());
    }

    /// Truncating a well-formed body at any offset is an error, never a panic.
    #[test]
    fn truncation_never_panics() {
        for request in requests() {
            let bytes = Request::Pd {
                cluster_id: 7,
                request,
            }
            .encode();
            for cut in 0..bytes.len() {
                let _ = Request::decode(&bytes[..cut]);
            }
        }
        for response in responses() {
            let bytes = Response::Pd(response).encode();
            for cut in 0..bytes.len() {
                let _ = Response::decode(&bytes[..cut]);
            }
        }
    }
}
