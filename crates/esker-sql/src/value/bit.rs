//! `bit(n)` and `bit varying(n)`: a string of ones and zeros, and the two length rules over it.
//!
//! # The literal is `B'…'`, and `X'…'` is the same type through a different alphabet
//!
//! `B'1010'` and `X'ff'` are both `bit` — `pg_typeof` says so for each — and neither carries a
//! typmod: `format_type(1560, -1)` is `"bit"`, quoted because the word is reserved, where a bare
//! `bit` *column* is `bit(1)` because its `atttypmod` is 1 and not -1. Both spellings are
//! case-insensitive (`b'1010'`, `x'ff'`), `B''` is a zero-length `bit`, and each refuses by
//! naming the offending character: `"2" is not a valid binary digit`,
//! `"G" is not a valid hexadecimal digit`.
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
    // **A leading `b` or `x` says which base the rest is in**, which is `varbit_in`'s own rule and
    // not the literal syntax's: `'xff'::varbit` is `11111111` and `'b101'::varbit` is `101`,
    // measured, where `X'ff'` and `B'101'` are the *literals* [`from_hex`] already serves. The two
    // doors look alike and are not the same one — this is a string being read by an input
    // function, and that is the parser reading a token — and the tell that this one was missing is
    // the refusal: `'xyz'::varbit` is `22P02 "y" is not a valid hexadecimal digit` on a real
    // server, naming the *second* character, where reading it as binary blames the `x`.
    // A prefix on its own is the empty bit string: `'x'::varbit` and `'b'::varbit` are both `''`.
    if let Some(hex) = text.strip_prefix(['x', 'X']) {
        return from_hex(hex);
    }
    let digits = text.strip_prefix(['b', 'B']).unwrap_or(text);
    for ch in digits.chars() {
        if ch != '0' && ch != '1' {
            return Err(SqlError::InvalidBinaryDigit(ch.to_string()));
        }
    }
    Ok(digits.to_owned())
}

/// Reads the digits of an `X'…'` literal: **four bits a digit**, most significant first.
///
/// `X'F'` is `1111` and `X'0'` is `0000` — measured, and the width is what a reader would get
/// wrong: the literal's length in bits is four times its length in characters, so `X'ff'` is eight
/// bits and not two. Its refusal names the character the way the binary one does, with its own
/// word: `X'FG'` is `22P02 "G" is not a valid hexadecimal digit`.
pub fn from_hex(digits: &str) -> Result<String> {
    let mut bits = String::with_capacity(digits.len() * 4);
    for ch in digits.chars() {
        let value = ch
            .to_digit(16)
            .ok_or_else(|| SqlError::InvalidHexadecimalDigit(ch.to_string()))?;
        for shift in (0..4).rev() {
            bits.push(if value >> shift & 1 == 1 { '1' } else { '0' });
        }
    }
    Ok(bits)
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

/// An integer as `bit(n)`: **its two's complement, sign-extended or truncated to `n` bits**.
///
/// `pg_cast` has `integer -> bit` and `bigint -> bit` as explicit casts by function, and this is
/// what those functions do. Measured, and the two halves are separate rules:
///
///   * `n` **shorter** than the source keeps the **low** `n` bits — `300::int4::bit(8)` is
///     `00101100`, which is 300 modulo 256, and `5::int4::bit(2)` is `01`.
///   * `n` **longer** than the source **sign-extends** — `5::int4::bit(40)` is thirty-seven zeros
///     then `101`, and `(-1)::int4::bit(40)` is forty ones. Zero-padding would be right for the
///     first and wrong for the second, which is the half a reader gets wrong.
///
/// `width` is the source's own width in bits: 32 for an `int4`, 64 for an `int8`.
#[must_use]
pub fn from_integer(value: i64, width: u32, bits: u32) -> String {
    let sign = u8::from(value < 0);
    (0..bits)
        .rev()
        .map(|at| {
            let bit = if at < width {
                u8::try_from((value >> at) & 1).unwrap_or(0)
            } else {
                sign
            };
            if bit == 1 { '1' } else { '0' }
        })
        .collect()
}

/// `bit(n)` as an integer: **the bits are the low `n` of the target, and the rest are zero**.
///
/// So the sign comes out of the bits themselves rather than being extended into them:
/// `'1'*32::bit(32)::int4` is `-1` — the top bit lands in the sign bit — and
/// `'1'*40::bit(40)::int8` is `1099511627775`, a *positive* number, because forty bits leave an
/// `int8`'s sign bit clear. Measured, both.
///
/// More bits than the target holds is `22003`, not a truncation: `'1'*40::bit(40)::int4` is
/// `integer out of range`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "the wrap is the rule: `bit(32)::int4` reinterprets the bits as a signed value, so \
              thirty-two ones are `-1` — measured — and the width check above is what makes each \
              conversion exact rather than lossy"
)]
pub fn to_integer(bits: &str, width: u32, ty: &'static str) -> Result<i64> {
    if u32::try_from(bits.len()).unwrap_or(u32::MAX) > width {
        return Err(SqlError::IntegerLiteralOutOfRange(ty));
    }
    let unsigned = bits
        .chars()
        .fold(0_u64, |value, ch| value << 1 | u64::from(ch == '1'));
    Ok(if width == 32 {
        i64::from(unsigned as u32 as i32)
    } else {
        unsigned as i64
    })
}

#[cfg(test)]
mod tests {
    use super::{fit, fit_to_column, from_hex, from_text};

    /// **Four bits a digit**, which is the whole of what `X'…'` adds.
    #[test]
    fn a_hexadecimal_literal_is_four_bits_a_digit() {
        assert_eq!(from_hex("F").unwrap(), "1111");
        assert_eq!(from_hex("f").unwrap(), "1111");
        assert_eq!(from_hex("0").unwrap(), "0000");
        assert_eq!(from_hex("ff").unwrap(), "11111111");
        assert_eq!(from_hex("").unwrap(), "");
        let refused = from_hex("FG").unwrap_err();
        assert_eq!(refused.sqlstate(), "22P02");
        assert_eq!(
            refused.to_string(),
            "\"G\" is not a valid hexadecimal digit"
        );
    }

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
