//! Properties of the transaction encodings: round trip, canonicity, ordering, and — the one
//! that matters most — that arbitrary bytes off disk produce an error and never a panic
//! (`CLAUDE.md` invariant 9).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_txn::codec::{Kind, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};
use esker_txn::key;
use proptest::prelude::*;

fn any_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Put),
        Just(Kind::Delete),
        Just(Kind::Rollback),
        Just(Kind::Lock),
    ]
}

fn any_short_value() -> impl Strategy<Value = Option<Bytes>> {
    prop::option::of(
        prop::collection::vec(any::<u8>(), 0..=SHORT_VALUE_MAX_LEN).prop_map(Bytes::from),
    )
}

proptest! {
    #[test]
    fn lock_records_round_trip(
        kind in prop_oneof![Just(Kind::Put), Just(Kind::Delete), Just(Kind::Lock)],
        start_ts: u64,
        ttl_ms: u64,
        primary in prop::collection::vec(any::<u8>(), 1..64),
        short_value in any_short_value(),
    ) {
        let record = LockRecord {
            kind,
            start_ts,
            ttl_ms,
            primary: Bytes::from(primary),
            // Only a Put carries a value; the format rejects one anywhere else on purpose.
            short_value: if kind == Kind::Put { short_value } else { None },
        };
        let bytes = record.encode();
        prop_assert_eq!(LockRecord::decode(&bytes).unwrap(), record.clone());
        // Canonical: one byte string per record, so a golden file means something.
        prop_assert_eq!(LockRecord::decode(&bytes).unwrap().encode(), bytes);
    }

    #[test]
    fn write_records_round_trip(
        kind in any_kind(),
        start_ts: u64,
        short_value in any_short_value(),
    ) {
        let record = WriteRecord {
            kind,
            start_ts,
            short_value: if kind == Kind::Put { short_value } else { None },
        };
        let bytes = record.encode();
        prop_assert_eq!(WriteRecord::decode(&bytes).unwrap(), record.clone());
        prop_assert_eq!(WriteRecord::decode(&bytes).unwrap().encode(), bytes);
    }

    /// The important one. These bytes come off disk, and a decoder that panics on a flipped
    /// bit takes the store down instead of reporting corruption.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = LockRecord::decode(&bytes);
        let _ = WriteRecord::decode(&bytes);
        let _ = key::split(&bytes);
        let _ = key::split_lock(&bytes);
    }

    /// A single flipped bit in a valid record must decode to *something* or to an error, but
    /// never to a different record that re-encodes to the same bytes — that would be a
    /// non-canonical encoding, and the golden file would stop meaning anything.
    #[test]
    fn a_damaged_record_never_decodes_non_canonically(
        start_ts: u64,
        primary in prop::collection::vec(any::<u8>(), 1..32),
        byte in 0usize..64,
        bit in 0u8..8,
    ) {
        let record = LockRecord::new(Kind::Put, start_ts, Bytes::from(primary));
        let mut bytes = record.encode();
        let index = byte % bytes.len();
        bytes[index] ^= 1 << bit;
        if let Ok(decoded) = LockRecord::decode(&bytes) {
            prop_assert_eq!(decoded.encode(), bytes);
        }
    }

    /// Encoded order is user-key order, then newest-version-first — for *every* pair of keys,
    /// including the prefix pairs `esker-keys`' own property test has to exclude
    /// (`docs/txn-spec.md` §2).
    #[test]
    fn versioned_keys_order_by_key_then_newest_first(
        key_a in prop::collection::vec(any::<u8>(), 0..24),
        key_b in prop::collection::vec(any::<u8>(), 0..24),
        ts_a: u64,
        ts_b: u64,
    ) {
        let ka = key::write(&key_a, ts_a);
        let kb = key::write(&key_b, ts_b);
        match key_a.cmp(&key_b) {
            std::cmp::Ordering::Equal => {
                // Within one key: the newer timestamp sorts first.
                prop_assert_eq!(ts_b.cmp(&ts_a), ka.cmp(&kb));
            }
            order => prop_assert_eq!(order, ka.cmp(&kb)),
        }
    }

    /// Every version of a key lies inside that key's range, and no other key's does. This is
    /// what makes "seek, then check the prefix" a correct read.
    #[test]
    fn a_key_s_versions_stay_inside_its_range(
        user_key in prop::collection::vec(any::<u8>(), 0..24),
        other in prop::collection::vec(any::<u8>(), 0..24),
        ts: u64,
    ) {
        let (start, end) = key::version_range(&user_key);
        prop_assert!(start <= key::write(&user_key, ts));
        prop_assert!(key::write(&user_key, ts) < end);
        prop_assert!(key::lock(&user_key) < end);

        if other != user_key {
            let outside = key::write(&other, ts);
            prop_assert!(outside < start || outside >= end, "{other:02x?} is inside {user_key:02x?}'s range");
        }
    }

    #[test]
    fn keys_round_trip(user_key in prop::collection::vec(any::<u8>(), 0..64), ts: u64) {
        prop_assert_eq!(key::split(&key::write(&user_key, ts)).unwrap(), (user_key.clone(), ts));
        prop_assert_eq!(key::split_lock(&key::lock(&user_key)).unwrap(), user_key);
    }

    /// A truncated key is an error at every length, never a shorter key that happens to parse.
    #[test]
    fn a_truncated_key_is_an_error(user_key in prop::collection::vec(any::<u8>(), 1..32), ts: u64) {
        let full = key::write(&user_key, ts);
        for cut in 0..full.len() {
            prop_assert!(key::split(&full[..cut]).is_err(), "cut to {} bytes parsed", cut);
        }
    }
}
