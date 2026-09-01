//! PD's private key space (*fixed*, format version 1).
//!
//! The placement driver keeps its whole state in its own `esker-engine` database, in the
//! default column family, under the `'m'` metadata space of `docs/DESIGN.md` §3. It is a
//! separate database from any store's, so nothing here can collide with a store's `'m'`
//! records — the prefix is kept anyway, because a key space with a documented layout is one
//! that can be dumped, and `esker pd inspect` dumps it.
//!
//! ```text
//! 'm' 'c'                        cluster record: the cluster id, minted once
//! 'm' 'a'                        allocator record: the end of the reserved id batch
//! 'm' 'h'                        operator history: the last few things PD asked for
//! 'm' 'k' ++ tag:u8 ++ end_key   range index:     which region ends here
//! 'm' 'r' ++ region_id:u64 BE    region record:   the Region, the leader hint, the last beat
//! 'm' 's' ++ store_id:u64 BE     store record:    address, stats, the last beat
//! 'm' 't'                        the oracle's high-water mark, in physical milliseconds
//! ```
//!
//! Ids are **big-endian** so that a scan runs in id order, the same reason the store's Raft log
//! is (`esker_store::raft_log`).
//!
//! # The range index, and why it needs a tag byte
//!
//! `GetRegion(key)` asks "which region's range contains this key", and the answer is *the first
//! region whose end key is past it*. That is one seek in a key space ordered by end key — which
//! is what [`range_key`] builds — with two wrinkles this module is entirely about.
//!
//! **An empty `end_key` means +∞** in region metadata (`docs/plans/phase-4.md` §5), and `b""`
//! sorts *below* every byte string. A plain index keyed by end key would therefore put the last
//! region — the one covering everything above its start — first, and every lookup past the
//! second-to-last region would fall off the end and find nothing. The `tag` byte fixes it:
//! bounded ends get [`TAG_BOUNDED`], the unbounded end gets [`TAG_UNBOUNDED`], and since
//! `1 < 2` the unbounded region sorts last, which is where +∞ belongs.
//!
//! **An end key is exclusive**, so a region ending exactly at `key` does *not* contain it and
//! must be skipped. [`range_seek_key`] seeks to `key ++ 0x00` rather than to `key`: appending a
//! zero byte is the immediate successor of a byte string, so the seek lands on the first index
//! entry strictly greater than `key` and the region that ends there is behind us.

use std::mem::size_of;

/// The metadata prefix of `docs/DESIGN.md` §3. Every key here starts with it.
pub const PREFIX: u8 = b'm';

/// Second byte of the cluster record's key.
pub const CLUSTER: u8 = b'c';
/// Second byte of the allocator record's key.
pub const ALLOC: u8 = b'a';
/// Second byte of the operator history's key.
pub const HISTORY: u8 = b'h';
/// Second byte of the range index's keys.
pub const RANGE: u8 = b'k';
/// Second byte of a region record's key.
pub const REGION: u8 = b'r';
/// Second byte of a store record's key.
pub const STORE: u8 = b's';
/// Second byte of the oracle's high-water mark.
pub const TSO: u8 = b't';
/// Second byte of the columnar-placement record's key.
///
/// `'l'` for the **learner** [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md)
/// Decision 1 calls a columnar replica — deliberately the same letter `esker-sql` uses for the
/// catalog record this is reported from, so the two read as one thing in two places.
pub const COLUMNAR: u8 = b'l';

/// Range-index tag for a region with a bounded end key.
pub const TAG_BOUNDED: u8 = 1;
/// Range-index tag for the region that runs to the end of the key space.
///
/// Above [`TAG_BOUNDED`] so that +∞ sorts last, which `b""` on its own would not.
pub const TAG_UNBOUNDED: u8 = 2;

/// Bytes in a key that is a prefix and a `u64`: `'m' ++ kind ++ id`.
pub const ID_KEY_LEN: usize = 2 + size_of::<u64>();

/// `'m' 'c'` — the cluster record.
#[must_use]
pub fn cluster_key() -> [u8; 2] {
    [PREFIX, CLUSTER]
}

/// `'m' 'a'` — the allocator record.
#[must_use]
pub fn alloc_key() -> [u8; 2] {
    [PREFIX, ALLOC]
}

/// `'m' 'h'` — the operator history.
#[must_use]
pub fn history_key() -> [u8; 2] {
    [PREFIX, HISTORY]
}

/// `'m' 'l'` — the whole columnar wish list, in one record.
///
/// One record rather than one per range, because a report is a **full assertion**: the set is
/// replaced as a unit, so storing it as a unit means a report can never be half-applied.
#[must_use]
pub fn columnar_key() -> [u8; 2] {
    [PREFIX, COLUMNAR]
}

/// `'m' 't'` — the oracle's high-water mark.
#[must_use]
pub fn tso_key() -> [u8; 2] {
    [PREFIX, TSO]
}

/// `'m' 's' ++ store_id`.
#[must_use]
pub fn store_key(store_id: u64) -> [u8; ID_KEY_LEN] {
    id_key(STORE, store_id)
}

/// `'m' 'r' ++ region_id`.
#[must_use]
pub fn region_key(region_id: u64) -> [u8; ID_KEY_LEN] {
    id_key(REGION, region_id)
}

fn id_key(kind: u8, id: u64) -> [u8; ID_KEY_LEN] {
    let mut key = [0_u8; ID_KEY_LEN];
    key[0] = PREFIX;
    key[1] = kind;
    key[2..].copy_from_slice(&id.to_be_bytes());
    key
}

/// The id in a key built by [`store_key`] or [`region_key`], or `None` if `key` is not one.
///
/// Used by the scans that rebuild PD's view: a key that does not parse is a key from another
/// part of the space, and the scan stops rather than guessing what it holds.
#[must_use]
pub fn id_in_key(kind: u8, key: &[u8]) -> Option<u64> {
    if key.len() != ID_KEY_LEN || key[0] != PREFIX || key[1] != kind {
        return None;
    }
    let mut id = [0_u8; size_of::<u64>()];
    id.copy_from_slice(&key[2..]);
    Some(u64::from_be_bytes(id))
}

/// The two-byte prefix every key of `kind` starts with, for a scan's bound.
#[must_use]
pub fn prefix(kind: u8) -> [u8; 2] {
    [PREFIX, kind]
}

/// The range-index key for a region ending at `end_key`, where empty means +∞.
#[must_use]
pub fn range_key(end_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + end_key.len());
    key.extend_from_slice(&[PREFIX, RANGE]);
    if end_key.is_empty() {
        key.push(TAG_UNBOUNDED);
    } else {
        key.push(TAG_BOUNDED);
        key.extend_from_slice(end_key);
    }
    key
}

/// Where a lookup for `key` starts: the first index entry whose end key is **strictly** past it.
///
/// The trailing zero byte is the point. `key ++ 0x00` is the immediate successor of `key` in
/// byte order, so seeking to it skips the region that ends exactly at `key` — which does not
/// contain it, an end key being exclusive — and lands on the first one that might.
#[must_use]
pub fn range_seek_key(key: &[u8]) -> Vec<u8> {
    let mut seek = Vec::with_capacity(4 + key.len());
    seek.extend_from_slice(&[PREFIX, RANGE, TAG_BOUNDED]);
    seek.extend_from_slice(key);
    seek.push(0);
    seek
}

/// The region id an index entry's *value* holds is the record; this reads the end key back out
/// of the index *key*, for the inspector and for the tests.
///
/// `None` when `key` is not a range-index key; `Some(None)` for the unbounded region.
#[must_use]
pub fn end_key_in_range_key(key: &[u8]) -> Option<Option<&[u8]>> {
    if key.len() < 3 || key[0] != PREFIX || key[1] != RANGE {
        return None;
    }
    match key[2] {
        TAG_BOUNDED => Some(Some(&key[3..])),
        TAG_UNBOUNDED if key.len() == 3 => Some(None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ID_KEY_LEN, RANGE, REGION, STORE, TAG_BOUNDED, TAG_UNBOUNDED, alloc_key, cluster_key,
        end_key_in_range_key, history_key, id_in_key, prefix, range_key, range_seek_key,
        region_key, store_key, tso_key,
    };

    /// Every key in this space is distinct from every other, and none is a prefix of another
    /// kind's. Two records sharing a key is a corruption that no checksum can see.
    #[test]
    fn the_singleton_keys_are_distinct() {
        let keys = [cluster_key(), alloc_key(), tso_key(), history_key()];
        let unique: std::collections::BTreeSet<[u8; 2]> = keys.into_iter().collect();
        assert_eq!(unique.len(), keys.len());
        for key in keys {
            assert_ne!(key[1], STORE);
            assert_ne!(key[1], REGION);
            assert_ne!(key[1], RANGE);
        }
    }

    #[test]
    fn an_id_key_round_trips_and_sorts_by_id() {
        for id in [0_u64, 1, 42, u64::MAX] {
            assert_eq!(id_in_key(STORE, &store_key(id)), Some(id));
            assert_eq!(id_in_key(REGION, &region_key(id)), Some(id));
            // A key of one kind never parses as another's.
            assert_eq!(id_in_key(REGION, &store_key(id)), None);
        }
        assert!(store_key(1) < store_key(2));
        assert!(region_key(2) < region_key(300));
        assert_eq!(store_key(1).len(), ID_KEY_LEN);
        assert!(store_key(u64::MAX).starts_with(&prefix(STORE)));
    }

    /// The whole reason for the tag byte: the region running to the end of the key space must
    /// sort **after** every bounded one, and `b""` does not.
    #[test]
    fn the_unbounded_region_sorts_last() {
        let bounded = [range_key(b"a"), range_key(b"m"), range_key(b"\xff\xff")];
        let unbounded = range_key(b"");
        for key in &bounded {
            assert!(key < &unbounded, "{key:?} did not sort before +infinity");
        }
        assert_eq!(unbounded[2], TAG_UNBOUNDED);
        assert_eq!(bounded[0][2], TAG_BOUNDED);
    }

    /// An end key is exclusive, so the seek for `key` must land past a region ending at `key`.
    #[test]
    fn the_seek_skips_a_region_that_ends_exactly_at_the_key() {
        let ends_at_m = range_key(b"m");
        let seek = range_seek_key(b"m");
        assert!(
            ends_at_m < seek,
            "a region ending at `m` does not contain `m`"
        );
        // And it does not skip the one that ends just past it.
        assert!(range_key(b"m\x00") >= seek);
        assert!(range_key(b"n") > seek);
    }

    /// The empty key is the smallest key in the space, and the first region has to own it.
    ///
    /// The comparison is `>=`, not `>`, because a seek lands on the first entry **at or after**
    /// its target: the index entry for a region ending at `b"\x00"` *is* the seek key for
    /// `b""`, and that region does contain the empty key.
    #[test]
    fn the_empty_key_seeks_below_every_bounded_end() {
        let seek = range_seek_key(b"");
        for end in [&b"\x00"[..], b"a", b"\xff"] {
            assert!(
                range_key(end) >= seek,
                "end {end:?} was skipped for the empty key"
            );
        }
        assert!(
            range_key(b"") > seek,
            "+infinity was skipped for the empty key"
        );
    }

    /// The property the seek is built on, stated directly: an index entry is at or after the
    /// seek key for `key` exactly when its end key is strictly past `key`.
    #[test]
    fn an_entry_is_found_exactly_when_its_end_is_past_the_key() {
        let keys: [&[u8]; 6] = [b"", b"\x00", b"a", b"ab", b"b", b"\xff"];
        for key in keys {
            let seek = range_seek_key(key);
            for end in keys {
                if end.is_empty() {
                    continue; // +infinity is past every key, and is tested above.
                }
                assert_eq!(
                    range_key(end) >= seek,
                    end > key,
                    "end {end:?} against key {key:?}"
                );
            }
        }
    }

    #[test]
    fn a_range_key_gives_its_end_back() {
        assert_eq!(
            end_key_in_range_key(&range_key(b"mm")),
            Some(Some(&b"mm"[..]))
        );
        assert_eq!(end_key_in_range_key(&range_key(b"")), Some(None));
        assert_eq!(end_key_in_range_key(&store_key(1)), None);
        assert_eq!(end_key_in_range_key(b"m"), None);
    }
}
