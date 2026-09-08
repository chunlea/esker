//! **`ON CONFLICT (lower(external_id))` — the expression form, which names an expression index.**
//!
//! The third of `insert_all_test`'s conflict-target errors:
//!
//! ```text
//! test_insert_all_and_upsert_all_with_expression_index
//!   INSERT INTO "books" ("external_id",…) VALUES ('ABC', …) ON CONFLICT (lower(external_id))
//!     DO NOTHING RETURNING "id"
//!   PG::SyntaxError: Expected: ), found: ( at Line: 1, Column: 166
//! ```
//!
//! `sqlparser` 0.62.0 reads the target with `parse_parenthesized_column_list`, so it takes
//! `Ident`s and stops at the `(` after `lower`. Its `ConflictTarget::Columns(Vec<Ident>)` could not
//! hold the call either. So the entry is replaced, before the parse, by a **placeholder
//! identifier** and the expression travels beside it
//! (`parse::strip_on_conflict_target`) — the same one shim that takes the partial index's
//! predicate off, because both live inside the one clause and one reader of a grammar is the rule.
//!
//! **The placeholder is collision-proof by checking, not by construction.** A delimited identifier
//! may hold any character but `"`, so no name is impossible; the shim bumps its candidate until
//! the source does not contain it, and a test below gives a table a column named after the first
//! candidate to prove the bump happens.
//!
//! # Measured on 19beta1
//!
//! ```text
//! CREATE UNIQUE INDEX g1oe_expr ON g1oe_books ((lower(external_id)));
//!
//! ON CONFLICT (lower(external_id)) DO NOTHING     'ABC' then 'abc': the second is INSERT 0 0
//! ON CONFLICT (lower(external_id)) DO UPDATE      updates the row already there
//! ON CONFLICT (upper(external_id))                42P10 — a different expression
//! ON CONFLICT (external_id)                       42P10 — the bare column is not the key
//! ON CONFLICT DO NOTHING            (no target)   takes it: INSERT 0 0
//! pg_indexes.indexdef                             … USING btree (lower(external_id))
//! ```
//!
//! The last two are the ones a guess gets wrong in opposite directions: a **bare** target takes an
//! expression index, and a target naming the underlying *column* does not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1oe_books (id bigserial primary key, external_id text, title text)",
    "CREATE UNIQUE INDEX g1oe_expr ON g1oe_books ((lower(external_id)))",
];

/// The statement `insert_all` sends over an expression index.
#[test]
fn the_expression_form_parses_and_infers_that_index() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "INSERT INTO g1oe_books (external_id, title) VALUES ('ABC', 'first') \
             ON CONFLICT (lower(external_id)) DO NOTHING RETURNING id"
        ),
        [["1"]]
    );
    // **A different spelling of the same key.** `abc` is only a conflict because the index is on
    // `lower(external_id)`, which is the whole reason the target has to carry the expression.
    assert!(
        node.rows(
            "INSERT INTO g1oe_books (external_id, title) VALUES ('abc', 'second') \
             ON CONFLICT (lower(external_id)) DO NOTHING RETURNING id"
        )
        .is_empty()
    );
    assert_eq!(node.rows("SELECT count(*) FROM g1oe_books"), [["1"]]);
}

/// The `DO UPDATE` half, over the row the expression matched.
#[test]
fn the_expression_form_updates_the_row_already_there() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO g1oe_books (external_id, title) VALUES ('ABC', 'first')")
        .unwrap();
    assert_eq!(
        node.rows(
            "INSERT INTO g1oe_books (external_id, title) VALUES ('AbC', 'third') \
             ON CONFLICT (lower(external_id)) DO UPDATE SET title = excluded.title \
             RETURNING id, title"
        ),
        [["1".to_owned(), "third".to_owned()]]
    );
}

/// A different expression is `42P10`, not a silently ignored target.
#[test]
fn a_different_expression_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "INSERT INTO g1oe_books (external_id, title) VALUES ('ABC', 'x') \
             ON CONFLICT (upper(external_id)) DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}

/// **The underlying column is not the key.** `ON CONFLICT (external_id)` over an index on
/// `lower(external_id)` is `42P10` — measured, and the direction an implementation that unwrapped
/// the expression would get wrong.
#[test]
fn the_bare_column_does_not_reach_an_expression_index() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "INSERT INTO g1oe_books (external_id, title) VALUES ('ABC', 'x') \
             ON CONFLICT (external_id) DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}

/// **A bare `ON CONFLICT` does take it**, which is the other direction and equally measured.
#[test]
fn a_bare_on_conflict_takes_the_expression_index() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO g1oe_books (external_id, title) VALUES ('ABC', 'first')")
        .unwrap();
    node.run(
        "INSERT INTO g1oe_books (external_id, title) VALUES ('abc', 'second') \
         ON CONFLICT DO NOTHING",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1oe_books"), [["1"]]);
}

/// **The placeholder gets out of the way of a real column of that name.** The first candidate is
/// `esker_on_conflict_expr_0`; here a column is called exactly that, so the shim must pick
/// another — and the statement must still find the index and still write that column.
#[test]
fn the_placeholder_steps_around_a_column_that_already_has_its_name() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1oe_clash (id bigserial primary key, external_id text, \
         esker_on_conflict_expr_0 text)",
        "CREATE UNIQUE INDEX g1oe_clash_ix ON g1oe_clash ((lower(external_id)))",
    ]);
    node.run(
        "INSERT INTO g1oe_clash (external_id, esker_on_conflict_expr_0) VALUES ('ABC', 'kept')",
    )
    .unwrap();
    node.run(
        "INSERT INTO g1oe_clash (external_id, esker_on_conflict_expr_0) VALUES ('abc', 'ignored') \
         ON CONFLICT (lower(external_id)) DO NOTHING",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT esker_on_conflict_expr_0 FROM g1oe_clash"),
        [["kept"]]
    );
}

/// Both shims in one clause, which is the shape that proves they compose: an expression target
/// **and** a partial index's predicate.
#[test]
fn an_expression_target_and_a_predicate_together() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1oe_both (id bigserial primary key, external_id text, live boolean)",
        "CREATE UNIQUE INDEX g1oe_both_ix ON g1oe_both ((lower(external_id))) WHERE live",
    ]);
    node.run("INSERT INTO g1oe_both (external_id, live) VALUES ('ABC', true)")
        .unwrap();
    node.run(
        "INSERT INTO g1oe_both (external_id, live) VALUES ('abc', true) \
         ON CONFLICT (lower(external_id)) WHERE (live) DO NOTHING",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1oe_both"), [["1"]]);
    // Without the predicate it is the partial index's own rule again: not inferable.
    assert_eq!(
        node.answer(
            "INSERT INTO g1oe_both (external_id, live) VALUES ('abc', true) \
             ON CONFLICT (lower(external_id)) DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}
