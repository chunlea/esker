//! The columnar record: what a table wants of the storage layer, in bytes two layers can read.
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5 and
//! [ADR 0030](../../../docs/adr/0030-the-row-codec-moves-down.md). One record, written by
//! `esker-sql`, read by a store that holds a columnar learner — and neither the store nor the
//! placement driver links `esker-sql`, which is why the codec is here and not there.
//!
//! ```text
//! key   := 'm' ++ "sql" ++ 'l' ++ tenant:u64 ++ table_id:u64
//! value := version:u8 ++ replicas:u8 ++ schema_version:varint ++ ncols:varint
//!                     ++ (type:u8 ++ missing:value) * ncols
//! ```
//!
//! **The count is a fixed-offset prefix on purpose.** A reader that wants only the replica count
//! reads byte 1 and stops — [`replicas`] — so a schema a future build cannot parse never stops it
//! reading a number it can.
//!
//! **`missing` travels with the types and is not optional.** [`crate::row::decode_row`] pads a row
//! written before a column existed with that column's missing value; a decoder given only types
//! builds `RowSchema::nullable` and reads NULL where the row store reads the default — silently,
//! and only for rows older than the `ALTER`. [`Published::row_schema`] hands back a built schema
//! rather than the parts, so that mistake cannot be made.
//!
//! # The kind byte is `'l'`, not the `'c'` ADR 0022 sketched
//!
//! `'c'` was already the checkpoint's when this was built — phase 6d took it, after 0022 was
//! written. Two kinds sharing a byte is one scan returning the other's records, to a reader that
//! cannot tell them apart because it does not link the crate that wrote them. `'l'` is for the
//! **learner** Decision 1 calls a columnar replica.

use esker_base::varint;

use crate::row::{RowError, RowSchema, decode_row, encode_row};
use crate::value::{ColumnType, Datum};
use crate::{codec, prefix};

/// The `esker-sql` namespace inside the metadata key space.
///
/// Copied from `esker_sql::catalog::record` rather than linked, for the reason this module
/// exists at all, and pinned by a test there.
const SQL: &[u8] = b"sql";

/// The kind byte. See the module docs for why it is not `'c'`.
const KIND_COLUMNAR: u8 = b'l';

/// The catalog format version this record is written at.
///
/// It shares `esker-sql`'s numbering because it lives in `esker-sql`'s key space and is written
/// by `esker-sql`; a record with a different version byte from its neighbours would be a second
/// numbering to keep in step. Pinned against the original by a test in `esker-sql`.
pub const RECORD_VERSION: u8 = 3;

/// A column's type tag, the frozen vocabulary shared with `esker_sql::catalog::record` and
/// `esker_columnar::value`.
fn tag_of(ty: ColumnType) -> u8 {
    match ty {
        ColumnType::Int8 => 1,
        ColumnType::Text => 2,
        ColumnType::Bool => 3,
        ColumnType::Bytea => 4,
        ColumnType::TimestampTz => 5,
        ColumnType::Double => 6,
        // Appended, never renumbered: an old file has no tag above 6 and reads unchanged, and a
        // reader that meets one it does not know answers corruption rather than guessing
        // ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
        ColumnType::Int4 => 7,
        ColumnType::Varchar => 8,
        ColumnType::Timestamp => 9,
        ColumnType::Int2 => 10,
        ColumnType::Real => 11,
        ColumnType::Bpchar => 12,
        ColumnType::Json => 13,
        ColumnType::Jsonb => 14,
        ColumnType::Date => 15,
        ColumnType::Time => 17,
        ColumnType::Uuid => 18,
        ColumnType::Interval => 19,
        ColumnType::Oid => 20,
        ColumnType::Numeric => 16,
        ColumnType::Int8Array => 21,
        ColumnType::Int4Array => 22,
        ColumnType::Int2Array => 25,
        ColumnType::NumericArray => 23,
        ColumnType::TextArray => 24,
        // **27 and 28, past the highest in use rather than the next after the last line.** 25 is
        // `smallint[]`, appended out of numeric order, and taking "the next one" gave it a second
        // owner — clippy's `unreachable pattern` on the decode side is what caught it, and a
        // duplicate tag here decodes one column type as another. 26 is free and is left free:
        // a gap costs nothing and a guess about why it is there costs a reader.
        ColumnType::Hstore => 27,
        ColumnType::HstoreArray => 28,
        ColumnType::Citext => 29,
        ColumnType::TsRange => 30,
        ColumnType::TstzRange => 31,
        ColumnType::Int4Range => 32,
        ColumnType::TsRangeArray => 33,
        ColumnType::BoolArray => 34,
        ColumnType::ByteaArray => 35,
        ColumnType::BpcharArray => 36,
        ColumnType::VarcharArray => 37,
        ColumnType::DateArray => 38,
        ColumnType::TimeArray => 39,
        ColumnType::TimestampArray => 40,
        ColumnType::TimestampTzArray => 41,
        ColumnType::IntervalArray => 42,
        ColumnType::RealArray => 43,
        ColumnType::DoubleArray => 44,
        ColumnType::UuidArray => 45,
        ColumnType::JsonArray => 46,
        ColumnType::JsonbArray => 47,
        ColumnType::OidArray => 48,
        ColumnType::CitextArray => 49,
        ColumnType::DateRange => 50,
        ColumnType::NumRange => 51,
        ColumnType::Int8Range => 52,
        ColumnType::TstzRangeArray => 53,
        ColumnType::Int4RangeArray => 54,
        ColumnType::DateRangeArray => 55,
        ColumnType::NumRangeArray => 56,
        ColumnType::Int8RangeArray => 57,
        ColumnType::Point => 58,
        ColumnType::PointArray => 59,
        ColumnType::FloatRange => 60,
        ColumnType::VarcharRange => 61,
        ColumnType::Money => 62,
        ColumnType::MoneyArray => 63,
        ColumnType::Inet => 64,
        ColumnType::Cidr => 65,
        ColumnType::MacAddr => 66,
        ColumnType::InetArray => 67,
        ColumnType::CidrArray => 68,
        ColumnType::MacAddrArray => 69,
        ColumnType::Bit => 70,
        ColumnType::VarBit => 71,
        ColumnType::BitArray => 72,
        ColumnType::VarBitArray => 73,
        ColumnType::Lseg => 74,
        ColumnType::Box => 75,
        ColumnType::Path => 76,
        ColumnType::Polygon => 77,
        ColumnType::Circle => 78,
        ColumnType::Line => 79,
        ColumnType::Xml => 80,
        ColumnType::XmlArray => 81,
        ColumnType::Ltree => 82,
        ColumnType::LtreeArray => 83,
        ColumnType::LQuery => 84,
    }
}

fn type_of(tag: u8) -> Result<ColumnType, RowError> {
    Ok(match tag {
        1 => ColumnType::Int8,
        2 => ColumnType::Text,
        3 => ColumnType::Bool,
        4 => ColumnType::Bytea,
        5 => ColumnType::TimestampTz,
        6 => ColumnType::Double,
        7 => ColumnType::Int4,
        8 => ColumnType::Varchar,
        9 => ColumnType::Timestamp,
        10 => ColumnType::Int2,
        11 => ColumnType::Real,
        12 => ColumnType::Bpchar,
        13 => ColumnType::Json,
        14 => ColumnType::Jsonb,
        15 => ColumnType::Date,
        17 => ColumnType::Time,
        18 => ColumnType::Uuid,
        19 => ColumnType::Interval,
        20 => ColumnType::Oid,
        16 => ColumnType::Numeric,
        21 => ColumnType::Int8Array,
        22 => ColumnType::Int4Array,
        25 => ColumnType::Int2Array,
        23 => ColumnType::NumericArray,
        24 => ColumnType::TextArray,
        27 => ColumnType::Hstore,
        28 => ColumnType::HstoreArray,
        29 => ColumnType::Citext,
        30 => ColumnType::TsRange,
        31 => ColumnType::TstzRange,
        32 => ColumnType::Int4Range,
        33 => ColumnType::TsRangeArray,
        34 => ColumnType::BoolArray,
        35 => ColumnType::ByteaArray,
        36 => ColumnType::BpcharArray,
        37 => ColumnType::VarcharArray,
        38 => ColumnType::DateArray,
        39 => ColumnType::TimeArray,
        40 => ColumnType::TimestampArray,
        41 => ColumnType::TimestampTzArray,
        42 => ColumnType::IntervalArray,
        43 => ColumnType::RealArray,
        44 => ColumnType::DoubleArray,
        45 => ColumnType::UuidArray,
        46 => ColumnType::JsonArray,
        47 => ColumnType::JsonbArray,
        48 => ColumnType::OidArray,
        49 => ColumnType::CitextArray,
        50 => ColumnType::DateRange,
        51 => ColumnType::NumRange,
        52 => ColumnType::Int8Range,
        53 => ColumnType::TstzRangeArray,
        54 => ColumnType::Int4RangeArray,
        55 => ColumnType::DateRangeArray,
        56 => ColumnType::NumRangeArray,
        57 => ColumnType::Int8RangeArray,
        58 => ColumnType::Point,
        59 => ColumnType::PointArray,
        60 => ColumnType::FloatRange,
        61 => ColumnType::VarcharRange,
        62 => ColumnType::Money,
        63 => ColumnType::MoneyArray,
        64 => ColumnType::Inet,
        65 => ColumnType::Cidr,
        66 => ColumnType::MacAddr,
        67 => ColumnType::InetArray,
        68 => ColumnType::CidrArray,
        69 => ColumnType::MacAddrArray,
        70 => ColumnType::Bit,
        71 => ColumnType::VarBit,
        72 => ColumnType::BitArray,
        73 => ColumnType::VarBitArray,
        74 => ColumnType::Lseg,
        75 => ColumnType::Box,
        76 => ColumnType::Path,
        77 => ColumnType::Polygon,
        78 => ColumnType::Circle,
        79 => ColumnType::Line,
        80 => ColumnType::Xml,
        81 => ColumnType::XmlArray,
        82 => ColumnType::Ltree,
        83 => ColumnType::LtreeArray,
        84 => ColumnType::LQuery,
        other => {
            return Err(RowError::Corrupt(format!(
                "column type tag {other} is not one of ours"
            )));
        }
    })
}

/// `'m' ++ "sql" ++ 'l' ++ tenant ++ table_id`.
#[must_use]
pub fn key(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_COLUMNAR]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// The `[start, end)` range holding one tenant's columnar records, in table-id order.
///
/// One scan of it is the whole map, which is what a reader below `esker-sql` wants: every table's
/// wish at once rather than one question per table.
#[must_use]
pub fn range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_COLUMNAR]].concat();
    codec::encode_u64(tenant, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    end.push(0xFF);
    (start, end)
}

/// The table id out of a columnar key.
pub fn table_id(tenant: u64, bytes: &[u8]) -> Result<u64, RowError> {
    let prefix = key(tenant, 0);
    let at = prefix
        .len()
        .checked_sub(8)
        .ok_or_else(|| RowError::Corrupt("a columnar key prefix shorter than an id".into()))?;
    let tail = bytes
        .get(at..)
        .ok_or_else(|| RowError::Corrupt("a columnar key with no table id".into()))?;
    let (id, rest) =
        codec::decode_u64(tail).map_err(|error| RowError::Corrupt(error.to_string()))?;
    if !rest.is_empty() {
        return Err(RowError::Corrupt(
            "bytes after a columnar key's table id".into(),
        ));
    }
    Ok(id)
}

/// How a table's rows decode, published for a layer that cannot ask `esker-sql`.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    /// The `schema_version` this was published at. Monotonic, one more per `ADD COLUMN`.
    ///
    /// A reader holding two keeps the higher and never installs an older over a newer — the only
    /// comparison it needs, since a schema is published in the same transaction as the `ALTER`
    /// that made it true.
    pub schema_version: u64,
    /// A type and a missing value per column, in encoding order.
    pub columns: Vec<(ColumnType, Option<Datum>)>,
}

impl Published {
    /// The [`RowSchema`] to decode this table's rows with.
    ///
    /// **Use this rather than assembling one.** `RowSchema::nullable(types)` is the natural thing
    /// to reach for when all you seem to have is types, and it is wrong: it pads every absent
    /// column with NULL, where a row written before an `ADD COLUMN ... DEFAULT <constant>` must
    /// pad with that constant.
    #[must_use]
    pub fn row_schema(&self) -> RowSchema {
        let types = self.columns.iter().map(|(ty, _)| *ty).collect();
        let missing = self
            .columns
            .iter()
            .map(|(_, value)| value.clone())
            .collect();
        RowSchema::new(types, missing)
    }
}

/// Writes the record.
pub fn encode(replicas: u8, published: Option<&Published>) -> Result<Vec<u8>, RowError> {
    let mut out = vec![RECORD_VERSION, replicas];
    let Some(published) = published else {
        varint::put_u64(0, &mut out);
        varint::put_u64(0, &mut out);
        return Ok(out);
    };
    varint::put_u64(published.schema_version, &mut out);
    varint::put_u64(published.columns.len() as u64, &mut out);
    for (ty, missing) in &published.columns {
        out.push(tag_of(*ty));
        put_value(missing.as_ref(), *ty, &mut out)?;
    }
    Ok(out)
}

/// The replica count alone, which is all a placement decision needs.
///
/// Reads two bytes. A schema this build cannot parse must not stop a reader getting a number it
/// can, which is why this is not the first field of [`decode`].
pub fn replicas(bytes: &[u8]) -> Result<u8, RowError> {
    let (&version, rest) = bytes
        .split_first()
        .ok_or_else(|| RowError::Corrupt("an empty columnar record".into()))?;
    if version != RECORD_VERSION {
        return Err(RowError::Corrupt(format!(
            "columnar record version {version} is not {RECORD_VERSION}"
        )));
    }
    rest.first()
        .copied()
        .ok_or_else(|| RowError::Corrupt("a columnar record with no replica count".into()))
}

/// The whole record: the count and the published schema.
pub fn decode(bytes: &[u8]) -> Result<(u8, Published), RowError> {
    let replicas = replicas(bytes)?;
    let mut rest = bytes
        .get(2..)
        .ok_or_else(|| RowError::Corrupt("a columnar record with no schema".into()))?;
    let schema_version = take_varint(&mut rest, "schema version")?;
    let count = usize::try_from(take_varint(&mut rest, "column count")?)
        .map_err(|_| RowError::Corrupt("more columns than this machine can count".into()))?;
    let mut columns = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let (&tag, tail) = rest
            .split_first()
            .ok_or_else(|| RowError::Corrupt("a columnar record ends inside a column".into()))?;
        rest = tail;
        let ty = type_of(tag)?;
        columns.push((ty, take_value(&mut rest, ty)?));
    }
    if !rest.is_empty() {
        return Err(RowError::Corrupt(
            "bytes after a columnar record's last column".into(),
        ));
    }
    Ok((
        replicas,
        Published {
            schema_version,
            columns,
        },
    ))
}

fn take_varint(bytes: &mut &[u8], what: &str) -> Result<u64, RowError> {
    let (value, consumed) = varint::get_u64(bytes)
        .map_err(|error| RowError::Corrupt(format!("columnar record {what}: {error}")))?;
    *bytes = &bytes[consumed..];
    Ok(value)
}

/// A presence byte, then the value as a one-column row — the same shape
/// `esker_sql::catalog::record` writes a column default with.
fn put_value(value: Option<&Datum>, ty: ColumnType, out: &mut Vec<u8>) -> Result<(), RowError> {
    let Some(value) = value else {
        out.push(0);
        return Ok(());
    };
    out.push(1);
    let encoded = encode_row(&[ty], std::slice::from_ref(value))?;
    varint::put_u64(encoded.len() as u64, out);
    out.extend_from_slice(&encoded);
    Ok(())
}

fn take_value(bytes: &mut &[u8], ty: ColumnType) -> Result<Option<Datum>, RowError> {
    let (&present, tail) = bytes
        .split_first()
        .ok_or_else(|| RowError::Corrupt("a columnar record ends inside a value".into()))?;
    *bytes = tail;
    match present {
        0 => return Ok(None),
        1 => {}
        other => {
            return Err(RowError::Corrupt(format!(
                "presence byte {other} is neither 0 nor 1"
            )));
        }
    }
    let len = usize::try_from(take_varint(bytes, "value length")?)
        .map_err(|_| RowError::Corrupt("a value longer than this machine can address".into()))?;
    let (body, tail) = bytes
        .split_at_checked(len)
        .ok_or_else(|| RowError::Corrupt(format!("a value of {len} bytes is truncated")))?;
    *bytes = tail;
    let mut row = decode_row(&RowSchema::nullable(vec![ty]), body)?;
    row.pop()
        .ok_or_else(|| RowError::Corrupt("a columnar value decoded to no columns".into()))
        .map(Some)
}
