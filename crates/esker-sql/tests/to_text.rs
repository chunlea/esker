//! `::text`, against PostgreSQL 19beta1's own answers.
//!
//! Three of `ActiveRecord`'s boot statements stop on `a cast to TEXT is not supported`. This is
//! the cast, evaluated **per row** — which is what a column needs and what folding a literal at
//! plan time could not give.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT f::text, length(f::text), f::text || '|' FROM tt WHERE id = 1",
        "The same two operators. The `f::text` in it is right — `tests/to_text.rs` asserts \
             the padding strip directly — and the line stays here so the operator unit inherits \
             the whole statement rather than half of it.",
        "UNMEASURED",
    )],
};

#[test]
fn every_to_text_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_to_text.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The two types whose cast is **not** their output function.
///
/// A boolean prints `t` and casts to `true`; a `character(n)` prints its padding and casts without
/// it. Every other type casts to exactly what it prints, which is what makes this a rule with two
/// exceptions rather than a table of fifteen.
#[test]
fn a_boolean_and_a_character_are_the_two_exceptions() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE t (id int8 PRIMARY KEY, flag bool, padded char(3), plain text)",
        "INSERT INTO t VALUES (1, true, 'x', 'x')",
        "INSERT INTO t VALUES (2, false, 'abc', 'abc')",
    ] {
        node.run(statement).unwrap();
    }

    // What each prints, which is the output function.
    assert_eq!(
        node.rows("SELECT flag, padded FROM t WHERE id = 1"),
        vec![vec!["t", "x  "]]
    );
    // What each casts to, which is not.
    assert_eq!(
        node.rows("SELECT flag::text, padded::text FROM t WHERE id = 1"),
        vec![vec!["true", "x"]]
    );
    assert_eq!(
        node.rows("SELECT flag::text, padded::text FROM t WHERE id = 2"),
        vec![vec!["false", "abc"]]
    );
}

/// A cast of NULL is NULL: a cast changes a value's type and never invents one.
#[test]
fn a_cast_of_null_is_null() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE n (id int8 PRIMARY KEY, a int8, b char(3), c bool)",
        "INSERT INTO n VALUES (1, NULL, NULL, NULL)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT a::text, b::text, c::text FROM n"),
        vec![vec!["\\N", "\\N", "\\N"]]
    );
}

/// Every other type casts to what it prints, including both JSON types.
#[test]
fn every_other_type_casts_to_what_it_prints() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE e (id int8 PRIMARY KEY, a int8, b int4, c int2, d varchar(5), \
         f timestamp(3), g float8, h real, i bytea, j json, k jsonb)",
        "INSERT INTO e VALUES (1, 42, 7, 3, 'ab', '2020-01-01 12:00:00.123456', 1.5, 1.5, \
         '\\x0102', '{\"b\":1,\"a\":2}', '{\"b\":1,\"a\":2}')",
    ] {
        node.run(statement).unwrap();
    }
    let printed = node.rows("SELECT a, b, c, d, f, g, h, i, j, k FROM e");
    let cast = node.rows(
        "SELECT a::text, b::text, c::text, d::text, f::text, g::text, h::text, i::text, \
         j::text, k::text FROM e",
    );
    assert_eq!(printed, cast, "the cast should be the output function here");
}
