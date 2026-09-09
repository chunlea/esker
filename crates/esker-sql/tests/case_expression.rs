//! `CASE WHEN … THEN … END`, against PostgreSQL 19beta1 — statement 198 of `schema.rb`, and the
//! one line every non-loading file of `activerecord/test` stopped on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** These two carried the arithmetic refusal `1/0` used to raise, then the width a
    // bare `1` used to have; the literal ladder's `int4` rung (ADR 0087) closed the second and the
    // rows had agreed since the first.
    types: &[],
    // Three, and all three are the same missing feature: this node has **no arithmetic
    // operators**, so `1/0` — the only expression PostgreSQL can be made to raise from inside an
    // unreached branch — is `0A000` naming `/` before the `CASE` is reached at all. The lines are
    // kept rather than dropped because they are the oracle's own proof that a `CASE`
    // short-circuits, and the property they prove is asserted against this node in
    // `only_the_chosen_branch_is_evaluated` with the per-row error it does have.
    answers: &[],
};

#[test]
fn every_case_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_case_expression.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The definition an index over a `CASE` prints, **over five lines**.
///
/// It cannot be a corpus row — a line there is tab-separated and holds no newline — so the four
/// answers are here, byte for byte as the capture recorded them with the newlines escaped
/// (`tests/corpus/pg19_case_expression.txt`, the `<NL>` block). Three facts live in this string
/// and nowhere else: the implicit `ELSE` is materialised **with its resolved type**, the condition
/// is parenthesised unless it is a bare boolean column, and the whole thing is indented rather
/// than printed on one line — which is why `ActiveRecord`'s schema dumper sees what it sees.
#[test]
fn a_case_index_prints_over_five_lines_with_its_else_filled_in() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE cs (id int8 PRIMARY KEY, rating int4, name text, flag boolean)",
        "CREATE INDEX cs_case ON cs ((CASE WHEN rating > 0 THEN lower(name) END) DESC)",
        "CREATE INDEX cs_case2 ON cs ((CASE WHEN rating > 0 THEN lower(name) ELSE upper(name) END))",
        "CREATE INDEX cs_mixed ON cs (rating, (CASE WHEN rating > 0 THEN lower(name) END))",
        "CREATE INDEX cs_bare ON cs ((CASE WHEN flag THEN name END))",
    ] {
        node.run(statement).unwrap();
    }
    for (name, printed) in [
        (
            "cs_case",
            "CREATE INDEX cs_case ON public.cs USING btree ((\nCASE\n    WHEN (rating > 0) THEN \
             lower(name)\n    ELSE NULL::text\nEND) DESC)",
        ),
        (
            "cs_case2",
            "CREATE INDEX cs_case2 ON public.cs USING btree ((\nCASE\n    WHEN (rating > 0) THEN \
             lower(name)\n    ELSE upper(name)\nEND))",
        ),
        (
            "cs_mixed",
            "CREATE INDEX cs_mixed ON public.cs USING btree (rating, (\nCASE\n    WHEN (rating > \
             0) THEN lower(name)\n    ELSE NULL::text\nEND))",
        ),
        (
            "cs_bare",
            "CREATE INDEX cs_bare ON public.cs USING btree ((\nCASE\n    WHEN flag THEN name\n    \
             ELSE NULL::text\nEND))",
        ),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_get_indexdef('{name}'::regclass)")),
            [[printed]],
            "for {name}"
        );
    }
    // `pg_get_expr` is the same text with **one pair of parentheses fewer** — the pair the key
    // list adds and this does not.
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = \
             'cs_case'::regclass"
        ),
        [["\nCASE\n    WHEN (rating > 0) THEN lower(name)\n    ELSE NULL::text\nEND"]]
    );
}

/// The branch a `CASE` chooses is the **only** one evaluated, which is observable and not an
/// optimisation.
///
/// PostgreSQL proves this with `1/0` — `CASE WHEN true THEN 1 ELSE 1/0 END` is `1` there and
/// `CASE WHEN false THEN 1 WHEN 1/0 = 0 THEN 2 ELSE 3 END` is `22012`, both captured — and this
/// node cannot write either, because it has no arithmetic operators: `/` is `0A000` naming itself,
/// which is why those three corpus lines are declared divergences rather than answers.
///
/// So the same property is put with the error this node *does* raise per row: `lower` of a
/// `bigint` is `42883`, from the evaluator rather than the planner. Unreached, it must not raise;
/// reached, it must. An implementation that evaluated every branch and then selected gets the
/// first wrong, and one that never evaluated a branch at all gets the second wrong.
#[test]
fn only_the_chosen_branch_is_evaluated() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE cs (id int8 PRIMARY KEY, name text)",
        "INSERT INTO cs VALUES (1, 'Alpha')",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT CASE WHEN true THEN name ELSE lower(id) END FROM cs"),
        [["Alpha"]],
        "the ELSE was not reached, so its 42883 was not raised"
    );
    let error = node
        .run("SELECT CASE WHEN false THEN name ELSE lower(id) END FROM cs")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    // The second `WHEN` is reached because the first did not match, and its condition raises.
    let error = node
        .run(
            "SELECT CASE WHEN false THEN name WHEN lower(id) = 'x' THEN name ELSE name END FROM cs",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    // And it is not reached when the first one matches.
    assert_eq!(
        node.rows(
            "SELECT CASE WHEN true THEN name WHEN lower(id) = 'x' THEN name ELSE name END FROM cs"
        ),
        [["Alpha"]]
    );
}

/// A row is written into an index over a `CASE`, found through it, and kept in step by an
/// `UPDATE` that moves it from one branch to the other.
///
/// The index is where a `CASE` stops being an expression and becomes stored bytes: the entry is
/// written from the value the expression had at insert and read from the value it has now, so a
/// `CASE` that evaluated differently in the two paths would leave an entry nothing finds.
#[test]
fn a_case_index_holds_the_branch_each_row_took() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE cs (id int8 PRIMARY KEY, rating int4, name text)",
        "CREATE UNIQUE INDEX cs_u ON cs ((CASE WHEN rating > 0 THEN lower(name) END))",
        "INSERT INTO cs VALUES (1, 5, 'Alpha')",
        // `rating = 0` takes the missing `ELSE`, so its key is NULL — and a NULL in a unique key
        // is not a duplicate of another NULL.
        "INSERT INTO cs VALUES (2, 0, 'Beta')",
        "INSERT INTO cs VALUES (3, 0, 'Gamma')",
    ] {
        node.run(statement).unwrap();
    }
    // The same lowercased name, in the other branch, is not a collision.
    node.run("INSERT INTO cs VALUES (4, 0, 'alpha')").unwrap();
    // In the same branch, it is.
    let error = node
        .run("INSERT INTO cs VALUES (5, 9, 'ALPHA')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    // Moving row 4 into the indexed branch collides with row 1, and moving row 1 out frees it.
    let error = node
        .run("UPDATE cs SET rating = 9 WHERE id = 4")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    node.run("UPDATE cs SET rating = 0 WHERE id = 1").unwrap();
    node.run("UPDATE cs SET rating = 9 WHERE id = 4").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM cs WHERE CASE WHEN rating > 0 THEN lower(name) END = 'alpha'"),
        [["4"]]
    );
}

/// The index-element grammar: a bare function call is an index column and every other expression
/// needs its own parentheses.
///
/// One refusal, `42601`, naming a **different token** each time — the token where PostgreSQL's
/// `ColId | func_expr_windowless | '(' a_expr ')'` stopped. The corpus has all six; this asserts
/// the pair that is the actual rule, because the accepted half is what makes the refusal a
/// grammar and not a blanket ban on expressions.
#[test]
fn an_index_expression_needs_its_own_parentheses_and_a_call_does_not() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE cs (id int8 PRIMARY KEY, rating int4, name text)")
        .unwrap();
    node.run("CREATE INDEX cs_call ON cs (lower(name))")
        .unwrap();
    for (written, token) in [
        ("(CASE WHEN rating > 0 THEN lower(name) END)", "CASE"),
        ("(name::text)", "::"),
        ("(rating + 1)", "+"),
        ("(name IS NULL)", "IS"),
        ("(NOT (name IS NULL))", "NOT"),
        ("(1)", "1"),
    ] {
        let error = node
            .run(&format!("CREATE INDEX cs_bad ON cs {written}"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), "42601", "for {written}");
        assert_eq!(
            error.to_string(),
            format!("syntax error at or near \"{token}\""),
            "for {written}"
        );
    }
}
