//! A composite type's text form — PostgreSQL's record literal, in and out.
//!
//! Measured in `tests/captures/pg19_composite.txt`, and the capture was taken **before** this was
//! written because three of its rules are not what a reader would guess:
//!
//! * whitespace **inside** the parens belongs to the field and whitespace outside them does not;
//! * `NULL` spelled out is the four-character string, not a NULL — only an *empty unquoted* field
//!   is one;
//! * an empty **quoted** field is the empty string, which is a different value, and keeping those
//!   two apart is the whole reason this text form is lossless.
//!
//! The value a column stores is this canonical text, the way `jsonb` stores its own
//! ([ADR 0042](../../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)):
//! composite equality is field by field, and two field-equal composites render identically, so the
//! comparison comes with the representation.

use crate::error::{Result, SqlError};

/// The fields of a record literal, or `22P02` naming the literal as PostgreSQL does.
///
/// **The field count is not checked here** — this function does not know the type. The caller
/// does, and raises the same `malformed record literal` for a count that does not match, which is
/// what a real server answers for `'(a,b,c)'::full_address`.
pub fn parse(text: &str) -> Result<Vec<Option<String>>> {
    let malformed = || SqlError::MalformedRecordLiteral(text.to_owned());
    // Outer whitespace is not part of anything: `' (a,b) '` is `(a,b)`. Inner whitespace *is*,
    // which is the pair this is careful about.
    let body = text.trim();
    let body = body
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(malformed)?;
    // `()` is malformed rather than a record of one empty field — measured.
    if body.is_empty() {
        return Err(malformed());
    }

    let mut fields = Vec::new();
    let mut current = String::new();
    // Whether anything **quoted** has been seen in this field, which is what tells an empty string
    // from a NULL: `("",x)` keeps an empty first field and `(,x)` has none.
    let mut quoted_here = false;
    let mut chars = body.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => current.push(chars.next().ok_or_else(malformed)?),
            '"' if in_quotes => {
                // `""` inside a quoted run is one quote; a lone `"` closes the run.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => {
                in_quotes = true;
                quoted_here = true;
            }
            ',' if !in_quotes => {
                fields.push(finish(std::mem::take(&mut current), quoted_here));
                quoted_here = false;
            }
            other => current.push(other),
        }
    }
    if in_quotes {
        return Err(malformed());
    }
    fields.push(finish(current, quoted_here));
    Ok(fields)
}

/// One field, once its runs have been read: **empty and never quoted is a NULL**.
fn finish(text: String, quoted: bool) -> Option<String> {
    if text.is_empty() && !quoted {
        None
    } else {
        Some(text)
    }
}

/// The fields as PostgreSQL prints them.
///
/// **A NULL contributes nothing at all** and an empty string contributes `""`, which is the pair
/// `parse` reads back. Everything else is quoted only when it has to be.
pub fn render(fields: &[Option<String>]) -> String {
    let mut out = String::from("(");
    for (at, field) in fields.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let Some(text) = field else { continue };
        if needs_quoting(text) {
            out.push('"');
            for c in text.chars() {
                // The output function always **doubles** a quote, even though the input accepts a
                // backslash before one; a backslash in the value doubles too.
                match c {
                    '"' => out.push_str("\"\""),
                    '\\' => out.push_str("\\\\"),
                    other => out.push(other),
                }
            }
            out.push('"');
        } else {
            out.push_str(text);
        }
    }
    out.push(')');
    out
}

/// Whether a field has to be quoted: **any** space counts, including a leading or trailing one.
fn needs_quoting(text: &str) -> bool {
    text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, ',' | '"' | '(' | ')' | '\\') || c.is_whitespace())
}

/// `parse` then `render`, which is what a text literal assigned to a composite column goes
/// through — so what comes back out is what a real server would have printed, not what was sent.
pub fn canonicalise(text: &str, arity: usize) -> Result<String> {
    let fields = parse(text)?;
    if fields.len() != arity {
        return Err(SqlError::MalformedRecordLiteral(text.to_owned()));
    }
    Ok(render(&fields))
}
