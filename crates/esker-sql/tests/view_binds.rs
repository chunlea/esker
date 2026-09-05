//! Writing through a view over the **extended** protocol — `$n`, which is what `ActiveRecord`
//! sends with `prepared_statements: true`.
//!
//! Its own file rather than more of `updatable_view.rs`: that one drives the parity harness and
//! this one the bind harness, and a test binary that loads both loads `provenance` twice
//! (`clippy::duplicate_mod`). The split is along the protocol, which is also the honest line —
//! every defect here was invisible to the simple-protocol tests next door.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// The fixture the Rails test builds, over the extended protocol.
fn bound_node() -> bind::Node {
    let mut node = bind::Node::new();
    for setup in [
        "CREATE TABLE books (id bigserial primary key, name text, status int default 0, format text)",
        "INSERT INTO books (name, format) VALUES ('a','paperback')",
        "INSERT INTO books (name, format) VALUES ('b','hardback')",
        "CREATE VIEW printed AS SELECT id, name, status, format FROM books WHERE format = 'paperback'",
    ] {
        node.bound(setup, &[]).unwrap();
    }
    node
}

/// **A `$n` compared with a view's column is typed by that column**, exactly as it is for a table.
///
/// The wider half of what `view_test.rb` found, and it is not about writing: `tables_for` resolved
/// a statement's relations with `View::table`, which does not answer for a view, so `bind::infer`
/// was handed an empty list and *every* parameter in a statement naming a view kept the `text`
/// fallback. With `prepared_statements: true` — `ActiveRecord`'s default — that is every
/// parameterised query against a view, not only the four writes that noticed it.
#[test]
fn a_parameter_is_typed_through_a_view() {
    let mut node = bound_node();
    assert_eq!(
        node.answer(
            "SELECT name FROM printed WHERE id = $1",
            &[Some(b"1".to_vec())]
        )
        .to_string(),
        node.answer(
            "SELECT name FROM books WHERE id = $1",
            &[Some(b"1".to_vec())]
        )
        .to_string(),
        "a parameter against the view must be typed the way the same parameter against the table \
         underneath is; `bigint = text` is the fallback being applied where a column type was \
         available all along"
    );
}

/// The `INSERT` `view_test.rb`'s `test_insert_record` sends, captured from PostgreSQL 19's log:
/// `INSERT INTO "printed_books" ("name","status","format") VALUES ($1,$2,$3) RETURNING "id"`,
/// with `$2 = '0'` into an `integer` column. The same statement against the table underneath
/// already worked, which is what says the defect is the view and not the parameter.
#[test]
fn a_bound_insert_through_a_view_takes_the_base_column_s_type() {
    let mut node = bound_node();
    let values = [
        Some(b"Rails in Action".to_vec()),
        Some(b"0".to_vec()),
        Some(b"paperback".to_vec()),
    ];
    let through = node.answer(
        "INSERT INTO printed (name, status, format) VALUES ($1, $2, $3) RETURNING id",
        &values,
    );
    assert!(
        !through.to_string().starts_with('!'),
        "refused: {through}\nthe same INSERT into `books` is accepted, and `'0'` into an integer \
         column is what ActiveRecord sends"
    );
}

/// The same, bound — which is how it arrives, and it must not depend on the protocol.
#[test]
fn a_view_qualified_column_is_rewritten_when_it_is_bound_too() {
    let mut node = bound_node();
    let answer = node.answer(
        "UPDATE printed SET name = $1 WHERE printed.id = $2",
        &[Some(b"AWDwR".to_vec()), Some(b"1".to_vec())],
    );
    assert!(!answer.to_string().starts_with('!'), "refused: {answer}");
}
