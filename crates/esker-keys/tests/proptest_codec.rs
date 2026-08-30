//! Property tests for the memcomparable codec.
//!
//! Two properties matter, and only one of them is obvious:
//!
//! * **round trip** — `decode(encode(v)) == v`;
//! * **order preservation** — `a < b` if and only if `encode(a) < encode(b)` bytewise.
//!
//! The second is the one the engine actually depends on, and it is the one a round-trip test
//! cannot see. A codec with the sign bit left unflipped, or with a timestamp encoded the
//! right way up, round-trips perfectly and sorts wrongly.
//!
//! Every property runs at least 1,000 cases (`prompts/00-scaffold.md` acceptance).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_keys::codec::{
    CodecError, GROUP_SIZE, Value, ValueKind, dec_ts, decode_bytes, decode_i64, decode_tuple,
    decode_u64, enc_ts, encode_bytes, encode_i64, encode_tuple, encode_u64, encoded_bytes_len,
};
use esker_keys::prefix;
use proptest::prelude::*;

const CASES: u32 = 1_000;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    }
}

/// The acceptance checklist for this phase asks for at least 1,000 cases per property. That
/// is a claim about configuration, so it is checked rather than asserted in a comment: the
/// same [`config`] the properties above use is driven directly and the runs are counted.
#[test]
fn the_property_configuration_runs_at_least_a_thousand_cases() {
    use std::cell::Cell;

    let runs = Cell::new(0u32);
    let mut runner = proptest::test_runner::TestRunner::new(config());
    runner
        .run(&any::<u64>(), |_| {
            runs.set(runs.get() + 1);
            Ok(())
        })
        .expect("the counting property cannot fail");

    assert_eq!(runs.get(), CASES);
    assert!(
        CASES >= 1_000,
        "the phase-0 acceptance checklist asks for 1,000 cases"
    );
}

fn enc_u64(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_u64(value, &mut out);
    out
}

fn enc_i64(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_i64(value, &mut out);
    out
}

fn enc_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bytes(value, &mut out);
    out
}

fn enc_tuple(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_tuple(values, &mut out);
    out
}

/// Byte strings whose length is a multiple of the group size.
///
/// Random lengths almost never land on a group boundary, and that is exactly the case where a
/// missing trailing group breaks the prefix-free property without breaking round trips.
fn group_aligned_bytes() -> impl Strategy<Value = Vec<u8>> {
    (0usize..6).prop_flat_map(|groups| prop::collection::vec(any::<u8>(), groups * GROUP_SIZE))
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn u64_round_trips(value: u64) {
        let encoded = enc_u64(value);
        let (decoded, rest) = decode_u64(&encoded).unwrap();
        prop_assert_eq!(decoded, value);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn u64_order_is_preserved(a: u64, b: u64) {
        prop_assert_eq!(a.cmp(&b), enc_u64(a).cmp(&enc_u64(b)));
    }

    #[test]
    fn i64_round_trips(value: i64) {
        let encoded = enc_i64(value);
        let (decoded, rest) = decode_i64(&encoded).unwrap();
        prop_assert_eq!(decoded, value);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn i64_order_is_preserved(a: i64, b: i64) {
        prop_assert_eq!(a.cmp(&b), enc_i64(a).cmp(&enc_i64(b)));
    }

    #[test]
    fn bytes_round_trip(value in prop::collection::vec(any::<u8>(), 0..64)) {
        let encoded = enc_bytes(&value);
        prop_assert_eq!(encoded.len(), encoded_bytes_len(value.len()));
        let (decoded, rest) = decode_bytes(&encoded).unwrap();
        prop_assert_eq!(decoded, value);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn bytes_order_is_preserved(
        a in prop::collection::vec(any::<u8>(), 0..48),
        b in prop::collection::vec(any::<u8>(), 0..48),
    ) {
        prop_assert_eq!(a.cmp(&b), enc_bytes(&a).cmp(&enc_bytes(&b)));
    }

    #[test]
    fn bytes_encoding_is_prefix_free(
        a in prop::collection::vec(any::<u8>(), 0..48),
        b in prop::collection::vec(any::<u8>(), 0..48),
    ) {
        prop_assume!(a != b);
        let (ea, eb) = (enc_bytes(&a), enc_bytes(&b));
        prop_assert!(!ea.starts_with(&eb), "{eb:?} is a prefix of {ea:?}");
        prop_assert!(!eb.starts_with(&ea), "{ea:?} is a prefix of {eb:?}");
    }

    /// The group-boundary case, in its own property so it is always exercised.
    #[test]
    fn group_aligned_bytes_stay_ordered_and_prefix_free(
        a in group_aligned_bytes(),
        b in group_aligned_bytes(),
    ) {
        let (ea, eb) = (enc_bytes(&a), enc_bytes(&b));
        prop_assert_eq!(a.cmp(&b), ea.cmp(&eb));
        prop_assert_eq!(decode_bytes(&ea).unwrap().0, a.clone());
        if a != b {
            prop_assert!(!ea.starts_with(&eb));
            prop_assert!(!eb.starts_with(&ea));
        }
    }

    /// Appending a byte must always make a value sort later and must never make the shorter
    /// encoding a prefix of the longer one.
    #[test]
    fn extending_a_byte_string_sorts_later(
        base in prop::collection::vec(any::<u8>(), 0..40),
        extra: u8,
    ) {
        let mut longer = base.clone();
        longer.push(extra);
        let (short, long) = (enc_bytes(&base), enc_bytes(&longer));
        prop_assert!(short < long);
        prop_assert!(!long.starts_with(&short));
    }

    #[test]
    fn timestamps_round_trip_and_reverse_their_order(a: u64, b: u64) {
        prop_assert_eq!(dec_ts(&enc_ts(a)).unwrap(), a);
        // Newer versions sort first, so the byte order is the reverse of the numeric order.
        prop_assert_eq!(a.cmp(&b), enc_ts(b).cmp(&enc_ts(a)));
    }

    #[test]
    fn versioned_keys_group_by_user_key(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        ts_a: u64,
        ts_b: u64,
    ) {
        prop_assume!(key_a != key_b);
        prop_assume!(!key_b.starts_with(&key_a) && !key_a.starts_with(&key_b));

        let ka = prefix::txn_key(&key_a, ts_a);
        let kb = prefix::txn_key(&key_b, ts_b);
        prop_assert_eq!(key_a.cmp(&key_b), ka.cmp(&kb));

        let (stripped, ts) = prefix::split_ts(&ka).unwrap();
        prop_assert_eq!(ts, ts_a);
        prop_assert_eq!(&stripped[1..], &key_a[..]);
    }

    #[test]
    fn tuples_round_trip(
        first: u64,
        second in prop::collection::vec(any::<u8>(), 0..32),
        third: i64,
    ) {
        let values = vec![
            Value::U64(first),
            Value::Bytes(second),
            Value::I64(third),
        ];
        let schema = [ValueKind::U64, ValueKind::Bytes, ValueKind::I64];
        let encoded = enc_tuple(&values);
        let (decoded, rest) = decode_tuple(&schema, &encoded).unwrap();
        prop_assert_eq!(decoded, values);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn tuple_order_follows_field_order(
        a in (any::<u64>(), prop::collection::vec(any::<u8>(), 0..24), any::<i64>()),
        b in (any::<u64>(), prop::collection::vec(any::<u8>(), 0..24), any::<i64>()),
    ) {
        let build = |(x, y, z): &(u64, Vec<u8>, i64)| {
            vec![Value::U64(*x), Value::Bytes(y.clone()), Value::I64(*z)]
        };
        // `Value` orders `U64 < I64 < Bytes` between variants, but within one schema every
        // field is compared against a field of the same kind, so this is a field-by-field
        // comparison of the logical values.
        prop_assert_eq!(a.cmp(&b), enc_tuple(&build(&a)).cmp(&enc_tuple(&build(&b))));
    }

    /// These bytes come off disk and off the network. Whatever they are, decoding returns a
    /// value or an error — never a panic (`CLAUDE.md` invariant 9).
    #[test]
    fn decoding_arbitrary_bytes_never_panics(raw in prop::collection::vec(any::<u8>(), 0..80)) {
        let _: Result<_, CodecError> = decode_u64(&raw);
        let _: Result<_, CodecError> = decode_i64(&raw);
        let _: Result<_, CodecError> = decode_bytes(&raw);
        let _: Result<_, CodecError> = dec_ts(&raw);
        let _: Result<_, CodecError> = decode_tuple(
            &[ValueKind::Bytes, ValueKind::U64, ValueKind::I64],
            &raw,
        );
        let _ = prefix::split_ts(&raw);
    }

    /// Anything the decoder does accept must re-encode to exactly the bytes it read: the
    /// encoding is canonical, so there is only one representation per value.
    #[test]
    fn accepted_encodings_are_canonical(raw in prop::collection::vec(any::<u8>(), 0..40)) {
        if let Ok((decoded, rest)) = decode_bytes(&raw) {
            let consumed = raw.len() - rest.len();
            prop_assert_eq!(enc_bytes(&decoded), raw[..consumed].to_vec());
        }
    }
}
