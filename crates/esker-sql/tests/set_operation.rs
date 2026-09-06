//! `UNION ALL` over N arms: the rows, the names, and the types.
//!
//! `with_test.rb` sends `WITH … AS (SELECT … UNION ALL (SELECT …) UNION ALL SELECT …) SELECT …`.
//! Nothing here was built — a set operation was `0A000` naming the operator — and this is the
//! first of four commits: `UNION ALL`, its cross-arm type unification, and its refusals.
//!
//! Everything asserted is measured in `tests/captures/pg19_set_operations.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE so (id int8 PRIMARY KEY, i int4, t text, n numeric)",
    "INSERT INTO so VALUES (1, 1, 'a', 1.5), (2, 2, 'b', 2.5)",
];

#[test]
fn the_arms_rows_come_back_in_order() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT 2"),
        vec![vec!["1"], vec!["2"]]
    );
    // Three arms, which is the shape `with_test.rb` sends and which the parser gives as a
    // left-leaning tree.
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    // And over a table, with a parenthesised arm in the middle.
    assert_eq!(
        node.rows("SELECT i FROM so UNION ALL (SELECT i FROM so) UNION ALL SELECT 9"),
        vec![vec!["1"], vec!["2"], vec!["1"], vec!["2"], vec!["9"]]
    );
}

/// **Duplicates are kept**, which is the whole of what `ALL` means.
#[test]
fn union_all_keeps_duplicates() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT 1"),
        vec![vec!["1"], vec!["1"]]
    );
}

/// The names come from the first arm and the types from every arm — two rules, not one.
#[test]
fn the_name_is_the_first_arms_and_the_type_is_every_arms() {
    let mut node = parity::Node::new(FIXTURE);

    let outcome = node
        .run("SELECT i FROM so UNION ALL SELECT n FROM so")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("a set operation answered no rows at all");
    };
    assert_eq!(fields.len(), 1);
    // The name is `i` — the first arm's — and the type is `numeric`, which only the second arm
    // has. 1700 is `numeric`.
    assert_eq!(fields[0].name, "i");
    assert_eq!(fields[0].type_oid, 1700);

    // And an alias on a later arm changes nothing.
    let outcome = node
        .run("SELECT 1 AS first_name UNION ALL SELECT 2 AS second_name")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].name, "first_name");
}

/// Integer widths and the numeric family, folded through the same promotion arithmetic uses.
#[test]
fn the_types_unify_the_way_a_promotion_does() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT pg_typeof(x) FROM (SELECT 1::int4 AS x UNION ALL SELECT 2::int8) q"),
        vec![vec!["bigint"], vec!["bigint"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(x) FROM (SELECT 1::int4 AS x UNION ALL SELECT 2.5::numeric) q"),
        vec![vec!["numeric"], vec!["numeric"]]
    );
}

/// **Three refusals, and they are three different codes.**
#[test]
fn the_three_refusals_are_postgresqls_own() {
    let mut node = parity::Node::new(FIXTURE);

    // A different number of columns is the grammar's `42601`, not a typing error.
    let error = node.run("SELECT 1, 2 UNION ALL SELECT 3").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert_eq!(
        error.to_string(),
        "each UNION query must have the same number of columns"
    );

    // Two typed columns with no common type name both, in the arms' order.
    let error = node
        .run("SELECT t FROM so UNION ALL SELECT i FROM so")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        error.to_string(),
        "UNION types text and integer cannot be matched"
    );
    let error = node
        .run("SELECT i FROM so UNION ALL SELECT t FROM so")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "UNION types integer and text cannot be matched"
    );
}

/// **What is not built yet is refused by name**, so the three commits after this one are visible.
#[test]
fn what_this_commit_does_not_do_is_named() {
    let mut node = parity::Node::new(FIXTURE);
    for statement in [
        "SELECT 1 UNION SELECT 2",
        "SELECT 1 INTERSECT SELECT 2",
        "SELECT 1 EXCEPT SELECT 2",
        "SELECT 1 UNION ALL SELECT 2 ORDER BY 1",
        "SELECT 1 UNION ALL SELECT 2 LIMIT 1",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement} was not refused by name"
        );
    }
}
