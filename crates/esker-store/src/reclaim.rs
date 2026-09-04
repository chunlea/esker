//! Reclaiming the key range of a database that has been dropped
//! ([ADR 0069](../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md)).
//!
//! `DROP DATABASE` deletes the catalog record — a small transaction, and the thing that makes the
//! database *gone*. It does **not** delete the rows: nothing can route into a dropped tenant's key
//! space once the catalog says it is not there, so the rows are unreachable, and unreachable is
//! what makes it safe to clear them by range instead of one MVCC version at a time.
//!
//! # The three properties, and where each comes from
//!
//! **Bounded work per pass.** One chunk, and a chunk is the intersection of the range with **one
//! region this store hosts**. That is not an arbitrary number: a region is already the unit this
//! system keeps bounded, by splitting at [`crate::REGION_SPLIT_SIZE`]. Chunking by anything else
//! would need arithmetic on keys the store cannot do — the engine's keys are memcomparable
//! encodings with a timestamp suffix, and "the key half way between these two" is not a question
//! they answer.
//!
//! **Idempotent.** [`crate::snapshot::clear_user_range`] returns early on a range that is already
//! empty, so a chunk cleared twice is a chunk cleared once. Nothing has to remember what it did.
//!
//! **Crash-safe at every instant.** The record is written *before* any byte is deleted and the
//! cursor is advanced *after*, both synced. A crash between them re-clears a chunk that is already
//! empty, which is the early return above. The other order — advancing the cursor and then
//! clearing — would skip a chunk on a crash and leave keys behind for ever with nothing coming
//! back for them, which is the leak ADR 0034 names.
//!
//! # The safepoint gate is what makes it correct, not merely fast
//!
//! A transaction whose snapshot was taken before the drop committed may still legally read those
//! rows. So a reclaim carries the drop's commit timestamp and does nothing until this store's
//! garbage-collection safepoint has reached it ([`crate::gc`]) — the floor PD publishes and the
//! MVCC collector already respects. Below it no such reader can exist, and the clear is invisible
//! rather than merely unlikely to be noticed.
//!
//! # What is not here
//!
//! Nothing proposes a reclaim yet. The trigger crosses the wire and `esker-proto` is sequenced
//! separately; ADR 0069 names the one message needed and stops. Everything below is reachable
//! in-process, which is what its tests drive.

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, WriteOptions, cf};
use esker_proto::{Decoder, Encoder, ProtoError, Region};

use crate::error::{Result, StoreError, engine_to_proto};
use crate::raft_cf;

/// Version byte on a reclaim record. A change to any field's meaning bumps it.
const RECLAIM_FORMAT_VERSION: u8 = 1;

/// A range this store has been told to reclaim, and how far it has got.
///
/// `cursor` is a user key inside `[start, end)`, or equal to `end` when there is nothing left. It
/// starts at `start` and only ever moves forward, so the record is its own progress report: a
/// `sst-dump` of the `raft` family says which ranges are half-reclaimed and where they stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaim {
    /// Inclusive lower bound of the whole range, and the record's identity.
    pub start: Bytes,
    /// Exclusive upper bound. **Empty means the end of the key space**, the same convention a
    /// region's `end_key` uses.
    pub end: Bytes,
    /// The commit timestamp of the drop this reclaim belongs to. Nothing is deleted until the
    /// store's safepoint has reached it.
    pub below_ts: u64,
    /// Where the walk has got to. `start` for a reclaim that has not begun.
    pub cursor: Bytes,
}

impl Reclaim {
    /// A reclaim of `[start, end)` that has not begun.
    #[must_use]
    pub fn new(start: Bytes, end: Bytes, below_ts: u64) -> Self {
        Self {
            cursor: start.clone(),
            start,
            end,
            below_ts,
        }
    }

    /// Whether the walk has reached the far end of the range.
    ///
    /// **A record on disk is never finished** — [`advance`] removes it rather than persist a
    /// cursor equal to `end`, and that is not tidiness. An empty `end` means the end of the key
    /// space, so a stored cursor of `""` could not be told from a walk that had not started: the
    /// next pass would take the whole key space as its chunk and clear every other tenant on this
    /// store. Reaching the end is signalled by the record's absence, which cannot be misread.
    ///
    /// Found by the test that drives an open-ended range, which span for eleven minutes clearing
    /// an already-empty range before this was written the other way round.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.cursor == self.end
    }
}

/// Whether `key` is strictly below `bound`, where an **empty bound is the end of the key space**.
///
/// The convention costs a helper because the alternative is worse: an empty `end_key` compared as
/// bytes is below everything, so a region that owns the tail of the key space would compare as
/// owning none of it. Every bound in this module goes through here.
fn below(key: &[u8], bound: &[u8]) -> bool {
    bound.is_empty() || key < bound
}

/// The larger of two lower bounds.
fn later(a: &[u8], b: &[u8]) -> Bytes {
    Bytes::copy_from_slice(if a >= b { a } else { b })
}

/// The smaller of two upper bounds, empty meaning the end of the key space.
fn earlier(a: &[u8], b: &[u8]) -> Bytes {
    if a.is_empty() {
        return Bytes::copy_from_slice(b);
    }
    if b.is_empty() {
        return Bytes::copy_from_slice(a);
    }
    Bytes::copy_from_slice(if a <= b { a } else { b })
}

/// `'D' ++ start_key` on the `raft` column family.
///
/// Keyed by the range's start rather than by an id, so two reclaims of one range are one record
/// and a scan returns them in key order. The key is variable length, which is why every reader
/// below checks the prefix byte rather than a fixed length.
#[must_use]
pub fn reclaim_key(start: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + start.len());
    key.push(raft_cf::RECLAIMING);
    key.extend_from_slice(start);
    key
}

/// The record's bytes.
#[must_use]
pub fn encode(reclaim: &Reclaim) -> Vec<u8> {
    let mut out = Encoder::new();
    out.put_u8(RECLAIM_FORMAT_VERSION);
    out.put_bytes(&reclaim.start);
    out.put_bytes(&reclaim.end);
    out.put_varint(reclaim.below_ts);
    out.put_bytes(&reclaim.cursor);
    out.finish()
}

/// One length-prefixed field, reported the way every other on-disk read in this crate reports.
fn field(input: &mut Decoder<'_>, name: &'static str) -> Result<Bytes> {
    input
        .get_bytes(name)
        .map(Bytes::copy_from_slice)
        .map_err(|error| StoreError::Bootstrap(format!("a reclaim record: {error}")))
}

/// Reads a record back.
///
/// # Errors
///
/// A version this build does not know, or bytes that do not decode — reported, never panicked on,
/// because this is on-disk data (`CLAUDE.md` invariant 9).
pub fn decode(bytes: &[u8]) -> Result<Reclaim> {
    let mut input = Decoder::new(bytes);
    let version = input
        .get_u8("reclaim.version")
        .map_err(|error| StoreError::Bootstrap(format!("a reclaim record: {error}")))?;
    if version != RECLAIM_FORMAT_VERSION {
        return Err(StoreError::Bootstrap(format!(
            "a reclaim record has format version {version}, expected {RECLAIM_FORMAT_VERSION}"
        )));
    }
    let start = field(&mut input, "reclaim.start")?;
    let end = field(&mut input, "reclaim.end")?;
    let below_ts = input
        .get_varint("reclaim.below_ts")
        .map_err(|error| StoreError::Bootstrap(format!("a reclaim record: {error}")))?;
    let cursor = field(&mut input, "reclaim.cursor")?;
    Ok(Reclaim {
        start,
        end,
        below_ts,
        cursor,
    })
}

/// Stages the record that says this range is being reclaimed and has not finished.
pub fn stage(batch: &mut WriteBatch, cf: u32, reclaim: &Reclaim) {
    batch.put(cf, &reclaim_key(&reclaim.start), &encode(reclaim));
}

/// Stages the removal of a finished reclaim.
///
/// Only once the range is empty as far as this store is concerned. Removing it on the strength of
/// having tried is how a reclamation that failed becomes a leak nothing comes back for.
pub fn stage_done(batch: &mut WriteBatch, cf: u32, start: &[u8]) {
    batch.delete(cf, &reclaim_key(start));
}

/// Every reclaim that had not finished when this store last stopped, in key order.
///
/// # Errors
///
/// A record that does not decode, which is a corrupt `raft` column family and not a state to
/// continue from.
pub fn load(db: &Db) -> Result<Vec<Reclaim>> {
    let mut iter = db.iter(cf::RAFT, &ReadOptions::default())?;
    let mut pending = Vec::new();
    iter.seek(&[raft_cf::RECLAIMING]);
    while iter.valid() {
        if iter.key().first() != Some(&raft_cf::RECLAIMING) {
            break;
        }
        pending.push(decode(iter.value())?);
        iter.next();
    }
    iter.status()?;
    Ok(pending)
}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The safepoint has not reached the drop's commit timestamp, so nothing was deleted. Carries
    /// what the store is working to, because "blocked" without the two numbers is not a diagnosis.
    Blocked {
        /// The timestamp the reclaim is waiting for.
        below_ts: u64,
        /// The safepoint the store is at now.
        safepoint: u64,
    },
    /// A chunk was cleared and the cursor now sits here.
    Advanced {
        /// The new cursor.
        cursor: Bytes,
    },
    /// Nothing of the range is on this store any more; the record has been removed.
    Finished,
}

/// The next chunk to clear: the intersection of what is left with one hosted region.
///
/// `None` when no region this store hosts overlaps what is left, which is what "finished here"
/// means — the rest of the range, if any, belongs to other stores and they reclaim their own.
///
/// The **first** overlapping region in key order, so the walk is monotone and a region that
/// arrives mid-walk cannot send the cursor backwards.
fn next_chunk(reclaim: &Reclaim, hosted: &[Region]) -> Option<(Bytes, Bytes)> {
    if reclaim.is_finished() {
        return None;
    }
    let mut best: Option<(Bytes, Bytes)> = None;
    for region in hosted {
        let low = later(&reclaim.cursor, &region.start_key);
        let high = earlier(&reclaim.end, &region.end_key);
        if !below(&low, &high) {
            continue;
        }
        if best.as_ref().is_none_or(|(chosen, _)| low < *chosen) {
            best = Some((low, high));
        }
    }
    best
}

/// Clears one chunk of `reclaim`, advancing and persisting its cursor.
///
/// The order is the one the module header argues for: the record is already on disk, the chunk is
/// cleared, and only then does the cursor move. A crash anywhere re-clears an empty chunk.
///
/// # Errors
///
/// The engine's, and a clear that left a key behind — which means a tombstone is still above the
/// compaction floor because something is holding an engine snapshot open.
pub fn advance(
    db: &Db,
    reclaim: &Reclaim,
    hosted: &[Region],
    safepoint: u64,
) -> std::result::Result<Progress, ProtoError> {
    if safepoint < reclaim.below_ts {
        // Not an error and not a failure: a reader whose snapshot predates the drop may still be
        // entitled to these rows. The pass simply does nothing and the next one asks again.
        return Ok(Progress::Blocked {
            below_ts: reclaim.below_ts,
            safepoint,
        });
    }
    let cf_id = db
        .cf_id(cf::RAFT)
        .ok_or_else(|| ProtoError::internal("the store opened without its `raft` column family"))?;

    let Some((low, high)) = next_chunk(reclaim, hosted) else {
        let mut batch = WriteBatch::new();
        stage_done(&mut batch, cf_id, &reclaim.start);
        db.write(batch, &WriteOptions::synced())
            .map_err(|error| engine_to_proto(&error))?;
        return Ok(Progress::Finished);
    };

    crate::snapshot::clear_user_range(db, &low, &high)?;

    // **The far end is signalled by removing the record, never by storing a cursor equal to it.**
    // `end` may be empty — the end of the key space — and a stored cursor of `""` cannot be told
    // from a walk that has not started. See `Reclaim::is_finished`.
    let mut batch = WriteBatch::new();
    if high == reclaim.end {
        stage_done(&mut batch, cf_id, &reclaim.start);
        db.write(batch, &WriteOptions::synced())
            .map_err(|error| engine_to_proto(&error))?;
        return Ok(Progress::Finished);
    }
    let mut moved = reclaim.clone();
    moved.cursor = high.clone();
    stage(&mut batch, cf_id, &moved);
    db.write(batch, &WriteOptions::synced())
        .map_err(|error| engine_to_proto(&error))?;
    Ok(Progress::Advanced { cursor: high })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, WriteBatch, WriteOptions, cf};
    use esker_proto::{Epoch, Peer, Region};

    use super::{Progress, Reclaim, advance, load, stage};

    /// A store with the built-in families, and its directory kept alive beside it.
    fn open(dir: &tempfile::TempDir) -> Arc<Db> {
        Arc::new(
            Db::open_with(
                dir.path(),
                Options {
                    create_if_missing: true,
                    ..Options::default()
                },
                Arc::new(LocalFileSystem::new()),
                &cf::BUILTIN,
            )
            .unwrap(),
        )
    }

    fn region(id: u64, start: &[u8], end: &[u8]) -> Region {
        Region {
            id,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, 10)],
            epoch: Epoch::new(1, 1),
        }
    }

    /// Writes one transactional key and one raw key under `user`, so a clear has to reach both
    /// physical namespaces and more than one column family to make the range empty.
    fn seed(db: &Db, user: &[u8]) {
        let mut batch = WriteBatch::new();
        batch.put(
            db.cf_id(cf::DEFAULT).unwrap(),
            &esker_keys::prefix::raw_key(user),
            b"raw",
        );
        batch.put(
            db.cf_id(cf::DEFAULT).unwrap(),
            &esker_keys::prefix::txn_key(user, 7),
            b"txn",
        );
        batch.put(
            db.cf_id(cf::WRITE).unwrap(),
            &esker_keys::prefix::txn_key(user, 7),
            b"commit",
        );
        db.write(batch, &WriteOptions::synced()).unwrap();
    }

    fn holds(db: &Db, user: &[u8]) -> bool {
        crate::snapshot::first_key_in_user_range(db, user, &[user[0] + 1])
            .unwrap()
            .is_some()
    }

    fn put_record(db: &Db, reclaim: &Reclaim) {
        let mut batch = WriteBatch::new();
        stage(&mut batch, db.cf_id(cf::RAFT).unwrap(), reclaim);
        db.write(batch, &WriteOptions::synced()).unwrap();
    }

    /// **The safepoint gate does the deciding, and it does nothing until it opens.**
    ///
    /// A transaction whose snapshot predates the drop may still legally read these rows, so a
    /// reclaim below the safepoint is not slow, it is forbidden. `Blocked` carries both numbers
    /// because "blocked" without them sends the next reader to the wrong question.
    #[test]
    fn nothing_is_deleted_until_the_safepoint_reaches_the_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        seed(&db, b"e");
        let pending = Reclaim::new(Bytes::from_static(b"d"), Bytes::from_static(b"m"), 500);
        put_record(&db, &pending);
        let hosted = [region(1, b"", b"")];

        assert_eq!(
            advance(&db, &pending, &hosted, 499).unwrap(),
            Progress::Blocked {
                below_ts: 500,
                safepoint: 499
            }
        );
        assert!(holds(&db, b"e"), "a blocked reclaim deleted something");

        // The instant the safepoint reaches it, the same call clears. One hosted region carries
        // the whole range, so the first chunk reaches `end` and the pass reports `Finished`.
        assert_eq!(
            advance(&db, &pending, &hosted, 500).unwrap(),
            Progress::Finished
        );
        assert!(!holds(&db, b"e"));
    }

    /// **A chunk is one hosted region**, so a range spanning three of them takes three passes and
    /// each pass is bounded by what a region is allowed to grow to.
    #[test]
    fn a_range_spanning_three_regions_is_walked_one_region_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        for key in [&b"c"[..], b"e", b"g", b"i", b"q"] {
            seed(&db, key);
        }
        let hosted = [
            region(1, b"", b"f"),
            region(2, b"f", b"h"),
            region(3, b"h", b""),
        ];
        let mut pending = Reclaim::new(Bytes::from_static(b"d"), Bytes::from_static(b"m"), 0);
        put_record(&db, &pending);

        let mut passes = 0;
        loop {
            match advance(&db, &pending, &hosted, 10).unwrap() {
                Progress::Advanced { cursor } => {
                    assert!(cursor > pending.cursor, "the cursor did not move forward");
                    pending.cursor = cursor;
                    passes += 1;
                }
                Progress::Finished => break,
                Progress::Blocked { .. } => unreachable!("the safepoint is above the drop"),
            }
            assert!(passes <= 4, "the walk did not terminate");
        }

        assert_eq!(
            passes, 2,
            "one Advanced per hosted region except the last, which reports Finished"
        );
        for key in [&b"e"[..], b"g", b"i"] {
            assert!(!holds(&db, key), "{key:?} was inside the range");
        }
        for key in [&b"c"[..], b"q"] {
            assert!(holds(&db, key), "{key:?} was outside the range");
        }
        assert!(
            load(&db).unwrap().is_empty(),
            "the finished record survived"
        );
    }

    /// **`kill -9` between two chunks resumes from the record and finishes the job.**
    ///
    /// The crash is modelled by dropping the `Db` and reopening the same directory, which is what
    /// a restart actually does: everything not synced is gone. The record was synced before the
    /// first byte was deleted, so the reopened store knows both what range it was clearing and
    /// where it had got to — and re-clearing the chunk it was part-way through is a no-op.
    #[test]
    fn a_reclaim_interrupted_between_chunks_resumes_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let hosted = [
            region(1, b"", b"f"),
            region(2, b"f", b"h"),
            region(3, b"h", b""),
        ];
        let cursor_after_one = {
            let db = open(&dir);
            for key in [&b"c"[..], b"e", b"g", b"i", b"q"] {
                seed(&db, key);
            }
            let pending = Reclaim::new(Bytes::from_static(b"d"), Bytes::from_static(b"m"), 0);
            put_record(&db, &pending);
            let Progress::Advanced { cursor } = advance(&db, &pending, &hosted, 10).unwrap() else {
                panic!("the first pass did not advance");
            };
            assert!(!holds(&db, b"e"), "the first chunk was not cleared");
            assert!(
                holds(&db, b"g"),
                "the first pass cleared more than its chunk"
            );
            cursor
            // and the process dies here
        };

        let db = open(&dir);
        let resumed = load(&db).unwrap();
        assert_eq!(resumed.len(), 1, "the restart lost the reclaim");
        let mut pending = resumed.into_iter().next().unwrap();
        assert_eq!(
            pending.cursor, cursor_after_one,
            "the restart resumed from the wrong place"
        );

        while let Progress::Advanced { cursor } = advance(&db, &pending, &hosted, 10).unwrap() {
            pending.cursor = cursor;
        }
        for key in [&b"e"[..], b"g", b"i"] {
            assert!(!holds(&db, key));
        }
        for key in [&b"c"[..], b"q"] {
            assert!(
                holds(&db, key),
                "the resumed walk took a key outside the range"
            );
        }
        assert!(load(&db).unwrap().is_empty());
    }

    /// A record survives a restart byte for byte, because a walk that resumed against a range it
    /// had reconstructed differently would clear the wrong keys.
    #[test]
    fn a_reclaim_record_round_trips_through_the_raft_family() {
        let dir = tempfile::tempdir().unwrap();
        let written = Reclaim {
            start: Bytes::from_static(b"d"),
            end: Bytes::new(),
            below_ts: 1 << 40,
            cursor: Bytes::from_static(b"g"),
        };
        {
            let db = open(&dir);
            put_record(&db, &written);
        }
        let db = open(&dir);
        assert_eq!(load(&db).unwrap(), vec![written]);
    }

    /// An **empty end key is the end of the key space**, not a bound below everything. A reclaim
    /// that read it as bytes would finish immediately and delete nothing, which is the quiet
    /// failure this convention invites.
    #[test]
    fn an_open_ended_reclaim_reaches_the_end_of_the_key_space() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        for key in [&b"c"[..], b"z"] {
            seed(&db, key);
        }
        let pending = Reclaim::new(Bytes::from_static(b"d"), Bytes::new(), 0);
        put_record(&db, &pending);
        assert!(!pending.is_finished(), "an empty end read as a byte bound");

        let hosted = [region(1, b"", b"")];
        // **One pass, and it must report `Finished`.** The only hosted region carries the range to
        // its open end, so the first chunk already reaches `end`. Written as an equality rather
        // than a loop on purpose: the loop is what span for eleven minutes when the cursor was
        // allowed to hold the empty end key, re-clearing an empty range for ever.
        assert_eq!(
            advance(&db, &pending, &hosted, 10).unwrap(),
            Progress::Finished
        );
        assert!(
            !holds(&db, b"z"),
            "the tail of the key space was not reached"
        );
        assert!(holds(&db, b"c"), "a key below the range was taken");
        assert!(
            load(&db).unwrap().is_empty(),
            "the finished record survived"
        );
    }
}
