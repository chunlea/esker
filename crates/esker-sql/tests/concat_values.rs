//! **The six `||` pairs that need a value, not a type** — wire v3 family **F3b**, the second unit.
//!
//! The first unit (`94365983`) built the operator table in `exec::query` and closed 96 of the 105
//! measured shape-rows by deciding **which pairs have a `||` at all**. These six were left, pinned
//! at today's answer in `tests/concat_operator.rs`, because each needs the evaluator to build a
//! value it could not build: a declared type without a value is worse than a refusal — a client is
//! told `bytea` and handed an error.
//!
//! Measured on 19beta1, values and all
//! (`tests/captures/pg19_concat_values.txt`):
//!
//! ```text
//! '101'::bit(3) || '01'::bit(2)   bit varying  10101      -- and the width WIDENS
//! '\x0102'::bytea || '\x03'       bytea        \x010203
//! 'a'::tsquery || 'b'::tsquery    tsquery      'a' | 'b'  -- an OR, not a join
//! 'a & b'::tsquery || 'c'         tsquery      'a' & 'b' | 'c'
//! 'a=>1'::hstore || 'x'::text     text         "a"=>"1"x  -- the hstore's canonical rendering
//! 'a b'::tsvector || 'x'::text    text         'a' 'b'x
//! ```
//!
//! **Two different fixes under one symbol.** `bit`, `bytea` and `tsquery` are new arms in the
//! evaluator: three same-type operators PostgreSQL has and this node did not. The `hstore` and
//! `tsvector` rows are the opposite — the evaluator had an operator and used it where PostgreSQL
//! uses `anynonarray || text`, because **a datum cannot say `unknown`**: `'a=>1'::hstore || 'b=>2'`
//! really is hstore concatenation on a real server (the unquoted literal resolves to `hstore`) and
//! `'a=>1'::hstore || 'x'::text` really is string concatenation, and the two arrive at the
//! evaluator as the same pair of `Datum`s. The plan is the only place that knows, so `resolve`
//! coerces the operand it settled on `text` for — which is what PostgreSQL's parser does too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS hstore"])
}

/// **Three same-type operators this node did not have**, each with the value 19beta1 builds.
#[test]
fn bit_bytea_and_tsquery_concatenate() {
    let mut node = node();
    for (sql, ty, value) in [
        ("'101'::bit(3) || '01'::bit(2)", "bit varying", "10101"),
        ("'1'::bit || '0'::bit", "bit varying", "10"),
        ("'\\x0102'::bytea || '\\x03'::bytea", "bytea", "\\x010203"),
        ("''::bytea || '\\x03'::bytea", "bytea", "\\x03"),
        ("'a'::tsquery || 'b'::tsquery", "tsquery", "'a' | 'b'"),
        (
            "'a & b'::tsquery || 'c'::tsquery",
            "tsquery",
            "'a' & 'b' | 'c'",
        ),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({sql})")),
            vec![vec![ty]],
            "{sql} is {ty} on 19beta1"
        );
        assert_eq!(
            node.rows(&format!("SELECT ({sql})::text")),
            vec![vec![value]],
            "{sql} is {value} on 19beta1"
        );
    }
}

/// **A `bit(3)` and a `bit(2)` make five digits, and the type has stopped promising a width.**
///
/// The widening is the detail an arm written by hand gets wrong, so it is asserted from both
/// sides: the answer casts back into a `bit(5)` without losing a digit, and it does **not** fit a
/// `bit(3)` — a `bit(n)` cast truncates, so a result that had silently stayed three digits would
/// pass the first assertion and fail this one.
///
/// **`length()` is not used here on purpose.** `length('101'::bit(3) || '01'::bit(2))` is `5` on
/// 19beta1 and `42883 function length(bit varying) does not exist` on this node — a missing
/// function, not a missing operator, and a different unit
/// (`esker-coord/b4-wire-v3-families.md`, "found beside F3b").
#[test]
fn the_bit_result_is_varying_and_keeps_every_digit() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT ('101'::bit(3) || '01'::bit(2))::bit(5)::text"),
        vec![vec!["10101"]]
    );
    assert_eq!(
        node.rows("SELECT ('101'::bit(3) || '01'::bit(2))::bit(3)::text"),
        vec![vec!["101"]],
        "a bit(n) cast truncates, so this is what a three-digit answer would have looked like"
    );
}

/// **A typed string beside an hstore or a tsvector is string concatenation**, and the operand is
/// rendered the way a real server renders it — an hstore canonically quoted, a tsvector as its
/// sorted lexemes.
#[test]
fn a_typed_string_beside_an_hstore_or_tsvector_is_text() {
    let mut node = node();
    for (sql, value) in [
        ("'a=>1'::hstore || 'x'::text", "\"a\"=>\"1\"x"),
        ("'x'::text || 'a=>1'::hstore", "x\"a\"=>\"1\""),
        ("'a b'::tsvector || 'x'::text", "'a' 'b'x"),
        ("'x'::text || 'a b'::tsvector", "x'a' 'b'"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({sql})")),
            vec![vec!["text"]],
            "{sql} is text on 19beta1"
        );
        assert_eq!(
            node.rows(&format!("SELECT {sql}")),
            vec![vec![value]],
            "{sql}"
        );
    }
}

/// **The lower bound, and it is the whole reason the plan has to decide**: an *unquoted* literal
/// beside an hstore is still hstore concatenation, and beside a tsvector still tsvector
/// concatenation. The datums are identical to the ones above — a `Datum::Hstore` and a
/// `Datum::Text` — so an evaluator that told them apart by their values would have to be wrong
/// about one of the two.
#[test]
fn an_unknown_literal_beside_one_is_still_its_own_operator() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT pg_typeof('a=>1'::hstore || 'b=>2')"),
        vec![vec!["hstore"]]
    );
    assert_eq!(
        node.rows("SELECT ('a=>1'::hstore || 'b=>2')::text"),
        vec![vec!["\"a\"=>\"1\", \"b\"=>\"2\""]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof('a'::tsvector || 'b'::tsvector)"),
        vec![vec!["tsvector"]]
    );
}

/// **NULL still propagates**, which `concat()` does not — the difference the corpus keeps beside
/// this one.
#[test]
fn a_null_operand_is_still_null() {
    let mut node = node();
    for sql in [
        "NULL::bit || '1'::bit",
        "NULL::bytea || '\\x03'::bytea",
        "'a'::tsquery || NULL::tsquery",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT (({sql}) IS NULL)::text")),
            vec![vec!["true"]],
            "{sql}"
        );
    }
}
