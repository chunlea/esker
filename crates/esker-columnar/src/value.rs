//! The six types a column can hold, and the schema that names them.
//!
//! **The type set is the row side's, exactly.** `esker_sql::value::ColumnType` has six variants
//! and a columnar copy of a row can hold no more than the row could. Inventing a richer model
//! here would produce encodings nothing can ever write, which still have to be fuzzed, golden
//! tested and maintained.
//!
//! The tag bytes are the row side's too — `1` int8, `2` text, `3` bool, `4` bytea, `5`
//! timestamptz, `6` double, as `esker_sql::catalog::record` writes them. Sharing the numbering is
//! free, keeps one type set to one spelling on disk, and costs no dependency: the two crates are
//! not linked and [`tags_match_the_row_side`](self) is the test that says so out loud. What the
//! two do *not* share is PostgreSQL's OIDs, which belong on the wire.
//!
//! `Text` and `Bytea` differ in exactly one respect down here: a `Text` value is required to be
//! UTF-8 and is validated on the way out of a decoder. That check is not decoration — a `String`
//! built from unchecked bytes is how a corrupt file turns into undefined behaviour further up.

use std::cmp::Ordering;

use crate::error::{Error, Result};

/// The order this system puts two doubles in, which is **not** IEEE's.
///
/// PostgreSQL's `float8` ordering, mirrored from `esker_sql::value::Datum::pg_cmp` and confirmed
/// there against a real server: **`NaN` is greater than every other value, `Infinity` included,
/// and equal to itself**, and `-0.0` equals `0.0`. It is the ordering `WHERE x > 5` uses, so a
/// `NaN` row really does match that predicate — which is exactly why statistics computed under
/// IEEE's rules cannot be pruned with here.
///
/// Kept as a function of its own rather than inlined, because it is the *specification* two
/// independent implementations have to share: the evaluator compares with it and the differential
/// harness's reference interpreter does too.
#[must_use]
pub fn pg_cmp_f64(left: f64, right: f64) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        // Neither is NaN, so the comparison is total; `-0.0 == 0.0` falls out of IEEE equality.
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

/// The same ordering one width down, for [`ColumnType::Real`].
#[must_use]
pub fn pg_cmp_f32(left: f32, right: f32) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        // Neither is NaN, so the comparison is total; `-0.0 == 0.0` falls out of IEEE equality.
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

/// One of the types a row carries (`esker_sql::value::ColumnType`), mirrored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnType {
    /// 64-bit signed integer; PostgreSQL's `bigint`.
    Int8,
    /// 32-bit signed integer; PostgreSQL's `integer`. A **distinct type** and not an `Int8` that
    /// happens to be small ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Int4,
    /// 16-bit signed integer; PostgreSQL's `smallint`. Distinct for the same reason.
    Int2,
    /// Variable-length UTF-8 string.
    Text,
    /// PostgreSQL's `character varying`: the same representation as [`ColumnType::Text`] and a
    /// different type, which is PostgreSQL's own model.
    Varchar,
    /// Two-valued, with no third state but NULL.
    Bool,
    /// Variable-length byte string.
    Bytea,
    /// An instant, microseconds from 2000-01-01 UTC. Stored exactly as an `Int8` is.
    TimestampTz,
    /// PostgreSQL's `timestamp` without time zone: the same eight bytes and a different type.
    Timestamp,
    /// IEEE-754 binary64.
    Double,
    /// IEEE-754 binary32; PostgreSQL's `real`.
    Real,
    /// PostgreSQL's `date`: a day, as a signed count from 2000-01-01 in four bytes.
    Date,
    /// PostgreSQL's `character(n)`, whose internal name is `bpchar`. The same bytes as a `Text`
    /// again; what differs is that its values arrive **already padded** to the column's length, so
    /// byte comparison is the blank-insensitive comparison PostgreSQL specifies.
    Bpchar,
    /// PostgreSQL's `json`: validated text, stored exactly as sent.
    Json,
    /// PostgreSQL's `jsonb`: the canonical text it prints as. Its equality is not its byte
    /// equality, which is why it is not a key type (ADR 0042) — but it is an ordinary `Bytes` run
    /// here, because a column is not a key.
    Jsonb,
}

impl ColumnType {
    /// Every type, for tests that must not silently skip one.
    pub const ALL: [ColumnType; 15] = [
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
        ColumnType::Bpchar,
        ColumnType::Json,
        ColumnType::Jsonb,
        ColumnType::Date,
    ];

    /// The tag byte this type is stored as. Frozen: see the module docs.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            ColumnType::Int8 => 1,
            ColumnType::Text => 2,
            ColumnType::Bool => 3,
            ColumnType::Bytea => 4,
            ColumnType::TimestampTz => 5,
            ColumnType::Double => 6,
            // Appended, never renumbered: an old file has no tag above 6 and reads unchanged.
            ColumnType::Int4 => 7,
            ColumnType::Varchar => 8,
            ColumnType::Timestamp => 9,
            ColumnType::Int2 => 10,
            ColumnType::Real => 11,
            ColumnType::Bpchar => 12,
            ColumnType::Json => 13,
            ColumnType::Jsonb => 14,
            ColumnType::Date => 15,
        }
    }

    /// The type a tag byte names, or corruption for one no version has ever written.
    pub fn from_tag(tag: u8) -> Result<Self> {
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
            other => {
                return Err(Error::corruption(
                    "schema",
                    format!("column type tag {other}"),
                ));
            }
        })
    }

    /// The name PostgreSQL uses when it talks about the type, for error messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ColumnType::Int8 => "bigint",
            ColumnType::Int4 => "integer",
            ColumnType::Int2 => "smallint",
            ColumnType::Varchar => "character varying",
            ColumnType::Text => "text",
            ColumnType::Bool => "boolean",
            ColumnType::Bytea => "bytea",
            ColumnType::TimestampTz => "timestamp with time zone",
            ColumnType::Timestamp => "timestamp without time zone",
            ColumnType::Double => "double precision",
            ColumnType::Real => "real",
            ColumnType::Bpchar => "character",
            ColumnType::Json => "json",
            ColumnType::Jsonb => "jsonb",
            ColumnType::Date => "date",
        }
    }

    /// Whether values of this type are stored as a run of bytes rather than a fixed width.
    #[must_use]
    pub fn is_variable_length(self) -> bool {
        matches!(
            self,
            ColumnType::Text
                | ColumnType::Varchar
                | ColumnType::Bpchar
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Bytea
        )
    }
}

/// One column of a value, owned. What the writer accepts and what a decoded row yields.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// No value. A NULL costs one bit in the mask and no bytes among the values.
    Null,
    /// An [`ColumnType::Int8`].
    Int8(i64),
    /// An [`ColumnType::Int4`].
    Int4(i32),
    /// An [`ColumnType::Int2`].
    Int2(i16),
    /// A [`ColumnType::Text`], already valid UTF-8 by construction.
    Text(String),
    /// A [`ColumnType::Bool`].
    Bool(bool),
    /// A [`ColumnType::Bytea`].
    Bytea(Vec<u8>),
    /// A [`ColumnType::TimestampTz`], microseconds from 2000-01-01 UTC.
    TimestampTz(i64),
    /// A [`ColumnType::Timestamp`], microseconds from 2000-01-01.
    Timestamp(i64),
    /// A [`ColumnType::Double`].
    Double(f64),
    /// A [`ColumnType::Real`].
    Real(f32),
    /// A [`ColumnType::Date`], days from 2000-01-01.
    Date(i32),
}

impl Value {
    /// Whether this value may be stored in a column of type `ty`. NULL fits every type.
    #[must_use]
    pub fn fits(&self, ty: ColumnType) -> bool {
        match self {
            Value::Null => true,
            Value::Int8(_) => ty == ColumnType::Int8,
            Value::Int4(_) => ty == ColumnType::Int4,
            Value::Int2(_) => ty == ColumnType::Int2,
            // One representation, two types: there is no `Value::Varchar` because there would be
            // nothing in it a `Text` does not hold.
            Value::Text(_) => matches!(
                ty,
                ColumnType::Text
                    | ColumnType::Varchar
                    | ColumnType::Bpchar
                    | ColumnType::Json
                    | ColumnType::Jsonb
            ),
            Value::Bool(_) => ty == ColumnType::Bool,
            Value::Bytea(_) => ty == ColumnType::Bytea,
            Value::TimestampTz(_) => ty == ColumnType::TimestampTz,
            Value::Timestamp(_) => ty == ColumnType::Timestamp,
            Value::Double(_) => ty == ColumnType::Double,
            Value::Real(_) => ty == ColumnType::Real,
            Value::Date(_) => ty == ColumnType::Date,
        }
    }

    /// Whether this is the absence of a value.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The type this value has, or `None` for NULL, which has none and fits every column.
    #[must_use]
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Value::Null => return None,
            Value::Int8(_) => ColumnType::Int8,
            Value::Int4(_) => ColumnType::Int4,
            Value::Int2(_) => ColumnType::Int2,
            Value::Text(_) => ColumnType::Text,
            Value::Bool(_) => ColumnType::Bool,
            Value::Bytea(_) => ColumnType::Bytea,
            Value::TimestampTz(_) => ColumnType::TimestampTz,
            Value::Timestamp(_) => ColumnType::Timestamp,
            Value::Double(_) => ColumnType::Double,
            Value::Real(_) => ColumnType::Real,
            Value::Date(_) => ColumnType::Date,
        })
    }

    /// This value, borrowed.
    #[must_use]
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Value::Null => ValueRef::Null,
            Value::Int8(v) | Value::TimestampTz(v) | Value::Timestamp(v) => ValueRef::Int(*v),
            // A day is an integer to the encoder, the way a timestamp is: the schema says which.
            Value::Int4(v) | Value::Date(v) => ValueRef::Int(i64::from(*v)),
            Value::Int2(v) => ValueRef::Int(i64::from(*v)),
            Value::Bool(v) => ValueRef::Bool(*v),
            Value::Double(v) => ValueRef::Double(*v),
            Value::Real(v) => ValueRef::Real(*v),
            Value::Text(v) => ValueRef::Bytes(v.as_bytes()),
            Value::Bytea(v) => ValueRef::Bytes(v),
        }
    }

    /// [`ValueRef::pg_cmp`], over owned values.
    #[must_use]
    pub fn pg_cmp(&self, other: &Self) -> Ordering {
        self.as_ref().pg_cmp(&other.as_ref())
    }
}

/// One column of a decoded chunk, borrowed from the buffers the reader owns.
///
/// The shape a scan wants: no allocation per value, and `Null` in the same enum as the values so
/// that one `match` is exhaustive over what a row can hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    /// No value.
    Null,
    /// An `Int8` or a `TimestampTz` — the two are the same 64 bits and the schema says which.
    Int(i64),
    /// A `Bool`.
    Bool(bool),
    /// A `Double`.
    Double(f64),
    /// A `Real`, at its own width. **Not** a widened [`ValueRef::Double`], for the reason
    /// `encode::float` gives: the widening is unspecified for a `NaN` payload, and a
    /// scan that answered differently on two targets would not be answering at all.
    Real(f32),
    /// A `Text` (validated UTF-8) or a `Bytea`.
    Bytes(&'a [u8]),
}

impl ValueRef<'_> {
    /// Whether this is the absence of a value.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, ValueRef::Null)
    }

    /// The order this system puts two values in, which is PostgreSQL's and not the bits'.
    ///
    /// Mirrored from `esker_sql::value::Datum::pg_cmp`, whose rules were confirmed against a real
    /// server, and it is the **specification** rather than an implementation detail: the fragment
    /// evaluator compares with it, statistics are computed in it, groups are identified by it, and
    /// the differential harness's reference interpreter uses it too. Three rules are its own:
    ///
    /// * **NULL sorts last** and is equal only to NULL — which is what makes a NULL its own
    ///   `GROUP BY` group. It is *not* what makes `x = NULL` unknown; that is a separate rule, and
    ///   it lives in the evaluator, which never reaches this function with a NULL operand.
    /// * **`NaN` is the largest float**, above `Infinity`, and equal to itself
    ///   ([`pg_cmp_f64`]).
    /// * **Text sorts by bytes**, not by a collation — a declared divergence this project makes
    ///   everywhere (`esker_sql::row`), because a locale-aware collation means linking C.
    ///
    /// Two values of different shapes cannot arise from a validated fragment, but the function is
    /// total anyway: it falls back to a fixed order over the variants so that no input can panic.
    #[must_use]
    pub fn pg_cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (ValueRef::Null, ValueRef::Null) => Ordering::Equal,
            // NULLS LAST, PostgreSQL's default for ascending order.
            (ValueRef::Null, _) => Ordering::Greater,
            (_, ValueRef::Null) => Ordering::Less,
            (ValueRef::Int(a), ValueRef::Int(b)) => a.cmp(b),
            (ValueRef::Bool(a), ValueRef::Bool(b)) => a.cmp(b),
            (ValueRef::Double(a), ValueRef::Double(b)) => pg_cmp_f64(*a, *b),
            (ValueRef::Real(a), ValueRef::Real(b)) => pg_cmp_f32(*a, *b),
            (ValueRef::Bytes(a), ValueRef::Bytes(b)) => a.cmp(b),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }

    /// A fixed order over the shapes, so that [`pg_cmp`](Self::pg_cmp) is total.
    fn rank(&self) -> u8 {
        match self {
            ValueRef::Bool(_) => 0,
            ValueRef::Int(_) => 1,
            ValueRef::Double(_) => 2,
            ValueRef::Real(_) => 3,
            ValueRef::Bytes(_) => 4,
            ValueRef::Null => 5,
        }
    }

    /// This reference as an owned [`Value`] of type `ty`.
    ///
    /// `Text` is validated here rather than trusted: bytes that reached a `String` unchecked are
    /// how a corrupt file becomes a wrong answer somewhere far away.
    pub fn to_value(self, ty: ColumnType) -> Result<Value> {
        Ok(match (self, ty) {
            (ValueRef::Null, _) => Value::Null,
            (ValueRef::Int(v), ColumnType::Int8) => Value::Int8(v),
            (ValueRef::Int(v), ColumnType::TimestampTz) => Value::TimestampTz(v),
            (ValueRef::Int(v), ColumnType::Timestamp) => Value::Timestamp(v),
            // Narrowed back from the widened run it rides in. A value outside `i32` cannot have
            // been written by an `Int4` column, so it is corruption rather than a value to clamp.
            (ValueRef::Int(v), ColumnType::Int4) => {
                Value::Int4(i32::try_from(v).map_err(|_| {
                    Error::corruption("column", format!("an integer column holds {v}"))
                })?)
            }
            (ValueRef::Int(v), ColumnType::Int2) => {
                Value::Int2(i16::try_from(v).map_err(|_| {
                    Error::corruption("column", format!("a smallint column holds {v}"))
                })?)
            }
            (ValueRef::Bool(v), ColumnType::Bool) => Value::Bool(v),
            (ValueRef::Double(v), ColumnType::Double) => Value::Double(v),
            // No narrowing and no check: a `real` is carried at its own width the whole way, so
            // there is no widened value that might not have been written by an `f32` and nothing
            // for a `NaN` payload to be lost to.
            (ValueRef::Real(v), ColumnType::Real) => Value::Real(v),
            // Narrowed back the way an `Int4` is, and for the same reason: a value outside `i32`
            // cannot have been written by a `Date` column.
            (ValueRef::Int(v), ColumnType::Date) => Value::Date(
                i32::try_from(v)
                    .map_err(|_| Error::corruption("column", format!("a date column holds {v}")))?,
            ),
            (ValueRef::Bytes(v), ColumnType::Bytea) => Value::Bytea(v.to_vec()),
            (
                ValueRef::Bytes(v),
                ColumnType::Text
                | ColumnType::Varchar
                | ColumnType::Bpchar
                | ColumnType::Json
                | ColumnType::Jsonb,
            ) => Value::Text(
                std::str::from_utf8(v)
                    .map_err(|error| Error::corruption("text column", error.to_string()))?
                    .to_owned(),
            ),
            (other, ty) => {
                return Err(Error::corruption(
                    "column",
                    format!("a {} column decoded to {other:?}", ty.name()),
                ));
            }
        })
    }
}

/// A column's name and type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// What the column is called. Not interpreted here; carried so a file describes itself.
    pub name: String,
    /// What it holds.
    pub ty: ColumnType,
}

impl ColumnDef {
    /// A column definition.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// The columns of a file, in the order every stripe stores its chunks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    columns: Vec<ColumnDef>,
}

/// Most columns any one file may have.
///
/// A bound so that a corrupt column count is refused rather than allocated, and so wide that
/// nothing legitimate meets it: PostgreSQL itself stops at 1600 columns per table.
pub const MAX_COLUMNS: usize = 4096;

/// Longest column name this format stores, in bytes.
pub const MAX_COLUMN_NAME: usize = 1024;

impl Schema {
    /// A schema, or an error if it is one no file may carry.
    ///
    /// Rejects an empty schema, more than [`MAX_COLUMNS`] columns, a name longer than
    /// [`MAX_COLUMN_NAME`] and a duplicate name. The checks are here rather than at the decoder
    /// so that both sides agree what is representable, and so a writer fails before it has
    /// written bytes nothing could read back.
    pub fn new(columns: Vec<ColumnDef>) -> Result<Self> {
        if columns.is_empty() {
            return Err(Error::InvalidArgument("a schema with no columns".into()));
        }
        if columns.len() > MAX_COLUMNS {
            return Err(Error::InvalidArgument(format!(
                "{} columns, more than the {MAX_COLUMNS} this format addresses",
                columns.len()
            )));
        }
        for (index, column) in columns.iter().enumerate() {
            if column.name.len() > MAX_COLUMN_NAME {
                return Err(Error::InvalidArgument(format!(
                    "column {index}'s name is {} bytes, over the {MAX_COLUMN_NAME} limit",
                    column.name.len()
                )));
            }
            if columns[..index]
                .iter()
                .any(|prior| prior.name == column.name)
            {
                return Err(Error::InvalidArgument(format!(
                    "two columns are named {:?}",
                    column.name
                )));
            }
        }
        Ok(Self { columns })
    }

    /// The columns, in storage order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// How many columns there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether the schema names no columns. Only a default-constructed one does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// The type of column `index`, or an error naming the file's width.
    pub fn column_type(&self, index: usize) -> Result<ColumnType> {
        self.columns
            .get(index)
            .map(|column| column.ty)
            .ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "column {index} of a file that has {}",
                    self.columns.len()
                ))
            })
    }

    /// The index of the column called `name`, if there is one.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::{ColumnDef, ColumnType, MAX_COLUMNS, Schema, Value, ValueRef};

    /// The tags are on disk. They are the row side's, and neither may move.
    #[test]
    fn tags_match_the_row_side() {
        // esker_sql::catalog::record's TAG_* constants, copied deliberately rather than linked.
        assert_eq!(ColumnType::Int8.tag(), 1);
        assert_eq!(ColumnType::Text.tag(), 2);
        assert_eq!(ColumnType::Bool.tag(), 3);
        assert_eq!(ColumnType::Bytea.tag(), 4);
        assert_eq!(ColumnType::TimestampTz.tag(), 5);
        assert_eq!(ColumnType::Double.tag(), 6);
        // Appended by ADR 0033, and the six above them did not move: an old file's tags still
        // name the types they always named, which is what makes a new type an addition rather
        // than a format change.
        assert_eq!(ColumnType::Int4.tag(), 7);
        assert_eq!(ColumnType::Varchar.tag(), 8);
        assert_eq!(ColumnType::Timestamp.tag(), 9);
        assert_eq!(ColumnType::Int2.tag(), 10);
        assert_eq!(ColumnType::Real.tag(), 11);
        assert_eq!(ColumnType::Bpchar.tag(), 12);
        assert_eq!(ColumnType::Date.tag(), 15);

        for ty in ColumnType::ALL {
            assert_eq!(ColumnType::from_tag(ty.tag()).unwrap(), ty);
        }
        assert!(ColumnType::from_tag(0).unwrap_err().is_corruption());
        // One past the last: a reader that meets a tag a newer writer used answers corruption
        // rather than guessing, which is the direction this vocabulary is built to fail in.
        // Derived from `ALL` rather than written, because a literal here goes stale the moment a
        // type is appended — and it has, once per type, which is a test asserting the absence of
        // the very thing the next unit adds.
        assert!(
            ColumnType::from_tag(u8::try_from(ColumnType::ALL.len()).unwrap() + 1)
                .unwrap_err()
                .is_corruption()
        );
    }

    /// A value fits its own type and nothing else — **except** the one pair that is deliberately
    /// two types over one representation.
    ///
    /// `text` and `character varying` are one varlena told apart by OID, which is PostgreSQL's own
    /// model and the reason [`Value`] has no `Varchar` variant: there would be nothing in one that
    /// a `Text` does not already hold ([ADR
    /// 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md)). Every other value is still
    /// exact, and the loop below asserts that rather than loosening for all of them.
    #[test]
    fn a_value_fits_its_own_type_and_only_what_shares_its_representation() {
        let values = [
            (Value::Int8(1), &[ColumnType::Int8][..]),
            (Value::Int4(1), &[ColumnType::Int4][..]),
            (
                Value::Text("a".into()),
                // Three types, one representation — the string family PostgreSQL has, told apart
                // by OID and not by bytes. A `bpchar`'s padding is applied before the value gets
                // here, so what arrives is a `Text` like any other.
                // Five types, one representation. `json` and `jsonb` join the string family in
                // *bytes* and not in comparison, which is what ADR 0042 is about — but `fits` is a
                // question about bytes, so here they belong with the rest.
                &[
                    ColumnType::Text,
                    ColumnType::Varchar,
                    ColumnType::Bpchar,
                    ColumnType::Json,
                    ColumnType::Jsonb,
                ][..],
            ),
            (Value::Bool(true), &[ColumnType::Bool][..]),
            (Value::Bytea(vec![1]), &[ColumnType::Bytea][..]),
            (Value::TimestampTz(1), &[ColumnType::TimestampTz][..]),
            (Value::Timestamp(1), &[ColumnType::Timestamp][..]),
            (Value::Double(1.0), &[ColumnType::Double][..]),
        ];
        for (value, fits) in &values {
            for other in ColumnType::ALL {
                assert_eq!(
                    value.fits(other),
                    fits.contains(&other),
                    "{value:?} vs {other:?}"
                );
            }
            assert!(Value::Null.fits(fits[0]), "NULL fits everything");
        }
        // `timestamp` and `timestamptz` share a *width* and not a representation: eight bytes
        // either way, and a value knows which it is, because they print differently.
        assert!(!Value::Timestamp(1).fits(ColumnType::TimestampTz));
        assert!(!Value::TimestampTz(1).fits(ColumnType::Timestamp));
    }

    #[test]
    fn text_is_validated_on_the_way_out() {
        let bad = [0xff, 0xfe];
        let error = ValueRef::Bytes(&bad)
            .to_value(ColumnType::Text)
            .unwrap_err();
        assert!(error.is_corruption(), "{error}");
        // The same bytes are a perfectly good bytea.
        assert_eq!(
            ValueRef::Bytes(&bad).to_value(ColumnType::Bytea).unwrap(),
            Value::Bytea(bad.to_vec())
        );
    }

    #[test]
    fn a_reference_of_the_wrong_shape_is_corruption_not_a_panic() {
        assert!(
            ValueRef::Int(1)
                .to_value(ColumnType::Text)
                .unwrap_err()
                .is_corruption()
        );
    }

    #[test]
    fn schemas_that_are_not_schemas() {
        assert!(Schema::new(Vec::new()).is_err(), "empty");
        assert!(
            Schema::new(vec![
                ColumnDef::new("a", ColumnType::Int8),
                ColumnDef::new("a", ColumnType::Text),
            ])
            .is_err(),
            "duplicate name"
        );
        assert!(
            Schema::new(vec![ColumnDef::new("x".repeat(2000), ColumnType::Int8)]).is_err(),
            "name too long"
        );
        let wide: Vec<ColumnDef> = (0..=MAX_COLUMNS)
            .map(|i| ColumnDef::new(format!("c{i}"), ColumnType::Int8))
            .collect();
        assert!(Schema::new(wide).is_err(), "too many columns");
    }

    #[test]
    fn a_schema_answers_for_its_columns() {
        let schema = Schema::new(vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("body", ColumnType::Text),
        ])
        .unwrap();
        assert_eq!(schema.len(), 2);
        assert!(!schema.is_empty());
        assert_eq!(schema.index_of("body"), Some(1));
        assert_eq!(schema.index_of("nope"), None);
        assert_eq!(schema.column_type(0).unwrap(), ColumnType::Int8);
        assert!(schema.column_type(2).is_err());
    }
}
