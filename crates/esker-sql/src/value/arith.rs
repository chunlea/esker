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
use esker_keys::numeric::{Decimal, Numeric};
use esker_keys::value::Datum;

/// The type `left <op> right` has, or the `42883` PostgreSQL raises when no operator exists.
///
/// `^` is the exception to everything: it yields `double precision` whatever it is given, so
/// `2::int4 ^ 3::int4` is a float — measured.
pub fn result_type(op: ArithOp, left: ColumnType, right: ColumnType) -> Result<ColumnType> {
    let undefined = || {
        Err(SqlError::UndefinedOperator {
            left: left.name().to_owned(),
            op: op.symbol(),
            right: right.name().to_owned(),
        })
    };
    // **The date and time types have their own table** and it is not a promotion: `date - date`
    // is an `integer` where `timestamp - timestamp` is an `interval`, and `time * 2` is an
    // `interval` and not a time (`super::temporal`). A pair it does not have really has no
    // operator on a real server either — `date + numeric` is `42883` there — so the fall-through
    // below is the right answer and not a missing feature.
    if temporal_type(left) || temporal_type(right) {
        return super::temporal::result_type(op, left, right).map_or_else(undefined, Ok);
    }
    // **And `money` has its own**, for the same reason and with a sharper edge: the operators are
    // not a promotion — `money * 2` is a money, `money * money` is `42883`, and `money / money`
    // is a `double precision`. A ladder cannot express any of those three.
    if left == ColumnType::Money || right == ColumnType::Money {
        return super::money::result_type(op, left, right).map_or_else(undefined, Ok);
    }
    // **The bit operators are integers only**, and they do not promote the way the rest of this
    // ladder does: a shift keeps its *left* operand's type, so `1::int2 << 4` is a `smallint`
    // where `1::int2 + 4` is an `integer`. Measured, both.
    if matches!(
        op,
        ArithOp::BitAnd
            | ArithOp::BitOr
            | ArithOp::BitXor
            | ArithOp::ShiftLeft
            | ArithOp::ShiftRight
    ) {
        if !integer_type(left) || !integer_type(right) {
            return undefined();
        }
        // A shift keeps the left type; the other three take the wider — which is the *same*
        // answer whenever the left is already the wider, so the two cases are one expression.
        let keeps_left =
            matches!(op, ArithOp::ShiftLeft | ArithOp::ShiftRight) || bits(left) >= bits(right);
        return Ok(if keeps_left { left } else { right });
    }
    if !numeric_type(left) || !numeric_type(right) {
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
    // Before the coercion, because the operands keep their **own** types: a money and an `int8`
    // are two types on either side of one operator, not two spellings of one.
    if matches!(left, Datum::Money(_)) || matches!(right, Datum::Money(_)) {
        return super::money::apply(op, left, right);
    }
    let (left, right) = (&coerce(left, ty)?, &coerce(right, ty)?);
    if temporal_type(ty)
        || matches!(
            left,
            Datum::Date(_) | Datum::Time(_) | Datum::Interval { .. }
        )
        || matches!(
            right,
            Datum::Date(_) | Datum::Time(_) | Datum::Interval { .. }
        )
    {
        return super::temporal::apply(op, ty, left, right);
    }
    match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
            integer(op, as_i64(left)?, as_i64(right)?, ty)
        }
        ColumnType::Real => float4(op, as_f64(left)?, as_f64(right)?),
        ColumnType::Double => float8(op, as_f64(left)?, as_f64(right)?),
        ColumnType::Numeric => numeric(op, &as_numeric(left)?, &as_numeric(right)?),
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
    // **The bit operators cannot overflow and are not range-checked.** A shift count is taken
    // modulo the type's width — `1::int4 << 32` is `1`, measured — and a shift that walks a bit
    // off the top wraps rather than raising, which is the hardware showing through PostgreSQL.
    if matches!(
        op,
        ArithOp::BitAnd
            | ArithOp::BitOr
            | ArithOp::BitXor
            | ArithOp::ShiftLeft
            | ArithOp::ShiftRight
    ) {
        return Ok(bitwise(op, left, right, ty));
    }
    let value = match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Subtract => left.checked_sub(right),
        ArithOp::Multiply => left.checked_mul(right),
        // `checked_div` is `None` for `i64::MIN / -1`, which is the overflow a real server reports
        // as `bigint out of range` rather than as a division error.
        ArithOp::Divide => left.checked_div(right),
        ArithOp::Modulo => left.checked_rem(right),
        // Unreachable: answered above, before anything is range-checked, because these five do
        // not overflow.
        ArithOp::BitAnd
        | ArithOp::BitOr
        | ArithOp::BitXor
        | ArithOp::ShiftLeft
        | ArithOp::ShiftRight => {
            return Err(SqlError::Internal(
                "a bit operator reached the checked integer forms".to_owned(),
            ));
        }
        ArithOp::Power => {
            return Err(SqlError::Internal(
                "integer ^ reached the integers".to_owned(),
            ));
        }
    }
    .ok_or_else(overflow)?;
    narrow(value, ty).ok_or_else(overflow)
}

/// Whether a type is one of the three the bit operators have.
fn integer_type(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
}

/// How many bits a value of this type has, which is also the modulus a shift count is taken in.
fn bits(ty: ColumnType) -> u32 {
    match ty {
        ColumnType::Int2 => 16,
        ColumnType::Int4 => 32,
        _ => 64,
    }
}

/// The five bit operators, at the declared width.
///
/// Computed in the *type's own* width and then widened back to an `i64`, which is what makes
/// `~12::int2` a `smallint` of `-13` rather than an `i64` whose high bits are all set, and what
/// makes `1::int4 << 31` the negative number PostgreSQL answers. **The shift count wraps**:
/// `rem_euclid` rather than `%`, so a negative count shifts the other way round exactly as
/// measured (`1::int4 << -1` is `1 << 31`).
fn bitwise(op: ArithOp, left: i64, right: i64, ty: ColumnType) -> Datum {
    let width = bits(ty);
    let places = u32::try_from(right.rem_euclid(i64::from(width))).unwrap_or(0);
    let value = match op {
        ArithOp::BitAnd => left & right,
        ArithOp::BitOr => left | right,
        ArithOp::BitXor => left ^ right,
        ArithOp::ShiftLeft => left.wrapping_shl(places),
        // Arithmetic, so the sign bit is copied: `(-1) >> 1` is `-1`.
        _ => sign_extend(left, width).wrapping_shr(places),
    };
    truncate(value, width)
}

/// An `i64` cut to `width` bits and sign-extended back, which is the value the declared type holds.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the truncation is the operation: a bit operator answers at its type's width, and \
              a bit that walked off the top is one PostgreSQL drops too"
)]
fn truncate(value: i64, width: u32) -> Datum {
    match width {
        16 => Datum::Int2(value as i16),
        32 => Datum::Int4(value as i32),
        _ => Datum::Int8(value),
    }
}

/// The same value read as a signed number of `width` bits, so a right shift copies the right bit.
#[expect(
    clippy::cast_possible_truncation,
    reason = "reading the low bits as the declared width is what sign-extending means"
)]
fn sign_extend(value: i64, width: u32) -> i64 {
    match width {
        16 => i64::from(value as i16),
        32 => i64::from(value as i32),
        _ => value,
    }
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
        // Unreachable: `result_type` refuses a bit operator over anything but an integer, so no
        // value of this type ever reaches one. Named rather than left to a catch-all so that a
        // sixth operator cannot arrive here silently.
        ArithOp::BitAnd
        | ArithOp::BitOr
        | ArithOp::BitXor
        | ArithOp::ShiftLeft
        | ArithOp::ShiftRight => {
            return Err(SqlError::UndefinedOperator {
                left: "double precision".to_owned(),
                op: op.symbol(),
                right: "double precision".to_owned(),
            });
        }
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

/// The exact forms. Scales are part of the answer here in a way they are not for a float:
/// addition takes the **larger** scale, multiplication takes the **sum** of the two, and division
/// aims at sixteen significant digits (`super::numeric::div_scale`, PostgreSQL's
/// `select_div_scale`).
///
/// # `NaN` is checked before the divisor
///
/// `'NaN'::numeric / 0` is `NaN` and `'Infinity'::numeric / 0` is `22012`. The same shape as the
/// NULL rule above and measured the same way: a value that absorbs everything absorbs the error
/// too, and one that does not, does not.
fn numeric(op: ArithOp, left: &Numeric, right: &Numeric) -> Result<Datum> {
    use Numeric::{Finite, NaN};
    // `^` over two exact values has a scale rule of its own — `2 ^ 3` is `8.0000000000000000` and
    // `10 ^ 100` is an integer — which is `numeric_power`'s and not `select_div_scale`'s. Named
    // rather than approximated with the float form, whose answer would differ in its last digits.
    if op == ArithOp::Power {
        return Err(SqlError::unsupported("the operator ^ over numeric"));
    }
    if matches!(left, NaN) || matches!(right, NaN) {
        return Ok(Datum::Numeric(NaN));
    }
    let (left, right) = match (left, right) {
        (Finite(left), Finite(right)) => (left, right),
        // An infinity: the answer is decided by the signs and never by the digits, and the two
        // that have no sign to decide by are `NaN`. Measured, each of them.
        (left, right) => return Ok(Datum::Numeric(infinite(op, left, right)?)),
    };
    let value = match op {
        ArithOp::Add => super::numeric::add(left, right),
        ArithOp::Subtract => super::numeric::subtract(left, right),
        ArithOp::Multiply => super::numeric::multiply(left, right),
        ArithOp::Divide => {
            let scale = super::numeric::div_scale(left, right);
            super::numeric::divide(left, right, scale).ok_or(SqlError::DivisionByZero)?
        }
        ArithOp::Modulo => super::numeric::modulo(left, right).ok_or(SqlError::DivisionByZero)?,
        ArithOp::Power => return Err(SqlError::unsupported("the operator ^ over numeric")),
        // Unreachable: `result_type` refuses a bit operator over anything but an integer, so no
        // value of this type ever reaches one. Named rather than left to a catch-all so that a
        // sixth operator cannot arrive here silently.
        ArithOp::BitAnd
        | ArithOp::BitOr
        | ArithOp::BitXor
        | ArithOp::ShiftLeft
        | ArithOp::ShiftRight => {
            return Err(SqlError::UndefinedOperator {
                left: "numeric".to_owned(),
                op: op.symbol(),
                right: "numeric".to_owned(),
            });
        }
    };
    Ok(Datum::Numeric(Finite(value)))
}

/// The arithmetic of the two infinities, where at least one operand is one.
///
/// Every answer here is measured: `Infinity - Infinity` and `Infinity * 0` and `Infinity / Infinity`
/// and `Infinity % 3` are `NaN`; `3 % Infinity` is 3; `0 / Infinity` is 0; `Infinity / 0` is the
/// division error, because a zero divisor is a zero divisor whatever the numerator.
fn infinite(op: ArithOp, left: &Numeric, right: &Numeric) -> Result<Numeric> {
    use Numeric::{Finite, NaN, NegInfinity, PosInfinity};
    let sign = |value: &Numeric| match value {
        PosInfinity => 1,
        NegInfinity => -1,
        Finite(value) if value.negative => -1,
        _ => 0,
    };
    let infinity = |sign: i32| {
        if sign < 0 { NegInfinity } else { PosInfinity }
    };
    let (left_infinite, right_infinite) = (
        matches!(left, PosInfinity | NegInfinity),
        matches!(right, PosInfinity | NegInfinity),
    );
    Ok(match op {
        ArithOp::Add => match (left_infinite, right_infinite) {
            // Two infinities of opposite sign have no sum, and of the same sign have theirs.
            (true, true) if sign(left) != sign(right) => NaN,
            (true, _) => infinity(sign(left)),
            _ => infinity(sign(right)),
        },
        ArithOp::Subtract => match (left_infinite, right_infinite) {
            (true, true) if sign(left) == sign(right) => NaN,
            (true, _) => infinity(sign(left)),
            _ => infinity(-sign(right)),
        },
        // **`Infinity * 0` is `NaN`**, which is why the finite side's sign is not enough: a zero
        // has no sign to give the product.
        ArithOp::Multiply => {
            let (finite, infinite_side) = if left_infinite {
                (right, left)
            } else {
                (left, right)
            };
            if matches!(finite, Finite(value) if value.is_zero()) {
                NaN
            } else {
                infinity(sign(infinite_side) * if sign(finite) < 0 { -1 } else { 1 })
            }
        }
        ArithOp::Divide => {
            if right_infinite && left_infinite {
                NaN
            } else if right_infinite {
                // A finite over an infinity is zero, at scale zero.
                Finite(super::numeric::of_i64_decimal(0))
            } else if matches!(right, Finite(value) if value.is_zero()) {
                return Err(SqlError::DivisionByZero);
            } else {
                infinity(sign(left) * if sign(right) < 0 { -1 } else { 1 })
            }
        }
        // `Infinity % 3` is `NaN` and `3 % Infinity` is 3 — the dividend, unchanged.
        ArithOp::Modulo => {
            if left_infinite {
                NaN
            } else if matches!(right, Finite(value) if value.is_zero()) {
                return Err(SqlError::DivisionByZero);
            } else {
                left.clone()
            }
        }
        ArithOp::Power => return Err(SqlError::unsupported("the operator ^ over numeric")),
        // Unreachable: `result_type` refuses a bit operator over anything but an integer, so no
        // value of this type ever reaches one. Named rather than left to a catch-all so that a
        // sixth operator cannot arrive here silently.
        ArithOp::BitAnd
        | ArithOp::BitOr
        | ArithOp::BitXor
        | ArithOp::ShiftLeft
        | ArithOp::ShiftRight => {
            return Err(SqlError::UndefinedOperator {
                left: "numeric".to_owned(),
                op: op.symbol(),
                right: "numeric".to_owned(),
            });
        }
    })
}

/// A datum that has already been converted to `numeric`.
fn as_numeric(value: &Datum) -> Result<Numeric> {
    Ok(match value {
        Datum::Numeric(value) => value.clone(),
        Datum::Int8(value) => super::numeric::of_i64(*value),
        Datum::Int4(value) => super::numeric::of_i64(i64::from(*value)),
        Datum::Int2(value) => super::numeric::of_i64(i64::from(*value)),
        other => {
            return Err(SqlError::Internal(format!(
                "{other:?} reached numeric arithmetic"
            )));
        }
    })
}

/// An `unknown` operand read as the type the operator resolved to.
fn coerce(value: &Datum, ty: ColumnType) -> Result<Datum> {
    match value {
        Datum::Text(text) => Datum::from_text(ty, text),
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
        // `float8 * numeric` is a `double precision` on a real server: the exact side is cast to
        // the inexact one, not the other way round.
        Datum::Numeric(value) => Ok(super::numeric::as_f64(value)),
        other => Err(SqlError::Internal(format!(
            "{other:?} reached float arithmetic"
        ))),
    }
}

/// The type `-operand` has, or the `42883` PostgreSQL raises for one with no negation.
///
/// Every number negates to itself. **A `time` negates to an `interval`** and a `date` and a
/// `timestamp` do not negate at all: an instant has no negative and a duration does. Measured.
pub fn negate_type(operand: ColumnType) -> Result<ColumnType> {
    if numeric_type(operand) {
        return Ok(operand);
    }
    super::temporal::negate_type(operand).ok_or(SqlError::UndefinedUnaryOperator {
        op: "-",
        operand: operand.name(),
    })
}

/// `-value`, at the width it is declared to: `-((-2147483648)::int4)` is `22003`, because the
/// smallest `int4` has no positive at its own width.
pub fn negate(value: &Datum) -> Result<Datum> {
    Ok(match value {
        Datum::Null => Datum::Null,
        Datum::Int2(value) => Datum::Int2(
            value
                .checked_neg()
                .ok_or(SqlError::IntegerLiteralOutOfRange("smallint"))?,
        ),
        Datum::Int4(value) => Datum::Int4(
            value
                .checked_neg()
                .ok_or(SqlError::IntegerLiteralOutOfRange("integer"))?,
        ),
        Datum::Int8(value) => Datum::Int8(
            value
                .checked_neg()
                .ok_or(SqlError::IntegerLiteralOutOfRange("bigint"))?,
        ),
        Datum::Double(value) => Datum::Double(-value),
        Datum::Real(value) => Datum::Real(-value),
        Datum::Numeric(Numeric::Finite(value)) => Datum::Numeric(Numeric::Finite(Decimal {
            negative: !value.negative && !value.is_zero(),
            digits: value.digits.clone(),
            scale: value.scale,
        })),
        Datum::Numeric(Numeric::PosInfinity) => Datum::Numeric(Numeric::NegInfinity),
        Datum::Numeric(Numeric::NegInfinity) => Datum::Numeric(Numeric::PosInfinity),
        Datum::Numeric(Numeric::NaN) => Datum::Numeric(Numeric::NaN),
        // A `time` and an `interval`, whose negations are both intervals.
        other => super::temporal::negate(other)?,
    })
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
        // A `numeric`'s absolute value keeps its scale: `abs(-3.75)` is `3.75` and not `3.8`.
        Datum::Numeric(Numeric::Finite(value)) => Datum::Numeric(Numeric::Finite(Decimal {
            negative: false,
            digits: value.digits.clone(),
            scale: value.scale,
        })),
        Datum::Numeric(Numeric::NegInfinity) => Datum::Numeric(Numeric::PosInfinity),
        Datum::Numeric(value @ (Numeric::PosInfinity | Numeric::NaN)) => {
            Datum::Numeric(value.clone())
        }
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
