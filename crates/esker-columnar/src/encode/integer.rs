//! Integers and timestamps: frame of reference, or deltas.
//!
//! ```text
//! frame of reference  min:zigzag varint ++ width:u8 ++ packed(v - min)
//! delta               first:zigzag varint ++ packed_for(zigzag(v[i] - v[i-1]))
//! ```
//!
//! Frame of reference wins on a column whose values sit in a narrow band anywhere on the number
//! line — an `id` between 1,000,000 and 1,001,000 costs ten bits a row, not sixty-four. Delta wins
//! on a column that climbs: a timestamp column arriving in order has deltas of a few milliseconds
//! whatever the absolute values are, and after zigzag and a second frame of reference those cost
//! a handful of bits. Both are tried and the smaller kept, because a rule for guessing which is
//! a magic number and this is two passes over cached data.
//!
//! # The two pieces of arithmetic the proptests exist to hold
//!
//! **Frame of reference subtracts in `u64`.** `v - min` where both are `i64` can exceed `i64` —
//! `i64::MAX - i64::MIN` does not fit — but never exceeds `u64`. So the offset is
//! `(v as u64).wrapping_sub(min as u64)`, which is the true unsigned distance for any `v >= min`
//! in two's complement, and the decoder adds it back the same way.
//!
//! **Delta wraps.** The difference between neighbours has the same problem and no minimum to
//! lean on, so it is a `wrapping_sub` into an `i64`, zigzagged into a `u64`, and
//! `wrapping_add`ed back. Exact for every pair of `i64`s, including the pairs that overflow.

use esker_base::varint;

use crate::cursor::Cursor;
use crate::encode::{Encoding, bitpack};
use crate::error::{Error, Result};

/// Encodes `values` both ways and keeps the smaller.
///
/// Ties go to frame of reference: it is the cheaper decode — one pass, no dependency between
/// neighbours — and a deterministic tie-break is what makes the golden files reproducible.
pub(crate) fn encode(values: &[i64]) -> (Encoding, Vec<u8>) {
    let mut reference = Vec::new();
    encode_reference(values, &mut reference);
    let mut delta = Vec::new();
    encode_delta(values, &mut delta);

    if delta.len() < reference.len() {
        (Encoding::Delta, delta)
    } else {
        (Encoding::FrameOfReference, reference)
    }
}

/// Reads `count` integers stored under `encoding`.
pub(crate) fn decode(
    encoding: Encoding,
    cursor: &mut Cursor<'_>,
    count: usize,
) -> Result<Vec<i64>> {
    match encoding {
        Encoding::FrameOfReference => decode_reference(cursor, count),
        Encoding::Delta => decode_delta(cursor, count),
        other => Err(Error::corruption(
            "integer column",
            format!("{other:?} is not an integer encoding"),
        )),
    }
}

fn encode_reference(values: &[i64], out: &mut Vec<u8>) {
    let min = values.iter().copied().min().unwrap_or(0);
    varint::put_i64(min, out);
    // The trick this encoding rests on: `v - min` can exceed `i64` but never `u64`, and the
    // unsigned distance between two `i64`s is exactly their two's-complement difference.
    #[allow(clippy::cast_sign_loss)]
    let offsets: Vec<u64> = values
        .iter()
        .map(|v| (*v as u64).wrapping_sub(min as u64))
        .collect();
    let width = bitpack::bit_width(offsets.iter().copied().max().unwrap_or(0));
    out.push(u8::try_from(width).unwrap_or(u8::MAX));
    bitpack::pack(&offsets, width, out);
}

fn decode_reference(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<i64>> {
    let min = varint::zigzag_decode(cursor.varint("frame minimum")?);
    let width = u32::from(cursor.u8("frame width")?);
    // The inverse of the cast in `encode_reference`, and exact for the same reason.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let values = bitpack::unpack(cursor, count, width, "frame offsets")?
        .into_iter()
        .map(|offset| (min as u64).wrapping_add(offset) as i64)
        .collect();
    Ok(values)
}

fn encode_delta(values: &[i64], out: &mut Vec<u8>) {
    varint::put_i64(values.first().copied().unwrap_or(0), out);
    let deltas: Vec<u64> = values
        .windows(2)
        .map(|pair| varint::zigzag_encode(pair[1].wrapping_sub(pair[0])))
        .collect();
    bitpack::pack_for(&deltas, out);
}

fn decode_delta(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<i64>> {
    let first = varint::zigzag_decode(cursor.varint("delta first value")?);
    let deltas = bitpack::unpack_for(cursor, count.saturating_sub(1), "deltas")?;
    let mut out = Vec::with_capacity(count);
    if count == 0 {
        return Ok(out);
    }
    out.push(first);
    let mut current = first;
    for delta in deltas {
        current = current.wrapping_add(varint::zigzag_decode(delta));
        out.push(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{decode, encode};
    use crate::cursor::Cursor;
    use crate::encode::Encoding;

    fn round_trip(values: &[i64]) -> (Encoding, usize, Vec<i64>) {
        let (encoding, bytes) = encode(values);
        let mut cursor = Cursor::new(&bytes, "test");
        let decoded = decode(encoding, &mut cursor, values.len()).unwrap();
        assert_eq!(cursor.remaining(), 0, "{encoding:?} left bytes behind");

        // Whichever the writer chose, the other must also be exact: the choice is about size.
        for candidate in [Encoding::FrameOfReference, Encoding::Delta] {
            let mut body = Vec::new();
            match candidate {
                Encoding::FrameOfReference => super::encode_reference(values, &mut body),
                _ => super::encode_delta(values, &mut body),
            }
            let mut cursor = Cursor::new(&body, "test");
            assert_eq!(
                decode(candidate, &mut cursor, values.len()).unwrap(),
                values,
                "{candidate:?}"
            );
        }
        (encoding, bytes.len(), decoded)
    }

    #[test]
    fn the_extremes_round_trip() {
        let cases: Vec<Vec<i64>> = vec![
            Vec::new(),
            vec![0],
            vec![i64::MIN],
            vec![i64::MAX],
            vec![i64::MIN, i64::MAX],
            vec![i64::MAX, i64::MIN],
            vec![i64::MIN, 0, i64::MAX, -1, 1],
            vec![-5; 100],
        ];
        for values in cases {
            let (_, _, decoded) = round_trip(&values);
            assert_eq!(decoded, values);
        }
    }

    /// The shape frame of reference exists for: a narrow band a long way from zero.
    #[test]
    fn a_narrow_band_costs_its_width_not_sixty_four_bits() {
        let values: Vec<i64> = (0..1000).map(|i| 1_000_000_000_000 + i % 900).collect();
        let (encoding, len, decoded) = round_trip(&values);
        assert_eq!(decoded, values);
        assert_eq!(encoding, Encoding::FrameOfReference);
        // Ten bits a row plus a header, against 8000 bytes stored plainly.
        assert!(len < 1400, "a 10-bit column cost {len} bytes");
    }

    /// The shape delta exists for: a climbing timestamp column with a huge absolute value.
    #[test]
    fn a_climbing_column_is_stored_as_deltas() {
        let mut now = 757_382_400_000_000i64;
        let values: Vec<i64> = (0..1000)
            .map(|i| {
                now += 1000 + i % 7;
                now
            })
            .collect();
        let (encoding, len, decoded) = round_trip(&values);
        assert_eq!(decoded, values);
        assert_eq!(encoding, Encoding::Delta);
        assert!(len < 600, "a delta column cost {len} bytes");
    }

    #[test]
    fn a_constant_column_is_nearly_free() {
        let (_, len, decoded) = round_trip(&vec![42; 100_000]);
        assert_eq!(decoded.len(), 100_000);
        assert!(len < 16, "a constant column cost {len} bytes");
    }

    #[test]
    fn an_encoding_that_is_not_an_integer_encoding_is_corruption() {
        let mut cursor = Cursor::new(&[], "test");
        assert!(
            decode(Encoding::Dictionary, &mut cursor, 0)
                .unwrap_err()
                .is_corruption()
        );
    }

    proptest! {
        /// Both encodings are exact for every input, overflowing pairs included.
        #[test]
        fn integers_round_trip(values in prop::collection::vec(any::<i64>(), 0..200)) {
            let (_, _, decoded) = round_trip(&values);
            prop_assert_eq!(decoded, values);
        }

        /// The same, over values drawn from a narrow band, which is where the encodings differ.
        #[test]
        fn banded_integers_round_trip(
            base in any::<i64>(),
            offsets in prop::collection::vec(0i64..1000, 0..200),
        ) {
            let values: Vec<i64> = offsets.iter().map(|o| base.wrapping_add(*o)).collect();
            let (_, _, decoded) = round_trip(&values);
            prop_assert_eq!(decoded, values);
        }

        /// Arbitrary bytes into either decoder: an error or the right count, never a panic.
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..80),
            count in 0usize..100,
        ) {
            for encoding in [Encoding::FrameOfReference, Encoding::Delta] {
                let mut cursor = Cursor::new(&bytes, "test");
                if let Ok(values) = decode(encoding, &mut cursor, count) {
                    prop_assert_eq!(values.len(), count);
                }
            }
        }
    }
}
