//! Service `0x03`: the placement driver's six methods, and the channel a caller drives them
//! through (`docs/DESIGN.md` §7 and §9).
//!
//! ```text
//! 0x0301 Bootstrap        register a store; create the cluster if this is the first
//! 0x0302 StoreHeartbeat   capacity and load, every 10 s
//! 0x0303 RegionHeartbeat  one region's leader reporting, every 60 s or on a change
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
use crate::region::Region;
use crate::{ProtoError, Request, Response, Transport};

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
    /// answer carries nothing. PD logs the drop; the next beat supersedes it either way.
    RegionHeartbeat,

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
}

impl PdResp {
    /// The method this is a response to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Bootstrap { .. } => Method::PdBootstrap,
            Self::StoreHeartbeat => Method::PdStoreHeartbeat,
            Self::RegionHeartbeat => Method::PdRegionHeartbeat,
            Self::GetRegion { .. } => Method::PdGetRegion,
            Self::AllocId { .. } => Method::PdAllocId,
            Self::Tso { .. } => Method::PdTso,
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Bootstrap { cluster_id, region } => {
                out.put_varint(*cluster_id);
                encode_opt_region(region.as_ref(), out);
            }
            Self::StoreHeartbeat | Self::RegionHeartbeat => {}
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
        }
    }

    pub(crate) fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match method {
            Method::PdBootstrap => Self::Bootstrap {
                cluster_id: input.get_varint("bootstrap.cluster_id")?,
                region: decode_opt_region(input)?,
            },
            Method::PdStoreHeartbeat => Self::StoreHeartbeat,
            Method::PdRegionHeartbeat => Self::RegionHeartbeat,
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
    ) -> Result<(), ProtoError> {
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
            PdResp::RegionHeartbeat => Ok(()),
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
    use super::{PdReq, PdResp, StoreInfo};
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
            PdResp::RegionHeartbeat,
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
