//! `ARRAY[…]` over expressions, replayed against what PostgreSQL 19beta1 answered.
//!
//! A constructor over constants folds at lowering, where what was written settles the element
//! type. This is the other one — an element that is a column has no value until there is a row —
//! and it is the whole of `ActiveRecord`'s `can_perform_case_insensitive_comparison_for?`, which
//! joins on `ARRAY[casttarget]::oidvector = proargtypes`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_array_constructor_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_constructor.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **What an `unknown` element does to the constructor's type** — `select_common_type`'s rule,
/// measured one spelling at a time on 19beta1, 2026-09-10, inside `BEGIN … ROLLBACK`.
///
/// An unknown contributes **no** type and is read with whatever the typed elements settle; only an
/// array of nothing but unknowns is `text`. This node had the opposite rule in
/// `parse::lower::lower_array_constructor` — "a string makes the whole array `text`" — while
/// `exec::query::quantified_element_type` had already written the right one down for the `= ANY`
/// side. One fact, two readers, and the folded one was the one nobody had put to the oracle: it
/// only became visible when the constructor stopped folding into a value that `assign` could
/// re-read, and it surfaced as
/// `42804 column "terms" is of type interval[] but expression is of type text[]` on an `INSERT`
/// that had been green for months.
///
/// The last two rows are the half a type rule alone would miss: the unknown is **read** with the
/// settled type, so a value that will not read is the input function's own error and not a
/// mismatch.
#[test]
fn an_unknown_element_takes_the_type_the_typed_ones_settle() {
    let mut node = parity::Node::new(&[]);
    for (statement, expected) in [
        (
            "SELECT pg_typeof(ARRAY['1 month'::interval, '1 year', '1 hour'])",
            "interval[]",
        ),
        ("SELECT pg_typeof(ARRAY['a'::name, 'b'])", "name[]"),
        (
            "SELECT pg_typeof(ARRAY['2020-01-01'::date, '2020-01-02'])",
            "date[]",
        ),
        ("SELECT pg_typeof(ARRAY[1, '2'])", "integer[]"),
        // All-unknown, and a NULL is unknown too.
        ("SELECT pg_typeof(ARRAY['x', 'y'])", "text[]"),
        ("SELECT pg_typeof(ARRAY[NULL])", "text[]"),
        ("SELECT pg_typeof(ARRAY[NULL, 'x'])", "text[]"),
        ("SELECT pg_typeof(ARRAY[1, NULL])", "integer[]"),
        ("SELECT pg_typeof(ARRAY[1, 1.5])", "numeric[]"),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![expected.to_owned()]],
            "{statement}"
        );
    }
    // The element is read as the settled type, so `[2]` is the number 2 and not the text `2`.
    assert_eq!(node.rows("SELECT (ARRAY[1, '2'])[2]"), vec![vec!["2"]]);
    // And an unknown that will not read is the input function's error, exactly as on a real
    // server: `22P02 invalid input syntax for type boolean: "x"`.
    assert!(node.run("SELECT ARRAY[true, 'x']").is_err());
    assert!(node.run("SELECT ARRAY['1 month'::interval, 'x']").is_err());
    // An empty constructor still has no type to take: `42P18`.
    assert!(node.run("SELECT ARRAY[]").is_err());
}
