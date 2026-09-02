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
        Datum::Int8(v) | Datum::TimestampTz(v) | Datum::Timestamp(v) => {
            out.extend_from_slice(&v.to_le_bytes());
        }
        // Four bytes, not eight. Nothing written before `int4` existed has a column of this type,
        // so the narrower width costs no compatibility and is what `pg_type.typlen` says it is.
        Datum::Int4(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Int2(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Bool(v) => out.push(u8::from(*v)),
        Datum::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
        Datum::Real(v) => out.extend_from_slice(&v.to_le_bytes()),
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

fn decode_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let truncated = || corrupt(format!("a {ty:?} is truncated"));
    Ok(match ty {
        ColumnType::Int8 | ColumnType::TimestampTz | ColumnType::Timestamp | ColumnType::Double => {
            let (head, rest) = bytes.split_first_chunk::<8>().ok_or_else(truncated)?;
            let value = match ty {
                ColumnType::Int8 => Datum::Int8(i64::from_le_bytes(*head)),
                ColumnType::TimestampTz => Datum::TimestampTz(i64::from_le_bytes(*head)),
                ColumnType::Timestamp => Datum::Timestamp(i64::from_le_bytes(*head)),
                _ => Datum::Double(f64::from_le_bytes(*head)),
            };
            (value, rest)
        }
        ColumnType::Int4 => {
            let (head, rest) = bytes.split_first_chunk::<4>().ok_or_else(truncated)?;
            (Datum::Int4(i32::from_le_bytes(*head)), rest)
        }
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
        ColumnType::Text | ColumnType::Varchar | ColumnType::Bytea => {
            let (len, consumed) = varint::get_u64(bytes)
                .map_err(|error| corrupt(format!("column length: {error}")))?;
            let len =
                usize::try_from(len).map_err(|_| corrupt("column longer than this machine"))?;
            let (body, rest) = bytes[consumed..]
                .split_at_checked(len)
                .ok_or_else(|| corrupt(format!("a column of {len} bytes is truncated")))?;
            let value = if matches!(ty, ColumnType::Text | ColumnType::Varchar) {
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
        Datum::Null => {}
        Datum::Int8(v) | Datum::TimestampTz(v) | Datum::Timestamp(v) => codec::encode_i64(*v, out),
        // Widened to the `i64` encoding rather than given one of its own: an index key has to sort
        // by value and the memcomparable `i64` form already does, for every `i32` there is. A
        // second encoding would be a second thing to get wrong for no gain — a key is not a row,
        // and nothing reads its width back except the decoder beside it, which knows the type.
        Datum::Int4(v) => codec::encode_i64(i64::from(*v), out),
        Datum::Int2(v) => codec::encode_i64(i64::from(*v), out),
        // Four bytes, its own width, in the order `sort_bits_of_f32` puts floats.
        Datum::Real(v) => out.extend_from_slice(&crate::value::sort_bits_of_f32(*v).to_be_bytes()),
        // One byte, already in order: false is 0 and true is 1.
        Datum::Bool(v) => out.push(u8::from(*v)),
        // Sign-magnitude does not sort as an integer does, and PostgreSQL has fewer floats than
        // IEEE has; `sort_bits_of_f64` handles both.
        Datum::Double(v) => codec::encode_u64(sort_bits_of_f64(*v), out),
        Datum::Text(v) => codec::encode_bytes(v.as_bytes(), out),
        Datum::Bytea(v) => codec::encode_bytes(v, out),
    }
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

fn decode_key_column(ty: ColumnType, bytes: &[u8]) -> Result<(Datum, &[u8])> {
    let decoded = |error: codec::CodecError| corrupt(format!("index key column: {error}"));
    Ok(match ty {
        ColumnType::Int8 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            (Datum::Int8(value), rest)
        }
        ColumnType::Int4 => {
            let (value, rest) = codec::decode_i64(bytes).map_err(decoded)?;
            let value = i32::try_from(value)
                .map_err(|_| corrupt(format!("index key holds {value}, which is not an int4")))?;
            (Datum::Int4(value), rest)
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
        ColumnType::Text | ColumnType::Varchar => {
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
    fn values_of(ty: ColumnType) -> proptest::strategy::BoxedStrategy<Datum> {
        use proptest::prelude::*;
        let values: BoxedStrategy<Datum> = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Datum::Int8).boxed(),
            ColumnType::Int4 => any::<i32>().prop_map(Datum::Int4).boxed(),
            ColumnType::Int2 => any::<i16>().prop_map(Datum::Int2).boxed(),
            ColumnType::Real => prop_oneof![
                7 => any::<f32>().prop_map(Datum::Real),
                3 => proptest::sample::select(vec![
                    0.0f32, -0.0, f32::NAN, -f32::NAN, f32::INFINITY, f32::NEG_INFINITY,
                ])
                .prop_map(Datum::Real),
            ]
            .boxed(),
            ColumnType::Text | ColumnType::Varchar => ".{0,32}".prop_map(Datum::Text).boxed(),
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
        proptest::collection::vec(
            proptest::sample::select(ColumnType::ALL.as_slice()),
            columns,
        )
        .prop_flat_map(move |types| {
            let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
            (Just(types), proptest::collection::vec(row, rows..=rows))
        })
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
