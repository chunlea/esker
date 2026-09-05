//! How a region's **user**-key range reaches the engine.
//!
//! A region owns `[start_key, end_key)` of the user key space (`docs/DESIGN.md` §6), and a store
//! writes each user key into one of two shapes: `'r' ++ user_key` for `RawKV` and
//! `'x' ++ enc(user_key) ++ !ts` for anything transactional. So a region is not one engine range;
//! it is one engine range **per physical namespace**, in every column family that holds data.
//!
//! Three callers need that mapping and it is one mapping:
//!
//! * a snapshot ships the region — every family, every namespace
//!   ([ADR 0032](../../../docs/adr/0032-a-snapshot-carries-every-column-family.md), written after
//!   format version 1 walked `default` under `'r'` alone and dropped every transactional record a
//!   region held);
//! * a reclaim empties a range
//!   ([ADR 0069](../../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md));
//! * a split measures the region and picks a boundary in it
//!   ([ADR 0073](../../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
//!
//! The third arrived last and had its own mapping — `['r' ++ start, 's')` in `default` — which is
//! how a SQL table came to occupy exactly one region whatever its size
//! (`docs/plans/split-region.md`). One definition is the fix, and this module is it.

use esker_keys::prefix;
use esker_proto::Region;

/// The namespace bytes a store actually writes into its data column families
/// (`esker_keys::prefix`), which is what a region's user-key range has to be mapped through.
///
/// **Two, not four.** `'t'` and `'m'` are namespaces of the *user* key space — a SQL row's key is
/// the user key a transaction writes, so it reaches the engine as `'x' ++ enc('t' ++ ..) ++ ts` —
/// so they are covered by walking `'x'` and are not ranges of their own. A third physical
/// namespace would have to be added here — which does not compile until [`physical_ranges`]
/// returns a range for it, and `a_range_is_walked_for_every_physical_namespace` says which.
pub(crate) const PHYSICAL_NAMESPACES: [u8; 2] = [prefix::RAW, prefix::TXN];

/// Every engine range a region's user-key range maps to, one per physical namespace.
///
/// The `'x'` bound uses `esker_txn::key::prefix`, which is the same mapping `txnkv::scan` gives a
/// client's range — memcomparable encoding is order-preserving and prefix-free, so a user key is
/// inside the region exactly when its engine key is inside the bounds here, timestamp suffix and
/// all.
///
/// An empty bound is the namespace's own edge rather than an encoding of the empty key: `'r'` is
/// below every raw key and `'x' + 1` is above every transactional one, which needs no argument
/// about what the codec does with nothing.
pub(crate) fn physical_ranges(region: &Region) -> [(Vec<u8>, Vec<u8>); PHYSICAL_NAMESPACES.len()] {
    physical_ranges_of(&region.start_key, &region.end_key)
}

/// The same mapping for a bare user-key range, which is what a reclaim that is not a whole region
/// needs ([ADR 0069](../../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md)).
///
/// A region *is* a user-key range, so this is the general case and [`physical_ranges`] is the
/// region-shaped call of it. An empty bound means the namespace's own edge, exactly as it does
/// there: `'r'` is below every raw key and `'x' + 1` is above every transactional one.
pub(crate) fn physical_ranges_of(
    start: &[u8],
    end: &[u8],
) -> [(Vec<u8>, Vec<u8>); PHYSICAL_NAMESPACES.len()] {
    let raw_low = prefix::raw_key(start);
    let raw_high = if end.is_empty() {
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(end)
    };
    let txn_low = if start.is_empty() {
        vec![prefix::TXN]
    } else {
        esker_txn::key::prefix(start)
    };
    let txn_high = if end.is_empty() {
        vec![prefix::TXN + 1]
    } else {
        esker_txn::key::prefix(end)
    };
    [(raw_low, raw_high), (txn_low, txn_high)]
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, Region};

    fn region(start: &[u8], end: &[u8]) -> Region {
        Region {
            id: 1,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::INITIAL,
        }
    }

    /// One range per physical namespace, each covering its own and starting at its own byte.
    ///
    /// The guard on the list above: a store that begins writing a third physical namespace has to
    /// add it to `PHYSICAL_NAMESPACES`, which does not compile until `physical_ranges` returns a
    /// range for it — and this says the ranges and the list are in the same order and about the
    /// same bytes, rather than merely the same length.
    #[test]
    fn a_range_is_walked_for_every_physical_namespace() {
        for (namespace, (low, high)) in super::PHYSICAL_NAMESPACES
            .into_iter()
            .zip(super::physical_ranges(&region(b"d", b"m")))
        {
            assert_eq!(low[0], namespace, "the low bound left its namespace");
            assert_eq!(high[0], namespace, "the high bound left its namespace");
            assert!(low < high);
        }
        // An open-ended region reaches the top of each namespace and no further, which is what
        // stops a region that runs to the end of the key space sweeping the next namespace up.
        for (namespace, (low, high)) in super::PHYSICAL_NAMESPACES
            .into_iter()
            .zip(super::physical_ranges(&region(b"", b"")))
        {
            assert_eq!(low, vec![namespace]);
            assert_eq!(high, vec![namespace + 1]);
        }
    }

    /// The `'x'` bounds hold exactly the region's own keys, every version of them.
    ///
    /// This is the property the group encoding buys and the one the whole mapping rests on: it is
    /// order-preserving and prefix-free, so a user key is inside `[start, end)` exactly when
    /// `'x' ++ enc(key) ++ !ts` is inside the bounds — for **every** timestamp, including the two
    /// extremes, which is where a bound that forgot the suffix would fail.
    #[test]
    fn the_transactional_bounds_hold_every_version_of_the_regions_own_keys() {
        let [_, (low, high)] = super::physical_ranges(&region(b"d", b"m"));
        for ts in [0, 1, u64::MAX / 2, u64::MAX] {
            for inside in [&b"d"[..], b"d\x00", b"dz", b"l", b"lzzzzzz"] {
                let key = esker_txn::key::write(inside, ts);
                assert!(
                    key >= low && key < high,
                    "{inside:?} at {ts} is the region's and fell outside its bounds"
                );
            }
            for outside in [&b"c"[..], b"czzz", b"m", b"m\x00", b"z"] {
                let key = esker_txn::key::write(outside, ts);
                assert!(
                    key < low || key >= high,
                    "{outside:?} at {ts} is not the region's and fell inside its bounds"
                );
            }
        }
    }
}
