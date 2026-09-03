//! Contract C3 for `IN (list)`, and the three-valued rule that is the whole of it.
//!
//! Forty statements put to a real PostgreSQL 19beta1 in one session and replayed against one
//! node. The trap is at the top of the corpus: **a NULL in the list does not mean false**, and the
//! consequence is that `NOT IN` over a list containing one matches nothing at all. The corpus grew
//! a second block after the first draft passed, and it caught a real bug: a NULL item **does not
//! stop the scan**, so `1 IN (NULL, 1)` is true where this crate first answered NULL.
//!
//! Why this and not something else: the first Rails scoreboard
//! (`docs/bench/rails-scoreboard.md`) measured every one of `ActiveRecord`'s 36 boot statements
//! against a running node and found that the framework cannot open a connection without this —
//! `AbstractAdapter` builds its type map from `… WHERE t.typname IN ('int2', …)`, which is the
//! first statement it ever sends. Four of the 36 are behind it and so is rung 2 of the ladder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same table the corpus builds, for the assertions a corpus cannot carry.
const FIXTURE: &[&str] = &[
    "CREATE TABLE inl (id int8 PRIMARY KEY, n text, k int8)",
    "INSERT INTO inl VALUES (1, 'one', 7), (2, 'two', NULL), (3, NULL, 9)",
];

/// What this node answers differently, and why. **Not one of them is about `IN`** — the corpus
/// surfaced gaps that were already there, which is what a capture is for.
///
/// It is one shorter than the `IN` unit left it. `SELECT 1 IN ('1')` was the worst entry on the
/// list — a *wrong answer* rather than a refusal, inherited from `=` through the `reconcile` the
/// two share — and it is now fixed: `tests/unknown_literal.rs` and `tests/corpus/pg19_unknown.txt`.
/// The two that remain from that family are no longer about typing an untyped literal at all, and
/// each says below what it is instead.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT id FROM inl WHERE (id IN (1, 2)) IS TRUE ORDER BY id",
            "`IS TRUE` is `0A000` naming itself here and was before this unit. `IN` composes \
             with everything this node does have — `AND`, `NOT`, a target list — and \
             `tests/in_list.rs` pins those.",
        ),
        // **`SELECT 1 IN (1.0)` stood here and is deleted** (ADR 0031, rule 2). It said an
        // `integer` against a `numeric` was a promotion this node did not do, and pointed at
        // `tests/unknown_literal.rs` for the counterexample that made widening to `double` the
        // wrong fix. Comparing the two *exactly* rather than through `f64` answers this line and
        // that counterexample the same way a real server does — see `value::float::pg_cmp_int`.
        (
            "SELECT 'a' IN (1)",
            "the `int4` divergence, not the untyped-literal one: the rule is working and `'a'` \
             *is* being read as an integer because of the `1`, so both servers raise `22P02`. \
             A bare constant is `integer` on a real server and `int8` here, so the message names \
             `bigint`. `tests/unknown_literal.rs` has the eleven statements this costs.",
        ),
        (
            "SELECT id FROM inl WHERE k IN (id + 6) ORDER BY id",
            "the operator `+` is `0A000` naming itself and was before this unit. The list is a \
             list of *expressions* here, so this runs the moment arithmetic does.",
        ),
        (
            "SELECT id FROM inl WHERE id + 0 IN (1, 2) ORDER BY id",
            "the operator `+`, as above, on the left-hand side.",
        ),
    ],
};

#[test]
fn every_in_list_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_in.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 38,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The trap, on its own, in the shape that returns a wrong row rather than an error.
///
/// "Not equal to any of them" is the reading that looks right and is not: `1 NOT IN (2, NULL)` is
/// **NULL**, so the row is dropped. An implementation that treated a NULL item as "no match"
/// would return it, with nothing to say so.
#[test]
fn a_null_in_the_list_is_not_a_false() {
    let mut node = parity::Node::new(FIXTURE);

    // A match wins outright, whatever else is in the list.
    assert_eq!(node.rows("SELECT 1 IN (1, NULL)"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 NOT IN (1, NULL)"), vec![vec!["f"]]);

    // A NULL item does not stop the scan: the match is still found behind it. This is the one
    // the first draft of the evaluator got wrong, and it drops a row rather than raising.
    assert_eq!(node.rows("SELECT 1 IN (NULL, 1)"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 NOT IN (NULL, 1)"), vec![vec!["f"]]);
    assert_eq!(node.rows("SELECT 1 IN (2, NULL, 1)"), vec![vec!["t"]]);
    assert_eq!(
        node.rows("SELECT id FROM inl WHERE id IN (NULL, 2, 3) ORDER BY id"),
        vec![vec!["2"], vec!["3"]]
    );

    // With no match, a NULL anywhere makes it unknown — in the list, or on the left.
    assert_eq!(node.rows("SELECT 1 IN (2, NULL)"), vec![vec!["\\N"]]);
    assert_eq!(node.rows("SELECT 1 NOT IN (2, NULL)"), vec![vec!["\\N"]]);
    assert_eq!(node.rows("SELECT NULL IN (1)"), vec![vec!["\\N"]]);
    assert_eq!(node.rows("SELECT NULL NOT IN (1)"), vec![vec!["\\N"]]);

    // And what that means for a `WHERE`: unknown is not true, so the row goes. Row 2 has a NULL
    // `k` and row 3 has `k = 9`; only 3 survives `k NOT IN (7)`.
    assert_eq!(
        node.rows("SELECT id FROM inl WHERE k NOT IN (7) ORDER BY id"),
        vec![vec!["3"]]
    );
}

/// `IN` is an expression and not only a predicate, which is what lets it appear in a target list
/// and compose with everything else that takes a boolean.
#[test]
fn in_is_an_expression() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT id, id IN (1, 2) FROM inl ORDER BY id"),
        vec![vec!["1", "t"], vec!["2", "t"], vec!["3", "f"]]
    );
    assert_eq!(
        node.rows("SELECT id IN (1, 2) AND true FROM inl ORDER BY id"),
        vec![vec!["t"], vec!["t"], vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM inl WHERE NOT id IN (1, 2) ORDER BY id"),
        vec![vec!["3"]]
    );
}

/// The list is compared under the **ordinary comparison rules**, which is a decision not to have
/// a rule of its own: every item is typed against the operand by `reconcile`, the same function
/// `=` uses, so `id IN ('1')` matches and `n IN (1)` is the same `42883` a bare `=` gives.
///
/// Two literals with nothing to type them against are the one shape this does not fix, and it is
/// `=`'s bug rather than `IN`'s — see [`DIVERGENCES`].
#[test]
fn the_list_uses_the_ordinary_comparison_rules() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT id FROM inl WHERE id IN ('1') ORDER BY id"),
        vec![vec!["1"]]
    );

    let error = node.run("SELECT id FROM inl WHERE n IN (1)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(error.to_string(), "operator does not exist: text = integer");

    // One bad item in an otherwise good list still fails the statement, because the list is typed
    // before any row is read.
    let error = node
        .run("SELECT id FROM inl WHERE id IN (1, 'two')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
}

/// `EXPLAIN` prints it as written, so a reader can see which clause narrowed nothing. It is a
/// filter and not an access path: `IN` over a primary key could be a point read per item, and
/// that is a planner decision this unit deliberately does not make (`docs/plans/phase-9-rails.md`
/// §5 — no cost model).
#[test]
fn explain_prints_the_list() {
    let mut node = parity::Node::new(FIXTURE);
    let plan = node.rows("EXPLAIN SELECT id FROM inl WHERE id IN (1, 2)");
    let text = plan.concat().concat();
    assert!(text.contains("IN (1, 2)"), "{text}");
}
