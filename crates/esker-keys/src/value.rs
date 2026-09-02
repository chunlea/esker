//! The six shapes a stored tuple can hold, and nothing about what they mean in SQL.
//!
//! This is the **storage layer's shared type vocabulary**. It lives here, beside the codecs that
//! write it, because [`crate::row`] and [`crate::codec`] both need to name a value and neither
//! may depend on `esker-sql`, which sits above the store.
//!
//! # What is deliberately not here
//!
//! Every PostgreSQL-ism stays in `esker-sql`, behind an extension trait: type OIDs, the text and
//! binary I/O formats, and [`Datum`]'s PostgreSQL *ordering*. Those are contract C3's surface and
//! they are measured against a running server; a crate that owns byte layout has no business
//! carrying them. [ADR 0029](../../../docs/adr/0030-the-row-codec-moves-down.md).
//!
//! # One comparison stays and one leaves, and they disagree
//!
//! [`Datum`]'s [`PartialEq`] is here and is **bitwise for doubles**, so `-0.0` is not `0.0` and one
//! `NaN` payload is not another. `pg_cmp` is in `esker-sql` and says the opposite: `NaN` equals
//! itself and sorts above every other float, which is what PostgreSQL does and what a `WHERE x > 5`
//! has to agree with.
//!
//! That is not a contradiction, it is the seam. Equality here answers *did these bytes survive the
//! round trip*, which is a storage question and must be exact. `pg_cmp` answers *how does a user's
//! query order these*, which is a SQL question and must match a real server. Putting them in one
//! crate is what would let somebody use the wrong one.

/// The 64 bits whose big-endian order is PostgreSQL's order over floats, for an index key.
///
/// Lives beside the type rather than in [`crate::row`] because which floats are *the same value*
/// is a fact about the type, not about the key encoding that has to respect it. The ordering it
/// produces is PostgreSQL's — `NaN` above `Infinity` — which is why `esker-sql`'s `pg_cmp` and
/// this function agree even though [`Datum`]'s `PartialEq` does not.
#[must_use]
pub fn sort_bits_of_f64(value: f64) -> u64 {
    let canonical = if value.is_nan() {
        f64::NAN
    } else if value == 0.0 {
        0.0
    } else {
        value
    };
    let bits = canonical.to_bits();
    if bits & SIGN == 0 { bits | SIGN } else { !bits }
}

/// The same for an `f32`, and 32 bits rather than 64.
///
/// **Not** `sort_bits_of_f64(value.into())`: widening an `f32` is exact, so that would sort
/// correctly — and it would write eight bytes where the type is four, which is the same lie about
/// a width that [`ColumnType::Real`] exists to avoid.
#[must_use]
pub fn sort_bits_of_f32(value: f32) -> u32 {
    const SIGN32: u32 = 1 << 31;
    let canonical = if value.is_nan() {
        f32::NAN
    } else if value == 0.0 {
        0.0
    } else {
        value
    };
    let bits = canonical.to_bits();
    if bits & SIGN32 == 0 {
        bits | SIGN32
    } else {
        !bits
    }
}

/// The inverse of [`sort_bits_of_f32`], up to the canonicalisation it performs.
#[must_use]
pub fn f32_of_sort_bits(bits: u32) -> f32 {
    const SIGN32: u32 = 1 << 31;
    f32::from_bits(if bits & SIGN32 == 0 {
        !bits
    } else {
        bits & !SIGN32
    })
}

/// The inverse of [`sort_bits_of_f64`], up to the canonicalisation it performs.
#[must_use]
pub fn f64_of_sort_bits(bits: u64) -> f64 {
    f64::from_bits(if bits & SIGN == 0 {
        !bits
    } else {
        bits & !SIGN
    })
}

/// One of the six types phase 6a executes (`docs/plans/phase-6a.md` §3).
///
/// The type of a stored value. What each one means to a client — its OID, its text format —
/// is `esker-sql`'s, on an extension trait over this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnType {
    /// 64-bit integer. PostgreSQL calls it `bigint` in messages and `int8` in DDL.
    Int8,
    /// 16-bit integer. PostgreSQL calls it `smallint` in messages and `int2` in DDL. A distinct
    /// type for the same reason `int4` is: two bytes, and `22003` past its own range.
    Int2,
    /// 32-bit integer. PostgreSQL calls it `integer` in messages and `int4` in DDL.
    ///
    /// A **distinct type and not an alias for [`ColumnType::Int8`]** ([ADR
    /// 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)): a client asking what a column is
    /// gets `int4`'s OID, and a value between 2^31 and 2^63 is `22003` here as it is on a real
    /// server rather than being quietly accepted.
    Int4,
    /// Variable-length UTF-8 string.
    Text,
    /// PostgreSQL's `character varying`. **The same representation as [`ColumnType::Text`] and a
    /// different type**, which is PostgreSQL's own model: `text`, `varchar` and `bpchar` are one
    /// varlena told apart by OID, not by bytes. So a `varchar` column's rows are byte-identical to
    /// a `text` column's and this is not a format change even in principle
    /// ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Varchar,
    /// Two-valued, with no third state but NULL.
    Bool,
    /// Variable-length byte string.
    Bytea,
    /// An instant, stored as microseconds from 2000-01-01 UTC.
    TimestampTz,
    /// PostgreSQL's `timestamp` **without** time zone: the same eight bytes as
    /// [`ColumnType::TimestampTz`] and a different type. It does no zone conversion, so what goes
    /// in is what comes out, and it prints with no offset
    /// ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Timestamp,
    /// IEEE-754 binary64.
    Double,
    /// IEEE-754 binary32; PostgreSQL's `real`. A distinct type because its **text** differs — the
    /// shortest digits that round-trip at 32 bits — and because a value a `double` holds is
    /// `22003` here at both ends of the range.
    Real,
}

impl ColumnType {
    /// Every type, for tests that must not silently skip one.
    pub const ALL: [ColumnType; 11] = [
        ColumnType::Int8,
        ColumnType::Int4,
        ColumnType::Int2,
        ColumnType::Text,
        ColumnType::Varchar,
        ColumnType::Bool,
        ColumnType::Bytea,
        ColumnType::TimestampTz,
        ColumnType::Timestamp,
        ColumnType::Double,
        ColumnType::Real,
    ];
}

/// One column's value, or its absence.
///
/// `PartialEq` compares floats by their **bits**, not by IEEE equality, so `NaN` equals itself and
/// `-0.0` does not equal `0.0`. That is the right question for an encoding module — "did this
/// value survive the round trip" — and the wrong one for SQL, where PostgreSQL says both the
/// opposite things. SQL's comparison is `esker_sql::value::PgDatum::pg_cmp`, in the crate that
/// owns what a value *means*, precisely so neither can be mistaken for the other.
#[derive(Debug, Clone)]
pub enum Datum {
    /// SQL NULL, of whatever the column's type is.
    Null,
    /// [`ColumnType::Int8`].
    Int8(i64),
    /// [`ColumnType::Int4`]. Four bytes on disk, and four bytes is the point: the width is what
    /// makes it a different type from an `int8` that happens to hold a small number.
    Int4(i32),
    /// [`ColumnType::Int2`]. Two bytes, for the same reason.
    Int2(i16),
    /// [`ColumnType::Text`]. Always valid UTF-8: the server encoding is UTF8, and bytes that are
    /// not are refused on the way in the way PostgreSQL refuses them.
    Text(String),
    /// [`ColumnType::Bool`].
    Bool(bool),
    /// [`ColumnType::Bytea`].
    Bytea(Vec<u8>),
    /// [`ColumnType::TimestampTz`], in microseconds from 2000-01-01 00:00:00 UTC — PostgreSQL's
    /// own epoch and its own representation, including the infinities `esker-sql` names
    /// `POS_INFINITY` and `NEG_INFINITY` — to this crate they are ordinary `i64` sentinels.
    TimestampTz(i64),
    /// [`ColumnType::Double`].
    Double(f64),
    /// [`ColumnType::Real`].
    Real(f32),
    /// [`ColumnType::Timestamp`], in microseconds from 2000-01-01 — the same representation as
    /// [`Datum::TimestampTz`], and a separate variant because the two print differently and a
    /// value has to know which it is.
    Timestamp(i64),
}

impl PartialEq for Datum {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Datum::Null, Datum::Null) => true,
            (Datum::Int8(a), Datum::Int8(b)) | (Datum::TimestampTz(a), Datum::TimestampTz(b)) => {
                a == b
            }
            (Datum::Int4(a), Datum::Int4(b)) => a == b,
            (Datum::Int2(a), Datum::Int2(b)) => a == b,
            (Datum::Timestamp(a), Datum::Timestamp(b)) => a == b,
            (Datum::Text(a), Datum::Text(b)) => a == b,
            (Datum::Bool(a), Datum::Bool(b)) => a == b,
            (Datum::Bytea(a), Datum::Bytea(b)) => a == b,
            // Bitwise, so a round-trip test cannot pass by turning -0.0 into 0.0 or one NaN
            // payload into another.
            (Datum::Double(a), Datum::Double(b)) => a.to_bits() == b.to_bits(),
            (Datum::Real(a), Datum::Real(b)) => a.to_bits() == b.to_bits(),
            _ => false,
        }
    }
}

impl Eq for Datum {}

impl Datum {
    /// The type this value belongs to, or `None` for NULL, which belongs to all of them.
    #[must_use]
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Datum::Null => return None,
            Datum::Int8(_) => ColumnType::Int8,
            Datum::Int4(_) => ColumnType::Int4,
            Datum::Int2(_) => ColumnType::Int2,
            Datum::Real(_) => ColumnType::Real,
            Datum::Timestamp(_) => ColumnType::Timestamp,
            Datum::Text(_) => ColumnType::Text,
            Datum::Bool(_) => ColumnType::Bool,
            Datum::Bytea(_) => ColumnType::Bytea,
            Datum::TimestampTz(_) => ColumnType::TimestampTz,
            Datum::Double(_) => ColumnType::Double,
        })
    }

    /// Whether this value fits a column of `ty`. NULL fits every type.
    #[must_use]
    pub fn fits(&self, ty: ColumnType) -> bool {
        match (self.column_type(), ty) {
            // NULL fits every column; and `text` fits `varchar` as well as `text`, because they are
            // **one representation and two types**. [`Datum`] has no `Varchar` variant, since there
            // would be nothing in one that a `Text` does not already hold — what differs is the
            // column's declared type, which comes from the schema and not from the value.
            (None, _) | (Some(ColumnType::Text), ColumnType::Text | ColumnType::Varchar) => true,
            (Some(actual), wanted) => actual == wanted,
        }
    }
}

/// The sign bit of an IEEE-754 binary64.
const SIGN: u64 = 1 << 63;
