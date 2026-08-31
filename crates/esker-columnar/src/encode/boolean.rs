//! Booleans, and the null mask — which is the same problem twice.
//!
//! Two layouts, and the writer keeps whichever is smaller:
//!
//! ```text
//! bitpacked  ceil(n/8) bytes, least-significant bit first
//! rle        first:u8 ++ run_count:varint ++ run_len:varint *      runs alternate from `first`
//! ```
//!
//! Bit packing wins on noise and costs a fixed one bit per row. Run-length wins on the two shapes
//! that actually dominate: a column that is entirely `false`, and a null mask for a column that
//! has no NULLs at all or is almost all NULLs after an `ALTER TABLE ADD COLUMN`. Ten thousand
//! rows of one run cost four bytes instead of 1250.
//!
//! The runs alternate rather than each carrying its own value, so a run costs a length and
//! nothing else. That is the standard trick and it has one consequence a decoder must enforce:
//! **the lengths have to add up to exactly the row count**. A stream whose runs are short has
//! lost data, and one whose runs are long is describing rows that are not there; both are
//! corruption, and neither may be resolved by trusting the count from somewhere else.

use esker_base::varint;

use crate::cursor::Cursor;
use crate::encode::Encoding;
use crate::error::{Error, Result};

use super::bitpack;

/// Which of the two boolean layouts a run of bits is stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoolLayout {
    /// One bit per value.
    Bitpacked,
    /// Alternating runs of equal values.
    Rle,
}

impl BoolLayout {
    /// The byte that names this layout inside a null mask.
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            BoolLayout::Bitpacked => 0,
            BoolLayout::Rle => 1,
        }
    }

    /// The layout a mask's kind byte names.
    pub(crate) fn from_u8(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(BoolLayout::Bitpacked),
            1 => Ok(BoolLayout::Rle),
            other => Err(Error::corruption(
                "boolean layout",
                format!("layout byte {other}"),
            )),
        }
    }

    /// The chunk encoding tag a boolean *column* under this layout is recorded as.
    pub(crate) fn encoding(self) -> Encoding {
        match self {
            BoolLayout::Bitpacked => Encoding::Bitpacked,
            BoolLayout::Rle => Encoding::Rle,
        }
    }

    /// The layout a chunk encoding tag names, for a boolean column.
    pub(crate) fn from_encoding(encoding: Encoding) -> Result<Self> {
        match encoding {
            Encoding::Bitpacked => Ok(BoolLayout::Bitpacked),
            Encoding::Rle => Ok(BoolLayout::Rle),
            other => Err(Error::corruption(
                "boolean column",
                format!("{other:?} is not a boolean encoding"),
            )),
        }
    }
}

/// Appends `bits` under `layout`.
pub(crate) fn encode_with(layout: BoolLayout, bits: &[bool], out: &mut Vec<u8>) {
    match layout {
        BoolLayout::Bitpacked => {
            let words: Vec<u64> = bits.iter().map(|bit| u64::from(*bit)).collect();
            bitpack::pack(&words, 1, out);
        }
        BoolLayout::Rle => {
            let runs = runs_of(bits);
            out.push(u8::from(bits.first().copied().unwrap_or(false)));
            varint::put_u64(runs.len() as u64, out);
            for run in runs {
                varint::put_u64(run, out);
            }
        }
    }
}

/// Reads `count` bits stored under `layout`.
pub(crate) fn decode_with(
    layout: BoolLayout,
    cursor: &mut Cursor<'_>,
    count: usize,
    field: &str,
) -> Result<Vec<bool>> {
    match layout {
        BoolLayout::Bitpacked => Ok(bitpack::unpack(cursor, count, 1, field)?
            .into_iter()
            .map(|word| word == 1)
            .collect()),
        BoolLayout::Rle => {
            let mut value = match cursor.u8(field)? {
                0 => false,
                1 => true,
                other => {
                    return Err(Error::corruption(
                        "boolean rle",
                        format!("{field} starts with the value byte {other}"),
                    ));
                }
            };
            let run_count = cursor.count(field, 1)?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..run_count {
                let run = cursor.varint(field)?;
                let run = usize::try_from(run).unwrap_or(usize::MAX);
                if run > count - out.len() {
                    return Err(Error::corruption(
                        "boolean rle",
                        format!(
                            "{field} has a run of {run} with only {} rows left of {count}",
                            count - out.len()
                        ),
                    ));
                }
                out.resize(out.len() + run, value);
                value = !value;
            }
            if out.len() != count {
                return Err(Error::corruption(
                    "boolean rle",
                    format!("{field} covers {} rows, not the {count} claimed", out.len()),
                ));
            }
            Ok(out)
        }
    }
}

/// Encodes `bits` both ways and keeps the smaller, returning the layout it chose.
///
/// Ties go to bit packing: it is the cheaper decode, and a deterministic tie-break is what makes
/// the golden files reproducible.
pub(crate) fn encode_smaller(bits: &[bool], out: &mut Vec<u8>) -> BoolLayout {
    let mut packed = Vec::new();
    encode_with(BoolLayout::Bitpacked, bits, &mut packed);
    let mut rle = Vec::new();
    encode_with(BoolLayout::Rle, bits, &mut rle);

    if rle.len() < packed.len() {
        out.extend_from_slice(&rle);
        BoolLayout::Rle
    } else {
        out.extend_from_slice(&packed);
        BoolLayout::Bitpacked
    }
}

/// Appends a self-describing null mask: a layout byte, then the bits. `true` means NULL.
pub(crate) fn encode_mask(nulls: &[bool], out: &mut Vec<u8>) {
    let at = out.len();
    out.push(0);
    let layout = encode_smaller(nulls, out);
    out[at] = layout.as_u8();
}

/// Reads a null mask of `count` bits.
pub(crate) fn decode_mask(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<bool>> {
    let layout = BoolLayout::from_u8(cursor.u8("null mask layout")?)?;
    decode_with(layout, cursor, count, "null mask")
}

/// The lengths of the alternating runs in `bits`, starting with the first value.
fn runs_of(bits: &[bool]) -> Vec<u64> {
    let mut runs = Vec::new();
    let mut current = match bits.first() {
        Some(first) => *first,
        None => return runs,
    };
    let mut len = 0u64;
    for bit in bits {
        if *bit == current {
            len += 1;
        } else {
            runs.push(len);
            current = *bit;
            len = 1;
        }
    }
    runs.push(len);
    runs
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{BoolLayout, decode_mask, decode_with, encode_mask, encode_smaller, encode_with};
    use crate::cursor::Cursor;
    use crate::encode::Encoding;

    fn round_trip(layout: BoolLayout, bits: &[bool]) -> Vec<bool> {
        let mut out = Vec::new();
        encode_with(layout, bits, &mut out);
        let mut cursor = Cursor::new(&out, "test");
        let decoded = decode_with(layout, &mut cursor, bits.len(), "bits").unwrap();
        assert_eq!(cursor.remaining(), 0, "{layout:?} left bytes behind");
        decoded
    }

    #[test]
    fn both_layouts_round_trip_the_awkward_cases() {
        let cases: Vec<Vec<bool>> = vec![
            Vec::new(),
            vec![false],
            vec![true],
            vec![false; 1000],
            vec![true; 1000],
            (0..1000).map(|i| i % 2 == 0).collect(),
            (0..1000).map(|i| i % 97 == 0).collect(),
            (0..17).map(|i| i < 9).collect(),
        ];
        for bits in cases {
            for layout in [BoolLayout::Bitpacked, BoolLayout::Rle] {
                assert_eq!(round_trip(layout, &bits), bits, "{layout:?}");
            }
        }
    }

    /// The two shapes each layout exists for, and the writer choosing between them.
    #[test]
    fn the_smaller_layout_is_the_one_kept() {
        let mut out = Vec::new();
        assert_eq!(
            encode_smaller(&vec![false; 10_000], &mut out),
            BoolLayout::Rle
        );
        assert!(out.len() < 8, "one run should cost a handful of bytes");

        let noisy: Vec<bool> = (0..10_000).map(|i| i % 3 == 0).collect();
        let mut out = Vec::new();
        assert_eq!(encode_smaller(&noisy, &mut out), BoolLayout::Bitpacked);
        assert_eq!(out.len(), 1250);

        // A tie goes to bit packing, which keeps the encoder deterministic.
        let mut out = Vec::new();
        assert_eq!(encode_smaller(&[], &mut out), BoolLayout::Bitpacked);
        assert!(out.is_empty());
    }

    #[test]
    fn a_mask_says_which_layout_it_used() {
        for bits in [vec![false; 100], (0..100).map(|i| i % 2 == 0).collect()] {
            let mut out = Vec::new();
            encode_mask(&bits, &mut out);
            let mut cursor = Cursor::new(&out, "test");
            assert_eq!(decode_mask(&mut cursor, bits.len()).unwrap(), bits);
            assert_eq!(cursor.remaining(), 0);
        }
    }

    #[test]
    fn layout_bytes_and_tags_are_frozen() {
        assert_eq!(BoolLayout::Bitpacked.as_u8(), 0);
        assert_eq!(BoolLayout::Rle.as_u8(), 1);
        assert_eq!(BoolLayout::from_u8(0).unwrap(), BoolLayout::Bitpacked);
        assert_eq!(BoolLayout::from_u8(1).unwrap(), BoolLayout::Rle);
        assert!(BoolLayout::from_u8(2).unwrap_err().is_corruption());

        assert_eq!(BoolLayout::Bitpacked.encoding(), Encoding::Bitpacked);
        assert_eq!(BoolLayout::Rle.encoding(), Encoding::Rle);
        assert_eq!(
            BoolLayout::from_encoding(Encoding::Rle).unwrap(),
            BoolLayout::Rle
        );
        assert!(
            BoolLayout::from_encoding(Encoding::Delta)
                .unwrap_err()
                .is_corruption()
        );
    }

    /// Runs that do not add up are corruption, in both directions.
    #[test]
    fn runs_must_cover_exactly_the_rows_claimed() {
        let bits = vec![true; 10];
        let mut out = Vec::new();
        encode_with(BoolLayout::Rle, &bits, &mut out);

        // Too few rows described.
        let mut cursor = Cursor::new(&out, "test");
        let error = decode_with(BoolLayout::Rle, &mut cursor, 11, "bits").unwrap_err();
        assert!(error.to_string().contains("covers 10 rows"), "{error}");

        // Too many: the run overruns the count.
        let mut cursor = Cursor::new(&out, "test");
        let error = decode_with(BoolLayout::Rle, &mut cursor, 9, "bits").unwrap_err();
        assert!(error.to_string().contains("run of 10"), "{error}");

        // A value byte that is neither 0 nor 1.
        let mut forged = out.clone();
        forged[0] = 2;
        let mut cursor = Cursor::new(&forged, "test");
        assert!(
            decode_with(BoolLayout::Rle, &mut cursor, 10, "bits")
                .unwrap_err()
                .is_corruption()
        );
    }

    proptest! {
        /// Both layouts are exact, whatever the bits.
        #[test]
        fn booleans_round_trip(bits in prop::collection::vec(any::<bool>(), 0..300)) {
            for layout in [BoolLayout::Bitpacked, BoolLayout::Rle] {
                prop_assert_eq!(round_trip(layout, &bits), bits.clone());
            }
            let mut out = Vec::new();
            encode_mask(&bits, &mut out);
            let mut cursor = Cursor::new(&out, "test");
            prop_assert_eq!(decode_mask(&mut cursor, bits.len()).unwrap(), bits);
        }

        /// Runs biased towards long ones, which is what a null mask actually looks like.
        #[test]
        fn run_heavy_booleans_round_trip(runs in prop::collection::vec(1usize..50, 1..40)) {
            let mut bits = Vec::new();
            let mut value = false;
            for run in runs {
                bits.resize(bits.len() + run, value);
                value = !value;
            }
            let mut out = Vec::new();
            let layout = encode_smaller(&bits, &mut out);
            let mut cursor = Cursor::new(&out, "test");
            prop_assert_eq!(decode_with(layout, &mut cursor, bits.len(), "b").unwrap(), bits);
        }

        /// Arbitrary bytes into either decoder: an error or the right count, never a panic.
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..80),
            count in 0usize..200,
        ) {
            for layout in [BoolLayout::Bitpacked, BoolLayout::Rle] {
                let mut cursor = Cursor::new(&bytes, "test");
                if let Ok(bits) = decode_with(layout, &mut cursor, count, "b") {
                    prop_assert_eq!(bits.len(), count);
                }
            }
            let mut cursor = Cursor::new(&bytes, "test");
            if let Ok(bits) = decode_mask(&mut cursor, count) {
                prop_assert_eq!(bits.len(), count);
            }
        }
    }
}
