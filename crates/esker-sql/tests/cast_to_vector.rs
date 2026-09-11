//! **A string casts to `oidvector` however it arrives** — wire v3 family **F7**, the `pg_cast` half.
//!
//! Measured over all 100 distinct type spellings of the wire v3 probe list
//! (`tests/captures/pg19_cast_at_use.txt`): `'<value>'::text::<T>` is accepted by 19beta1 for
//! **every one of the 100** — an explicit cast out of a string type is always allowed, through the
//! target's input function, whether or not `pg_cast` has a row for the pair. This node refused
//! exactly two:
//!
//! ```text
//! '1 2'::text::oidvector    42846 cannot cast type text to oidvector     PG: oidvector
//! '{…}'::text::regclass[]   0A000 a relation name read as a regclass …   PG: regclass[]
//! ```
//!
//! **The first is this unit.** `CatalogFunc::OidVector`'s evaluator took a `Datum::Array` and
//! nothing else, so a `Datum::Text` fell into the arm that raises "this array has an element with
//! no oid to render" — a sentence about arrays, for a value that is not one. The lowering already
//! knew better and says so in its own comment — *"A string is read as a vector, an array is built
//! into one"* — but it only reads a string when the operand is a **bare quoted literal**;
//! `::text::oidvector`, a column, and a parameter all arrive as something else.
//!
//! **`int2vector` was already right**, and that asymmetry is the whole tell: it reaches
//! `lower_type` and the ordinary literal path because `sqlparser` has a `DataType` for it, while
//! `oidvector` is a `CastTarget` and goes down a path written for `ARRAY[…]::oidvector` alone.
//!
//! **The second row was not this unit and is now closed too**, in
//! `tests/regclass_array_from_text.rs`: `text -> regclass[]` has no `pg_cast` row, so it is the
//! target's **input function** — `array_in` with `regclassin` per element — and the per-element
//! name resolution is the executor's rule carried into the row evaluator. So the sweep's two
//! refusals are both gone, and `'<value>'::text::<T>` is accepted for all 100 spellings here as
//! it is on 19beta1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Every way a string can reach the cast**, because the one that worked was the one shape the
/// lowering special-cases.
#[test]
fn a_string_becomes_an_oidvector_from_any_operand() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE v (t text)",
        "INSERT INTO v VALUES ('25 1043')",
    ]);
    for sql in [
        "SELECT pg_typeof('25 1043'::text::oidvector)",
        "SELECT pg_typeof(t::oidvector) FROM v",
        "SELECT pg_typeof(('25' || ' 1043')::oidvector)",
    ] {
        assert_eq!(node.rows(sql), vec![vec!["oidvector"]], "{sql}");
    }
    // And the value is the vector's own rendering, not the string it came from being handed back:
    // `oidvector` prints space separated, which is what `pg_proc.proargtypes` holds.
    assert_eq!(
        node.rows("SELECT t::oidvector::text FROM v"),
        vec![vec!["25 1043"]]
    );
    assert_eq!(
        node.rows("SELECT ('25 1043'::text::oidvector = ARRAY[25, 1043]::oidvector)::text"),
        vec![vec!["true"]],
        "the two ways of writing it are the same value"
    );
}

/// **The twin that was already right**, kept beside it so the pair cannot drift apart again.
#[test]
fn int2vector_takes_a_string_the_same_way() {
    let mut node = parity::Node::new(&["CREATE TABLE w (t text)", "INSERT INTO w VALUES ('1 2')"]);
    for sql in [
        "SELECT pg_typeof('1 2'::text::int2vector)",
        "SELECT pg_typeof(t::int2vector) FROM w",
    ] {
        assert_eq!(node.rows(sql), vec![vec!["int2vector"]], "{sql}");
    }
}

/// **The lower bound: an array still builds one, and an array of the wrong thing still refuses.**
///
/// `ARRAY[…]::oidvector` is what `ActiveRecord`'s case-insensitivity probe compares against
/// `pg_proc.proargtypes`, and it must keep working; an element with no oid to render is still
/// `42846`, which is the sentence a real server gives the same cast.
#[test]
fn an_array_still_builds_one_and_a_wrong_element_still_refuses() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT ARRAY[25, 1043]::oidvector::text"),
        vec![vec!["25 1043"]]
    );
    assert_eq!(
        node.rows("SELECT ARRAY['text'::regtype]::oidvector::text"),
        vec![vec!["25"]]
    );
    assert_eq!(
        node.answer("SELECT ARRAY['2020-01-01'::date]::oidvector")
            .to_string(),
        "!42846 cannot cast type date to oidvector"
    );
}

/// **And the text that is not a vector is `22P02`**, the input function's own refusal — not the
/// cast's. Measured on 19beta1: `'x'::oidvector` is
/// `22P02 invalid input syntax for type oid: "x"`, which `tests/captures/pg19_pg_cast.txt` already
/// carries for the literal spelling; this asserts the cast path reaches the same reader.
#[test]
fn a_string_that_is_not_a_vector_is_the_input_functions_refusal() {
    let mut node = parity::Node::new(&["CREATE TABLE u (t text)", "INSERT INTO u VALUES ('x')"]);
    assert_eq!(
        node.answer("SELECT 'x'::text::oidvector").to_string(),
        "!22P02 invalid input syntax for type oid: \"x\""
    );
    assert_eq!(
        node.answer("SELECT t::oidvector FROM u").to_string(),
        "!22P02 invalid input syntax for type oid: \"x\""
    );
}
