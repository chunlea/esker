//! The six values PD stores, and their strict decoders (*fixed*, format version 1).
//!
//! Every record is `version:u8 ++ fields`, hand-encoded with `esker-proto`'s [`Encoder`] — the
//! same convention the store uses for its Raft state (`esker_store::raft_log`) and for the same
//! reasons. There is no per-record checksum because the engine already checksums every byte it
//! stores, in the WAL and in every SST block; a second CRC over the same bytes would buy
//! nothing and would have to be maintained.
//!
//! A decode that finds an unknown version, a short field or a trailing byte returns
//! [`PdError::Corrupt`]. Never a panic, never a default (`CLAUDE.md` invariants 2 and 9).
//!
//! # Why the region is encoded here rather than by `esker-proto`
//!
//! [`Region`] travels on the wire *and* lives on PD's disk, and those are two formats that
//! merely happen to agree today. The wire encoding belongs to `esker-proto` and changes with
//! `WIRE_VERSION`; this one belongs to PD's database and changes with a migration. Sharing one
//! encoder would tie a wire-compatibility decision to an on-disk one — and the golden files in
//! this crate exist precisely to catch the day they diverge.

use bytes::Bytes;
use esker_proto::{Decoder, Encoder, Epoch, Peer, PeerRole, Region};

use crate::error::{PdError, Result};

/// Version byte on every record PD writes. A change to any field's meaning bumps it.
pub const RECORD_VERSION: u8 = 1;

/// The cluster's identity, written once by the first successful bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterRecord {
    /// Minted at bootstrap, checked on every later request, never zero.
    pub cluster_id: u64,
    /// The id of region 1 — the region covering everything that bootstrap created.
    pub first_region_id: u64,
    /// When the cluster was bootstrapped, in physical milliseconds. Informational.
    pub created_ms: u64,
}

/// The id allocator's durable state: the last id that has been *reserved*.
///
/// Ids up to and including `allocated_end` may have been handed out; a restart resumes at
/// `allocated_end + 1`. That is the whole crash-safety argument — the end of a batch is
/// persisted before any id in it leaves, so a crash skips ids rather than repeating them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocRecord {
    /// The last reserved id.
    pub allocated_end: u64,
}

/// The oracle's high-water mark, in physical milliseconds.
///
/// Every timestamp ever handed out has a physical part **strictly below** this, and a restart
/// resumes at `max(clock, high_water_ms)`. See [`crate::tso`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsoRecord {
    /// The mark.
    pub high_water_ms: u64,
}

/// What a store's heartbeat reports about itself (`docs/plans/phase-4.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreStats {
    /// Bytes of storage the store has.
    pub capacity: u64,
    /// Bytes still free.
    pub available: u64,
    /// Regions with a peer on this store.
    pub region_count: u64,
    /// Regions this store leads.
    pub leader_count: u64,
    /// Bytes of user data applied, for the balance operators of 4d.
    pub applied_bytes: u64,
}

/// One store, as PD knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRecord {
    /// The store's cluster-unique id.
    pub store_id: u64,
    /// Where to reach it. A client addresses a store by id; turning one into a socket is PD's
    /// job (`docs/DESIGN.md` §10), and this is the map.
    pub address: String,
    /// When the store first registered, in physical milliseconds.
    pub started_ms: u64,
    /// When its last heartbeat arrived. Liveness is derived from this and nothing else.
    pub last_heartbeat_ms: u64,
    /// Its last reported stats.
    pub stats: StoreStats,
}

/// One region, as PD knows it: the routing entry plus what the last heartbeat said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionRecord {
    /// The range, the peers and the epoch — everything a client needs to address it.
    pub region: Region,
    /// The peer PD believes leads it, or zero for "no opinion". Zero rather than an `Option`
    /// because that is what the wire's peer hints already use (`esker_proto::RequestHeader`).
    pub leader_peer_id: u64,
    /// The Raft term of that leader, as of the last heartbeat.
    pub term: u64,
    /// Approximate bytes of user data, from the leader's own estimate. 4b splits on it.
    pub approximate_size: u64,
    /// The leader's apply index, so that 4c can rebuild its operator view from heartbeats
    /// alone after a restart (`docs/plans/phase-4.md` §6, race 5).
    pub applied_index: u64,
    /// When the last heartbeat for this region arrived.
    pub last_heartbeat_ms: u64,
}

impl RegionRecord {
    /// A region PD has been told about but never had a heartbeat for — what bootstrap writes.
    #[must_use]
    pub fn new(region: Region, now_ms: u64) -> Self {
        Self {
            region,
            leader_peer_id: 0,
            term: 0,
            approximate_size: 0,
            applied_index: 0,
            last_heartbeat_ms: now_ms,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------

fn start(out: &mut Encoder) {
    out.put_u8(RECORD_VERSION);
}

fn open<'a>(what: &'static str, bytes: &'a [u8]) -> Result<Decoder<'a>> {
    let mut input = Decoder::new(bytes);
    let version = input
        .get_u8("record.version")
        .map_err(|error| PdError::corrupt(what, error.to_string()))?;
    if version != RECORD_VERSION {
        return Err(PdError::corrupt(
            what,
            format!("format version {version}, expected {RECORD_VERSION}"),
        ));
    }
    Ok(input)
}

fn close(what: &'static str, input: Decoder<'_>) -> Result<()> {
    input
        .finish()
        .map_err(|error| PdError::corrupt(what, error.to_string()))
}

fn field(what: &'static str) -> impl Fn(esker_proto::DecodeError) -> PdError {
    move |error| PdError::corrupt(what, error.to_string())
}

impl ClusterRecord {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        out.put_varint(self.cluster_id);
        out.put_varint(self.first_region_id);
        out.put_varint(self.created_ms);
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "cluster";
        let mut input = open(WHAT, bytes)?;
        let record = Self {
            cluster_id: input.get_varint("cluster.id").map_err(field(WHAT))?,
            first_region_id: input
                .get_varint("cluster.first_region")
                .map_err(field(WHAT))?,
            created_ms: input
                .get_varint("cluster.created_ms")
                .map_err(field(WHAT))?,
        };
        close(WHAT, input)?;
        if record.cluster_id == 0 {
            return Err(PdError::corrupt(WHAT, "cluster id zero"));
        }
        Ok(record)
    }
}

impl AllocRecord {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        out.put_varint(self.allocated_end);
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "alloc";
        let mut input = open(WHAT, bytes)?;
        let record = Self {
            allocated_end: input.get_varint("alloc.end").map_err(field(WHAT))?,
        };
        close(WHAT, input)?;
        Ok(record)
    }
}

impl TsoRecord {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        out.put_varint(self.high_water_ms);
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "tso";
        let mut input = open(WHAT, bytes)?;
        let record = Self {
            high_water_ms: input.get_varint("tso.high_water_ms").map_err(field(WHAT))?,
        };
        close(WHAT, input)?;
        Ok(record)
    }
}

impl StoreRecord {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        out.put_varint(self.store_id);
        out.put_str(&self.address);
        out.put_varint(self.started_ms);
        out.put_varint(self.last_heartbeat_ms);
        out.put_varint(self.stats.capacity);
        out.put_varint(self.stats.available);
        out.put_varint(self.stats.region_count);
        out.put_varint(self.stats.leader_count);
        out.put_varint(self.stats.applied_bytes);
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "store";
        let mut input = open(WHAT, bytes)?;
        let record = Self {
            store_id: input.get_varint("store.id").map_err(field(WHAT))?,
            address: input
                .get_str("store.address")
                .map_err(field(WHAT))?
                .to_owned(),
            started_ms: input.get_varint("store.started_ms").map_err(field(WHAT))?,
            last_heartbeat_ms: input.get_varint("store.last_beat").map_err(field(WHAT))?,
            stats: StoreStats {
                capacity: input.get_varint("store.capacity").map_err(field(WHAT))?,
                available: input.get_varint("store.available").map_err(field(WHAT))?,
                region_count: input.get_varint("store.regions").map_err(field(WHAT))?,
                leader_count: input.get_varint("store.leaders").map_err(field(WHAT))?,
                applied_bytes: input
                    .get_varint("store.applied_bytes")
                    .map_err(field(WHAT))?,
            },
        };
        close(WHAT, input)?;
        Ok(record)
    }
}

impl RegionRecord {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        encode_region(&self.region, &mut out);
        out.put_varint(self.leader_peer_id);
        out.put_varint(self.term);
        out.put_varint(self.approximate_size);
        out.put_varint(self.applied_index);
        out.put_varint(self.last_heartbeat_ms);
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "region";
        let mut input = open(WHAT, bytes)?;
        let record = Self {
            region: decode_region(WHAT, &mut input)?,
            leader_peer_id: input.get_varint("region.leader").map_err(field(WHAT))?,
            term: input.get_varint("region.term").map_err(field(WHAT))?,
            approximate_size: input.get_varint("region.size").map_err(field(WHAT))?,
            applied_index: input.get_varint("region.applied").map_err(field(WHAT))?,
            last_heartbeat_ms: input.get_varint("region.last_beat").map_err(field(WHAT))?,
        };
        close(WHAT, input)?;
        Ok(record)
    }
}

/// What PD did about one operator, one line at a time (*fixed*, version 1).
///
/// The in-flight set is memory — a restart forgets it, deliberately
/// ([ADR 0013](../../docs/adr/0013-repair-operators-are-requests-not-commands.md)) — which
/// leaves "what did PD do to my cluster, and why is it shaped like this" a question nothing
/// could answer after the fact. This is that answer: a bounded ring of the last
/// [`HISTORY_CAPACITY`] events, on disk, so `esker pd inspect` can show it for a PD that is not
/// even running.
///
/// It is a **debugging record and nothing more**. No decision reads it; losing it costs an
/// explanation, never a repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorEvent {
    /// When it happened, on PD's clock.
    pub at_ms: u64,
    /// The region it was about.
    pub region_id: u64,
    /// Which operator.
    pub kind: EventKind,
    /// What happened to it.
    pub outcome: EventOutcome,
    /// The store it named — where a replica was going or leaving, or taking office.
    pub store_id: u64,
    /// The peer it named.
    pub peer_id: u64,
}

/// Which operator an [`OperatorEvent`] is about (*fixed*). Zero is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    /// `AddPeer`.
    AddPeer = 1,
    /// `RemovePeer`.
    RemovePeer = 2,
    /// `TransferLeader`.
    TransferLeader = 3,
}

/// What became of it (*fixed*). Zero is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventOutcome {
    /// PD asked for it.
    Issued = 1,
    /// A heartbeat showed it done.
    Done = 2,
    /// The region changed underneath it.
    Cancelled = 3,
    /// Nothing moved for the whole timeout.
    TimedOut = 4,
}

impl EventKind {
    /// The wire byte.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The kind for a byte, or `None` for one this version does not define.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::AddPeer),
            2 => Some(Self::RemovePeer),
            3 => Some(Self::TransferLeader),
            _ => None,
        }
    }

    /// The name a report prints.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::AddPeer => "AddPeer",
            Self::RemovePeer => "RemovePeer",
            Self::TransferLeader => "TransferLeader",
        }
    }
}

impl EventOutcome {
    /// The wire byte.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The outcome for a byte, or `None` for one this version does not define.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Issued),
            2 => Some(Self::Done),
            3 => Some(Self::Cancelled),
            4 => Some(Self::TimedOut),
            _ => None,
        }
    }

    /// The name a report prints.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
        }
    }
}

/// Events the history keeps. Oldest dropped when it is full.
///
/// Sixty-four is a few minutes of a busy repair and several hours of a quiet cluster, and it
/// keeps the whole ring in one record of a couple of kilobytes — which is why the ring is one
/// record rather than one key per event: a bounded thing that is written whole cannot leak keys,
/// and there is no cursor to keep.
pub const HISTORY_CAPACITY: usize = 64;

/// The ring of recent operator events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryRecord {
    /// Oldest first.
    pub events: Vec<OperatorEvent>,
}

impl HistoryRecord {
    /// Appends `event`, dropping the oldest if the ring is full.
    pub fn push(&mut self, event: OperatorEvent) {
        if self.events.len() >= HISTORY_CAPACITY {
            let over = self.events.len() - HISTORY_CAPACITY + 1;
            self.events.drain(..over);
        }
        self.events.push(event);
    }

    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        start(&mut out);
        out.put_varint(self.events.len() as u64);
        for event in &self.events {
            out.put_varint(event.at_ms);
            out.put_varint(event.region_id);
            out.put_u8(event.kind.as_u8());
            out.put_u8(event.outcome.as_u8());
            out.put_varint(event.store_id);
            out.put_varint(event.peer_id);
        }
        out.finish()
    }

    /// Reads the record back.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "history";
        let mut input = open(WHAT, bytes)?;
        let count = input.get_count("history.count").map_err(field(WHAT))?;
        let mut events = Vec::with_capacity(count);
        for _ in 0..count {
            let at_ms = input.get_varint("event.at_ms").map_err(field(WHAT))?;
            let region_id = input.get_varint("event.region_id").map_err(field(WHAT))?;
            let kind = input.get_u8("event.kind").map_err(field(WHAT))?;
            let outcome = input.get_u8("event.outcome").map_err(field(WHAT))?;
            let Some(kind) = EventKind::from_u8(kind) else {
                return Err(PdError::corrupt(WHAT, format!("operator kind {kind}")));
            };
            let Some(outcome) = EventOutcome::from_u8(outcome) else {
                return Err(PdError::corrupt(WHAT, format!("outcome {outcome}")));
            };
            events.push(OperatorEvent {
                at_ms,
                region_id,
                kind,
                outcome,
                store_id: input.get_varint("event.store_id").map_err(field(WHAT))?,
                peer_id: input.get_varint("event.peer_id").map_err(field(WHAT))?,
            });
        }
        close(WHAT, input)?;
        if events.len() > HISTORY_CAPACITY {
            return Err(PdError::corrupt(
                WHAT,
                format!("{} events, more than the ring holds", events.len()),
            ));
        }
        Ok(Self { events })
    }
}

/// The value of a range-index entry: which region ends at that key.
#[must_use]
pub fn encode_range_entry(region_id: u64) -> Vec<u8> {
    let mut out = Encoder::new();
    start(&mut out);
    out.put_varint(region_id);
    out.finish()
}

/// Reads a range-index entry back.
pub fn decode_range_entry(bytes: &[u8]) -> Result<u64> {
    const WHAT: &str = "range index";
    let mut input = open(WHAT, bytes)?;
    let region_id = input.get_varint("index.region_id").map_err(field(WHAT))?;
    close(WHAT, input)?;
    Ok(region_id)
}

/// `id ++ start ++ end ++ epoch ++ peers`. PD's own layout; see the module docs.
fn encode_region(region: &Region, out: &mut Encoder) {
    out.put_varint(region.id);
    out.put_bytes(&region.start_key);
    out.put_bytes(&region.end_key);
    out.put_varint(region.epoch.conf_ver);
    out.put_varint(region.epoch.version);
    out.put_varint(region.peers.len() as u64);
    for peer in &region.peers {
        out.put_varint(peer.store_id);
        out.put_varint(peer.peer_id);
        out.put_u8(peer.role.as_u8());
    }
}

fn decode_region(what: &'static str, input: &mut Decoder<'_>) -> Result<Region> {
    let id = input.get_varint("region.id").map_err(field(what))?;
    let start_key = Bytes::copy_from_slice(input.get_bytes("region.start").map_err(field(what))?);
    let end_key = Bytes::copy_from_slice(input.get_bytes("region.end").map_err(field(what))?);
    let epoch = Epoch {
        conf_ver: input.get_varint("region.conf_ver").map_err(field(what))?,
        version: input.get_varint("region.version").map_err(field(what))?,
    };
    let count = input.get_count("region.peers").map_err(field(what))?;
    let mut peers = Vec::with_capacity(count);
    for _ in 0..count {
        let store_id = input.get_varint("peer.store_id").map_err(field(what))?;
        let peer_id = input.get_varint("peer.peer_id").map_err(field(what))?;
        let byte = input.get_u8("peer.role").map_err(field(what))?;
        let Some(role) = PeerRole::from_u8(byte) else {
            return Err(PdError::corrupt(what, format!("peer role {byte}")));
        };
        peers.push(Peer {
            store_id,
            peer_id,
            role,
        });
    }
    Ok(Region {
        id,
        start_key,
        end_key,
        peers,
        epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AllocRecord, ClusterRecord, RECORD_VERSION, RegionRecord, StoreRecord, StoreStats,
        TsoRecord, decode_range_entry, encode_range_entry,
    };
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, PeerRole, Region};

    fn region() -> Region {
        Region {
            id: 7,
            start_key: Bytes::from_static(b"aaa"),
            end_key: Bytes::from_static(b"mmm"),
            peers: vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 2,
                    peer_id: 11,
                    role: PeerRole::Learner,
                },
            ],
            epoch: Epoch::new(3, 4),
        }
    }

    fn store() -> StoreRecord {
        StoreRecord {
            store_id: 3,
            address: "127.0.0.1:20160".to_owned(),
            started_ms: 1_700_000_000_000,
            last_heartbeat_ms: 1_700_000_010_000,
            stats: StoreStats {
                capacity: 1 << 40,
                available: 1 << 39,
                region_count: 12,
                leader_count: 4,
                applied_bytes: 999,
            },
        }
    }

    fn region_record() -> RegionRecord {
        RegionRecord {
            region: region(),
            leader_peer_id: 10,
            term: 5,
            approximate_size: 96 << 20,
            applied_index: 4242,
            last_heartbeat_ms: 1_700_000_020_000,
        }
    }

    #[test]
    fn every_record_round_trips() {
        let cluster = ClusterRecord {
            cluster_id: 0xDEAD_BEEF_CAFE,
            first_region_id: 1,
            created_ms: 1_700_000_000_000,
        };
        assert_eq!(ClusterRecord::decode(&cluster.encode()).unwrap(), cluster);

        let alloc = AllocRecord {
            allocated_end: 1_000,
        };
        assert_eq!(AllocRecord::decode(&alloc.encode()).unwrap(), alloc);

        let tso = TsoRecord {
            high_water_ms: 1_700_000_003_000,
        };
        assert_eq!(TsoRecord::decode(&tso.encode()).unwrap(), tso);

        assert_eq!(StoreRecord::decode(&store().encode()).unwrap(), store());
        assert_eq!(
            RegionRecord::decode(&region_record().encode()).unwrap(),
            region_record()
        );
        assert_eq!(decode_range_entry(&encode_range_entry(9)).unwrap(), 9);
    }

    /// The unbounded region is the one that has to survive the round trip intact: an empty end
    /// key means +∞, and a decoder that turned it into "an empty range" would make region 1 own
    /// nothing.
    #[test]
    fn the_first_region_round_trips_with_its_empty_bounds() {
        let record = RegionRecord::new(Region::bootstrap(1, 1, 1), 42);
        let back = RegionRecord::decode(&record.encode()).unwrap();
        assert_eq!(back, record);
        assert!(back.region.end_key.is_empty());
        assert!(back.region.contains(b"\xff\xff\xff\xff"));
    }

    /// A record written by a future version is refused, not misread. This is the check that
    /// makes a format change a migration rather than a silent misinterpretation.
    #[test]
    fn a_record_from_another_version_is_refused() {
        let mut bytes = store().encode();
        assert_eq!(bytes[0], RECORD_VERSION);
        bytes[0] = RECORD_VERSION + 1;
        assert!(StoreRecord::decode(&bytes).is_err());
    }

    #[test]
    fn a_trailing_byte_is_refused() {
        for mut bytes in [
            store().encode(),
            region_record().encode(),
            encode_range_entry(3),
        ] {
            bytes.push(0);
            assert!(
                StoreRecord::decode(&bytes).is_err()
                    && RegionRecord::decode(&bytes).is_err()
                    && decode_range_entry(&bytes).is_err()
            );
        }
    }

    /// Truncating any record at any offset is an error and never a panic (invariant 9).
    #[test]
    fn truncation_never_panics() {
        let records: Vec<Vec<u8>> = vec![
            ClusterRecord {
                cluster_id: 5,
                first_region_id: 1,
                created_ms: 9,
            }
            .encode(),
            AllocRecord { allocated_end: 7 }.encode(),
            TsoRecord { high_water_ms: 7 }.encode(),
            store().encode(),
            region_record().encode(),
            encode_range_entry(3),
        ];
        for bytes in records {
            for cut in 0..bytes.len() {
                let short = &bytes[..cut];
                let _ = ClusterRecord::decode(short);
                let _ = AllocRecord::decode(short);
                let _ = TsoRecord::decode(short);
                let _ = StoreRecord::decode(short);
                let _ = RegionRecord::decode(short);
                let _ = decode_range_entry(short);
            }
        }
    }

    fn event(region_id: u64, outcome: super::EventOutcome) -> super::OperatorEvent {
        super::OperatorEvent {
            at_ms: 1_700_000_000_000,
            region_id,
            kind: super::EventKind::AddPeer,
            outcome,
            store_id: 4,
            peer_id: 41,
        }
    }

    #[test]
    fn the_history_round_trips_and_keeps_the_newest() {
        let mut history = super::HistoryRecord::default();
        for region_id in 0..u64::try_from(super::HISTORY_CAPACITY).unwrap() + 10 {
            history.push(event(region_id, super::EventOutcome::Issued));
        }
        assert_eq!(history.events.len(), super::HISTORY_CAPACITY);
        assert_eq!(
            history.events[0].region_id, 10,
            "the ring dropped from the front"
        );
        assert_eq!(
            super::HistoryRecord::decode(&history.encode()).unwrap(),
            history
        );
    }

    /// A kind or an outcome this version does not define is an error, not a default — the same
    /// rule every tag in this codebase follows.
    #[test]
    fn an_unknown_kind_or_outcome_is_refused() {
        let mut history = super::HistoryRecord::default();
        history.push(event(7, super::EventOutcome::Done));
        let good = history.encode();
        // version ++ count ++ at_ms(6) ++ region_id(1) ++ kind ++ outcome
        let kind_at = good.len() - 4;
        assert_eq!(good[kind_at], super::EventKind::AddPeer.as_u8());
        for byte in [0u8, 4, 200] {
            let mut bytes = good.clone();
            bytes[kind_at] = byte;
            assert!(super::HistoryRecord::decode(&bytes).is_err(), "kind {byte}");
            let mut bytes = good.clone();
            bytes[kind_at + 1] = byte.wrapping_add(u8::from(byte == 4));
            if super::EventOutcome::from_u8(bytes[kind_at + 1]).is_none() {
                assert!(super::HistoryRecord::decode(&bytes).is_err());
            }
        }
    }

    /// A cluster id of zero is not an id, the same rule every tag and role in this codebase
    /// follows: a zeroed record must not decode as a valid one.
    #[test]
    fn a_zero_cluster_id_is_corruption() {
        let bytes = ClusterRecord {
            cluster_id: 0,
            first_region_id: 1,
            created_ms: 0,
        }
        .encode();
        assert!(ClusterRecord::decode(&bytes).is_err());
    }

    /// A peer role byte this version does not define is an error, not a voter.
    #[test]
    fn an_unknown_peer_role_is_refused() {
        let record = RegionRecord {
            region: Region {
                peers: vec![Peer::voter(1, 1)],
                ..region()
            },
            // Every trailing field zero, so each is a single-byte varint and the role byte is
            // at a position this test can name rather than search for.
            leader_peer_id: 0,
            term: 0,
            approximate_size: 0,
            applied_index: 0,
            last_heartbeat_ms: 0,
        };
        let mut bytes = record.encode();
        let at = bytes.len() - 6;
        assert_eq!(bytes[at], PeerRole::Voter.as_u8());
        bytes[at] = 9;
        assert!(RegionRecord::decode(&bytes).is_err());
    }
}
