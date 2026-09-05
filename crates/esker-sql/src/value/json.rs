//! `json` and `jsonb`: validating one and canonicalising the other.
//!
//! Two types over one representation, and almost every fact about them is a *difference*
//! ([ADR 0042](../../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)).
//! `json` is a **validated string** — the text you sent, byte for byte, whitespace and duplicate
//! keys and all. `jsonb` is a value, stored and printed in the canonical form PostgreSQL prints:
//! keys reordered, duplicates dropped, separators normalised, numbers as `numeric` writes them.
//!
//! Everything here was measured against 19beta1 and lives in `tests/corpus/pg19_json.txt`.
//!
//! **What is deliberately not here: comparison.** `jsonb` has a total order — by kind first
//! (`Object > Array > Boolean > Number > String > Null`), then by value, with numbers compared
//! numerically — and this module does not implement it, because nothing can call it yet: a `jsonb`
//! value is a `Datum::Text` and the type is gone by the time two are compared. Comparison is
//! refused (`0A000`) rather than answered from the bytes, which would say `f` for
//! `'1.0'::jsonb = '1.00'::jsonb` where a real server says `t`. The order is recorded in ADR 0042
//! and measured in the corpus; the code for it belongs to the unit that gives `jsonb` a `Datum` of
//! its own and can therefore reach it.

use std::fmt::Write as _;

use crate::error::{Result, SqlError};

/// One JSON value, parsed. The shape a canonical form is written from.
///
/// Numbers keep their **text**, not a parsed float: `1.00` prints `1.00` and `1e2` prints `100`,
/// which are `numeric`'s rules for that input rather than any float's. Two numbers that print
/// differently can still be equal — the whole of ADR 0042's difficulty — so the text is what is
/// stored, and comparing two of them is the thing this module deliberately does not do.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// The digits as `numeric` would print them.
    Number(String),
    Str(String),
    Array(Vec<Json>),
    /// Already deduplicated and in PostgreSQL's key order.
    Object(Vec<(String, Json)>),
}

/// Whether `text` is a JSON document, leaving it exactly as it is.
///
/// What a `json` column does on the way in, and the whole of what the type promises. A NUL escape
/// is **accepted** here and refused by `jsonb`, which is the one input that tells the two apart.
pub(crate) fn validate(text: &str) -> Result<()> {
    parse(text, Nulls::Allow).map(|_| ())
}

/// `text` as the canonical form `jsonb` stores and prints.
///
/// Three normalisations at once, and an implementation that did any two would still be wrong:
/// keys are reordered **by length, then bytes**, duplicates are dropped with the **last** winning,
/// and every colon and comma is followed by exactly one space.
pub(crate) fn canonicalise(text: &str) -> Result<String> {
    let value = parse(text, Nulls::Refuse)?;
    let mut out = String::with_capacity(text.len());
    write_canonical(&value, &mut out);
    Ok(out)
}

/// PostgreSQL's `jsonb` key order: **length first, then bytes**.
///
/// Not the lexicographic order a reader expects — `{"z":1,"aa":2}` stores as `{"z": 1, "aa": 2}`
/// because `z` is shorter. One function so the parser's sort and [`concat`]'s insertion agree by
/// construction: a merge that inserted in a different order from the one the parser produced would
/// write a document that no longer round-trips.
fn key_order(left: &str, right: &str) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

/// `jsonb || jsonb`: PostgreSQL's document concatenation.
///
/// **Two objects merge; anything else concatenates as arrays.** Measured on 19beta1, every
/// combination:
///
/// | left | right | answer |
/// |---|---|---|
/// | `{"a":1,"b":2}` | `{"b":3,"c":4}` | `{"a": 1, "b": 3, "c": 4}` — the **right** wins a shared key |
/// | `{"a":{"x":1}}` | `{"a":{"y":2}}` | `{"a": {"y": 2}}` — **not** a deep merge |
/// | `[1,2]` | `[3]` | `[1, 2, 3]` |
/// | `[1,2]` | `3` | `[1, 2, 3]` — a scalar is a one-element array |
/// | `{"a":1}` | `[1]` | `[{"a": 1}, 1]` — an object beside an array is an *element* |
/// | `"x"` | `"y"` | `["x", "y"]` — two scalars make an array rather than an error |
/// | `null` | `null` | `[null, null]` |
///
/// So there is exactly one special case and one rule: both objects, or both sides read as arrays.
/// Reasoning would put the object case last and make `{"a":1} || [1]` a merge of a key into a
/// list; measurement puts it first and makes everything else a concatenation.
///
/// Both arguments are already canonical — this is only ever called on stored `jsonb` — so parsing
/// cannot fail on anything a caller can reach, and a failure is returned rather than assumed away.
pub(crate) fn concat(left: &str, right: &str) -> Result<String> {
    let left = parse(left, Nulls::Refuse)?;
    let right = parse(right, Nulls::Refuse)?;
    let merged = match (left, right) {
        (Json::Object(mut into), Json::Object(from)) => {
            for (key, value) in from {
                match into.binary_search_by(|(existing, _)| key_order(existing, &key)) {
                    // **The right operand wins**, which is the half of the rule a set union would
                    // get backwards.
                    Ok(at) => into[at].1 = value,
                    Err(at) => into.insert(at, (key, value)),
                }
            }
            Json::Object(into)
        }
        (left, right) => {
            let mut out = elements(left);
            out.extend(elements(right));
            Json::Array(out)
        }
    };
    let mut out = String::new();
    write_canonical(&merged, &mut out);
    Ok(out)
}

/// `doc -> key` and `doc ->> key`: one member of an object, or one element of an array.
///
/// `None` is SQL NULL. **The two operators differ in exactly two places**, both measured on
/// 19beta1 (`captures/pg19_json_fetch.txt`):
///
/// * a JSON **null** member is the string `null` through `->` and SQL NULL through `->>` — so
///   `->>` cannot tell a missing key from a null one and `->` can;
/// * a **string** member loses its quotes through `->>` and keeps them through `->`. Every other
///   kind of value renders the same both ways, which is why this is one function with a flag
///   rather than two that would drift.
///
/// A subscript indexes an array, **counting from the end when it is negative** (`'[10,20]' ->> -1`
/// is `20`), and is out of range rather than an error when it does not land. A key against an
/// array, a subscript against an object, or either against a scalar is NULL and never an error —
/// `'{"b":"b"}'::jsonb -> 'b' -> 'x'` is NULL, not `22023`.
pub(crate) fn fetch(text: &str, key: Option<&Key<'_>>, as_text: bool) -> Result<Option<String>> {
    let Some(key) = key else { return Ok(None) };
    let found = match (parse(text, Nulls::Allow)?, key) {
        (Json::Object(members), Key::Member(name)) => members
            .into_iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| value),
        (Json::Array(items), Key::At(at)) => {
            let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
            // Negative counts back from the end; `-len` is the first element and anything below
            // it misses, exactly as anything at or above `len` does.
            let at = if *at < 0 { len + at } else { *at };
            usize::try_from(at)
                .ok()
                .and_then(|at| items.into_iter().nth(at))
        }
        // A key against an array, a subscript against an object, and either against a scalar.
        _ => None,
    };
    Ok(match found {
        None => None,
        // The two differences, and the whole of them.
        Some(Json::Null) if as_text => None,
        Some(Json::Str(value)) if as_text => Some(value),
        Some(value) => {
            let mut out = String::new();
            write_canonical(&value, &mut out);
            Some(out)
        }
    })
}

/// Which member a fetch is asking for: a name, or a position in an array.
pub(crate) enum Key<'a> {
    /// `doc -> 'name'`.
    Member(&'a str),
    /// `doc -> 2`, which counts from the end when negative.
    At(i64),
}

/// What one side contributes to an array concatenation: an array's own elements, or itself.
fn elements(value: Json) -> Vec<Json> {
    match value {
        Json::Array(items) => items,
        other => vec![other],
    }
}

/// Whether a NUL escape is a value or an error, which is the one input that splits the types.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Nulls {
    /// `json`: stored as sent.
    Allow,
    /// `jsonb`: `22P05`, because its stored form is text and a NUL cannot be in one.
    Refuse,
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(digits) => out.push_str(digits),
        Json::Str(text) => write_string(text, out),
        Json::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Json::Object(entries) => {
            out.push('{');
            for (at, (key, item)) in entries.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_canonical(item, out);
            }
            out.push('}');
        }
    }
}

/// A string, escaped the way PostgreSQL prints one: the six named escapes, `\uXXXX` for the other
/// control characters, and everything else as itself — an `e`-acute in the input comes back as
/// itself rather than as an escape.
/// One JSON string literal, quotes and escapes included.
///
/// Shared with `EXPLAIN (FORMAT JSON)`, which builds a document rather than parsing one and needs
/// exactly this escaping and no other.
pub(crate) fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// `22P02`, which every malformed document gets.
fn invalid(text: &str) -> SqlError {
    SqlError::InvalidTextRepresentation {
        ty: "json",
        value: text.to_owned(),
    }
}

fn parse(text: &str, nulls: Nulls) -> Result<Json> {
    let mut parser = Parser {
        rest: text,
        whole: text,
        nulls,
    };
    parser.skip_space();
    let value = parser.value()?;
    parser.skip_space();
    if !parser.rest.is_empty() {
        return Err(invalid(text));
    }
    Ok(value)
}

struct Parser<'a> {
    rest: &'a str,
    whole: &'a str,
    nulls: Nulls,
}

impl Parser<'_> {
    fn fail(&self) -> SqlError {
        invalid(self.whole)
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t', '\n', '\r']);
    }

    fn eat(&mut self, want: char) -> Result<()> {
        let mut chars = self.rest.chars();
        if chars.next() == Some(want) {
            self.rest = chars.as_str();
            Ok(())
        } else {
            Err(self.fail())
        }
    }

    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    fn value(&mut self) -> Result<Json> {
        match self.peek().ok_or_else(|| self.fail())? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self.string().map(Json::Str),
            't' => self.literal("true").map(|()| Json::Bool(true)),
            'f' => self.literal("false").map(|()| Json::Bool(false)),
            'n' => self.literal("null").map(|()| Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str) -> Result<()> {
        self.rest = self.rest.strip_prefix(word).ok_or_else(|| self.fail())?;
        Ok(())
    }

    fn array(&mut self) -> Result<Json> {
        self.eat('[')?;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(']') {
            self.rest = &self.rest[1..];
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value()?);
            self.skip_space();
            match self.peek() {
                Some(',') => self.rest = &self.rest[1..],
                Some(']') => {
                    self.rest = &self.rest[1..];
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.fail()),
            }
        }
    }

    /// An object, **deduplicated and sorted** as it is built.
    ///
    /// Last key wins — `{"a":1,"a":2}` is `{"a": 2}` — and the order is by key **length first,
    /// then bytes**. One example cannot tell that rule from plain lexicographic order, so the
    /// corpus carries two: `{"bb":1,"a":2,"ccc":3}` and `{"ab":1,"ba":2,"aa":3}`.
    fn object(&mut self) -> Result<Json> {
        self.eat('{')?;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.rest = &self.rest[1..];
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            self.eat(':')?;
            self.skip_space();
            let value = self.value()?;
            match entries.iter_mut().find(|(seen, _)| *seen == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
            self.skip_space();
            match self.peek() {
                Some(',') => self.rest = &self.rest[1..],
                Some('}') => {
                    self.rest = &self.rest[1..];
                    entries.sort_by(|(left, _), (right, _)| key_order(left, right));
                    return Ok(Json::Object(entries));
                }
                _ => return Err(self.fail()),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.eat('"')?;
        let mut out = String::new();
        loop {
            let character = self.next_char()?;
            match character {
                '"' => return Ok(out),
                '\\' => {
                    let escape = self.next_char()?;
                    match escape {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        // A NUL that `json` accepts pushes nothing: it cannot go in a Rust
                        // `String`, and it is never printed back — `json` returns the *original
                        // text*, which still holds the escape as the six characters sent.
                        'u' => {
                            if let Some(decoded) = self.unicode_escape()? {
                                out.push(decoded);
                            }
                        }
                        _ => return Err(self.fail()),
                    }
                }
                other => out.push(other),
            }
        }
    }

    /// `\uXXXX`, or `None` for the NUL that only `json` accepts.
    ///
    /// This is where the two types part company, and it is the only input that does: `jsonb`
    /// answers `22P05` because its stored form is text and a NUL cannot be in one, while `json`
    /// stores the document unchanged. Casting that stored `json` to `jsonb` raises the same error
    /// later, which is what makes `json`'s permissiveness safe rather than a trap.
    fn unicode_escape(&mut self) -> Result<Option<char>> {
        let digits = self.rest.get(..4).ok_or_else(|| self.fail())?;
        let code = u32::from_str_radix(digits, 16).map_err(|_| self.fail())?;
        self.rest = &self.rest[4..];
        if code == 0 {
            return match self.nulls {
                Nulls::Allow => Ok(None),
                Nulls::Refuse => Err(SqlError::UnsupportedUnicodeEscape),
            };
        }
        char::from_u32(code).map(Some).ok_or_else(|| self.fail())
    }

    fn next_char(&mut self) -> Result<char> {
        let mut chars = self.rest.chars();
        let character = chars.next().ok_or_else(|| self.fail())?;
        self.rest = chars.as_str();
        Ok(character)
    }

    /// A number, kept as the text `numeric` would print for it.
    ///
    /// `1.00` stays `1.00` — the trailing zero is `numeric`'s scale and PostgreSQL preserves it —
    /// while `1e2` becomes `100` and `1E400` becomes four hundred digits, because an exponent is
    /// not part of what `numeric` prints. That asymmetry is measured, not chosen.
    fn number(&mut self) -> Result<Json> {
        let taken = self
            .rest
            .find(|c: char| !matches!(c, '0'..='9' | '-' | '+' | '.' | 'e' | 'E'))
            .unwrap_or(self.rest.len());
        let (digits, rest) = self.rest.split_at(taken);
        if digits.is_empty() {
            return Err(self.fail());
        }
        self.rest = rest;
        let text = numeric_text(digits).ok_or_else(|| self.fail())?;
        Ok(Json::Number(text))
    }
}

/// A JSON number as `numeric` prints it: the exponent expanded, the scale kept.
fn numeric_text(digits: &str) -> Option<String> {
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (digits, 0),
    };
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa.strip_prefix('+').unwrap_or(mantissa)),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (mantissa, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    // The decimal point moved by the exponent, which is all an exponent is.
    let mut all: Vec<u8> = whole.bytes().chain(fraction.bytes()).collect();
    let point = i64::try_from(whole.len()).ok()? + i64::from(exponent);
    let mut text = String::new();
    text.push_str(sign);
    if point <= 0 {
        text.push_str("0.");
        for _ in 0..-point {
            text.push('0');
        }
        text.push_str(std::str::from_utf8(&all).ok()?);
    } else {
        let point = usize::try_from(point).ok()?;
        while all.len() < point {
            all.push(b'0');
        }
        let (left, right) = all.split_at(point);
        text.push_str(std::str::from_utf8(left).ok()?);
        if !right.is_empty() {
            text.push('.');
            text.push_str(std::str::from_utf8(right).ok()?);
        }
    }
    Some(text)
}
