//! Regions, epochs and peers: the routing types that both requests and errors carry.
//!
//! A **region** is a contiguous key range replicated by one Raft group (`docs/DESIGN.md` §6).
//! Regions tile the key space, so the first one is `["", "")` and an empty `end_key` always
//! means "to the end of the key space" rather than "an empty range".
//!
//! An **epoch** is `(conf_ver, version)`: `conf_ver` bumps on a membership change and
//! `version` on a split. `CLAUDE.md` invariant 5 turns on it — every request carries the epoch
//! the client believes, and a store refuses to serve a range it no longer owns. In this phase
//! there is one region whose epoch never moves, and the check runs anyway: an invariant that
//! is only enforced once it can fail is one that was never tested.
//!
//! These live beside the framing rather than in [`crate::messages`] because
//! [`crate::ProtoError::EpochNotMatch`] carries them too — the redirect hint *is* the region.

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};

/// `(conf_ver, version)` — the region's epoch (`docs/DESIGN.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Epoch {
    /// Bumped by a membership change.
    pub conf_ver: u64,
    /// Bumped by a split.
    pub version: u64,
}

impl Epoch {
    /// The epoch a bootstrapped region starts at.
    pub const INITIAL: Self = Self {
        conf_ver: 1,
        version: 1,
    };

    /// An epoch with both counters given.
    #[must_use]
    pub fn new(conf_ver: u64, version: u64) -> Self {
        Self { conf_ver, version }
    }

    /// Whether `self` is behind `other` in either counter.
    ///
    /// The two counters move independently — a split does not bump `conf_ver`, a membership
    /// change does not bump `version` — so "stale" is not a single comparison. A client behind
    /// in *either* one is looking at a region that has changed underneath it.
    #[must_use]
    pub fn is_stale_against(self, other: Self) -> bool {
        self.conf_ver < other.conf_ver || self.version < other.version
    }

    pub(crate) fn encode(self, out: &mut Encoder) {
        out.put_varint(self.conf_ver);
        out.put_varint(self.version);
    }

    /// What [`Epoch::encode`] writes, in bytes.
    pub(crate) fn encoded_len(self) -> usize {
        crate::codec::varint_len(self.conf_ver) + crate::codec::varint_len(self.version)
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            conf_ver: input.get_varint("epoch.conf_ver")?,
            version: input.get_varint("epoch.version")?,
        })
    }
}

/// What a peer is allowed to do in its Raft group (*fixed*).
///
/// Zero is reserved, as everywhere else in this format: a zeroed byte must not decode as a
/// voting member of a consensus group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PeerRole {
    /// Votes in elections and counts towards a quorum.
    Voter = 1,
    /// Receives the log but neither votes nor counts towards a quorum
    /// (`docs/DESIGN.md` §5).
    ///
    /// **On its way to being a voter.** This is the transient state of an `AddPeer`: the store
    /// catches the replica up and the leader promotes it. A learner that must *never* be
    /// promoted is [`PeerRole::ColumnarLearner`], and the two have to be different values for
    /// exactly that reason.
    Learner = 2,
    /// A columnar replica: receives the log, never votes, and is **never promoted**
    /// ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 1).
    ///
    /// # Why this is a role and not a flag somewhere else
    ///
    /// A store promotes learners that have caught up, and it decides that from the region
    /// record. With two roles, a columnar replica is indistinguishable from one being caught up,
    /// so the next promotion round makes it a voter — at which point it counts towards a quorum
    /// and is asked to serve row reads it holds no rows for. The failure arrives minutes after a
    /// placement that appeared to succeed, and reads as a placement bug rather than as this.
    ///
    /// It is **per peer** because one region can hold both at once: a replica being caught up on
    /// its way to voting, and a columnar copy that never will. A marker on the region could not
    /// tell them apart.
    ///
    /// It is in the **region record** because promotion is the *leader's* decision, taken
    /// elsewhere. A store hosting a columnar replica knows perfectly well what it holds; the
    /// leader is the one that needs telling.
    ///
    /// # Raft is untouched
    ///
    /// To Raft this is a learner and nothing else: the conf change stays
    /// `ConfChangeKind::AddLearner`, quorums are unchanged, and `esker-raft` has no idea the
    /// distinction exists. What carries it is the conf change's **context**, which
    /// `esker-raft` documents as caller data it never interprets and which `esker-store` already
    /// uses to carry a store id. Because the context is replicated in the log, every peer —
    /// the leader included — applies the same role.
    ColumnarLearner = 3,
}

impl PeerRole {
    /// Every role this version defines.
    pub const ALL: [Self; 3] = [Self::Voter, Self::Learner, Self::ColumnarLearner];

    /// The wire byte.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The role for a wire byte, or `None` for one this version does not define.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Voter),
            2 => Some(Self::Learner),
            3 => Some(Self::ColumnarLearner),
            _ => None,
        }
    }
}

/// One replica of one region on one store (`docs/DESIGN.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Peer {
    /// The store hosting this replica.
    pub store_id: u64,
    /// This replica's id, unique across the cluster.
    pub peer_id: u64,
    /// Voter or learner.
    pub role: PeerRole,
}

impl Peer {
    /// A voting peer.
    #[must_use]
    pub fn voter(store_id: u64, peer_id: u64) -> Self {
        Self {
            store_id,
            peer_id,
            role: PeerRole::Voter,
        }
    }

    /// A peer that receives the log but neither votes nor counts towards a quorum.
    ///
    /// Two things are learners and the wire does not tell them apart, on purpose: a replica
    /// being caught up before it is promoted to a voter, and a columnar replica that is never
    /// promoted at all ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 1).
    /// Raft treats them identically because there is nothing to treat differently; what differs
    /// is only which operator asked for one, which is PD's business and not this type's.
    #[must_use]
    pub fn learner(store_id: u64, peer_id: u64) -> Self {
        Self {
            store_id,
            peer_id,
            role: PeerRole::Learner,
        }
    }

    pub(crate) fn encode(self, out: &mut Encoder) {
        out.put_varint(self.store_id);
        out.put_varint(self.peer_id);
        out.put_u8(self.role.as_u8());
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let store_id = input.get_varint("peer.store_id")?;
        let peer_id = input.get_varint("peer.peer_id")?;
        let byte = input.get_u8("peer.role")?;
        let Some(role) = PeerRole::from_u8(byte) else {
            return Err(DecodeError::UnknownTag {
                what: "peer role",
                tag: u64::from(byte),
            });
        };
        Ok(Self {
            store_id,
            peer_id,
            role,
        })
    }
}

/// A contiguous key range replicated by one Raft group (`docs/DESIGN.md` §6).
///
/// The epoch is a field of the region rather than a sibling of it, because it *is* the
/// region's version: a client holding a `Region` holds everything needed to address it and to
/// know whether what it holds is still true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// Cluster-unique region id.
    pub id: u64,
    /// Inclusive start of the range.
    pub start_key: Bytes,
    /// Exclusive end of the range. Empty means "to the end of the key space".
    pub end_key: Bytes,
    /// Every replica, in no particular order.
    pub peers: Vec<Peer>,
    /// `(conf_ver, version)`.
    pub epoch: Epoch,
}

impl Region {
    /// The first region of a new cluster: id 1, covering everything, one voting peer
    /// (`docs/DESIGN.md` §7, "the first store to register receives region 1").
    #[must_use]
    pub fn bootstrap(id: u64, store_id: u64, peer_id: u64) -> Self {
        Self {
            id,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(store_id, peer_id)],
            epoch: Epoch::INITIAL,
        }
    }

    /// Whether `key` falls inside this region's range.
    ///
    /// An empty `end_key` means the range runs to the end of the key space, which is why this
    /// cannot be a plain range comparison.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= &self.start_key[..] && (self.end_key.is_empty() || key < &self.end_key[..])
    }

    /// Whether `[start, end)` is entirely inside this region, where an empty `end` means the
    /// end of the key space. An empty range — `start == end` — is inside if `start` is.
    #[must_use]
    pub fn contains_range(&self, start: &[u8], end: &[u8]) -> bool {
        if start < &self.start_key[..] {
            return false;
        }
        if self.end_key.is_empty() {
            // This region runs to the end of the key space, so nothing can be past its end.
            return true;
        }
        if end.is_empty() {
            // The request runs to the end of the key space and this region does not.
            return false;
        }
        end <= &self.end_key[..]
    }

    /// Whether this region shares any key with `[start, end)`, where an **empty `end` means the
    /// end of the key space** — the same convention as [`Region::end_key`].
    ///
    /// This is the question `EpochNotMatch` is answered with: a client whose cached region has
    /// split is asking for a range that is now several regions, and every local one that overlaps
    /// it is a region the client needs. Written out rather than left to `Ord` for the reason the
    /// module doc gives — `b""` as an upper bound sorts below every key, so a comparison that
    /// forgets the convention silently reports no overlap for the last region in the cluster.
    ///
    /// An empty request range — `start == end`, both non-empty — is a point at `start`: it
    /// overlaps the region containing `start` and nothing else.
    #[must_use]
    pub fn overlaps(&self, start: &[u8], end: &[u8]) -> bool {
        if start == end && !start.is_empty() {
            return self.contains(start);
        }
        // `self` ends at or before the request begins.
        let before = !self.end_key.is_empty() && &self.end_key[..] <= start;
        // The request ends at or before `self` begins.
        let after = !end.is_empty() && end <= &self.start_key[..];
        !(before || after)
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        out.put_varint(self.id);
        out.put_bytes(&self.start_key);
        out.put_bytes(&self.end_key);
        self.epoch.encode(out);
        out.put_varint(self.peers.len() as u64);
        for peer in &self.peers {
            peer.encode(out);
        }
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let id = input.get_varint("region.id")?;
        let start_key = Bytes::copy_from_slice(input.get_bytes("region.start_key")?);
        let end_key = Bytes::copy_from_slice(input.get_bytes("region.end_key")?);
        let epoch = Epoch::decode(input)?;
        let count = input.get_count("region.peers")?;
        let mut peers = Vec::with_capacity(count);
        for _ in 0..count {
            peers.push(Peer::decode(input)?);
        }
        Ok(Self {
            id,
            start_key,
            end_key,
            peers,
            epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Epoch, Peer, PeerRole, Region};
    use crate::codec::{Decoder, Encoder};
    use bytes::Bytes;

    fn round_trip(region: &Region) -> Region {
        let mut out = Encoder::new();
        region.encode(&mut out);
        let bytes = out.finish();
        let mut input = Decoder::new(&bytes);
        let back = Region::decode(&mut input).unwrap();
        input.finish().unwrap();
        back
    }

    #[test]
    fn a_region_round_trips() {
        let region = Region {
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
        };
        assert_eq!(round_trip(&region), region);

        let bootstrap = Region::bootstrap(1, 1, 1);
        assert_eq!(round_trip(&bootstrap), bootstrap);
        assert_eq!(bootstrap.epoch, Epoch::INITIAL);
    }

    /// An empty `end_key` is the end of the key space, not an empty range. Getting this
    /// backwards makes the first region of a cluster own nothing.
    #[test]
    fn an_empty_end_key_means_the_end_of_the_key_space() {
        let whole = Region::bootstrap(1, 1, 1);
        for key in [&b""[..], b"a", b"\xff\xff\xff"] {
            assert!(whole.contains(key), "{key:?}");
        }
        assert!(whole.contains_range(b"", b""));
        assert!(whole.contains_range(b"a", b"z"));
    }

    #[test]
    fn a_bounded_region_owns_exactly_its_range() {
        let region = Region {
            id: 2,
            start_key: Bytes::from_static(b"d"),
            end_key: Bytes::from_static(b"m"),
            peers: Vec::new(),
            epoch: Epoch::INITIAL,
        };
        assert!(!region.contains(b"c"));
        assert!(region.contains(b"d"), "start is inclusive");
        assert!(region.contains(b"l"));
        assert!(!region.contains(b"m"), "end is exclusive");

        assert!(region.contains_range(b"d", b"m"));
        assert!(!region.contains_range(b"c", b"m"));
        assert!(!region.contains_range(b"d", b"n"));
        assert!(
            !region.contains_range(b"d", b""),
            "an unbounded scan runs past a bounded region"
        );
    }

    /// The two counters move on different events, so a client can be stale in one and current
    /// in the other. A single `<` on a packed value would miss half the cases.
    #[test]
    fn either_counter_being_behind_is_stale() {
        let current = Epoch::new(2, 3);
        assert!(
            Epoch::new(1, 3).is_stale_against(current),
            "conf_ver behind"
        );
        assert!(Epoch::new(2, 2).is_stale_against(current), "version behind");
        assert!(!current.is_stale_against(current));
        assert!(
            !Epoch::new(3, 4).is_stale_against(current),
            "ahead, not stale"
        );
    }

    /// The golden. These bytes are a wire format — a region travels in `EpochNotMatch`, in
    /// `GetRegion` and in every region heartbeat — so changing them is a format change and needs
    /// an ADR and a `WIRE_VERSION` bump (`docs/adr/0002-formats-are-hand-rolled.md`).
    ///
    /// Written out by hand rather than captured from the encoder, so it disagrees when the
    /// encoder moves instead of moving with it.
    #[test]
    fn a_region_encodes_to_the_documented_bytes() {
        let region = Region {
            id: 2,
            start_key: Bytes::from_static(b"d"),
            end_key: Bytes::from_static(b"m"),
            peers: vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 2,
                    peer_id: 300,
                    role: PeerRole::Learner,
                },
            ],
            epoch: Epoch::new(3, 4),
        };
        let mut out = Encoder::new();
        region.encode(&mut out);
        assert_eq!(
            out.finish(),
            vec![
                2,    // id
                1,    // start_key: one byte
                b'd', //
                1,    // end_key: one byte
                b'm', //
                3,    // epoch.conf_ver
                4,    // epoch.version
                2,    // peers: two of them
                1,    // peers[0].store_id
                10,   // peers[0].peer_id
                1,    // peers[0].role: Voter
                2,    // peers[1].store_id
                0xAC, // peers[1].peer_id: 300 is two varint bytes
                0x02, //
                2,    // peers[1].role: Learner
            ],
        );
    }

    /// Region 1 as a cluster bootstraps it, byte for byte. Both key fields are an empty
    /// length-prefixed string — *not* absent, and not a sentinel: the emptiness of `end_key` is
    /// what means "+∞", and it has to be on the wire for the far end to read it.
    #[test]
    fn the_bootstrap_region_encodes_to_the_documented_bytes() {
        let mut out = Encoder::new();
        Region::bootstrap(1, 1, 1).encode(&mut out);
        assert_eq!(
            out.finish(),
            vec![
                1, // id
                0, // start_key: empty
                0, // end_key: empty, meaning the end of the key space
                1, // epoch.conf_ver
                1, // epoch.version
                1, // peers: one
                1, // peers[0].store_id
                1, // peers[0].peer_id
                1, // peers[0].role: Voter
            ],
        );
    }

    /// The comparison `EpochNotMatch` is built from. Every case that a naive `Ord` on the two
    /// key pairs would get wrong is here: the unbounded region, the unbounded request, and the
    /// two ranges that merely touch at a boundary without sharing a key.
    #[test]
    fn overlap_is_decided_with_the_empty_end_key_convention() {
        let middle = Region {
            id: 2,
            start_key: Bytes::from_static(b"g"),
            end_key: Bytes::from_static(b"q"),
            peers: Vec::new(),
            epoch: Epoch::INITIAL,
        };

        assert!(middle.overlaps(b"g", b"q"), "exactly itself");
        assert!(middle.overlaps(b"", b""), "the whole key space");
        assert!(middle.overlaps(b"a", b"h"), "straddles the start");
        assert!(middle.overlaps(b"p", b"z"), "straddles the end");
        assert!(middle.overlaps(b"h", b"i"), "strictly inside");
        assert!(
            middle.overlaps(b"a", b""),
            "an unbounded request from below"
        );
        assert!(!middle.overlaps(b"a", b"g"), "ends where the region starts");
        assert!(!middle.overlaps(b"q", b"z"), "starts where the region ends");
        assert!(
            !middle.overlaps(b"q", b""),
            "unbounded, but starting past it"
        );
        assert!(!middle.overlaps(b"a", b"f"), "entirely below");

        // A point request is the key it names.
        assert!(middle.overlaps(b"h", b"h"));
        assert!(!middle.overlaps(b"z", b"z"));

        // The last region in the cluster is the case a forgotten convention loses: its end is
        // empty, so every request above its start overlaps it.
        let last = Region {
            id: 3,
            start_key: Bytes::from_static(b"q"),
            end_key: Bytes::new(),
            peers: Vec::new(),
            epoch: Epoch::INITIAL,
        };
        assert!(last.overlaps(b"z", b""));
        assert!(last.overlaps(b"z", b"zz"));
        assert!(last.overlaps(b"\xff\xff", b"\xff\xff"));
        assert!(!last.overlaps(b"a", b"q"));
    }

    #[test]
    fn peer_roles_are_distinct_and_nonzero() {
        assert_eq!(PeerRole::from_u8(0), None);
        for role in PeerRole::ALL {
            assert_eq!(PeerRole::from_u8(role.as_u8()), Some(role));
        }
    }

    #[test]
    fn an_unknown_peer_role_is_an_error() {
        let mut out = Encoder::new();
        out.put_varint(1);
        out.put_bytes(b"");
        out.put_bytes(b"");
        Epoch::INITIAL.encode(&mut out);
        out.put_varint(1);
        out.put_varint(1);
        out.put_varint(1);
        out.put_u8(9); // not a role
        let bytes = out.finish();
        assert!(Region::decode(&mut Decoder::new(&bytes)).is_err());
    }
}
