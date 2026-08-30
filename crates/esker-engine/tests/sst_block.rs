//! The block format: prefix-compressed entries, restart points, and a cursor that goes both
//! ways.
//!
//! Golden bytes first — the layout is on disk, so it is frozen — then the boundaries: restart
//! intervals at both extremes, empty and single-entry blocks, seeks either side of every gap,
//! and walking off both ends. Two proptests close it out against a `BTreeMap` oracle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::dbformat::{BytewiseComparator, Comparator};
use esker_engine::sst::{Block, BlockBuilder, BlockIter};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;

fn comparator() -> Arc<dyn Comparator> {
    Arc::new(BytewiseComparator)
}

fn build(restart_interval: usize, entries: &[(Vec<u8>, Vec<u8>)]) -> Block {
    let mut builder = BlockBuilder::new(restart_interval);
    for (key, value) in entries {
        builder.add(key, value).expect("entries fit in a block");
    }
    let bytes = builder.finish().to_vec();
    Block::new(Arc::from(bytes.into_boxed_slice())).expect("a freshly built block parses")
}

fn collect(iter: &mut BlockIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    out
}

fn collect_reverse(iter: &mut BlockIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    iter.seek_to_last();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    out
}

fn pairs(keys: &[&[u8]]) -> Vec<(Vec<u8>, Vec<u8>)> {
    keys.iter()
        .enumerate()
        .map(|(i, k)| (k.to_vec(), format!("v{i}").into_bytes()))
        .collect()
}

/// The block bytes are on disk, so this layout is frozen. Decoded by hand here so a
/// change to the entry encoding fails with a readable diff.
#[test]
fn golden_block_bytes() {
    let entries = pairs(&[b"aaaa", b"aaab", b"bbbb"]);
    let mut builder = BlockBuilder::new(2);
    for (key, value) in &entries {
        builder.add(key, value).unwrap();
    }
    let bytes = builder.finish().to_vec();

    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        // entry 0: restart, shared=0 non_shared=4 value_len=2 "aaaa" "v0"
        0x00, 0x04, 0x02, b'a', b'a', b'a', b'a', b'v', b'0',
        // entry 1: shared=3 non_shared=1 value_len=2 "b" "v1"
        0x03, 0x01, 0x02, b'b', b'v', b'1',
        // entry 2: restart (counter hit the interval), shared=0 non_shared=4 "bbbb" "v2"
        0x00, 0x04, 0x02, b'b', b'b', b'b', b'b', b'v', b'2',
        // restart array: offsets 0 and 15, then the count
        0x00, 0x00, 0x00, 0x00,
        0x0f, 0x00, 0x00, 0x00,
        0x02, 0x00, 0x00, 0x00,
    ];
    assert_eq!(
        bytes, expected,
        "the block layout changed; that is a format change (ADR + version bump)"
    );

    let block = Block::new(Arc::from(bytes.into_boxed_slice())).unwrap();
    assert_eq!(block.num_restarts(), 2);
    assert_eq!(collect(&mut block.iter(comparator())), entries);
}

/// An interval of 1 makes every entry a restart point; an interval larger than the block
/// makes exactly one. Both are boundary cases the table builder can produce.
#[test]
fn restart_intervals_at_both_extremes() {
    let entries = pairs(&[b"a", b"b", b"c", b"d"]);
    for (interval, expected_restarts) in [(0usize, 4usize), (1, 4), (3, 2), (99, 1)] {
        let block = build(interval, &entries);
        assert_eq!(
            block.num_restarts(),
            expected_restarts,
            "interval {interval}"
        );
        let mut iter = block.iter(comparator());
        assert_eq!(collect(&mut iter), entries, "interval {interval}");
        let mut reversed = collect_reverse(&mut iter);
        reversed.reverse();
        assert_eq!(reversed, entries, "interval {interval} reversed");
        for (key, value) in &entries {
            iter.seek(key);
            assert!(iter.valid());
            assert_eq!(iter.key(), &key[..]);
            assert_eq!(iter.value(), &value[..]);
        }
    }
}

/// An empty block is well-defined: it parses, and every cursor operation on it is invalid
/// rather than a panic.
#[test]
fn empty_block_is_iterable_and_empty() {
    let block = build(16, &[]);
    assert!(block.is_empty());
    assert_eq!(block.num_restarts(), 1);
    let mut iter = block.iter(comparator());
    assert_eq!(collect(&mut iter), vec![]);
    assert_eq!(collect_reverse(&mut iter), vec![]);
    iter.seek(b"anything");
    assert!(!iter.valid());
    iter.seek_for_prev(b"anything");
    assert!(!iter.valid());
    iter.next();
    iter.prev();
    assert!(!iter.valid());
    iter.status().expect("an empty block is not corrupt");
}

/// A single entry exercises every boundary at once: it is the first, the last, and the
/// only restart point.
#[test]
fn single_entry_block() {
    let entries = pairs(&[b"only"]);
    let block = build(16, &entries);
    let mut iter = block.iter(comparator());
    assert_eq!(collect(&mut iter), entries);
    assert_eq!(collect_reverse(&mut iter), entries);

    iter.seek(b"aaa");
    assert_eq!(iter.key(), b"only");
    iter.seek(b"zzz");
    assert!(!iter.valid(), "seek past the last key must invalidate");
    iter.seek_for_prev(b"zzz");
    assert_eq!(iter.key(), b"only");
    iter.seek_for_prev(b"aaa");
    assert!(!iter.valid(), "no key at or before the first");
}

/// Seek lands on the first key >= target, and `seek_for_prev` on the last key <= target,
/// including between, before and after every entry.
#[test]
fn seek_lands_on_the_right_side_of_a_gap() {
    let entries = pairs(&[b"b", b"d", b"f"]);
    let block = build(2, &entries);
    let mut iter = block.iter(comparator());

    for (target, expected) in [
        (b"a".as_slice(), Some(b"b".as_slice())),
        (b"b", Some(b"b")),
        (b"c", Some(b"d")),
        (b"f", Some(b"f")),
        (b"g", None),
    ] {
        iter.seek(target);
        assert_eq!(
            iter.valid().then(|| iter.key()),
            expected,
            "seek {target:?}"
        );
    }

    for (target, expected) in [
        (b"a".as_slice(), None),
        (b"b", Some(b"b".as_slice())),
        (b"c", Some(b"b")),
        (b"f", Some(b"f")),
        (b"g", Some(b"f")),
    ] {
        iter.seek_for_prev(target);
        assert_eq!(
            iter.valid().then(|| iter.key()),
            expected,
            "seek_for_prev {target:?}"
        );
    }
}

/// Walking off either end and then back must work: an invalid cursor is a position, not a
/// broken state.
#[test]
fn stepping_past_the_ends() {
    let entries = pairs(&[b"a", b"b", b"c"]);
    let block = build(2, &entries);
    let mut iter = block.iter(comparator());

    iter.seek_to_last();
    iter.next();
    assert!(!iter.valid());
    iter.next();
    assert!(!iter.valid(), "next on an invalid cursor must be a no-op");
    iter.seek_to_first();
    assert_eq!(iter.key(), b"a");
    iter.prev();
    assert!(!iter.valid());
    iter.prev();
    assert!(!iter.valid(), "prev on an invalid cursor must be a no-op");
    iter.seek_to_last();
    assert_eq!(iter.key(), b"c");
    iter.status().unwrap();
}

/// Mixing directions must not lose the cursor's place.
#[test]
fn alternating_next_and_prev() {
    let entries = pairs(&[b"k0", b"k1", b"k2", b"k3", b"k4", b"k5"]);
    let block = build(2, &entries);
    let mut iter = block.iter(comparator());
    iter.seek(b"k3");
    assert_eq!(iter.key(), b"k3");
    iter.prev();
    assert_eq!(iter.key(), b"k2");
    iter.next();
    assert_eq!(iter.key(), b"k3");
    iter.next();
    assert_eq!(iter.key(), b"k4");
    iter.prev();
    assert_eq!(iter.key(), b"k3");
    iter.prev();
    assert_eq!(iter.key(), b"k2");
    iter.prev();
    assert_eq!(iter.key(), b"k1");
    iter.status().unwrap();
}

/// A block whose trailer or restart array cannot be believed must be rejected at parse
/// time, not walked into.
#[test]
fn malformed_blocks_are_rejected() {
    let arc = |v: Vec<u8>| Arc::from(v.into_boxed_slice());
    assert!(Block::new(arc(vec![])).is_err(), "no trailer");
    assert!(Block::new(arc(vec![0, 0, 0])).is_err(), "short trailer");
    assert!(Block::new(arc(vec![0, 0, 0, 0])).is_err(), "zero restarts");
    // Claims 1000 restart points in 8 bytes.
    assert!(Block::new(arc(vec![0, 0, 0, 0, 0xe8, 0x03, 0, 0])).is_err());
    // Claims a restart count that overflows when multiplied out.
    assert!(Block::new(arc(vec![0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])).is_err());
}

/// A restart point aimed into the restart array, and an entry claiming to share more
/// bytes than the previous key has: both are corruption a checksum would not catch if the
/// bytes were rewritten wholesale, and both must surface as a status, never a panic.
#[test]
fn corrupt_entries_surface_as_status() {
    let entries = pairs(&[b"aaaa", b"aaab", b"aaac"]);
    let mut builder = BlockBuilder::new(16);
    for (key, value) in &entries {
        builder.add(key, value).unwrap();
    }
    let mut bytes = builder.finish().to_vec();

    // Entry 1 shares 3 bytes with "aaaa"; claim it shares 200.
    let entry1 = 3 + 4 + 2;
    bytes[entry1] = 200;
    let block = Block::new(Arc::from(bytes.into_boxed_slice())).unwrap();
    let mut iter = block.iter(comparator());
    iter.seek_to_first();
    assert_eq!(iter.key(), b"aaaa");
    iter.next();
    assert!(!iter.valid());
    assert!(iter.status().is_err(), "an impossible shared prefix passed");
}

proptest! {
    /// Build from a sorted map, read every key back, forwards and backwards, at every
    /// restart interval. Keys are drawn to collide heavily in their prefixes and to
    /// include 0xFF bytes, which is where a length-prefixed decoder goes wrong.
    #[test]
    fn round_trips_over_arbitrary_sorted_entries(
        raw in prop::collection::btree_map(
            prop_oneof![
                prop::collection::vec(prop_oneof![Just(0u8), Just(0xffu8), any::<u8>()], 0..24),
                (0..8usize, 0..8usize).prop_map(|(a, b)| {
                    let mut k = vec![0xffu8; a];
                    k.extend(std::iter::repeat_n(b'x', b));
                    k
                }),
            ],
            prop::collection::vec(any::<u8>(), 0..40),
            0..64,
        ),
        interval in 1usize..20,
    ) {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = raw.into_iter().collect();
        let block = build(interval, &entries);
        let mut iter = block.iter(comparator());

        prop_assert_eq!(collect(&mut iter), entries.clone());

        let mut reversed = collect_reverse(&mut iter);
        reversed.reverse();
        prop_assert_eq!(reversed, entries.clone());

        // Every key is found exactly, and every key seeks to itself.
        for (key, value) in &entries {
            iter.seek(key);
            prop_assert!(iter.valid());
            prop_assert_eq!(iter.key(), &key[..]);
            prop_assert_eq!(iter.value(), &value[..]);

            iter.seek_for_prev(key);
            prop_assert!(iter.valid());
            prop_assert_eq!(iter.key(), &key[..]);
        }
        prop_assert!(iter.status().is_ok());
    }

    /// Seeking to a key that is not in the block must land where a `BTreeMap` says it
    /// should: the model is the definition of correct here.
    #[test]
    fn seek_matches_a_btree_model(
        keys in prop::collection::btree_set(prop::collection::vec(any::<u8>(), 0..6), 0..40),
        probes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..6), 1..30),
        interval in 1usize..8,
    ) {
        let model: BTreeMap<Vec<u8>, Vec<u8>> =
            keys.iter().map(|k| (k.clone(), k.clone())).collect();
        let entries: Vec<(Vec<u8>, Vec<u8>)> = model.clone().into_iter().collect();
        let block = build(interval, &entries);
        let mut iter = block.iter(comparator());

        for probe in &probes {
            iter.seek(probe);
            let expected = model.range(probe.clone()..).next().map(|(k, _)| k.clone());
            prop_assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);

            iter.seek_for_prev(probe);
            let expected = model.range(..=probe.clone()).next_back().map(|(k, _)| k.clone());
            prop_assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);
        }
        prop_assert!(iter.status().is_ok());
    }
}
