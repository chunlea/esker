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

/// A body that **does** name itself is the fixpoint, and it answers.
///
/// This test asserted the refusal when this file was written: a CTE here is inlined
/// (`plan::cte`), and a body that names itself cannot be — substituting it never terminates. The
/// second evaluation model it needed is `exec::recursive`, and the statement below is the one the
/// old refusal quoted. Kept here rather than deleted because this file is where the reader arrives
/// asking what a set operation inside a `WITH` can be; `tests/recursive_cte.rs` is the whole rule.
#[test]
fn a_body_that_names_itself_is_the_fixpoint() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 5) \
              SELECT n FROM t"
        ),
        vec![vec!["1"], vec!["2"], vec!["3"], vec!["4"], vec!["5"]]
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

/// **A `WITH` written outside the set is every arm's**, not the first arm's alone.
///
/// The set's `WITH` is lowered onto the set's own `Select`, and CTE inlining walked `from`, the
/// joins and the subqueries — and not `set_arms`. So `w` was substituted in the first arm and
/// reached the catalog as a table name in the second: `relation "w" does not exist` for a name the
/// statement had just defined. Measured on PostgreSQL 19: `1`, `1`.
#[test]
fn a_cte_outside_the_set_is_visible_from_every_arm() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("WITH w AS (SELECT 1 AS n) SELECT n FROM w UNION ALL SELECT n FROM w"),
        vec![vec!["1"], vec!["1"]]
    );
    // The same set inside a derived table, which is the other planner.
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM (WITH w AS (SELECT 1 AS n) SELECT n FROM w UNION ALL \
             SELECT n FROM w) AS d"
        ),
        vec![vec!["2"]]
    );
    // And the third arm, which is where a walk that stopped at "the other arm" would have gone
    // wrong next.
    assert_eq!(
        node.rows(
            "WITH w AS (SELECT 1 AS n) SELECT n FROM w UNION ALL SELECT n FROM w UNION ALL \
             SELECT n + 1 FROM w ORDER BY 1"
        ),
        vec![vec!["1"], vec!["1"], vec!["2"]]
    );
}

/// **The common type is `select_common_type`'s, and it is not the arithmetic promotion.**
///
/// Measured on PostgreSQL 19 through `\gdesc` (`tests/captures/pg19_union_types.txt`) — through
/// `\gdesc` and not `pg_typeof`, because a `UNION ALL` in a `FROM` is flattened and `pg_typeof`
/// then reports each branch's own type. Two rows a reader guesses wrong: `int8` beside `real` is
/// `real` (not `double precision`, which the promotion table would say), and `int8` beside `oid`
/// is `oid`. The capture's string rows — `varchar` beside `text` stays `character varying`,
/// because the two cast each other implicitly and the first arm wins — are in the rule and not
/// in this test: a cast to `varchar`, `bpchar`, `json` or `jsonb` in a bare target list is typed
/// `text` by this node, so there is no arm of those types to unify here yet.
#[test]
fn the_common_type_is_postgresqls_own() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, oid) in [
        (
            "SELECT '2020-01-01'::date UNION ALL SELECT '2020-01-01 10:00'::timestamp",
            1114,
        ),
        (
            "SELECT '2020-01-01 10:00'::timestamp UNION ALL SELECT '2020-01-01 10:00+00'::timestamptz",
            1184,
        ),
        (
            "SELECT '2020-01-01 10:00+00'::timestamptz UNION ALL SELECT '2020-01-01 10:00'::timestamp",
            1184,
        ),
        ("SELECT 1::int2 UNION ALL SELECT 2::int8", 20),
        ("SELECT 1::int8 UNION ALL SELECT 2::real", 700),
        ("SELECT 1::numeric UNION ALL SELECT 2::real", 700),
        ("SELECT 1::int8 UNION ALL SELECT '2'::oid", 26),
        ("SELECT 'so'::regclass UNION ALL SELECT 2::int8", 2205),
        (
            "SELECT ARRAY[1]::int4[] UNION ALL SELECT ARRAY[2]::int8[]",
            1016,
        ),
    ] {
        match node.run(sql).unwrap() {
            esker_sql::pgwire::session::Outcome::Rows { fields, .. } => {
                assert_eq!(fields[0].type_oid, oid, "{sql}");
            }
            other @ esker_sql::pgwire::session::Outcome::Done { .. } => panic!("{sql}: {other:?}"),
        }
    }
}

/// Two refusals with two codes, both in PostgreSQL's words: categories that cannot be matched
/// (`42804`), and one category with no implicit cast between its two types (`42846`, which
/// names the arm it could not convert and the type the set settled on).
#[test]
fn the_two_refusals_are_told_apart() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, code, message) in [
        (
            "SELECT 1::int4 UNION ALL SELECT 'b'::text",
            "42804",
            "UNION types integer and text cannot be matched",
        ),
        (
            "SELECT 1::int4 UNION ALL SELECT true",
            "42804",
            "UNION types integer and boolean cannot be matched",
        ),
        (
            "SELECT '2020-01-01'::date UNION ALL SELECT 'b'::text",
            "42804",
            "UNION types date and text cannot be matched",
        ),
        (
            "SELECT '10:00'::time UNION ALL SELECT '1 hour'::interval",
            "42804",
            "UNION types time without time zone and interval cannot be matched",
        ),
        (
            "SELECT 1::money UNION ALL SELECT 2::numeric",
            "42846",
            "UNION could not convert type numeric to money",
        ),
        (
            "SELECT 1::int4 UNION ALL SELECT 2::money",
            "42846",
            "UNION could not convert type money to integer",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), code, "{sql}");
        assert_eq!(error.to_string(), message, "{sql}");
    }
}

/// **The common type is PostgreSQL's rule, and it is asymmetric.**
///
/// `select_common_type` keeps the running candidate unless it is *not* its category's preferred
/// type **and** it can be implicitly cast to the other while the other cannot be cast back. This
/// crate had the rule and asked `is_preferred` of the left alone, so a right-hand preferred type
/// never won; making it *symmetric* fixed fifteen shapes and broke two, which is how the shape
/// came out.
///
/// **The pair that shows the asymmetry is `name` and `text`**, which cast implicitly **both**
/// ways: neither displaces the other, so the arm that came **first** wins. A symmetric rule
/// answers `text` for both, because `text` is preferred and it lets the right side win a tie the
/// left had already taken. Measured on 19beta1, and the two orders are the whole test.
#[test]
fn the_arm_that_came_first_wins_a_tie_and_only_a_tie() {
    let mut node = parity::Node::new(&[]);
    // Both directions implicit: first wins, and it wins in *both* orders.
    assert_eq!(
        node.rows("SELECT pg_typeof(coalesce('a'::name, 'b'::text))"),
        vec![vec!["name"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(coalesce('a'::text, 'b'::name))"),
        vec![vec!["text"]]
    );
    // And a `UNION` over the same pair answers the same way, because it is the same function.
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(v) FROM (SELECT 'a'::name AS v UNION ALL SELECT 'b'::text) t LIMIT 1"
        ),
        vec![vec!["name"]]
    );
}

/// **Two rungs of the ladder are not what reasoning gives**, and both are measured.
#[test]
fn float4_sits_above_numeric_and_name_above_varchar() {
    let mut node = parity::Node::new(&[]);
    // `numeric -> float4` is implicit and `float4 -> numeric` is only an assignment, so the pair
    // is `real` — **in both orders**, which is what says it is a property of the pair and not of
    // which arm came first.
    for statement in [
        "SELECT pg_typeof(v) FROM (SELECT 1::numeric AS v UNION ALL SELECT 1::float4) t LIMIT 1",
        "SELECT pg_typeof(v) FROM (SELECT 1::float4 AS v UNION ALL SELECT 1::numeric) t LIMIT 1",
    ] {
        assert_eq!(node.rows(statement), vec![vec!["real"]], "{statement}");
    }
    // `name` beats the other string types and loses to `text`; `varchar -> name` is implicit
    // where `name -> varchar` is an assignment.
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(v) FROM (SELECT 'a'::varchar AS v UNION ALL SELECT 'b'::name) t \
             LIMIT 1"
        ),
        vec![vec!["name"]]
    );
}

/// **`GREATEST` and `LEAST` take the common type, not arithmetic's.**
///
/// They used `+`'s promotion, which is a right answer to a different question: `int2 + float4`
/// really is a `double precision`, because adding them needs the wider float. `GREATEST` picks one
/// of the values it was given, so it takes the type the pair resolves to.
#[test]
fn greatest_takes_the_common_type_and_not_the_wider_one() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(GREATEST(1::int2, 1::float4)), pg_typeof(LEAST(1::int2, 1::float4))"
        ),
        vec![vec!["real", "real"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(GREATEST(1::numeric, 1::float4))"),
        vec![vec!["real"]]
    );
    // The values are unchanged, which is what makes this a declared type and not an answer.
    assert_eq!(
        node.rows("SELECT GREATEST(1::int2, 2::float4), LEAST(1::int2, 2::float4)"),
        vec![vec!["2", "1"]]
    );
}
