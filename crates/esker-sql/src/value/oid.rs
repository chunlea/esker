//! `oid`: a four-byte **unsigned** integer that prints as a plain number.
//!
//! OID 26, `typlen` 4, `typcategory` N, `typinput` `oidin`. It is the type every catalog
//! identifier really has — `pg_class.oid`, `pg_type.oid`, and what a `regtype` *is* underneath.
//!
//! # Unsigned is the whole of the difference
//!
//! An `int4` and an `oid` are both four bytes, and the sign is what separates them. Measured:
//! `(-1)::oid` is **`4294967295`** — it wraps rather than refusing, because the cast is a
//! reinterpretation — and `'4294967296'::oid` is `22003 value "4294967296" is out of range for
//! type oid`. Text that is not a number at all is `22P02`, with the string quoted.

use crate::error::{Result, SqlError};

/// What `oidout` writes: the number, and nothing else.
#[must_use]
pub fn to_text(value: u32) -> String {
    value.to_string()
}

/// PostgreSQL's `oidin`, which is **C's `strtoul` with base 0** and not this server's own integer
/// scanner.
///
/// The two disagree on the same digits, measured on 19beta1:
///
/// ```text
///   '010'::oid    8            <- a leading zero is octal
///   '010'::int4   10           <- and to int4in it is nothing at all
///   '0x10'::oid   16           '0x10'::int4   16
///   '0o17'::oid   22P02        '0o17'::int4   15
///   '0b101'::oid  22P02        '0b101'::int4  5
///   '1_000'::oid  22P02        '1_000'::int4  1000
///   '08'::oid     22P02        <- not an octal digit
/// ```
///
/// So `oid` takes hexadecimal and octal in C's spelling and takes neither of PostgreSQL's own
/// newer prefixes nor its digit separators. Nothing here is guessable from the other type; all of
/// it is in `tests/captures/pg19_oid_type.txt`.
///
/// **A negative one wraps** into the unsigned range — `(-1)::oid` is `4294967295` and `'-0x10'` is
/// `4294967280` — while anything past `u32::MAX` in magnitude is `22003`.
pub fn from_text(text: &str) -> Result<u32> {
    let body = text.trim();
    let (negative, digits) = match body.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, body.strip_prefix('+').unwrap_or(body)),
    };
    let (radix, digits) = radix_of(digits);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(invalid(text));
    }
    let Ok(magnitude) = u64::from_str_radix(digits, radix) else {
        // Told apart by whether it *looked* like a number in its own radix: `4294967296` is a
        // range error and `abc` — or `08`, whose digit is not an octal one — is a syntax error.
        return Err(if digits.bytes().all(|byte| byte.is_ascii_digit()) {
            out_of_range(text)
        } else {
            invalid(text)
        });
    };
    if magnitude > u64::from(u32::MAX) + u64::from(negative) {
        return Err(out_of_range(text));
    }
    let value = u32::try_from(magnitude).unwrap_or(u32::MAX);
    Ok(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

/// The base a run of digits is read in, and the digits with their prefix removed.
///
/// `0x`/`0X` is hexadecimal and a leading `0` is octal, which is `strtoul`'s rule and not this
/// server's: `0o` and `0b` are **not** here, because `oidin` does not take them.
fn radix_of(digits: &str) -> (u32, &str) {
    if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        return (16, rest);
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return (8, &digits[1..]);
    }
    (10, digits)
}

fn invalid(text: &str) -> SqlError {
    SqlError::InvalidTextRepresentation {
        ty: "oid",
        value: text.to_owned(),
    }
}

fn out_of_range(text: &str) -> SqlError {
    SqlError::OidOutOfRange(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{from_text, to_text};

    /// The ends, and the wrap that makes this not an `int4`.
    #[test]
    fn a_negative_wraps_and_the_top_is_the_unsigned_one() {
        assert_eq!(to_text(from_text("123").unwrap()), "123");
        assert_eq!(to_text(from_text("0").unwrap()), "0");
        assert_eq!(to_text(from_text("4294967295").unwrap()), "4294967295");
        // Measured on 19beta1: `(-1)::oid` is the top of the range, not an error.
        assert_eq!(to_text(from_text("-1").unwrap()), "4294967295");
    }

    /// Two failures, two codes: out of range, and not a number at all.
    #[test]
    fn past_the_top_is_a_range_error_and_letters_are_a_syntax_one() {
        let error = from_text("4294967296").unwrap_err();
        assert_eq!(error.sqlstate(), "22003");
        assert_eq!(
            error.to_string(),
            "value \"4294967296\" is out of range for type oid"
        );
        for text in ["abc", "", "1.5"] {
            let error = from_text(text).unwrap_err();
            assert_eq!(error.sqlstate(), "22P02", "{text}");
        }
    }
}
