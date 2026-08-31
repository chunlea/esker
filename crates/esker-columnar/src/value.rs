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

use crate::error::{Error, Result};

/// One of the six types a row carries (`esker_sql::value::ColumnType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnType {
    /// 64-bit signed integer; PostgreSQL's `bigint`.
    Int8,
    /// Variable-length UTF-8 string.
    Text,
    /// Two-valued, with no third state but NULL.
    Bool,
    /// Variable-length byte string.
    Bytea,
    /// An instant, microseconds from 2000-01-01 UTC. Stored exactly as an `Int8` is.
    TimestampTz,
    /// IEEE-754 binary64.
    Double,
}

impl ColumnType {
    /// Every type, for tests that must not silently skip one.
    pub const ALL: [ColumnType; 6] = [
        ColumnType::Int8,
        ColumnType::Text,
        ColumnType::Bool,
        ColumnType::Bytea,
        ColumnType::TimestampTz,
        ColumnType::Double,
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
            ColumnType::Text => "text",
            ColumnType::Bool => "boolean",
            ColumnType::Bytea => "bytea",
            ColumnType::TimestampTz => "timestamp with time zone",
            ColumnType::Double => "double precision",
        }
    }

    /// Whether values of this type are stored as a run of bytes rather than a fixed width.
    #[must_use]
    pub fn is_variable_length(self) -> bool {
        matches!(self, ColumnType::Text | ColumnType::Bytea)
    }
}

/// One column of a value, owned. What the writer accepts and what a decoded row yields.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// No value. A NULL costs one bit in the mask and no bytes among the values.
    Null,
    /// An [`ColumnType::Int8`].
    Int8(i64),
    /// A [`ColumnType::Text`], already valid UTF-8 by construction.
    Text(String),
    /// A [`ColumnType::Bool`].
    Bool(bool),
    /// A [`ColumnType::Bytea`].
    Bytea(Vec<u8>),
    /// A [`ColumnType::TimestampTz`], microseconds from 2000-01-01 UTC.
    TimestampTz(i64),
    /// A [`ColumnType::Double`].
    Double(f64),
}

impl Value {
    /// Whether this value may be stored in a column of type `ty`. NULL fits every type.
    #[must_use]
    pub fn fits(&self, ty: ColumnType) -> bool {
        match self {
            Value::Null => true,
            Value::Int8(_) => ty == ColumnType::Int8,
            Value::Text(_) => ty == ColumnType::Text,
            Value::Bool(_) => ty == ColumnType::Bool,
            Value::Bytea(_) => ty == ColumnType::Bytea,
            Value::TimestampTz(_) => ty == ColumnType::TimestampTz,
            Value::Double(_) => ty == ColumnType::Double,
        }
    }

    /// Whether this is the absence of a value.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
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
    /// A `Text` (validated UTF-8) or a `Bytea`.
    Bytes(&'a [u8]),
}

impl ValueRef<'_> {
    /// Whether this is the absence of a value.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, ValueRef::Null)
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
            (ValueRef::Bool(v), ColumnType::Bool) => Value::Bool(v),
            (ValueRef::Double(v), ColumnType::Double) => Value::Double(v),
            (ValueRef::Bytes(v), ColumnType::Bytea) => Value::Bytea(v.to_vec()),
            (ValueRef::Bytes(v), ColumnType::Text) => Value::Text(
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

        for ty in ColumnType::ALL {
            assert_eq!(ColumnType::from_tag(ty.tag()).unwrap(), ty);
        }
        assert!(ColumnType::from_tag(0).unwrap_err().is_corruption());
        assert!(ColumnType::from_tag(7).unwrap_err().is_corruption());
    }

    #[test]
    fn a_value_fits_its_own_type_and_no_other() {
        let values = [
            (Value::Int8(1), ColumnType::Int8),
            (Value::Text("a".into()), ColumnType::Text),
            (Value::Bool(true), ColumnType::Bool),
            (Value::Bytea(vec![1]), ColumnType::Bytea),
            (Value::TimestampTz(1), ColumnType::TimestampTz),
            (Value::Double(1.0), ColumnType::Double),
        ];
        for (value, ty) in &values {
            assert!(value.fits(*ty));
            for other in ColumnType::ALL {
                assert_eq!(value.fits(other), other == *ty, "{value:?} vs {other:?}");
            }
            assert!(Value::Null.fits(*ty), "NULL fits everything");
        }
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
