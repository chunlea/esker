//! `numeric`: the type whose **scale is part of the value**.
//!
//! `1.0`, `1.00` and `1.000` are three values that all compare equal and all print as written.
//! Every other rule here follows from that one, and it is the rule ADR 0031 has been refusing
//! `avg(int8)` over since unit 0 — a type that normalised trailing zeros would be wrong on the
//! first row of the corpus.
//!
//! # Rounding is half **away from zero**, and never banker's
//!
//! The discriminating case is `1.245::numeric(10,2)`, which is `1.25` — half-to-even would give
//! `1.24`. The same rule for the cast, for an assignment into a declared scale, and for negatives:
//! `(-1.245)::numeric(10,2)` is `-1.25`. Measured; it is *not* the rule
//! `super::timestamp`'s `round_to_precision` uses for a microsecond tie, and one type having two
//! rounding rules is what makes that worth saying twice.
//!
//! # `NaN` is a value, and it is the largest one
//!
//! `'NaN' = 'NaN'` is `t` and `'NaN' > 1` is `t` — the opposite of IEEE and the opposite of what
//! `float8` does in this same crate. It is a variant rather than a bit pattern for exactly that
//! reason: nothing about it should be able to inherit a float's rules by accident.

use std::cmp::Ordering;

use esker_keys::numeric::{Decimal, Numeric};

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, NO_TYPMOD, PgType as _};

/// PostgreSQL's bounds on a declared precision. Both messages shout: `NUMERIC precision 1001 must
/// be between 1 and 1000`.
pub const MIN_PRECISION: i32 = 1;
/// The largest declared precision.
pub const MAX_PRECISION: i32 = 1000;
/// The bounds on a declared scale, which is signed: `numeric(10,-2)` is a real type.
pub const MIN_SCALE: i32 = -1000;
/// The largest declared scale.
pub const MAX_SCALE: i32 = 1000;

/// `((p << 16) | s) + 4` — a **third** typmod encoding, next to `varchar`'s length + 4 and
/// `timestamp`'s bare precision.
///
/// Measured: `format_type(1700, 655366)` is `numeric(10,2)` and `655366` is `(10 << 16) | 2` plus
/// 4. The scale is stored in the low sixteen bits as a **signed** value, which is how a negative
/// one survives: `format_type(1700, 786434)` is `numeric(11,-2)`.
#[must_use]
pub fn typmod_of(precision: i32, scale: i32) -> i32 {
    ((precision << 16) | (scale & 0xffff)) + 4
}

/// The precision and scale a typmod carries, or `None` for a `numeric` declared without one.
#[must_use]
pub fn precision_and_scale(typmod: i32) -> Option<(i32, i32)> {
    if typmod < 4 {
        return None;
    }
    let packed = typmod - 4;
    // Sign-extended from sixteen bits, which is what makes `-2` come back as `-2`. Written
    // through the bytes rather than through two casts, so nothing here can lose a sign.
    let low = u16::try_from(packed & 0xffff).unwrap_or(0);
    let scale = i32::from(i16::from_ne_bytes(low.to_ne_bytes()));
    Some((packed >> 16, scale))
}

/// What `numeric_out` writes — [`esker_keys::numeric::to_text`], which is where the rendering
/// lives because `esker-store` needs it and cannot see this crate.
pub use esker_keys::numeric::to_text;

/// PostgreSQL's `numeric_in`.
///
/// Digits, an optional point, an optional exponent, and the three words. **`Infinity` and `inf`
/// are the same value** and so are their negatives; the words are matched case-insensitively,
/// which is what a real server does.
pub fn from_text(text: &str) -> Result<Numeric> {
    let body = text.trim();
    let invalid = || SqlError::InvalidTextRepresentation {
        ty: ColumnType::Numeric.name(),
        value: text.to_owned(),
    };
    match () {
        () if body.eq_ignore_ascii_case("nan") => return Ok(Numeric::NaN),
        () if body.eq_ignore_ascii_case("infinity") || body.eq_ignore_ascii_case("inf") => {
            return Ok(Numeric::PosInfinity);
        }
        () if body.eq_ignore_ascii_case("+infinity") || body.eq_ignore_ascii_case("+inf") => {
            return Ok(Numeric::PosInfinity);
        }
        () if body.eq_ignore_ascii_case("-infinity") || body.eq_ignore_ascii_case("-inf") => {
            return Ok(Numeric::NegInfinity);
        }
        () => {}
    }
    parse_decimal(body).map(Numeric::Finite).ok_or_else(invalid)
}

/// The finite half of [`from_text`].
fn parse_decimal(body: &str) -> Option<Decimal> {
    let (negative, body) = match body.as_bytes().first() {
        Some(b'-') => (true, &body[1..]),
        Some(b'+') => (false, &body[1..]),
        _ => (false, body),
    };
    // An exponent shifts the point rather than being stored: `1.5e2` is `150` at scale 0, which is
    // what a real server holds for it.
    let (body, exponent) = match body.find(['e', 'E']) {
        Some(at) => {
            let exponent: i32 = body[at + 1..].parse().ok()?;
            (&body[..at], exponent)
        }
        None => (body, 0),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut digits: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes())
        .map(|b| b - b'0')
        .collect();
    let scale = i32::try_from(fraction.len()).ok()?.checked_sub(exponent)?;
    // Leading zeros are not digits, but a value that is all zeros keeps one.
    while digits.len() > 1 && digits[0] == 0 {
        digits.remove(0);
    }
    let all_zero = digits.iter().all(|digit| *digit == 0);
    Some(Decimal {
        negative: negative && !all_zero,
        digits,
        scale,
    })
}

/// PostgreSQL's ordering: `-Infinity` < finite < `Infinity` < **`NaN`**.
///
/// The last one is the surprise and it is measured: `'NaN'::numeric > 1` is `t`, where a
/// `float8`'s `NaN` is unordered. Two finite values are compared as **numbers**, so `1.0` and
/// `1.00` are equal — which is why an index key normalises (`esker_keys::row`).
#[must_use]
pub fn pg_cmp(left: &Numeric, right: &Numeric) -> Ordering {
    fn rank(value: &Numeric) -> u8 {
        match value {
            Numeric::NegInfinity => 0,
            Numeric::Finite(_) => 1,
            Numeric::PosInfinity => 2,
            Numeric::NaN => 3,
        }
    }
    match (left, right) {
        (Numeric::Finite(a), Numeric::Finite(b)) => compare_finite(a, b),
        (a, b) => rank(a).cmp(&rank(b)),
    }
}

/// Two finite decimals, by value.
fn compare_finite(left: &Decimal, right: &Decimal) -> Ordering {
    let (left, right) = (left.normalised(), right.normalised());
    match (left.is_zero(), right.is_zero()) {
        (true, true) => return Ordering::Equal,
        // Zero is neither positive nor negative, so its side is decided by the other value's sign.
        (true, false) => {
            return if right.negative {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        (false, true) => {
            return if left.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        (false, false) => {}
    }
    match (left.negative, right.negative) {
        (false, true) => return Ordering::Greater,
        (true, false) => return Ordering::Less,
        _ => {}
    }
    // Same sign: the bigger magnitude wins, and for a negative pair that reverses the answer.
    let magnitude = left
        .exponent()
        .cmp(&right.exponent())
        .then_with(|| compare_digits(&left.digits, &right.digits));
    if left.negative {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Digit strings of two values with the same exponent, left-aligned — so `12` beats `119`.
fn compare_digits(left: &[u8], right: &[u8]) -> Ordering {
    for at in 0..left.len().max(right.len()) {
        let (a, b) = (
            left.get(at).copied().unwrap_or(0),
            right.get(at).copied().unwrap_or(0),
        );
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// A value fitted to a declared `numeric(p, s)`, rounding half away from zero.
///
/// Two different `22003`s, and PostgreSQL words them differently for a reason a client can act on:
/// an infinity **cannot** be held by any typmod (`a field with precision 10, scale 2 cannot hold
/// an infinite value`), where a finite value that is merely too big names the bound it broke
/// (`must round to an absolute value less than 10^8`). `NaN` fits every typmod, which is the
/// asymmetry between the two specials.
pub fn fit_to_typmod(value: Numeric, typmod: i32) -> Result<Numeric> {
    let Some((precision, scale)) = precision_and_scale(typmod) else {
        return Ok(value);
    };
    let decimal = match value {
        // `NaN` fits anything; an infinity fits nothing that has a precision.
        Numeric::NaN => return Ok(Numeric::NaN),
        Numeric::PosInfinity | Numeric::NegInfinity => {
            return Err(SqlError::NumericFieldOverflow {
                detail: format!(
                    "A field with precision {precision}, scale {scale} cannot hold an infinite \
                     value."
                ),
            });
        }
        Numeric::Finite(decimal) => decimal,
    };
    let rounded = round_to_scale(&decimal, scale);
    // The digits left of the point must fit `precision - scale`.
    let whole = i64::try_from(rounded.digits.len()).unwrap_or(i64::MAX) - i64::from(rounded.scale);
    if !rounded.is_zero() && whole > i64::from(precision - scale) {
        // **`10^0` is written `1`.** PostgreSQL spells the bound as a power of ten except when
        // the exponent is zero, where it writes the number — measured on `numeric(1000,1000)`,
        // whose every digit is fractional so nothing at all fits in front of the point.
        let bound = match precision - scale {
            0 => "1".to_owned(),
            digits => format!("10^{digits}"),
        };
        return Err(SqlError::NumericFieldOverflow {
            detail: format!(
                "A field with precision {precision}, scale {scale} must round to an absolute \
                 value less than {bound}."
            ),
        });
    }
    Ok(Numeric::Finite(rounded))
}

/// The value at exactly `scale` fractional digits, **rounding half away from zero**.
fn round_to_scale(decimal: &Decimal, scale: i32) -> Decimal {
    if decimal.scale == scale {
        return decimal.clone();
    }
    if decimal.scale < scale {
        // Lengthening: the declared scale is printed, so the zeros are appended rather than
        // implied. `1.0::numeric(10,3)` is `1.000`.
        let mut digits = decimal.digits.clone();
        let extra = usize::try_from(scale - decimal.scale).unwrap_or(0);
        digits.extend(std::iter::repeat_n(0, extra));
        return Decimal {
            negative: decimal.negative,
            digits,
            scale,
        };
    }
    // Shortening: drop digits, and carry when the first dropped one is 5 or more.
    let drop = usize::try_from(decimal.scale - scale).unwrap_or(0);
    if drop >= decimal.digits.len() {
        // Everything is dropped. The result is 0 or ±1 in the last kept place, decided by the
        // digit that would have been first — which exists only when the drop is exactly one past.
        let round_up =
            drop == decimal.digits.len() && decimal.digits.first().is_some_and(|d| *d >= 5);
        return Decimal {
            negative: decimal.negative && round_up,
            digits: vec![u8::from(round_up)],
            scale,
        };
    }
    let keep = decimal.digits.len() - drop;
    let mut digits = decimal.digits[..keep].to_vec();
    if decimal.digits[keep] >= 5 {
        // Carry, left to right, growing by one digit when it runs off the end: `9.5` at scale 0
        // is `10`.
        let mut at = digits.len();
        loop {
            if at == 0 {
                digits.insert(0, 1);
                break;
            }
            at -= 1;
            if digits[at] == 9 {
                digits[at] = 0;
            } else {
                digits[at] += 1;
                break;
            }
        }
    }
    let all_zero = digits.iter().all(|digit| *digit == 0);
    Decimal {
        negative: decimal.negative && !all_zero,
        digits,
        scale,
    }
}

/// `numeric(p, s)` as `format_type` prints it, or bare for a type with no typmod.
#[must_use]
pub fn format_typmod(typmod: i32) -> String {
    match precision_and_scale(typmod) {
        Some((precision, scale)) => format!("numeric({precision},{scale})"),
        None => "numeric".to_owned(),
    }
}

/// The typmod a declared `numeric(p[, s])` gets, with PostgreSQL's two bounds checks.
///
/// A **bare precision means scale zero**, not "no scale": `numeric(10)` is `numeric(10,0)` and
/// rounds. Measured, and it is the one that would silently keep a fraction if it were read as
/// "unspecified".
pub fn declared_typmod(precision: Option<i32>, scale: Option<i32>) -> Result<i32> {
    let Some(precision) = precision else {
        return Ok(NO_TYPMOD);
    };
    if !(MIN_PRECISION..=MAX_PRECISION).contains(&precision) {
        return Err(SqlError::NumericPrecisionOutOfRange(precision));
    }
    let scale = scale.unwrap_or(0);
    if !(MIN_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(SqlError::NumericScaleOutOfRange(scale));
    }
    Ok(typmod_of(precision, scale))
}

/// An integer as a decimal, exactly — for the cross-type comparisons `numeric` has with the three
/// integer widths. Exact in that direction whatever the width, which is why it goes this way
/// rather than the other.
#[must_use]
pub fn of_i64(value: i64) -> Numeric {
    let negative = value < 0;
    let digits: Vec<u8> = value
        .unsigned_abs()
        .to_string()
        .bytes()
        .map(|b| b - b'0')
        .collect();
    Numeric::Finite(Decimal {
        negative: negative && digits.iter().any(|digit| *digit != 0),
        digits,
        scale: 0,
    })
}

/// A `numeric` as the `f64` PostgreSQL promotes it to when it meets a float.
///
/// **The exact type loses to the inexact one**, which is PostgreSQL's own rule: `numeric + float8`
/// is `double precision` where `numeric + int4` stays `numeric`. So this direction is the lossy
/// one on purpose, and it is the one a real server takes.
#[must_use]
pub fn as_f64(value: &Numeric) -> f64 {
    match value {
        Numeric::NaN => f64::NAN,
        Numeric::PosInfinity => f64::INFINITY,
        Numeric::NegInfinity => f64::NEG_INFINITY,
        Numeric::Finite(_) => to_text(value).parse().unwrap_or(f64::NAN),
    }
}
