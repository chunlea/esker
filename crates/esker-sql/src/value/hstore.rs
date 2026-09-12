//! `hstore`: a map of text to nullable text, as PostgreSQL's extension defines it.
//!
//! # One representation, two types
//!
//! An hstore is stored as its **canonical text** in a [`crate::value::Datum::Text`], the way
//! `json` and `jsonb` already are (`Datum::fits`): there is nothing in an hstore-shaped datum that
//! a `Text` does not already hold, and what differs is the column's declared type. That is what makes equality,
//! ordering, grouping and an index over an hstore column fall out of the text machinery already
//! here — and it is *correct* rather than convenient, because the canonical form is a function of
//! the content: two hstores are equal exactly when their canonical forms are.
//!
//! # The canonical form, measured rather than assumed
//!
//! `hstore_out` is not an echo of the input. Every key and value is quoted, pairs are joined with
//! `, `, a NULL value is the bare word `NULL`, and the pairs are ordered — **by the key's length
//! first and its bytes second**. That last rule is the one plain reasoning gets backwards, and one
//! statement decides it: `'z=>1, aa=>2'` prints `"z"=>"1", "aa"=>"2"`, where any byte-wise sort
//! puts `aa` first. Within a single length it *is* byte order, so a corpus of same-length keys
//! agrees with the wrong rule; `tests/corpus/pg19_hstore.txt` has both.
//!
//! # Two opposite rules for one collision
//!
//! A repeated key **in one literal keeps the first** — `'a=>1, a=>2'` is `"a"=>"1"` — and `||`
//! keeps the **right**: `'a=>1' || 'a=>2'` is `"a"=>"2"`. Both measured, and neither follows from
//! the other.

use std::collections::BTreeMap;

use crate::error::{Result, SqlError};

/// One parsed hstore: its pairs, in the order the canonical form prints them.
///
/// A `BTreeMap` over [`Key`] rather than over `String`, because the order is the whole point and
/// it is not `String`'s.
pub type Hstore = BTreeMap<Key, Option<String>>;

/// An hstore key, ordered the way `hstore_out` orders one: **length first, then bytes**.
///
/// A newtype rather than a `String` so the ordering cannot be got wrong by whoever collects the
/// pairs next — deriving `Ord` on the `String` would compile, pass every same-length test, and
/// print `"aa"` before `"z"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key(pub String);

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.as_bytes().cmp(other.0.as_bytes()))
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Reads an hstore literal, or the `42601` a real server answers for one it cannot.
///
/// **`42601`, a syntax error**, and not the `22P02` every other bad literal in this crate gives:
/// measured, `'a'::hstore` is `syntax error in hstore: unexpected end of string`. The extension
/// reads its own input and reports it as syntax, which is worth keeping because a client that
/// distinguishes the two learns something true from it.
pub fn from_text(text: &str) -> Result<Hstore> {
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0;
    let mut out = Hstore::new();
    loop {
        skip_space(&bytes, &mut at);
        if at >= bytes.len() {
            return Ok(out);
        }
        // **A key is never NULL**: a bare `NULL` on the left of `=>` is the four-character
        // string, and only a value reads it as nothing. The same word means different things on
        // the two sides of the arrow, measured.
        let key =
            token(&bytes, &mut at, KeySide)?.ok_or_else(|| syntax("unexpected end of string"))?;
        skip_space(&bytes, &mut at);
        if !eat(&bytes, &mut at, "=>") {
            return Err(syntax(if at >= bytes.len() {
                "unexpected end of string"
            } else {
                "unexpected character"
            }));
        }
        skip_space(&bytes, &mut at);
        let value = token(&bytes, &mut at, ValueSide)?;
        // **The first of a repeated key wins**, which is the opposite of what `||` does. Measured:
        // `'a=>1, a=>2'` is `"a"=>"1"`.
        out.entry(Key(key)).or_insert(value);
        skip_space(&bytes, &mut at);
        if at >= bytes.len() {
            return Ok(out);
        }
        if bytes[at] == ',' {
            at += 1;
            continue;
        }
        return Err(syntax("unexpected character"));
    }
}

/// The pairs a `text[]` stands for — `hstore(text[])`, which is the function the `pg_cast` row
/// `text[] -> hstore` calls (`e`, by function; measured in `pg_cast` on 19beta1, 2026-09-11).
///
/// The array is read **key, value, key, value**, so:
///
/// ```text
/// '{a,1}'::text[]::hstore        "a"=>"1"
/// '{a,1,b,2}'::text[]::hstore    "a"=>"1", "b"=>"2"
/// '{}'::text[]::hstore           the empty hstore
/// '{a,NULL}'::text[]::hstore     "a"=>NULL        a NULL value is a value
/// '{a,1,a,2}'::text[]::hstore    "a"=>"1"         the first of a repeated key wins
/// '{bb,2,a,1,ccc,3}'            "a"=>"1", "bb"=>"2", "ccc"=>"3"   canonical order, not written
/// '{x}'::text[]::hstore          2202E array must have even number of elements
/// '{NULL,1}'::text[]::hstore     22004 null value not allowed for hstore key
/// ```
///
/// **The odd-length refusal is about the array**, which is what says a real server attempted the
/// conversion at all rather than refusing the pair of types — `42846 cannot cast type text[] to
/// hstore` was this node's answer and was the last open row of the cast matrix
/// (`tests/captures/pg19_cast_matrix.txt`).
///
/// `or_insert` and not `insert`, because a repeated key keeps its **first** value; a `BTreeMap`
/// would otherwise keep the last, which is the opposite and is measured.
pub fn from_text_array(elements: &[Option<String>]) -> Result<Hstore> {
    if !elements.len().is_multiple_of(2) {
        return Err(SqlError::HstoreArrayOddLength);
    }
    let mut map = Hstore::new();
    for pair in elements.chunks(2) {
        let (key, value) = (&pair[0], &pair[1]);
        let Some(key) = key else {
            return Err(SqlError::HstoreNullKey);
        };
        map.entry(Key(key.clone())).or_insert_with(|| value.clone());
    }
    Ok(map)
}

/// The canonical text, which is what a client is sent and what the row holds.
#[must_use]
pub fn to_text(map: &Hstore) -> String {
    map.iter()
        .map(|(key, value)| {
            let value = match value {
                // **The bare word, unquoted**, which is what makes it a NULL rather than the
                // four-character string a quoted one would be.
                None => "NULL".to_owned(),
                Some(value) => quote(value),
            };
            format!("{}=>{value}", quote(&key.0))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `json` an hstore casts to: **every value a JSON string, and a NULL a JSON null.**
///
/// One function for both targets, because the two orders coincide: an hstore's canonical order is
/// length-then-bytes ([`Key`]) and that is `jsonb`'s object order as well, so the text this builds
/// is canonical for a `jsonb` and is the hstore's own order for a `json`. Measured on 19beta1 with
/// the extension created inside the transaction, 2026-09-10:
///
/// ```text
/// 'b=>2, a=>1, cc=>3'::hstore::json    {"a": "1", "b": "2", "cc": "3"}
/// 'a=>NULL, b=>1'::hstore::json        {"a": null, "b": "1"}
/// ''::hstore::json                     {}
/// '"a b"=>"x \"y\"", c=>"1.5"'::hstore::json   {"c": "1.5", "a b": "x \"y\""}
/// ```
///
/// **The numbers stay strings**: `hstore_to_json` does not guess a value's JSON type — that is
/// `hstore_to_json_loose`, a different function with a different name — so `2` comes out `"2"`.
/// The space after the colon is `json`'s own rendering and is what both servers print.
#[must_use]
pub fn to_json(map: &Hstore) -> String {
    let mut out = String::from("{");
    for (at, (key, value)) in map.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        super::json::write_string(&key.0, &mut out);
        out.push_str(": ");
        match value {
            None => out.push_str("null"),
            Some(value) => super::json::write_string(value, &mut out),
        }
    }
    out.push('}');
    out
}

/// One key or value, quoted the way `hstore_out` quotes it: always, with `\` before a `"` or a `\`.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Which side of the `=>` a token is on, which decides two things about how it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// A key: `=>` ends it, and a bare `NULL` is the string.
    Key,
    /// A value: it runs to the comma, so `'a=>b=>c'` is one pair whose value is `b=>c`, and a bare
    /// `NULL` is nothing.
    Value,
}

use Side::{Key as KeySide, Value as ValueSide};

/// One key or value: a quoted string, or a bare run.
///
/// **Where a bare run ends depends on the side**, and it is the difference between `'a=>b=>c'`
/// being one pair and being a syntax error: a key stops at `=>`, and a value runs to the comma.
/// `None` is the unquoted word `NULL`, **case-insensitively** — but only for a value.
fn token(text: &[char], at: &mut usize, side: Side) -> Result<Option<String>> {
    if *at < text.len() && text[*at] == '"' {
        *at += 1;
        let mut out = String::new();
        while *at < text.len() {
            match text[*at] {
                '\\' if *at + 1 < text.len() => {
                    out.push(text[*at + 1]);
                    *at += 2;
                }
                '"' => {
                    *at += 1;
                    return Ok(Some(out));
                }
                ch => {
                    out.push(ch);
                    *at += 1;
                }
            }
        }
        return Err(syntax("unexpected end of string"));
    }
    let start = *at;
    let mut out = String::new();
    while *at < text.len() {
        // `=>` ends a bare token and a comma separates pairs; everything else is content, which is
        // why `'a=>b=>c'` is one pair whose value is `b=>c` — the value's `=>` is past the only
        // one that matters.
        if text[*at] == ','
            || (side == KeySide && text[*at] == '=' && text.get(*at + 1) == Some(&'>'))
        {
            break;
        }
        out.push(text[*at]);
        *at += 1;
    }
    if *at == start {
        return Ok(None);
    }
    let trimmed = out.trim_end().to_owned();
    if side == ValueSide && trimmed.eq_ignore_ascii_case("null") {
        return Ok(None);
    }
    Ok(Some(trimmed))
}

fn skip_space(text: &[char], at: &mut usize) {
    while *at < text.len() && text[*at].is_whitespace() {
        *at += 1;
    }
}

fn eat(text: &[char], at: &mut usize, want: &str) -> bool {
    let wanted: Vec<char> = want.chars().collect();
    if text.len() < *at + wanted.len() || text[*at..*at + wanted.len()] != wanted[..] {
        return false;
    }
    *at += wanted.len();
    true
}

fn syntax(what: &str) -> SqlError {
    SqlError::HstoreSyntax(what.to_owned())
}

#[cfg(test)]
mod tests {
    //! Every case here is a line of `tests/corpus/pg19_hstore.txt`, kept beside the code because
    //! the corpus cannot run until the type is wired into a column and these can run now.

    use super::{from_text, to_text};

    fn round(text: &str) -> String {
        to_text(&from_text(text).unwrap())
    }

    /// The order is the key's **length** and then its bytes, which is the rule a byte-wise sort
    /// gets wrong on exactly one of these two lines.
    #[test]
    fn the_canonical_order_is_length_then_bytes() {
        assert_eq!(round("z=>1, aa=>2"), r#""z"=>"1", "aa"=>"2""#);
        assert_eq!(
            round("bb=>1, a=>2, ccc=>3, b=>4"),
            r#""a"=>"2", "b"=>"4", "bb"=>"1", "ccc"=>"3""#
        );
        assert_eq!(round("a=>b, 1=>2"), r#""1"=>"2", "a"=>"b""#);
    }

    /// A repeated key keeps the **first**, which is the opposite of what `||` does.
    #[test]
    fn a_repeated_key_keeps_the_first() {
        assert_eq!(round("a=>1, a=>2"), r#""a"=>"1""#);
    }

    /// An unquoted `NULL` is a real NULL whatever its case; a quoted one is the string.
    #[test]
    fn an_unquoted_null_is_a_null_and_a_quoted_one_is_not() {
        assert_eq!(
            round(r#"a=>null, b=>NuLl, c=>"NULL""#),
            r#""a"=>NULL, "b"=>NULL, "c"=>"NULL""#
        );
        // A key called NULL is quoted like any other, so the same four letters mean different
        // things on the two sides of `=>`.
        assert_eq!(round("NULL=>NULL"), r#""NULL"=>NULL"#);
        assert_eq!(
            round("NULL=>x, null=>y, Null=>z"),
            r#""NULL"=>"x", "Null"=>"z", "null"=>"y""#
        );
    }

    /// The quoting the suite actually pushes through — `test_quotes`, `test_backslash`,
    /// `test_comma`, `test_parse6/7`.
    #[test]
    fn the_suite_s_quoting_round_trips() {
        assert_eq!(
            round(r#""a"=>"b\"ar", "1\"foo"=>"2""#),
            r#""a"=>"b\"ar", "1\"foo"=>"2""#
        );
        assert_eq!(round(r#""a b"=>"b ar""#), r#""a b"=>"b ar""#);
        assert_eq!(round(r#""a\\b"=>"b\\ar""#), r#""a\\b"=>"b\\ar""#);
        assert_eq!(round(r#""a, b"=>"bar""#), r#""a, b"=>"bar""#);
        assert_eq!(round(r#""\"a"=>"q>w""#), r#""\"a"=>"q>w""#);
        assert_eq!(
            round(r#""c"=>"}", "\"a\""=>"b \"a b""#),
            r#""c"=>"}", "\"a\""=>"b \"a b""#
        );
    }

    /// `=>` inside a bare value needs no escape, and whitespace around either side is trimmed.
    #[test]
    fn a_bare_value_runs_to_the_comma() {
        assert_eq!(round("a=>b=>c"), r#""a"=>"b=>c""#);
        assert_eq!(round("  a  =>  b  ,  c=>d  "), r#""a"=>"b", "c"=>"d""#);
    }

    /// The empty hstore is not NULL and is not a syntax error.
    #[test]
    fn the_empty_hstore_is_empty_text() {
        assert_eq!(round(""), "");
        assert!(from_text("").unwrap().is_empty());
    }

    /// A literal with no `=>` is `42601`, which is a **syntax** error and not the `22P02` every
    /// other bad literal here gives.
    #[test]
    fn a_literal_with_no_arrow_is_a_syntax_error() {
        let error = from_text("a").unwrap_err();
        assert_eq!(error.sqlstate(), "42601");
        assert_eq!(
            error.to_string(),
            "syntax error in hstore: unexpected end of string"
        );
    }
}
