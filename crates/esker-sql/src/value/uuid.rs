//! `uuid`: sixteen bytes, read far more loosely than they are written.
//!
//! OID 2950, `typlen` 16, `typcategory` U. Nothing about the value is a number — `uuid::int` is
//! `42846` on a real server — and its order is its bytes' order, which is what lets an index key
//! hold one unchanged.
//!
//! # Five spellings, one value
//!
//! `uuid_in` validates the **sixteen bytes**, not the shape. All five of these are the same
//! value, and every one of them prints back canonical: hyphenated, upper case, with no hyphens at
//! all, wrapped in braces, and hyphenated in the wrong places. An implementation that matched
//! `8-4-4-4-12` would reject four inputs a real server accepts.
//!
//! **But whitespace is not trimmed.** One trailing space is `22P02`, where `' integer '` is a
//! perfectly good `regtype`. Permissive about separators, strict about padding — measured both
//! ways in `tests/corpus/pg19_uuid.txt`.
//!
//! # One error, always
//!
//! Every failure is `22P02 invalid input syntax for type uuid: "…"` with the offending string
//! quoted — a wrong length, a wrong character, an empty string and an unbalanced brace alike.
//! Unlike `date` and `time` there is no range error, because **every** sixteen-byte string is a
//! uuid.

use std::fmt::Write as _;

use crate::error::{Result, SqlError};

/// What PostgreSQL's `uuid_out` writes: lower case, hyphenated `8-4-4-4-12`, always.
#[must_use]
pub fn to_text(value: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (at, byte) in value.iter().enumerate() {
        if matches!(at, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        // `write!` into the string rather than allocating one per byte.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// PostgreSQL's `uuid_in`.
///
/// Reads exactly thirty-two hex digits, ignoring hyphens **wherever they fall**, with an optional
/// surrounding pair of braces. Anything else — including a space at either end — is `22P02`.
pub fn from_text(text: &str) -> Result<[u8; 16]> {
    let body = match (text.starts_with('{'), text.ends_with('}')) {
        (true, true) => &text[1..text.len() - 1],
        // A brace on one side only is not a wrapper, and is refused rather than read past.
        (true, false) | (false, true) => return Err(invalid(text)),
        (false, false) => text,
    };
    let mut bytes = [0u8; 16];
    let mut digits = 0usize;
    for byte in body.bytes() {
        // A hyphen anywhere is a separator, which is why `a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11`
        // reads: PostgreSQL counts hex digits and not groups.
        if byte == b'-' {
            continue;
        }
        let Some(value) = hex_digit(byte) else {
            return Err(invalid(text));
        };
        if digits >= 32 {
            return Err(invalid(text));
        }
        // High nibble first: the digits are the bytes, most significant half of each byte first.
        bytes[digits / 2] |= value << (4 - 4 * (digits % 2));
        digits += 1;
    }
    if digits != 32 {
        return Err(invalid(text));
    }
    Ok(bytes)
}

/// One hex digit's value, in either case.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid(text: &str) -> SqlError {
    SqlError::InvalidTextRepresentation {
        ty: "uuid",
        value: text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{from_text, to_text};

    /// The five spellings a real server reads as one value.
    #[test]
    fn five_spellings_are_one_value() {
        let canonical = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        for text in [
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11",
        ] {
            let value = from_text(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(to_text(&value), canonical, "{text}");
        }
    }

    /// Everything else is `22P02`, including the padding a `regtype` would have tolerated.
    #[test]
    fn everything_else_is_one_error() {
        for text in [
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a111",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380g11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 ",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "not-a-uuid",
            "",
        ] {
            let error = from_text(text).unwrap_err();
            assert_eq!(error.sqlstate(), "22P02", "{text}");
            assert_eq!(
                error.to_string(),
                format!("invalid input syntax for type uuid: \"{text}\"")
            );
        }
    }

    /// The two ends, which are the values an ordering test leans on.
    #[test]
    fn the_two_ends_round_trip() {
        for text in [
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ] {
            assert_eq!(to_text(&from_text(text).unwrap()), text);
        }
    }
}
