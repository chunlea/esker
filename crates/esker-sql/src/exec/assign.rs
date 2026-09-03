//! A value meeting a column: the assignment cast, or the `42804` that names both types.
//!
//! Three statements put an arbitrary expression's value into a column — an `INSERT`'s `VALUES`
//! tuple, an `UPDATE`'s `SET`, and an `ON CONFLICT DO UPDATE`'s — and all three ask this one
//! question afterwards. Before this module they asked [`Datum::fits`], which is *storage*
//! equality: it answers whether the value is already of the column's type and knows nothing about
//! a cast. That is the right question for a row read back off disk and the wrong one for a value
//! a statement computed, because PostgreSQL puts a `timestamptz` into a `timestamp` column
//! without being asked and refuses a `time` — one accepted, one `42804`, and `fits` says no to
//! both.
//!
//! # What is here is what was measured
//!
//! The datetime family, from `tests/corpus/pg19_values_catalog_function.txt`: `CURRENT_TIMESTAMP`
//! and `CURRENT_DATE` go into a `timestamp` column, `CURRENT_TIME` and `LOCALTIME` do not.
//! Everything else keeps the strict rule until a capture shows otherwise — a cast invented here
//! would be this node accepting a statement a real server refuses, which is the failure ADR 0031
//! ranks above a refusal because nothing reports it.

use esker_keys::value::Datum;

use crate::catalog::ColumnDef;
use crate::error::{Result, SqlError};
use crate::value::{ColumnType, PgType};

/// One value, ready to store in `column`, or the `42804` naming both types.
///
/// The message is PostgreSQL's own, down to the order — the column's type first, the
/// expression's second — and it carries the rewrite-or-cast `HINT` that a real server sends with
/// it (`crate::error`).
pub(super) fn into_column(value: Datum, column: &ColumnDef) -> Result<Datum> {
    if value.fits(column.ty) {
        return Ok(value);
    }
    coerce(&value, column.ty).ok_or_else(|| SqlError::DatatypeMismatchInColumn {
        column: column.name.clone(),
        column_type: column.ty.name(),
        // A NULL fits every column and never reaches here, so a value with no type cannot either.
        expression_type: value.column_type().map_or("unknown", PgType::name),
    })
}

/// PostgreSQL's assignment casts, narrowed to the datetime family this node has measured.
///
/// **The zone is dropped and added rather than shifted**: this node honours `TimeZone` only
/// where it means UTC (`crate::parameter`), so the two timestamp types hold the same
/// microseconds for the same instant and the conversion between them is the identity. The day a
/// session zone means anything else, this is one of the places that has to learn about it — and
/// `crate::value::PgDatum::pg_cmp`'s arm for the same pair is the other.
fn coerce(value: &Datum, ty: ColumnType) -> Option<Datum> {
    Some(match (value, ty) {
        (Datum::TimestampTz(micros), ColumnType::Timestamp) => Datum::Timestamp(*micros),
        (Datum::Timestamp(micros), ColumnType::TimestampTz) => Datum::TimestampTz(*micros),
        // **A date is the midnight it names**, the same promotion `pg_cmp` makes to compare one
        // with a timestamp.
        (Datum::Date(day), ColumnType::Timestamp) => {
            Datum::Timestamp(crate::value::date::as_micros(*day))
        }
        (Datum::Date(day), ColumnType::TimestampTz) => {
            Datum::TimestampTz(crate::value::date::as_micros(*day))
        }
        _ => return None,
    })
}
