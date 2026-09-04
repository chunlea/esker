//! The real [`super::RowDecoder`]: a stored row into typed columns.
//!
//! [ADR 0030](../../../../docs/adr/0030-the-row-value-codec-moves-down.md) moved the row value
//! codec down into `esker-keys`, which is what makes this possible at all: the store can decode a
//! stored row without linking `esker-sql`, which sits *above* it. Until that landed this trait had
//! only a test double (`docs/plans/phase-8-learner.md`, RULED-1).
//!
//! # The schema is a pair, and the second half is the one that bites
//!
//! [`RowSchema`] is `{ types, missing }`. `missing` is PostgreSQL 11's `attmissingval`: the value a
//! column pads with for rows written **before** it existed, which is what makes
//! `ADD COLUMN ... DEFAULT <constant>` instant on a populated table.
//!
//! Building this from types alone — `RowSchema::nullable(types)`, the natural thing to reach for —
//! is wrong in a way nothing loud would catch. For a table that took
//! `ADD COLUMN c int8 NOT NULL DEFAULT 42`, every row written before that `ALTER` would read **42
//! through the row store and NULL here**. Silently, and only for the old rows. So the pair is what
//! this takes and there is no constructor that accepts less.
//!
//! # A stale schema is lag, not corruption
//!
//! `decode_row` **refuses** a row wider than the schema it is read against — that is corruption,
//! not a case to tolerate ([ADR 0019](../../../../docs/adr/0019-a-row-says-how-many-columns-it-has.md)
//! Decision 1). A columnar learner whose schema push has not arrived yet sees exactly that, and it
//! is not corruption there: it is a row written under a schema version this replica has not been
//! told about.
//!
//! Told apart by [`DecodeOutcome`], on the **variant** and never on the message. The apply path
//! may not fetch — a schema lookup on the log's critical path makes apply latency depend on
//! another region's availability, and a lookup that fails stalls the log rather than failing a
//! request — so the region simply stops advancing its applied index until the push lands. That
//! degrades into a state the system already handles end to end: *a learner that cannot apply is a
//! learner that is behind*, which the heartbeat reports and `RefusalReason::TooFarBehind` turns
//! into a row-scan fallback.

use std::fmt;

use esker_columnar::{ColumnDef, Schema, Value};
use esker_keys::row::{RowError, RowSchema, decode_row};
use esker_keys::value::{ColumnType as StoredType, Datum};

use super::RowDecoder;
use crate::error::{Result, StoreError};

/// What a decode attempt meant, when it did not produce a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeOutcome {
    /// The row is wider than this schema, which means this replica has not been told about the
    /// schema version the row was written under. **Wait, do not fetch** — see the module header.
    SchemaBehind,
    /// The bytes are genuinely not a row of any schema.
    Corrupt,
}

/// A decoder for one table, at one schema version.
pub struct TableDecoder {
    /// The columnar schema the run is built from: the table's own columns.
    columns: Schema,
    /// The row codec's view of the same columns, `missing` values included.
    row: RowSchema,
    /// The same `missing` values in the columnar vocabulary, for a read that has to widen an
    /// older run to this schema. Kept beside `row` rather than derived from it because
    /// `RowSchema` does not hand its own back and `esker-keys` is not this crate's to change.
    missing: Vec<Value>,
    /// Which schema version this was built from, so a stale push is refused over a fresh one.
    schema_version: u64,
}

impl fmt::Debug for TableDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TableDecoder")
            .field("columns", &self.columns.len())
            .field("schema_version", &self.schema_version)
            .finish_non_exhaustive()
    }
}

impl TableDecoder {
    /// Builds a decoder from a pushed schema.
    ///
    /// `missing` must be the catalog's, not `None` repeated — see the module header for what
    /// happens otherwise. It is a required argument rather than an `Option` for exactly that
    /// reason: there is no shorter call that quietly does the wrong thing.
    pub fn new(
        names: &[String],
        types: &[StoredType],
        missing: &[Option<Datum>],
        schema_version: u64,
    ) -> Result<Self> {
        if names.len() != types.len() {
            return Err(StoreError::Bootstrap(format!(
                "{} column names for {} types",
                names.len(),
                types.len()
            )));
        }
        // **A table with an array column has no columnar replica**, and it is refused here
        // rather than approximated: the columnar format has no array run, and mapping one onto
        // some other type would give the replica a column whose values are not the table's.
        // The SQL side already routes such a query to the row engine (`exec::fragment`), so this
        // is the second of two doors on the same rule.
        let mut mapped = Vec::with_capacity(types.len());
        for (name, ty) in names.iter().zip(types) {
            let Some(columnar) = columnar_type(*ty) else {
                return Err(StoreError::Bootstrap(format!(
                    "the column {name} is a {ty:?}, which has no columnar representation"
                )));
            };
            mapped.push(ColumnDef::new(name.clone(), columnar));
        }
        let columns = Schema::new(mapped)
            .map_err(|error| StoreError::Bootstrap(format!("the table's columns: {error}")))?;
        Ok(Self {
            columns,
            row: RowSchema::new(types.to_vec(), missing.to_vec()),
            missing: missing
                .iter()
                .map(|datum| datum.as_ref().map_or(Value::Null, value_of))
                .collect(),
            schema_version,
        })
    }

    /// The schema version this decoder was built from.
    #[must_use]
    pub fn schema_version(&self) -> u64 {
        self.schema_version
    }

    /// Classifies a failure without reading its message.
    ///
    /// `RowError::Corrupt` is what a row wider than the schema gives, which is the stale-schema
    /// case; the other two are genuine. Matched on the **variant**, because a message is prose and
    /// prose changes.
    #[must_use]
    pub fn classify(error: &RowError) -> DecodeOutcome {
        match error {
            RowError::Corrupt(_) => DecodeOutcome::SchemaBehind,
            RowError::Mismatch(_) | RowError::InvalidUtf8(_) => DecodeOutcome::Corrupt,
        }
    }
}

impl RowDecoder for TableDecoder {
    fn schema(&self) -> &Schema {
        &self.columns
    }

    fn missing(&self) -> Vec<Value> {
        self.missing.clone()
    }

    /// Decodes the row's data columns. The key is not read here at all: the row value carries
    /// **every** column including the primary key, and a version's identity is the key's own bytes
    /// (see [`super::KEY_COLUMN`]), so nothing in this function needs to parse one.
    fn decode(&self, _key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>> {
        let Some(bytes) = value else {
            // A tombstone: every data column NULL. Its identity is its key, added by the caller.
            return Ok(vec![Value::Null; self.columns.len()]);
        };
        let row = decode_row(&self.row, bytes).map_err(|error| {
            StoreError::Bootstrap(format!("{:?}: {error}", Self::classify(&error)))
        })?;
        Ok(row.iter().map(value_of).collect())
    }
}

/// The storage vocabulary into the columnar one.
///
/// Two enums of the same shapes, which ADR 0030 leaves deliberately unconverged — a dedup for
/// somebody not in the middle of a milestone. A **total** match, so that a seventh type added to
/// either side is a compile error here rather than a column that silently reads NULL.
fn columnar_type(ty: StoredType) -> Option<esker_columnar::ColumnType> {
    Some(match ty {
        StoredType::Int8 => esker_columnar::ColumnType::Int8,
        StoredType::Int4 => esker_columnar::ColumnType::Int4,
        StoredType::Int2 => esker_columnar::ColumnType::Int2,
        StoredType::Real => esker_columnar::ColumnType::Real,
        StoredType::Varchar => esker_columnar::ColumnType::Varchar,
        StoredType::Bpchar => esker_columnar::ColumnType::Bpchar,
        StoredType::Json => esker_columnar::ColumnType::Json,
        StoredType::Jsonb => esker_columnar::ColumnType::Jsonb,
        StoredType::Text => esker_columnar::ColumnType::Text,
        StoredType::Bool => esker_columnar::ColumnType::Bool,
        StoredType::Bytea => esker_columnar::ColumnType::Bytea,
        StoredType::TimestampTz => esker_columnar::ColumnType::TimestampTz,
        StoredType::Timestamp => esker_columnar::ColumnType::Timestamp,
        StoredType::Double => esker_columnar::ColumnType::Double,
        StoredType::Date => esker_columnar::ColumnType::Date,
        StoredType::Numeric => esker_columnar::ColumnType::Numeric,
        StoredType::Time => esker_columnar::ColumnType::Time,
        StoredType::Uuid => esker_columnar::ColumnType::Uuid,
        StoredType::Interval => esker_columnar::ColumnType::Interval,
        StoredType::Oid => esker_columnar::ColumnType::Oid,
        StoredType::Int8Array
        | StoredType::Int4Array
        | StoredType::Int2Array
        | StoredType::NumericArray
        | StoredType::TextArray
        // **An hstore column is not columnar**, the same deliberate gap an array column is: it is
        // text-shaped and `esker-columnar` could hold one, but its own `ColumnType` is a separate
        // enum and teaching it a type is that crate's unit. A table with one routes to the row
        // engine, which is correct and slower — and the `Option` here is what says so out loud.
        | StoredType::Hstore
        | StoredType::HstoreArray
        // Not columnar either, and for a sharper reason than hstore's: a citext's comparison is
        // not its bytes', so a columnar run that filtered or sorted one would have to fold, and
        // that crate's `ColumnType` has no way to say so.
        | StoredType::Citext
        // A range is not columnar either: its ordering is not its text's, so a columnar run could
        // not sort or filter one without the comparison this vocabulary has no way to carry.
        | StoredType::TsRange
        | StoredType::TstzRange
        | StoredType::Int4Range
        | StoredType::TsRangeArray
        | StoredType::DateRange
        | StoredType::NumRange
        | StoredType::Int8Range
        | StoredType::Money
        | StoredType::MoneyArray
        | StoredType::Inet
        | StoredType::Cidr
        | StoredType::MacAddr
        | StoredType::InetArray
        | StoredType::CidrArray
        | StoredType::MacAddrArray
        | StoredType::Bit
        | StoredType::VarBit
        | StoredType::BitArray
        | StoredType::VarBitArray
        | StoredType::Lseg
        | StoredType::Box
        | StoredType::Path
        | StoredType::Polygon
        | StoredType::Circle
        | StoredType::Line
        // **`xml` is not columnar**, for `json`'s reason without `json`'s exception: that crate's
        // `ColumnType` has a `Json` and no `Xml`, and teaching it one is that crate's unit.
        | StoredType::Xml
        | StoredType::XmlArray
        // Nor is `ltree`, for `citext`'s reason exactly: its comparison is not its bytes', and
        // this vocabulary has no way to carry the one it has.
        | StoredType::Ltree
        | StoredType::LtreeArray
        | StoredType::LQuery
        | StoredType::FloatRange
        | StoredType::VarcharRange
        | StoredType::TstzRangeArray
        | StoredType::Int4RangeArray
        | StoredType::DateRangeArray
        | StoredType::NumRangeArray
        | StoredType::Int8RangeArray
        | StoredType::Point
        | StoredType::PointArray | StoredType::BoolArray | StoredType::ByteaArray | StoredType::BpcharArray | StoredType::VarcharArray | StoredType::DateArray | StoredType::TimeArray | StoredType::TimestampArray | StoredType::TimestampTzArray | StoredType::IntervalArray | StoredType::RealArray | StoredType::DoubleArray | StoredType::UuidArray | StoredType::JsonArray | StoredType::JsonbArray | StoredType::OidArray | StoredType::CitextArray => return None,
    })
}

/// Likewise for a value. Total, for the same reason.
///
/// An array never reaches it: [`columnar_type`] refuses the column before a decoder exists.
fn value_of(datum: &Datum) -> Value {
    match datum {
        // A citext never reaches this either — the column is refused above, for the same reason
        // an array is: this vocabulary has no way to carry a comparison that folds.
        // A point never reaches this either: `columnar_type` refuses the column, the same way
        // it refuses a citext and an array.
        Datum::Point { .. }
        | Datum::Money(_)
        | Datum::Inet { .. }
        | Datum::MacAddr(_)
        | Datum::Bit { .. }
        | Datum::Geometry { .. }
        | Datum::Ltree(_)
        | Datum::Null
        | Datum::Array(_)
        | Datum::Citext(_)
        | Datum::Hstore(_)
        | Datum::Range { .. } => Value::Null,
        Datum::Int8(int) => Value::Int8(*int),
        Datum::Int4(int) => Value::Int4(*int),
        Datum::Int2(int) => Value::Int2(*int),
        Datum::Real(real) => Value::Real(*real),
        Datum::Text(text) => Value::Text(text.clone()),
        Datum::Bool(flag) => Value::Bool(*flag),
        Datum::Bytea(bytes) => Value::Bytea(bytes.clone()),
        Datum::TimestampTz(ts) => Value::TimestampTz(*ts),
        Datum::Timestamp(ts) => Value::Timestamp(*ts),
        Datum::Double(double) => Value::Double(*double),
        Datum::Date(day) => Value::Date(*day),
        // Its text, which is lossless for this type: a `numeric`'s scale is in its digits.
        Datum::Numeric(value) => Value::Numeric(esker_keys::numeric::to_text(value)),
        Datum::Time(micros) => Value::Time(*micros),
        Datum::Uuid(bytes) => Value::Uuid(*bytes),
        Datum::Oid(v) => Value::Oid(*v),
        // The three fields in the layout the row codec writes, which is what the columnar carries.
        Datum::Interval {
            months,
            days,
            micros,
        } => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&months.to_le_bytes());
            bytes[4..8].copy_from_slice(&days.to_le_bytes());
            bytes[8..].copy_from_slice(&micros.to_le_bytes());
            Value::Interval(bytes)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use esker_columnar::Value;
    use esker_keys::row::{RowError, RowSchema, encode_row};
    use esker_keys::value::{ColumnType as StoredType, Datum};

    use super::{DecodeOutcome, TableDecoder};
    use crate::columnar::RowDecoder;

    fn names(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("c{index}")).collect()
    }

    /// `id int8` and `name text`, keyed on `id`.
    fn two_column() -> TableDecoder {
        TableDecoder::new(
            &["id".into(), "name".into()],
            &[StoredType::Int8, StoredType::Text],
            &[None, None],
            1,
        )
        .unwrap()
    }

    /// Any key at all: [`TableDecoder::decode`] does not read it. Identity is the apply target's
    /// `__key` column, which is the key's own bytes.
    fn key_of(id: i64) -> Vec<u8> {
        id.to_be_bytes().to_vec()
    }

    #[test]
    fn a_row_decodes_into_its_columns() {
        let decoder = two_column();
        let value = encode_row(
            &[StoredType::Int8, StoredType::Text],
            &[Datum::Int8(7), Datum::Text("seven".into())],
        )
        .unwrap();
        assert_eq!(
            decoder.decode(&key_of(7), Some(&value)).unwrap(),
            vec![Value::Int8(7), Value::Text("seven".into())]
        );
    }

    /// A tombstone has no value, so every data column is NULL. Its identity does not come from
    /// here at all: it is the key's own bytes, which the apply target adds.
    #[test]
    fn a_tombstone_keeps_its_key_and_nothing_else() {
        let decoder = two_column();
        assert_eq!(
            decoder.decode(&key_of(9), None).unwrap(),
            vec![Value::Null, Value::Null],
            "a tombstone's data columns are all NULL; its identity is its key"
        );
    }

    /// **The bug the harness was blind to, given its own regression at last.**
    ///
    /// After `ADD COLUMN c int8 NOT NULL DEFAULT 42`, a row written *before* the `ALTER` is two
    /// columns wide against a three-column schema. `decode_row` pads it with the column's
    /// `missing` value — PostgreSQL 11's `attmissingval` — so the row store reads `42`. A decoder
    /// built from types alone pads NULL, and the columnar copy would answer `NULL` where the row
    /// store answers `42`: silently, and only for rows older than the `ALTER`, which `row.rs` says
    /// of itself is "the hardest case to notice".
    #[test]
    fn a_row_predating_an_added_column_reads_its_default_and_not_null() {
        let old_row = encode_row(
            &[StoredType::Int8, StoredType::Text],
            &[Datum::Int8(1), Datum::Text("before".into())],
        )
        .unwrap();

        // The schema after the ALTER: three columns, the third padding 42.
        let widened = TableDecoder::new(
            &["id".into(), "name".into(), "c".into()],
            &[StoredType::Int8, StoredType::Text, StoredType::Int8],
            &[None, None, Some(Datum::Int8(42))],
            2,
        )
        .unwrap();
        assert_eq!(
            widened.decode(&key_of(1), Some(&old_row)).unwrap(),
            vec![
                Value::Int8(1),
                Value::Text("before".into()),
                Value::Int8(42)
            ],
            "a row older than the ALTER did not read the column's default"
        );

        // And the wrong construction, kept as the contrast: types alone pads NULL.
        let naive = TableDecoder::new(
            &["id".into(), "name".into(), "c".into()],
            &[StoredType::Int8, StoredType::Text, StoredType::Int8],
            &[None, None, None],
            2,
        )
        .unwrap();
        assert_eq!(
            naive.decode(&key_of(1), Some(&old_row)).unwrap()[2],
            Value::Null,
            "this is what building from types alone costs, and why `missing` is required"
        );
    }

    /// A row **wider** than the schema is this replica being behind, not corruption — told apart
    /// on the variant, never on the message.
    #[test]
    fn a_row_wider_than_the_schema_is_schema_lag() {
        assert_eq!(
            TableDecoder::classify(&RowError::Corrupt("row is wider".into())),
            DecodeOutcome::SchemaBehind
        );
        assert_eq!(
            TableDecoder::classify(&RowError::InvalidUtf8(0xff)),
            DecodeOutcome::Corrupt
        );
        assert_eq!(
            TableDecoder::classify(&RowError::Mismatch("a caller bug".into())),
            DecodeOutcome::Corrupt
        );

        // And it really is what a wide row produces, rather than only what this build calls it.
        let narrow = RowSchema::new(vec![StoredType::Int8], vec![None]);
        let wide = encode_row(
            &[StoredType::Int8, StoredType::Int8],
            &[Datum::Int8(1), Datum::Int8(2)],
        )
        .unwrap();
        let error = esker_keys::row::decode_row(&narrow, &wide).unwrap_err();
        assert_eq!(TableDecoder::classify(&error), DecodeOutcome::SchemaBehind);
    }

    #[test]
    fn a_mismatched_name_count_is_refused_at_construction() {
        let error = TableDecoder::new(&names(3), &[StoredType::Int8], &[None], 1)
            .expect_err("three names for one type");
        assert!(error.to_string().contains("column names"), "{error}");
    }
}
