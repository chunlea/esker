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
/// **`UNION` without `ALL` deduplicates**, and `NULL` counts as equal to `NULL` for it.
#[test]
fn union_without_all_deduplicates() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT 1 UNION SELECT 1"), vec![vec!["1"]]);
    // Three arms, and the `UNION` deduplicates everything before it rather than the arm beside
    // it: `a UNION ALL b UNION c` is `((a ∪all b) ∪ c)`.
    let rows = node.rows("SELECT 1 UNION ALL SELECT 1 UNION SELECT 2");
    assert_eq!(rows.len(), 2, "the dedup did not reach the first two arms");
    assert!(rows.contains(&vec!["1".to_owned()]));
    assert!(rows.contains(&vec!["2".to_owned()]));
    // **`NULL` is equal to `NULL` here**, which it is nowhere else: measured, two NULL arms
    // deduplicate to one row.
    assert_eq!(
        node.rows("SELECT NULL::int4 UNION SELECT NULL::int4").len(),
        1
    );
    // And over a table, where the duplicates are rows rather than literals.
    assert_eq!(
        node.rows("SELECT i FROM so UNION SELECT i FROM so").len(),
        2
    );
}

/// **`ORDER BY` after the last arm sorts the whole set**, and may name two things and no third.
#[test]
fn the_set_takes_an_order_by_a_limit_and_an_offset() {
    let mut node = parity::Node::new(FIXTURE);

    // The ordinal.
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT 2 ORDER BY 1 DESC"),
        vec![vec!["2"], vec!["1"]]
    );
    // The **output** name, which is the first arm's.
    assert_eq!(
        node.rows("SELECT i AS c FROM so UNION ALL SELECT 9 ORDER BY c DESC"),
        vec![vec!["9"], vec!["2"], vec!["1"]]
    );
    // `LIMIT` and `OFFSET` over the result, not over an arm.
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT 2 LIMIT 1 OFFSET 1"),
        vec![vec!["2"]]
    );
    // And the two together, which is the shape a paginated set has.
    assert_eq!(
        node.rows("SELECT i FROM so UNION ALL SELECT 9 ORDER BY 1 DESC LIMIT 2"),
        vec![vec!["9"], vec!["2"]]
    );
}

/// **The underlying column is not visible**: by the time the set exists it is called what the
/// first arm called it.
#[test]
fn an_order_by_cannot_name_the_column_the_first_arm_renamed() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node
        .run("SELECT i AS c FROM so UNION ALL SELECT 9 ORDER BY i")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
}

/// **A `UNION ALL` inside a `WITH`**, which is the statement `with_test.rb` sends.
#[test]
fn a_cte_body_may_be_a_set_operation() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("WITH t AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM t"),
        vec![vec!["1"], vec!["2"]]
    );
    // Three arms with a parenthesised one in the middle: `with_test.rb`'s own shape.
    assert_eq!(
        node.rows(
            "WITH t AS (SELECT i FROM so UNION ALL (SELECT i FROM so) UNION ALL SELECT 9) \
             SELECT * FROM t"
        )
        .len(),
        5
    );
}

/// **`RECURSIVE` is a keyword about the bodies, not about the list.**
///
/// A `WITH RECURSIVE` whose body does not name itself is an ordinary `WITH` on a real server and
/// answers — measured — so the keyword alone is not a refusal here either.
#[test]
fn with_recursive_over_a_body_that_is_not_recursive_answers() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("WITH RECURSIVE t AS (SELECT 1 AS n) SELECT n FROM t"),
        vec![vec!["1"]]
    );
    assert_eq!(
        node.rows("WITH RECURSIVE t AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM t"),
        vec![vec!["1"], vec!["2"]]
    );
}

/// A body that **does** name itself is refused by name, and the name is the CTE's.
///
/// The fixpoint is a second evaluation model, not a variation on this one: a CTE here is inlined
/// (`plan::cte`), and a body that names itself cannot be — substituting it would never terminate.
/// What it needs is a working table iterated to a fixed point with its own termination rule and
/// its own memory bound.
#[test]
fn a_body_that_names_itself_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node
        .run(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 5) \
              SELECT n FROM t",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(
        error.to_string().contains("whose body names itself"),
        "the refusal did not say why: {error}"
    );
}

#[test]
fn what_this_commit_does_not_do_is_named() {
    let mut node = parity::Node::new(FIXTURE);
    for statement in ["SELECT 1 INTERSECT SELECT 2", "SELECT 1 EXCEPT SELECT 2"] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement} was not refused by name"
        );
    }
}
