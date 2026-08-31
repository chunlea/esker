//! Text and byte strings: a dictionary, or lengths and bytes.
//!
//! ```text
//! plain       packed_for(lengths) ++ concatenated bytes
//! dictionary  dict_count:varint ++ packed_for(entry lengths) ++ entry bytes
//!                              ++ packed_for(codes)
//! ```
//!
//! Grouping the lengths away from the bytes is not cosmetic. Lengths are small and alike, so a
//! frame of reference packs them to a few bits each and a column of fixed-width strings costs a
//! *zero-bit* length array; and the bytes then form one run for LZ4 to work on rather than being
//! interleaved with numbers it cannot model.
//!
//! **The dictionary is sorted.** Nothing in this milestone needs that, and it is free at write
//! time — the distinct values are collected into an ordered set either way. It is recorded here
//! because it is a property a later reader may lean on: a sorted dictionary makes a range
//! predicate over a chunk a pair of binary searches, and finding out later that the order was
//! incidental would mean a format version to fix.
//!
//! The dictionary is tried only when the distinct values are at most half the rows. Above that
//! ratio the codes alone cost more than the values they replace, so it is a proof rather than a
//! threshold somebody tuned. Below it, both encodings are built and the smaller kept.
//!
//! # What a decoder must not believe
//!
//! A length prefix is the classic way a corrupt file becomes an allocation, and a dictionary adds
//! a second: a thousand codes into one long entry *expand*. So every length is checked against
//! [`MAX_VALUE_LEN`], every running total against [`MAX_COLUMN_BYTES`] and against the bytes
//! actually left, and every code against the dictionary's size — before anything is copied.

use std::collections::BTreeSet;

use esker_base::varint;

use crate::cursor::Cursor;
use crate::encode::{Encoding, bitpack};
use crate::error::{Error, Result};
use crate::format::{MAX_COLUMN_BYTES, MAX_VALUE_LEN};

/// One decoded run of byte strings: `offsets` has one more entry than there are values, and
/// value `i` is `data[offsets[i]..offsets[i + 1]]`.
#[derive(Debug)]
pub(crate) struct ByteRun {
    /// Where each value starts, plus a final entry holding the total length.
    pub(crate) offsets: Vec<u32>,
    /// Every value's bytes, end to end.
    pub(crate) data: Vec<u8>,
}

/// Encodes `values` under whichever of the two encodings is smaller.
///
/// Fails only if a single value is longer than [`MAX_VALUE_LEN`], which is a caller's error
/// rather than a decoder's: nothing can store it and pretending otherwise writes a file that
/// cannot be read back.
pub(crate) fn encode(values: &[&[u8]]) -> Result<(Encoding, Vec<u8>)> {
    if let Some(long) = values.iter().find(|value| value.len() > MAX_VALUE_LEN) {
        return Err(Error::InvalidArgument(format!(
            "a value of {} bytes, over the {MAX_VALUE_LEN} this format stores",
            long.len()
        )));
    }

    let mut plain = Vec::new();
    encode_plain(values, &mut plain);

    // Above half the rows the codes cost more than the values they stand in for, so there is
    // nothing to weigh: skip building the dictionary at all.
    let distinct: BTreeSet<&[u8]> = values.iter().copied().collect();
    if distinct.len() * 2 <= values.len() {
        let mut dictionary = Vec::new();
        encode_dictionary(values, &distinct, &mut dictionary);
        if dictionary.len() < plain.len() {
            return Ok((Encoding::Dictionary, dictionary));
        }
    }
    Ok((Encoding::Plain, plain))
}

/// Reads `count` byte strings stored under `encoding`.
pub(crate) fn decode(encoding: Encoding, cursor: &mut Cursor<'_>, count: usize) -> Result<ByteRun> {
    match encoding {
        Encoding::Plain => decode_plain(cursor, count),
        Encoding::Dictionary => decode_dictionary(cursor, count),
        other => Err(Error::corruption(
            "byte column",
            format!("{other:?} is not a byte-string encoding"),
        )),
    }
}

fn encode_plain(values: &[&[u8]], out: &mut Vec<u8>) {
    let lengths: Vec<u64> = values.iter().map(|value| value.len() as u64).collect();
    bitpack::pack_for(&lengths, out);
    for value in values {
        out.extend_from_slice(value);
    }
}

fn encode_dictionary(values: &[&[u8]], distinct: &BTreeSet<&[u8]>, out: &mut Vec<u8>) {
    let entries: Vec<&[u8]> = distinct.iter().copied().collect();
    varint::put_u64(entries.len() as u64, out);
    let lengths: Vec<u64> = entries.iter().map(|entry| entry.len() as u64).collect();
    bitpack::pack_for(&lengths, out);
    for entry in &entries {
        out.extend_from_slice(entry);
    }

    let codes: Vec<u64> = values
        .iter()
        .map(|value| {
            // The set the entries came from, so the search always succeeds.
            entries.binary_search(value).unwrap_or(0) as u64
        })
        .collect();
    bitpack::pack_for(&codes, out);
}

/// Sums lengths, refusing anything that could not be there, and returns the total.
fn total_of(lengths: &[u64], cursor: &Cursor<'_>, what: &str) -> Result<usize> {
    let mut total: usize = 0;
    for length in lengths {
        let length = usize::try_from(*length).unwrap_or(usize::MAX);
        if length > MAX_VALUE_LEN {
            return Err(Error::corruption(
                "byte column",
                format!("a {what} of {length} bytes, over the {MAX_VALUE_LEN} limit"),
            ));
        }
        total = total.saturating_add(length);
        if total > cursor.remaining() {
            return Err(Error::corruption(
                "byte column",
                format!(
                    "{what} lengths total at least {total} bytes with {} remaining",
                    cursor.remaining()
                ),
            ));
        }
    }
    Ok(total)
}

fn run_from(lengths: &[u64], data: &[u8]) -> ByteRun {
    let mut offsets = Vec::with_capacity(lengths.len() + 1);
    let mut at: u32 = 0;
    offsets.push(at);
    for length in lengths {
        // `total_of` has already bounded the sum by the bytes present — far below `u32::MAX` —
        // so the saturation below is unreachable, and `Column::new` re-checks the result anyway.
        at = at.saturating_add(u32::try_from(*length).unwrap_or(u32::MAX));
        offsets.push(at);
    }
    ByteRun {
        offsets,
        data: data.to_vec(),
    }
}

fn decode_plain(cursor: &mut Cursor<'_>, count: usize) -> Result<ByteRun> {
    let lengths = bitpack::unpack_for(cursor, count, "value lengths")?;
    let total = total_of(&lengths, cursor, "value")?;
    let data = cursor.bytes(total, "value bytes")?;
    Ok(run_from(&lengths, data))
}

fn decode_dictionary(cursor: &mut Cursor<'_>, count: usize) -> Result<ByteRun> {
    let entry_count = cursor.count("dictionary size", 1)?;
    let lengths = bitpack::unpack_for(cursor, entry_count, "dictionary lengths")?;
    let total = total_of(&lengths, cursor, "dictionary entry")?;
    let entries = cursor.bytes(total, "dictionary bytes")?;
    let dictionary = run_from(&lengths, entries);

    let codes = bitpack::unpack_for(cursor, count, "dictionary codes")?;
    let mut offsets = Vec::with_capacity(count + 1);
    let mut data = Vec::new();
    offsets.push(0u32);
    for code in codes {
        let code = usize::try_from(code).unwrap_or(usize::MAX);
        if code >= entry_count {
            return Err(Error::corruption(
                "byte column",
                format!("code {code} into a dictionary of {entry_count} entries"),
            ));
        }
        let start = dictionary.offsets[code] as usize;
        let end = dictionary.offsets[code + 1] as usize;
        // A dictionary expands: a thousand codes into one long entry is a thousand copies of it,
        // so the bound is on what comes out and is checked as it accumulates.
        if data.len().saturating_add(end - start) > MAX_COLUMN_BYTES {
            return Err(Error::corruption(
                "byte column",
                format!("a dictionary column expanding past {MAX_COLUMN_BYTES} bytes"),
            ));
        }
        data.extend_from_slice(&dictionary.data[start..end]);
        // The expansion check above caps `data` at MAX_COLUMN_BYTES, which is well below u32.
        offsets.push(u32::try_from(data.len()).unwrap_or(u32::MAX));
    }
    Ok(ByteRun { offsets, data })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{ByteRun, decode, encode};
    use crate::cursor::Cursor;
    use crate::encode::Encoding;
    use crate::format::MAX_VALUE_LEN;

    fn values_of(run: &ByteRun) -> Vec<Vec<u8>> {
        run.offsets
            .windows(2)
            .map(|pair| run.data[pair[0] as usize..pair[1] as usize].to_vec())
            .collect()
    }

    fn round_trip(values: &[&[u8]]) -> (Encoding, usize, Vec<Vec<u8>>) {
        let (encoding, bytes) = encode(values).unwrap();
        let mut cursor = Cursor::new(&bytes, "test");
        let run = decode(encoding, &mut cursor, values.len()).unwrap();
        assert_eq!(cursor.remaining(), 0, "{encoding:?} left bytes behind");
        assert_eq!(run.offsets.len(), values.len() + 1);
        (encoding, bytes.len(), values_of(&run))
    }

    #[test]
    fn the_awkward_shapes_round_trip() {
        let empty: &[u8] = b"";
        let cases: Vec<Vec<&[u8]>> = vec![
            Vec::new(),
            vec![empty],
            vec![empty, empty, empty],
            vec![b"a".as_slice(), b"", b"ccc"],
            vec![&[0xff, 0x00, 0xfe]],
            vec![b"same".as_slice(); 100],
        ];
        for values in cases {
            let (_, _, decoded) = round_trip(&values);
            let expected: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
            assert_eq!(decoded, expected);
        }
    }

    /// The shape the dictionary exists for, and the shape it must not be used for.
    #[test]
    fn low_cardinality_becomes_a_dictionary_and_high_cardinality_does_not() {
        let words: [&[u8]; 4] = [b"alpha", b"beta", b"gamma", b"delta"];
        let repeated: Vec<&[u8]> = (0..1000).map(|i| words[i % 4]).collect();
        let (encoding, len, decoded) = round_trip(&repeated);
        assert_eq!(encoding, Encoding::Dictionary);
        assert_eq!(decoded.len(), 1000);
        // Two bits a row plus the four words, against roughly 5.5 KiB stored plainly.
        assert!(len < 300, "a four-word column cost {len} bytes");

        let distinct: Vec<Vec<u8>> = (0..500)
            .map(|i| format!("row-{i:04}").into_bytes())
            .collect();
        let borrowed: Vec<&[u8]> = distinct.iter().map(Vec::as_slice).collect();
        let (encoding, _, decoded) = round_trip(&borrowed);
        assert_eq!(encoding, Encoding::Plain);
        assert_eq!(decoded, distinct);
    }

    /// Fixed-width values cost a zero-bit length array, which is the point of grouping lengths.
    #[test]
    fn fixed_width_values_pay_nothing_for_their_lengths() {
        let values: Vec<Vec<u8>> = (0..1000u32)
            .map(|i| format!("{i:08}").into_bytes())
            .collect();
        let borrowed: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let (encoding, len, decoded) = round_trip(&borrowed);
        assert_eq!(encoding, Encoding::Plain);
        assert_eq!(decoded, values);
        assert!(len < 8 * 1000 + 16, "lengths were not free: {len} bytes");
    }

    #[test]
    fn a_value_nothing_could_store_is_refused_at_the_writer() {
        let huge = vec![0u8; MAX_VALUE_LEN + 1];
        let error = encode(&[huge.as_slice()]).unwrap_err();
        assert!(error.to_string().contains("over the"), "{error}");
    }

    #[test]
    fn a_code_outside_the_dictionary_is_corruption() {
        let words: Vec<&[u8]> = (0..100)
            .map(|i| if i % 2 == 0 { b"a".as_slice() } else { b"b" })
            .collect();
        let (encoding, bytes) = encode(&words).unwrap();
        assert_eq!(encoding, Encoding::Dictionary);

        // The codes are one bit wide over two entries; widening the last byte forges a code of 1
        // where the dictionary still holds two entries, so instead shrink the dictionary claim.
        let mut forged = bytes.clone();
        forged[0] = 1; // dict_count 2 -> 1, leaving codes that name entry 1
        let mut cursor = Cursor::new(&forged, "test");
        let error = decode(Encoding::Dictionary, &mut cursor, words.len()).unwrap_err();
        assert!(error.is_corruption(), "{error}");
    }

    #[test]
    fn an_encoding_that_is_not_a_byte_encoding_is_corruption() {
        let mut cursor = Cursor::new(&[], "test");
        assert!(
            decode(Encoding::Delta, &mut cursor, 0)
                .unwrap_err()
                .is_corruption()
        );
    }

    proptest! {
        /// Both encodings are exact, whatever the values.
        #[test]
        fn byte_strings_round_trip(values in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..12), 0..60,
        )) {
            let borrowed: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
            let (_, _, decoded) = round_trip(&borrowed);
            prop_assert_eq!(decoded, values);
        }

        /// Drawn from a small alphabet, which is where the dictionary is chosen.
        #[test]
        fn low_cardinality_strings_round_trip(picks in prop::collection::vec(0usize..5, 0..80)) {
            let alphabet: [&[u8]; 5] = [b"", b"one", b"two", b"three", b"\xff\xfe"];
            let values: Vec<&[u8]> = picks.iter().map(|p| alphabet[*p]).collect();
            let (_, _, decoded) = round_trip(&values);
            let expected: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
            prop_assert_eq!(decoded, expected);
        }

        /// Arbitrary bytes into either decoder: an error or a consistent run, never a panic.
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..100),
            count in 0usize..60,
        ) {
            for encoding in [Encoding::Plain, Encoding::Dictionary] {
                let mut cursor = Cursor::new(&bytes, "test");
                if let Ok(run) = decode(encoding, &mut cursor, count) {
                    prop_assert_eq!(run.offsets.len(), count + 1);
                    prop_assert_eq!(*run.offsets.last().unwrap() as usize, run.data.len());
                }
            }
        }
    }
}
