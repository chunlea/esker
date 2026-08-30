//! The memcomparable codec: encoded byte order equals logical order
//! (`docs/DESIGN.md` §3).
//!
//! Everything below `esker-keys` compares keys with `memcmp`. That is only useful if the
//! encoding preserves order, so each type is encoded to make `memcmp` give the right answer:
//!
//! * `u64` — eight bytes big-endian, because that is what `memcmp` already orders correctly.
//! * `i64` — the same, with the sign bit flipped first, which maps the signed range onto the
//!   unsigned range in order.
//! * `bytes` — eight-byte groups, each followed by a marker byte `0xFF - pad_count`. The last
//!   group is zero-padded. This is the `TiDB` group encoding, and it is **prefix-free**: no
//!   encoded value is a byte prefix of another, which is what makes tuples safe to
//!   concatenate.
//! * tuples — the concatenation of the encoded fields. Because every field encoding is either
//!   fixed-width or prefix-free, the concatenation compares field by field.
//!
//! The bytes encoding has one trap worth stating plainly: an input whose length is a multiple
//! of eight still gets a trailing padded group. Without it `encode(b"12345678")` would be a
//! byte prefix of `encode(b"123456789")` and the prefix-free guarantee would quietly fail.

use thiserror::Error;

/// Bytes of payload in one group of the bytes encoding.
pub const GROUP_SIZE: usize = 8;

/// Bytes one group occupies once encoded: payload plus its marker.
pub const ENCODED_GROUP_SIZE: usize = GROUP_SIZE + 1;

/// Marker of a group that is entirely payload and is followed by another group.
pub const MARKER_FULL: u8 = 0xFF;

/// Width of an encoded `u64`, `i64` or timestamp.
pub const FIXED_INT_SIZE: usize = 8;

/// Why a key could not be decoded.
///
/// Every variant is reachable from bytes read off disk or off the network, so decoding
/// returns one of these rather than panicking (`CLAUDE.md` invariant 9).
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The buffer ended in the middle of a value.
    #[error("truncated key: needed {needed} more bytes, found {found}")]
    Truncated {
        /// Bytes the value requires.
        needed: usize,
        /// Bytes actually available.
        found: usize,
    },
    /// A group marker was not one of `0xF7..=0xFF`, so this is not a bytes encoding.
    #[error("invalid group marker {marker:#04x}: expected 0xf7..=0xff")]
    BadGroupMarker {
        /// The byte that was found where a marker belongs.
        marker: u8,
    },
    /// A final group claimed padding, but the padding bytes were not zero. The encoding is
    /// canonical, so this means the bytes are damaged.
    #[error("non-zero padding in the final group of a bytes encoding")]
    DirtyPadding,
    /// The caller decoded a whole key and bytes were left over.
    #[error("{count} trailing bytes after the last field")]
    TrailingBytes {
        /// How many bytes were left.
        count: usize,
    },
}

/// A decoded field. Encodings are not self-describing, so decoding needs the schema — see
/// [`decode_tuple`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// An unsigned 64-bit integer.
    U64(u64),
    /// A signed 64-bit integer.
    I64(i64),
    /// A byte string of any length.
    Bytes(Vec<u8>),
}

/// The type of a field, which is what a decoder needs to know in advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueKind {
    /// See [`Value::U64`].
    U64,
    /// See [`Value::I64`].
    I64,
    /// See [`Value::Bytes`].
    Bytes,
}

impl Value {
    /// The kind of this value, for building a schema from an example tuple.
    #[must_use]
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::U64(_) => ValueKind::U64,
            Value::I64(_) => ValueKind::I64,
            Value::Bytes(_) => ValueKind::Bytes,
        }
    }
}

/// Appends the encoding of `value`, eight bytes big-endian.
pub fn encode_u64(value: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Decodes a `u64`, returning it and the rest of the buffer.
pub fn decode_u64(buf: &[u8]) -> Result<(u64, &[u8]), CodecError> {
    let (head, rest) = split_fixed(buf)?;
    Ok((u64::from_be_bytes(head), rest))
}

/// Appends the encoding of `value`: big-endian with the sign bit flipped, so that negative
/// values sort before non-negative ones.
pub fn encode_i64(value: i64, out: &mut Vec<u8>) {
    out.extend_from_slice(&flip_sign(value).to_be_bytes());
}

/// Decodes an `i64`, returning it and the rest of the buffer.
pub fn decode_i64(buf: &[u8]) -> Result<(i64, &[u8]), CodecError> {
    let (head, rest) = split_fixed(buf)?;
    Ok((unflip_sign(u64::from_be_bytes(head)), rest))
}

/// Maps the signed range onto the unsigned range in order: `i64::MIN` becomes `0`.
fn flip_sign(value: i64) -> u64 {
    #[allow(
        clippy::cast_sign_loss,
        reason = "the xor is the order-preserving mapping"
    )]
    {
        (value as u64) ^ (1 << 63)
    }
}

/// Inverse of [`flip_sign`].
fn unflip_sign(value: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap, reason = "inverse of flip_sign")]
    {
        (value ^ (1 << 63)) as i64
    }
}

fn split_fixed(buf: &[u8]) -> Result<([u8; FIXED_INT_SIZE], &[u8]), CodecError> {
    if buf.len() < FIXED_INT_SIZE {
        return Err(CodecError::Truncated {
            needed: FIXED_INT_SIZE,
            found: buf.len(),
        });
    }
    let (head, rest) = buf.split_at(FIXED_INT_SIZE);
    let mut fixed = [0u8; FIXED_INT_SIZE];
    fixed.copy_from_slice(head);
    Ok((fixed, rest))
}

/// Bytes that [`encode_bytes`] will append for an input of `len` bytes.
///
/// Always at least one group, and always one more group when `len` is a multiple of
/// [`GROUP_SIZE`] — that trailing group is what keeps the encoding prefix-free.
#[must_use]
pub fn encoded_bytes_len(len: usize) -> usize {
    (len / GROUP_SIZE + 1) * ENCODED_GROUP_SIZE
}

/// Appends the group encoding of `value`.
pub fn encode_bytes(value: &[u8], out: &mut Vec<u8>) {
    out.reserve(encoded_bytes_len(value.len()));

    let mut chunks = value.chunks_exact(GROUP_SIZE);
    for chunk in &mut chunks {
        out.extend_from_slice(chunk);
        out.push(MARKER_FULL);
    }

    // The final group is always written, even when the remainder is empty. That is the
    // prefix-free property: `encode(b"12345678")` must not be a prefix of
    // `encode(b"123456789")`.
    let tail = chunks.remainder();
    let padding = GROUP_SIZE - tail.len();
    out.extend_from_slice(tail);
    out.extend(std::iter::repeat_n(0u8, padding));
    #[allow(clippy::cast_possible_truncation, reason = "padding is 1..=8")]
    out.push(MARKER_FULL - padding as u8);
}

/// Decodes a group-encoded byte string, returning it and the rest of the buffer.
///
/// Rejects a truncated group, a marker outside `0xF7..=0xFF`, and padding that is not zero.
/// The encoding is canonical, so each of those means the bytes are not something this codec
/// wrote.
pub fn decode_bytes(buf: &[u8]) -> Result<(Vec<u8>, &[u8]), CodecError> {
    let mut out = Vec::new();
    let mut rest = buf;

    loop {
        if rest.len() < ENCODED_GROUP_SIZE {
            return Err(CodecError::Truncated {
                needed: ENCODED_GROUP_SIZE,
                found: rest.len(),
            });
        }
        let (group, tail) = rest.split_at(ENCODED_GROUP_SIZE);
        let (payload, marker) = (&group[..GROUP_SIZE], group[GROUP_SIZE]);
        rest = tail;

        if marker == MARKER_FULL {
            out.extend_from_slice(payload);
            continue;
        }

        let padding = usize::from(MARKER_FULL - marker);
        if padding > GROUP_SIZE {
            return Err(CodecError::BadGroupMarker { marker });
        }
        let kept = GROUP_SIZE - padding;
        if payload[kept..].iter().any(|&byte| byte != 0) {
            return Err(CodecError::DirtyPadding);
        }
        out.extend_from_slice(&payload[..kept]);
        return Ok((out, rest));
    }
}

/// Appends one field.
pub fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::U64(v) => encode_u64(*v, out),
        Value::I64(v) => encode_i64(*v, out),
        Value::Bytes(v) => encode_bytes(v, out),
    }
}

/// Appends a tuple: just the fields, one after another.
///
/// Every field encoding is fixed-width or prefix-free, so the concatenation compares field by
/// field — which is why the trailing group in [`encode_bytes`] is not optional.
pub fn encode_tuple(values: &[Value], out: &mut Vec<u8>) {
    for value in values {
        encode_value(value, out);
    }
}

/// Decodes a tuple with the given schema, returning the fields and the rest of the buffer.
///
/// The encoding carries no type tags — `docs/DESIGN.md` §3 defines a tuple as the plain
/// concatenation of its fields — so the caller supplies the schema.
pub fn decode_tuple<'a>(
    schema: &[ValueKind],
    buf: &'a [u8],
) -> Result<(Vec<Value>, &'a [u8]), CodecError> {
    let mut values = Vec::with_capacity(schema.len());
    let mut rest = buf;
    for kind in schema {
        let value = match kind {
            ValueKind::U64 => {
                let (v, tail) = decode_u64(rest)?;
                rest = tail;
                Value::U64(v)
            }
            ValueKind::I64 => {
                let (v, tail) = decode_i64(rest)?;
                rest = tail;
                Value::I64(v)
            }
            ValueKind::Bytes => {
                let (v, tail) = decode_bytes(rest)?;
                rest = tail;
                Value::Bytes(v)
            }
        };
        values.push(value);
    }
    Ok((values, rest))
}

/// Encodes an MVCC timestamp so that **newer versions sort first**.
///
/// The value is the bitwise complement of `ts`, big-endian. Complementing reverses the order,
/// which is what turns a point read into "the first key under this prefix" instead of a scan
/// to the end of the version chain (`docs/DESIGN.md` §3).
#[must_use]
pub fn enc_ts(ts: u64) -> [u8; FIXED_INT_SIZE] {
    (!ts).to_be_bytes()
}

/// Recovers a timestamp written by [`enc_ts`].
pub fn dec_ts(buf: &[u8]) -> Result<u64, CodecError> {
    let (encoded, _) = split_fixed(buf)?;
    Ok(!u64::from_be_bytes(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_bytes(value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_bytes(value, &mut out);
        out
    }

    /// The trap this codec exists to avoid. An input of exactly one group still gets a
    /// trailing padded group, so no encoding is a byte prefix of another.
    #[test]
    fn a_full_group_still_gets_a_trailing_group() {
        let eight = enc_bytes(b"12345678");
        let nine = enc_bytes(b"123456789");

        assert_eq!(eight.len(), 18);
        assert_eq!(nine.len(), 18);
        assert_eq!(&eight[..9], b"12345678\xff");
        assert_eq!(&eight[9..], b"\0\0\0\0\0\0\0\0\xf7");

        assert!(!nine.starts_with(&eight), "the encoding is not prefix-free");
        assert!(eight < nine, "ordering broke at a group boundary");
    }

    /// Multiples of eight are the length class the property tests are least likely to hit by
    /// chance, so they are checked explicitly.
    #[test]
    fn multiples_of_the_group_size_stay_ordered_and_prefix_free() {
        for groups in 0..5usize {
            let base = vec![b'a'; groups * GROUP_SIZE];
            let mut longer = base.clone();
            longer.push(b'a');

            let encoded_base = enc_bytes(&base);
            let encoded_longer = enc_bytes(&longer);

            assert_eq!(encoded_base.len(), encoded_bytes_len(base.len()));
            assert_eq!(encoded_base.len() % ENCODED_GROUP_SIZE, 0);
            assert!(encoded_base < encoded_longer, "{groups} groups");
            assert!(
                !encoded_longer.starts_with(&encoded_base),
                "{groups} groups"
            );
            assert_eq!(decode_bytes(&encoded_base).unwrap().0, base);
        }
    }

    #[test]
    fn empty_input_encodes_to_one_padded_group() {
        assert_eq!(enc_bytes(b""), b"\0\0\0\0\0\0\0\0\xf7");
        assert_eq!(decode_bytes(&enc_bytes(b"")).unwrap().0, b"");
        // The empty string sorts before everything, including a string of zero bytes.
        assert!(enc_bytes(b"") < enc_bytes(b"\0"));
    }

    /// The direction of `enc_ts` is the whole point, and a round-trip test passes just as
    /// happily with it inverted. So this asserts the direction itself.
    #[test]
    fn newer_timestamps_sort_first() {
        let pairs = [
            (0u64, 1u64),
            (1, 2),
            (1, u64::MAX),
            (100, 1_000_000),
            (0, u64::MAX),
        ];
        for (older, newer) in pairs {
            assert!(older < newer, "test data is wrong");
            assert!(
                enc_ts(older) > enc_ts(newer),
                "ts {older} must encode after ts {newer}"
            );
        }
        assert_eq!(enc_ts(0), [0xFF; 8]);
        assert_eq!(enc_ts(u64::MAX), [0x00; 8]);
        for ts in [0u64, 1, 42, u64::MAX / 2, u64::MAX] {
            assert_eq!(dec_ts(&enc_ts(ts)).unwrap(), ts);
        }
    }

    #[test]
    fn signed_integers_sort_across_zero() {
        let ordered = [i64::MIN, -1_000_000, -1, 0, 1, 1_000_000, i64::MAX];
        let mut previous: Option<Vec<u8>> = None;
        for value in ordered {
            let mut encoded = Vec::new();
            encode_i64(value, &mut encoded);
            if let Some(previous) = &previous {
                assert!(previous < &encoded, "{value} sorted out of order");
            }
            assert_eq!(decode_i64(&encoded).unwrap().0, value);
            previous = Some(encoded);
        }
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(matches!(
            decode_u64(&[0, 1, 2]),
            Err(CodecError::Truncated {
                needed: 8,
                found: 3
            })
        ));
        assert!(matches!(
            decode_bytes(&[0; 8]),
            Err(CodecError::Truncated { .. })
        ));
        // A marker below 0xF7 claims more than eight padding bytes.
        assert!(matches!(
            decode_bytes(&[0, 0, 0, 0, 0, 0, 0, 0, 0x10]),
            Err(CodecError::BadGroupMarker { marker: 0x10 })
        ));
        // Padding must be zero: the encoding is canonical.
        assert_eq!(
            decode_bytes(&[b'a', 0, 0, 0, 0, 0, 0, 1, 0xF8]),
            Err(CodecError::DirtyPadding)
        );
        // A run of full-group markers that never terminates.
        assert!(matches!(
            decode_bytes(&[0xFF; 18]),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn tuples_decode_back_to_their_fields() {
        let values = vec![
            Value::U64(7),
            Value::Bytes(b"region".to_vec()),
            Value::I64(-3),
            Value::Bytes(Vec::new()),
        ];
        let schema: Vec<ValueKind> = values.iter().map(Value::kind).collect();

        let mut encoded = Vec::new();
        encode_tuple(&values, &mut encoded);
        encoded.extend_from_slice(b"suffix");

        let (decoded, rest) = decode_tuple(&schema, &encoded).unwrap();
        assert_eq!(decoded, values);
        assert_eq!(rest, b"suffix");
    }

    /// The reason bytes must be prefix-free: otherwise a field boundary could be misread and
    /// two different tuples could compare equal.
    #[test]
    fn tuple_order_follows_field_order() {
        let cases = [
            (
                vec![Value::Bytes(b"a".to_vec()), Value::U64(2)],
                vec![Value::Bytes(b"ab".to_vec()), Value::U64(1)],
            ),
            (
                vec![Value::Bytes(b"a".to_vec()), Value::U64(1)],
                vec![Value::Bytes(b"a".to_vec()), Value::U64(2)],
            ),
            (
                vec![Value::U64(1), Value::Bytes(b"z".to_vec())],
                vec![Value::U64(2), Value::Bytes(b"a".to_vec())],
            ),
        ];
        for (smaller, larger) in cases {
            assert!(smaller < larger, "test data is wrong");
            let (mut a, mut b) = (Vec::new(), Vec::new());
            encode_tuple(&smaller, &mut a);
            encode_tuple(&larger, &mut b);
            assert!(a < b, "{smaller:?} did not encode before {larger:?}");
        }
    }
}
