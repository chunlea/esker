//! Writing through a view, which PostgreSQL calls **auto-updatable**.
//!
//! `view_test.rb`'s `UpdateableViewTest` writes through one four times and this node answered
//! `42P01 relation "printed_books" does not exist` — a *wrong answer*, not a missing feature: the
//! relation is right there, `SELECT` reads it, and a real server writes through it. Measured on
//! 19beta1 in one rolled-back session:
//!
//! ```text
//! CREATE VIEW p AS SELECT id, name, status, format FROM books WHERE format = 'paperback';
//! UPDATE p SET name = 'AWDwR' WHERE id = 1;       -> UPDATE 1
//! INSERT INTO p (name, format) VALUES ('c',…);    -> INSERT 0 1
//! UPDATE p SET format = 'hardback' WHERE id = 1;  -> UPDATE 1     and the row leaves the view
//! DELETE FROM p;                                  -> DELETE 1
//! ```
//!
//! The last two are what a guess gets wrong. Without `WITH CHECK OPTION` a row may be updated
//! **out** of the view rather than refused, and a `DELETE` with no `WHERE` deletes what the *view*
//! shows and not the table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE books (id bigserial primary key, name text, status int default 0, format text)",
        "INSERT INTO books (name, format) VALUES ('a','paperback')",
        "INSERT INTO books (name, format) VALUES ('b','hardback')",
        "CREATE VIEW printed AS SELECT id, name, status, format FROM books WHERE format = 'paperback'",
    ])
}

/// **The four writes the Rails test makes, and the two answers a guess gets wrong.**
#[test]
fn a_simple_view_is_written_through_to_the_table_underneath() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT name FROM printed"),
        vec![vec!["a".to_owned()]]
    );

    node.run("UPDATE printed SET name = 'AWDwR' WHERE id = 1")
        .expect("an update through the view");
    assert_eq!(
        node.rows("SELECT name FROM books WHERE id = 1"),
        vec![vec!["AWDwR".to_owned()]],
        "the write reached the table underneath"
    );

    node.run("INSERT INTO printed (name, format) VALUES ('c','paperback')")
        .expect("an insert through the view");
    assert_eq!(
        node.rows("SELECT count(*) FROM printed"),
        vec![vec!["2".to_owned()]]
    );

    // **The view's own `WHERE` bounds the write**: the hardback row is not the view's to touch.
    node.run("UPDATE printed SET name = 'nope'").unwrap();
    assert_eq!(
        node.rows("SELECT name FROM books WHERE format = 'hardback'"),
        vec![vec!["b".to_owned()]],
        "a row the view does not show must not be written through it"
    );

    // A row updated **out** of the view is allowed, not refused — there is no `WITH CHECK OPTION`.
    node.run("UPDATE printed SET format = 'hardback' WHERE id = 1")
        .expect("moving a row out of the view is allowed");
    assert_eq!(
        node.rows("SELECT count(*) FROM printed"),
        vec![vec!["1".to_owned()]]
    );

    // And an unqualified `DELETE` deletes what the view shows, not the table.
    node.run("DELETE FROM printed").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM printed"),
        vec![vec!["0".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM books"),
        vec![vec!["2".to_owned()]],
        "the rows the view never showed are still there"
    );
}

/// **A view that renames still writes through**, under the base table's column names.
#[test]
fn a_renaming_view_writes_through_under_the_base_names() {
    let mut node = node();
    node.run("CREATE VIEW titled AS SELECT id AS book_id, name AS title FROM books")
        .unwrap();
    node.run("UPDATE titled SET title = 'renamed' WHERE book_id = 1")
        .expect("an update through a renaming view");
    assert_eq!(
        node.rows("SELECT name FROM books WHERE id = 1"),
        vec![vec!["renamed".to_owned()]]
    );
}

/// **What is not auto-updatable is refused in PostgreSQL's own sentences.**
///
/// `55000`, not `0A000` — measured, and it is the surprising part: a refusal that reads like a
/// missing feature is spelled by a real server as an object that is not in the state the statement
/// needs.
#[test]
fn a_view_that_is_not_auto_updatable_is_refused_the_way_a_real_server_refuses_it() {
    let mut node = node();
    node.run("CREATE VIEW counted AS SELECT count(*) AS n FROM books")
        .unwrap();
    node.run("CREATE VIEW distinctly AS SELECT DISTINCT id, name FROM books")
        .unwrap();

    for (sql, message) in [
        ("UPDATE counted SET n = 1", "cannot update view \"counted\""),
        (
            "INSERT INTO distinctly (name) VALUES ('x')",
            "cannot insert into view \"distinctly\"",
        ),
        (
            "DELETE FROM distinctly",
            "cannot delete from view \"distinctly\"",
        ),
    ] {
        let refused = node.run(sql).expect_err(sql);
        assert_eq!(refused.sqlstate(), "55000", "{sql}: {refused}");
        assert_eq!(refused.to_string(), message, "{sql}");
    }
}
