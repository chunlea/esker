//! **One `select_common_type`, for the three constructs that unify a list of types** — wire v3
//! family **F9**.
//!
//! A `UNION`, a `COALESCE` and a `CASE` all answer the same question, and this crate answered it
//! three and a half different ways: the set-operation path had PostgreSQL's two passes and the
//! right two sentences, `resolve_coalesce` folded through the *arithmetic* promotion table,
//! `resolve_case` folded through `unify` behind a **comparison** predicate, and `expr_type` had
//! two more folds of its own for the type a client is *told*.
//!
//! **Measured over every same-category pair of the wire v3 probe list's 100 spellings** — 2,778
//! ordered pairs, both shapes, 5,556 shape-rows
//! (`tests/captures/pg19_branch_common_type.txt`). Before: **5,198 disagreed with 19beta1**.
//! After: **100**, and every one of those is a missing `pg_cast` row or the two vectors' identity,
//! neither of which is this rule.
//!
//! The algorithm, which is `select_common_type` and its verify pass:
//!
//! 1. the candidate is the first type; a later type in a **different `typcategory`** is `42804`;
//!    otherwise the candidate is displaced only when it is not its category's preferred type, it
//!    casts **implicitly** to the other, and the other does not cast back;
//! 2. then every input must reach the candidate by an implicit cast, or it is **`42846`** — a
//!    different code and a different sentence from step 1.
//!
//! **An array's category is `A`, not its element's.** That is the half the old `unify` got wrong
//! by recursing into elements before the category test: `"char"[]` beside `bigint[]` is
//! `42846 could not convert` on a real server and was `42804` here, and `"char"[]` beside `text[]`
//! is answered `text[]` there and was refused here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS citext"])
}

/// **The pairs whose common type moved**, each with what this node answered before
/// (`results` in the capture) and what 19beta1 answers.
#[test]
fn the_candidate_is_chosen_the_way_postgresql_chooses_it() {
    let mut node = node();
    for (left, right, pg, was) in [
        // arrays: the category is `A` for both, so the element pair decides the *cast*, not the
        // category — this was `42804 types "char"[] and text[] cannot be matched`.
        (r#"'{"x"}'::"char"[]"#, "'{x}'::text[]", "text[]", "42804"),
        (
            "'{1}'::bigint[]",
            "'{1.5}'::double precision[]",
            "double precision[]",
            "42804",
        ),
        // the preferred type wins, and `text` is preferred where `citext` is not
        ("'x'::citext", "'x'::text", "text", "citext"),
        ("'x'::text", "'x'::citext", "text", "text"),
        // and where **both** directions are implicit the operand that came first keeps it, which
        // is what separates this pair from the one above: `citext -> text` is implicit and
        // `text -> citext` is only an assignment, so `text` displaces `citext` in both orders,
        // while `varchar -> text` and `text -> varchar` are both implicit and neither displaces.
        // This row first asserted `text` from memory and was wrong; the capture says
        // `character varying`.
        (
            "'x'::character varying",
            "'x'::text",
            "character varying",
            "character varying",
        ),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(COALESCE({left}, {right}))")),
            vec![vec![pg]],
            "COALESCE is {pg} on 19beta1; this node answered {was}"
        );
    }
}

/// **`42846`, not `42804`, for one category with no cast between them** — the second pass, which
/// only the set-operation path had.
///
/// The sentence names the branch that could not be converted and the type the construct settled
/// on, and the construct's own word: `COALESCE`, `CASE/WHEN`, `UNION`.
#[test]
fn one_category_and_no_cast_is_its_own_sentence() {
    let mut node = node();
    assert_eq!(
        node.answer(r#"SELECT COALESCE('{"x"}'::"char"[], '{1}'::bigint[])"#)
            .to_string(),
        "!42846 COALESCE could not convert type bigint[] to \"char\"[]"
    );
    assert_eq!(
        node.answer(r#"SELECT CASE WHEN true THEN '{"x"}'::"char"[] ELSE '{1}'::bigint[] END"#)
            .to_string(),
        "!42846 CASE/WHEN could not convert type \"char\"[] to bigint[]",
        "a CASE's list starts at its ELSE, so the pair is named the other way round"
    );
    assert_eq!(
        node.answer("SELECT '1'::money UNION SELECT '1'::numeric")
            .to_string(),
        "!42846 UNION could not convert type numeric to money",
        "the path this rule was written for, unchanged"
    );
}

/// **`42804` is still `42804` where the categories really do differ**, which is the lower bound the
/// second pass must not swallow.
#[test]
fn two_categories_are_still_the_other_sentence() {
    let mut node = node();
    for (sql, expected) in [
        (
            r#"SELECT COALESCE('x'::"char", 'x'::text)"#,
            "!42804 COALESCE types \"char\" and text cannot be matched",
        ),
        (
            "SELECT COALESCE(1, 'x'::text)",
            "!42804 COALESCE types integer and text cannot be matched",
        ),
        (
            "SELECT CASE WHEN true THEN 1 ELSE 'x'::text END",
            "!42804 CASE types text and integer cannot be matched",
        ),
    ] {
        assert_eq!(node.answer(sql).to_string(), expected, "{sql}");
    }
}

/// **The type a client is *told* is the type the plan settled on**, which is the half that had two
/// more folds of its own.
///
/// `pg_typeof(COALESCE("char"[], text[]))` read one of them and said `"char"[]` while the
/// resolution settled on `text[]`. A declared type that disagrees with the plan is the `->` fetch's
/// bug, and `ActiveRecord` decodes by it.
#[test]
fn the_declared_type_and_the_resolved_type_are_one() {
    let mut node = node();
    assert_eq!(
        node.rows(r#"SELECT pg_typeof(COALESCE('{"x"}'::"char"[], '{x}'::text[]))"#),
        vec![vec!["text[]"]]
    );
    // the same expression as a projection, where the declared type is what the wire carries
    assert_eq!(
        node.answer(r#"SELECT COALESCE('{"x"}'::"char"[], '{x}'::text[])"#)
            .to_string()
            .split('\t')
            .next()
            .unwrap_or(""),
        "text[]",
        "the RowDescription and pg_typeof are one answer"
    );
}

/// **A type still unifies with itself, and an all-`unknown` list is still `text`** — F4's unit and
/// `select_common_type`'s own fallback, neither of which this rule may take away.
#[test]
fn the_two_answers_that_need_no_candidate() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT pg_typeof(COALESCE('{\"a\":1}'::json, '{\"a\":1}'::json))"),
        vec![vec!["json"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(COALESCE(NULL, 'x'))"),
        vec![vec!["text"]]
    );
}
