//! Binary arithmetic: what `+ - * / % ^` yield, and on which types.
//!
//! Two questions, answered in one place because they have to agree. **What type does
//! `left <op> right` have** is decided before any row is read — a client is told the column's OID
//! in the row description — and **what value does it have** is decided per row. A promotion rule
//! that disagrees with the evaluator is a column whose declared type is not the type of its
//! values, which is worse than either being wrong on its own.
//!
//! # PostgreSQL has no mixed operators; it has implicit casts
//!
//! There is no `int4 + float4` on a real server. The parser finds `float8 + float8` and casts both
//! sides to it, which is why `2::int4 + 3::float4` is a **double precision** and
//! `2::float4 + 3::float4` is a `real`. The table in [`result_type`] is that resolution written
//! out, not a widening ladder: a ladder ranks `float4` above `int4` and answers `real`.
//!
//! # Strictness comes before every other rule
//!
//! `NULL::int4 / 0` is **NULL**, not `22012`. An operator with a NULL operand is never evaluated,
//! so the divisor is never looked at. Checking for a zero divisor before checking for NULL raises
//! where PostgreSQL returns a row — measured, and the reason the NULL arm in [`apply`] is first.

use crate::error::{Result, SqlError};
use crate::plan::ArithOp;
use crate::value::{ColumnType, PgDatum as _, PgType};
use esker_keys::value::Datum;

/// The type `left <op> right` has, or the `42883` PostgreSQL raises when no operator exists.
///
/// `^` is the exception to everything: it yields `double precision` whatever it is given, so
/// `2::int4 ^ 3::int4` is a float — measured.
pub fn result_type(op: ArithOp, left: ColumnType, right: ColumnType) -> Result<ColumnType> {
    let undefined = || {
        Err(SqlError::UndefinedOperator {
            left: left.name(),
            op: op.symbol(),
            right: right.name(),
        })
    };
    if !numeric_type(left) || !numeric_type(right) {
        // **A `42883` says the operator does not exist and a `0A000` says this node has not built
        // it**, and the difference is which of the two is true. PostgreSQL has `date - date`,
        // `date + interval` and `time * 2`; it has no `boolean + integer`. Claiming the first
        // kind does not exist would be a wrong answer about a real server.
        if temporal_type(left) || temporal_type(right) {
            return Err(SqlError::unsupported(format!(
                "the operator {} over {} and {}",
                op.symbol(),
                left.name(),
                right.name()
            )));
        }
        return undefined();
    }
    // `%` exists for the integers and for `numeric`, and **not for the floats** — `7::float8 %
    // 2::float8` is `42883 operator does not exist: double precision % double precision`, which is
    // a refusal a real server makes and not one this node invents.
    if op == ArithOp::Modulo && (float_type(left) || float_type(right)) {
        return undefined();
    }
    if op == ArithOp::Power {
        // `numeric ^ numeric` is the one form that stays exact; everything else resolves to the
        // `float8` operator, integers included.
        return Ok(
            if left == ColumnType::Numeric && right == ColumnType::Numeric {
                ColumnType::Numeric
            } else {
                ColumnType::Double
            },
        );
    }
    Ok(match (left, right) {
        // Two floats of the same width keep it; a `float4` beside anything wider becomes a
        // `float8`, which is also what an integer beside a `float4` becomes.
        (ColumnType::Real, ColumnType::Real) => ColumnType::Real,
        (ColumnType::Double | ColumnType::Real, _) | (_, ColumnType::Double | ColumnType::Real) => {
            ColumnType::Double
        }
        (ColumnType::Numeric, _) | (_, ColumnType::Numeric) => ColumnType::Numeric,
        (ColumnType::Int8, _) | (_, ColumnType::Int8) => ColumnType::Int8,
        (ColumnType::Int4, _) | (_, ColumnType::Int4) => ColumnType::Int4,
        _ => ColumnType::Int2,
    })
}

/// Whether a type has arithmetic at all.
fn numeric_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Numeric
    )
}

/// Whether a type has arithmetic on a real server that this node has not built yet.
fn temporal_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Date
            | ColumnType::Time
            | ColumnType::Timestamp
            | ColumnType::TimestampTz
            | ColumnType::Interval
    )
}

fn float_type(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Real | ColumnType::Double)
}

/// `left <op> right`, both already converted to `ty` — [`result_type`]'s answer.
///
/// The NULL arm is first and that is load-bearing; see the module note.
pub fn apply(op: ArithOp, ty: ColumnType, left: &Datum, right: &Datum) -> Result<Datum> {
    if matches!(left, Datum::Null) || matches!(right, Datum::Null) {
        return Ok(Datum::Null);
    }
    // An `unknown` reaches here as text — `'5' + 1` is 6 on a real server — and is read by the
    // result type's own input function, so `'a' + 1` is that function's `22P02` and not a
    // `42883` about an operator on `text`.
    let (left, right) = (&coerce(left, ty)?, &coerce(right, ty)?);
    match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
            integer(op, as_i64(left)?, as_i64(right)?, ty)
        }
        ColumnType::Real => float4(op, as_f64(left)?, as_f64(right)?),
        ColumnType::Double => float8(op, as_f64(left)?, as_f64(right)?),
        other => Err(SqlError::unsupported(format!(
            "arithmetic over {}",
            other.name()
        ))),
    }
}

/// The integer forms, at the **declared width**: `32767::int2 + 1` is `22003 smallint out of
/// range` and the same sum in an `int4` is 32768.
///
/// Rust's `/` truncates toward zero and its `%` takes the sign of the dividend, which is what
/// PostgreSQL does — `-7 / 2` is -3 and `-7 % 3` is -1, measured both ways round. So the operators
/// are used as they are rather than corrected.
fn integer(op: ArithOp, left: i64, right: i64, ty: ColumnType) -> Result<Datum> {
    let overflow = || SqlError::IntegerLiteralOutOfRange(ty.name());
    if matches!(op, ArithOp::Divide | ArithOp::Modulo) && right == 0 {
        return Err(SqlError::DivisionByZero);
    }
    let value = match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Subtract => left.checked_sub(right),
        ArithOp::Multiply => left.checked_mul(right),
        // `checked_div` is `None` for `i64::MIN / -1`, which is the overflow a real server reports
        // as `bigint out of range` rather than as a division error.
        ArithOp::Divide => left.checked_div(right),
        ArithOp::Modulo => left.checked_rem(right),
        ArithOp::Power => {
            return Err(SqlError::Internal(
                "integer ^ reached the integers".to_owned(),
            ));
        }
    }
    .ok_or_else(overflow)?;
    narrow(value, ty).ok_or_else(overflow)
}

/// The value as the width it was declared at, or `None` when it does not fit.
fn narrow(value: i64, ty: ColumnType) -> Option<Datum> {
    Some(match ty {
        ColumnType::Int2 => Datum::Int2(i16::try_from(value).ok()?),
        ColumnType::Int4 => Datum::Int4(i32::try_from(value).ok()?),
        _ => Datum::Int8(value),
    })
}

/// The `real` forms. PostgreSQL computes them in double and rounds the result to single, which is
/// what makes `7::float4 / 2::float4` exactly 3.5 and an inexact one round once rather than twice.
///
/// The overflow check is against **`real`'s** range, so a product that is finite as a double and
/// infinite as a single is `22003` — the declared width again, the same rule the integers follow.
fn float4(op: ArithOp, left: f64, right: f64) -> Result<Datum> {
    let value = float(op, left, right)?;
    // The narrowing is the point: PostgreSQL computes a `real` operator in double and rounds the
    // result once, and a result too large for a `real` becomes infinite here and is caught below
    // as the `22003` it is on a real server.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "rounding to `real` is what the type is; the overflow it can produce is checked"
    )]
    let narrowed = value as f32;
    finite(f64::from(narrowed), left, right)?;
    Ok(Datum::Real(narrowed))
}

fn float8(op: ArithOp, left: f64, right: f64) -> Result<Datum> {
    let value = float(op, left, right)?;
    finite(value, left, right)?;
    Ok(Datum::Double(value))
}

/// The shared float arithmetic, before the overflow check.
fn float(op: ArithOp, left: f64, right: f64) -> Result<f64> {
    // **A float divided by zero raises**; it does not yield `Infinity`. The same `22012` the
    // integers get, measured on `float4` and `float8` alike.
    if op == ArithOp::Divide && right == 0.0 {
        return Err(SqlError::DivisionByZero);
    }
    Ok(match op {
        ArithOp::Add => left + right,
        ArithOp::Subtract => left - right,
        ArithOp::Multiply => left * right,
        ArithOp::Divide => left / right,
        // Neither reaches here: `%` over floats is `42883` at plan time, and `^` is its own
        // function because it has a failure the others do not.
        ArithOp::Modulo => {
            return Err(SqlError::Internal("float % reached the floats".to_owned()));
        }
        ArithOp::Power => return power(left, right),
    })
}

/// `^`, and the one condition that is neither an overflow nor a NaN.
fn power(left: f64, right: f64) -> Result<f64> {
    // `(-2) ^ 0.5` is `2201F`, a code of its own: the answer is a complex number and PostgreSQL
    // says so rather than returning NaN.
    if left < 0.0 && right.fract() != 0.0 && right.is_finite() {
        return Err(SqlError::ComplexResult);
    }
    Ok(left.powf(right))
}

/// `22003 value out of range: overflow` when finite operands produced an infinite result.
///
/// An operand that was already infinite is not an overflow: `'Infinity'::float8 + 1` is `Infinity`
/// and `1e308 * 10` is the error. Measured, both.
fn finite(value: f64, left: f64, right: f64) -> Result<()> {
    if value.is_infinite() && left.is_finite() && right.is_finite() {
        return Err(SqlError::FloatOverflow);
    }
    Ok(())
}

/// An `unknown` operand read as the type the operator resolved to.
fn coerce(value: &Datum, ty: ColumnType) -> Result<Datum> {
    match value {
        Datum::Text(text) => Datum::from_text(ty, text),
        // `numeric` has its own arithmetic — exact, and with scale rules the floats do not have —
        // and until it is built a `numeric` operand is refused **by name** rather than rounded
        // into a float, which would answer where the answer is not PostgreSQL's.
        Datum::Numeric(_) => Err(SqlError::unsupported("arithmetic over numeric")),
        other => Ok(other.clone()),
    }
}

/// A datum that has already been converted to an integer type.
fn as_i64(value: &Datum) -> Result<i64> {
    match value {
        Datum::Int8(value) => Ok(*value),
        Datum::Int4(value) => Ok(i64::from(*value)),
        Datum::Int2(value) => Ok(i64::from(*value)),
        other => Err(SqlError::Internal(format!(
            "{other:?} reached integer arithmetic"
        ))),
    }
}

/// A datum that has already been converted to a float type.
fn as_f64(value: &Datum) -> Result<f64> {
    match value {
        Datum::Double(value) => Ok(*value),
        Datum::Real(value) => Ok(f64::from(*value)),
        // The precision loss is PostgreSQL's too: `int8 + float8` casts the integer to a double
        // and a value past 2^53 arrives rounded on a real server exactly as it does here.
        #[allow(
            clippy::cast_precision_loss,
            reason = "the implicit cast to double is the operator PostgreSQL resolved to"
        )]
        Datum::Int8(value) => Ok(*value as f64),
        Datum::Int4(value) => Ok(f64::from(*value)),
        Datum::Int2(value) => Ok(f64::from(*value)),
        other => Err(SqlError::Internal(format!(
            "{other:?} reached float arithmetic"
        ))),
    }
}

/// `abs(x)`: the same type in and out, and `22003` where the positive does not exist.
///
/// `abs((-32768)::int2)` is an error on a real server, and so is `abs((-2147483648)::int4)` — the
/// two's-complement minimum has no positive at its own width. Measured, both.
pub fn abs(value: &Datum) -> Result<Datum> {
    Ok(match value {
        Datum::Null => Datum::Null,
        Datum::Int2(value) => Datum::Int2(
            value
                .checked_abs()
                .ok_or(SqlError::IntegerLiteralOutOfRange("smallint"))?,
        ),
        Datum::Int4(value) => Datum::Int4(
            value
                .checked_abs()
                .ok_or(SqlError::IntegerLiteralOutOfRange("integer"))?,
        ),
        Datum::Int8(value) => Datum::Int8(
            value
                .checked_abs()
                .ok_or(SqlError::IntegerLiteralOutOfRange("bigint"))?,
        ),
        Datum::Double(value) => Datum::Double(value.abs()),
        Datum::Real(value) => Datum::Real(value.abs()),
        other => {
            return Err(SqlError::UndefinedFunctionTypes(format!(
                "abs({})",
                other.column_type().map_or("unknown", PgType::name)
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{ArithOp, ColumnType, Datum, apply, result_type};

    /// The promotion table, from the capture — including the three rows a widening ladder gets
    /// wrong.
    #[test]
    fn the_promotion_table_is_postgresql_s() {
        let add = |left, right| result_type(ArithOp::Add, left, right).ok();
        // A ladder ranks `real` above `integer` and answers `real`; PostgreSQL has no mixed
        // operator and casts both sides to `double precision`.
        assert_eq!(
            add(ColumnType::Int4, ColumnType::Real),
            Some(ColumnType::Double)
        );
        assert_eq!(
            add(ColumnType::Real, ColumnType::Real),
            Some(ColumnType::Real)
        );
        assert_eq!(
            add(ColumnType::Real, ColumnType::Double),
            Some(ColumnType::Double)
        );
        // A `numeric` beside a float is a float; beside an integer it is exact.
        assert_eq!(
            add(ColumnType::Numeric, ColumnType::Double),
            Some(ColumnType::Double)
        );
        assert_eq!(
            add(ColumnType::Int8, ColumnType::Numeric),
            Some(ColumnType::Numeric)
        );
        assert_eq!(
            add(ColumnType::Int2, ColumnType::Int4),
            Some(ColumnType::Int4)
        );
        assert_eq!(
            add(ColumnType::Int2, ColumnType::Int2),
            Some(ColumnType::Int2)
        );
        // `^` is a float whatever it is given, unless both sides are exact.
        assert_eq!(
            result_type(ArithOp::Power, ColumnType::Int4, ColumnType::Int4).ok(),
            Some(ColumnType::Double)
        );
        assert_eq!(
            result_type(ArithOp::Power, ColumnType::Numeric, ColumnType::Numeric).ok(),
            Some(ColumnType::Numeric)
        );
        // `%` exists for the integers and not for the floats.
        assert!(result_type(ArithOp::Modulo, ColumnType::Int8, ColumnType::Int8).is_ok());
        assert!(result_type(ArithOp::Modulo, ColumnType::Double, ColumnType::Double).is_err());
        assert!(add(ColumnType::Text, ColumnType::Int4).is_none());
    }

    /// Strictness comes first: a NULL operand is answered before the divisor is looked at.
    #[test]
    fn a_null_over_zero_is_null_and_not_a_division_error() {
        assert_eq!(
            apply(
                ArithOp::Divide,
                ColumnType::Int4,
                &Datum::Null,
                &Datum::Int4(0)
            )
            .ok(),
            Some(Datum::Null)
        );
        assert_eq!(
            apply(
                ArithOp::Divide,
                ColumnType::Int4,
                &Datum::Int4(1),
                &Datum::Int4(0)
            )
            .unwrap_err()
            .sqlstate(),
            "22012"
        );
    }

    /// Division truncates toward zero and `%` takes the dividend's sign — both directions.
    #[test]
    fn division_truncates_toward_zero_and_modulo_follows_the_dividend() {
        let int4 = |op, left: i32, right: i32| {
            apply(
                op,
                ColumnType::Int4,
                &Datum::Int4(left),
                &Datum::Int4(right),
            )
            .ok()
        };
        assert_eq!(int4(ArithOp::Divide, -7, 2), Some(Datum::Int4(-3)));
        assert_eq!(int4(ArithOp::Divide, 7, -2), Some(Datum::Int4(-3)));
        assert_eq!(int4(ArithOp::Modulo, -7, 3), Some(Datum::Int4(-1)));
        assert_eq!(int4(ArithOp::Modulo, 7, -3), Some(Datum::Int4(1)));
    }

    /// Overflow is at the **declared** width, so the same sum is an error in one type and a value
    /// in the next one up.
    #[test]
    fn overflow_is_at_the_declared_width() {
        assert_eq!(
            apply(
                ArithOp::Add,
                ColumnType::Int2,
                &Datum::Int2(32767),
                &Datum::Int2(1)
            )
            .unwrap_err()
            .to_string(),
            "smallint out of range"
        );
        assert_eq!(
            apply(
                ArithOp::Add,
                ColumnType::Int4,
                &Datum::Int2(32767),
                &Datum::Int2(1)
            )
            .ok(),
            Some(Datum::Int4(32768))
        );
    }
}
