//! **An array of an array** — the 196 rows r1's wire census v2 found under one mechanism.
//!
//! When the operand is already an array, this node degraded the answer to `text`/`text[]` where
//! PostgreSQL keeps the element type: 49 array types across `ARRAY[c]`, `(ARRAY[c])[1]`,
//! `unnest(ARRAY[c])` and `array_agg(c)`, the four `reg*` and the extension arrays included.
//!
//! **There is no array of an array, here or on a real server.** `ARRAY[c]` over an `int[]` is an
//! `int[]` with one more dimension; `pg_type._int4.typarray` is 0, so a `typarray` lookup answers
//! *nothing* and the constructor's rule is a different rule from `array_of`. The value side of
//! this crate already agreed — `ArrayValue` is flat elements plus `dims`, and its own module doc
//! says a two-dimensional array is not an array of arrays — so what was missing was the type rule
//! and the constructor's stacking, not a representation.
//!
//! Three things measured that reasoning gets wrong, each pinned below:
//!
//!   * `(ARRAY[c])[1]` on the two-dimensional result is **NULL of the element type**; it takes
//!     `[1][2]` to reach a value. A subscript list shorter than the dimensions under-specifies.
//!   * `ARRAY[NULL::int[]]` is **`{}`**, not `{NULL}` — a NULL array operand contributes no
//!     dimensions and no elements — while `ARRAY['{1,2}'::int[], NULL::int[]]` is a dimension
//!     *mismatch* rather than a NULL row.
//!   * The two mismatch refusals are **different messages from different functions**, both
//!     `2202E`: the constructor's from `ExecEvalArrayExpr`, `array_agg`'s from
//!     `accumArrayResultArr`.
//!
//! Measured in `tests/captures/pg19_array_of_array.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

// Nothing is declared: every row below agrees with PostgreSQL once the mechanism is fixed.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// **The whole measured family**, replayed against the corpus.
#[test]
fn every_array_of_array_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_of_array.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The declared type, read the way r1's wire probe reads it** — three element types across the
/// four shapes, which is the cross product the census found the defect in.
#[test]
fn the_element_type_survives_every_array_shape() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8)"]);
    // (statement, the oid PostgreSQL 19beta1 declares)
    for (statement, oid) in [
        // int[] — _int4 is 1007, int4 is 23
        (
            "SELECT ARRAY[c] AS v FROM (VALUES ('{1,2}'::int[])) s(c)",
            1007,
        ),
        (
            "SELECT (ARRAY[c])[1] AS v FROM (VALUES ('{1,2}'::int[])) s(c)",
            23,
        ),
        (
            "SELECT unnest(ARRAY[c]) AS v FROM (VALUES ('{1,2}'::int[])) s(c)",
            23,
        ),
        (
            "SELECT array_agg(c) AS v FROM (VALUES ('{1,2}'::int[])) s(c)",
            1007,
        ),
        // tsrange[] — _tsrange is 3909, tsrange is 3908
        (
            "SELECT ARRAY[c] AS v FROM (VALUES ('{\"[2020-01-01,2020-01-02)\"}'::tsrange[])) s(c)",
            3909,
        ),
        (
            "SELECT (ARRAY[c])[1] AS v FROM (VALUES ('{\"[2020-01-01,2020-01-02)\"}'::tsrange[])) s(c)",
            3908,
        ),
        (
            "SELECT unnest(ARRAY[c]) AS v FROM (VALUES ('{\"[2020-01-01,2020-01-02)\"}'::tsrange[])) s(c)",
            3908,
        ),
        (
            "SELECT array_agg(c) AS v FROM (VALUES ('{\"[2020-01-01,2020-01-02)\"}'::tsrange[])) s(c)",
            3909,
        ),
        // regclass[] — _regclass is 2210, regclass is 2205
        (
            "SELECT ARRAY[c] AS v FROM (VALUES ('{pg_class}'::regclass[])) s(c)",
            2210,
        ),
        (
            "SELECT (ARRAY[c])[1] AS v FROM (VALUES ('{pg_class}'::regclass[])) s(c)",
            2205,
        ),
        (
            "SELECT unnest(ARRAY[c]) AS v FROM (VALUES ('{pg_class}'::regclass[])) s(c)",
            2205,
        ),
        (
            "SELECT array_agg(c) AS v FROM (VALUES ('{pg_class}'::regclass[])) s(c)",
            2210,
        ),
    ] {
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        assert_eq!(
            fields[0].type_oid, oid,
            "{statement}\n  declared {} where PostgreSQL 19beta1 declares {oid}",
            fields[0].type_oid
        );
    }
}

/// **The literal half of the same twelve, and it is green.**
///
/// r1's v3 smoke found the node did not degrade uniformly by operand provenance, and it was right:
/// with the operand written as a literal, `ARRAY['{1,2}'::int[]]` answered a scalar `text` holding
/// `{"{1,2}"}` where the runtime form already answered `text[]` — **a different mechanism from the
/// one the test above fixed**. The type rule is `array_over` and it was in place; what defeated it
/// was earlier, `lower_array_constructor` folding a typed array literal to its text before any
/// constructor rule could see an array at all. `stack_folded_arrays` is what closed it: the fold
/// keeps the element type and builds the outer array from the inner one instead of reading the
/// inner one's printed form. For `regclass[]` the same fold was the `0A000 a relation name read as
/// a regclass without a catalog` this register met as #38, and it went with it.
///
/// **This doc carried "red, and handed over" and a "delete the `#[ignore]`" instruction for a day
/// after the `#[ignore]` was gone and all twelve were passing.** A stale sentence in a doc comment
/// is invisible to the gate — nothing compiles it, nothing runs it — and it is how a register
/// comes to claim more work than the tree has. The expectations are measured in
/// `tests/captures/pg19_array_of_array.txt`; the oids below are the assertion.
#[test]
fn the_element_type_survives_a_literal_operand_too() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8)"]);
    for (statement, oid) in [
        ("SELECT ARRAY['{1,2}'::int[]] AS v", 1007),
        ("SELECT (ARRAY['{1,2}'::int[]])[1] AS v", 23),
        ("SELECT unnest(ARRAY['{1,2}'::int[]]) AS v", 23),
        ("SELECT array_agg('{1,2}'::int[]) AS v", 1007),
        (
            "SELECT ARRAY['{\"[2020-01-01,2020-01-02)\"}'::tsrange[]] AS v",
            3909,
        ),
        (
            "SELECT (ARRAY['{\"[2020-01-01,2020-01-02)\"}'::tsrange[]])[1] AS v",
            3908,
        ),
        (
            "SELECT unnest(ARRAY['{\"[2020-01-01,2020-01-02)\"}'::tsrange[]]) AS v",
            3908,
        ),
        (
            "SELECT array_agg('{\"[2020-01-01,2020-01-02)\"}'::tsrange[]) AS v",
            3909,
        ),
        ("SELECT ARRAY['{pg_class}'::regclass[]] AS v", 2210),
        ("SELECT (ARRAY['{pg_class}'::regclass[]])[1] AS v", 2205),
        ("SELECT unnest(ARRAY['{pg_class}'::regclass[]]) AS v", 2205),
        ("SELECT array_agg('{pg_class}'::regclass[]) AS v", 2210),
    ] {
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        assert_eq!(
            fields[0].type_oid, oid,
            "{statement}\n  declared {} where PostgreSQL 19beta1 declares {oid}",
            fields[0].type_oid
        );
    }
}
