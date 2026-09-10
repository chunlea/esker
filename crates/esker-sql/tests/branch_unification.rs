//! **A type unifies with itself** — wire v3 family **F4**, 22 rows.
//!
//! `COALESCE` and `CASE` settle their branches on one type, and both asked
//! `exec::query`'s `same_family` whether two branches *could* be one. That is the wrong question,
//! and its own doc comment says so:
//!
//! > **`json` compares with nothing, including another `json`.** … a family test says "the same
//! > type compares with itself" and here that is the case PostgreSQL refuses.
//!
//! It is a **comparison** predicate — deliberately false for `json` beside `json`, because
//! `'{}'::json = '{}'::json` really is `42883` — and unification is a different question.
//! `COALESCE(json, json)` needs no comparison at all, which is why 19beta1 answers it while
//! refusing `min(json)`.
//!
//! **Measured whole, both shapes, all 100 type spellings the wire v3 probe list carries**
//! (`tests/captures/pg19_branch_unification.txt`): on 19beta1 **every one** of the 100 answers its
//! own type for `COALESCE(x, x)` and for `CASE WHEN true THEN x ELSE x END` — a same-type
//! unification never refuses. This node refused exactly **three**: `json`, `json[]` and `xml`.
//! `xml[]` was already answered, because an array gets its own family number and the scalar's rule
//! never reached it — the same seam as F2, seen from the other side.
//!
//! `unify` already answers this correctly: its first line is `if left == right { return Ok(left) }`.
//! The family test in front of it was the whole defect.
//!
//! **What this is not.** Unifying two *different* types is a separate surface and this unit does
//! not touch it: the lower bounds below pin every refusal 19beta1 gives for a pair, and the 42
//! cross-type rows where this node and 19beta1 disagree are family **F9** in
//! `esker-coord/b4-wire-v3-families.md`, measured and not built.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS hstore"])
}

/// **The three this node refused, and the two shapes are one rule.**
///
/// `42804 COALESCE types json and json cannot be matched` — a sentence that names the same type
/// twice is the shape of the defect: nothing about `json` beside `json` is unmatched.
#[test]
fn a_type_unifies_with_itself() {
    let mut node = node();
    for (written, named) in [
        ("'{\"a\":1}'::json", "json"),
        ("ARRAY['{\"a\":1}'::json]", "json[]"),
        ("'<a/>'::xml", "xml"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(COALESCE({written}, {written}))")),
            vec![vec![named]],
            "COALESCE({named}, {named}) is {named} on 19beta1"
        );
        assert_eq!(
            node.rows(&format!(
                "SELECT pg_typeof(CASE WHEN true THEN {written} ELSE {written} END)"
            )),
            vec![vec![named]],
            "CASE over two {named} branches is {named} on 19beta1"
        );
    }
}

/// **The lower bound: two *different* types still refuse, in 19beta1's own sentence.**
///
/// A fix that unified everything would pass the test above and break these. The two sqlstates are
/// different questions — `42804` is "no common type in this category", `42846` is "there is one and
/// the value cannot be converted to it" — and both were measured, including which of the pair each
/// message names first (`COALESCE` walks left to right, a `CASE` starts at its `ELSE`).
#[test]
fn two_different_types_still_refuse() {
    let mut node = node();
    for (left, right, coalesce, case) in [
        (
            "'{\"a\":1}'::json",
            "'x'::text",
            "!42804 COALESCE types json and text cannot be matched",
            "!42804 CASE types text and json cannot be matched",
        ),
        (
            "'<a/>'::xml",
            "'x'::text",
            "!42804 COALESCE types xml and text cannot be matched",
            "!42804 CASE types text and xml cannot be matched",
        ),
        (
            "1",
            "'x'::text",
            "!42804 COALESCE types integer and text cannot be matched",
            "!42804 CASE types text and integer cannot be matched",
        ),
        (
            "ARRAY['{\"a\":1}'::json]",
            "'{\"a\":1}'::json",
            "!42804 COALESCE types json[] and json cannot be matched",
            "!42804 CASE types json and json[] cannot be matched",
        ),
    ] {
        assert_eq!(
            node.answer(&format!("SELECT COALESCE({left}, {right})"))
                .to_string(),
            coalesce
        );
        assert_eq!(
            node.answer(&format!(
                "SELECT CASE WHEN true THEN {left} ELSE {right} END"
            ))
            .to_string(),
            case
        );
    }
}

/// **And the type that made `same_family` say no is still uncomparable**, which is what the family
/// test was really for: this fix must reach unification and nothing else.
#[test]
fn json_still_has_no_equality() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT '{\"a\":1}'::json = '{\"a\":1}'::json")
            .to_string(),
        "!42883 operator does not exist: json = json \
         DETAIL: No operator of that name accepts the given argument types. \
         HINT: You might need to add explicit type casts."
    );
    assert_eq!(
        node.answer("SELECT '<a/>'::xml = '<a/>'::xml").to_string(),
        "!42883 operator does not exist: xml = xml \
         DETAIL: No operator of that name accepts the given argument types. \
         HINT: You might need to add explicit type casts."
    );
}
