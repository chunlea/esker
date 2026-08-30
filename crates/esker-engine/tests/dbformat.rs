//! Internal keys: the golden layout, and the order the whole read path depends on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cmp::Ordering;
use std::sync::Arc;

use esker_engine::dbformat::{
    BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, MAX_SEQNO, internal_key,
    split_internal_key,
};
use proptest::prelude::*;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn unhex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn kind_named(name: &str) -> EntryKind {
    match name {
        "Delete" => EntryKind::Delete,
        "Put" => EntryKind::Put,
        "DeleteRange" => EntryKind::DeleteRange,
        other => panic!("unknown kind {other} in the golden file"),
    }
}

fn comparator() -> InternalKeyComparator {
    InternalKeyComparator::new(Arc::new(BytewiseComparator))
}

/// `user_key ++ tag:u64` little-endian, byte for byte, against a file written by a separate
/// encoder.
#[test]
fn golden_internal_keys() {
    let golden = include_str!("golden/internal-keys.txt");
    let mut checked = 0;
    for line in golden.lines() {
        if line.starts_with('#') || line.trim().is_empty() || line.starts_with("order ") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "malformed golden line: {line}");
        let (user, seqno, kind, expected) = (
            unhex(parts[0]),
            parts[1].parse::<u64>().unwrap(),
            kind_named(parts[2]),
            parts[3],
        );

        let key = internal_key(&user, seqno, kind);
        assert_eq!(hex(&key), expected, "{line}");

        let (back_user, back_seqno, back_kind) = split_internal_key(&key).unwrap();
        assert_eq!((back_user, back_seqno, back_kind), (&user[..], seqno, kind));
        checked += 1;
    }
    assert!(checked >= 5, "the golden file lost its cases");
}

/// The golden file also lists keys in the order the engine must sort them: user key ascending,
/// then newest first. Sorting them by the comparator has to be a no-op.
#[test]
fn golden_order_is_the_comparator_order() {
    let golden = include_str!("golden/internal-keys.txt");
    let expected: Vec<Vec<u8>> = golden
        .lines()
        .filter_map(|line| line.strip_prefix("order "))
        .map(unhex)
        .collect();
    assert!(expected.len() >= 7, "the golden file lost its order cases");

    let c = comparator();
    for pair in expected.windows(2) {
        assert_eq!(
            c.cmp(&pair[0], &pair[1]),
            Ordering::Less,
            "{} should sort before {}",
            hex(&pair[0]),
            hex(&pair[1])
        );
    }

    let mut shuffled = expected.clone();
    shuffled.reverse();
    shuffled.sort_by(|a, b| c.cmp(a, b));
    assert_eq!(shuffled, expected);
}

proptest! {
    /// The definition, checked directly: user key by the user comparator, then tag descending.
    #[test]
    fn order_is_user_ascending_then_tag_descending(
        a_user in prop::collection::vec(any::<u8>(), 0..8),
        a_seq in 0u64..=MAX_SEQNO,
        a_kind in 0u8..3,
        b_user in prop::collection::vec(any::<u8>(), 0..8),
        b_seq in 0u64..=MAX_SEQNO,
        b_kind in 0u8..3,
    ) {
        let a_kind = EntryKind::from_u8(a_kind).unwrap();
        let b_kind = EntryKind::from_u8(b_kind).unwrap();
        let a = internal_key(&a_user, a_seq, a_kind);
        let b = internal_key(&b_user, b_seq, b_kind);

        let expected = a_user.cmp(&b_user).then_with(|| {
            let a_tag = (a_seq << 8) | u64::from(a_kind.as_u8());
            let b_tag = (b_seq << 8) | u64::from(b_kind.as_u8());
            b_tag.cmp(&a_tag)
        });
        prop_assert_eq!(comparator().cmp(&a, &b), expected);
    }

    /// Encoding and decoding are inverse, at every sequence number.
    #[test]
    fn internal_keys_round_trip(
        user in prop::collection::vec(any::<u8>(), 0..32),
        seqno in 0u64..=MAX_SEQNO,
        kind in 0u8..3,
    ) {
        let kind = EntryKind::from_u8(kind).unwrap();
        let key = internal_key(&user, seqno, kind);
        prop_assert_eq!(key.len(), user.len() + 8);
        let (back_user, back_seqno, back_kind) = split_internal_key(&key).unwrap();
        prop_assert_eq!(back_user, &user[..]);
        prop_assert_eq!(back_seqno, seqno);
        prop_assert_eq!(back_kind, kind);
    }

    /// Nothing off a damaged disk may panic the comparator, and it must still be a total order.
    #[test]
    fn arbitrary_bytes_compare_without_panicking(
        a in prop::collection::vec(any::<u8>(), 0..20),
        b in prop::collection::vec(any::<u8>(), 0..20),
    ) {
        let c = comparator();
        let forward = c.cmp(&a, &b);
        let backward = c.cmp(&b, &a);
        prop_assert_eq!(forward, backward.reverse());
        prop_assert_eq!(c.cmp(&a, &a), Ordering::Equal);
    }

    /// A separator taken between two keys must stay strictly between them, or an SST index
    /// entry would point past a key that exists.
    #[test]
    fn shortest_separator_stays_in_range(
        a in prop::collection::vec(any::<u8>(), 0..12),
        b in prop::collection::vec(any::<u8>(), 0..12),
        a_seq in 0u64..1000,
        b_seq in 0u64..1000,
    ) {
        let c = comparator();
        let start = internal_key(&a, a_seq, EntryKind::Put);
        let limit = internal_key(&b, b_seq, EntryKind::Put);
        prop_assume!(c.cmp(&start, &limit) == Ordering::Less);

        let mut separator = start.clone();
        c.find_shortest_separator(&mut separator, &limit);
        prop_assert!(c.cmp(&start, &separator) != Ordering::Greater);
        prop_assert!(c.cmp(&separator, &limit) == Ordering::Less);
    }

    /// A short successor must be greater than or equal to the key it came from.
    #[test]
    fn short_successor_never_goes_backwards(
        key in prop::collection::vec(any::<u8>(), 0..12),
        seq in 0u64..1000,
    ) {
        let c = comparator();
        let key = internal_key(&key, seq, EntryKind::Put);
        let mut successor = key.clone();
        c.find_short_successor(&mut successor);
        prop_assert!(c.cmp(&key, &successor) != Ordering::Greater);
    }
}
