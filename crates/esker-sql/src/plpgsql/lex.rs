//! The PL/pgSQL tokenizer: PostgreSQL's core scanner, cut down to what reading a body needs.
//!
//! **What it must get right is where a token ends**, not what a token means. A body is split into
//! statements at `;`, and an SQL statement inside it runs to the `;` at bracket depth zero — so a
//! `;` inside a string, a quoted identifier, a dollar quote or a comment must never end anything.
//! What an operator does or whether a number fits is the SQL layer's to decide once the fragment
//! reaches it, and this module does not try.
//!
//! The rules are PostgreSQL's `scan.l`: `''` inside a string and `""` inside an identifier are one
//! quote, a dollar quote closes only on its own tag, comments nest, and a multi-character operator
//! does not end in `+` or `-` unless it holds one of `~`, `!`, `@`, `#`, `%`, `^`, `&`, `|`, a
//! backtick or `?` — which is what reads
//! `n=-1` as `n`, `=`, `-`, `1` rather than as an operator `=-`.

use crate::error::{Result, SqlError};

/// The characters an operator is made of.
const OPERATOR: &[u8] = b"+-*/<>=~!@#%^&|`?";

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// An unquoted identifier or a keyword, compared without regard to case.
    Word,
    /// `"…"`.
    QuotedIdent,
    /// `'…'`, a prefixed string (`E'…'`, `B'…'`, `X'…'`, `N'…'`, `U&'…'`) or `$tag$…$tag$`.
    Literal,
    /// A numeric constant.
    Number,
    /// `$1`.
    Parameter,
    /// A run of operator characters, or `:=`, `::` or `..`.
    Operator,
    /// A single `(`, `)`, `[`, `]`, `,`, `;`, `.` or `:` — or any character no other kind takes.
    Punct,
}

/// One token: what it is, and the bytes of the body it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Token {
    /// What it is.
    pub(super) kind: Kind,
    /// Its first byte.
    pub(super) start: usize,
    /// One past its last byte.
    pub(super) end: usize,
}

impl Token {
    /// The token as it was written.
    pub(super) fn text(self, source: &str) -> &str {
        source.get(self.start..self.end).unwrap_or_default()
    }

    /// Whether this is the unquoted word `word`, in any case.
    pub(super) fn is_word(self, source: &str, word: &str) -> bool {
        self.kind == Kind::Word && self.text(source).eq_ignore_ascii_case(word)
    }

    /// Whether this is the punctuation mark `punct`.
    pub(super) fn is_punct(self, source: &str, punct: char) -> bool {
        self.kind == Kind::Punct && self.text(source).chars().eq(std::iter::once(punct))
    }

    /// Whether this is the operator `operator`, exactly.
    pub(super) fn is_operator(self, source: &str, operator: &str) -> bool {
        self.kind == Kind::Operator && self.text(source) == operator
    }

    /// The name an identifier stands for: an unquoted word lower-cased the way PostgreSQL folds it,
    /// a quoted one as written with `""` read as one quote. `None` for any other token.
    pub(super) fn identifier(self, source: &str) -> Option<String> {
        let text = self.text(source);
        match self.kind {
            Kind::Word => Some(text.to_ascii_lowercase()),
            Kind::QuotedIdent => Some(
                text.get(1..text.len().saturating_sub(1))?
                    .replace("\"\"", "\""),
            ),
            _ => None,
        }
    }

    /// The value a string literal stands for, or `None` for any other token.
    ///
    /// `E'…'` reads its backslash escapes and a plain `'…'` reads none — PostgreSQL's reading under
    /// `standard_conforming_strings = on`. A dollar quote is its body, exactly.
    pub(super) fn literal(self, source: &str) -> Option<String> {
        if self.kind != Kind::Literal {
            return None;
        }
        let text = self.text(source);
        let last = text.len().saturating_sub(1);
        match text.as_bytes().first()? {
            b'$' => {
                let tag = text.get(1..)?.find('$')? + 2;
                text.get(tag..text.len().checked_sub(tag)?)
                    .map(str::to_owned)
            }
            b'E' | b'e' => Some(unescape(text.get(2..last)?)),
            _ => {
                let open = text.find('\'')?;
                Some(text.get(open + 1..last)?.replace("''", "'"))
            }
        }
    }
}

/// Splits a body into tokens.
///
/// Refuses only what PostgreSQL's scanner refuses — a string, identifier, dollar quote or comment
/// that never closes — with PostgreSQL's sentence: `unterminated quoted string at or near "…"`,
/// where the text is everything from the opening quote to the end of the body.
pub(super) fn tokenize(source: &str) -> Result<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let start = at;
        let next = bytes.get(at + 1).copied();
        let kind = match byte {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => {
                at += 1;
                continue;
            }
            b'-' if next == Some(b'-') => {
                at = bytes
                    .get(at..)
                    .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
                    .map_or(bytes.len(), |line| at + line);
                continue;
            }
            b'/' if next == Some(b'*') => {
                at = comment_end(source, at)?;
                continue;
            }
            b'\'' => {
                at = quoted_end(source, start, at, b'\'', "unterminated quoted string")?;
                Kind::Literal
            }
            b'"' => {
                at = quoted_end(source, start, at, b'"', "unterminated quoted identifier")?;
                Kind::QuotedIdent
            }
            b'E' | b'e' if next == Some(b'\'') => {
                at = escaped_end(source, start, at + 1)?;
                Kind::Literal
            }
            b'B' | b'b' | b'X' | b'x' | b'N' | b'n' if next == Some(b'\'') => {
                at = quoted_end(source, start, at + 1, b'\'', "unterminated quoted string")?;
                Kind::Literal
            }
            b'U' | b'u' if next == Some(b'&') && bytes.get(at + 2) == Some(&b'\'') => {
                at = quoted_end(source, start, at + 2, b'\'', "unterminated quoted string")?;
                Kind::Literal
            }
            b'$' => {
                let (end, kind) = dollar(source, at)?;
                at = end;
                kind
            }
            b'0'..=b'9' => {
                at = number_end(bytes, at);
                Kind::Number
            }
            b'.' if next.is_some_and(|next| next.is_ascii_digit()) => {
                at = number_end(bytes, at);
                Kind::Number
            }
            b'.' if next == Some(b'.') => {
                at += 2;
                Kind::Operator
            }
            b':' if matches!(next, Some(b'=' | b':')) => {
                at += 2;
                Kind::Operator
            }
            _ if OPERATOR.contains(&byte) => {
                at = operator_end(bytes, at);
                Kind::Operator
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                at = word_end(bytes, at);
                Kind::Word
            }
            // Only an ASCII byte reaches here — every byte of a multi-byte character starts or
            // continues a word — so one byte is always one whole character.
            _ => {
                at += 1;
                Kind::Punct
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: at,
        });
    }
    Ok(tokens)
}

/// PostgreSQL's scanner error: what went wrong, and the body from where it started.
fn unterminated(source: &str, start: usize, problem: &str) -> SqlError {
    SqlError::PlpgsqlSyntax(format!(
        "{problem} at or near \"{}\"",
        source.get(start..).unwrap_or_default()
    ))
}

/// Where a quoted token whose quote is at `open` ends: after its closing quote, where a doubled
/// quote is one quote rather than the end.
fn quoted_end(source: &str, start: usize, open: usize, quote: u8, problem: &str) -> Result<usize> {
    let bytes = source.as_bytes();
    let mut at = open + 1;
    while let Some(&byte) = bytes.get(at) {
        if byte == quote {
            if bytes.get(at + 1) == Some(&quote) {
                at += 2;
                continue;
            }
            return Ok(at + 1);
        }
        at += 1;
    }
    Err(unterminated(source, start, problem))
}

/// Where an `E'…'` string whose quote is at `open` ends: a backslash escapes the character after
/// it, a quote included, and `''` is one quote here too.
fn escaped_end(source: &str, start: usize, open: usize) -> Result<usize> {
    let bytes = source.as_bytes();
    let mut at = open + 1;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\'') => at += 2,
            b'\'' => return Ok(at + 1),
            _ => at += 1,
        }
    }
    Err(unterminated(source, start, "unterminated quoted string"))
}

/// A `$`: a parameter `$1`, a dollar quote `$tag$…$tag$`, or — alone — an operator character.
fn dollar(source: &str, at: usize) -> Result<(usize, Kind)> {
    let bytes = source.as_bytes();
    if bytes.get(at + 1).is_some_and(u8::is_ascii_digit) {
        let mut end = at + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        return Ok((end, Kind::Parameter));
    }
    // The tag is an identifier that does not start with a digit, or nothing at all.
    let mut tag_end = at + 1;
    if bytes
        .get(tag_end)
        .is_some_and(|&byte| byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80)
    {
        while bytes
            .get(tag_end)
            .is_some_and(|&byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80)
        {
            tag_end += 1;
        }
    }
    if bytes.get(tag_end) != Some(&b'$') {
        return Ok((at + 1, Kind::Operator));
    }
    let tag = source.get(at..=tag_end).unwrap_or_default();
    let body = tag_end + 1;
    match source.get(body..).and_then(|rest| rest.find(tag)) {
        Some(close) => Ok((body + close + tag.len(), Kind::Literal)),
        None => Err(unterminated(
            source,
            at,
            "unterminated dollar-quoted string",
        )),
    }
}

/// Where a run of digits starting at `at` ends.
fn digits_end(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    at
}

/// Where a number ends: digits, a fraction, an exponent — and never at the first `.` of `..`, so
/// that `1..3` is a number, an operator and a number.
fn number_end(bytes: &[u8], at: usize) -> usize {
    let mut at = digits_end(bytes, at);
    if bytes.get(at) == Some(&b'.') && bytes.get(at + 1) != Some(&b'.') {
        at = digits_end(bytes, at + 1);
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(at + 1), Some(b'+' | b'-')));
        if bytes.get(at + 1 + sign).is_some_and(u8::is_ascii_digit) {
            at = digits_end(bytes, at + 1 + sign);
        }
    }
    at
}

/// Where a run of operator characters ends, by PostgreSQL's two rules: a comment starting inside
/// the run ends it, and a multi-character operator drops trailing `+` and `-` unless it holds one
/// of `~`, `!`, `@`, `#`, `%`, `^`, `&`, `|`, a backtick or `?`.
fn operator_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while let Some(&byte) = bytes.get(end) {
        if !OPERATOR.contains(&byte) {
            break;
        }
        let next = bytes.get(end + 1).copied();
        if end > start
            && ((byte == b'-' && next == Some(b'-')) || (byte == b'/' && next == Some(b'*')))
        {
            break;
        }
        end += 1;
    }
    let run = bytes.get(start..end).unwrap_or_default();
    if run.len() > 1 && !run.iter().any(|byte| b"~!@#%^&|`?".contains(byte)) {
        while end - start > 1 && matches!(bytes.get(end - 1), Some(b'+' | b'-')) {
            end -= 1;
        }
    }
    end
}

/// Where an unquoted identifier ends: letters, digits, `_`, `$` and any non-ASCII byte.
fn word_end(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(|&byte| {
        byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
    }) {
        at += 1;
    }
    at
}

/// Where a `/* … */` comment that opens at `start` ends. Comments nest, as PostgreSQL's do.
fn comment_end(source: &str, start: usize) -> Result<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0_usize;
    let mut at = start;
    while let Some(&byte) = bytes.get(at) {
        let next = bytes.get(at + 1).copied();
        if byte == b'/' && next == Some(b'*') {
            depth += 1;
            at += 2;
        } else if byte == b'*' && next == Some(b'/') {
            depth = depth.saturating_sub(1);
            at += 2;
            if depth == 0 {
                return Ok(at);
            }
        } else {
            at += 1;
        }
    }
    Err(unterminated(source, start, "unterminated /* comment"))
}

/// An `E'…'` string's escapes, read the way PostgreSQL reads them.
fn unescape(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                }
                out.push('\'');
            }
            '\\' => match chars.next() {
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(digit @ '0'..='7') => {
                    let mut value = digit.to_digit(8).unwrap_or(0);
                    for _ in 0..2 {
                        let Some(more) = chars.peek().and_then(|next| next.to_digit(8)) else {
                            break;
                        };
                        value = value * 8 + more;
                        chars.next();
                    }
                    out.push(char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                Some(kind @ ('x' | 'u' | 'U')) => {
                    let width = match kind {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let mut value = 0_u32;
                    let mut read = 0;
                    while read < width
                        && let Some(more) = chars.peek().and_then(|next| next.to_digit(16))
                    {
                        value = value.wrapping_mul(16).wrapping_add(more);
                        chars.next();
                        read += 1;
                    }
                    if read == 0 {
                        out.push(kind);
                    } else {
                        out.push(char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                }
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}
