//! `double precision`, in and out of text.
//!
//! The digits are the easy half: the shortest decimal string that reads back as the same `f64` is
//! what PostgreSQL prints (its `extra_float_digits` has defaulted to that since PostgreSQL 12) and
//! it is also what Rust's own float formatting produces, so `{:e}` supplies them and nothing here
//! reimplements Ryu.
//!
//! The hard half is *which notation*, and it was measured rather than assumed. PostgreSQL prints
//! plain digits while the value's decimal exponent is in `-4 ..= 14` and switches to `d.ddde±NN`
//! outside it — so `100000000000000` comes back in full and `1e+15` does not, `0.0001` is plain
//! and `1e-05` is not. That boundary is not `%g`'s, it does not depend on how many significant
//! digits the value has, and the exponent always carries a sign and at least two digits.

use std::cmp::Ordering;

use super::PgType;
use crate::error::{Result, SqlError};
use crate::value::ColumnType;

/// Below `10^LOW_EXPONENT` and at or above `10^(HIGH_EXPONENT+1)`, PostgreSQL uses exponent
/// notation. Both ends were read off a server: `0.0001` prints plain and `0.00001` does not,
/// `999999999999999` prints plain and `1e15` does not.
const LOW_EXPONENT: i32 = -4;
const HIGH_EXPONENT: i32 = 14;

/// What PostgreSQL's `float8out` writes.
pub(super) fn to_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    // `{:e}` is `[-]d[.ddd]e[-]dd` with the shortest digits that round-trip -- the same digits
    // PostgreSQL's Ryu produces, in a shape that is easy to take apart.
    shortest_as_postgresql_writes_it(&format!("{value:e}"))
}

/// What PostgreSQL's `float4out` writes: the same rule over **`f32`'s** shortest digits.
///
/// That narrower `{:e}` is the whole visible difference between the two types. Rust's formatter
/// writes the fewest digits that round-trip at the value's own width, so `1.0 / 3.0` as an `f32`
/// is `0.33333334` where the `f64` is `0.3333333333333333` — and a `real` stored as a `double`
/// would print seventeen digits a real server never wrote.
pub(super) fn to_text_f32(value: f32) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    shortest_as_postgresql_writes_it(&format!("{value:e}"))
}

/// Plain or scientific, by the same exponent window either width uses.
fn shortest_as_postgresql_writes_it(shortest: &str) -> String {
    let shortest = shortest.to_owned();
    let (sign, rest) = match shortest.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", shortest.as_str()),
    };
    let (mantissa, exponent) = rest
        .split_once('e')
        .unwrap_or_else(|| unreachable!("`{{:e}}` always writes an exponent: {shortest}"));
    let exponent: i32 = exponent
        .parse()
        .unwrap_or_else(|_| unreachable!("`{{:e}}` writes a decimal exponent: {shortest}"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    if (LOW_EXPONENT..=HIGH_EXPONENT).contains(&exponent) {
        plain(sign, &digits, exponent)
    } else {
        scientific(sign, &digits, exponent)
    }
}

/// `1e14` as `100000000000000`, `0.0001` as `0.0001`.
fn plain(sign: &str, digits: &str, exponent: i32) -> String {
    if exponent < 0 {
        #[allow(
            clippy::cast_sign_loss,
            reason = "the branch is only taken for a negative exponent"
        )]
        let leading = (-exponent - 1) as usize;
        return format!("{sign}0.{}{digits}", "0".repeat(leading));
    }
    #[allow(clippy::cast_sign_loss, reason = "the exponent is non-negative here")]
    let integer_len = exponent as usize + 1;
    match digits.len().cmp(&integer_len) {
        Ordering::Greater => {
            let (whole, fraction) = digits.split_at(integer_len);
            format!("{sign}{whole}.{fraction}")
        }
        _ => format!("{sign}{digits}{}", "0".repeat(integer_len - digits.len())),
    }
}

/// `1e+15`, `1e-05`, `5e-324`: a sign always, and at least two exponent digits.
fn scientific(sign: &str, digits: &str, exponent: i32) -> String {
    let mantissa = match digits.split_at_checked(1) {
        Some((first, "")) => first.to_owned(),
        Some((first, rest)) => format!("{first}.{rest}"),
        None => digits.to_owned(),
    };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!("{sign}{mantissa}e{exponent_sign}{:02}", exponent.abs())
}

/// What PostgreSQL's `float8in` reads.
///
/// Rust's own `f64` parser covers the grammar exactly — including `.5`, `5.`, a leading sign, and
/// `inf`/`infinity`/`nan` in any case — but it is silent about the two conditions PostgreSQL
/// reports: a literal too large becomes infinity and one too small becomes zero, where PostgreSQL
/// raises `22003` for both. So the range check is done here rather than trusted to the parser.
pub(super) fn from_text(text: &str) -> Result<f64> {
    read(text, ColumnType::Double)
}

/// PostgreSQL's `float4in`. The same lexer, narrowed — and the narrowing is where the type is.
///
/// A value that is a perfectly good `double` and not a `real` is `22003` in **both** directions,
/// which is the half a reader would not guess: `1e40` overflows as expected, and `1e-50`
/// **underflows to an error** rather than to zero, where a `float8` holds it as a denormal.
/// Measured on 19beta1, and the message quotes the value expanded to plain decimal, because the
/// literal is a `numeric` on its way in and that is `numeric`'s own text.
pub(super) fn from_text_f32(text: &str) -> Result<f32> {
    let wide = read(text, ColumnType::Real)?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the truncation is the range check: both directions of it are caught below"
    )]
    let narrow = wide as f32;
    let out_of_range = || SqlError::FloatOutOfRange {
        ty: ColumnType::Real.name(),
        value: text.to_owned(),
    };
    if narrow.is_infinite() && wide.is_finite() {
        return Err(out_of_range());
    }
    if narrow == 0.0 && wide != 0.0 {
        return Err(out_of_range());
    }
    Ok(narrow)
}

/// PostgreSQL's ordering over `f32`, which is the same rule as [`pg_cmp`] one width down.
pub(super) fn pg_cmp_f32(a: f32, b: f32) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        // Not IEEE's: `-0.0` ties with `0.0`, which `partial_cmp` already gives.
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

fn read(text: &str, ty: ColumnType) -> Result<f64> {
    let bad = || SqlError::InvalidTextRepresentation {
        ty: ty.name(),
        value: text.to_owned(),
    };
    let out_of_range = || SqlError::FloatOutOfRange {
        ty: ty.name(),
        value: text.to_owned(),
    };

    let body = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let unsigned = body.strip_prefix(['-', '+']).unwrap_or(body);

    // PostgreSQL's input function reaches the platform's `strtod`, which reads hexadecimal floats.
    // Contract C2: named, not silently answered with a number we guessed at or an error about
    // input that is in fact valid.
    if unsigned.len() >= 2 && unsigned[..2].eq_ignore_ascii_case("0x") {
        return Err(SqlError::unsupported(format!(
            "hexadecimal float input \"{text}\""
        )));
    }

    let value: f64 = body.parse().map_err(|_| bad())?;
    if value.is_infinite()
        && !unsigned.eq_ignore_ascii_case("inf")
        && !unsigned.eq_ignore_ascii_case("infinity")
    {
        return Err(out_of_range());
    }
    // Underflow: every digit rounded away. A literal that is really zero has no non-zero digit in
    // it, which is what tells `0.0000` apart from `1e-400`.
    if value == 0.0 && unsigned.bytes().any(|b| b.is_ascii_digit() && b != b'0') {
        return Err(out_of_range());
    }
    Ok(value)
}

/// A decimal literal as `numeric_out` would print it: plain, with no exponent.
///
/// This exists for one message. A float that will not fit is `22003`, and PostgreSQL quotes **two
/// different texts** for the same value depending on how it arrived — measured on 19beta1:
///
/// ```text
/// SELECT 1e400::float8     "1000…000" is out of range for type double precision   (401 digits)
/// SELECT '1e400'::float8   "1e400"    is out of range for type double precision
/// ```
///
/// The difference is not the float at all: a bare `1e400` is a **`numeric`** before anything casts
/// it, so the value the error quotes is `numeric`'s own text, and `numeric` has no exponent
/// notation. A string goes to the input function unchanged and is quoted unchanged. Both widths
/// behave this way, so this is the literal path for `real` and `double precision` alike.
pub(crate) fn plain_decimal(text: &str) -> String {
    let (sign, body) = match text.strip_prefix(['-', '+']) {
        Some(rest) if text.starts_with('-') => ("-", rest),
        Some(rest) => ("", rest),
        None => ("", text),
    };
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => match exponent.parse::<i32>() {
            Ok(exponent) => (mantissa, exponent),
            // Not a number this function can move the point of; leave it as written.
            Err(_) => return text.to_owned(),
        },
        None => return text.to_owned(),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: String = format!("{whole}{fraction}");
    // Where the point sits after the exponent is applied, counted from the left of `digits`.
    let point = i32::try_from(whole.len()).unwrap_or(0) + exponent;

    let mut out = String::from(sign);
    let Ok(point) = usize::try_from(point) else {
        // The point sits left of every digit: `0.` then that many zeros, then the digits.
        out.push_str("0.");
        for _ in 0..point.unsigned_abs() {
            out.push('0');
        }
        out.push_str(&digits);
        return out;
    };
    if point >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(point - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point]);
        out.push('.');
        out.push_str(&digits[point..]);
    }
    out
}

/// PostgreSQL's ordering, which is not IEEE's: every `NaN` is one value, it is greater than
/// `Infinity`, and `-0.0` ties with `0.0`.
pub(super) fn pg_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a
            .partial_cmp(&b)
            .unwrap_or_else(|| unreachable!("neither operand is NaN")),
    }
}
