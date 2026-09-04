//! `money` as text: cents in, cents out.
//!
//! # The type is an `i64` of cents and that is all it is
//!
//! `pg_type` reports `typlen` 8 and `typstorage` `p` — a plain fixed-width value, not a varlena —
//! so the range is exactly `i64`'s in hundredths: `$92,233,720,368,547,758.07` is the top and one
//! cent more is `22003`. Both ends were measured, and both are values a client can write.
//!
//! # The input is far looser than the output
//!
//! Measured against 19beta1 with `lc_monetary = 'C'`, which is what `money_test.rb` sets:
//!
//! | written | read back |
//! |---|---|
//! | `567.89` | `$567.89` |
//! | `$567.89` | `$567.89` |
//! | `-567.89`, `-$567.89` | `-$567.89` |
//! | `(1.00)`, `($1.00)` | `-$1.00` |
//! | `1,234.56` | `$1,234.56` |
//! | `.12` | `$0.12` |
//! | `1` | `$1.00` |
//!
//! **The brackets are a minus sign**, which is the accountants' notation and the one rule here a
//! reader would not guess. The thousands separators are accepted on the way in and *written* on
//! the way out, which is why the output is not the input.
//!
//! # Rounding is half away from zero, not half to even
//!
//! Probed with four consecutive half-cents rather than one, because a single value cannot tell the
//! two rules apart: `567.855` → `$567.86`, `567.865` → `$567.87`, `567.875` → `$567.88`,
//! `567.885` → `$567.89`. Every one rounds up; half-to-even would have given `$567.86` and
//! `$567.88` for the first and third.

use crate::error::{Result, SqlError};
use crate::plan::ArithOp;
use crate::value::ColumnType;
use esker_keys::value::Datum;

/// The output function: `$1,234.56`, `-$1,234.56`, `$0.00`.
///
/// Built from the **magnitude** as a `u64` rather than from a negated `i64`, because the smallest
/// `money` is `i64::MIN` cents and negating it is the one arithmetic overflow this type can be
/// asked for — `-$92,233,720,368,547,758.08` is a value, not an error.
#[must_use]
pub fn to_text(cents: i64) -> String {
    let magnitude = cents.unsigned_abs();
    let whole = magnitude / 100;
    let fraction = magnitude % 100;
    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.char_indices() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${grouped}.{fraction:02}")
}

/// The same value as a **`numeric`**'s text: `567.89`, `-567.89`, `0.00`.
///
/// Not [`to_text`] with the symbol stripped, and that is the point of it being a function: a
/// `money` cast to a `numeric` is `567.89` on a real server — no `$`, no separators, and scale 2
/// kept — where the output function writes `$567.89`. Going through the output function made the
/// cast `22P02 invalid input syntax for type numeric: "$567.89"`, which blames the value for a
/// conversion that exists.
#[must_use]
pub fn to_numeric_text(cents: i64) -> String {
    let magnitude = cents.unsigned_abs();
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

/// The input function, `cash_in`'s shape: an optional bracket pair, an optional sign, an optional
/// `$`, digits with optional separators, and an optional fraction.
///
/// Two different errors, both measured: text that is not a number at all is
/// `22P02 invalid input syntax for type money: "abc"`, and a number too big for the `i64` is
/// `22003 value "…" is out of range for type money` — a *data* exception, because the text parsed
/// and the value will not fit.
pub fn from_text(text: &str) -> Result<i64> {
    let invalid = || SqlError::InvalidTextRepresentation {
        ty: "money",
        value: text.to_owned(),
    };
    let overflow = || SqlError::IntegerOutOfRange {
        ty: "money",
        value: text.to_owned(),
    };

    let body = text.trim();
    // **Brackets are a minus sign.** `(1.00)` is `-$1.00`, and the sign they carry multiplies with
    // an explicit one exactly as PostgreSQL's does.
    let (body, bracketed) = match body.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        Some(inner) => (inner.trim(), true),
        None => (body, false),
    };
    let (body, signed) = match body.strip_prefix('-') {
        Some(rest) => (rest.trim_start(), true),
        None => (body.strip_prefix('+').unwrap_or(body).trim_start(), false),
    };
    let body = body.strip_prefix('$').unwrap_or(body).trim_start();
    // A sign may follow the symbol as well as precede it: `$-1.00` is `-$1.00`.
    let (body, signed_after) = match body.strip_prefix('-') {
        Some(rest) => (rest.trim_start(), true),
        None => (body, false),
    };
    let negative = bracketed ^ signed ^ signed_after;

    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };
    // The separators are accepted anywhere in the integer part, which is what a real server does
    // with them: `cash_in` skips them rather than checking the grouping.
    let whole: String = whole.chars().filter(|ch| *ch != ',').collect();
    if whole.is_empty() && fraction.is_empty() {
        return Err(invalid());
    }
    if !whole.chars().all(|ch| ch.is_ascii_digit())
        || !fraction.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(invalid());
    }
    let mut cents: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse::<i128>().map_err(|_| overflow())? * 100
    };
    let mut digits = fraction.chars();
    let tens = digits.next().and_then(|ch| ch.to_digit(10)).unwrap_or(0);
    let units = digits.next().and_then(|ch| ch.to_digit(10)).unwrap_or(0);
    cents += i128::from(tens) * 10 + i128::from(units);
    // **Half away from zero**, on the first digit that does not fit.
    if digits
        .next()
        .and_then(|ch| ch.to_digit(10))
        .is_some_and(|d| d >= 5)
    {
        cents += 1;
    }
    if negative {
        cents = -cents;
    }
    i64::try_from(cents).map_err(|_| overflow())
}

/// The type `left <op> right` has when either side is a `money`, or `None` for a pair a real
/// server has no operator for.
///
/// **`money` has its own table for the reason the date types do**: the operators are not a
/// promotion ladder. `money * 2` is a money and `money * money` is `42883`; `money / money` is a
/// **`double precision`** and `2 / money` is `42883`; and there is no unary minus at all —
/// `-('1.00'::money)` is `42883 operator does not exist: - money`, measured.
#[must_use]
pub fn result_type(op: ArithOp, left: ColumnType, right: ColumnType) -> Option<ColumnType> {
    let scale = |ty: ColumnType| {
        matches!(
            ty,
            ColumnType::Int2
                | ColumnType::Int4
                | ColumnType::Int8
                | ColumnType::Real
                | ColumnType::Double
                | ColumnType::Numeric
        )
    };
    match (op, left, right) {
        (ArithOp::Add | ArithOp::Subtract, ColumnType::Money, ColumnType::Money) => {
            Some(ColumnType::Money)
        }
        // **Either side**, which is the one asymmetry worth stating: `3 * money` is a money and
        // `3 / money` is not an operator at all.
        (ArithOp::Multiply, ColumnType::Money, other) if scale(other) => Some(ColumnType::Money),
        (ArithOp::Multiply, other, ColumnType::Money) if scale(other) => Some(ColumnType::Money),
        (ArithOp::Divide, ColumnType::Money, ColumnType::Money) => Some(ColumnType::Double),
        (ArithOp::Divide, ColumnType::Money, other) if scale(other) => Some(ColumnType::Money),
        _ => None,
    }
}

/// `left <op> right` where one of them is a `money`, evaluated.
///
/// # Three rounding rules, each measured with four half-cents
///
/// One probe cannot tell half-up from half-to-even, so each of these was asked four consecutive
/// times:
///
/// * **`money * integer` and `money ± money` are exact** — no rounding to do.
/// * **`money * float`, `money * numeric` and `money / float`, `money / numeric` round half to
///   even**: `$0.05 * 0.5` is `$0.02` and `$0.15 * 0.5` is `$0.08`, where half-up would give
///   `$0.03` and `$0.08`.
/// * **`money / integer` truncates toward zero**: `$2.00 / 3` is `$0.66`, where every rounding
///   rule gives `$0.67`. It is C integer division, and it is the one case where the two families
///   disagree — `$2.00 / 3::numeric` *is* `$0.67`.
///
/// A non-integer divisor or factor goes through `f64`, which is what makes those two rules one
/// line each. It is exact for every value a client is likely to write and is not exact for a
/// `numeric` with more than 15 significant digits — declared here rather than hidden, because the
/// alternative is a second decimal implementation for an operator no suite test writes.
pub fn apply(op: ArithOp, left: &Datum, right: &Datum) -> Result<Datum> {
    let overflow = || SqlError::MoneyOutOfRange;
    let integer = |datum: &Datum| match datum {
        Datum::Int8(v) => Some(*v),
        Datum::Int4(v) => Some(i64::from(*v)),
        Datum::Int2(v) => Some(i64::from(*v)),
        _ => None,
    };
    let scaled = |datum: &Datum| match datum {
        Datum::Double(v) => Some(*v),
        Datum::Real(v) => Some(f64::from(*v)),
        Datum::Numeric(v) => Some(super::numeric::as_f64(v)),
        _ => None,
    };
    // The cents a rounded `f64` names, or `22003` for one no `money` can hold — `NaN` included,
    // which `try_from` on a float cannot be asked about directly.
    let cents_of = |value: f64| {
        let rounded = value.round_ties_even();
        if !rounded.is_finite() || rounded < -(2f64.powi(63)) || rounded >= 2f64.powi(63) {
            return Err(overflow());
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the bounds check above is the conversion: every value that reaches this \
                      line is a whole number inside `i64`'s range"
        )]
        Ok(Datum::Money(rounded as i64))
    };
    match (op, left, right) {
        (ArithOp::Add, Datum::Money(a), Datum::Money(b)) => {
            a.checked_add(*b).map(Datum::Money).ok_or_else(overflow)
        }
        (ArithOp::Subtract, Datum::Money(a), Datum::Money(b)) => {
            a.checked_sub(*b).map(Datum::Money).ok_or_else(overflow)
        }
        (ArithOp::Divide, Datum::Money(a), Datum::Money(b)) => {
            if *b == 0 {
                return Err(SqlError::DivisionByZero);
            }
            #[expect(
                clippy::cast_precision_loss,
                reason = "the quotient of two cent counts is a `double precision` on a real \
                          server too, and this is that operator"
            )]
            Ok(Datum::Double(*a as f64 / *b as f64))
        }
        (ArithOp::Multiply, Datum::Money(cents), other)
        | (ArithOp::Multiply, other, Datum::Money(cents)) => {
            match (integer(other), scaled(other)) {
                (Some(factor), _) => cents
                    .checked_mul(factor)
                    .map(Datum::Money)
                    .ok_or_else(overflow),
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the f64 path is what reproduces the measured half-to-even rounding; \
                          its limit is stated in this function's doc"
                )]
                (None, Some(factor)) => cents_of(*cents as f64 * factor),
                (None, None) => Err(undefined(op, left, right)),
            }
        }
        (ArithOp::Divide, Datum::Money(cents), other) => match (integer(other), scaled(other)) {
            (Some(0), _) => Err(SqlError::DivisionByZero),
            // **Truncating**, which is the one place the integer and the scaled forms disagree.
            (Some(divisor), _) => Ok(Datum::Money(cents / divisor)),
            (None, Some(divisor)) => {
                if divisor == 0.0 {
                    return Err(SqlError::DivisionByZero);
                }
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "see the multiply arm: the f64 path is the measured rounding"
                )]
                cents_of(*cents as f64 / divisor)
            }
            (None, None) => Err(undefined(op, left, right)),
        },
        _ => Err(undefined(op, left, right)),
    }
}

/// The `42883` for a pair with no operator, named from the values rather than from a type table so
/// that `money % 2` and `money * money` each say what they were given.
fn undefined(op: ArithOp, left: &Datum, right: &Datum) -> SqlError {
    let name = |datum: &Datum| {
        datum
            .column_type()
            .map_or("unknown", <ColumnType as super::PgType>::name)
            .to_owned()
    };
    SqlError::UndefinedOperator {
        left: name(left),
        op: op.symbol(),
        right: name(right),
    }
}

#[cfg(test)]
mod tests {
    use super::{from_text, to_text};

    /// Every row of the capture's input table, and the output beside it.
    #[test]
    fn the_input_is_looser_than_the_output() {
        for (written, read_back) in [
            ("567.89", "$567.89"),
            ("$567.89", "$567.89"),
            ("-567.89", "-$567.89"),
            ("-$567.89", "-$567.89"),
            ("(1.00)", "-$1.00"),
            ("($1.00)", "-$1.00"),
            ("1,234.56", "$1,234.56"),
            ("$1,234.56", "$1,234.56"),
            ("12345678.12", "$12,345,678.12"),
            ("0.12", "$0.12"),
            (".12", "$0.12"),
            ("1", "$1.00"),
            ("0", "$0.00"),
        ] {
            assert_eq!(to_text(from_text(written).unwrap()), read_back, "{written}");
        }
    }

    /// **Half away from zero**, which one value cannot distinguish from half-to-even.
    #[test]
    fn a_half_cent_rounds_away_from_zero() {
        for (written, read_back) in [
            ("567.855", "$567.86"),
            ("567.865", "$567.87"),
            ("567.875", "$567.88"),
            ("567.885", "$567.89"),
            ("567.894", "$567.89"),
            ("567.896", "$567.90"),
        ] {
            assert_eq!(to_text(from_text(written).unwrap()), read_back, "{written}");
        }
    }

    /// Both ends of the `i64`, and the cent past each of them.
    #[test]
    fn the_range_is_the_i64_s() {
        assert_eq!(from_text("92233720368547758.07").unwrap(), i64::MAX);
        assert_eq!(from_text("-92233720368547758.08").unwrap(), i64::MIN);
        assert_eq!(to_text(i64::MAX), "$92,233,720,368,547,758.07");
        assert_eq!(to_text(i64::MIN), "-$92,233,720,368,547,758.08");
        assert_eq!(
            from_text("92233720368547758.08").unwrap_err().sqlstate(),
            "22003"
        );
        assert_eq!(from_text("abc").unwrap_err().sqlstate(), "22P02");
        assert_eq!(from_text("").unwrap_err().sqlstate(), "22P02");
    }
}
