//! Reals, stored as they are.
//!
//! Four little-endian bytes each, and [`double`](super::double)'s reasoning about why there is no
//! second encoding applies unchanged one width down.
//!
//! # Why a `real` is not a widened `double`
//!
//! Every `f32` is an `f64`, so a `real` column *could* ride in the doubles run the way an `int4`
//! rides in the integers one. It does not, for two reasons the integer case does not have:
//!
//! * **`f32` to `f64` and back is not a bit operation.** It is exact for every finite value and
//!   both infinities, and it is *unspecified* for a `NaN` payload — the Rust reference promises
//!   nothing, this machine happens to preserve even a signalling one, and an x86 `cvtss2sd` quiets
//!   it. Correctness that holds on one target and not another is not correctness, and the widening
//!   would have made a fragment's answer depend on where it was read.
//! * **It is four bytes, and a widened run writes eight.** Doubling a column for a type chosen
//!   because it is half the width is the cost of columnar storage paid backwards.
//!
//! So the bits survive exactly here, as they do for a `double` and as they already did on the row
//! side (`esker_keys::row` writes `to_le_bytes`) — which is what makes the two storage paths answer
//! the same for the same value, and what the differential harness compares.

use crate::cursor::Cursor;
use crate::encode::Encoding;
use crate::error::{Error, Result};

/// Encodes `values` plainly. There is only one encoding; the return shape matches its siblings.
pub(crate) fn encode(values: &[f32]) -> (Encoding, Vec<u8>) {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    (Encoding::Plain, out)
}

/// Reads `count` reals.
pub(crate) fn decode(
    encoding: Encoding,
    cursor: &mut Cursor<'_>,
    count: usize,
) -> Result<Vec<f32>> {
    if encoding != Encoding::Plain {
        return Err(Error::corruption(
            "real column",
            format!("{encoding:?} is not a real encoding"),
        ));
    }
    let needed = count.checked_mul(4).ok_or_else(|| {
        Error::corruption(
            "real column",
            format!("{count} reals cannot be counted in bytes"),
        )
    })?;
    let bytes = cursor.bytes(needed, "reals")?;
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| {
            let mut fixed = [0u8; 4];
            fixed.copy_from_slice(chunk);
            f32::from_le_bytes(fixed)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{decode, encode};
    use crate::cursor::Cursor;
    use crate::encode::Encoding;

    fn round_trip(values: &[f32]) -> Vec<f32> {
        let (encoding, bytes) = encode(values);
        assert_eq!(encoding, Encoding::Plain);
        let mut cursor = Cursor::new(&bytes, "test");
        let decoded = decode(encoding, &mut cursor, values.len()).unwrap();
        assert_eq!(cursor.remaining(), 0);
        decoded
    }

    /// Every awkward float there is, compared by bits: `NaN != NaN` would pass vacuously.
    #[test]
    fn the_awkward_reals_survive_bit_for_bit() {
        let values = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            f32::MIN,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::EPSILON,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
            // A signalling NaN, the one a widened run would have been allowed to quiet.
            f32::from_bits(0x7f80_0001),
            f32::from_bits(0xffc0_dead),
        ];
        let decoded = round_trip(&values);
        for (before, after) in values.iter().zip(&decoded) {
            assert_eq!(before.to_bits(), after.to_bits(), "{before} became {after}");
        }
        assert_eq!(round_trip(&[]), Vec::<f32>::new());
    }

    /// Four bytes each, not the eight a widened run would have written.
    #[test]
    fn a_real_costs_four_bytes() {
        let (_, bytes) = encode(&[1.0, 2.0, 3.0]);
        assert_eq!(bytes.len(), 12);
    }

    #[test]
    fn a_short_or_wrongly_tagged_run_is_corruption() {
        let (_, bytes) = encode(&[1.0, 2.0]);
        let mut cursor = Cursor::new(&bytes[..5], "test");
        assert!(
            decode(Encoding::Plain, &mut cursor, 2)
                .unwrap_err()
                .is_corruption()
        );

        let mut cursor = Cursor::new(&bytes, "test");
        assert!(
            decode(Encoding::Delta, &mut cursor, 2)
                .unwrap_err()
                .is_corruption()
        );
    }

    proptest! {
        #[test]
        fn reals_round_trip(bits in prop::collection::vec(any::<u32>(), 0..200)) {
            let values: Vec<f32> = bits.iter().map(|b| f32::from_bits(*b)).collect();
            let decoded = round_trip(&values);
            prop_assert_eq!(decoded.len(), values.len());
            for (before, after) in values.iter().zip(&decoded) {
                prop_assert_eq!(before.to_bits(), after.to_bits());
            }
        }

        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..80),
            count in 0usize..40,
        ) {
            let mut cursor = Cursor::new(&bytes, "test");
            if let Ok(values) = decode(Encoding::Plain, &mut cursor, count) {
                prop_assert_eq!(values.len(), count);
            }
        }
    }
}
