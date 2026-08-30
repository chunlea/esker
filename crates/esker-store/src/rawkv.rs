//! The eight `RawKv` methods, over the engine.
//!
//! Everything here is **synchronous**. `CLAUDE.md` keeps `esker-engine` free of async, so the
//! handlers are ordinary blocking code and [`crate::server`] runs them on a blocking thread.
//! That is also what makes them testable without a runtime.
//!
//! # The `'r'` namespace
//!
//! A client sends raw user bytes and gets raw user bytes back. Everything stored gains the
//! `'r'` prefix of `docs/DESIGN.md` §3 here, and only here — on point keys, on batch keys, on
//! scan bounds and on delete-range bounds alike. A client that could put a prefix on the wire
//! could address a namespace that is not its own (`CLAUDE.md` invariant 7), and a key that
//! happens to *be* `b"r"` is stored at `b"rr"` like any other.
//!
//! # Bounds
//!
//! Two of them, and both are correctness rather than politeness. A scan stops at a **byte
//! budget** as well as a key count, because a response that will not fit in one frame is a
//! response the transport must refuse — and the client would get nothing at all rather than
//! the first page of something. A `DeleteRange` stops at a **key count**, for the reason in
//! ADR 0006.

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, WriteOptions, cf};
use esker_keys::prefix;
use esker_proto::{ProtoError, RawKvReq, RawKvResp};

use crate::error::engine_to_proto;
use crate::region::RegionMeta;

/// Most key-value pairs one `Scan` returns, whatever the caller asked for.
pub const MAX_SCAN_LIMIT: u32 = 8192;

/// Most bytes of keys and values one `Scan` returns.
///
/// Well under `esker_proto::MAX_FRAME_SIZE`, because the response also carries its framing and
/// its per-pair length prefixes. A scan that stops early returns fewer pairs; a scan that did
/// not stop would build a frame the transport is obliged to refuse.
pub const MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;

/// Most keys one `DeleteRange` will remove. See ADR 0006.
pub const MAX_DELETE_RANGE_KEYS: usize = 10_000;

/// Limits a store applies to the requests it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most pairs one scan returns.
    pub max_scan_limit: u32,
    /// Most bytes one scan returns.
    pub max_scan_bytes: usize,
    /// Most keys one `DeleteRange` removes before it refuses.
    pub max_delete_range_keys: usize,
}

impl Limits {
    /// The defaults above.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_scan_limit: MAX_SCAN_LIMIT,
            max_scan_bytes: MAX_SCAN_BYTES,
            max_delete_range_keys: MAX_DELETE_RANGE_KEYS,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

/// Serves one `RawKv` request against `db`, for the range `region` owns.
///
/// The header has already been checked by the caller; the key-range checks happen here,
/// because only here is it known which keys a request touches.
pub fn serve(
    db: &Db,
    region: &RegionMeta,
    limits: &Limits,
    request: RawKvReq,
) -> Result<RawKvResp, ProtoError> {
    match request {
        RawKvReq::Get { key } => {
            region.check_key(&key)?;
            Ok(RawKvResp::Get {
                value: get(db, &key)?,
            })
        }
        RawKvReq::BatchGet { keys } => {
            region.check_keys(keys.iter().map(|key| &key[..]))?;
            Ok(RawKvResp::BatchGet {
                values: batch_get(db, &keys)?,
            })
        }
        RawKvReq::Put { key, value, sync } => {
            region.check_key(&key)?;
            let mut batch = WriteBatch::new();
            batch.put(data_cf(db)?, &prefix::raw_key(&key), &value);
            write(db, batch, sync)?;
            Ok(RawKvResp::Put)
        }
        RawKvReq::BatchPut { pairs, sync } => {
            region.check_keys(pairs.iter().map(|(key, _)| &key[..]))?;
            let cf_id = data_cf(db)?;
            let mut batch = WriteBatch::new();
            for (key, value) in &pairs {
                batch.put(cf_id, &prefix::raw_key(key), value);
            }
            write(db, batch, sync)?;
            Ok(RawKvResp::BatchPut)
        }
        RawKvReq::Delete { key, sync } => {
            region.check_key(&key)?;
            let mut batch = WriteBatch::new();
            batch.delete(data_cf(db)?, &prefix::raw_key(&key));
            write(db, batch, sync)?;
            Ok(RawKvResp::Delete)
        }
        RawKvReq::DeleteRange { start, end, sync } => {
            region.check_range(&start, &end)?;
            Ok(RawKvResp::DeleteRange {
                deleted: delete_range(db, region, limits, &start, &end, sync)?,
            })
        }
        RawKvReq::Scan {
            start,
            end,
            limit,
            reverse,
        } => {
            let (low, high) = scan_bounds(region, &start, &end, reverse)?;
            Ok(RawKvResp::Scan {
                pairs: scan(db, limits, &low, &high, limit, reverse)?,
            })
        }
        RawKvReq::CompareAndSwap { .. } => Err(ProtoError::internal(
            "CompareAndSwap is served by the store, which holds the lock it needs",
        )),
    }
}

/// Reads one key.
pub fn get(db: &Db, key: &[u8]) -> Result<Option<Bytes>, ProtoError> {
    db.get(cf::DEFAULT, &prefix::raw_key(key), &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))
}

/// Reads several keys **at one snapshot**.
///
/// One snapshot rather than several reads, so a batch answers a single question about a
/// single state of the database. Without it, a batch spanning a concurrent write could report
/// a pair of values that never existed together.
pub fn batch_get(db: &Db, keys: &[Bytes]) -> Result<Vec<Option<Bytes>>, ProtoError> {
    let options = ReadOptions {
        snapshot: Some(db.snapshot()),
        ..ReadOptions::default()
    };
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        values.push(
            db.get(cf::DEFAULT, &prefix::raw_key(key), &options)
                .map_err(|error| engine_to_proto(&error))?,
        );
    }
    Ok(values)
}

/// Writes a batch, waiting for durability when asked.
pub fn write(db: &Db, batch: WriteBatch, sync: bool) -> Result<(), ProtoError> {
    let options = if sync {
        WriteOptions::synced()
    } else {
        WriteOptions::unsynced()
    };
    db.write(batch, &options)
        .map(|_seqno| ())
        .map_err(|error| engine_to_proto(&error))
}

/// Replaces `key` with `value` — or deletes it — only if it currently holds `expected`.
///
/// The caller holds whatever lock makes this atomic; on its own this is a read followed by a
/// write. [`crate::Store`] takes the exclusive side of its write gate around it, which is what
/// stops a concurrent `Put` from landing in between. From phase 3 the Raft log is that
/// serialisation point and the gate goes away.
pub fn compare_and_swap(
    db: &Db,
    key: &[u8],
    expected: Option<&[u8]>,
    value: Option<&[u8]>,
    sync: bool,
) -> Result<RawKvResp, ProtoError> {
    let stored = get(db, key)?;
    let matched = stored.as_deref() == expected;
    if !matched {
        return Ok(RawKvResp::CompareAndSwap {
            swapped: false,
            previous: stored,
        });
    }

    let cf_id = data_cf(db)?;
    let stored_key = prefix::raw_key(key);
    let mut batch = WriteBatch::new();
    match value {
        Some(value) => batch.put(cf_id, &stored_key, value),
        None => batch.delete(cf_id, &stored_key),
    }
    write(db, batch, sync)?;
    Ok(RawKvResp::CompareAndSwap {
        swapped: true,
        previous: stored,
    })
}

/// Removes everything in `[start, end)`, up to the configured limit.
///
/// Implemented as a scan and a batch of point deletes, **not** as the engine's `DeleteRange`
/// (ADR 0006): the engine's v1 range tombstone answers for the key at `begin` and no other, so
/// using it would report a range deleted while leaving nearly all of it in place. The deletes
/// go in one `WriteBatch`, so the range disappears atomically.
fn delete_range(
    db: &Db,
    region: &RegionMeta,
    limits: &Limits,
    start: &[u8],
    end: &[u8],
    sync: bool,
) -> Result<u64, ProtoError> {
    let (low, high) = scan_bounds(region, start, end, false)?;
    let cf_id = data_cf(db)?;

    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;
    let mut batch = WriteBatch::new();
    let mut deleted = 0usize;

    iter.seek(&low);
    while iter.valid() && iter.key() < &high[..] {
        if deleted == limits.max_delete_range_keys {
            return Err(ProtoError::Unsupported {
                detail: format!(
                    "DeleteRange over more than {} keys is not supported in this version; \
                     delete in smaller ranges (ADR 0006)",
                    limits.max_delete_range_keys
                ),
            });
        }
        batch.delete(cf_id, iter.key());
        deleted += 1;
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;

    if deleted > 0 {
        write(db, batch, sync)?;
    }
    Ok(deleted as u64)
}

/// Reads a range in key order, or in reverse.
fn scan(
    db: &Db,
    limits: &Limits,
    low: &[u8],
    high: &[u8],
    limit: u32,
    reverse: bool,
) -> Result<Vec<(Bytes, Bytes)>, ProtoError> {
    // Zero means "the server's maximum"; anything larger is capped to it, so a scan is always
    // a bounded answer whatever the caller asks for.
    let limit = if limit == 0 {
        limits.max_scan_limit
    } else {
        limit.min(limits.max_scan_limit)
    } as usize;

    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;
    let mut pairs = Vec::new();
    let mut bytes = 0usize;

    if reverse {
        iter.seek_for_prev(high);
        // `seek_for_prev` lands on the largest key `<= high`, and `high` is exclusive.
        while iter.valid() && iter.key() >= high {
            iter.prev();
        }
    } else {
        iter.seek(low);
    }

    while iter.valid() && pairs.len() < limit {
        let key = iter.key();
        if reverse {
            if key < low {
                break;
            }
        } else if key >= high {
            break;
        }

        let value = iter.value();
        bytes += key.len() + value.len();
        // Checked after the first pair is counted, so a single pair larger than the budget is
        // still returned: a scan that can never make progress is worse than a large frame.
        if bytes > limits.max_scan_bytes && !pairs.is_empty() {
            break;
        }
        pairs.push((
            Bytes::copy_from_slice(user_key(key)?),
            Bytes::copy_from_slice(value),
        ));

        if reverse {
            iter.prev();
        } else {
            iter.next();
        }
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    Ok(pairs)
}

/// The stored bounds of a range request, in engine key space.
///
/// An empty `end` means "to the end of the region", which is the end of the region's own range
/// when it has one and the end of the `'r'` namespace when it does not. Getting this wrong
/// would let a scan run out of the namespace and into `'x'` — the transactional key space —
/// which is why the namespace end is computed rather than assumed to be unreachable.
fn scan_bounds(
    region: &RegionMeta,
    start: &[u8],
    end: &[u8],
    reverse: bool,
) -> Result<(Vec<u8>, Vec<u8>), ProtoError> {
    let (low, high) = if reverse { (end, start) } else { (start, end) };
    // For a reverse scan the caller names the *upper* bound first, so the range being checked
    // is still `[low, high)`.
    region.check_range(low, high)?;

    let stored_low = prefix::raw_key(low);
    let stored_high = if high.is_empty() {
        if region.region().end_key.is_empty() {
            namespace_end()
        } else {
            prefix::raw_key(&region.region().end_key)
        }
    } else {
        prefix::raw_key(high)
    };
    Ok((stored_low, stored_high))
}

/// The first key after the whole `'r'` namespace.
///
/// `'r'` is one byte, so its namespace ends at the next byte value. A scan bounded by this
/// stops at the end of `RawKV` rather than continuing into whatever namespace sorts next.
fn namespace_end() -> Vec<u8> {
    vec![prefix::RAW + 1]
}

/// Strips the namespace byte from a stored key, giving back what the client sent.
fn user_key(stored: &[u8]) -> Result<&[u8], ProtoError> {
    stored.strip_prefix(&[prefix::RAW]).ok_or_else(|| {
        // Only reachable if a scan ran outside the namespace, which the bounds above prevent.
        ProtoError::internal(format!(
            "a RawKV scan returned a key outside the 'r' namespace: {stored:?}"
        ))
    })
}

/// The column family `RawKv` data lives in.
fn data_cf(db: &Db) -> Result<u32, ProtoError> {
    db.cf_id(cf::DEFAULT)
        .ok_or_else(|| ProtoError::internal("the store opened without its `default` column family"))
}

#[cfg(test)]
mod tests {
    use super::{Limits, namespace_end, scan_bounds, user_key};
    use crate::region::RegionMeta;
    use esker_keys::prefix;

    #[test]
    fn a_stored_key_is_the_user_key_under_the_namespace() {
        assert_eq!(prefix::raw_key(b"k"), b"rk");
        // The case the client lane's test sends: a user key that is itself the namespace byte.
        assert_eq!(prefix::raw_key(b"r"), b"rr");
        assert_eq!(prefix::raw_key(b""), b"r");
        assert_eq!(user_key(b"rr").unwrap(), b"r");
        assert_eq!(user_key(b"r").unwrap(), b"");
    }

    /// Every key the namespace can hold must sort below its end, or an unbounded scan runs
    /// into the transactional key space.
    #[test]
    fn the_namespace_end_is_above_every_key_in_it() {
        let end = namespace_end();
        for user in [&b""[..], b"a", b"r", b"\xff", b"\xff\xff\xff\xff"] {
            assert!(
                prefix::raw_key(user) < end,
                "{user:?} sorts at or above the namespace end"
            );
        }
        assert_eq!(end, vec![prefix::TXN.min(prefix::RAW + 1)]);
    }

    /// An unbounded scan of the whole first region is bounded in storage by the namespace, not
    /// by the end of the key space.
    #[test]
    fn an_unbounded_scan_stops_at_the_end_of_the_namespace() {
        let region = RegionMeta::bootstrap(1, 1, 1);
        let (low, high) = scan_bounds(&region, b"", b"", false).unwrap();
        assert_eq!(low, prefix::raw_key(b""));
        assert_eq!(high, namespace_end());
    }

    #[test]
    fn a_bounded_scan_uses_the_keys_it_was_given() {
        let region = RegionMeta::bootstrap(1, 1, 1);
        let (low, high) = scan_bounds(&region, b"a", b"m", false).unwrap();
        assert_eq!(low, prefix::raw_key(b"a"));
        assert_eq!(high, prefix::raw_key(b"m"));
    }

    /// A reverse scan names its upper bound first, and the range checked is still `[low, high)`.
    #[test]
    fn a_reverse_scan_swaps_the_bounds_it_was_given() {
        let region = RegionMeta::bootstrap(1, 1, 1);
        let (low, high) = scan_bounds(&region, b"z", b"a", true).unwrap();
        assert_eq!(low, prefix::raw_key(b"a"));
        assert_eq!(high, prefix::raw_key(b"z"));
    }

    #[test]
    fn the_limits_are_bounded_and_fit_a_frame() {
        let limits = Limits::new();
        assert!(limits.max_scan_limit > 0);
        assert!(
            limits.max_scan_bytes < esker_proto::MAX_FRAME_SIZE,
            "a full scan must still fit in one frame, with room for its framing"
        );
        assert!(limits.max_delete_range_keys > 0);
    }
}
