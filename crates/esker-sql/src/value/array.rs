//! `array_in` and `array_out`: an array literal read, and an array value printed.
//!
//! The two are a pair and neither is the obvious thing.
//!
//! # Reading
//!
//! **An element is trimmed unless it was quoted.** `'{ a , b }'` is `{a,b}` and `'{" a "}'` keeps
//! its spaces. **An unquoted `NULL` is a NULL and a quoted `"NULL"` is the string** — the one rule
//! that makes `{NULL,"NULL"}` two different elements. Inside a quoted element a backslash escapes
//! the next character and is the *element's* escape, not SQL's, so `'{"a\"b"}'` holds `a"b`.
//!
//! **A malformed literal and a bad element are different failures.** `'{1,x}'::int[]` is
//! `22P02 invalid input syntax for type integer: "x"` — the element type answering for its own
//! value — while `'{a,,b}'` is `22P02 malformed array literal` with a `DETAIL` naming the
//! character. Four DETAILs, all measured, one SQLSTATE.
//!
//! **Sub-arrays must match.** `'{{1,2},{3}}'` is malformed, and the check is on the *shape* rather
//! than on the count: a two-dimensional array is one flat element list with a shape beside it.
//!
//! # Printing
//!
//! **Only what needs quoting is quoted**: `{a,b}` but `{"a b","c,d"}`, and an element that would
//! read back as a NULL — the four letters, in any case — is quoted so that it does not.
//! A lower bound that is not one is printed as the `[l:u]=` prefix PostgreSQL prints, because that
//! bound is part of the value and has to survive the round trip.

use std::fmt::Write as _;

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, PgDatum as _};
use esker_keys::array::ArrayValue;
use esker_keys::value::Datum;

/// `array_in`: a literal read as an array of `element`.
pub fn from_text(text: &str, element: ColumnType) -> Result<ArrayValue> {
    let malformed = |detail: &str| {
        Err(SqlError::MalformedArrayLiteral {
            value: text.to_owned(),
            detail: detail.to_owned(),
        })
    };
    let body = text.trim();
    // The optional `[l:u]=` prefix, which sets the lower bound of each dimension. Only the first
    // is kept: this node's arrays are subscripted from one bound, which is every bound
    // `ActiveRecord` writes and every one the capture holds.
    let (lower, body) = match strip_bounds(body) {
        Some((lower, rest)) => (lower, rest),
        None => (1, body),
    };
    let mut parser = Parser {
        bytes: body.as_bytes(),
        at: 0,
    };
    parser.skip_space();
    if parser.peek() != Some(b'{') {
        return malformed("Array value must start with \"{\" or dimension information.");
    }
    let mut values = Vec::new();
    // **Indexed by depth and filled as each level closes**, which is innermost-first: a plain
    // `Vec` push would have recorded the *inner* count at index 0 and made `'{{1,2},{3}}'` read
    // as a two-by-one array instead of the malformed literal it is.
    let mut dims: Vec<Option<i32>> = Vec::new();
    parser.read_braced(element, 0, &mut dims, &mut values, text)?;
    let dims: Vec<i32> = dims.into_iter().map(|dim| dim.unwrap_or(0)).collect();
    parser.skip_space();
    if parser.at < parser.bytes.len() {
        return malformed(&format!(
            "Unexpected \"{}\" character.",
            char::from(parser.bytes[parser.at])
        ));
    }
    // An empty array has **no** dimensions rather than one of length zero, which is what makes
    // `array_length('{}', 1)` NULL where `cardinality('{}')` is 0.
    let dims = if values.is_empty() { Vec::new() } else { dims };
    Ok(ArrayValue {
        element,
        lower,
        dims,
        values,
    })
}

/// The `[l:u]=` prefix, and what follows it.
fn strip_bounds(body: &str) -> Option<(i32, &str)> {
    let rest = body.strip_prefix('[')?;
    let (bounds, tail) = rest.split_once("]=")?;
    let (low, _high) = bounds.split_once(':')?;
    Some((low.trim().parse().ok()?, tail))
}

/// The recursive-descent half, which is where the dimension check lives.
struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// One `{...}`, at nesting `depth`, appending its elements and checking its shape.
    fn read_braced(
        &mut self,
        element: ColumnType,
        depth: usize,
        dims: &mut Vec<Option<i32>>,
        values: &mut Vec<Option<Datum>>,
        whole: &str,
    ) -> Result<()> {
        let malformed = |detail: &str| {
            Err(SqlError::MalformedArrayLiteral {
                value: whole.to_owned(),
                detail: detail.to_owned(),
            })
        };
        // `self.peek()` is `{`, checked by the caller.
        self.at += 1;
        let mut count = 0i32;
        loop {
            self.skip_space();
            match self.peek() {
                None => return malformed("Unexpected end of input."),
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                Some(b'{') => {
                    let before = values.len();
                    self.read_braced(element, depth + 1, dims, values, whole)?;
                    let _ = before;
                }
                Some(b',') => return malformed("Unexpected \",\" character."),
                Some(_) => {
                    // A scalar element here means this brace holds elements, so nothing deeper
                    // may appear in it — `'{a{b}'` is the `{` character reported by name.
                    let value = self.read_element(element, whole)?;
                    values.push(value);
                }
            }
            count += 1;
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                None => return malformed("Unexpected end of input."),
                Some(other) => {
                    return malformed(&format!("Unexpected \"{}\" character.", char::from(other)));
                }
            }
        }
        // **The shape is checked against the first sub-array at this depth**, which is what makes
        // `'{{1,2},{3}}'` malformed rather than ragged.
        if dims.len() <= depth {
            dims.resize(depth + 1, None);
        }
        match dims[depth] {
            None => dims[depth] = Some(count),
            Some(known) if known == count => {}
            Some(_) => {
                return malformed(
                    "Multidimensional arrays must have sub-arrays with matching dimensions.",
                );
            }
        }
        Ok(())
    }

    /// One element: quoted or bare, up to the next `,` or `}`.
    fn read_element(&mut self, element: ColumnType, whole: &str) -> Result<Option<Datum>> {
        let malformed = |detail: &str| SqlError::MalformedArrayLiteral {
            value: whole.to_owned(),
            detail: detail.to_owned(),
        };
        if self.peek() == Some(b'"') {
            self.at += 1;
            let mut text = String::new();
            loop {
                match self.peek() {
                    None => return Err(malformed("Unexpected end of input.")),
                    // The escape is the **element's**, so a backslash takes the next byte
                    // whatever it is.
                    Some(b'\\') => {
                        self.at += 1;
                        let Some(byte) = self.peek() else {
                            return Err(malformed("Unexpected end of input."));
                        };
                        text.push(char::from(byte));
                        self.at += 1;
                    }
                    Some(b'"') => {
                        self.at += 1;
                        break;
                    }
                    Some(_) => {
                        let start = self.at;
                        self.at += 1;
                        while self.at < self.bytes.len() && !self.bytes[self.at].is_ascii() {
                            self.at += 1;
                        }
                        text.push_str(
                            std::str::from_utf8(&self.bytes[start..self.at])
                                .map_err(|_| malformed("Unexpected end of input."))?,
                        );
                    }
                }
            }
            // A **quoted** element is never a NULL, which is the whole of `{NULL,"NULL"}`.
            self.skip_space();
            if !matches!(self.peek(), Some(b',' | b'}') | None) {
                return Err(malformed("Incorrectly quoted array element."));
            }
            return Ok(Some(Datum::from_text(element, &text)?));
        }
        let start = self.at;
        while !matches!(self.peek(), Some(b',' | b'}') | None) {
            if self.peek() == Some(b'{') {
                return Err(malformed("Unexpected \"{\" character."));
            }
            if self.peek() == Some(b'"') {
                return Err(malformed("Incorrectly quoted array element."));
            }
            self.at += 1;
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| malformed("Unexpected end of input."))?;
        // **Unquoted, so it is trimmed** — `'{ a , b }'` is `{a,b}` — and an unquoted `NULL` in
        // any case is the NULL element.
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("null") {
            return Ok(None);
        }
        Ok(Some(Datum::from_text(element, trimmed)?))
    }
}

/// `array_out`: the text PostgreSQL prints for this value.
#[must_use]
pub fn to_text(value: &ArrayValue) -> String {
    let mut out = String::new();
    // The bound is part of the value, so a value that does not start at one says where it starts.
    if value.lower != 1 && !value.values.is_empty() {
        let upper = value.lower + i32::try_from(value.values.len()).unwrap_or(0) - 1;
        let _ = write!(out, "[{}:{upper}]=", value.lower);
    }
    write_dimension(value, &value.dims, 0, &mut 0, &mut out);
    out
}

/// One brace level, recursing while there are dimensions left below it.
fn write_dimension(
    value: &ArrayValue,
    dims: &[i32],
    depth: usize,
    next: &mut usize,
    out: &mut String,
) {
    out.push('{');
    let Some(&count) = dims.get(depth) else {
        out.push('}');
        return;
    };
    for at in 0..count {
        if at > 0 {
            out.push(',');
        }
        if depth + 1 < dims.len() {
            write_dimension(value, dims, depth + 1, next, out);
        } else {
            let element = match value.values.get(*next) {
                Some(Some(element)) => quoted(element.to_text().unwrap_or_default().as_str()),
                // An unquoted `NULL` is how a NULL element prints, which is what makes it read
                // back as one.
                _ => "NULL".to_owned(),
            };
            out.push_str(&element);
            *next += 1;
        }
    }
    out.push('}');
}

/// An element, quoted only if printing it bare would not read back as itself.
fn quoted(text: &str) -> String {
    let needs = text.is_empty()
        || text.eq_ignore_ascii_case("null")
        || text
            .chars()
            .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
    if !needs {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}
