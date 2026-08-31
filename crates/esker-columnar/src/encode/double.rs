//! Doubles, stored as they are.
//!
//! Eight little-endian bytes each, and no second encoding — which is a decision rather than an
//! omission, and [ADR 0027](../../../../docs/adr/0027-columnar-file-format.md) records why. A
//! delta over floats is lossy or pointless. A dictionary needs an equality over `f64` that
//! answers for `NaN` and `-0.0`, which would be a second equality relation in a format whose
//! statistics already need one, and two of them eventually disagree. LZ4 over the top handles the
//! repetitive case, and adding an encoding later is a version bump this format is built to take.
//!
//! What this module does promise is that the bits survive exactly: a signalling `NaN` with a
//! payload, a negative zero, both infinities. `to_le_bytes` and `from_le_bytes` are bit
//! operations rather than numeric ones, which is the only reason that is true — and the proptest
//! compares `to_bits`, not values, because `NaN != NaN` would make a value comparison pass
//! vacuously.

use crate::cursor::Cursor;
use crate::encode::Encoding;
use crate::error::{Error, Result};

/// Encodes `values` plainly. There is only one encoding; the return shape matches its siblings.
pub(crate) fn encode(values: &[f64]) -> (Encoding, Vec<u8>) {
    let mut out = Vec::with_capacity(values.len() * 8);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    (Encoding::Plain, out)
}

/// Reads `count` doubles.
pub(crate) fn decode(
    encoding: Encoding,
    cursor: &mut Cursor<'_>,
    count: usize,
) -> Result<Vec<f64>> {
    if encoding != Encoding::Plain {
        return Err(Error::corruption(
            "double column",
            format!("{encoding:?} is not a double encoding"),
        ));
    }
    let needed = count.checked_mul(8).ok_or_else(|| {
        Error::corruption(
            "double column",
            format!("{count} doubles cannot be counted in bytes"),
        )
    })?;
    let bytes = cursor.bytes(needed, "doubles")?;
    Ok(bytes
        .chunks_exact(8)
        .map(|chunk| {
            let mut fixed = [0u8; 8];
            fixed.copy_from_slice(chunk);
            f64::from_le_bytes(fixed)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{decode, encode};
    use crate::cursor::Cursor;
    use crate::encode::Encoding;

    fn round_trip(values: &[f64]) -> Vec<f64> {
        let (encoding, bytes) = encode(values);
        assert_eq!(encoding, Encoding::Plain);
        let mut cursor = Cursor::new(&bytes, "test");
        let decoded = decode(encoding, &mut cursor, values.len()).unwrap();
        assert_eq!(cursor.remaining(), 0);
        decoded
    }

    /// Every awkward float there is, compared by bits: `NaN != NaN` would pass vacuously.
    #[test]
    fn the_awkward_floats_survive_bit_for_bit() {
        let values = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7ff0_0000_0000_0001),
            f64::from_bits(0xfff8_0000_dead_beef),
        ];
        let decoded = round_trip(&values);
        for (before, after) in values.iter().zip(&decoded) {
            assert_eq!(before.to_bits(), after.to_bits(), "{before} became {after}");
        }
        assert_eq!(round_trip(&[]), Vec::<f64>::new());
    }

    #[test]
    fn a_short_or_wrongly_tagged_run_is_corruption() {
        let (_, bytes) = encode(&[1.0, 2.0]);
        let mut cursor = Cursor::new(&bytes[..9], "test");
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
        fn doubles_round_trip(bits in prop::collection::vec(any::<u64>(), 0..200)) {
            let values: Vec<f64> = bits.iter().map(|b| f64::from_bits(*b)).collect();
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
