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
//! row value  version:u8=2 ++ columns:varint ++ null_bitmap:ceil(columns/8)
//!                ++ non-NULL columns in column order
//! ```
//!
//! # Four decisions worth their own paragraph
//!
//! **The row says how many columns it holds, and that is what makes `ALTER TABLE ADD COLUMN`
//! free.** A row written before the column existed is not rewritten; it is read back with the
//! trailing columns padded to NULL, which is what PostgreSQL shows for them anyway. The count has
//! to be *in the row* for that to be sound: without it the bitmap's length is taken from the
//! reader's column list, so a two-column row read as three would take the third bitmap bit
//! (unset, meaning "not NULL") and run off the end of the value — and once the column count
//! crosses a multiple of eight the bitmap grows a byte and the reader would take the first
//! column's bytes as bitmap, which is a *wrong answer* rather than an error. A count in the row
//! is one varint and it closes both. Reading a row that claims *more* columns than the reader
//! has is corruption, not padding: the catalog is read at the same snapshot as the row, so a row
//! from a newer schema cannot be visible to a transaction that cannot see the schema.
//! `ADR 0019` records the change and what it does not solve — a `DROP COLUMN` needs a row that
//! carries column *identity*, not a count.
//!
//! # Three more decisions worth their own paragraph
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

use crate::numeric::{Decimal, Numeric};
use crate::value::{ColumnType, Datum, sort_bits_of_f64};
use crate::{codec, prefix};

/// The version byte a row value starts with. An older node reading a newer row must fail rather
/// than misread it (`CLAUDE.md` invariant 2).
///
/// Version 2 added the column count (ADR 0019). Version 1 is not read: it was never written
/// anywhere but a test, and a compatibility path for data that does not exist is one nothing can
/// check.
pub const ROW_FORMAT_VERSION: u8 = 2;

/// Ahead of every index column: the value is present.
const KEY_PRESENT: u8 = 0x00;

/// Ahead of every index column: the column is NULL. Above [`KEY_PRESENT`], so NULLs sort last.
const KEY_NULL: u8 = 0x01;

/// Encodes one row's columns as the value half of its key/value pair.
///
/// The count comes first so that a reader with *more* columns than the writer had — every reader
/// of a row written before an `ALTER TABLE ADD COLUMN` — knows where the bitmap ends. The bitmap
/// comes before the values so that a projection can skip a NULL column without decoding anything,
/// and a row of all NULLs is a header and a run of set bits.
pub fn encode_row(types: &[ColumnType], values: &[Datum]) -> Result<Vec<u8>> {
    if types.len() != values.len() {
        return Err(RowError::Mismatch(format!(
            "a row of {} values does not fit a table of {} columns",
            values.len(),
            types.len()
        )));
    }

    let bitmap_len = types.len().div_ceil(8);
    let mut out = Vec::with_capacity(2 + bitmap_len + 8 * types.len());
    out.push(ROW_FORMAT_VERSION);
    varint::put_u64(types.len() as u64, &mut out);
    let bitmap_at = out.len();
    out.resize(bitmap_at + bitmap_len, 0);

    for (index, (ty, value)) in types.iter().zip(values).enumerate() {
        if matches!(value, Datum::Null) {
            out[bitmap_at + index / 8] |= 1 << (index % 8);
            continue;
        }
        if !value.fits(*ty) {
            return Err(RowError::Mismatch(format!(
                "column {index} is {ty:?} and was given {value:?}"
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
        // **Sixteen bytes and no length header**, which is exactly what `pg_type.typlen` reports
        // for a `point`: the two coordinates, `x` first.
        Datum::Point { x, y } => {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        // **A money is its cents**, so it is written as the `i64` it is — eight bytes and no
        // scale, because every `money` has the same one.
        Datum::Int8(v)
        | Datum::TimestampTz(v)
        | Datum::Timestamp(v)
        | Datum::Time(v)
        | Datum::Money(v) => {
            out.extend_from_slice(&v.to_le_bytes());
        }
        // Four bytes, not eight. Nothing written before `int4` existed has a column of this type,
        // so the narrower width costs no compatibility and is what `pg_type.typlen` says it is.
        Datum::Int4(v) | Datum::Date(v) => out.extend_from_slice(&v.to_le_bytes()),
        // Four bytes, unsigned — its own width, like the `int4` it is not.
        Datum::Oid(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Int2(v) => out.extend_from_slice(&v.to_le_bytes()),
        // Sixteen bytes, fixed, so no length precedes them.
        Datum::Uuid(v) => out.extend_from_slice(v),
        // **Nineteen fixed bytes**: the family, the prefix length, the `cidr` flag and the
        // address. The flag is in the row and *not* in the key, which is the whole difference
        // between the two — a row remembers which type the value is and a comparison must not.
        Datum::Inet {
            family,
            bits,
            cidr,
            addr,
        } => {
            out.push(*family);
            out.push(*bits);
            out.push(u8::from(*cidr));
            out.extend_from_slice(addr);
        }
        // Six, which is what `pg_type.typlen` says a `macaddr` is.
        Datum::MacAddr(v) => out.extend_from_slice(v),
        // Sixteen bytes, the three fields in their own widths and in declaration order.
        Datum::Interval {
            months,
            days,
            micros,
        } => {
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&micros.to_le_bytes());
        }
        Datum::Bool(v) => out.push(u8::from(*v)),
        Datum::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Real(v) => out.extend_from_slice(&v.to_le_bytes()),
        // **A citext is stored as it was written** — the folding is the comparison's, not the
        // value's, so the row keeps the user's capitals and only the key below is folded.
        Datum::Text(v) | Datum::Citext(v) | Datum::Hstore(v) | Datum::Range { text: v, .. } => {
            varint::put_u64(v.len() as u64, out);
            out.extend_from_slice(v.as_bytes());
        }
        // A kind byte, then — for a finite value only — the scale and the digits. **The digits
        // are stored as written**, trailing zeros included: the scale is part of a `numeric` and
        // `1.00` is not `1.0`. The key encoding below is the one that normalises, because there
        // two spellings of one number have to become one key.
        Datum::Numeric(v) => {
            put_numeric(v, out);
        }
        Datum::Bytea(v) => {
            varint::put_u64(v.len() as u64, out);
            out.extend_from_slice(v);
        }
        // **The shape is written, not derived.** An empty array has no dimensions and a
        // two-dimensional one is a flat element list with a shape beside it, so neither can be
        // reconstructed from the elements alone — and the lower bound is part of the value:
        // `'[0:2]={1,2,3}'` has to print back as `[0:2]={1,2,3}`.
        Datum::Array(v) => {
            varint::put_u64(varint::zigzag_encode(i64::from(v.lower)), out);
            varint::put_u64(v.dims.len() as u64, out);
            for dim in &v.dims {
                varint::put_u64(varint::zigzag_encode(i64::from(*dim)), out);
            }
            varint::put_u64(v.values.len() as u64, out);
            for element in &v.values {
                // A byte per element, because a NULL element is not the array being NULL and the
                // row's own NULL bitmap has nothing to say about it.
                match element {
                    None => out.push(0),
                    Some(value) => {
                        out.push(1);
                        encode_column(value, out);
                    }
                }
            }
        }
    }
}

/// Reads a row value back, given the columns the table has **now**.
///
/// The encoding carries no type tags, so the schema is what makes it readable — the same rule as
/// `codec::decode_tuple`. Every failure is a typed error: a truncated or corrupt row
/// is data, and data never panics this crate (`CLAUDE.md` invariant 9).
///
/// A row written with *fewer* columns than `types` — every row written before an `ALTER TABLE ADD
/// COLUMN` — is padded from `missing`, which is why the `ALTER` rewrites nothing. A row that claims
/// *more* is corruption: rows and the catalog are read at one snapshot, so a row from a schema the
/// reader cannot see cannot be visible to it either.
///
/// # The pad travels with the types, in one value
///
/// [`RowSchema`] is the pair, and it is one type rather than two arguments so that a types list and
/// a pad list cannot drift apart — which they would, given four plan nodes each carrying both.
/// What a column pads with is PostgreSQL 11's `attmissingval`
/// (`crate::catalog::ColumnDef::missing`), and it is what makes `ADD COLUMN ... DEFAULT <constant>`
/// instant on a populated table. [`RowSchema::nullable`] is every column padding NULL, which is ADR
/// 0019's original rule, still the answer for a column added without a default, and the right
/// answer for a tuple that is not a table row at all — an index entry's primary key, a value out of
/// a catalog record.
pub fn decode_row(schema: &RowSchema, bytes: &[u8]) -> Result<Vec<Datum>> {
    let (types, missing) = (schema.types.as_slice(), schema.missing.as_slice());
    let (&version, rest) = bytes.split_first().ok_or_else(|| corrupt("row is empty"))?;
    if version != ROW_FORMAT_VERSION {
        return Err(corrupt(format!(
            "row format version {version} is not {ROW_FORMAT_VERSION}"
        )));
    }

    let (written, consumed) =
        varint::get_u64(rest).map_err(|error| corrupt(format!("row column count: {error}")))?;
    let written = usize::try_from(written)
        .map_err(|_| corrupt("a row of more columns than this machine can count"))?;
    if written > types.len() {
        return Err(corrupt(format!(
            "a row of {written} columns in a table of {}",
            types.len()
        )));
    }
    let rest = &rest[consumed..];

    let bitmap_len = written.div_ceil(8);
    let (bitmap, mut rest) = rest
        .split_at_checked(bitmap_len)
        .ok_or_else(|| corrupt("row ends inside its NULL bitmap"))?;

    let mut values = Vec::with_capacity(types.len());
    for (index, ty) in types.iter().take(written).enumerate() {
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
    // Columns added after this row was written, each padded with what the catalog says an absent
    // value means. NULL where nothing says otherwise — the `ALTER` that adds a column with no
    // default refuses `NOT NULL`, so NULL is a value it is allowed to hold; a column added *with*
    // a constant default is `NOT NULL`-able precisely because this pad is that constant.
    for index in values.len()..types.len() {
        values.push(missing.get(index).cloned().flatten().unwrap_or(Datum::Null));
    }
    Ok(values)
}

/// How one table's rows decode: a type per column, and what an absent column reads as.
///
/// The two lists are the same length by construction. They are one value because a plan node that
/// carried them separately would have two chances to be built with the wrong pair, and the symptom
/// would be a column reading NULL instead of its default — only for rows written before it was
/// added, which is the hardest case to notice.
#[derive(Debug, Clone, PartialEq)]
pub struct RowSchema {
    types: Vec<ColumnType>,
    missing: Vec<Option<Datum>>,
}

impl RowSchema {
    /// A schema whose columns pad with the values given, shortest wins: an entry past the end of
    /// `missing` pads NULL.
    #[must_use]
    pub fn new(types: Vec<ColumnType>, mut missing: Vec<Option<Datum>>) -> Self {
        missing.resize(types.len(), None);
        RowSchema { types, missing }
    }

    /// A schema whose columns all pad NULL.
    ///
    /// ADR 0019's rule, and the right one for anything that is not a table row: an index entry's
    /// primary key and a constant in a catalog record are tuples of exactly their own width, so
    /// there is nothing for a pad to answer for.
    #[must_use]
    pub fn nullable(types: Vec<ColumnType>) -> Self {
        let missing = vec![None; types.len()];
        RowSchema { types, missing }
    }

    /// The column types, in encoding order.
    #[must_use]
    pub fn types(&self) -> &[ColumnType] {
        &self.types
    }

    /// How many columns the schema has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Whether the schema has no columns at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one match over the whole type vocabulary, and the array half is a list of names \
              rather than of rules; splitting it would put half the vocabulary somewhere else"
)]
fn decode_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let truncated = || corrupt(format!("a {ty:?} is truncated"));
    Ok(match ty {
        // The sixteen bytes `encode_column` wrote: `x` then `y`, little-endian.
        ColumnType::Point => {
            let (head, rest) = bytes.split_first_chunk::<16>().ok_or_else(truncated)?;
            let (x, y) = head.split_at(8);
            (
                Datum::Point {
                    x: f64::from_le_bytes(x.try_into().unwrap_or([0; 8])),
                    y: f64::from_le_bytes(y.try_into().unwrap_or([0; 8])),
                },
                rest,
            )
        }
        ColumnType::Int8
        | ColumnType::TimestampTz
        | ColumnType::Timestamp
        | ColumnType::Money
        | ColumnType::Double => {
            let (head, rest) = bytes.split_first_chunk::<8>().ok_or_else(truncated)?;
            let value = match ty {
                ColumnType::Int8 => Datum::Int8(i64::from_le_bytes(*head)),
                ColumnType::TimestampTz => Datum::TimestampTz(i64::from_le_bytes(*head)),
                ColumnType::Timestamp => Datum::Timestamp(i64::from_le_bytes(*head)),
                ColumnType::Money => Datum::Money(i64::from_le_bytes(*head)),
                _ => Datum::Double(f64::from_le_bytes(*head)),
            };
            (value, rest)
        }
        ColumnType::Inet | ColumnType::Cidr => {
            let (head, rest) = bytes.split_first_chunk::<19>().ok_or_else(truncated)?;
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&head[3..]);
            (
                Datum::Inet {
                    family: head[0],
                    bits: head[1],
                    cidr: head[2] != 0,
                    addr,
                },
                rest,
            )
        }
        ColumnType::MacAddr => {
            let (head, rest) = bytes.split_first_chunk::<6>().ok_or_else(truncated)?;
            (Datum::MacAddr(*head), rest)
        }
        ColumnType::Int4 => {
            let (head, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
            (Datum::Int4(i32::from_le_bytes(*head)), rest)
        }
        // The same four little-endian bytes an `int4` is, which is what a `date` is on a real
        // server too — a day count from 2000-01-01, told apart from an integer by the column's
        // type and not by its bytes.
        ColumnType::Date => {
            let (head, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
            (Datum::Date(i32::from_le_bytes(*head)), rest)
        }
        // Eight little-endian bytes, the width `pg_type.typlen` gives it: a time of day is a
        // microsecond count and one day does not fit in four bytes of them.
        ColumnType::Time => {
            let (head, rest) = bytes.split_first_chunk::<8>().ok_or_else(truncated)?;
            (Datum::Time(i64::from_le_bytes(*head)), rest)
        }
        ColumnType::Uuid => {
            let (head, rest) = bytes.split_first_chunk::<16>().ok_or_else(truncated)?;
            (Datum::Uuid(*head), rest)
        }
        ColumnType::Interval => {
            let (head, rest) = bytes.split_first_chunk::<16>().ok_or_else(truncated)?;
            let (months, tail) = head.split_at(4);
            let (days, micros) = tail.split_at(4);
            (
                Datum::Interval {
                    months: i32::from_le_bytes(months.try_into().unwrap_or([0; 4])),
                    days: i32::from_le_bytes(days.try_into().unwrap_or([0; 4])),
                    micros: i64::from_le_bytes(micros.try_into().unwrap_or([0; 8])),
                },
                rest,
            )
        }
        ColumnType::Oid => {
            let (head, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
            (Datum::Oid(u32::from_le_bytes(*head)), rest)
        }
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::HstoreArray
        | ColumnType::TsRangeArray
        | ColumnType::TstzRangeArray
        | ColumnType::Int4RangeArray
        | ColumnType::DateRangeArray
        | ColumnType::NumRangeArray
        | ColumnType::Int8RangeArray
        | ColumnType::PointArray
        | ColumnType::BoolArray
        | ColumnType::ByteaArray
        | ColumnType::BpcharArray
        | ColumnType::VarcharArray
        | ColumnType::DateArray
        | ColumnType::TimeArray
        | ColumnType::TimestampArray
        | ColumnType::TimestampTzArray
        | ColumnType::IntervalArray
        | ColumnType::RealArray
        | ColumnType::DoubleArray
        | ColumnType::UuidArray
        | ColumnType::JsonArray
        | ColumnType::JsonbArray
        | ColumnType::OidArray
        | ColumnType::CitextArray
        | ColumnType::MoneyArray
        | ColumnType::InetArray
        | ColumnType::CidrArray
        | ColumnType::MacAddrArray => return decode_array(ty, bytes),
        ColumnType::Int2 => {
            let (head, rest) = bytes.split_first_chunk::<2>().ok_or_else(truncated)?;
            (Datum::Int2(i16::from_le_bytes(*head)), rest)
        }
        ColumnType::Real => {
            let (head, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
            (Datum::Real(f32::from_le_bytes(*head)), rest)
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
        ColumnType::Numeric => return take_numeric(bytes),
        ColumnType::Text
        | ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Hstore
        | ColumnType::Citext
        | ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange
        | ColumnType::Bytea => {
            let (len, consumed) = varint::get_u64(bytes)
                .map_err(|error| corrupt(format!("column length: {error}")))?;
            let len =
                usize::try_from(len).map_err(|_| corrupt("column longer than this machine"))?;
            let (body, rest) = bytes[consumed..]
                .split_at_checked(len)
                .ok_or_else(|| corrupt(format!("a column of {len} bytes is truncated")))?;
            let value = text_shaped(ty, body)?;
            (value, rest)
        }
    })
}

/// The server encoding is UTF8, so bytes that are not are refused the way PostgreSQL refuses them
/// rather than replaced with U+FFFD, which would silently change a stored value.
fn text_from_utf8(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        let at = error.utf8_error().valid_up_to();
        RowError::InvalidUtf8(error.as_bytes().get(at).copied().unwrap_or(0))
    })
}

fn corrupt(what: impl Into<String>) -> RowError {
    RowError::Corrupt(what.into())
}

/// What reading or writing a stored tuple can fail with.
///
/// One variant, because the row codec has one failure: the bytes do not say what a row of this
/// schema says. Corruption is a fact about bytes, which is why it can be named here at all —
/// everything a *user* did wrong is caught above, before a value reaches this layer.
/// Three, because they map to three different things above and collapsing them would lose the
/// distinction a caller reports to a user: corruption is `esker-sql`'s `DataCorrupted`, a
/// mismatch is an internal error because only a bug produces it, and invalid UTF-8 keeps
/// PostgreSQL's own `22021`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowError {
    /// The bytes are not a row of the schema they were read against.
    #[error("corrupt row: {0}")]
    Corrupt(String),
    /// The caller handed values that do not fit the schema. A bug above, never a wire condition.
    #[error("{0}")]
    Mismatch(String),
    /// Stored bytes are not UTF-8. Refused rather than replaced with U+FFFD, which would silently
    /// change a stored value.
    #[error("invalid byte sequence for encoding UTF8: {0:#04x}")]
    InvalidUtf8(u8),
}

/// The row codec's result.
pub type Result<T> = std::result::Result<T, RowError>;

/// `'t' ++ tenant ++ table_id ++ 'r' ++ memcomparable(primary key)`.
///
/// Primary key columns are `NOT NULL` by definition, so there is no NULL marker here; a NULL
/// reaching this function is a check the executor skipped, not something a client can send.
pub fn row_key(tenant: u64, table_id: u64, primary_key: &[Datum]) -> Result<Vec<u8>> {
    let mut key = prefix::table_row_prefix(tenant, table_id);
    for value in primary_key {
        if matches!(value, Datum::Null) {
            return Err(RowError::Mismatch(
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
                return Err(RowError::Mismatch(
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
        // Nothing is written for a NULL — the bitmap records it — nor for a **point**, which is
        // not an index key at all: `point = point` is `42883` on a real server and a key space
        // needs an order the type does not have. The column type is refused in
        // `decode_key_column`, which is where the error a caller sees comes from.
        Datum::Null | Datum::Point { .. } => {}
        Datum::Int8(v)
        | Datum::TimestampTz(v)
        | Datum::Timestamp(v)
        | Datum::Time(v)
        | Datum::Money(v) => {
            codec::encode_i64(*v, out);
        }
        // **Family, address, prefix — and not the `cidr` flag.** That is what makes
        // `'192.168.1.1'::inet = '192.168.1.1'::cidr` one key as well as one value, and it is why
        // the row encoding above carries a byte this does not. Eighteen fixed bytes, big-endian
        // throughout, so plain byte order is PostgreSQL's order.
        Datum::Inet {
            family, bits, addr, ..
        } => {
            out.push(*family);
            out.extend_from_slice(addr);
            out.push(*bits);
        }
        Datum::MacAddr(v) => out.extend_from_slice(v),
        // Widened to the `i64` encoding rather than given one of its own: an index key has to sort
        // by value and the memcomparable `i64` form already does, for every `i32` there is. A
        // second encoding would be a second thing to get wrong for no gain — a key is not a row,
        // and nothing reads its width back except the decoder beside it, which knows the type.
        Datum::Int4(v) | Datum::Date(v) => codec::encode_i64(i64::from(*v), out),
        // **The bytes are the key.** A uuid's order is its bytes' order — `uuid_cmp` is a
        // `memcmp` — and they are fixed width, so nothing has to be escaped or terminated.
        Datum::Uuid(v) => out.extend_from_slice(v),
        // **The key is the value, not the representation.** An index over an interval has to put
        // `1 mon` and `30 days` in the same place, because they are equal — so the key is the
        // microseconds the comparison converts them to, and the row keeps what was written.
        Datum::Interval {
            months,
            days,
            micros,
        } => codec::encode_i64(interval_total(*months, *days, *micros), out),
        // Widened to the `i64` key encoding, whose order is the unsigned's for every value
        // an `oid` can hold — they are all non-negative.
        Datum::Oid(v) => codec::encode_i64(i64::from(*v), out),
        Datum::Int2(v) => codec::encode_i64(i64::from(*v), out),
        // Four bytes, its own width, in the order `sort_bits_of_f32` puts floats.
        Datum::Real(v) => out.extend_from_slice(&crate::value::sort_bits_of_f32(*v).to_be_bytes()),
        // One byte, already in order: false is 0 and true is 1.
        Datum::Bool(v) => out.push(u8::from(*v)),
        // Sign-magnitude does not sort as an integer does, and PostgreSQL has fewer floats than
        // IEEE has; `sort_bits_of_f64` handles both.
        Datum::Double(v) => codec::encode_u64(sort_bits_of_f64(*v), out),
        // An hstore's key is its canonical text: its comparison *is* text's, unlike citext's.
        Datum::Text(v) | Datum::Hstore(v) | Datum::Range { text: v, .. } => {
            codec::encode_bytes(v.as_bytes(), out);
        }
        // **The key is the folded value**, which is the whole of how citext works: byte order over
        // folded bytes is case-insensitive order, byte equality over them is case-insensitive
        // equality, and a unique index therefore refuses two rows differing only in case — which
        // is what `validates_uniqueness_of` is enforced by. The *row* still holds the spelling.
        Datum::Citext(v) => codec::encode_bytes(v.to_lowercase().as_bytes(), out),
        Datum::Bytea(v) => codec::encode_bytes(v, out),
        // **The one encoding here that normalises**, and it has to: `1.0` and `1.00` are
        // different values of this type that *compare equal*, and an index key whose bytes
        // differed for them would let a `UNIQUE` index hold both. The row keeps the scale it was
        // written with; the key keeps the number.
        Datum::Numeric(v) => encode_key_numeric(v, out),
        Datum::Array(v) => encode_key_array(v, out),
    }
}

/// An array as bytes that sort the way PostgreSQL's `array_cmp` compares.
///
/// Element by element over the **flattened** elements, then by how many there are, then by the
/// shape. Three rules and each is measured: `'{}'` sorts first, `'{1,2,3}'` before
/// `'{{1,2},{3,4}}'` — which flattens to four elements and is longer — and `'{1,NULL,3}'` after
/// both, because **a NULL element sorts above every value**, not below.
///
/// The marker before each element is what produces all three: `0x00` ends the sequence and is
/// below the `0x01` that introduces a value, so a prefix sorts first; `0xFF` stands for a NULL
/// and is above every value. The shape follows the terminator, so it decides only between arrays
/// whose elements are identical — which is the tiebreak that keeps `'{1,2,3,4}'` and
/// `'{{1,2},{3,4}}'` two rows in a unique index, as they are two values on a real server.
fn encode_key_array(value: &crate::array::ArrayValue, out: &mut Vec<u8>) {
    for element in &value.values {
        match element {
            Some(element) => {
                out.push(1);
                encode_key_column(element, out);
            }
            None => out.push(0xFF),
        }
    }
    out.push(0);
    codec::encode_i64(i64::try_from(value.dims.len()).unwrap_or(i64::MAX), out);
    for dim in &value.dims {
        codec::encode_i64(i64::from(*dim), out);
    }
    codec::encode_i64(i64::from(value.lower), out);
}

/// An array back out of a row, given the type the schema says the column is.
fn decode_array(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let Some(element) = crate::array::ArrayValue::element_of(ty) else {
        return Err(corrupt(format!("{ty:?} is not an array type")));
    };
    let truncated = || corrupt(format!("a {ty:?} is truncated"));
    let mut rest = bytes;
    let read = |rest: &mut &[u8]| -> Result<u64> {
        let (value, used) =
            varint::get_u64(rest).map_err(|error| corrupt(format!("an array: {error}")))?;
        *rest = rest.get(used..).ok_or_else(truncated)?;
        Ok(value)
    };
    let lower = narrow_dimension(varint::zigzag_decode(read(&mut rest)?))?;
    let count = usize::try_from(read(&mut rest)?).map_err(|_| truncated())?;
    let mut dims = Vec::with_capacity(count.min(16));
    for _ in 0..count {
        dims.push(narrow_dimension(varint::zigzag_decode(read(&mut rest)?))?);
    }
    let elements = usize::try_from(read(&mut rest)?).map_err(|_| truncated())?;
    let mut values = Vec::with_capacity(elements.min(1024));
    for _ in 0..elements {
        let (&present, tail) = rest.split_first().ok_or_else(truncated)?;
        rest = tail;
        match present {
            0 => values.push(None),
            1 => {
                let (value, tail) = decode_column(element, rest)?;
                values.push(Some(value));
                rest = tail;
            }
            other => {
                return Err(corrupt(format!(
                    "an array element's presence byte is {other}"
                )));
            }
        }
    }
    Ok((
        Datum::Array(crate::array::ArrayValue {
            element,
            lower,
            dims,
            values,
        }),
        rest,
    ))
}

/// A dimension or a lower bound, which are `i32` on a real server too.
fn narrow_dimension(value: i64) -> Result<i32> {
    i32::try_from(value).map_err(|_| corrupt(format!("an array dimension of {value}")))
}

/// The types whose *ordering* is not their stored bytes', and which therefore cannot be an index
/// key.
///
/// A `jsonb`'s equality is not its bytes'; an hstore's and a range's **are**, and their *order* is
/// not — which is the same disqualification arrived at from the other side. Reaching this means the
/// bytes claim a key this crate never wrote.
fn not_a_key() -> RowError {
    corrupt("an index key column of type json, jsonb, hstore or a range")
}

/// The message [`not_a_key`] carries, so a test can tell that refusal from every other one.
#[cfg(test)]
const NOT_A_KEY: &str = "an index key column of type json, jsonb, hstore or a range";

/// A text-shaped column's value, chosen by the column's type rather than by the bytes.
///
/// **The bytes are the same for all of these** — a length and the UTF-8 — and what differs is which
/// `Datum` they become, which is the column's business and not the encoding's. A citext comes back
/// as a citext and not as a `Text` under a different column type: the difference between the two is
/// the *comparison*, and a comparison sees only values. A row that decoded to `Datum::Text` would
/// sort, group and deduplicate by bytes, which is what made `count(DISTINCT cival)` answer 2 where
/// a real server says 1. An hstore and a range are here for the same reason.
fn text_shaped(ty: ColumnType, body: &[u8]) -> Result<Datum> {
    Ok(match ty {
        ColumnType::Citext => Datum::Citext(text_from_utf8(body)?),
        ColumnType::Hstore => Datum::Hstore(text_from_utf8(body)?),
        // The subtype comes from the *column*, which is where it is known: the bytes are only the
        // canonical text.
        ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange => Datum::Range {
            subtype: Box::new(range_subtype(ty)),
            text: text_from_utf8(body)?,
        },
        ColumnType::Text
        | ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Json
        | ColumnType::Jsonb => Datum::Text(text_from_utf8(body)?),
        _ => Datum::Bytea(body.to_vec()),
    })
}

/// The subtype a range column's bounds are, which the column type names and the bytes do not.
/// What a range type is a range **of**, which is `pg_range.rngsubtype` on a real server.
///
/// **`pub`, and the only copy.** `esker_sql::value::range_subtype` delegates here: it was a second
/// table with the same three arms, and the day a fourth range type arrived only one of them
/// learned it — the proptest round trip caught it at once, handing a `DateRange` column a range
/// whose subtype said `Timestamp`. One table cannot drift from itself.
///
/// `int4range` and `int8range` both answer `bigint`: an `int4` is read as an `int8` everywhere in
/// this crate, which is the standing constant-width trade and not a fact about ranges.
#[must_use]
pub fn range_subtype(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::TstzRange => ColumnType::TimestampTz,
        ColumnType::Int4Range | ColumnType::Int8Range => ColumnType::Int8,
        ColumnType::DateRange => ColumnType::Date,
        ColumnType::NumRange => ColumnType::Numeric,
        ColumnType::FloatRange => ColumnType::Double,
        ColumnType::VarcharRange => ColumnType::Varchar,
        _ => ColumnType::Timestamp,
    }
}

/// A text-shaped index key column.
///
/// **A citext key holds the folded value**, so it decodes to a folded one — that is what makes a
/// unique index over one refuse two rows differing in case, and why an index is not where a citext
/// *value* is read back from; the row is. `Datum::Citext` says which of the two a caller has, so
/// confusing them is a type error rather than a wrong spelling.
fn decode_key_text(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let (body, rest) = codec::decode_bytes(bytes)
        .map_err(|error| corrupt(format!("index key column: {error}")))?;
    let text = text_from_utf8(&body)?;
    Ok(match ty {
        ColumnType::Citext => (Datum::Citext(text), rest),
        _ => (Datum::Text(text), rest),
    })
}

/// An array back out of an index key, in the shape [`encode_key_array`] wrote.
fn decode_key_array(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let Some(element) = crate::array::ArrayValue::element_of(ty) else {
        return Err(corrupt(format!("{ty:?} is not an array type")));
    };
    let truncated = || corrupt(format!("an index key holding a {ty:?} is truncated"));
    let decoded = |error: codec::CodecError| corrupt(format!("index key array: {error}"));
    let mut rest = bytes;
    let mut values = Vec::new();
    loop {
        let (&marker, tail) = rest.split_first().ok_or_else(truncated)?;
        rest = tail;
        match marker {
            0 => break,
            0xFF => values.push(None),
            1 => {
                let (value, tail) = decode_key_column(element, rest)?;
                values.push(Some(value));
                rest = tail;
            }
            other => {
                return Err(corrupt(format!("an array element's key marker is {other}")));
            }
        }
    }
    let (count, tail) = codec::decode_i64(rest).map_err(decoded)?;
    rest = tail;
    let count = usize::try_from(count).map_err(|_| truncated())?;
    let mut dims = Vec::with_capacity(count.min(16));
    for _ in 0..count {
        let (dim, tail) = codec::decode_i64(rest).map_err(decoded)?;
        rest = tail;
        dims.push(narrow_dimension(dim)?);
    }
    let (lower, tail) = codec::decode_i64(rest).map_err(decoded)?;
    Ok((
        Datum::Array(crate::array::ArrayValue {
            element,
            lower: narrow_dimension(lower)?,
            dims,
            values,
        }),
        tail,
    ))
}

/// A `numeric` as bytes that sort by value.
///
/// Five ordered groups, so a byte comparison answers before any digit is looked at:
/// `-Infinity` < negative < zero < positive < `Infinity` < `NaN`. **`NaN` sorts above everything**,
/// which is PostgreSQL's rule for this type and the opposite of `float8`'s — measured,
/// `'NaN'::numeric > 1` is `t`.
///
/// Inside the positive group the value is the exponent then the digits, both in a form that
/// compares as bytes: a bigger exponent is a bigger number whatever the digits, and two numbers
/// with one exponent are ordered by their digits from the left. The negative group is the same
/// bytes **complemented**, which reverses the order the way two's complement does for an integer.
fn encode_key_numeric(value: &Numeric, out: &mut Vec<u8>) {
    const NEG_INFINITY: u8 = 0;
    const NEGATIVE: u8 = 1;
    const ZERO: u8 = 2;
    const POSITIVE: u8 = 3;
    const POS_INFINITY: u8 = 4;
    const NAN: u8 = 5;

    let decimal = match value {
        Numeric::NegInfinity => return out.push(NEG_INFINITY),
        Numeric::PosInfinity => return out.push(POS_INFINITY),
        Numeric::NaN => return out.push(NAN),
        Numeric::Finite(decimal) => decimal.normalised(),
    };
    if decimal.is_zero() {
        return out.push(ZERO);
    }
    out.push(if decimal.negative { NEGATIVE } else { POSITIVE });
    // The exponent, in the codec's own order-preserving `i64`: big-endian with the sign bit
    // flipped. Offsetting it by a constant instead would sort correctly only until the exponent
    // reached the end of the offset, and this one is total over every `i64`.
    let mut body = Vec::with_capacity(9 + decimal.digits.len());
    codec::encode_i64(decimal.exponent(), &mut body);
    // Digits with a terminator below every digit, so `1` sorts before `11` — the same reason a
    // string encoding needs one.
    body.extend(decimal.digits.iter().map(|digit| digit + 1));
    body.push(0);
    if decimal.negative {
        for byte in &mut body {
            *byte = !*byte;
        }
    }
    out.extend_from_slice(&body);
}

/// Reads **index**-key columns back, which is what a secondary index lookup does to recover the
/// primary key it is pointing at.
///
/// # This does not read a row key
///
/// Index-key columns carry a NULL marker in front of each field — that is what lets a NULL sort
/// last and not collide with an empty string — and [`row_key`] writes no markers, because a
/// primary key column is `NOT NULL` by definition and a marker on every one of them would be a
/// byte per column for a case that cannot arise.
///
/// So this refuses a row key, and refuses it *confusingly*: the first byte it meets is the first
/// byte of a value, and it is reported as an index marker that is not one of ours. It is not a
/// decoder that happens to be strict about row keys; there is no public decoder for a row key at
/// all. A caller that wants a row's primary key back has three honest options — read the row and
/// take the key columns from it, carry the primary key alongside the key it built, or treat the
/// key bytes as an opaque identity, which is what a columnar learner does.
///
/// Named here because it has already cost one lane an afternoon of reading an index-marker error
/// against a row key. Adding the decoder is a small piece of work nobody has yet needed enough to
/// do; being told why it is absent is what this comment is for.
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

/// Whether a column of this type can be **part of an index key**.
///
/// The one list, and `decode_key_column` obeys it — named plainly rather than linked,
/// because it is private and a public doc may not point into private scope. Two kinds of type are on it and only the
/// first is PostgreSQL's rule: `json` and `point` have no default btree operator class *there*
/// either, and the rest — `jsonb`, `hstore`, the json and hstore arrays, every range — index fine
/// on a real server and not here, because their key encoding has not been written. A caller that
/// asks before building an index turns that gap into a refusal a client can read; without one,
/// the index is built and the first row written to it is an internal corruption error
/// (`esker_sql::exec::ddl::refuse_unindexable`, which is that caller).
#[must_use]
pub fn is_index_key(ty: ColumnType) -> bool {
    !matches!(
        ty,
        ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Point
            | ColumnType::Hstore
            | ColumnType::HstoreArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range
            | ColumnType::DateRange
            | ColumnType::NumRange
            | ColumnType::Int8Range
            | ColumnType::FloatRange
            | ColumnType::VarcharRange
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "one match over the whole type vocabulary; splitting it would hide which types are \
              keys and which are not, which is the only thing this function says"
)]
fn decode_key_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    // Asked here rather than only in the arms below, so that the one list a caller can read is the
    // same list this function obeys. The arms stay: a type missing from **either** is still
    // refused by the other, which is the direction a disagreement has to fail in.
    if !is_index_key(ty) {
        return Err(not_a_key());
    }
    let decoded = |error: codec::CodecError| corrupt(format!("index key column: {error}"));
    Ok(match ty {
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::TsRangeArray
        | ColumnType::TstzRangeArray
        | ColumnType::Int4RangeArray
        | ColumnType::DateRangeArray
        | ColumnType::NumRangeArray
        | ColumnType::Int8RangeArray
        | ColumnType::PointArray
        | ColumnType::BoolArray
        | ColumnType::ByteaArray
        | ColumnType::BpcharArray
        | ColumnType::VarcharArray
        | ColumnType::DateArray
        | ColumnType::TimeArray
        | ColumnType::TimestampArray
        | ColumnType::TimestampTzArray
        | ColumnType::IntervalArray
        | ColumnType::RealArray
        | ColumnType::DoubleArray
        | ColumnType::UuidArray
        | ColumnType::OidArray
        | ColumnType::CitextArray
        | ColumnType::MoneyArray
        | ColumnType::InetArray
        | ColumnType::CidrArray
        | ColumnType::MacAddrArray => return decode_key_array(ty, bytes),
        ColumnType::Int8 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Int8(value), rest)
        }
        ColumnType::Inet | ColumnType::Cidr => {
            let (head, rest) = bytes
                .split_first_chunk::<18>()
                .ok_or_else(|| corrupt("an index key column of type inet is truncated"))?;
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&head[1..17]);
            (
                Datum::Inet {
                    family: head[0],
                    bits: head[17],
                    // **The key does not say**, so the decode picks the type whose output
                    // function keeps nothing back. An index key is not where a value is read
                    // from — the row is — which is the rule `decode_key_text` already states for
                    // a citext.
                    cidr: false,
                    addr,
                },
                rest,
            )
        }
        ColumnType::MacAddr => {
            let (head, rest) = bytes
                .split_first_chunk::<6>()
                .ok_or_else(|| corrupt("an index key column of type macaddr is truncated"))?;
            (Datum::MacAddr(*head), rest)
        }
        // **A money is an index key**, unlike every other type added since `point`: `CREATE INDEX`
        // on one succeeds on a real server, and cents in an `i64` have exactly the order the key
        // encoding gives them.
        ColumnType::Money => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Money(value), rest)
        }
        // Read back **normalised**, which is what was written: an index key holds the number and
        // the row holds the scale it was spelled with. A decoder that claimed otherwise would be
        // inventing trailing zeros the key never carried.
        ColumnType::Numeric => return decode_key_numeric(bytes),
        ColumnType::Int4 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            let value = i32::try_from(value)
                .map_err(|_| corrupt(format!("index key holds {value}, which is not an int4")))?;
            (Datum::Int4(value), rest)
        }
        ColumnType::Date => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            let value = i32::try_from(value)
                .map_err(|_| corrupt(format!("index key holds {value}, which is not a date")))?;
            (Datum::Date(value), rest)
        }
        // No narrowing on the way back: a time is already the width the key encoding uses, and
        // its whole range — midnight through `24:00:00` inclusive — is ordinary `i64`.
        ColumnType::Time => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Time(value), rest)
        }
        ColumnType::Uuid => {
            let (head, rest) = bytes
                .split_first_chunk::<16>()
                .ok_or_else(|| corrupt("an index key with a short uuid"))?;
            (Datum::Uuid(*head), rest)
        }
        // Read back as microseconds with no months and no days, which is what the key holds: the
        // *value*, not the fields it was written with. The row is where the fields live.
        ColumnType::Interval => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (
                Datum::Interval {
                    months: 0,
                    days: 0,
                    micros: value,
                },
                rest,
            )
        }
        ColumnType::Oid => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            let value = u32::try_from(value)
                .map_err(|_| corrupt(format!("index key holds {value}, which is not an oid")))?;
            (Datum::Oid(value), rest)
        }
        ColumnType::Int2 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            let value = i16::try_from(value)
                .map_err(|_| corrupt(format!("index key holds {value}, which is not an int2")))?;
            (Datum::Int2(value), rest)
        }
        ColumnType::TimestampTz => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::TimestampTz(value), rest)
        }
        ColumnType::Timestamp => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Timestamp(value), rest)
        }
        ColumnType::Double => {
            let (bits, rest) = codec::decode_u64(bytes).map_err(decoded)?;
            (Datum::Double(crate::value::f64_of_sort_bits(bits)), rest)
        }
        ColumnType::Real => {
            let (head, rest) = bytes
                .split_first_chunk::<4>()
                .ok_or_else(|| corrupt("index key ends inside a real"))?;
            let bits = u32::from_be_bytes(*head);
            (Datum::Real(crate::value::f32_of_sort_bits(bits)), rest)
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
        // **Not a key column**, either of them. A `jsonb`'s equality is not its byte equality —
        // `1.0` and `1.00` print differently and compare equal — so an index over one would return
        // rows a scan does not, and `json` has no equality operator at all on a real server. The
        // SQL layer refuses both at `CREATE TABLE`; reaching here means the bytes claim a key this
        // crate never wrote, which is corruption rather than something to decode.
        // **Not a key column**, and hstore is here for a reason worth reading rather than
        // guessing. Its *equality* is its byte equality — the canonical form is a function of the
        // content — so sharing `text`'s representation is safe for `=`. Its **order is not**:
        // measured on 19beta1, `'a=>NULL'` sorts **first** among hstores sharing a key, where its
        // canonical text `"a"=>NULL` sorts after `"a"=>"2"` because `N` is above `"`. A byte
        // ordered key would return an index scan in an order a real server does not, which is
        // ADR 0042's rule one type later: a type may share another's representation only if it
        // shares its comparison, and hstore shares half of one.
        // **Not a key column.** A range's *ordering* is not its canonical text's — PostgreSQL
        // compares the lower bound, then its inclusivity, then the upper — so a byte-ordered key
        // would scan in an order a real server does not, which is the trap `hstore` fell into and
        // `tests/row_order.rs` caught. Its equality *is* its text's, which is why it groups and
        // deduplicates correctly without one.
        ColumnType::Json
        | ColumnType::Jsonb
        // **A point joins them with the sharpest reason of the four**: `json` has no equality
        // with another type, an hstore's *order* is not its text's, a range's order is not its
        // canonical text's — and a point has no equality even with itself, so there is no order
        // for a key to reproduce at all.
        | ColumnType::Point
        | ColumnType::Hstore
        | ColumnType::HstoreArray
        | ColumnType::JsonArray
        | ColumnType::JsonbArray
        | ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange => Err(not_a_key())?,
        ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar | ColumnType::Citext => {
            return decode_key_text(ty, bytes);
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

/// Every entry of one index that holds **these key values**, whatever suffix it carries.
///
/// The range a uniqueness check scans when the entries are suffixed — which is what a deferrable
/// constraint's are, so that two colliding rows can coexist until the check runs
/// (`esker_sql::exec::deferred`). The end is the **prefix's** successor and not the key's: a
/// suffixed entry sorts *after* the bare value key, so a range ending one byte past that key would
/// contain none of them.
///
/// # Errors
///
/// When a value cannot be encoded as a key column.
pub fn index_value_range(
    tenant: u64,
    table_id: u64,
    index_id: u64,
    columns: &[Datum],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let start = index_key(tenant, table_id, index_id, columns, None)?;
    let end = successor(start.clone());
    Ok((start, end))
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

/// The other half of [`encode_key_numeric`].
fn decode_key_numeric(bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let (&group, rest) = bytes
        .split_first()
        .ok_or_else(|| corrupt("a numeric key with no group byte"))?;
    let negative = match group {
        0 => return Ok((Datum::Numeric(Numeric::NegInfinity), rest)),
        2 => return Ok((Datum::Numeric(Numeric::Finite(Decimal::zero())), rest)),
        4 => return Ok((Datum::Numeric(Numeric::PosInfinity), rest)),
        5 => return Ok((Datum::Numeric(Numeric::NaN), rest)),
        1 => true,
        3 => false,
        other => return Err(corrupt(format!("numeric key group byte {other}"))),
    };
    let flip = |byte: u8| if negative { !byte } else { byte };
    let (head, rest) = rest
        .split_first_chunk::<8>()
        .ok_or_else(|| corrupt("a numeric key with no exponent"))?;
    let mut exponent_bytes = *head;
    for byte in &mut exponent_bytes {
        *byte = flip(*byte);
    }
    let (exponent, _) = codec::decode_i64(&exponent_bytes)
        .map_err(|_| corrupt("a numeric key with a short exponent"))?;
    let mut digits = Vec::new();
    let mut rest = rest;
    loop {
        let (&byte, tail) = rest
            .split_first()
            .ok_or_else(|| corrupt("a numeric key with no terminator"))?;
        rest = tail;
        let byte = flip(byte);
        if byte == 0 {
            break;
        }
        if byte > 10 {
            return Err(corrupt(format!("numeric key digit byte {byte}")));
        }
        digits.push(byte - 1);
    }
    if digits.is_empty() {
        return Err(corrupt("a numeric key with no digits"));
    }
    let scale = i64::try_from(digits.len()).unwrap_or(i64::MAX) - 1 - exponent;
    let scale =
        i32::try_from(scale).map_err(|_| corrupt(format!("a numeric key scale {scale}")))?;
    Ok((
        Datum::Numeric(Numeric::Finite(Decimal {
            negative,
            digits,
            scale,
        })),
        rest,
    ))
}

/// Tags for the four shapes a [`Numeric`] can be. Ours, like every tag in this format.
const NUMERIC_NAN: u8 = 0;
const NUMERIC_POS_INFINITY: u8 = 1;
const NUMERIC_NEG_INFINITY: u8 = 2;
const NUMERIC_FINITE: u8 = 3;
const NUMERIC_FINITE_NEGATIVE: u8 = 4;

/// One `numeric`, into a row.
///
/// A kind byte, and for a finite value a zigzag scale and the digits **one per byte**. One byte
/// per digit rather than two per byte: a `numeric` here is a column of a row and not a hot loop,
/// and a nibble-packed form would need a length parity bit to say whether the last nibble is a
/// digit — a second thing to get wrong for half the bytes of a value that is usually short.
fn put_numeric(value: &Numeric, out: &mut Vec<u8>) {
    let decimal = match value {
        Numeric::NaN => return out.push(NUMERIC_NAN),
        Numeric::PosInfinity => return out.push(NUMERIC_POS_INFINITY),
        Numeric::NegInfinity => return out.push(NUMERIC_NEG_INFINITY),
        Numeric::Finite(decimal) => decimal,
    };
    out.push(if decimal.negative {
        NUMERIC_FINITE_NEGATIVE
    } else {
        NUMERIC_FINITE
    });
    // Zigzag, because a scale is signed and small in both directions: `numeric(10,-2)` is real.
    varint::put_u64(varint::zigzag_encode(i64::from(decimal.scale)), out);
    varint::put_u64(decimal.digits.len() as u64, out);
    out.extend_from_slice(&decimal.digits);
}

/// The other half of [`put_numeric`].
fn take_numeric(bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let (&kind, rest) = bytes
        .split_first()
        .ok_or_else(|| corrupt("a numeric with no kind byte"))?;
    let negative = match kind {
        NUMERIC_NAN => return Ok((Datum::Numeric(Numeric::NaN), rest)),
        NUMERIC_POS_INFINITY => return Ok((Datum::Numeric(Numeric::PosInfinity), rest)),
        NUMERIC_NEG_INFINITY => return Ok((Datum::Numeric(Numeric::NegInfinity), rest)),
        NUMERIC_FINITE => false,
        NUMERIC_FINITE_NEGATIVE => true,
        other => return Err(corrupt(format!("numeric kind byte {other}"))),
    };
    let (scale, consumed) =
        varint::get_u64(rest).map_err(|error| corrupt(format!("numeric scale: {error}")))?;
    let rest = &rest[consumed..];
    let (len, consumed) =
        varint::get_u64(rest).map_err(|error| corrupt(format!("numeric length: {error}")))?;
    let len = usize::try_from(len).map_err(|_| corrupt("a numeric longer than this machine"))?;
    let (digits, rest) = rest[consumed..]
        .split_at_checked(len)
        .ok_or_else(|| corrupt(format!("a numeric of {len} digits is truncated")))?;
    if digits.is_empty() || digits.iter().any(|digit| *digit > 9) {
        return Err(corrupt("a numeric digit outside 0..=9"));
    }
    Ok((
        Datum::Numeric(Numeric::Finite(Decimal {
            negative,
            digits: digits.to_vec(),
            scale: unzigzag(scale)?,
        })),
        rest,
    ))
}

/// A signed scale back from [`varint::zigzag_encode`], refused if it is not an `i32`.
fn unzigzag(value: u64) -> Result<i32> {
    let wide = varint::zigzag_decode(value);
    i32::try_from(wide).map_err(|_| corrupt(format!("a numeric scale of {wide}")))
}

/// An interval as one number of microseconds, which is what comparison and an index key use.
///
/// **A month is thirty days and a day is twenty-four hours**, which is PostgreSQL's own rule for
/// comparing intervals: `'1 mon' = '30 days'` is `t` and `'1 year' = '360 days'` is `t`. It is a
/// conversion for ordering only — storage keeps the fields, so `1 mon 1 day` prints as itself.
#[must_use]
pub fn interval_total(months: i32, days: i32, micros: i64) -> i64 {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    i64::from(months)
        .saturating_mul(30)
        .saturating_add(i64::from(days))
        .saturating_mul(MICROS_PER_DAY)
        .saturating_add(micros)
}

#[cfg(test)]
mod tests {
    use super::RowError;
    use super::{
        ROW_FORMAT_VERSION, RowSchema, decode_key_columns, decode_row, encode_row, index_key,
        index_range, row_key, table_row_range, unique_index_key_is_unique_by_value,
    };
    use crate::value::{ColumnType, Datum};

    // PostgreSQL's timestamptz sentinels, as literals. The named constants are `esker-sql`'s,
    // beside the text formats that spell them "infinity" — the codec treats them as ordinary
    // `i64`s and these tests want them only as interesting values to round-trip.
    const POS_INFINITY: i64 = i64::MAX;
    const NEG_INFINITY: i64 = i64::MIN;
    const MIN_MICROS: i64 = -211_813_488_000_000_000;
    const MAX_MICROS: i64 = 9_223_371_331_200_000_000 - 1;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    /// The golden. These bytes are the format, and changing any of them is a format change with
    /// everything that implies (`CLAUDE.md`, "ask before doing"). Version 2 added the column
    /// count after the version byte, so that a row survives an `ALTER TABLE ADD COLUMN`
    /// (ADR 0019).
    #[test]
    fn a_row_value_is_a_version_a_count_a_bitmap_and_the_columns() {
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
                "02",               // format version
                "06",               // varint 6: the columns this row was written with
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
            decode_row(
                &RowSchema::nullable(types.to_vec()),
                &encode_row(&types, &values).unwrap()
            )
            .unwrap(),
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
                "02",               // version
                "09",               // varint 9 columns -- and so a two-byte bitmap
                "fe",               // columns 1..=7 are NULL, column 0 is not
                "00",               // column 8 is not NULL
                "0700000000000000", // column 0
                "0800000000000000", // column 8
            )
        );
        assert_eq!(
            decode_row(&RowSchema::nullable(types.to_vec()), &encoded).unwrap(),
            values
        );
    }

    /// The point of the column count: a row written before `ALTER TABLE ADD COLUMN` reads back
    /// with the new columns NULL, and nothing rewrote it.
    #[test]
    fn a_row_written_before_a_column_existed_reads_back_padded_with_nulls() {
        let before = [ColumnType::Int8, ColumnType::Text];
        let row = encode_row(&before, &[Datum::Int8(1), Datum::Text("one".into())]).unwrap();

        let after = [ColumnType::Int8, ColumnType::Text, ColumnType::Bool];
        assert_eq!(
            decode_row(&RowSchema::nullable(after.to_vec()), &row).unwrap(),
            [Datum::Int8(1), Datum::Text("one".into()), Datum::Null]
        );

        // And again, because two successive ALTERs are what a table actually gets.
        let after = [
            ColumnType::Int8,
            ColumnType::Text,
            ColumnType::Bool,
            ColumnType::Double,
        ];
        assert_eq!(
            decode_row(&RowSchema::nullable(after.to_vec()), &row).unwrap(),
            [
                Datum::Int8(1),
                Datum::Text("one".into()),
                Datum::Null,
                Datum::Null,
            ]
        );
    }

    /// The case a lenient decoder gets *wrong* rather than merely wrong-length: at nine columns
    /// the bitmap grows a second byte, so a reader that sized the bitmap from its own column list
    /// would read the first column's leading byte as bitmap and answer with plausible nonsense.
    /// The count in the row is what stops it.
    #[test]
    fn adding_a_ninth_column_does_not_reinterpret_the_bitmap() {
        let eight = [ColumnType::Int8; 8];
        let values: Vec<Datum> = (0..8).map(Datum::Int8).collect();
        let row = encode_row(&eight, &values).unwrap();
        assert_eq!(row[1], 8, "the count, before a one-byte bitmap");

        let nine = [ColumnType::Int8; 9];
        let mut expected = values;
        expected.push(Datum::Null);
        assert_eq!(
            decode_row(&RowSchema::nullable(nine.to_vec()), &row).unwrap(),
            expected
        );
    }

    /// The other direction is corruption, not padding. A row and the catalog are read at one
    /// snapshot, so a transaction that cannot see the `ALTER` cannot see a row that used it.
    #[test]
    fn a_row_of_more_columns_than_the_table_has_is_corruption() {
        let row = encode_row(
            &[ColumnType::Int8, ColumnType::Int8],
            &[Datum::Int8(1), Datum::Int8(2)],
        )
        .unwrap();
        let error = decode_row(&RowSchema::nullable(vec![ColumnType::Int8]), &row).unwrap_err();
        assert!(matches!(error, RowError::Corrupt(_)), "{error:?}");
    }

    /// An empty string is not a NULL, and the encoding must not let them become each other.
    #[test]
    fn an_empty_value_and_a_null_are_different_rows() {
        let types = [ColumnType::Text];
        let empty = encode_row(&types, &[Datum::Text(String::new())]).unwrap();
        let null = encode_row(&types, &[Datum::Null]).unwrap();
        assert_ne!(empty, null);
        assert_eq!(
            decode_row(&RowSchema::nullable(types.to_vec()), &empty).unwrap(),
            [Datum::Text(String::new())]
        );
        assert_eq!(
            decode_row(&RowSchema::nullable(types.to_vec()), &null).unwrap(),
            [Datum::Null]
        );
    }

    /// Invariant 2: an unknown version is an error value, never a guess and never a panic.
    #[test]
    fn a_row_from_a_future_version_is_refused() {
        let mut row = encode_row(&[ColumnType::Int8], &[Datum::Int8(1)]).unwrap();
        row[0] = ROW_FORMAT_VERSION + 1;
        let error = decode_row(&RowSchema::nullable(vec![ColumnType::Int8]), &row).unwrap_err();
        assert!(matches!(error, RowError::Corrupt(_)), "{error:?}");
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
                decode_row(&RowSchema::nullable(types.to_vec()), &row[..cut]).is_err(),
                "{cut} bytes decoded as a whole row"
            );
        }
        let mut trailing = row.clone();
        trailing.push(0);
        assert!(
            decode_row(&RowSchema::nullable(types.to_vec()), &trailing).is_err(),
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
        assert_eq!(
            decode_row(&RowSchema::nullable(types.to_vec()), &row).unwrap(),
            values
        );

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
            row_key(1, 2, &[Datum::Null]).unwrap_err(),
            RowError::Mismatch(
                "a NULL reached a primary key; NOT NULL is checked before the key is built".into()
            )
        );
    }

    /// Bytes that are not UTF-8 are refused rather than replaced, which would silently change a
    /// stored value — and the offending byte is carried, because that is what the message above
    /// reports.
    ///
    /// The *wording* is not asserted here. PostgreSQL's exact phrasing lives with
    /// `esker_sql::SqlError::InvalidByteSequence`, which this maps to, because a compatibility
    /// string is part of what a value means to a client and not part of what its bytes are.
    #[test]
    fn a_text_column_that_is_not_utf8_is_refused() {
        let mut row = encode_row(&[ColumnType::Text], &[Datum::Text("ab".into())]).unwrap();
        *row.last_mut().unwrap() = 0xff;
        let error = decode_row(&RowSchema::nullable(vec![ColumnType::Text]), &row).unwrap_err();
        assert_eq!(error, RowError::InvalidUtf8(0xff));
    }

    /// Every value a column of `ty` can hold, NULL included.
    #[allow(
        clippy::too_many_lines,
        reason = "one strategy per column type; the list is the vocabulary and splitting it would \
                  hide which types are covered"
    )]
    fn values_of(ty: ColumnType) -> proptest::strategy::BoxedStrategy<Datum> {
        use proptest::prelude::*;
        let values: BoxedStrategy<Datum> = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Datum::Int8).boxed(),
            // **Every `f64` including the ones that are not numbers**, because the round trip is
            // over the bits: a point holding `NaN` is a row a client can write and has to read
            // back unchanged.
            ColumnType::Point => (any::<f64>(), any::<f64>())
                .prop_map(|(x, y)| Datum::Point { x, y })
                .boxed(),
            ColumnType::Int4 => any::<i32>().prop_map(Datum::Int4).boxed(),
            ColumnType::Date => any::<i32>().prop_map(Datum::Date).boxed(),
            // The whole closed range, both ends included, because `24:00:00` is a value.
            ColumnType::Time => (0i64..=86_400_000_000).prop_map(Datum::Time).boxed(),
            ColumnType::Uuid => any::<[u8; 16]>().prop_map(Datum::Uuid).boxed(),
            ColumnType::Oid => any::<u32>().prop_map(Datum::Oid).boxed(),
            // Bounded so the total in microseconds cannot overflow, which is what the key holds.
            ColumnType::Interval => (-100_000i32..100_000, -100_000i32..100_000, any::<i32>())
                .prop_map(|(months, days, micros)| Datum::Interval {
                    months,
                    days,
                    micros: i64::from(micros),
                })
                .boxed(),
            // Weighted towards the shapes the encoding has cases for: the three specials, zero,
            // and a finite value at a scale on either side of nothing.
            ColumnType::Numeric => prop_oneof![
                6 => (any::<bool>(), proptest::collection::vec(0u8..=9, 1..12), -6i32..6)
                    .prop_map(|(negative, digits, scale)| Datum::Numeric(
                        crate::numeric::Numeric::Finite(crate::numeric::Decimal {
                            negative: negative && !digits.iter().all(|d| *d == 0),
                            digits,
                            scale,
                        })
                    )),
                4 => proptest::sample::select(vec![
                    crate::numeric::Numeric::NaN,
                    crate::numeric::Numeric::PosInfinity,
                    crate::numeric::Numeric::NegInfinity,
                    crate::numeric::Numeric::Finite(crate::numeric::Decimal::zero()),
                ])
                .prop_map(Datum::Numeric),
            ]
            .boxed(),
            // **An array of the element type's own values**, NULL elements included, at a lower
            // bound that is sometimes not one and occasionally in two dimensions — every part of
            // the value the encodings have to carry, so the round trip is what proves they do.
            ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray
            | ColumnType::HstoreArray
            | ColumnType::TsRangeArray
            | ColumnType::TstzRangeArray
            | ColumnType::Int4RangeArray
            | ColumnType::DateRangeArray
            | ColumnType::NumRangeArray
            | ColumnType::Int8RangeArray
            | ColumnType::PointArray
            | ColumnType::BoolArray
            | ColumnType::ByteaArray
            | ColumnType::BpcharArray
            | ColumnType::VarcharArray
            | ColumnType::DateArray
            | ColumnType::TimeArray
            | ColumnType::TimestampArray
            | ColumnType::TimestampTzArray
            | ColumnType::IntervalArray
            | ColumnType::RealArray
            | ColumnType::DoubleArray
            | ColumnType::UuidArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::OidArray
            | ColumnType::CitextArray
            | ColumnType::MoneyArray
            | ColumnType::InetArray
            | ColumnType::CidrArray
            | ColumnType::MacAddrArray => {
                let element = crate::array::ArrayValue::element_of(ty).unwrap_or(ColumnType::Text);
                (
                    proptest::collection::vec(
                        proptest::option::of(values_of(element).prop_filter(
                            "a NULL element is the `None`, not a `Datum::Null`",
                            |value| !matches!(value, Datum::Null),
                        )),
                        0..6,
                    ),
                    -2i32..3,
                    any::<bool>(),
                )
                    .prop_map(move |(values, lower, two_dimensional)| {
                        let mut value =
                            crate::array::ArrayValue::one_dimensional(element, lower, values);
                        // A shape with the same elements under it, which is the case the key's
                        // dimension tiebreak exists for.
                        if two_dimensional && value.values.len() == 4 {
                            value.dims = vec![2, 2];
                        }
                        Datum::Array(value)
                    })
                    .boxed()
            }
            ColumnType::Int2 => any::<i16>().prop_map(Datum::Int2).boxed(),
            ColumnType::Real => prop_oneof![
                7 => any::<f32>().prop_map(Datum::Real),
                3 => proptest::sample::select(vec![
                    0.0f32, -0.0, f32::NAN, -f32::NAN, f32::INFINITY, f32::NEG_INFINITY,
                ])
                .prop_map(Datum::Real),
            ]
            .boxed(),
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                ".{0,32}".prop_map(Datum::Text).boxed()
            }
            // Valid documents, because that is what a `json` column holds — an arbitrary string
            // is not one, and the row codec is only ever handed a value the SQL layer validated.
            // An hstore is a `Datum::Text` here like every other text-shaped type: this crate
            // stores the canonical form and `esker_sql::value::hstore` is what makes one, so the
            // strategy is text and the round trip is the text's.
            ColumnType::Hstore => ".*".prop_map(Datum::Hstore).boxed(),
            // A range's stored form is its canonical text, and `empty` is the one value every
            // subtype has — enough to state the round trip, which is what this property is.
            ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range
            | ColumnType::DateRange
            | ColumnType::NumRange
            | ColumnType::Int8Range
            | ColumnType::FloatRange
            | ColumnType::VarcharRange => Just(Datum::Range {
                subtype: Box::new(super::range_subtype(ty)),
                text: "empty".to_owned(),
            })
            .boxed(),
            // A citext's *key* is its folded value, so the strategy is folded text: an unfolded
            // one would state a round trip the key encoding does not make.
            ColumnType::Citext => ".*"
                .prop_map(|text: String| Datum::Citext(text.to_lowercase()))
                .boxed(),
            // The whole `i64`, because that is the whole type: `money`'s range is `i64` cents and
            // both ends of it are real values a client can write.
            ColumnType::Money => proptest::num::i64::ANY.prop_map(Datum::Money).boxed(),
            // Both families and both types, because all four combinations are values a client can
            // write and the key encoding orders across them.
            // **The flag is the column's, not the strategy's.** A `Datum::Inet` whose `cidr` is
            // `false` is an `inet` value, and `Datum::fits` refuses it in a `cidr` column — which
            // is the property working: the flag is part of what the column type *is*.
            ColumnType::Inet | ColumnType::Cidr => (
                proptest::sample::select(vec![crate::value::INET_V4, crate::value::INET_V6]),
                proptest::array::uniform16(proptest::num::u8::ANY),
                Just(ty == ColumnType::Cidr),
            )
                .prop_map(move |(family, addr, cidr)| {
                    let full = if family == crate::value::INET_V6 {
                        128
                    } else {
                        32
                    };
                    Datum::Inet {
                        family,
                        bits: full,
                        cidr,
                        addr: if family == crate::value::INET_V6 {
                            addr
                        } else {
                            let mut narrow = [0u8; 16];
                            narrow[..4].copy_from_slice(&addr[..4]);
                            narrow
                        },
                    }
                })
                .boxed(),
            ColumnType::MacAddr => proptest::array::uniform6(proptest::num::u8::ANY)
                .prop_map(Datum::MacAddr)
                .boxed(),
            ColumnType::Json | ColumnType::Jsonb => proptest::sample::select(vec![
                "null",
                "true",
                "1",
                "1.00",
                "\"s\"",
                "[]",
                "[1, 2]",
                "{}",
                "{\"a\": 1}",
            ])
            .prop_map(|text| Datum::Text(text.to_owned()))
            .boxed(),
            ColumnType::Bool => any::<bool>().prop_map(Datum::Bool).boxed(),
            ColumnType::Bytea => proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(Datum::Bytea)
                .boxed(),
            ColumnType::Timestamp => (MIN_MICROS..=MAX_MICROS).prop_map(Datum::Timestamp).boxed(),
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
        // `ALL` and the user-range representations beside it: the second list is not in the
        // first for the reason `ColumnType::USER_RANGES` gives, and a codec property that
        // skipped it would leave two stored types unchecked.
        let every: Vec<ColumnType> = ColumnType::ALL
            .into_iter()
            .chain(ColumnType::USER_RANGES)
            .collect();
        proptest::collection::vec(proptest::sample::select(every), columns).prop_flat_map(
            move |types| {
                let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
                (Just(types), proptest::collection::vec(row, rows..=rows))
            },
        )
    }

    /// **`is_index_key` and the decoder say the same thing about every type.**
    ///
    /// Two readers of one list, which is what [`super::is_index_key`] exists to be: `esker_sql`
    /// asks it before building an index, and `decode_key_column` asks it before reading a key.
    /// They are checked against each other here because a type on one list and not the other is
    /// exactly the shape of bug this crate keeps finding — a table with a second copy — and the
    /// consequence is the bad one: an index a client is allowed to create and cannot write to.
    #[test]
    fn every_type_agrees_with_itself_about_being_an_index_key() {
        for ty in ColumnType::ALL.into_iter().chain(ColumnType::USER_RANGES) {
            // A present marker and no body: every type errors, and only these error *this* way.
            let refused = decode_key_columns(&[ty], &[super::KEY_PRESENT])
                .is_err_and(|error| error.to_string().contains(super::NOT_A_KEY));
            assert_eq!(
                refused,
                !super::is_index_key(ty),
                "{ty:?} is refused by the decoder and allowed by `is_index_key`, or the reverse"
            );
        }
    }

    /// **The key bytes sort the way the numbers do**, and equal numbers written differently
    /// produce the *same* bytes.
    ///
    /// `tests/proptest_codec.rs` opens by naming the failure this catches: an encoding that is
    /// "right way up, round-trips perfectly and sorts wrongly". A `numeric` key is the encoding
    /// in this module most able to do that — five ordered groups, an exponent ahead of the
    /// digits, a terminator under every digit and a complement over the whole body when the
    /// value is negative — and a round-trip property sees none of it.
    ///
    /// The ladder is written as groups: within a group every spelling must encode to identical
    /// bytes, because `1`, `1.0` and `1.00` are one value and an index may hold it once.
    #[test]
    fn a_numeric_key_sorts_the_way_the_number_does() {
        use crate::numeric::{Decimal, Numeric};

        /// `digits` as written, with `scale` of them after the point.
        fn finite(negative: bool, digits: &str, scale: i32) -> Numeric {
            Numeric::Finite(Decimal {
                negative,
                digits: digits.bytes().map(|byte| byte - b'0').collect(),
                scale,
            })
        }
        let key =
            |value: &Numeric| index_key(1, 2, 3, &[Datum::Numeric(value.clone())], None).unwrap();

        // Ascending. Every inner slice is one value, spelled every way this codec allows.
        let ladder: Vec<Vec<Numeric>> = vec![
            vec![Numeric::NegInfinity],
            vec![finite(true, "123456789012345678905", 1)],
            // A negative scale multiplies, so this is -1230 three ways — and it is *below*
            // -1.5, which is the ordering a magnitude-blind encoding gets backwards.
            vec![
                finite(true, "1230", 0),
                finite(true, "123", -1),
                finite(true, "12300", 1),
            ],
            vec![finite(true, "15", 1)],
            vec![
                finite(true, "10", 1),
                finite(true, "1", 0),
                finite(true, "100", 2),
            ],
            vec![finite(true, "1", 3)],
            // Zero carries no sign, and every scale of it is the same value.
            vec![
                Decimal::zero(),
                Decimal {
                    negative: false,
                    digits: vec![0, 0],
                    scale: 2,
                },
                Decimal {
                    negative: true,
                    digits: vec![0],
                    scale: -2,
                },
            ]
            .into_iter()
            .map(Numeric::Finite)
            .collect(),
            vec![finite(false, "1", 3)],
            vec![
                finite(false, "1", 0),
                finite(false, "10", 1),
                finite(false, "100", 2),
            ],
            vec![finite(false, "15", 1)],
            vec![finite(false, "9", 0)],
            // The pair a lexicographic encoding gets wrong: "10" sorts under "9" as text.
            vec![finite(false, "10", 0)],
            vec![finite(false, "123", -1), finite(false, "1230", 0)],
            vec![finite(false, "123456789012345678905", 1)],
            vec![Numeric::PosInfinity],
            vec![Numeric::NaN],
        ];

        for group in &ladder {
            let first = key(&group[0]);
            for other in &group[1..] {
                assert_eq!(
                    key(other),
                    first,
                    "{:?} and {:?} are one value and must be one key",
                    group[0],
                    other
                );
            }
        }
        for pair in ladder.windows(2) {
            let (low, high) = (key(&pair[0][0]), key(&pair[1][0]));
            assert!(
                low < high,
                "{:?} must sort below {:?}",
                pair[0][0],
                pair[1][0]
            );
        }
    }

    proptest::proptest! {
        /// Round trip: whatever goes into a row comes back out of it, for any mixture of types
        /// and any placement of NULLs.
        #[test]
        /// Every row survives every number of columns appended after it was written, which is
        /// the property `ALTER TABLE ADD COLUMN` rests on. The appended types are arbitrary:
        /// nothing of them is read, because the padding is NULL whatever they are.
        #[test]
        fn a_row_survives_any_number_of_columns_appended_after_it(
            (types, rows) in schema_and_rows(0..12, 1),
            added in proptest::collection::vec(
                proptest::sample::select(&[
                    ColumnType::Int8,
                    ColumnType::Text,
                    ColumnType::Bool,
                    ColumnType::Bytea,
                    ColumnType::TimestampTz,
                    ColumnType::Double,
                ][..]),
                0..6,
            ),
        ) {
            let row = encode_row(&types, &rows[0]).unwrap();
            let mut widened = types.clone();
            widened.extend_from_slice(&added);
            let mut expected = rows[0].clone();
            expected.resize(widened.len(), Datum::Null);
            proptest::prop_assert_eq!(decode_row(&RowSchema::nullable(widened.clone()), &row).unwrap(), expected);
        }

        #[test]
        fn any_row_survives_encode_and_decode((types, rows) in schema_and_rows(0..12, 1)) {
            let values = &rows[0];
            let encoded = encode_row(&types, values).unwrap();
            proptest::prop_assert_eq!(&decode_row(&RowSchema::nullable(types.clone()), &encoded).unwrap(), values);
        }

        /// Invariant 9 for an on-disk format: arbitrary bytes are an error or a value, never a
        /// panic.
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            bytes in proptest::collection::vec(proptest::arbitrary::any::<u8>(), 0..64)
        ) {
            let types = [ColumnType::Int8, ColumnType::Text, ColumnType::Double];
            let _ = decode_row(&RowSchema::nullable(types.to_vec()), &bytes);
            let _ = decode_key_columns(&types, &bytes);
        }
    }
}
