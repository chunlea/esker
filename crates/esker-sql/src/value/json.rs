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
use crate::value::{ColumnType, Datum, PgDatum as _, PgType as _};

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
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(text) > crate::value::INLINE_VALUE_DEPTH {
        let owned = text.to_owned();
        return crate::value::on_a_deep_stack("json", move || validate_inner(&owned));
    }
    validate_inner(text)
}

fn validate_inner(text: &str) -> Result<()> {
    parse(text, Nulls::Allow).map(|_| ())
}

/// `text` as the canonical form `jsonb` stores and prints.
///
/// Three normalisations at once, and an implementation that did any two would still be wrong:
/// keys are reordered **by length, then bytes**, duplicates are dropped with the **last** winning,
/// and every colon and comma is followed by exactly one space.
pub(crate) fn canonicalise(text: &str) -> Result<String> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(text) > crate::value::INLINE_VALUE_DEPTH {
        let owned = text.to_owned();
        return crate::value::on_a_deep_stack("json", move || canonicalise_inner(&owned));
    }
    canonicalise_inner(text)
}

fn canonicalise_inner(text: &str) -> Result<String> {
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
/// Whether `left` contains `right`, as `jsonb`'s `@>`.
///
/// **The array-contains-scalar exception is top level only, and it is scalars only.** `'[2]' @> '2'`
/// is `t` while `'{"a":[1,2]}' @> '{"a":1}'` is `f` and `'[{"a":1}]' @> '{"a":1}'` is `f` — so the
/// exception cannot live in the recursion, which is where a reading of the documentation would put
/// it. Measured, all four, in `tests/captures/pg19_containment.txt`.
///
/// That one placement is also what makes nesting match in both directions: with the exception out
/// of the recursion, `'[[1,2]]' @> '[1,2]'` is `f` because a scalar on the right cannot match an
/// array on the left, and `'[1,2]' @> '[[1,2]]'` is `f` because an array on the right cannot match
/// a scalar. A containment that flattens is wrong twice, and both are cells here.
pub(crate) fn contains(left: &str, right: &str) -> Result<bool> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(left).max(nesting_of(right)) > crate::value::INLINE_VALUE_DEPTH {
        let (left, right) = (left.to_owned(), right.to_owned());
        return crate::value::on_a_deep_stack("json", move || contains_inner(&left, &right));
    }
    contains_inner(left, right)
}

fn contains_inner(left: &str, right: &str) -> Result<bool> {
    let (left, right) = (parse(left, Nulls::Refuse)?, parse(right, Nulls::Refuse)?);
    // The exception: an array contains a bare scalar it holds. Only here, only a scalar.
    if let (Json::Array(items), Json::Null | Json::Bool(_) | Json::Number(_) | Json::Str(_)) =
        (&left, &right)
    {
        return Ok(items.iter().any(|item| within(item, &right)));
    }
    Ok(within(&left, &right))
}

/// The recursion, which has no exceptions: like matches like, or nothing matches.
fn within(left: &Json, right: &Json) -> bool {
    match (left, right) {
        // Every element on the right must be inside *some* element on the left — so order and
        // duplicates are free, and `'[1,2]' @> '[]'` is vacuously true.
        (Json::Array(a), Json::Array(b)) => b.iter().all(|r| a.iter().any(|l| within(l, r))),
        // Every pair on the right must be present on the left, and its value contained rather
        // than merely equal: `'{"a":{"b":1,"c":2}}' @> '{"a":{"b":1}}'`.
        (Json::Object(a), Json::Object(b)) => b.iter().all(|(key, value)| {
            a.iter()
                .any(|(k, v)| key_order(k, key) == std::cmp::Ordering::Equal && within(v, value))
        }),
        // A container never matches a scalar in here, and that is the whole of the nesting rule.
        (Json::Array(_) | Json::Object(_), _) | (_, Json::Array(_) | Json::Object(_)) => false,
        // Two scalars: contained means equal, and equal is the document's equality, so `1.0`
        // contains `1.00`.
        _ => order(left, right) == std::cmp::Ordering::Equal,
    }
}

/// Where `left` sorts against `right` as two `jsonb` values.
///
/// **Written against the measured cells and not derived from a model**, because the model that
/// explains the ordering is false: "a scalar is internally a one-element array, so a zero-length
/// array wins on length" accounts for every ordering cell on 19beta1 and predicts `[null] = null`
/// and `[1] = 1`, which are both `f`. See `tests/captures/pg19_jsonb_order.txt`, where the two
/// refuting rows sit beside the cells they refute.
///
/// So the rank below is the measurement: `[] < null < string < number < boolean < array < object`,
/// with **the empty array below every scalar** and every other array above every scalar. Within a
/// kind: numbers compare as `numeric` (`1.0 = 1.00`, `2 < 10`, where text says otherwise), arrays
/// by length before contents (`[2] < [1,1]`), objects by pair count before keys and then values.
/// Key order is not part of the value because [`canonicalise`] already sorted it.
pub(crate) fn compare(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(left).max(nesting_of(right)) > crate::value::INLINE_VALUE_DEPTH {
        let (left, right) = (left.to_owned(), right.to_owned());
        return crate::value::on_a_deep_stack("json", move || compare_inner(&left, &right));
    }
    compare_inner(left, right)
}

fn compare_inner(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    Ok(order(
        &parse(left, Nulls::Refuse)?,
        &parse(right, Nulls::Refuse)?,
    ))
}

/// The rank a value sorts in, measured — see [`compare`].
fn rank(value: &Json) -> u8 {
    match value {
        // Not a mistake and not derivable: `'[]' < 'null'` while `'[1]' > 'true'`.
        Json::Array(items) if items.is_empty() => 0,
        Json::Null => 1,
        Json::Str(_) => 2,
        Json::Number(_) => 3,
        Json::Bool(_) => 4,
        Json::Array(_) => 5,
        Json::Object(_) => 6,
    }
}

fn order(left: &Json, right: &Json) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match rank(left).cmp(&rank(right)) {
        Ordering::Equal => {}
        other => return other,
    }
    match (left, right) {
        (Json::Bool(a), Json::Bool(b)) => a.cmp(b),
        // As `numeric`, which is the whole reason this is not a text comparison.
        (Json::Number(a), Json::Number(b)) => {
            match (
                crate::value::numeric::from_text(a),
                crate::value::numeric::from_text(b),
            ) {
                (Ok(a), Ok(b)) => crate::value::numeric::pg_cmp(&a, &b),
                // Unreachable for a parsed document; ordering by the canonical digits is the
                // honest fallback rather than a panic on a value that has already been accepted.
                _ => a.cmp(b),
            }
        }
        (Json::Str(a), Json::Str(b)) => a.cmp(b),
        (Json::Array(a), Json::Array(b)) => a
            .len()
            .cmp(&b.len())
            .then_with(|| compare_in_order(a.iter().zip(b.iter()))),
        (Json::Object(a), Json::Object(b)) => a.len().cmp(&b.len()).then_with(|| {
            // Keys before values, and both in the canonical key order.
            a.iter()
                .zip(b.iter())
                .find_map(|((ka, _), (kb, _))| match key_order(ka, kb) {
                    Ordering::Equal => None,
                    other => Some(other),
                })
                .unwrap_or_else(|| {
                    compare_in_order(a.iter().map(|(_, v)| v).zip(b.iter().map(|(_, v)| v)))
                })
        }),
        // Two values of one rank are one kind, and the three that reach here — two nulls, two
        // empty arrays, and a rank shared by nothing else — are equal by having nothing to
        // compare.
        _ => Ordering::Equal,
    }
}

fn compare_in_order<'a>(pairs: impl Iterator<Item = (&'a Json, &'a Json)>) -> std::cmp::Ordering {
    for (a, b) in pairs {
        match order(a, b) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

pub(crate) fn concat(left: &str, right: &str) -> Result<String> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(left).max(nesting_of(right)) > crate::value::INLINE_VALUE_DEPTH {
        let (left, right) = (left.to_owned(), right.to_owned());
        return crate::value::on_a_deep_stack("json", move || concat_inner(&left, &right));
    }
    concat_inner(left, right)
}

fn concat_inner(left: &str, right: &str) -> Result<String> {
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
/// [`Key`] without the borrow, so a deep fetch can cross a thread boundary (#101). Two words and
/// an `i64`; the shallow path never builds one.
enum OwnedKey {
    Member(String),
    At(i64),
}

pub(crate) fn fetch(text: &str, key: Option<&Key<'_>>, as_text: bool) -> Result<Option<String>> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is walked
    // and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(text) > crate::value::INLINE_VALUE_DEPTH {
        let owned = text.to_owned();
        let key = match key {
            Some(Key::Member(name)) => Some(OwnedKey::Member((*name).to_owned())),
            Some(Key::At(at)) => Some(OwnedKey::At(*at)),
            None => None,
        };
        return crate::value::on_a_deep_stack("json", move || {
            let borrowed = match &key {
                Some(OwnedKey::Member(name)) => Some(Key::Member(name)),
                Some(OwnedKey::At(at)) => Some(Key::At(*at)),
                None => None,
            };
            fetch_inner(&owned, borrowed.as_ref(), as_text)
        });
    }
    fetch_inner(text, key, as_text)
}

fn fetch_inner(text: &str, key: Option<&Key<'_>>, as_text: bool) -> Result<Option<String>> {
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

/// The nesting this text reaches, counted **without recursing** — one pass over the bytes, so the
/// decision below costs nothing a deep value would not already pay.
///
/// Strings are skipped, because a `[` inside one is a character and not a level. An over-estimate
/// would be harmless (it spawns a thread that was not needed) and an under-estimate would not, so
/// the escape handling is the careful half.
fn nesting_of(text: &str) -> usize {
    let (mut depth, mut deepest, mut in_string, mut escaped) = (0_usize, 0_usize, false, false);
    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

fn parse(text: &str, nulls: Nulls) -> Result<Json> {
    // **The branch is not here**, though it was: parsing on a big stack and handing the tree back
    // leaves it to be walked and freed on the caller's, one frame per level either way, and the
    // probe measured exactly that — the child stopped dying at 750 levels and started dying at
    // 4,500. It is the public entries below that each take the whole operation to a sized thread,
    // because each of them returns something non-recursive and can therefore let the tree die where
    // it was born (`debts-v1.1.md` #101).
    let mut parser = Parser {
        rest: text,
        whole: text,
        nulls,
    };
    parser.skip_space();
    let value = parser.value(0)?;
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

    /// One value, at `depth` containers deep.
    ///
    /// **The single place the recursion turns**, which is why the bound is here and not in the two
    /// containers: every nested value reaches this line (`debts-v1.1.md` #101). Past it the answer
    /// is `54001`, which is what PostgreSQL says when `max_stack_depth` is exceeded — and the stack
    /// this runs on was sized for the bound, so reaching it means the value really is too deep
    /// rather than the thread being too small.
    fn value(&mut self, depth: usize) -> Result<Json> {
        if depth > crate::value::MAX_VALUE_DEPTH {
            return Err(SqlError::StatementTooComplex);
        }
        match self.peek().ok_or_else(|| self.fail())? {
            '{' => self.object(depth),
            '[' => self.array(depth),
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

    fn array(&mut self, depth: usize) -> Result<Json> {
        self.eat('[')?;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(']') {
            self.rest = &self.rest[1..];
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value(depth + 1)?);
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
    fn object(&mut self, depth: usize) -> Result<Json> {
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
            let value = self.value(depth + 1)?;
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

/// The targets a `jsonb` has a cast to, out of `pg_cast`.
///
/// Seven, and `text` is deliberately not one of them: `jsonb::text` is the document's own text and
/// is `ToText`'s business, not a scalar extraction.
pub(crate) fn casts_to_scalar(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Bool
            | ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Numeric
    )
}

/// A `jsonb` cast to one of [`casts_to_scalar`]'s targets — **the kind is checked before the value
/// is read**.
///
/// That order is the whole of it. PostgreSQL asks what kind of JSON it has and only then hands the
/// digits to a number's input function, so `'{"a":1}'::jsonb::numeric` is
/// `22023 cannot cast jsonb object to type numeric`. This node used to hand the *document* to the
/// target's input function and answer `22P02 invalid input syntax` — a refusal either way, and a
/// different one to a client that branches on `SQLSTATE` (`debts-v1.1.md` #44, group 5).
///
/// Measured on 19beta1, 2026-09-10, one statement per kind:
///
/// ```text
/// '1'::jsonb::int4        1          '1.5'::jsonb::int4    2      — rounds, as numeric does
/// 'true'::jsonb::bool     t          'true'::jsonb::int4   cannot cast jsonb boolean to type integer
/// 'null'::jsonb::int4     NULL       '"x"'::jsonb::int4    cannot cast jsonb string to type integer
/// '[1]'::jsonb::int4      cannot cast jsonb array to type integer
/// '1'::jsonb::bool        cannot cast jsonb numeric to type boolean
/// '99999999999'::jsonb::int2   smallint out of range      — the ordinary 22003, not this one
/// ```
///
/// **Three of those a reader would get wrong.** A JSON `null` is not refused at all — it is SQL
/// NULL. A number's kind word is **`numeric`**, not `number`. And a value that is the right kind
/// and the wrong size is the number's own `22003`, because by then the shape check has passed and
/// this function is out of the way.
pub(crate) fn cast_to_scalar(text: &str, to: ColumnType) -> Result<Datum> {
    // **The whole operation on a sized thread when the value is deep** (#101): the tree is
    // walked and freed wherever it is last owned, so only this answer crosses back.
    if nesting_of(text) > crate::value::INLINE_VALUE_DEPTH {
        let owned = text.to_owned();
        return crate::value::on_a_deep_stack("json", move || cast_to_scalar_inner(&owned, to));
    }
    cast_to_scalar_inner(text, to)
}

fn cast_to_scalar_inner(text: &str, to: ColumnType) -> Result<Datum> {
    match parse(text, Nulls::Refuse)? {
        // The one kind with no refusal: a JSON null is an SQL NULL, whatever the target.
        Json::Null => Ok(Datum::Null),
        Json::Bool(flag) if to == ColumnType::Bool => Ok(Datum::Bool(flag)),
        // A number reaches the target's own conversion, which is where its range error belongs.
        Json::Number(digits) if to != ColumnType::Bool => {
            let number = super::numeric::from_text(&digits)?;
            match to {
                ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
                    super::numeric_to_integer(number, to)
                }
                _ => Datum::from_text(to, &digits),
            }
        }
        other => Err(SqlError::CannotCastJsonbShape {
            kind: kind_name(&other),
            to: to.name(),
        }),
    }
}

/// PostgreSQL's word for a JSON kind, as its refusals write it.
///
/// **`numeric` and not `number`**, which is the one a reader guesses wrong: measured,
/// `'1'::jsonb::bool` is `cannot cast jsonb numeric to type boolean`.
fn kind_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "numeric",
        Json::Str(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    /// **Every containment cell of the capture, checked against the implementation.**
    ///
    /// Same rule as the ordering test below: the capture is the specification, so the test reads
    /// it rather than restating it.
    #[test]
    fn every_measured_containment_cell_agrees() {
        let capture = include_str!("../../tests/captures/pg19_containment.txt");
        let mut checked = 0;
        for line in capture.lines().filter(|line| !line.starts_with('#')) {
            let mut fields = line.split('\t');
            let (Some(statement), Some(_), Some(rows)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(inner) = statement
                .strip_prefix("SELECT ('")
                .and_then(|rest| rest.strip_suffix(") AS v"))
            else {
                continue;
            };
            let Some((left, rest)) = inner.split_once("'::jsonb ") else {
                continue;
            };
            let Some((op, right)) = rest.split_once(" '") else {
                continue;
            };
            let Some(right) = right.strip_suffix("'::jsonb") else {
                continue;
            };
            // `<@` is `@>` with the operands the other way round — measured, and the reason one
            // implementation answers both.
            let ours = match op {
                "@>" => super::contains(left, right),
                "<@" => super::contains(right, left),
                _ => continue,
            }
            .expect("both sides parse");
            assert_eq!(ours, rows == "t", "{left} {op} {right}");
            checked += 1;
        }
        assert!(
            checked > 28,
            "only {checked} cells read; the capture did not load"
        );
    }

    /// **Every cell of the capture, checked against the implementation.**
    ///
    /// The capture is the specification for this family — the header says to implement against the
    /// cells and not to derive them — so the test reads the cells rather than restating them. A row
    /// that is re-measured differently on a later PostgreSQL fails here without anybody editing a
    /// list.
    #[test]
    fn every_measured_cell_agrees() {
        let capture = include_str!("../../tests/captures/pg19_jsonb_order.txt");
        let mut checked = 0;
        for line in capture.lines().filter(|line| !line.starts_with('#')) {
            let mut fields = line.split('\t');
            let (Some(statement), Some(_), Some(rows)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            // `SELECT ('<a>'::jsonb <op> '<b>'::jsonb) AS v`
            let Some(inner) = statement
                .strip_prefix("SELECT ('")
                .and_then(|rest| rest.strip_suffix(") AS v"))
            else {
                continue;
            };
            let Some((left, rest)) = inner.split_once("'::jsonb ") else {
                continue;
            };
            let Some((op, right)) = rest.split_once(" '") else {
                continue;
            };
            let Some(right) = right.strip_suffix("'::jsonb") else {
                continue;
            };
            let ordering = super::compare(left, right).expect("both sides parse");
            let ours = match op {
                "<" => ordering == Ordering::Less,
                ">" => ordering == Ordering::Greater,
                "=" => ordering == Ordering::Equal,
                _ => continue,
            };
            assert_eq!(
                ours,
                rows == "t",
                "{left} {op} {right}: PostgreSQL says {rows}"
            );
            checked += 1;
        }
        assert!(
            checked > 45,
            "only {checked} cells read; the capture did not load"
        );
    }
}
