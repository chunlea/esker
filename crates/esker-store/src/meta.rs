//! The region metadata record: `'m' ++ region_id` on the `raft` column family.
//!
//! This is the answer to *which regions does this store host?* — and it is on disk rather than in
//! a config file or in the placement driver's reply, because **the persisted thing is the anchor a
//! restart replays from**. That is the same rule phase 3 arrived at for the Raft configuration
//! (`91de89a`, `86d9824`): a store that asked PD what it hosts would serve whatever PD currently
//! believes, which is not the same question as what its own log and data say.
//!
//! # Its own encoding, deliberately
//!
//! [`Region`] already knows how to put itself on the wire, and this record does **not** use it.
//! The raft column family is an on-disk format and the wire is not; they must be able to move
//! independently (`docs/plans/phase-3.md` §11.2, `docs/adr/0002-formats-are-hand-rolled.md`). The
//! two layouts are near-identical today, which is exactly when sharing one encoder looks free and
//! is not: the first time a wire field is added for a client's benefit, every database written
//! before it would become unreadable.
//!
//! ```text
//! 'm' ++ region_id:u64 BE  →  version:u8 ++ id ++ start_key ++ end_key
//!                             ++ conf_ver ++ version ++ peer_count ++ peers
//! peer                     =  store_id ++ peer_id ++ role:u8
//! ```
//!
//! Everything unmarked is a varint; `start_key` and `end_key` are length-prefixed. An **empty
//! `end_key` means the end of the key space**, which is why it is written as a length-prefixed
//! empty string rather than omitted: absence and emptiness would be the same bytes, and one of
//! them means "region 1 owns everything".
//!
//! # What a restart does with them
//!
//! [`load_regions`] returns every record. The caller starts a peer for each — but only for those
//! whose peer list contains *this store*. A record naming a store that is no longer a member is
//! what a crash between `RemovePeer` applying and this store deleting its data leaves behind
//! (`docs/plans/phase-4.md` §6, race 3), and starting a peer for it would put a voter back into a
//! group that has already removed it.

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, cf};
use esker_proto::{Decoder, Encoder, Epoch, Peer, PeerRole, Region};

use crate::error::{Result, StoreError};
use crate::raft_cf;
use crate::raft_log::{REGION_KEY_LEN, metadata_key, pending_snapshot_key};

/// Version byte on a region metadata record. A change to any field's meaning bumps it.
const METADATA_FORMAT_VERSION: u8 = 1;

/// The record's bytes.
#[must_use]
pub fn encode_region(region: &Region) -> Vec<u8> {
    let mut out = Encoder::with_capacity(32 + region.start_key.len() + region.end_key.len());
    out.put_u8(METADATA_FORMAT_VERSION);
    encode_region_into(&mut out, region);
    out.finish()
}

/// A region's fields, without the record's version byte.
///
/// Shared with the snapshot stream's header ([`crate::snapshot`]), which describes a region for a
/// different reason and versions itself separately. The *fields* are the same question — what a
/// region is — and writing them twice is how the two answers drift.
pub fn encode_region_into(out: &mut Encoder, region: &Region) {
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

/// Reads a record written by [`encode_region`].
///
/// Bytes off a disk are never trusted (`CLAUDE.md` invariant 2): a wrong version byte, an unknown
/// peer role, a range whose start is above its end, and trailing bytes are all errors. A region
/// with no peers is one too — a region this store has a record for is one it hosts a peer of, and
/// a record that names none cannot say which peer that is.
pub fn decode_region(bytes: &[u8]) -> Result<Region> {
    let mut input = Decoder::new(bytes);
    let version = input
        .get_u8("region.version")
        .map_err(|error| corrupt(&error))?;
    if version != METADATA_FORMAT_VERSION {
        return Err(StoreError::Bootstrap(format!(
            "region metadata record has format version {version}, expected {METADATA_FORMAT_VERSION}"
        )));
    }
    decode_region_from(&mut input).and_then(|region| {
        input.finish().map_err(|error| corrupt(&error))?;
        Ok(region)
    })
}

/// A region's fields, without the record's version byte and without consuming the input's end.
///
/// The counterpart of [`encode_region_into`], and the place every "is this region possible"
/// check lives: bytes off a disk or a socket are never trusted (`CLAUDE.md` invariant 2).
pub fn decode_region_from(input: &mut Decoder<'_>) -> Result<Region> {
    let id = input.get_varint("region.id").map_err(|e| corrupt(&e))?;
    let start_key = Bytes::copy_from_slice(
        input
            .get_bytes("region.start_key")
            .map_err(|e| corrupt(&e))?,
    );
    let end_key =
        Bytes::copy_from_slice(input.get_bytes("region.end_key").map_err(|e| corrupt(&e))?);
    let conf_ver = input
        .get_varint("region.conf_ver")
        .map_err(|e| corrupt(&e))?;
    let version = input.get_varint("region.epoch").map_err(|e| corrupt(&e))?;
    let count = input.get_count("region.peers").map_err(|e| corrupt(&e))?;
    let mut peers = Vec::with_capacity(count.min(16));
    for _ in 0..count {
        let store_id = input
            .get_varint("region.peer.store")
            .map_err(|e| corrupt(&e))?;
        let peer_id = input
            .get_varint("region.peer.id")
            .map_err(|e| corrupt(&e))?;
        let byte = input.get_u8("region.peer.role").map_err(|e| corrupt(&e))?;
        let Some(role) = PeerRole::from_u8(byte) else {
            return Err(StoreError::Bootstrap(format!(
                "region {id} has a peer with role byte {byte}, which this version does not define"
            )));
        };
        peers.push(Peer {
            store_id,
            peer_id,
            role,
        });
    }

    if peers.is_empty() {
        return Err(StoreError::Bootstrap(format!(
            "region {id} has no peers; a record on this store must name the peer it hosts"
        )));
    }
    // An empty end key is the end of the key space, so only a *non-empty* one can be too low.
    if !end_key.is_empty() && start_key >= end_key {
        return Err(StoreError::Bootstrap(format!(
            "region {id} has start {start_key:?} at or above its end {end_key:?}"
        )));
    }
    Ok(Region {
        id,
        start_key,
        end_key,
        peers,
        epoch: Epoch::new(conf_ver, version),
    })
}

/// Adds `region`'s record to `batch`.
///
/// Staged rather than written, so a caller can put it in the same batch as whatever made it true:
/// a bootstrap writes it beside the region's first Raft state, and `TODO(phase-4b)` a split writes
/// both halves' records in the batch that applies the split entry — which is what makes a split
/// atomic on a peer rather than a sequence a crash can land inside.
pub fn stage_region(batch: &mut WriteBatch, cf: u32, region: &Region) {
    batch.put(cf, &metadata_key(region.id), &encode_region(region));
}

/// Announces that a snapshot is being applied to a region, at `index`.
///
/// Written **before** anything else the receive touches, and removed only once the region is
/// complete. Between the two, a restart finds the record and knows not to start the region: its
/// data may be part of a snapshot and part of nothing, which is the one state that must never be
/// served (`docs/plans/phase-4.md` §13.1).
///
/// It carries the whole region rather than just its id, and that is what makes the recovery
/// possible: the keys a partial receive left behind are in the region's **range**, and a restart
/// that knew only an id could not find them — nor could the retry, which would then refuse to
/// start because the range it was given is not empty.
pub fn stage_pending_snapshot(batch: &mut WriteBatch, cf: u32, region: &Region, index: u64) {
    let mut out = Encoder::new();
    out.put_u8(METADATA_FORMAT_VERSION);
    out.put_varint(index);
    encode_region_into(&mut out, region);
    batch.put(cf, &pending_snapshot_key(region.id), &out.finish());
}

/// Removes the announcement, which is what makes the region complete.
pub fn stage_snapshot_done(batch: &mut WriteBatch, cf: u32, region_id: u64) {
    batch.delete(cf, &pending_snapshot_key(region_id));
}

/// Every region a snapshot was part-way into when this store last stopped, with the index each
/// was being brought to.
pub fn load_pending_snapshots(db: &Db) -> Result<Vec<(Region, u64)>> {
    let mut iter = db.iter(cf::RAFT, &ReadOptions::default())?;
    let mut pending = Vec::new();
    iter.seek(&[raft_cf::PENDING_SNAPSHOT]);
    while iter.valid() {
        let key = iter.key();
        if key.first() != Some(&raft_cf::PENDING_SNAPSHOT) {
            break;
        }
        if key.len() != REGION_KEY_LEN {
            return Err(StoreError::Bootstrap(format!(
                "a pending-snapshot key is {} bytes, expected {REGION_KEY_LEN}",
                key.len()
            )));
        }
        let mut id = [0_u8; 8];
        id.copy_from_slice(&key[1..]);
        let region_id = u64::from_be_bytes(id);

        let mut input = Decoder::new(iter.value());
        let version = input
            .get_u8("pending.version")
            .map_err(|error| corrupt(&error))?;
        if version != METADATA_FORMAT_VERSION {
            return Err(StoreError::Bootstrap(format!(
                "a pending-snapshot record has format version {version}, expected \
                 {METADATA_FORMAT_VERSION}"
            )));
        }
        let index = input
            .get_varint("pending.index")
            .map_err(|error| corrupt(&error))?;
        let region = decode_region_from(&mut input)?;
        input.finish().map_err(|error| corrupt(&error))?;
        if region.id != region_id {
            return Err(StoreError::Bootstrap(format!(
                "the pending-snapshot record under key {region_id} says it is region {}",
                region.id
            )));
        }
        pending.push((region, index));
        iter.next();
    }
    iter.status()?;
    Ok(pending)
}

/// Adds the removal of a region's record to `batch`.
///
/// `TODO(phase-4c)`: the `RemovePeer` operator is what calls this, in the batch that also deletes
/// the region's data and its Raft log.
pub fn stage_removal(batch: &mut WriteBatch, cf: u32, region_id: u64) {
    batch.delete(cf, &metadata_key(region_id));
}

/// Every region this store has a record for, in id order.
///
/// Scans the `'m'` prefix of the `raft` column family. Records are keyed by big-endian region id,
/// so the scan is already in id order and nothing sorts it.
pub fn load_regions(db: &Db) -> Result<Vec<Region>> {
    let mut iter = db.iter(cf::RAFT, &ReadOptions::default())?;
    let mut regions = Vec::new();
    iter.seek(&[raft_cf::METADATA]);
    while iter.valid() {
        let key = iter.key();
        if key.first() != Some(&raft_cf::METADATA) {
            break;
        }
        if key.len() != REGION_KEY_LEN {
            return Err(StoreError::Bootstrap(format!(
                "a region metadata key is {} bytes, expected {REGION_KEY_LEN}",
                key.len()
            )));
        }
        let mut id = [0_u8; 8];
        id.copy_from_slice(&key[1..]);
        let keyed = u64::from_be_bytes(id);

        let region = decode_region(iter.value())?;
        // The id is in the key *and* in the value. It costs a varint and buys the same check the
        // log entries make: a value read under the wrong key is caught rather than returned as a
        // region that claims to be somewhere it is not.
        if region.id != keyed {
            return Err(StoreError::Bootstrap(format!(
                "the region metadata record under key {keyed} says it is region {}",
                region.id
            )));
        }
        regions.push(region);
        iter.next();
    }
    iter.status()?;
    Ok(regions)
}

fn corrupt(error: &esker_proto::DecodeError) -> StoreError {
    StoreError::Bootstrap(format!("corrupt region metadata: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{decode_region, encode_region, load_regions, stage_region, stage_removal};
    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, WriteBatch, WriteOptions, cf};
    use esker_proto::{Epoch, Peer, PeerRole, Region};
    use std::sync::Arc;

    fn region(id: u64, start: &[u8], end: &[u8]) -> Region {
        Region {
            id,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, id * 10)],
            epoch: Epoch::INITIAL,
        }
    }

    fn open() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap();
        (dir, Arc::new(db))
    }

    fn write(db: &Db, regions: &[Region]) {
        let cf_id = db.cf_id(cf::RAFT).unwrap();
        let mut batch = WriteBatch::new();
        for region in regions {
            stage_region(&mut batch, cf_id, region);
        }
        db.write(batch, &WriteOptions { sync: true }).unwrap();
    }

    #[test]
    fn a_record_round_trips() {
        let full = Region {
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
        assert_eq!(decode_region(&encode_region(&full)).unwrap(), full);

        let bootstrap = Region::bootstrap(1, 1, 1);
        assert_eq!(
            decode_region(&encode_region(&bootstrap)).unwrap(),
            bootstrap
        );
    }

    /// The golden. This is an on-disk format: changing these bytes makes every database written
    /// before the change unreadable, so it needs an ADR and a version bump
    /// (`docs/adr/0002-formats-are-hand-rolled.md`).
    ///
    /// Region 1 is the case the rest of phase 4 leans on. Both key fields are a length-prefixed
    /// empty string — the emptiness of `end_key` is what means "+∞", so it has to be *written*,
    /// not omitted.
    #[test]
    fn the_bootstrap_region_is_stored_as_the_documented_bytes() {
        assert_eq!(
            encode_region(&Region::bootstrap(1, 1, 1)),
            vec![
                1, // format version
                1, // region id
                0, // start_key: empty
                0, // end_key: empty, meaning the end of the key space
                1, // epoch.conf_ver
                1, // epoch.version
                1, // one peer
                1, // peers[0].store_id
                1, // peers[0].peer_id
                1, // peers[0].role: Voter
            ]
        );
    }

    /// Bytes off a disk are never trusted. Each of these is a way a record could be wrong that a
    /// decoder which merely parsed would pass along as a region.
    #[test]
    fn a_record_that_cannot_be_true_is_an_error_not_a_region() {
        let good = encode_region(&region(2, b"d", b"m"));

        let mut wrong_version = good.clone();
        wrong_version[0] = 2;
        assert!(decode_region(&wrong_version).is_err(), "format version");

        let mut truncated = good.clone();
        truncated.pop();
        assert!(decode_region(&truncated).is_err(), "a short record");

        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_region(&trailing).is_err(), "trailing bytes");

        let mut bad_role = good.clone();
        let last = bad_role.len() - 1;
        bad_role[last] = 9;
        assert!(decode_region(&bad_role).is_err(), "an undefined peer role");

        assert!(
            decode_region(&encode_region(&Region {
                peers: Vec::new(),
                ..region(3, b"", b"")
            }))
            .is_err(),
            "a region this store hosts must name the peer it hosts"
        );

        assert!(
            decode_region(&encode_region(&region(4, b"m", b"d"))).is_err(),
            "a start above its end"
        );
        assert!(
            decode_region(&encode_region(&region(5, b"m", b"m"))).is_err(),
            "an empty range owns nothing and cannot be a region"
        );
        // But an empty *end* is the end of the key space, not a low bound.
        decode_region(&encode_region(&region(6, b"m", b""))).unwrap();
    }

    #[test]
    fn every_record_comes_back_in_id_order() {
        let (_dir, db) = open();
        assert!(load_regions(&db).unwrap().is_empty(), "a fresh database");

        let regions = vec![
            region(3, b"q", b""),
            region(1, b"", b"g"),
            region(2, b"g", b"q"),
        ];
        write(&db, &regions);

        let loaded = load_regions(&db).unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.id).collect::<Vec<_>>(),
            [1, 2, 3],
            "big-endian keys mean the scan is already in id order"
        );
        assert_eq!(loaded[2].start_key, Bytes::from_static(b"q"));
        assert_eq!(loaded[2].end_key, Bytes::new());
    }

    /// The scan must stop at the end of the `'m'` prefix. The state records under `'s'` sort
    /// immediately after it, and reading one as a region would be a decode error at best.
    #[test]
    fn the_scan_stops_at_the_end_of_its_prefix() {
        let (_dir, db) = open();
        let cf_id = db.cf_id(cf::RAFT).unwrap();
        write(&db, &[region(1, b"", b"")]);

        // A record under each neighbouring prefix: `'l'` sorts before `'m'`, `'s'` after.
        let mut batch = WriteBatch::new();
        batch.put(
            cf_id,
            &crate::raft_log::log_entry_key(1, 1),
            b"not a region",
        );
        batch.put(cf_id, &crate::raft_log::state_key(1), b"not a region");
        db.write(batch, &WriteOptions { sync: true }).unwrap();

        let loaded = load_regions(&db).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 1);
    }

    #[test]
    fn a_removed_record_is_gone() {
        let (_dir, db) = open();
        let cf_id = db.cf_id(cf::RAFT).unwrap();
        write(&db, &[region(1, b"", b"g"), region(2, b"g", b"")]);

        let mut batch = WriteBatch::new();
        stage_removal(&mut batch, cf_id, 1);
        db.write(batch, &WriteOptions { sync: true }).unwrap();

        let loaded = load_regions(&db).unwrap();
        assert_eq!(loaded.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    }

    /// The id is in the key and in the value, and they have to agree — the same consistency
    /// check the log entries make, for the same reason.
    #[test]
    fn a_record_under_the_wrong_key_is_corruption() {
        let (_dir, db) = open();
        let cf_id = db.cf_id(cf::RAFT).unwrap();
        let mut batch = WriteBatch::new();
        batch.put(
            cf_id,
            &crate::raft_log::metadata_key(1),
            &encode_region(&region(2, b"", b"")),
        );
        db.write(batch, &WriteOptions { sync: true }).unwrap();

        let error = load_regions(&db).unwrap_err();
        assert!(error.to_string().contains("says it is region 2"), "{error}");
    }
}
