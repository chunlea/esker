//! `bit(n)` and `bit varying(n)`: a string of ones and zeros, and the two length rules over it.
//!
//! # One representation, two types, and they compare equal
//!
//! `B'101'::bit varying = B'101'::bit(3)` is `t` on a real server, so under
//! [ADR 0042](../../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)
//! the two may share a representation. What differs is the length rule and the name the client is
//! told, so — as with `inet` and `cidr` — the value carries which of the two it is.
//!
//! # The length rule is the whole type, and a cast and an assignment disagree
//!
//! Measured, each one separately:
//!
//! | | `bit(8)` | `bit varying(4)` |
//! |---|---|---|
//! | cast, too short | `'101'::bit(8)` is `10100000` — **padded on the right** | — |
//! | cast, too long | `'101010101'::bit(4)` is `1010` — truncated | `'10101'::bit varying(3)` is `101` |
//! | assignment, too short | `22026 bit string length 3 does not match type bit(8)` | fits |
//! | assignment, too long | the same `22026` | `22001 bit string too long for type bit varying(4)` |
//!
//! **A cast pads and truncates in silence; an assignment refuses.** The padding is on the *right*,
//! which is the half a reader would get wrong — a bit string is written most-significant first and
//! `'101'::bit(8)` is `10100000`, not `00000101`.
//!
//! # `0xF` is not a bit string
//!
//! `'0xF'::bit(4)` is `22P02 "x" is not a valid binary digit` on a real server. The hexadecimal
//! form is the **literal** `x'F'`, which is `1111`, and `ActiveRecord`'s own `bit_varying` type is
//! what turns a `"0xF"` in Ruby into bits before the statement is built — `bit_string_test.rb`'s
//! round trip asserts `1111` and the server never sees the `0x`.

use crate::error::{Result, SqlError};

/// Reads a bit string: ones and zeros, and nothing else.
///
/// The error names the **character**, not the type: `'FF'::bit(8)` is
/// `22P02 "F" is not a valid binary digit`, which is its own sentence and not the
/// `invalid input syntax for type …` every other type gives.
pub fn from_text(text: &str) -> Result<String> {
    for ch in text.chars() {
        if ch != '0' && ch != '1' {
            return Err(SqlError::InvalidBinaryDigit(ch.to_string()));
        }
    }
    Ok(text.to_owned())
}

/// A **cast** to `bit(n)` or `bit varying(n)`: padded on the right, or truncated.
///
/// `None` for a type with no length, which is what a bare `bit varying` is — it holds whatever it
/// is given.
#[must_use]
pub fn fit(bits: &str, length: Option<usize>, varying: bool) -> String {
    let Some(length) = length else {
        return bits.to_owned();
    };
    if bits.len() > length {
        return bits[..length].to_owned();
    }
    // **Only a fixed-width `bit` pads**; a `bit varying` shorter than its limit stays short.
    if varying {
        return bits.to_owned();
    }
    let mut out = bits.to_owned();
    out.extend(std::iter::repeat_n('0', length - bits.len()));
    out
}

/// An **assignment** to `bit(n)` or `bit varying(n)`, which refuses where the cast adjusts.
///
/// Two different errors and two different classes, both measured: a fixed-width column wants the
/// exact length (`22026`, either way), and a varying one only a maximum (`22001`).
pub fn fit_to_column(bits: &str, length: Option<usize>, varying: bool) -> Result<()> {
    let Some(length) = length else {
        return Ok(());
    };
    if varying {
        if bits.len() > length {
            return Err(SqlError::BitStringTooLong(format!("bit varying({length})")));
        }
        return Ok(());
    }
    if bits.len() != length {
        return Err(SqlError::BitStringLengthMismatch {
            length: bits.len(),
            ty: format!("bit({length})"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fit, fit_to_column, from_text};

    /// Every row of the module's own table.
    #[test]
    fn a_cast_adjusts_where_an_assignment_refuses() {
        assert_eq!(fit("101", Some(8), false), "10100000");
        assert_eq!(fit("101010101", Some(4), false), "1010");
        assert_eq!(fit("10101", Some(3), true), "101");
        assert_eq!(fit("101", Some(4), true), "101");
        assert_eq!(fit("101", None, true), "101");

        assert_eq!(
            fit_to_column("101", Some(8), false).unwrap_err().sqlstate(),
            "22026"
        );
        assert_eq!(
            fit_to_column("101010101", Some(8), false)
                .unwrap_err()
                .sqlstate(),
            "22026"
        );
        assert_eq!(
            fit_to_column("10101", Some(4), true)
                .unwrap_err()
                .sqlstate(),
            "22001"
        );
        assert!(fit_to_column("0101", Some(4), true).is_ok());
        assert!(fit_to_column("101", Some(4), true).is_ok());
    }

    /// The error names the character.
    #[test]
    fn only_ones_and_zeros() {
        assert_eq!(from_text("00001010").unwrap(), "00001010");
        assert_eq!(from_text("").unwrap(), "");
        let refused = from_text("FF").unwrap_err();
        assert_eq!(refused.sqlstate(), "22P02");
        assert_eq!(refused.to_string(), "\"F\" is not a valid binary digit");
        assert_eq!(
            from_text("0xF").unwrap_err().to_string(),
            "\"x\" is not a valid binary digit"
        );
    }
}
