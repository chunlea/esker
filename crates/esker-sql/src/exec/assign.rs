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
pub(super) fn into_column(
    value: Datum,
    column: &ColumnDef,
    user_type: Option<&crate::catalog::TypeDef>,
) -> Result<Datum> {
    // **An enum first, and by its own rule.** The column is an `int2` in the row, so every test
    // below would be about the storage: `'angry'` would be `22P02 invalid input syntax for type
    // smallint` where a real server names the enum and the label it did not have.
    if let Some(def) = user_type {
        return into_enum(value, column, def);
    }
    if value.fits(column.ty) {
        return Ok(value);
    }
    coerce(&value, column.ty).ok_or_else(|| SqlError::DatatypeMismatchInColumn {
        column: column.name.clone(),
        column_type: column.ty.name().to_owned(),
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
    // **An array's cast is its element's cast**, which is PostgreSQL's own rule and the reason
    // this is one arm rather than sixteen: `ARRAY['one','two']` is a `text[]` and a
    // `character varying(255)[]` column takes it, exactly as a bare `'one'` goes into a
    // `varchar` one. The rebuilt value carries the **column's** element type, which is what makes
    // `pg_typeof(tags)` answer `character varying[]` afterwards rather than `text[]`.
    if let (Datum::Array(array), Some(want)) =
        (value, esker_keys::array::ArrayValue::element_of(ty))
    {
        if array.element == want {
            return None;
        }
        let mut coerced = array.clone();
        coerced.element = want;
        for element in &mut coerced.values {
            if let Some(datum) = element.take() {
                *element = Some(if datum.fits(want) {
                    datum
                } else {
                    coerce(&datum, want)?
                });
            }
        }
        return Some(Datum::Array(coerced));
    }
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
        // **The other direction, and it is an *assignment* cast only.** `pg_cast.castcontext` is
        // `'a'` for both timestamp types to `date`: allowed when a value is assigned to a column,
        // not when two values are combined — which is why it lives here and not in the promotion
        // table `crate::value::arith` uses. `insert_all` is what needs it: `ActiveRecord` writes
        // one `CURRENT_TIMESTAMP` into `created_at`, `updated_at` **and** `updated_on`, and the
        // last of those is a `t.date`.
        //
        // A one-way relation, so it cannot be modelled as "these types are compatible": the
        // reverse is `'i'`, implicit, and is the two arms above.
        (Datum::Timestamp(micros) | Datum::TimestampTz(micros), ColumnType::Date) => {
            Datum::Date(crate::value::date::from_micros(*micros)?)
        }
        _ => return None,
    })
}

/// The enum a column was declared as, or `None` for a column that is not one.
///
/// The labels are on the `TableDef` rather than on the column, because the type is its own catalog
/// record and the column stores only its oid — see `crate::catalog::TableDef::enums`, which is
/// where the read happens and where the "only when a column has one" guard lives.
pub(super) fn enum_of<'a>(
    table: &'a crate::catalog::TableDef,
    column: &ColumnDef,
) -> Option<&'a crate::catalog::TypeDef> {
    let def = user_type_of(table, column)?;
    matches!(def.kind, crate::catalog::TypeKind::Enum { .. }).then_some(def)
}

/// The user-defined type a column was declared as, **whatever kind it is**.
///
/// [`enum_of`] narrows this to enums, and the two are not interchangeable: an enum is the kind
/// whose *values* are rewritten — a label in, an ordinal stored, a label out — and everything
/// keyed on that must ask the narrow question. A user-defined **range** stores its own value
/// unchanged, and what it needs is only the type's *name and oid*: `pg_typeof`, the
/// `RowDescription` a client picks its decoder from, and `information_schema`. Asking the enum
/// question there answered `floatrange` with `float8range`, the storage — ADR 0031's worst class,
/// a wrong value where the right one was one lookup away.
pub(super) fn user_type_of<'a>(
    table: &'a crate::catalog::TableDef,
    column: &ColumnDef,
) -> Option<&'a crate::catalog::TypeDef> {
    table.enums.get(&column.user_type?)
}

/// A **literal** meeting an enum column, as the value it stands for.
///
/// Only an `unknown` — a quoted string — is a label; everything else keeps its own type and lands
/// on [`into_enum`]'s `42804`. Reading them all as text would turn `VALUES (1)` into the label
/// `"1"` and answer `22P02` where a real server says
/// `column "current_mood" is of type mood but expression is of type integer`, which is a different
/// error about a different mistake.
pub(super) fn enum_literal(literal: &crate::plan::Literal) -> Datum {
    use crate::plan::Literal;
    match literal {
        Literal::Null | Literal::TypedNull(_) => Datum::Null,
        Literal::String(text) => Datum::Text(text.clone()),
        Literal::Typed(value) => (**value).clone(),
        // A bare integer constant is an `int8` here and an `integer` there — the standing
        // constant-width divergence — and either way it is not a label.
        Literal::Integer(value) => Datum::Int8(*value),
        Literal::Decimal(digits) => Datum::Double(digits.parse().unwrap_or(f64::NAN)),
        Literal::Bool(flag) => Datum::Bool(*flag),
    }
}

/// One value, ready to store in a column declared as an **enum**: the ordinal of the label it
/// names.
///
/// Three answers, all measured against 19beta1:
///
/// * a label becomes its **ordinal** — an `int2`, the label's position, which is what makes the
///   ordering `ORDER BY current_mood` gives declaration order rather than the alphabet;
/// * a string that is not one of the labels is
///   **`22P02 invalid input value for enum mood: "angry"`** — the input-syntax class, and the same
///   error from an `INSERT`, an `UPDATE` and a bare cast, which is why all three arrive here;
/// * anything that is not a string at all is
///   **`42804 column "current_mood" is of type mood but expression is of type integer`**, the
///   sentence every other type's mismatch already uses, with the type's own name in it.
///
/// A NULL is a NULL, as it is for every column.
pub(super) fn into_enum(
    value: Datum,
    column: &ColumnDef,
    def: &crate::catalog::TypeDef,
) -> Result<Datum> {
    let crate::catalog::TypeKind::Enum { labels } = &def.kind else {
        return Err(SqlError::Internal(
            "a column carrying a user type that is not an enum reached the write path".to_owned(),
        ));
    };
    match value {
        Datum::Null => Ok(Datum::Null),
        // Text, and an `unknown` literal arrives as text too — which is what makes the *unquoted*
        // `current_mood = 'sad'` work where `current_mood = 'sad'::text` is `42883` on a real
        // server: one is a literal waiting for a type and the other is already a `text`.
        Datum::Text(text) => match crate::catalog::enum_ordinal(labels, &text) {
            Some(ordinal) => Ok(Datum::Int2(ordinal)),
            None => Err(SqlError::InvalidEnumValue {
                ty: def.name.clone(),
                value: text,
            }),
        },
        other => Err(SqlError::DatatypeMismatchInColumn {
            column: column.name.clone(),
            column_type: def.name.clone(),
            expression_type: other.column_type().map_or("unknown", PgType::name),
        }),
    }
}

/// The label an enum column's stored ordinal names, for a value on its way **out**.
///
/// The inverse of [`into_enum`], and the reason a client never sees the `int2`: what is stored is
/// the position, what is sent is the label. An ordinal no label has answers NULL rather than a
/// neighbouring label — see `crate::catalog::enum_label`, and ADR 0050's never-reuse rule, which
/// is what keeps that case unreachable for a value this node wrote.
#[must_use]
pub(super) fn from_enum(value: &Datum, def: &crate::catalog::TypeDef) -> Datum {
    let crate::catalog::TypeKind::Enum { labels } = &def.kind else {
        return value.clone();
    };
    match value {
        Datum::Int2(ordinal) => match crate::catalog::enum_label(labels, *ordinal) {
            Some(label) => Datum::Text(label.to_owned()),
            None => Datum::Null,
        },
        other => other.clone(),
    }
}
