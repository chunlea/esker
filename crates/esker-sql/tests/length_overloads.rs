//! **`length` is four names and four overload sets** — found beside wire v3 family F3b, and it is
//! not the missing function it was first written down as.
//!
//! Measured on 19beta1 (`tests/captures/pg19_length_overloads.txt`). `pg_proc` has eight rows for
//! the four names, and they do **not** agree with each other:
//!
//! ```text
//!                 length          char_length   octet_length   bit_length
//! text/character  chars           chars         bytes          bytes x 8
//! bit / varbit    **bits**        -             bytes          bits
//! bytea           bytes           -             bytes          bytes x 8
//! tsvector        **lexemes**     -             -              -
//! lseg / path     **float8**      -             -              -
//! ```
//!
//! This crate mapped `length`, `char_length` and `character_length` to **one** function and gave it
//! `text`'s answer for everything, so over the wire v3 probe list's 100 spellings **17 rows
//! diverged** — four where it refused what 19beta1 answers (`bit`, `bytea`, `lseg`, `path`) and
//! thirteen where it answered what 19beta1 refuses (the four ranges, the two vectors, `json`,
//! `jsonb`, `lquery`, `xml`, `void` and `int2vector`). The same shape `||` had: everything is text,
//! so everything answers.
//!
//! **`length(lseg)` is a `double precision`** — the geometric length, not a count — which is the
//! detail that says these are different functions wearing one name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS citext"])
}

/// **What `length` counts, per type**, with the value 19beta1 gives.
#[test]
fn length_counts_what_the_type_is_made_of() {
    let mut node = node();
    for (written, answer, ty) in [
        ("'abc'::text", "3", "integer"),
        ("'abc'::character(3)", "3", "integer"),
        ("'abc'::varchar", "3", "integer"),
        ("'abc'::citext", "3", "integer"),
        ("'abc'::name", "3", "integer"),
        ("'x'::\"char\"", "1", "integer"),
        // bits, not bytes and not characters
        ("'1'::bit", "1", "integer"),
        ("'101'::varbit", "3", "integer"),
        // bytes
        ("'\\x0102'::bytea", "2", "integer"),
        // lexemes
        ("'a b'::tsvector", "2", "integer"),
        // **the geometric length, and a `double precision`**
        ("'[(0,0),(3,4)]'::lseg", "5", "double precision"),
        ("'[(0,0),(3,4)]'::path", "5", "double precision"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT length({written})::text")),
            vec![vec![answer]],
            "length({written}) is {answer} on 19beta1"
        );
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(length({written}))")),
            vec![vec![ty]],
            "length({written}) is a {ty} on 19beta1"
        );
    }
}

/// **And what it refuses**, which is everything else — the half this crate answered.
#[test]
fn length_refuses_what_it_has_no_overload_for() {
    let mut node = node();
    for (written, named) in [
        ("'{\"a\":1}'::json", "json"),
        ("'{\"a\":1}'::jsonb", "jsonb"),
        ("'<a/>'::xml", "xml"),
        ("'[1,2]'::int4range", "int4range"),
        ("'[2020-01-01,2020-01-02]'::daterange", "daterange"),
        ("1::int8", "bigint"),
        ("'2020-01-01'::date", "date"),
        ("'(1,2)'::point", "point"),
    ] {
        assert_eq!(
            node.answer(&format!("SELECT length({written})"))
                .to_string(),
            format!(
                "!42883 function length({named}) does not exist \
                 DETAIL: No function of that name accepts the given argument types. \
                 HINT: You might need to add explicit type casts."
            ),
            "length({named})"
        );
    }
}

/// **`char_length` is a narrower list than `length`, and they were one function here.**
///
/// `char_length` and `character_length` have two overloads on 19beta1 — `text` and `character` —
/// so a `bit`, a `bytea`, a `tsvector`, an `lseg` and a `path` all refuse where `length` answers.
#[test]
fn char_length_is_not_length() {
    let mut node = node();
    for name in ["char_length", "character_length"] {
        assert_eq!(node.rows(&format!("SELECT {name}('abc')")), vec![vec!["3"]]);
        for (written, named) in [
            ("'1'::bit", "bit"),
            ("'\\x0102'::bytea", "bytea"),
            ("'a b'::tsvector", "tsvector"),
        ] {
            assert_eq!(
                node.answer(&format!("SELECT {name}({written})"))
                    .to_string(),
                format!(
                    "!42883 function {name}({named}) does not exist \
                     DETAIL: No function of that name accepts the given argument types. \
                     HINT: You might need to add explicit type casts."
                ),
                "{name}({named}) has no overload on 19beta1"
            );
        }
    }
}

/// **`octet_length` counts bytes and has its own three**: `text`, `character`, `bit`, `bytea` — and
/// not `tsvector`, `lseg` or `path`.
#[test]
fn octet_length_counts_bytes() {
    let mut node = node();
    for (written, answer) in [
        ("'abc'::text", "3"),
        ("'1'::bit", "1"),
        ("'\\x0102'::bytea", "2"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT octet_length({written})")),
            vec![vec![answer]],
            "octet_length({written})"
        );
    }
    assert_eq!(
        node.answer("SELECT octet_length('a b'::tsvector)")
            .to_string(),
        "!42883 function octet_length(tsvector) does not exist \
         DETAIL: No function of that name accepts the given argument types. \
         HINT: You might need to add explicit type casts."
    );
}

/// **`bit_length` is a fourth name with three overloads and this node has none of them** — pinned
/// at the *named* refusal it gives, which is the honest state and not a wrong answer.
///
/// 19beta1: `bit_length('abc')` is 24, `bit_length('1'::bit)` is 1, `bit_length('\x0102'::bytea)`
/// is 16. Its own small item.
#[test]
fn bit_length_is_still_a_named_refusal() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT bit_length('abc')").to_string(),
        "!0A000 the function bit_length is not supported"
    );
}
