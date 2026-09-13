//! `format(formatstr, args…)` — PostgreSQL's `text_format`, measured against 19beta1
//! (`tests/corpus/pg19_format.txt`).
//!
//! **Three conversions and a width.** `%s` is the argument's output function, and a `NULL` is the
//! empty string; `%I` is `quote_ident`, and a `NULL` is `22004`; `%L` is `quote_literal`, and a
//! `NULL` is the word `NULL`, unquoted. `%n$` picks an argument by position, and the argument after
//! one picked that way is `n + 1` — `format('%2$s %s', 'a', 'b', 'c')` is `b c`. A width pads on the
//! left, or on the right with `-` or with a negative width taken from `*`.
//!
//! It is here because `check_all_foreign_keys_valid!` builds every statement it `EXECUTE`s with it
//! ([ADR 0113](../../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md)).

use crate::error::{Result, SqlError};
use crate::value::{Datum, PgDatum as _};

/// `format(…)`: the first argument is the format string, the rest are what it formats.
///
/// A `NULL` format string is a `NULL` result. Arguments past the last one the string asks for are
/// ignored, measured.
pub fn format(args: &[Datum]) -> Result<Datum> {
    let Some((pattern, arguments)) = args.split_first() else {
        return Ok(Datum::Null);
    };
    let Some(pattern) = (!matches!(pattern, Datum::Null))
        .then(|| pattern.to_text())
        .flatten()
    else {
        return Ok(Datum::Null);
    };
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    // The argument a conversion with no position of its own takes.
    let mut next = 0_usize;
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        let spec = Spec::read(&mut chars)?;
        let (mut width, mut left) = (spec.width, spec.left);
        if spec.star {
            let value = arguments.get(next).ok_or(SqlError::FormatTooFewArguments)?;
            next += 1;
            // A `NULL` width is no width; a negative one pads on the right.
            let asked = match value {
                Datum::Null => 0,
                value => value
                    .to_text()
                    .and_then(|text| text.parse::<i64>().ok())
                    .ok_or_else(|| {
                        SqlError::unsupported("format() with a width that is not an integer")
                    })?,
            };
            left |= asked < 0;
            width = usize::try_from(asked.unsigned_abs()).unwrap_or(usize::MAX);
        }
        let at = match spec.position {
            Some(position) => position - 1,
            None => next,
        };
        next = at + 1;
        let value = arguments.get(at).ok_or(SqlError::FormatTooFewArguments)?;
        let text = match spec.conversion {
            'L' => match value {
                Datum::Null => "NULL".to_owned(),
                value => quote_literal(&value.to_text().unwrap_or_default()),
            },
            'I' => match value {
                Datum::Null => return Err(SqlError::FormatNullIdentifier),
                value => crate::catalog::quote_identifier(&value.to_text().unwrap_or_default()),
            },
            _ => match value {
                Datum::Null => String::new(),
                value => value.to_text().unwrap_or_default(),
            },
        };
        let pad = width.saturating_sub(text.chars().count());
        if left {
            out.push_str(&text);
            out.extend(std::iter::repeat_n(' ', pad));
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(&text);
        }
    }
    Ok(Datum::Text(out))
}

/// One conversion, `%[n$][-][width | *]type`, read after its `%`.
struct Spec {
    /// `n$`, one-based.
    position: Option<usize>,
    /// `-`.
    left: bool,
    /// A width written as digits, or zero.
    width: usize,
    /// `*`: the width is the next argument.
    star: bool,
    /// `s`, `I` or `L`.
    conversion: char,
}

impl Spec {
    fn read(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Spec> {
        let mut spec = Spec {
            position: None,
            left: false,
            width: 0,
            star: false,
            conversion: 's',
        };
        let mut digits_were_width = false;
        if let Some(number) = digits(chars) {
            if chars.peek() == Some(&'$') {
                chars.next();
                if number == 0 {
                    return Err(SqlError::FormatArgumentZero);
                }
                spec.position = Some(number);
            } else {
                spec.width = number;
                digits_were_width = true;
            }
        }
        if !digits_were_width {
            while chars.peek() == Some(&'-') {
                chars.next();
                spec.left = true;
            }
            if chars.peek() == Some(&'*') {
                chars.next();
                if digits(chars).is_some() {
                    return Err(SqlError::unsupported("format() with a positional width"));
                }
                spec.star = true;
            } else if let Some(number) = digits(chars) {
                spec.width = number;
            }
        }
        spec.conversion = match chars.next() {
            None => return Err(SqlError::FormatUnterminatedSpecifier),
            Some(conversion @ ('s' | 'I' | 'L')) => conversion,
            Some(other) => return Err(SqlError::FormatUnrecognizedSpecifier(other)),
        };
        Ok(spec)
    }
}

/// A run of decimal digits, or `None` when there is none.
fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<usize> {
    let mut number: Option<usize> = None;
    while let Some(digit) = chars.peek().and_then(|ch| ch.to_digit(10)) {
        chars.next();
        number = Some(
            number
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit as usize),
        );
    }
    number
}

/// PostgreSQL's `quote_literal`: quoted, every quote and backslash doubled, and an `E` in front
/// when there was a backslash to double — `E'back\\slash'`, measured.
fn quote_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 3);
    if text.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::format;
    use crate::value::Datum;

    fn text(value: &str) -> Datum {
        Datum::Text(value.to_owned())
    }

    fn formatted(args: &[Datum]) -> String {
        match format(args).unwrap() {
            Datum::Text(out) => out,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_statement_check_all_foreign_keys_valid_builds() {
        assert_eq!(
            formatted(&[
                text(
                    "UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = \
                     '%1$I' AND connamespace::regnamespace = '%2$I'::regnamespace; ALTER TABLE \
                     %2$I.%3$I VALIDATE CONSTRAINT %1$I;"
                ),
                text("fk_parent_node"),
                text("Mixed Schema"),
                text("nodes"),
            ]),
            "UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = \
             'fk_parent_node' AND connamespace::regnamespace = '\"Mixed Schema\"'::regnamespace; \
             ALTER TABLE \"Mixed Schema\".nodes VALIDATE CONSTRAINT fk_parent_node;"
        );
    }

    #[test]
    fn a_position_moves_the_next_argument() {
        assert_eq!(
            formatted(&[text("%2$s %s"), text("a"), text("b"), text("c")]),
            "b c"
        );
    }

    #[test]
    fn a_width_pads_and_a_negative_one_pads_on_the_right() {
        assert_eq!(
            formatted(&[
                text("%5s|%-5s|%*s|%*s|"),
                text("a"),
                text("b"),
                Datum::Int4(3),
                text("c"),
                Datum::Int4(-3),
                text("d"),
            ]),
            "    a|b    |  c|d  |"
        );
    }

    #[test]
    fn a_literal_with_a_backslash_is_an_escape_string() {
        assert_eq!(
            formatted(&[text("%L"), text("back\\slash")]),
            "E'back\\\\slash'"
        );
        assert_eq!(formatted(&[text("%L"), Datum::Null]), "NULL");
    }
}
