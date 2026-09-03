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

/// PostgreSQL's `oidin`.
///
/// Reads an optionally-signed decimal. **A negative one wraps** into the unsigned range, which is
/// what `(-1)::oid` being `4294967295` means; anything outside 32 bits either way is `22003`.
pub fn from_text(text: &str) -> Result<u32> {
    let body = text.trim();
    if let Ok(value) = body.parse::<u32>() {
        return Ok(value);
    }
    // A negative number is *reinterpreted*, not refused — the same bits read as unsigned.
    if let Ok(value) = body.parse::<i64>() {
        return i32::try_from(value)
            .map(|narrow| u32::from_ne_bytes(narrow.to_ne_bytes()))
            .map_err(|_| out_of_range(text));
    }
    // Told apart by whether it *looked* like a number: `4294967296` is a range error and `abc`
    // is a syntax one, which is the same split every integer type makes.
    if body
        .strip_prefix(['-', '+'])
        .unwrap_or(body)
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        && !body.is_empty()
    {
        return Err(out_of_range(text));
    }
    Err(SqlError::InvalidTextRepresentation {
        ty: "oid",
        value: text.to_owned(),
    })
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
