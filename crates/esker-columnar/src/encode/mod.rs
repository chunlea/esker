//! The per-type encodings, and the tag byte that says which one a chunk used.
//!
//! Every encoding here is a pair of pure functions over a slice of values and a `Vec<u8>` — no
//! file, no chunk framing, no compression. That is what lets each one carry its own round-trip
//! proptest next to it, which `CLAUDE.md` requires of a format and this crate has one of per
//! encoding.
//!
//! Which encoding a chunk gets is decided by *encoding it both ways and keeping the smaller*.
//! That sounds wasteful and is not: the candidates are a handful of passes over data already in
//! cache, and the alternative is a heuristic — "dictionary when cardinality is below a third" —
//! that is a magic number somebody tunes once against one workload and nobody revisits. The
//! decoder does not care how the choice was made; it reads the tag.

pub(crate) mod bitpack;
pub(crate) mod boolean;
pub(crate) mod bytes;
pub(crate) mod double;
pub(crate) mod float;
pub(crate) mod integer;

use esker_base::varint;

use crate::column::{Column, ColumnData, NullMask, count_nulls};
use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::format::MAX_STRIPE_ROWS;
use crate::value::ColumnType;

/// Encodes one column into a chunk payload, choosing the encoding and returning both.
///
/// The payload is what [`crate::frame::encode_chunk`] then compresses and checksums:
///
/// ```text
/// payload := encoding:u8 ++ rows:varint ++ null_count:varint
///              ++ [null mask, if there are any NULLs]
///              ++ values, for the rows that are not NULL
/// ```
///
/// The encoding tag, the row count and the null count are all **repeated** here from the footer
/// entry that names the chunk. That is deliberate duplication: a decoder must not resolve a
/// disagreement between two regions in favour of whichever it happened to read first, so the two
/// disagreeing is corruption and is reported as such. It also makes a chunk decodable on its own,
/// which is what lets the fuzz test feed one without inventing a footer.
pub fn encode_column(column: &Column) -> Result<(Encoding, Vec<u8>)> {
    let (encoding, body) = match column.data() {
        ColumnData::Ints(values) => integer::encode(values),
        ColumnData::Doubles(values) => double::encode(values),
        ColumnData::Floats(values) => float::encode(values),
        ColumnData::Bools(values) => {
            let mut out = Vec::new();
            let layout = boolean::encode_smaller(values, &mut out);
            (layout.encoding(), out)
        }
        ColumnData::Bytes { offsets, data } => {
            let values: Vec<&[u8]> = offsets
                .windows(2)
                .map(|pair| &data[pair[0] as usize..pair[1] as usize])
                .collect();
            bytes::encode(&values)?
        }
    };

    let nulls = column.nulls();
    let mut out = Vec::with_capacity(body.len() + 16);
    out.push(encoding.as_u8());
    varint::put_u64(nulls.rows() as u64, &mut out);
    varint::put_u64(nulls.nulls() as u64, &mut out);
    if nulls.nulls() > 0 {
        boolean::encode_mask(&nulls.to_bools(), &mut out);
    }
    out.extend_from_slice(&body);
    Ok((encoding, out))
}

/// Reads one column chunk's payload back.
///
/// `ty`, `rows` and `encoding` come from the footer, and every one of them is checked against
/// what the chunk itself says before a value is decoded.
///
/// One dispatch over every column type, which is why it is long: the list grows by a line each
/// time a type is added, and splitting it would put half the types in another function without
/// making either half easier to read.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per column type, and they belong together"
)]
pub fn decode_column(
    ty: ColumnType,
    rows: u64,
    encoding: Encoding,
    payload: &[u8],
) -> Result<Column> {
    let mut cursor = Cursor::new(payload, "column chunk");

    let tag = cursor.u8("chunk encoding")?;
    let stated = Encoding::from_u8(tag)
        .ok_or_else(|| Error::corruption("column chunk", format!("encoding tag {tag}")))?;
    if stated != encoding {
        return Err(Error::corruption(
            "column chunk",
            format!("the footer says {encoding:?} and the chunk says {stated:?}"),
        ));
    }

    let stated_rows = cursor.varint("chunk rows")?;
    if stated_rows != rows {
        return Err(Error::corruption(
            "column chunk",
            format!("the footer says {rows} rows and the chunk says {stated_rows}"),
        ));
    }
    let rows = usize::try_from(rows)
        .map_err(|_| Error::corruption("column chunk", format!("{rows} rows in one chunk")))?;
    if rows > MAX_STRIPE_ROWS {
        return Err(Error::corruption(
            "column chunk",
            format!("{rows} rows in one chunk, over the {MAX_STRIPE_ROWS} a stripe may hold"),
        ));
    }

    let null_count = cursor.varint("chunk null count")?;
    let null_count = usize::try_from(null_count).unwrap_or(usize::MAX);
    if null_count > rows {
        return Err(Error::corruption(
            "column chunk",
            format!("{null_count} nulls among {rows} rows"),
        ));
    }

    let nulls = if null_count == 0 {
        NullMask::none(rows)
    } else {
        let flags = boolean::decode_mask(&mut cursor, rows)?;
        // The mask and the count are two statements of the same fact, so they must agree.
        if count_nulls(&flags) != null_count {
            return Err(Error::corruption(
                "column chunk",
                format!(
                    "the null mask marks {} rows and the count says {null_count}",
                    count_nulls(&flags)
                ),
            ));
        }
        NullMask::from_bools(&flags)
    };

    let present = rows - null_count;
    let data = match ty {
        ColumnType::Int8
        | ColumnType::TimestampTz
        | ColumnType::Timestamp
        | ColumnType::Int4
        | ColumnType::Int2
        | ColumnType::Date
        | ColumnType::Time => ColumnData::Ints(integer::decode(encoding, &mut cursor, present)?),
        ColumnType::Double => ColumnData::Doubles(double::decode(encoding, &mut cursor, present)?),
        ColumnType::Real => ColumnData::Floats(float::decode(encoding, &mut cursor, present)?),
        ColumnType::Bool => {
            let layout = boolean::BoolLayout::from_encoding(encoding)?;
            ColumnData::Bools(boolean::decode_with(
                layout,
                &mut cursor,
                present,
                "values",
            )?)
        }
        ColumnType::Text
        | ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Numeric
        | ColumnType::Bytea => {
            let run = bytes::decode(encoding, &mut cursor, present)?;
            ColumnData::Bytes {
                offsets: run.offsets,
                data: run.data,
            }
        }
    };
    cursor.finish()?;

    let column = Column::new(ty, nulls, data)?;
    if matches!(
        ty,
        ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
    ) {
        // Text is UTF-8 by definition, and a `String` built from unchecked bytes is how a corrupt
        // file becomes a wrong answer somewhere far away. Pay for the check once, here.
        for value in &column {
            if let crate::value::ValueRef::Bytes(raw) = value {
                std::str::from_utf8(raw).map_err(|error| {
                    Error::corruption("text column", format!("value is not utf-8: {error}"))
                })?;
            }
        }
    }
    Ok(column)
}

/// Which encoding a column chunk's values are stored under.
///
/// The tag is on disk, so these numbers are frozen. A tag no version has written is corruption,
/// never a value to skip past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Values as they are: fixed width for numbers, bit-packed lengths plus bytes for strings.
    Plain,
    /// Frame of reference: one minimum, then bit-packed offsets from it.
    FrameOfReference,
    /// Zigzag deltas between neighbours, themselves frame-of-reference packed.
    Delta,
    /// A dictionary of distinct values, then bit-packed codes into it.
    Dictionary,
    /// One bit per boolean.
    Bitpacked,
    /// Alternating runs of equal booleans.
    Rle,
}

impl Encoding {
    /// The byte written into the footer's chunk entry. Frozen.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Encoding::Plain => 1,
            Encoding::FrameOfReference => 2,
            Encoding::Delta => 3,
            Encoding::Dictionary => 4,
            Encoding::Bitpacked => 5,
            Encoding::Rle => 6,
        }
    }

    /// The encoding a tag byte names, or `None` for one no version has written.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Encoding::Plain),
            2 => Some(Encoding::FrameOfReference),
            3 => Some(Encoding::Delta),
            4 => Some(Encoding::Dictionary),
            5 => Some(Encoding::Bitpacked),
            6 => Some(Encoding::Rle),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Encoding, decode_column, encode_column};
    use crate::column::Column;
    use crate::value::{ColumnType, Value};

    /// One value of each type, or NULL, so a generated column is always well typed.
    fn value_of(ty: ColumnType) -> impl Strategy<Value = Value> {
        let present = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Value::Int8).boxed(),
            ColumnType::Int4 => any::<i32>().prop_map(Value::Int4).boxed(),
            ColumnType::Int2 => any::<i16>().prop_map(Value::Int2).boxed(),
            ColumnType::Date => any::<i32>().prop_map(Value::Date).boxed(),
            // Both ends of the closed range, and midnight, which is where a run-length encoding
            // of a mostly-empty time column lands.
            ColumnType::Time => (0i64..=86_400_000_000).prop_map(Value::Time).boxed(),
            ColumnType::Real => any::<u32>()
                .prop_map(|bits| Value::Real(f32::from_bits(bits)))
                .boxed(),
            ColumnType::Timestamp => (-1_000i64..1_000)
                .prop_map(|d| Value::Timestamp(757_382_400_000_000 + d * 1_000))
                .boxed(),
            ColumnType::TimestampTz => (-1_000i64..1_000)
                .prop_map(|d| Value::TimestampTz(757_382_400_000_000 + d * 1_000))
                .boxed(),
            ColumnType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
            ColumnType::Double => any::<u64>()
                .prop_map(|bits| Value::Double(f64::from_bits(bits)))
                .boxed(),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb => (0usize..4)
                .prop_map(|pick| Value::Text(["", "a", "beta", "\u{1f600}"][pick].to_owned()))
                .boxed(),
            ColumnType::Bytea => prop::collection::vec(any::<u8>(), 0..6)
                .prop_map(Value::Bytea)
                .boxed(),
            // A `numeric` rides the column as its text: the three non-finite spellings, a value
            // no integer type could hold, and the two zeroes that differ only in scale.
            ColumnType::Numeric => (0usize..6)
                .prop_map(|pick| {
                    Value::Numeric(
                        [
                            "0",
                            "0.00",
                            "-1.5",
                            "12345678901234567890.5",
                            "NaN",
                            "-Infinity",
                        ][pick]
                            .to_owned(),
                    )
                })
                .boxed(),
        };
        prop_oneof![1 => Just(Value::Null), 6 => present]
    }

    fn column_of(ty: ColumnType) -> impl Strategy<Value = Column> {
        prop::collection::vec(value_of(ty), 0..120)
            .prop_map(move |values| Column::build(ty, &values).unwrap())
    }

    fn round_trip(column: &Column) -> Column {
        let (encoding, payload) = encode_column(column).unwrap();
        decode_column(column.ty(), column.rows() as u64, encoding, &payload).unwrap()
    }

    const ALL: [Encoding; 6] = [
        Encoding::Plain,
        Encoding::FrameOfReference,
        Encoding::Delta,
        Encoding::Dictionary,
        Encoding::Bitpacked,
        Encoding::Rle,
    ];

    /// The tags are in every footer this format has ever written.
    #[test]
    fn encoding_tags_are_frozen() {
        assert_eq!(Encoding::Plain.as_u8(), 1);
        assert_eq!(Encoding::FrameOfReference.as_u8(), 2);
        assert_eq!(Encoding::Delta.as_u8(), 3);
        assert_eq!(Encoding::Dictionary.as_u8(), 4);
        assert_eq!(Encoding::Bitpacked.as_u8(), 5);
        assert_eq!(Encoding::Rle.as_u8(), 6);

        for encoding in ALL {
            assert_eq!(Encoding::from_u8(encoding.as_u8()), Some(encoding));
        }
        assert_eq!(Encoding::from_u8(0), None);
        assert_eq!(Encoding::from_u8(7), None);
    }

    /// Every type, through the whole chunk payload, including the shapes that break decoders.
    #[test]
    fn every_type_round_trips_through_a_chunk() {
        let cases: Vec<(ColumnType, Vec<Value>)> = vec![
            (ColumnType::Int8, Vec::new()),
            (ColumnType::Int8, vec![Value::Null; 500]),
            (
                ColumnType::Int8,
                vec![Value::Int8(i64::MIN), Value::Null, Value::Int8(i64::MAX)],
            ),
            (
                ColumnType::TimestampTz,
                (0..500)
                    .map(|i| Value::TimestampTz(757_382_400_000_000 + i * 1_000))
                    .collect(),
            ),
            (
                ColumnType::Bool,
                (0..1000).map(|i| Value::Bool(i % 137 == 0)).collect(),
            ),
            (
                ColumnType::Double,
                vec![
                    Value::Double(f64::NAN),
                    Value::Double(-0.0),
                    Value::Null,
                    Value::Double(f64::INFINITY),
                ],
            ),
            (
                ColumnType::Text,
                (0..400)
                    .map(|i| match i % 5 {
                        0 => Value::Null,
                        1 => Value::Text(String::new()),
                        _ => Value::Text(format!("row-{}", i % 3)),
                    })
                    .collect(),
            ),
            (
                ColumnType::Bytea,
                vec![
                    Value::Bytea(vec![0xff, 0x00]),
                    Value::Bytea(Vec::new()),
                    Value::Null,
                ],
            ),
        ];
        for (ty, values) in cases {
            let column = Column::build(ty, &values).unwrap();
            let decoded = round_trip(&column);
            assert!(decoded.identical(&column), "{ty:?} did not round-trip");
            assert_eq!(decoded.rows(), values.len());
        }
    }

    /// A chunk that disagrees with the footer that named it is corruption, never resolved.
    #[test]
    fn a_chunk_and_its_footer_entry_must_agree() {
        let column = Column::build(
            ColumnType::Int8,
            &[Value::Int8(1), Value::Null, Value::Int8(3)],
        )
        .unwrap();
        let (encoding, payload) = encode_column(&column).unwrap();

        let error = decode_column(ColumnType::Int8, 4, encoding, &payload).unwrap_err();
        assert!(
            error.to_string().contains("4 rows and the chunk says 3"),
            "{error}"
        );

        let wrong = if encoding == Encoding::Delta {
            Encoding::FrameOfReference
        } else {
            Encoding::Delta
        };
        let error = decode_column(ColumnType::Int8, 3, wrong, &payload).unwrap_err();
        assert!(error.to_string().contains("the footer says"), "{error}");

        // A null count that does not match the mask it comes with.
        let mut forged = payload.clone();
        let count_at = 1 + esker_base::varint::encoded_len_u64(3);
        forged[count_at] = 2;
        let error = decode_column(ColumnType::Int8, 3, encoding, &forged).unwrap_err();
        assert!(error.is_corruption(), "{error}");
    }

    /// Text is validated where it is decoded, not where it is used.
    #[test]
    fn invalid_utf8_in_a_text_column_is_corruption() {
        let column = Column::build(ColumnType::Bytea, &[Value::Bytea(vec![0xff, 0xfe])]).unwrap();
        let (encoding, payload) = encode_column(&column).unwrap();
        // The two types share their encodings, so the same bytes are a text chunk's bytes.
        let error = decode_column(ColumnType::Text, 1, encoding, &payload).unwrap_err();
        assert!(error.to_string().contains("utf-8"), "{error}");
    }

    #[test]
    fn trailing_bytes_in_a_chunk_are_corruption() {
        let column = Column::build(ColumnType::Bool, &[Value::Bool(true)]).unwrap();
        let (encoding, mut payload) = encode_column(&column).unwrap();
        payload.push(0);
        assert!(decode_column(ColumnType::Bool, 1, encoding, &payload).is_err());
    }

    proptest! {
        /// The property this whole unit exists for: a column survives its own encoding, for
        /// every type, whatever is in it.
        #[test]
        fn columns_round_trip_int8(column in column_of(ColumnType::Int8)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        #[test]
        fn columns_round_trip_timestamp(column in column_of(ColumnType::TimestampTz)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        #[test]
        fn columns_round_trip_bool(column in column_of(ColumnType::Bool)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        #[test]
        fn columns_round_trip_double(column in column_of(ColumnType::Double)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        #[test]
        fn columns_round_trip_text(column in column_of(ColumnType::Text)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        #[test]
        fn columns_round_trip_bytea(column in column_of(ColumnType::Bytea)) {
            prop_assert!(round_trip(&column).identical(&column));
        }

        /// Arbitrary bytes into the chunk decoder, for every type and every encoding tag: an
        /// error or a consistent column, never a panic.
        #[test]
        fn arbitrary_chunk_bytes_never_panic(
            payload in prop::collection::vec(any::<u8>(), 0..120),
            rows in 0u64..80,
            tag in 1u8..=6,
        ) {
            let encoding = Encoding::from_u8(tag).unwrap();
            for ty in ColumnType::ALL {
                if let Ok(column) = decode_column(ty, rows, encoding, &payload) {
                    prop_assert_eq!(column.rows() as u64, rows);
                    prop_assert_eq!(column.ty(), ty);
                }
            }
        }
    }

    /// The allocation hazard a small fuzz corpus cannot reach: a chunk that claims two billion
    /// rows of one-bit values. The bits really are there, so the cursor's "no count larger than
    /// the bytes behind it" rule is satisfied — and the array it would decode into is sixty-four
    /// times the size of the chunk. Two separate caps refuse it.
    #[test]
    fn a_chunk_claiming_more_rows_than_a_stripe_may_hold_is_refused() {
        use crate::format::MAX_STRIPE_ROWS;

        let mut payload = vec![Encoding::Bitpacked.as_u8()];
        esker_base::varint::put_u64(MAX_STRIPE_ROWS as u64 + 1, &mut payload);
        esker_base::varint::put_u64(0, &mut payload);
        payload.resize(payload.len() + (MAX_STRIPE_ROWS + 1).div_ceil(8), 0);

        let error = decode_column(
            ColumnType::Bool,
            MAX_STRIPE_ROWS as u64 + 1,
            Encoding::Bitpacked,
            &payload,
        )
        .unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("a stripe may hold"), "{error}");

        // And one row under the cap is accepted for its shape, so the guard is a cap and not a
        // blanket refusal — it fails on the missing bytes instead.
        let mut payload = vec![Encoding::Bitpacked.as_u8()];
        esker_base::varint::put_u64(MAX_STRIPE_ROWS as u64, &mut payload);
        esker_base::varint::put_u64(0, &mut payload);
        let error = decode_column(
            ColumnType::Bool,
            MAX_STRIPE_ROWS as u64,
            Encoding::Bitpacked,
            &payload,
        )
        .unwrap_err();
        assert!(error.to_string().contains("remain"), "{error}");
    }
}
