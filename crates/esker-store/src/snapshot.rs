//! Shipping a region to a peer that cannot be caught up from the log.
//!
//! A follower whose next index is below the leader's first index has a hole no `AppendEntries` can
//! fill, because the entries that would fill it have been compacted away ([`crate::peer`]'s
//! `compact`). The answer is the region's *state* rather than its history: the leader sends what
//! the keys are now, and the follower adopts it wholesale along with the metadata that says which
//! index it is as of.
//!
//! # The one thing that has to be true
//!
//! **A snapshot is never half-visible.** A region whose data is partly the snapshot's and partly
//! its own answers from a state no peer ever had, which is worse than answering nothing. Every
//! decision here is downstream of that sentence, and the staging in [`crate::server::Store`] is
//! where it is enforced (`docs/plans/phase-4.md` §13.1).
//!
//! # Key-value pairs, not SST files
//!
//! `docs/DESIGN.md` §6 describes shipping the checkpoint's SSTs and `ingest`ing them, and this
//! sends key-value pairs instead. Two reasons, both found rather than assumed:
//!
//! * **A checkpoint hard-links whole files, and a file straddles a region boundary.** The
//!   receiver would get keys belonging to the region's neighbour — data it has no claim to, and
//!   which `Db::ingest` would refuse outright if the neighbour happens to live on the same store.
//!   Range-precision is not something file granularity can offer.
//! * **`Db::ingest` refuses any overlap, tombstones included** (its own docs say so; rewriting
//!   sequence numbers is a v2 feature). So a receive that had to be retried after a partial one
//!   could never ingest again, and the recovery path would be the thing that wedged.
//!
//! What is given up is that the bytes are read and written rather than linked. They cross a
//! network either way, so the read is paid regardless; the cost is one write on the receiver.
//! `TODO(post-v1)`: with sequence-number rewriting and a range-clipped checkpoint, this becomes
//! the link-only transfer §6 describes, and only this module changes.
//!
//! # Format (*fixed*, version 1)
//!
//! The stream is a run of chunks. The first is a header; every one after it is a batch of pairs.
//!
//! ```text
//! header = 1:u8 ++ 1:u8(kind) ++ region ++ index ++ term ++ voters ++ learners
//! pairs  = 1:u8 ++ 2:u8(kind) ++ crc32c:u32 ++ count ++ (key ++ value)*
//! ```
//!
//! Everything unmarked is a varint; keys and values are length-prefixed. The keys are **user**
//! keys — the `'r'` namespace is the store's and is added on the way in, exactly as it is for a
//! request (`CLAUDE.md` invariant 7). Each batch carries its own CRC because a stream that
//! delivered a corrupt chunk and then completed would otherwise look like a clean transfer.
//!
//! A failed chunk **restarts the whole snapshot**. There is no resume and no per-chunk
//! retransmit: a snapshot is idempotent and cheap to redo relative to the bookkeeping resuming
//! needs, and a partial transfer is discarded by the staging anyway.

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, WriteOptions, cf, crc32c};
use esker_keys::prefix;
use esker_proto::{Decoder, Encoder, ProtoError, Region};
use esker_raft::{ConfState, SnapshotMeta};

use crate::error::engine_to_proto;

/// Version byte on every chunk of a snapshot stream.
const SNAPSHOT_FORMAT_VERSION: u8 = 1;

/// Chunk kinds. Zero is reserved, as everywhere in this project's formats.
const CHUNK_HEADER: u8 = 1;
const CHUNK_PAIRS: u8 = 2;

/// How many bytes of pairs one chunk carries before it is sent.
///
/// `docs/DESIGN.md` §6 says 1 MiB. It is a target rather than a limit: a chunk is closed once it
/// passes this, so a single large value makes one oversized chunk rather than being split across
/// two — the transport's frame limit is far above it (`docs/DESIGN.md` §9).
pub const CHUNK_TARGET_BYTES: usize = crate::SNAPSHOT_CHUNK_SIZE;

/// What the first chunk of a stream says the rest of it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// The region, as the sender holds it: range, peers and epoch.
    pub region: Region,
    /// Where this snapshot sits in the log, and the membership as of that index.
    pub meta: SnapshotMeta,
}

impl SnapshotHeader {
    /// The chunk's bytes.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = Encoder::new();
        out.put_u8(SNAPSHOT_FORMAT_VERSION);
        out.put_u8(CHUNK_HEADER);
        crate::meta::encode_region_into(&mut out, &self.region);
        out.put_varint(self.meta.index);
        out.put_varint(self.meta.term);
        put_ids(&mut out, &self.meta.conf.voters);
        put_ids(&mut out, &self.meta.conf.learners);
        Bytes::from(out.finish())
    }

    /// Reads a chunk written by [`SnapshotHeader::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut input = Decoder::new(bytes);
        expect_kind(&mut input, CHUNK_HEADER)?;
        let region = crate::meta::decode_region_from(&mut input)
            .map_err(|error| ProtoError::corrupt("snapshot header", error.to_string()))?;
        let index = varint(&mut input, "snapshot.index")?;
        let term = varint(&mut input, "snapshot.term")?;
        let voters = get_ids(&mut input, "snapshot.voters")?;
        let learners = get_ids(&mut input, "snapshot.learners")?;
        input
            .finish()
            .map_err(|error| ProtoError::corrupt("snapshot header", error.to_string()))?;

        let mut conf = ConfState { voters, learners };
        conf.normalize();
        if index == 0 {
            return Err(ProtoError::corrupt(
                "snapshot header",
                "a snapshot at index zero is the empty snapshot, which is never sent",
            ));
        }
        Ok(Self {
            region,
            meta: SnapshotMeta { index, term, conf },
        })
    }
}

/// One chunk of key-value pairs, checksummed.
#[must_use]
pub fn encode_pairs(pairs: &[(Bytes, Bytes)]) -> Bytes {
    let mut body = Encoder::new();
    body.put_varint(pairs.len() as u64);
    for (key, value) in pairs {
        body.put_bytes(key);
        body.put_bytes(value);
    }
    let body = body.finish();

    let mut out = Encoder::with_capacity(body.len() + 16);
    out.put_u8(SNAPSHOT_FORMAT_VERSION);
    out.put_u8(CHUNK_PAIRS);
    out.put_u32(crc32c::checksum(&body));
    // Length-prefixed rather than "the rest of the chunk", so a truncated frame is a decode error
    // rather than a body that happens to checksum against a shorter slice.
    out.put_bytes(&body);
    Bytes::from(out.finish())
}

/// Reads a chunk written by [`encode_pairs`], checking its CRC before anything is believed.
pub fn decode_pairs(bytes: &[u8]) -> Result<Vec<(Bytes, Bytes)>, ProtoError> {
    let mut input = Decoder::new(bytes);
    expect_kind(&mut input, CHUNK_PAIRS)?;
    let expected = input
        .get_u32("snapshot.crc")
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    let body = input
        .get_bytes("snapshot.body")
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    input
        .finish()
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    let found = crc32c::checksum(body);
    if found != expected {
        return Err(ProtoError::corrupt(
            "snapshot chunk",
            format!("checksum {found:#010x}, expected {expected:#010x}"),
        ));
    }

    let mut input = Decoder::new(body);
    let count = input
        .get_count("snapshot.pairs")
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    let mut pairs = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let key = owned(&mut input, "snapshot.key")?;
        let value = owned(&mut input, "snapshot.value")?;
        pairs.push((key, value));
    }
    input
        .finish()
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    Ok(pairs)
}

/// Reads one region's pairs at a pinned snapshot, handing each batch to `emit`.
///
/// The read is pinned by the engine snapshot the caller passes, which is what makes the stream a
/// picture of one instant rather than of a moving target. That snapshot and the `meta.index` must
/// be taken together, on the peer's own driver thread, or the metadata would name an index the
/// data does not include.
///
/// Synchronous, and expected to run on a blocking thread: it walks the region.
pub fn read_pairs<E>(
    db: &Db,
    region: &Region,
    read: esker_engine::Snapshot,
    target_bytes: usize,
    mut emit: E,
) -> Result<(), ProtoError>
where
    E: FnMut(Vec<(Bytes, Bytes)>) -> Result<(), ProtoError>,
{
    let low = prefix::raw_key(&region.start_key);
    let high = if region.end_key.is_empty() {
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(&region.end_key)
    };
    let options = ReadOptions {
        snapshot: Some(read),
        // A snapshot walks the whole region once and would evict everything a live workload has
        // in the cache for the sake of blocks nobody will read again.
        fill_cache: false,
        ..ReadOptions::default()
    };
    let mut iter = db
        .iter(cf::DEFAULT, &options)
        .map_err(|error| engine_to_proto(&error))?;

    let mut batch: Vec<(Bytes, Bytes)> = Vec::new();
    let mut bytes = 0usize;
    iter.seek(&low);
    while iter.valid() && iter.key() < &high[..] {
        let Some(user) = iter.key().strip_prefix(&[prefix::RAW]) else {
            return Err(ProtoError::internal(format!(
                "a snapshot read a key outside the 'r' namespace: {:?}",
                iter.key()
            )));
        };
        bytes += user.len() + iter.value().len();
        batch.push((
            Bytes::copy_from_slice(user),
            Bytes::copy_from_slice(iter.value()),
        ));
        if bytes >= target_bytes {
            emit(std::mem::take(&mut batch))?;
            bytes = 0;
        }
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    // The last batch goes even when it is empty: a region with no keys still has to produce a
    // stream the receiver can complete, or an empty region could never be shipped at all.
    emit(batch)?;
    Ok(())
}

/// Empties a region's key range so a snapshot can refill it, and proves that it did.
///
/// **This is what lets a peer that already holds data be caught up by a snapshot**, which 4c could
/// not do and which phase-4 acceptance showed is not an edge case: a learner adopts a snapshot, the
/// leader writes on and compacts past it, and from then on the log cannot catch it up and a second
/// snapshot was refused. The region then sat at one voter for ever
/// (`docs/plans/phase-4.md` §17).
///
/// Three steps, and each is needed for a reason the previous one creates:
///
/// 1. `delete_range` over the region's range. On its own this is not enough: a deletion is a
///    *stored entry*, so the range now holds tombstones, and the pairs written over them would
///    leave anything the snapshot does not contain hidden rather than gone;
/// 2. `flush`, which moves the tombstone out of the memtable and into L0 — the only level a range
///    tombstone can live in ([ADR 0017](../../../docs/adr/0017-range-tombstones.md));
/// 3. `compact_range` over the same range, which **discharges** it: the compaction applies the
///    tombstone and drops every file it covers rather than propagating it.
///
/// The discharge only runs when every live reader already sees the delete — `seqno <= floor`,
/// where the floor is the oldest pinned snapshot. **So this must be called with no engine snapshot
/// held**, which is why it takes `&Db` and not a reader: a snapshot pinned by this very code path
/// would hold the floor below its own tombstone and block the discharge it is waiting for.
///
/// # The emptiness check is load-bearing
///
/// It is not a debug assertion. If the discharge could not fully empty the range — a tombstone
/// still above the floor because something else pinned a snapshot, say — then refilling would
/// leave the region serving a mix of its own state and whatever survived, and the mix would look
/// exactly like correct data. Rather than weaken the check, this refuses and says so: catching the
/// peer up is then still impossible, which is where it was before, and the failure is loud.
pub fn clear_range(db: &Db, region: &Region) -> Result<(), ProtoError> {
    let (low, high) = range_bounds(region);
    let cf_id = db
        .cf_id(cf::DEFAULT)
        .ok_or_else(|| ProtoError::internal("the default column family is missing"))?;

    if first_key_in(db, &low, &high)?.is_none() {
        // Nothing to clear, which is the ordinary case: a replica placed on a store that never
        // held this range. No tombstone is written for it, so no discharge has to be waited for.
        return Ok(());
    }

    let mut batch = WriteBatch::new();
    batch.delete_range(cf_id, &low, &high);
    db.write(batch, &WriteOptions { sync: true })
        .map_err(|error| engine_to_proto(&error))?;
    db.flush(cf::DEFAULT)
        .map_err(|error| engine_to_proto(&error))?;
    db.compact_range(cf::DEFAULT, Some(&low), Some(&high))
        .map_err(|error| engine_to_proto(&error))?;

    if let Some(survivor) = first_key_in(db, &low, &high)? {
        return Err(ProtoError::Unsupported {
            detail: format!(
                "region {}: clearing [{:?}, {:?}) left key {:?} behind, so the range cannot be \
                 refilled from a snapshot without serving a mix of two states. The discharge \
                 could not run, which means a tombstone is still above the compaction floor — \
                 something is holding an engine snapshot open (docs/plans/phase-4.md §18)",
                region.id, region.start_key, region.end_key, survivor
            ),
        });
    }
    Ok(())
}

/// The `'r'`-namespaced bounds of a region's range, with the open end handled once.
fn range_bounds(region: &Region) -> (Vec<u8>, Vec<u8>) {
    let low = prefix::raw_key(&region.start_key);
    let high = if region.end_key.is_empty() {
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(&region.end_key)
    };
    (low, high)
}

/// The first key in `[low, high)`, if the store holds one.
fn first_key_in(db: &Db, low: &[u8], high: &[u8]) -> Result<Option<Vec<u8>>, ProtoError> {
    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;
    iter.seek(low);
    let found = (iter.valid() && iter.key() < high).then(|| iter.key().to_vec());
    iter.status().map_err(|error| engine_to_proto(&error))?;
    Ok(found)
}

/// Writes a batch of a snapshot's pairs into the data column family.
///
/// Not synced. Nothing reads these until the region is adopted, and the adoption *is* synced — so
/// a crash before it leaves keys that no region covers and that the next attempt overwrites.
pub fn stage_pairs(db: &Db, pairs: &[(Bytes, Bytes)]) -> Result<(), ProtoError> {
    if pairs.is_empty() {
        return Ok(());
    }
    let cf_id = db.cf_id(cf::DEFAULT).ok_or_else(|| {
        ProtoError::internal("the store opened without its `default` column family")
    })?;
    let mut batch = WriteBatch::new();
    for (key, value) in pairs {
        batch.put(cf_id, &prefix::raw_key(key), value);
    }
    db.write(batch, &WriteOptions { sync: false })
        .map(|_| ())
        .map_err(|error| engine_to_proto(&error))
}

/// Removes every key a partial receive left in `region`'s range.
///
/// The recovery half of the announcement record. A receive that stopped part-way left keys no
/// region covers; they are harmless where they are, but they would make the **retry** refuse to
/// start ([`clear_range`]), so the restart clears them and the range is clean again.
///
/// Point deletes, because the engine has no range tombstones in v1 (ADR 0006). The cost is
/// proportional to what was received before the crash, which is the honest price of not being able
/// to drop a range in one write.
pub fn discard_range(db: &Db, region: &Region) -> Result<u64, ProtoError> {
    let low = prefix::raw_key(&region.start_key);
    let high = if region.end_key.is_empty() {
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(&region.end_key)
    };
    let cf_id = db.cf_id(cf::DEFAULT).ok_or_else(|| {
        ProtoError::internal("the store opened without its `default` column family")
    })?;
    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;
    let mut batch = WriteBatch::new();
    let mut removed = 0u64;
    iter.seek(&low);
    while iter.valid() && iter.key() < &high[..] {
        batch.delete(cf_id, iter.key());
        removed += 1;
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    if removed > 0 {
        // Synced: the announcement record is removed in a later write, and a crash between the
        // two must not leave the keys behind with nothing pointing at them.
        db.write(batch, &WriteOptions { sync: true })
            .map_err(|error| engine_to_proto(&error))?;
    }
    Ok(removed)
}

fn expect_kind(input: &mut Decoder<'_>, kind: u8) -> Result<(), ProtoError> {
    let version = input
        .get_u8("snapshot.version")
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(ProtoError::corrupt(
            "snapshot chunk",
            format!("format version {version}, expected {SNAPSHOT_FORMAT_VERSION}"),
        ));
    }
    let found = input
        .get_u8("snapshot.kind")
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))?;
    if found != kind {
        return Err(ProtoError::corrupt(
            "snapshot chunk",
            format!("chunk kind {found}, expected {kind}"),
        ));
    }
    Ok(())
}

fn varint(input: &mut Decoder<'_>, field: &'static str) -> Result<u64, ProtoError> {
    input
        .get_varint(field)
        .map_err(|error| ProtoError::corrupt("snapshot header", error.to_string()))
}

fn owned(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, ProtoError> {
    input
        .get_bytes(field)
        .map(Bytes::copy_from_slice)
        .map_err(|error| ProtoError::corrupt("snapshot chunk", error.to_string()))
}

fn put_ids(out: &mut Encoder, ids: &[u64]) {
    out.put_varint(ids.len() as u64);
    for id in ids {
        out.put_varint(*id);
    }
}

fn get_ids(input: &mut Decoder<'_>, field: &'static str) -> Result<Vec<u64>, ProtoError> {
    let count = input
        .get_count(field)
        .map_err(|error| ProtoError::corrupt("snapshot header", error.to_string()))?;
    let mut ids = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        ids.push(varint(input, field)?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use esker_keys::prefix;

    use super::{
        CHUNK_TARGET_BYTES, SnapshotHeader, clear_range, decode_pairs, encode_pairs, first_key_in,
        read_pairs, stage_pairs,
    };
    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, cf};
    use esker_proto::{Epoch, Peer, Region};
    use esker_raft::{ConfState, SnapshotMeta};
    use std::sync::Arc;

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

    fn region(start: &[u8], end: &[u8]) -> Region {
        Region {
            id: 3,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, 10), Peer::voter(2, 20)],
            epoch: Epoch::new(2, 5),
        }
    }

    fn header() -> SnapshotHeader {
        SnapshotHeader {
            region: region(b"d", b"m"),
            meta: SnapshotMeta {
                index: 42,
                term: 7,
                conf: ConfState {
                    voters: vec![10, 20],
                    learners: vec![30],
                },
            },
        }
    }

    #[test]
    fn a_header_round_trips() {
        let header = header();
        assert_eq!(SnapshotHeader::decode(&header.encode()).unwrap(), header);
    }

    /// The header is what tells the receiver which region it is being given and which index it is
    /// as of. Every way it could be wrong is an error rather than a region built on a guess.
    #[test]
    fn a_header_that_cannot_be_true_is_refused() {
        let good = header().encode();

        let mut wrong_version = good.to_vec();
        wrong_version[0] = 9;
        assert!(SnapshotHeader::decode(&wrong_version).is_err(), "version");

        let mut wrong_kind = good.to_vec();
        wrong_kind[1] = 2;
        assert!(
            SnapshotHeader::decode(&wrong_kind).is_err(),
            "a pairs chunk"
        );

        let mut trailing = good.to_vec();
        trailing.push(0);
        assert!(SnapshotHeader::decode(&trailing).is_err(), "trailing bytes");

        let mut short = good.to_vec();
        short.pop();
        assert!(SnapshotHeader::decode(&short).is_err(), "truncated");

        // Index zero is the empty snapshot, which the core distinguishes and which is never sent.
        let empty = SnapshotHeader {
            meta: SnapshotMeta {
                index: 0,
                ..header().meta
            },
            ..header()
        };
        assert!(SnapshotHeader::decode(&empty.encode()).is_err());
    }

    /// A chunk carries its own checksum because a stream that delivered a corrupt chunk and then
    /// completed would otherwise look like a clean transfer. Corruption is an error value, never
    /// a silently accepted key (`CLAUDE.md` invariant 2).
    #[test]
    fn a_chunk_of_pairs_round_trips_and_a_flipped_byte_is_caught() {
        let pairs = vec![
            (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
            (Bytes::from_static(b"b"), Bytes::from_static(b"")),
        ];
        let encoded = encode_pairs(&pairs);
        assert_eq!(decode_pairs(&encoded).unwrap(), pairs);
        assert_eq!(decode_pairs(&encode_pairs(&[])).unwrap(), Vec::new());

        // Every byte of the body, flipped, is caught. The header bytes ahead of the CRC are
        // checked by their own version and kind.
        for at in 6..encoded.len() {
            let mut damaged = encoded.to_vec();
            damaged[at] ^= 0xff;
            assert!(
                decode_pairs(&damaged).is_err(),
                "a flip at byte {at} was accepted"
            );
        }
    }

    /// The stream is exactly the region's range, in key order, and the keys on the wire are the
    /// user's — the `'r'` namespace is the store's business at both ends.
    #[test]
    fn a_read_covers_the_regions_range_and_nothing_else() {
        let (_dir, db) = open();
        let all: Vec<(Bytes, Bytes)> = ["a", "d", "e", "f", "m", "z"]
            .iter()
            .map(|key| (Bytes::from(key.to_string()), Bytes::from(format!("v{key}"))))
            .collect();
        stage_pairs(&db, &all).unwrap();

        let mut seen: Vec<(Bytes, Bytes)> = Vec::new();
        read_pairs(
            &db,
            &region(b"d", b"m"),
            db.snapshot(),
            CHUNK_TARGET_BYTES,
            |batch| {
                seen.extend(batch);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            seen.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
            vec![
                Bytes::from_static(b"d"),
                Bytes::from_static(b"e"),
                Bytes::from_static(b"f")
            ],
            "the read ran outside the region"
        );
        assert_eq!(seen[0].1, Bytes::from_static(b"vd"));

        // An unbounded region takes everything from its start upwards, and an empty one still
        // produces a stream a receiver can complete.
        let mut count = 0;
        read_pairs(
            &db,
            &region(b"m", b""),
            db.snapshot(),
            CHUNK_TARGET_BYTES,
            |batch| {
                count += batch.len();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 2, "`m` and `z`");

        let mut chunks = 0;
        read_pairs(
            &db,
            &region(b"n", b"o"),
            db.snapshot(),
            CHUNK_TARGET_BYTES,
            |_| {
                chunks += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(chunks, 1, "an empty region still produces one chunk");
    }

    /// A target size closes chunks; it does not split a pair across two.
    #[test]
    fn the_target_size_cuts_the_stream_into_chunks() {
        let (_dir, db) = open();
        let pairs: Vec<(Bytes, Bytes)> = (0..50)
            .map(|n| {
                (
                    Bytes::from(format!("k{n:04}")),
                    Bytes::from(vec![b'v'; 100]),
                )
            })
            .collect();
        stage_pairs(&db, &pairs).unwrap();

        let mut sizes = Vec::new();
        read_pairs(&db, &region(b"", b""), db.snapshot(), 512, |batch| {
            sizes.push(batch.len());
            Ok(())
        })
        .unwrap();
        assert!(
            sizes.len() > 5,
            "the target did not cut the stream: {sizes:?}"
        );
        assert_eq!(
            sizes.iter().sum::<usize>(),
            50,
            "pairs were lost or doubled"
        );
    }

    /// A range that already holds keys is **emptied** so a snapshot can refill it, and the
    /// neighbours on either side are left exactly as they were.
    ///
    /// The range half is what phase-4 acceptance needed and 4c refused. The neighbour half is what
    /// makes it safe to do at all: a discharge that took a file spanning a boundary would delete
    /// another region's data, and a store commonly holds the regions on both sides.
    #[test]
    fn a_range_that_already_holds_keys_is_cleared_for_a_snapshot() {
        let (_dir, db) = open();
        // A clean range needs no clearing and says so by succeeding.
        clear_range(&db, &region(b"d", b"m")).unwrap();

        stage_pairs(
            &db,
            &[
                (Bytes::from_static(b"a"), Bytes::from_static(b"before")),
                (Bytes::from_static(b"e"), Bytes::from_static(b"v")),
                (Bytes::from_static(b"f"), Bytes::from_static(b"v")),
                (Bytes::from_static(b"z"), Bytes::from_static(b"after")),
            ],
        )
        .unwrap();

        clear_range(&db, &region(b"d", b"m")).unwrap();

        let low = prefix::raw_key(b"d");
        let high = prefix::raw_key(b"m");
        assert_eq!(
            first_key_in(&db, &low, &high).unwrap(),
            None,
            "the range was not emptied, so refilling it would serve a mix of two states"
        );
        // And only that range: the keys on either side of it are untouched.
        for (key, what) in [(&b"a"[..], "below"), (&b"z"[..], "above")] {
            let full = prefix::raw_key(key);
            assert!(
                first_key_in(&db, &full, &[prefix::RAW + 1])
                    .unwrap()
                    .is_some(),
                "clearing a region's range took a key {what} it"
            );
        }
    }

    /// Clearing is idempotent, because a retried transfer runs it again on a range its own
    /// previous attempt already emptied.
    #[test]
    fn clearing_a_range_twice_is_the_same_as_clearing_it_once() {
        let (_dir, db) = open();
        stage_pairs(&db, &[(Bytes::from_static(b"e"), Bytes::from_static(b"v"))]).unwrap();
        clear_range(&db, &region(b"d", b"m")).unwrap();
        clear_range(&db, &region(b"d", b"m")).unwrap();
        let low = prefix::raw_key(b"d");
        let high = prefix::raw_key(b"m");
        assert_eq!(first_key_in(&db, &low, &high).unwrap(), None);
    }
}
