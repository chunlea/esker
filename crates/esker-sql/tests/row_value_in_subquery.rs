//! `(a, b) IN (SELECT x, y …)` — **a row on the left, compared column by column.**
//!
//! `delete_all_test.rb`'s composite-key delete sends
//! `DELETE FROM "cpk_orders" WHERE ("cpk_orders"."shop_id", "cpk_orders"."id") IN (SELECT …)` and
//! got `0A000 the expression (…) is not supported` (round 2, item 3).
//!
//! Measured on PostgreSQL 19, and the three-valued rule is per **row**, not per value:
//!
//! ```text
//! (1,2)    IN (SELECT 1,2)   -> t
//! (1,NULL) IN (SELECT 1,2)   -> NULL   one column matches, the other is unknown
//! (1,NULL) IN (SELECT 2,3)   -> f      a definite mismatch decides it, NULL or not
//! (1,2)    IN (SELECT 3,4)   -> f
//! (1,2) NOT IN (SELECT 1,2)  -> f   ·  (1,NULL) NOT IN (SELECT 2,3) -> t
//! (1,2)    IN (SELECT 1)      ERROR: subquery has too few columns
//! ```
//!
//! The third line is the one worth having a test for: a NULL in the left-hand row does **not** make
//! the answer unknown by itself. It makes the *column* unknown, and a definite mismatch anywhere
//! else in the row still settles it. A rule written per value — "a NULL operand is NULL" — gets
//! that one backwards.
//!
//! There is deliberately no general row-value expression here. The subquery expression carries a
//! **list** of operands, one being the ordinary case, and the comparison folds over it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE cpk (shop_id bigint, id bigint, n varchar)",
    "INSERT INTO cpk VALUES (1,1,'a'), (1,2,'b'), (2,1,'c')",
];

fn one(node: &mut parity::Node, sql: &str) -> String {
    node.rows(sql)
        .first()
        .and_then(|row| row.first())
        .cloned()
        .unwrap_or_else(|| "<no row>".to_owned())
}

#[test]
fn a_row_matches_when_every_column_does() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(one(&mut node, "SELECT (1, 2) IN (SELECT 1, 2)"), "t");
    assert_eq!(one(&mut node, "SELECT (1, 2) IN (SELECT 3, 4)"), "f");
    assert_eq!(one(&mut node, "SELECT (1, 2) NOT IN (SELECT 1, 2)"), "f");
}

/// **A NULL in the left-hand row is not an automatic NULL answer.** It makes one column unknown;
/// a definite mismatch in another column still decides the row.
#[test]
fn a_null_column_leaves_the_row_undecided_only_when_nothing_else_decides_it() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(one(&mut node, "SELECT (1, NULL) IN (SELECT 1, 2)"), "\\N");
    assert_eq!(one(&mut node, "SELECT (1, NULL) IN (SELECT 2, 3)"), "f");
    assert_eq!(one(&mut node, "SELECT (1, NULL) NOT IN (SELECT 2, 3)"), "t");
}

#[test]
fn the_composite_key_delete_the_suite_sends() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        one(
            &mut node,
            "SELECT count(*) FROM cpk WHERE (cpk.shop_id, cpk.id) \
             IN (SELECT shop_id, id FROM cpk WHERE n = 'a')"
        ),
        "1"
    );
    node.run(
        "DELETE FROM cpk WHERE (cpk.shop_id, cpk.id) IN (SELECT shop_id, id FROM cpk WHERE n = 'b')",
    )
    .unwrap();
    assert_eq!(one(&mut node, "SELECT count(*) FROM cpk"), "2");
}

/// The arity is checked, and PostgreSQL says which way it is short.
#[test]
fn a_subquery_with_the_wrong_number_of_columns_says_so() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.run("SELECT (1, 2) IN (SELECT 1)")
            .unwrap_err()
            .to_string(),
        "subquery has too few columns"
    );
    assert_eq!(
        node.run("SELECT 1 IN (SELECT 1, 2)")
            .unwrap_err()
            .to_string(),
        "subquery has too many columns"
    );
}
