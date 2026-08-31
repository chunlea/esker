//! Rows and keys: the two on-disk formats phase 6a adds to the `'t'` key space.
//!
//! A table row is one key/value pair. The key places the row in the ordered key space and is what
//! every scan, split and index lookup compares; the value carries the columns and is opaque to
//! everything below this crate (`CLAUDE.md` invariant 7). They are built by different rules and
//! for different reasons, and the difference is the whole design:
//!
//! * **The value is compact.** Little-endian fixed widths and varint-prefixed bytes, behind a
//!   version byte and a NULL bitmap. Nothing about it needs to sort.
//! * **The key is memcomparable.** Byte order *is* value order, because that is what makes a range
//!   scan a range scan and a unique index a key collision. `esker-keys` already provides the
//!   order-preserving encodings; what this module adds is the per-type mapping into them and the
//!   NULL marker an index column needs.
//!
//! # Layout (`docs/plans/phase-6a.md` §6, `docs/DESIGN.md` §3)
//!
//! ```text
//! row key    't' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ memcomparable(primary key columns)
//! index key  't' ++ tenant:u64 ++ table_id:u64 ++ 'i' ++ index_id:u64
//!                ++ (null_marker ++ memcomparable(column))*  [++ memcomparable(primary key)]
//! row value  version:u8=1 ++ null_bitmap:ceil(n/8) ++ non-NULL columns in column order
//! ```
//!
//! # Three decisions worth their own paragraph
//!
//! **The trailing primary key on an index entry is what makes it unique, so a unique index leaves
//! it off** — its absence is what turns a duplicate into a collision on one key, which is exactly
//! the write-write conflict Percolator already detects (`crate::backend`). But a NULL is not a
//! duplicate: PostgreSQL admits any number of NULLs in a `UNIQUE` column, confirmed against the
//! server, so a unique index entry whose key contains a NULL keeps the primary key suffix after
//! all. [`unique_index_key_is_unique_by_value`] is where that split lives.
//!
//! **A NULL sorts last**, which is PostgreSQL's default for an ascending index, and it is a
//! *marker byte* rather than an absence — `0x00` for a value and `0x01` for a NULL, ahead of each
//! index column. Without it, a NULL and an empty string would encode to the same bytes.
//!
//! **Text sorts by bytes, not by a collation.** The database this was captured against sorts
//! `a` before `B` under `en_US.utf8`; we sort `B` before `a`, which is what `COLLATE "C"` does in
//! PostgreSQL and what every byte-ordered key space does. This is a declared divergence, not an
//! accident: a locale-aware collation means ICU or the platform's C library, and this project
//! compiles no C (`CLAUDE.md`, dependency policy). Declaring it keeps it checkable —
//! `tests/corpus/pg19_order.txt` is sorted with `COLLATE "C"` on purpose, and a future collation
//! would change that file rather than quietly change answers.

use esker_base::varint;
use esker_keys::{codec, prefix};

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum, sort_bits_of_f64};

/// The version byte a row value starts with. An older node reading a newer row must fail rather
/// than misread it (`CLAUDE.md` invariant 2).
pub const ROW_FORMAT_VERSION: u8 = 1;

/// Ahead of every index column: the value is present.
const KEY_PRESENT: u8 = 0x00;

/// Ahead of every index column: the column is NULL. Above [`KEY_PRESENT`], so NULLs sort last.
const KEY_NULL: u8 = 0x01;

/// Encodes one row's columns as the value half of its key/value pair.
///
/// The bitmap comes before the values so that a projection can skip a NULL column without decoding
/// anything, and a row of all NULLs is a version byte and a run of set bits.
pub fn encode_row(types: &[ColumnType], values: &[Datum]) -> Result<Vec<u8>> {
    if types.len() != values.len() {
        return Err(SqlError::Internal(format!(
            "a row of {} values does not fit a table of {} columns",
            values.len(),
            types.len()
        )));
    }

    let bitmap_len = types.len().div_ceil(8);
    let mut out = Vec::with_capacity(1 + bitmap_len + 8 * types.len());
    out.push(ROW_FORMAT_VERSION);
    out.resize(1 + bitmap_len, 0);

    for (index, (ty, value)) in types.iter().zip(values).enumerate() {
        if matches!(value, Datum::Null) {
            out[1 + index / 8] |= 1 << (index % 8);
            continue;
        }
        if !value.fits(*ty) {
            return Err(SqlError::Internal(format!(
                "column {index} is {} and was given {value:?}",
                ty.name()
            )));
        }
        encode_column(value, &mut out);
    }
    Ok(out)
}

fn encode_column(value: &Datum, out: &mut Vec<u8>) {
    match value {
        // Nothing is written for a NULL; the bitmap is what records it.
        Datum::Null => {}
        Datum::Int8(v) | Datum::TimestampTz(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Bool(v) => out.push(u8::from(*v)),
        Datum::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Text(v) => {
            varint::put_u64(v.len() as u64, out);
            out.extend_from_slice(v.as_bytes());
        }
        Datum::Bytea(v) => {
            varint::put_u64(v.len() as u64, out);
            out.extend_from_slice(v);
        }
    }
}

/// Reads a row value back, given the columns it was written with.
///
/// The encoding carries no type tags, so the schema is what makes it readable — the same rule as
/// `esker_keys::codec::decode_tuple`. Every failure is a typed error: a truncated or corrupt row
/// is data, and data never panics this crate (`CLAUDE.md` invariant 9).
pub fn decode_row(types: &[ColumnType], bytes: &[u8]) -> Result<Vec<Datum>> {
    let (&version, rest) = bytes.split_first().ok_or_else(|| corrupt("row is empty"))?;
    if version != ROW_FORMAT_VERSION {
        return Err(corrupt(format!(
            "row format version {version} is not {ROW_FORMAT_VERSION}"
        )));
    }

    let bitmap_len = types.len().div_ceil(8);
    let (bitmap, mut rest) = rest
        .split_at_checked(bitmap_len)
        .ok_or_else(|| corrupt("row ends inside its NULL bitmap"))?;

    let mut values = Vec::with_capacity(types.len());
    for (index, ty) in types.iter().enumerate() {
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            values.push(Datum::Null);
            continue;
        }
        let (value, tail) = decode_column(*ty, rest)?;
        values.push(value);
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(corrupt(format!(
            "{} bytes after the last column",
            rest.len()
        )));
    }
    Ok(values)
}

fn decode_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let truncated = || corrupt(format!("a {} is truncated", ty.name()));
    Ok(match ty {
        ColumnType::Int8 | ColumnType::TimestampTz | ColumnType::Double => {
            let (head, rest) = bytes.split_first_chunk::<8>().ok_or_else(truncated)?;
            let value = match ty {
                ColumnType::Int8 => Datum::Int8(i64::from_le_bytes(*head)),
                ColumnType::TimestampTz => Datum::TimestampTz(i64::from_le_bytes(*head)),
                _ => Datum::Double(f64::from_le_bytes(*head)),
            };
            (value, rest)
        }
        ColumnType::Bool => {
            let (&byte, rest) = bytes.split_first().ok_or_else(truncated)?;
            // Any other byte is a value we never wrote; refusing it keeps a corrupt row from
            // turning into a plausible one.
            match byte {
                0 | 1 => (Datum::Bool(byte == 1), rest),
                other => return Err(corrupt(format!("boolean byte {other} is neither 0 nor 1"))),
            }
        }
        ColumnType::Text | ColumnType::Bytea => {
            let (len, consumed) = varint::get_u64(bytes)
                .map_err(|error| corrupt(format!("column length: {error}")))?;
            let len =
                usize::try_from(len).map_err(|_| corrupt("column longer than this machine"))?;
            let (body, rest) = bytes[consumed..]
                .split_at_checked(len)
                .ok_or_else(|| corrupt(format!("a column of {len} bytes is truncated")))?;
            let value = if ty == ColumnType::Text {
                Datum::Text(text_from_utf8(body)?)
            } else {
                Datum::Bytea(body.to_vec())
            };
            (value, rest)
        }
    })
}

/// The server encoding is UTF8, so bytes that are not are refused the way PostgreSQL refuses them
/// rather than replaced with U+FFFD, which would silently change a stored value.
fn text_from_utf8(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        let at = error.utf8_error().valid_up_to();
        SqlError::InvalidByteSequence(error.as_bytes().get(at).copied().unwrap_or(0))
    })
}

fn corrupt(what: impl Into<String>) -> SqlError {
    SqlError::DataCorrupted(what.into())
}

/// `'t' ++ tenant ++ table_id ++ 'r' ++ memcomparable(primary key)`.
///
/// Primary key columns are `NOT NULL` by definition, so there is no NULL marker here; a NULL
/// reaching this function is a check the executor skipped, not something a client can send.
pub fn row_key(tenant: u64, table_id: u64, primary_key: &[Datum]) -> Result<Vec<u8>> {
    let mut key = prefix::table_row_prefix(tenant, table_id);
    for value in primary_key {
        if matches!(value, Datum::Null) {
            return Err(SqlError::Internal(
                "a NULL reached a primary key; NOT NULL is checked before the key is built".into(),
            ));
        }
        encode_key_column(value, &mut key);
    }
    Ok(key)
}

/// `'t' ++ tenant ++ table_id ++ 'i' ++ index_id ++ columns [++ primary key]`.
///
/// `primary_key` is `Some` for a non-unique index — where the suffix is what keeps two rows with
/// the same indexed value apart — and for a unique index whose key contains a NULL, because
/// PostgreSQL admits any number of NULLs in a `UNIQUE` column and two of them must not collide.
/// [`unique_index_key_is_unique_by_value`] is the predicate that decides.
pub fn index_key(
    tenant: u64,
    table_id: u64,
    index_id: u64,
    columns: &[Datum],
    primary_key: Option<&[Datum]>,
) -> Result<Vec<u8>> {
    let mut key = prefix::table_index_prefix(tenant, table_id, index_id);
    for value in columns {
        if matches!(value, Datum::Null) {
            key.push(KEY_NULL);
            continue;
        }
        key.push(KEY_PRESENT);
        encode_key_column(value, &mut key);
    }
    if let Some(primary_key) = primary_key {
        for value in primary_key {
            if matches!(value, Datum::Null) {
                return Err(SqlError::Internal(
                    "a NULL reached a primary key suffix".into(),
                ));
            }
            encode_key_column(value, &mut key);
        }
    }
    Ok(key)
}

/// Whether a unique index entry over these columns is unique by its value alone — that is, whether
/// [`index_key`] may leave the primary key off, which is what turns a duplicate into the key
/// collision the transaction layer already detects.
///
/// False as soon as one column is NULL: PostgreSQL treats NULLs in a `UNIQUE` column as distinct
/// from each other, so those entries need the primary key suffix to keep them apart.
#[must_use]
pub fn unique_index_key_is_unique_by_value(columns: &[Datum]) -> bool {
    !columns.iter().any(|value| matches!(value, Datum::Null))
}

fn encode_key_column(value: &Datum, out: &mut Vec<u8>) {
    match value {
        Datum::Null => {}
        Datum::Int8(v) | Datum::TimestampTz(v) => codec::encode_i64(*v, out),
        // One byte, already in order: false is 0 and true is 1.
        Datum::Bool(v) => out.push(u8::from(*v)),
        // Sign-magnitude does not sort as an integer does, and PostgreSQL has fewer floats than
        // IEEE has; `sort_bits_of_f64` handles both.
        Datum::Double(v) => codec::encode_u64(sort_bits_of_f64(*v), out),
        Datum::Text(v) => codec::encode_bytes(v.as_bytes(), out),
        Datum::Bytea(v) => codec::encode_bytes(v, out),
    }
}

/// Reads index-key columns back, which is what a secondary index lookup does to recover the
/// primary key it is pointing at.
pub fn decode_key_columns(types: &[ColumnType], mut bytes: &[u8]) -> Result<(Vec<Datum>, usize)> {
    let total = bytes.len();
    let mut values = Vec::with_capacity(types.len());
    for ty in types {
        let (&marker, rest) = bytes
            .split_first()
            .ok_or_else(|| corrupt("index key ends before a column marker"))?;
        match marker {
            KEY_NULL => {
                values.push(Datum::Null);
                bytes = rest;
            }
            KEY_PRESENT => {
                let (value, rest) = decode_key_column(*ty, rest)?;
                values.push(value);
                bytes = rest;
            }
            other => return Err(corrupt(format!("index key marker byte {other}"))),
        }
    }
    Ok((values, total - bytes.len()))
}

fn decode_key_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let decoded = |error: codec::CodecError| corrupt(format!("index key column: {error}"));
    Ok(match ty {
        ColumnType::Int8 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Int8(value), rest)
        }
        ColumnType::TimestampTz => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::TimestampTz(value), rest)
        }
        ColumnType::Double => {
            let (bits, rest) = codec::decode_u64(bytes).map_err(decoded)?;
            (Datum::Double(crate::value::f64_of_sort_bits(bits)), rest)
        }
        ColumnType::Bool => {
            let (&byte, rest) = bytes
                .split_first()
                .ok_or_else(|| corrupt("index key ends inside a boolean"))?;
            match byte {
                0 | 1 => (Datum::Bool(byte == 1), rest),
                other => return Err(corrupt(format!("boolean byte {other} in an index key"))),
            }
        }
        ColumnType::Text => {
            let (body, rest) = codec::decode_bytes(bytes).map_err(decoded)?;
            (Datum::Text(text_from_utf8(&body)?), rest)
        }
        ColumnType::Bytea => {
            let (body, rest) = codec::decode_bytes(bytes).map_err(decoded)?;
            (Datum::Bytea(body), rest)
        }
    })
}

/// The half-open key range holding every row of one table, for a sequential scan.
///
/// The end is the row prefix with its last byte raised, which is the successor of every key under
/// it and belongs to nothing else — the index space of the same table starts at `'i'`, below `'r'`,
/// so this cannot run into it.
#[must_use]
pub fn table_row_range(tenant: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let start = prefix::table_row_prefix(tenant, table_id);
    (start.clone(), successor(start))
}

/// The half-open key range holding every entry of one index.
#[must_use]
pub fn index_range(tenant: u64, table_id: u64, index_id: u64) -> (Vec<u8>, Vec<u8>) {
    let start = prefix::table_index_prefix(tenant, table_id, index_id);
    (start.clone(), successor(start))
}

/// The first key after every key beginning with `prefix`.
///
/// Raising the last byte is only correct while it is not `0xFF`; every prefix this module builds
/// ends in `'r'`, `'i'` or an encoded id, so the general case is handled by dropping trailing
/// `0xFF` bytes rather than by assuming.
fn successor(mut prefix: Vec<u8>) -> Vec<u8> {
    while let Some(last) = prefix.last_mut() {
        if *last == u8::MAX {
            prefix.pop();
            continue;
        }
        *last += 1;
        return prefix;
    }
    // An all-0xFF prefix has no successor; an empty vector is the end of the key space.
    prefix
}

#[cfg(test)]
mod tests {
    use super::{
        ROW_FORMAT_VERSION, decode_key_columns, decode_row, encode_row, index_key, index_range,
        row_key, table_row_range, unique_index_key_is_unique_by_value,
    };
    use crate::sqlstate;
    use crate::value::{ColumnType, Datum, MAX_MICROS, MIN_MICROS, NEG_INFINITY, POS_INFINITY};

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    /// The golden. These bytes are the format, and changing any of them is a format change with
    /// everything that implies (`CLAUDE.md`, "ask before doing").
    #[test]
    fn a_row_value_is_a_version_a_bitmap_and_the_columns() {
        let types = [
            ColumnType::Int8,
            ColumnType::Text,
            ColumnType::Bool,
            ColumnType::Bytea,
            ColumnType::TimestampTz,
            ColumnType::Double,
        ];
        let values = [
            Datum::Int8(1),
            Datum::Text("hi".into()),
            Datum::Bool(true),
            Datum::Bytea(vec![0xde, 0xad]),
            Datum::TimestampTz(0),
            Datum::Double(1.0),
        ];
        assert_eq!(
            hex(&encode_row(&types, &values).unwrap()),
            concat!(
                "01",               // format version
                "00",               // NULL bitmap: six columns, none NULL
                "0100000000000000", // 1 as little-endian i64
                "026869",           // varint 2, "hi"
                "01",               // true
                "02dead",           // varint 2, the bytes
                "0000000000000000", // 2000-01-01, the epoch itself
                "000000000000f03f", // 1.0 as little-endian IEEE-754
            )
        );
        assert_eq!(
            decode_row(&types, &encode_row(&types, &values).unwrap()).unwrap(),
            values
        );
    }

    /// A NULL takes a bit and no bytes, which is what makes a wide sparse row cheap. The bitmap is
    /// little-endian within each byte: column 0 is bit 0.
    #[test]
    fn a_null_costs_one_bit_and_no_bytes() {
        let types = [ColumnType::Int8; 9];
        let mut values = vec![Datum::Null; 9];
        values[0] = Datum::Int8(7);
        values[8] = Datum::Int8(8);
        let encoded = encode_row(&types, &values).unwrap();
        assert_eq!(
            hex(&encoded),
            concat!(
                "01",               // version
                "fe",               // columns 1..=7 are NULL, column 0 is not
                "00",               // column 8 is not NULL
                "0700000000000000", // column 0
                "0800000000000000", // column 8
            )
        );
        assert_eq!(decode_row(&types, &encoded).unwrap(), values);
    }

    /// An empty string is not a NULL, and the encoding must not let them become each other.
    #[test]
    fn an_empty_value_and_a_null_are_different_rows() {
        let types = [ColumnType::Text];
        let empty = encode_row(&types, &[Datum::Text(String::new())]).unwrap();
        let null = encode_row(&types, &[Datum::Null]).unwrap();
        assert_ne!(empty, null);
        assert_eq!(
            decode_row(&types, &empty).unwrap(),
            [Datum::Text(String::new())]
        );
        assert_eq!(decode_row(&types, &null).unwrap(), [Datum::Null]);
    }

    /// Invariant 2: an unknown version is an error value, never a guess and never a panic.
    #[test]
    fn a_row_from_a_future_version_is_refused() {
        let mut row = encode_row(&[ColumnType::Int8], &[Datum::Int8(1)]).unwrap();
        row[0] = ROW_FORMAT_VERSION + 1;
        let error = decode_row(&[ColumnType::Int8], &row).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::DATA_CORRUPTED);
    }

    /// Every truncation of a row is an error and none of them is a panic (invariant 9).
    #[test]
    fn every_truncated_row_is_an_error() {
        let types = [ColumnType::Int8, ColumnType::Text, ColumnType::Bool];
        let row = encode_row(
            &types,
            &[
                Datum::Int8(-1),
                Datum::Text("abc".into()),
                Datum::Bool(false),
            ],
        )
        .unwrap();
        for cut in 0..row.len() {
            assert!(
                decode_row(&types, &row[..cut]).is_err(),
                "{cut} bytes decoded as a whole row"
            );
        }
        let mut trailing = row.clone();
        trailing.push(0);
        assert!(
            decode_row(&types, &trailing).is_err(),
            "trailing bytes are corruption too"
        );
    }

    /// The layout from `docs/DESIGN.md` §3, byte for byte: a row key is the table's prefix and
    /// then the primary key in an order-preserving encoding.
    #[test]
    fn a_row_key_is_the_table_prefix_and_the_primary_key() {
        let key = row_key(1, 2, &[Datum::Int8(5)]).unwrap();
        assert_eq!(
            hex(&key),
            concat!(
                "74",               // 't'
                "0000000000000001", // tenant
                "0000000000000002", // table id
                "72",               // 'r'
                "8000000000000005", // 5, with the sign bit flipped so negatives sort first
            )
        );
        let (start, end) = table_row_range(1, 2);
        assert!(
            key >= start && key < end,
            "a row key is inside its table's range"
        );
    }

    /// A unique index leaves the primary key off, which is what makes a duplicate a collision on
    /// one key -- the write-write conflict `crate::backend` already detects.
    #[test]
    fn a_unique_index_key_holds_no_primary_key_and_a_secondary_one_does() {
        let unique = index_key(1, 2, 3, &[Datum::Text("a".into())], None).unwrap();
        let secondary =
            index_key(1, 2, 3, &[Datum::Text("a".into())], Some(&[Datum::Int8(9)])).unwrap();
        assert!(
            secondary.starts_with(&unique),
            "the suffix is all that differs"
        );
        assert!(secondary.len() > unique.len());

        let other_row = index_key(
            1,
            2,
            3,
            &[Datum::Text("a".into())],
            Some(&[Datum::Int8(10)]),
        )
        .unwrap();
        assert_ne!(
            secondary, other_row,
            "two rows with one value need two entries"
        );

        let (start, end) = index_range(1, 2, 3);
        assert!(unique >= start && secondary < end);
    }

    /// PostgreSQL admits any number of NULLs in a `UNIQUE` column -- confirmed against the server.
    /// So a unique entry with a NULL in it keeps the primary key suffix, or the second NULL row
    /// would be reported as a duplicate of the first.
    #[test]
    fn nulls_in_a_unique_index_do_not_collide_with_each_other() {
        assert!(unique_index_key_is_unique_by_value(&[Datum::Text(
            "a".into()
        )]));
        assert!(!unique_index_key_is_unique_by_value(&[Datum::Null]));
        assert!(!unique_index_key_is_unique_by_value(&[
            Datum::Int8(1),
            Datum::Null
        ]));

        let left = index_key(1, 2, 3, &[Datum::Null], Some(&[Datum::Int8(1)])).unwrap();
        let right = index_key(1, 2, 3, &[Datum::Null], Some(&[Datum::Int8(2)])).unwrap();
        assert_ne!(left, right);
    }

    /// A NULL sorts after every value, which is PostgreSQL's default for an ascending index, and
    /// it is a marker byte rather than an absence -- without one, NULL and `''` would be equal.
    #[test]
    fn a_null_index_column_sorts_last_and_is_not_an_empty_string() {
        let null = index_key(1, 2, 3, &[Datum::Null], None).unwrap();
        let empty = index_key(1, 2, 3, &[Datum::Text(String::new())], None).unwrap();
        let value = index_key(1, 2, 3, &[Datum::Text("zzz".into())], None).unwrap();
        assert_ne!(null, empty);
        assert!(empty < value && value < null, "NULLs last");
    }

    /// An index entry is read to recover the primary key it points at, so the columns have to come
    /// back out -- including how many bytes they took, since the rest is the primary key.
    #[test]
    fn index_key_columns_decode_back_with_their_width() {
        let columns = [Datum::Text("ab".into()), Datum::Null, Datum::Int8(-3)];
        let types = [ColumnType::Text, ColumnType::Bool, ColumnType::Int8];
        let key = index_key(1, 2, 3, &columns, Some(&[Datum::Int8(77)])).unwrap();
        let head = index_range(1, 2, 3).0.len();
        let (decoded, consumed) = decode_key_columns(&types, &key[head..]).unwrap();
        assert_eq!(decoded, columns);

        let (suffix, _) = decode_key_columns(&[ColumnType::Int8], &{
            let mut marked = vec![super::KEY_PRESENT];
            marked.extend_from_slice(&key[head + consumed..]);
            marked
        })
        .unwrap();
        assert_eq!(suffix, [Datum::Int8(77)]);
    }

    /// The two ends of the timestamp range and both infinities survive a row, which is the point of
    /// storing PostgreSQL's own representation rather than a translated one.
    #[test]
    fn the_timestamp_sentinels_survive_a_row() {
        let types = [ColumnType::TimestampTz; 4];
        let values = [
            Datum::TimestampTz(NEG_INFINITY),
            Datum::TimestampTz(MIN_MICROS),
            Datum::TimestampTz(MAX_MICROS),
            Datum::TimestampTz(POS_INFINITY),
        ];
        let row = encode_row(&types, &values).unwrap();
        assert_eq!(decode_row(&types, &row).unwrap(), values);

        let keys: Vec<_> = values
            .iter()
            .map(|value| index_key(1, 2, 3, std::slice::from_ref(value), None).unwrap())
            .collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "-infinity .. infinity in order"
        );
    }

    /// A NULL cannot be part of a primary key, and reaching this function with one is a check the
    /// executor skipped rather than something a client can send.
    #[test]
    fn a_null_primary_key_is_refused_rather_than_encoded() {
        assert_eq!(
            row_key(1, 2, &[Datum::Null]).unwrap_err().sqlstate(),
            sqlstate::INTERNAL_ERROR
        );
    }

    /// Bytes that are not UTF-8 are refused with PostgreSQL's own condition rather than replaced,
    /// which would silently change a stored value.
    #[test]
    fn a_text_column_that_is_not_utf8_is_refused() {
        let mut row = encode_row(&[ColumnType::Text], &[Datum::Text("ab".into())]).unwrap();
        *row.last_mut().unwrap() = 0xff;
        let error = decode_row(&[ColumnType::Text], &row).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert_eq!(
            error.to_string(),
            "invalid byte sequence for encoding \"UTF8\": 0xff"
        );
    }

    /// Every value a column of `ty` can hold, NULL included.
    fn values_of(ty: ColumnType) -> proptest::strategy::BoxedStrategy<Datum> {
        use proptest::prelude::*;
        let values: BoxedStrategy<Datum> = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Datum::Int8).boxed(),
            ColumnType::Text => ".{0,32}".prop_map(Datum::Text).boxed(),
            ColumnType::Bool => any::<bool>().prop_map(Datum::Bool).boxed(),
            ColumnType::Bytea => proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(Datum::Bytea)
                .boxed(),
            ColumnType::TimestampTz => prop_oneof![
                9 => (MIN_MICROS..=MAX_MICROS).prop_map(Datum::TimestampTz),
                1 => proptest::sample::select(vec![NEG_INFINITY, POS_INFINITY])
                    .prop_map(Datum::TimestampTz),
            ]
            .boxed(),
            // Weighted towards the values with a special ordering: PostgreSQL has fewer floats
            // than IEEE does, and it is the ties that an encoding gets wrong.
            ColumnType::Double => prop_oneof![
                7 => any::<f64>().prop_map(Datum::Double),
                3 => proptest::sample::select(vec![
                    0.0, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY,
                ])
                .prop_map(Datum::Double),
            ]
            .boxed(),
        };
        prop_oneof![9 => values, 1 => Just(Datum::Null)].boxed()
    }

    /// A schema, and as many rows of it as asked for.
    fn schema_and_rows(
        columns: std::ops::Range<usize>,
        rows: usize,
    ) -> impl proptest::strategy::Strategy<Value = (Vec<ColumnType>, Vec<Vec<Datum>>)> {
        use proptest::prelude::*;
        proptest::collection::vec(
            proptest::sample::select(ColumnType::ALL.as_slice()),
            columns,
        )
        .prop_flat_map(move |types| {
            let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
            (Just(types), proptest::collection::vec(row, rows..=rows))
        })
    }

    /// Two values PostgreSQL considers equal. Enumerated rather than filtered, because the
    /// interesting pairs are rare enough that a filter rejects almost everything.
    fn equal_pair() -> impl proptest::strategy::Strategy<Value = (Datum, Datum)> {
        use proptest::prelude::*;
        prop_oneof![
            proptest::sample::select(vec![
                (Datum::Double(0.0), Datum::Double(-0.0)),
                (Datum::Double(f64::NAN), Datum::Double(-f64::NAN)),
                (
                    Datum::Double(f64::NAN),
                    // A quiet NaN carrying a payload: still one value to PostgreSQL.
                    Datum::Double(f64::from_bits(0x7ff8_0000_dead_beef)),
                ),
                (Datum::Null, Datum::Null),
            ]),
            schema_and_rows(1..2, 1).prop_map(|(_, rows)| (rows[0][0].clone(), rows[0][0].clone())),
        ]
    }

    proptest::proptest! {
        /// Round trip: whatever goes into a row comes back out of it, for any mixture of types
        /// and any placement of NULLs.
        #[test]
        fn any_row_survives_encode_and_decode((types, rows) in schema_and_rows(0..12, 1)) {
            let values = &rows[0];
            let encoded = encode_row(&types, values).unwrap();
            proptest::prop_assert_eq!(&decode_row(&types, &encoded).unwrap(), values);
        }

        /// Byte order is value order. This is the property the whole key encoding exists for: get
        /// it wrong and a range scan silently returns the wrong rows.
        #[test]
        fn key_order_is_value_order((_types, rows) in schema_and_rows(1..2, 2)) {
            let (left, right) = (&rows[0][0], &rows[1][0]);
            let key = |value: &Datum| index_key(1, 2, 3, std::slice::from_ref(value), None).unwrap();
            proptest::prop_assert_eq!(
                key(left).cmp(&key(right)),
                left.pg_cmp(right),
                "{:?} vs {:?}", left, right
            );
        }

        /// A composite key compares field by field, which needs every field encoding to be
        /// fixed-width or prefix-free -- the property `esker_keys::codec` provides and this one
        /// checks is still true once a NULL marker is in front of each field.
        #[test]
        fn a_composite_key_compares_field_by_field((_types, rows) in schema_and_rows(1..4, 2)) {
            let (left, right) = (&rows[0], &rows[1]);
            let expected = left
                .iter()
                .zip(right)
                .map(|(a, b)| a.pg_cmp(b))
                .find(|ordering| !ordering.is_eq())
                .unwrap_or(std::cmp::Ordering::Equal);
            proptest::prop_assert_eq!(
                index_key(1, 2, 3, left, None).unwrap()
                    .cmp(&index_key(1, 2, 3, right, None).unwrap()),
                expected
            );
        }

        /// Values PostgreSQL calls equal must encode identically, or a unique index would admit
        /// two entries for one value. Every pair that is equal without being identical is a float:
        /// the two zeros, and any two NaNs.
        #[test]
        fn values_that_compare_equal_encode_identically((left, right) in equal_pair()) {
            proptest::prop_assert!(left.pg_cmp(&right).is_eq(), "the strategy is wrong");
            proptest::prop_assert_eq!(
                index_key(1, 2, 3, std::slice::from_ref(&left), None).unwrap(),
                index_key(1, 2, 3, std::slice::from_ref(&right), None).unwrap(),
                "{:?} and {:?} are one value to PostgreSQL", left, right
            );
        }

        /// Invariant 9 for an on-disk format: arbitrary bytes are an error or a value, never a
        /// panic.
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            bytes in proptest::collection::vec(proptest::arbitrary::any::<u8>(), 0..64)
        ) {
            let types = [ColumnType::Int8, ColumnType::Text, ColumnType::Double];
            let _ = decode_row(&types, &bytes);
            let _ = decode_key_columns(&types, &bytes);
        }
    }
}
