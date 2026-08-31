//! Bit-width packing: the primitive every other encoding is built on.
//!
//! A run of `u64`s known to fit in `width` bits each, laid end to end with no padding between
//! them, **least-significant bit first**. A column of a thousand values in the range 0..=6 costs
//! three bits each and 375 bytes, and that is what frame of reference, delta, dictionary codes
//! and string lengths all reduce to once their own trick has been applied.
//!
//! Two properties are worth stating because everything above depends on them:
//!
//! * **`width == 0` writes nothing.** All the values are zero, which after a frame of reference
//!   means they are all equal to the minimum. A constant column costs a minimum and a width byte.
//! * **The padding bits of the last byte are zero.** Nothing reads them, but a format with golden
//!   files cannot afford an encoder whose output depends on what was in the accumulator.
//!
//! This is the one place in the crate where a wrong shift produces plausible numbers rather than
//! a failure, so [`pack`] and [`unpack`] are written to be obviously right rather than fast, and
//! the proptest checks them against a bit-at-a-time reference as well as against each other.

use esker_base::varint;

use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::format::MAX_COLUMN_BYTES;

/// Bits needed to hold every value up to and including `max`. Zero when `max` is zero.
pub(crate) fn bit_width(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// The widest value `width` bits can hold.
pub(crate) fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Bytes `count` values of `width` bits occupy, or `None` if that many bits cannot be counted.
pub(crate) fn packed_len(count: usize, width: u32) -> Option<usize> {
    count
        .checked_mul(width as usize)
        .map(|bits| bits.div_ceil(8))
}

/// Appends `values` packed at `width` bits each. Bits above `width` are dropped.
pub(crate) fn pack(values: &[u64], width: u32, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let keep = mask(width);
    // `acc` holds `used` bits waiting to be written; `used` is always below 64 at the top of the
    // loop, which is what makes every shift below well defined.
    let mut acc: u64 = 0;
    let mut used: u32 = 0;
    for &value in values {
        let value = value & keep;
        acc |= value << used;
        let room = 64 - used;
        if width < room {
            used += width;
        } else {
            out.extend_from_slice(&acc.to_le_bytes());
            // The low `room` bits went into `acc`; the rest start the next word.
            acc = if room == 64 { 0 } else { value >> room };
            used = width - room;
        }
    }
    while used > 0 {
        out.push((acc & 0xff) as u8);
        acc >>= 8;
        used = used.saturating_sub(8);
    }
}

/// Reads `count` values of `width` bits from the front of `cursor`.
///
/// Consumes exactly [`packed_len`] bytes. A width above 64, or a count whose bits are not there,
/// is corruption — never a panic and never a short read presented as a full one.
pub(crate) fn unpack(
    cursor: &mut Cursor<'_>,
    count: usize,
    width: u32,
    field: &str,
) -> Result<Vec<u64>> {
    if width > 64 {
        return Err(Error::corruption(
            "bit packing",
            format!("{field} has a width of {width} bits"),
        ));
    }
    if width == 0 {
        return Ok(vec![0; count]);
    }
    let needed = packed_len(count, width).ok_or_else(|| {
        Error::corruption(
            "bit packing",
            format!("{field} of {count} values at {width} bits cannot be counted"),
        )
    })?;
    // A narrow width expands: a one-bit run of `n` values is `n/8` bytes on disk and `8n` in
    // memory, a factor of sixty-four. The cursor's rule — no count larger than the bytes behind
    // it — is satisfied by such a run and is not enough on its own, so the *output* is bounded
    // too, by the same limit a decoded column has.
    if count.saturating_mul(8) > MAX_COLUMN_BYTES {
        return Err(Error::corruption(
            "bit packing",
            format!("{field} of {count} values would decode to more than {MAX_COLUMN_BYTES} bytes"),
        ));
    }
    let bytes = cursor.bytes(needed, field)?;

    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * width as usize;
        let mut value: u64 = 0;
        let mut got: u32 = 0;
        while got < width {
            let bit = start + got as usize;
            // `bit % 8` is 0..=7, which every integer type holds.
            #[allow(clippy::cast_possible_truncation)]
            let in_byte = (bit % 8) as u32;
            let take = (8 - in_byte).min(width - got);
            let chunk = u64::from(bytes[bit / 8] >> in_byte) & mask(take);
            value |= chunk << got;
            got += take;
        }
        out.push(value);
    }
    Ok(out)
}

/// Appends `values` as a frame of reference: `min:varint ++ width:u8 ++ packed(v - min)`.
///
/// The shape every unsigned run in this format uses — string lengths, dictionary codes, and the
/// zigzagged deltas of an integer column. A run of equal values costs the minimum and a zero
/// width, which is what makes a constant column nearly free.
pub(crate) fn pack_for(values: &[u64], out: &mut Vec<u8>) {
    let min = values.iter().copied().min().unwrap_or(0);
    let width = bit_width(values.iter().copied().map(|v| v - min).max().unwrap_or(0));
    varint::put_u64(min, out);
    out.push(u8::try_from(width).unwrap_or(u8::MAX));
    let offsets: Vec<u64> = values.iter().map(|v| v - min).collect();
    pack(&offsets, width, out);
}

/// Reads `count` values written by [`pack_for`].
///
/// A minimum plus an offset can overflow only if the bytes are corrupt, so the addition wraps
/// rather than panicking; the values that come back are then nonsense, which is what the checks
/// above every caller are for.
pub(crate) fn unpack_for(cursor: &mut Cursor<'_>, count: usize, field: &str) -> Result<Vec<u64>> {
    let min = cursor.varint(field)?;
    let width = u32::from(cursor.u8(field)?);
    Ok(unpack(cursor, count, width, field)?
        .into_iter()
        .map(|offset| min.wrapping_add(offset))
        .collect())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{bit_width, mask, pack, pack_for, packed_len, unpack, unpack_for};
    use crate::cursor::Cursor;
    use crate::format::MAX_COLUMN_BYTES;

    /// One bit at a time, which is the definition the fast path has to match.
    fn reference_pack(values: &[u64], width: u32) -> Vec<u8> {
        let mut bits = Vec::new();
        for &value in values {
            for bit in 0..width {
                bits.push((value >> bit) & 1 == 1);
            }
        }
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (index, set) in bits.iter().enumerate() {
            if *set {
                out[index / 8] |= 1 << (index % 8);
            }
        }
        out
    }

    #[test]
    fn widths_and_masks() {
        assert_eq!(bit_width(0), 0);
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(2), 2);
        assert_eq!(bit_width(255), 8);
        assert_eq!(bit_width(256), 9);
        assert_eq!(bit_width(u64::MAX), 64);
        assert_eq!(mask(0), 0);
        assert_eq!(mask(1), 1);
        assert_eq!(mask(64), u64::MAX);
        assert_eq!(mask(65), u64::MAX);
        assert_eq!(packed_len(0, 7), Some(0));
        assert_eq!(packed_len(8, 1), Some(1));
        assert_eq!(packed_len(3, 3), Some(2));
        assert_eq!(packed_len(usize::MAX, 64), None);
    }

    /// A zero width writes nothing and reads back as zeros: the constant-column case.
    #[test]
    fn a_zero_width_costs_nothing() {
        let mut out = Vec::new();
        pack(&[0, 0, 0], 0, &mut out);
        assert!(out.is_empty());
        let mut cursor = Cursor::new(&out, "test");
        assert_eq!(unpack(&mut cursor, 3, 0, "v").unwrap(), vec![0, 0, 0]);
        assert_eq!(cursor.remaining(), 0);
    }

    #[test]
    fn the_boundary_widths_round_trip() {
        for width in [1u32, 7, 8, 9, 31, 32, 33, 63, 64] {
            let keep = mask(width);
            let values: Vec<u64> = (0..37u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9) & keep)
                .collect();
            let mut out = Vec::new();
            pack(&values, width, &mut out);
            assert_eq!(out.len(), packed_len(values.len(), width).unwrap());
            assert_eq!(out, reference_pack(&values, width), "width {width}");

            let mut cursor = Cursor::new(&out, "test");
            assert_eq!(
                unpack(&mut cursor, values.len(), width, "v").unwrap(),
                values
            );
            assert_eq!(cursor.remaining(), 0, "width {width} left bytes behind");
        }
    }

    /// A narrow width expands sixty-four to one, so the *output* is bounded as well as the
    /// input. Reachable through a dictionary's entry count, which is bounded only by the bytes
    /// behind it — 200 MB of one-byte entries would ask for 1.6 GB of `u64`s.
    #[test]
    fn a_run_that_would_expand_past_the_column_limit_is_refused() {
        let mut cursor = Cursor::new(&[], "test");
        let error = unpack(&mut cursor, MAX_COLUMN_BYTES / 8 + 1, 1, "v").unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(
            error.to_string().contains("would decode to more"),
            "{error}"
        );

        // The guard runs before any read, so it is not a disguised short-buffer error: one under
        // the limit reaches the cursor and fails there instead.
        let mut cursor = Cursor::new(&[], "test");
        let error = unpack(&mut cursor, MAX_COLUMN_BYTES / 8, 1, "v").unwrap_err();
        assert!(error.to_string().contains("remain"), "{error}");
    }

    #[test]
    fn a_short_or_impossible_run_is_corruption() {
        let mut out = Vec::new();
        pack(&[1, 2, 3], 8, &mut out);
        let mut cursor = Cursor::new(&out[..2], "test");
        assert!(unpack(&mut cursor, 3, 8, "v").unwrap_err().is_corruption());

        let mut cursor = Cursor::new(&out, "test");
        assert!(unpack(&mut cursor, 3, 65, "v").unwrap_err().is_corruption());
    }

    proptest! {
        /// Packing then unpacking is the identity, at every width, for every value that fits.
        #[test]
        fn pack_unpack_is_the_identity(
            width in 0u32..=64,
            raw in prop::collection::vec(any::<u64>(), 0..200),
        ) {
            let values: Vec<u64> = raw.iter().map(|v| v & mask(width)).collect();
            let mut out = Vec::new();
            pack(&values, width, &mut out);
            prop_assert_eq!(out.len(), packed_len(values.len(), width).unwrap());
            prop_assert_eq!(&out, &reference_pack(&values, width));

            let mut cursor = Cursor::new(&out, "test");
            let decoded = unpack(&mut cursor, values.len(), width, "v").unwrap();
            prop_assert_eq!(decoded, values);
            prop_assert_eq!(cursor.remaining(), 0);
        }

        /// Whatever the bytes are, unpacking them cannot panic and cannot over-read.
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..64),
            count in 0usize..64,
            width in 0u32..=70,
        ) {
            let mut cursor = Cursor::new(&bytes, "test");
            if let Ok(values) = unpack(&mut cursor, count, width, "v") {
                prop_assert_eq!(values.len(), count);
                prop_assert!(values.iter().all(|v| *v <= mask(width)));
            }
        }
    }

    #[test]
    fn a_frame_of_reference_round_trips_and_is_free_when_constant() {
        let cases: Vec<Vec<u64>> = vec![
            Vec::new(),
            vec![7],
            vec![9; 500],
            (0..500u64).collect(),
            vec![0, u64::MAX],
            vec![u64::MAX; 3],
        ];
        for values in cases {
            let mut out = Vec::new();
            pack_for(&values, &mut out);
            let mut cursor = Cursor::new(&out, "test");
            assert_eq!(unpack_for(&mut cursor, values.len(), "v").unwrap(), values);
            assert_eq!(cursor.remaining(), 0);
        }

        // A constant run is a minimum and a zero width, whatever its length.
        let mut out = Vec::new();
        pack_for(&vec![1234; 100_000], &mut out);
        assert!(out.len() <= 3, "a constant run cost {} bytes", out.len());
    }

    proptest! {
        #[test]
        fn frames_of_reference_round_trip(values in prop::collection::vec(any::<u64>(), 0..200)) {
            let mut out = Vec::new();
            pack_for(&values, &mut out);
            let mut cursor = Cursor::new(&out, "test");
            prop_assert_eq!(unpack_for(&mut cursor, values.len(), "v").unwrap(), values);
            prop_assert_eq!(cursor.remaining(), 0);
        }

        #[test]
        fn arbitrary_frame_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..64),
            count in 0usize..64,
        ) {
            let mut cursor = Cursor::new(&bytes, "test");
            if let Ok(values) = unpack_for(&mut cursor, count, "v") {
                prop_assert_eq!(values.len(), count);
            }
        }
    }
}
