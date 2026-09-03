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

/// Zero, or any small integer, as a `Decimal` at scale zero.
#[must_use]
pub fn of_i64_decimal(value: i64) -> Decimal {
    match of_i64(value) {
        Numeric::Finite(value) => value,
        // Unreachable: an `i64` is always finite.
        _ => Decimal::zero(),
    }
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

// ---------------------------------------------------------------------------------------------
// Arithmetic
//
// The numeric unit shipped the type "stored, compared, ordered, indexed and printed; not yet
// computed with". `sum` and `avg` are what needed it computed with, and these are the two
// operations they need — nothing else, deliberately: a general arithmetic surface is the
// engine's missing `BinaryOp` and its own unit.
// ---------------------------------------------------------------------------------------------

/// The magnitudes of two digit strings compared, longest-first then lexicographically.
fn cmp_digits(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `a + b` on magnitudes, most significant digit first.
fn add_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u8;
    for at in 0..a.len().max(b.len()) {
        let left = a.get(a.len().wrapping_sub(at + 1)).copied().unwrap_or(0);
        let right = b.get(b.len().wrapping_sub(at + 1)).copied().unwrap_or(0);
        let sum = left + right + carry;
        out.push(sum % 10);
        carry = sum / 10;
    }
    if carry != 0 {
        out.push(carry);
    }
    out.reverse();
    out
}

/// `a - b` on magnitudes, where `a >= b`.
fn sub_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for at in 0..a.len() {
        let left = i8::try_from(a[a.len() - at - 1]).unwrap_or(0);
        let right = b
            .get(b.len().wrapping_sub(at + 1))
            .copied()
            .map_or(0, |digit| i8::try_from(digit).unwrap_or(0));
        let mut value = left - right - borrow;
        if value < 0 {
            value += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(u8::try_from(value).unwrap_or(0));
    }
    out.reverse();
    trim_leading(out)
}

/// Drops leading zeros, keeping at least one digit.
fn trim_leading(mut digits: Vec<u8>) -> Vec<u8> {
    let keep = digits
        .iter()
        .position(|digit| *digit != 0)
        .unwrap_or(digits.len() - 1);
    digits.drain(..keep);
    digits
}

/// `value` with `count` zeros appended, which multiplies it by `10^count`.
fn shift_left(digits: &[u8], count: usize) -> Vec<u8> {
    let mut out = digits.to_vec();
    out.extend(std::iter::repeat_n(0u8, count));
    out
}

/// Two decimals added, keeping the wider scale — which is what PostgreSQL's `+` does.
#[must_use]
pub fn add(left: &Decimal, right: &Decimal) -> Decimal {
    let scale = left.scale.max(right.scale);
    let left_digits = shift_left(
        &left.digits,
        usize::try_from(scale - left.scale).unwrap_or(0),
    );
    let right_digits = shift_left(
        &right.digits,
        usize::try_from(scale - right.scale).unwrap_or(0),
    );
    let (negative, digits) = if left.negative == right.negative {
        (left.negative, add_digits(&left_digits, &right_digits))
    } else if cmp_digits(&left_digits, &right_digits).is_ge() {
        (left.negative, sub_digits(&left_digits, &right_digits))
    } else {
        (right.negative, sub_digits(&right_digits, &left_digits))
    };
    // Zero is never negative here, which is the invariant `Decimal` states.
    let negative = negative && digits.iter().any(|digit| *digit != 0);
    Decimal {
        negative,
        digits,
        scale,
    }
}

/// `left - right`, which is `left + (-right)` and needs no arithmetic of its own.
#[must_use]
pub fn subtract(left: &Decimal, right: &Decimal) -> Decimal {
    let negated = Decimal {
        // Zero is never negative, which is `Decimal`'s invariant and the reason this is not a
        // plain `!right.negative`.
        negative: !right.negative && !right.is_zero(),
        digits: right.digits.clone(),
        scale: right.scale,
    };
    add(left, &negated)
}

/// `left * right`, whose scale is the **sum** of the two.
///
/// `1.50 * 1.50` is `2.2500` and not `2.25`: the scales add, so the trailing zeros are part of
/// the answer. `0 * 3.14` is `0.00` for the same reason — nothing is stripped, because the scale
/// says how many places the value is known to and multiplying two known values knows more places,
/// not fewer. Measured, all three.
#[must_use]
pub fn multiply(left: &Decimal, right: &Decimal) -> Decimal {
    let digits = trim_leading(mul_digits(&left.digits, &right.digits));
    let negative = (left.negative != right.negative) && digits.iter().any(|digit| *digit != 0);
    Decimal {
        negative,
        digits,
        scale: left.scale + right.scale,
    }
}

/// Schoolbook multiplication over base-10 digits, most significant first.
fn mul_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut product = vec![0u32; a.len() + b.len()];
    for (i, left) in a.iter().rev().enumerate() {
        for (j, right) in b.iter().rev().enumerate() {
            product[i + j] += u32::from(*left) * u32::from(*right);
        }
    }
    let mut carry = 0u32;
    for slot in &mut product {
        let total = *slot + carry;
        *slot = total % 10;
        carry = total / 10;
    }
    let mut digits: Vec<u8> = product
        .iter()
        .rev()
        .map(|digit| u8::try_from(*digit).unwrap_or(0))
        .collect();
    if digits.is_empty() {
        digits.push(0);
    }
    digits
}

/// `left % right`: the remainder, at the **larger** of the two scales and with the **dividend's**
/// sign.
///
/// `10.5 % 3` is `1.5`, `(-10) % 3` is `-1` and `10 % (-3)` is `1` — measured, all three, because
/// the two signs are where a remainder and a modulus part company. `None` for a zero divisor,
/// which the caller turns into `22012`.
#[must_use]
pub fn modulo(left: &Decimal, right: &Decimal) -> Option<Decimal> {
    if right.is_zero() {
        return None;
    }
    // Both brought to one scale, which makes them two integers and the remainder an integer
    // division — no rounding anywhere, which is what a remainder must not have.
    let scale = left.scale.max(right.scale);
    let numerator = shift_left(
        &left.digits,
        usize::try_from(scale - left.scale).unwrap_or(0),
    );
    let divisor = shift_left(
        &right.digits,
        usize::try_from(scale - right.scale).unwrap_or(0),
    );
    let mut remainder: Vec<u8> = vec![0];
    for digit in &numerator {
        if remainder == [0] {
            remainder = vec![*digit];
        } else {
            remainder.push(*digit);
        }
        remainder = trim_leading(remainder);
        while cmp_digits(&remainder, &divisor).is_ge() {
            remainder = sub_digits(&remainder, &divisor);
        }
    }
    let digits = trim_leading(remainder);
    let negative = left.negative && digits.iter().any(|digit| *digit != 0);
    Some(Decimal {
        negative,
        digits,
        scale,
    })
}

/// The fractional digits PostgreSQL's division produces for these two operands.
///
/// This is `select_div_scale` from `numeric.c`, and it is **not** a fixed number of places: it
/// aims at sixteen *significant* digits, so `3 / 3` comes out at twenty fractional digits and
/// `5 / 3` at sixteen. Measured against 19beta1 before it was written, and each of those two is
/// a test below.
///
/// The weights are in PostgreSQL's base-10000 digits, which is why they are computed here rather
/// than taken from the base-10 representation this crate stores: the rule counts groups of four.
#[must_use]
pub fn div_scale(numerator: &Decimal, denominator: &Decimal) -> i32 {
    /// The base-10000 weight and leading group of a decimal's magnitude.
    fn weight_and_lead(value: &Decimal) -> (i32, i32) {
        let normalised = value.normalised();
        if normalised.is_zero() {
            return (0, 0);
        }
        let integer_digits =
            i32::try_from(normalised.digits.len()).unwrap_or(i32::MAX) - normalised.scale;
        // The leading base-10000 group is the first `integer_digits mod 4` digits (or four) —
        // **padded with zeros when the value has fewer**, which is not a detail. `normalised()`
        // strips trailing zeros, so `10` is stored as the digit `1` at scale -1; reading only the
        // digits that are there makes its leading group `1` instead of `10`, and `10 / 3` then
        // takes the `first digit <=` branch and comes out at twenty places where PostgreSQL gives
        // sixteen. The group is a fixed-width number, so it is built to that width.
        let lead_len = integer_digits.rem_euclid(4);
        let lead_len = if lead_len == 0 { 4 } else { lead_len };
        let lead: i32 = (0..lead_len).fold(0, |acc, at| {
            let digit = usize::try_from(at)
                .ok()
                .and_then(|at| normalised.digits.get(at))
                .copied()
                .unwrap_or(0);
            acc * 10 + i32::from(digit)
        });
        // `weight` counts whole base-10000 groups above the point, less one.
        ((integer_digits - 1).div_euclid(4), lead)
    }

    let (weight1, lead1) = weight_and_lead(numerator);
    let (weight2, lead2) = weight_and_lead(denominator);
    let mut qweight = weight1 - weight2;
    if lead1 <= lead2 {
        qweight -= 1;
    }
    // Sixteen significant digits, less whatever the quotient's own weight already provides.
    let mut rscale = 16 - qweight * 4;
    rscale = rscale.max(numerator.scale).max(denominator.scale).max(0);
    rscale.min(1000)
}

/// `left / right` at `scale` fractional digits, rounded **half away from zero**.
///
/// `None` for a division by zero, which the caller turns into `22012`.
#[must_use]
pub fn divide(left: &Decimal, right: &Decimal, scale: i32) -> Option<Decimal> {
    if right.is_zero() {
        return None;
    }
    // left/right = (L × 10^-ls) / (R × 10^-rs) = (L / R) × 10^(rs-ls); wanted at 10^-scale, so
    // the numerator is shifted by `scale + rs - ls` and one more digit for the rounding decision.
    let shift = scale + right.scale - left.scale + 1;
    let numerator = if shift >= 0 {
        shift_left(&left.digits, usize::try_from(shift).unwrap_or(0))
    } else {
        let drop = usize::try_from(-shift).unwrap_or(0);
        if drop >= left.digits.len() {
            vec![0]
        } else {
            left.digits[..left.digits.len() - drop].to_vec()
        }
    };
    // Schoolbook long division, one digit at a time.
    let mut quotient: Vec<u8> = Vec::with_capacity(numerator.len());
    let mut remainder: Vec<u8> = vec![0];
    for digit in &numerator {
        // `remainder = remainder * 10 + digit`, which is one push and **not** a shift as well:
        // doing both multiplied by a hundred and quietly divided by ten.
        if remainder == [0] {
            remainder = vec![*digit];
        } else {
            remainder.push(*digit);
        }
        remainder = trim_leading(remainder);
        let mut count = 0u8;
        while cmp_digits(&remainder, &right.digits).is_ge() {
            remainder = sub_digits(&remainder, &right.digits);
            count += 1;
        }
        quotient.push(count);
    }
    // The extra digit decides the rounding, half away from zero.
    let last = quotient.pop().unwrap_or(0);
    let mut digits = trim_leading(quotient);
    if last >= 5 {
        digits = add_digits(&digits, &[1]);
    }
    let negative = (left.negative != right.negative) && digits.iter().any(|digit| *digit != 0);
    Some(Decimal {
        negative,
        digits,
        scale,
    })
}

/// Two numerics added, with the non-finite rules PostgreSQL's `+` has.
///
/// `NaN` absorbs everything, and two infinities of opposite sign are `NaN` — the same answers
/// `float8` gives, and measured for this type rather than assumed from that one.
#[must_use]
pub fn sum(left: &Numeric, right: &Numeric) -> Numeric {
    match (left, right) {
        // A `NaN` absorbs, and two opposite infinities make one — measured for this type, not
        // borrowed from `float8`, though the answers turn out to be the same.
        (Numeric::NaN, _)
        | (_, Numeric::NaN)
        | (Numeric::PosInfinity, Numeric::NegInfinity)
        | (Numeric::NegInfinity, Numeric::PosInfinity) => Numeric::NaN,
        (Numeric::PosInfinity, _) | (_, Numeric::PosInfinity) => Numeric::PosInfinity,
        (Numeric::NegInfinity, _) | (_, Numeric::NegInfinity) => Numeric::NegInfinity,
        (Numeric::Finite(left), Numeric::Finite(right)) => Numeric::Finite(add(left, right)),
    }
}

/// `sum / count` at PostgreSQL's own division scale — what `avg` answers.
///
/// `None` only for a zero count, which the caller has already turned into NULL.
#[must_use]
pub fn mean(sum: &Numeric, count: i64) -> Option<Numeric> {
    if count == 0 {
        return None;
    }
    let Numeric::Finite(total) = sum else {
        // An infinity or a `NaN` divided by a count is itself, which is what `float8`'s average
        // does with the same inputs.
        return Some(sum.clone());
    };
    let Numeric::Finite(divisor) = of_i64(count) else {
        return None;
    };
    let scale = div_scale(total, &divisor);
    divide(total, &divisor, scale).map(Numeric::Finite)
}

#[cfg(test)]
mod arithmetic_tests {
    use super::{Numeric, add, div_scale, divide, from_text, to_text};

    fn dec(text: &str) -> super::Decimal {
        match from_text(text).unwrap() {
            Numeric::Finite(value) => value,
            other => panic!("{other:?} is not finite"),
        }
    }

    /// Addition keeps the wider scale, which is what `sum` prints by.
    #[test]
    fn addition_keeps_the_wider_scale() {
        for (left, right, expect) in [
            ("1.5", "2.25", "3.75"),
            ("1.00", "2", "3.00"),
            ("-1.5", "1.5", "0.0"),
            ("-2.5", "1.5", "-1.0"),
            ("1.5", "-2.5", "-1.0"),
            ("9223372036854775807", "1", "9223372036854775808"),
        ] {
            let sum = add(&dec(left), &dec(right));
            assert_eq!(to_text(&Numeric::Finite(sum)), expect, "{left} + {right}");
        }
    }

    /// **The division scale is not a fixed number of places**, and these are PostgreSQL's own
    /// answers: `3/3` is twenty fractional digits and `5/3` is sixteen, because the rule aims at
    /// sixteen *significant* ones. Captured from 19beta1 before this was written.
    #[test]
    fn the_division_scale_is_postgresqls_own() {
        for (left, right, expect) in [
            ("3", "3", 20),
            ("5", "3", 16),
            ("4.0", "2", 16),
            ("3.123456789012345678", "2", 18),
            ("3", "2", 16),
        ] {
            assert_eq!(
                div_scale(&dec(left), &dec(right)),
                expect,
                "{left} / {right}"
            );
        }
    }

    /// And the quotient itself, to the digit — every one of these is an `avg` a real server
    /// printed in the capture this unit was built from.
    #[test]
    fn the_quotient_matches_postgresql_to_the_digit() {
        for (left, right, expect) in [
            ("3", "3", "1.00000000000000000000"),
            ("5", "3", "1.6666666666666667"),
            ("4.0", "2", "2.0000000000000000"),
            ("3", "2", "1.5000000000000000"),
            ("3.75", "2", "1.8750000000000000"),
            ("3.123456789012345678", "2", "1.561728394506172839"),
            ("30", "2", "15.0000000000000000"),
            ("300", "2", "150.0000000000000000"),
            ("-3", "2", "-1.5000000000000000"),
        ] {
            let (a, b) = (dec(left), dec(right));
            let scale = div_scale(&a, &b);
            let quotient = divide(&a, &b, scale).expect("a non-zero divisor");
            assert_eq!(
                to_text(&Numeric::Finite(quotient)),
                expect,
                "{left} / {right}"
            );
        }
        // A zero divisor is the caller's `22012`, not a value.
        assert!(divide(&dec("1"), &dec("0"), 16).is_none());
    }
}
