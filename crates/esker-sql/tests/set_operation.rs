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

/// **An arm with no type of its own takes the other arm's** — `debts-v1.1.md` **#75**.
///
/// PostgreSQL resolves a set operation's column from the arms that *have* a type and then reads
/// each unknown literal — a bare `NULL` or a quoted string with no cast — as that type. This node
/// has no `unknown`: such a literal is already a `text` by the time anything asks, so it was given
/// a vote in the unification and every one of these was a `42804`.
///
/// **Re-measured on 19beta1 2026-09-11** for this fix, in one `BEGIN … ROLLBACK` with a savepoint
/// per statement so no refusal swallows the rest. Every expectation below is that server's:
///
/// ```text
/// SELECT 'lit' UNION ALL SELECT tt FROM u     lit ; a        pg_typeof  text
/// SELECT NULL UNION ALL SELECT 1              NULL ; 1       pg_typeof  integer
/// SELECT NULL UNION ALL SELECT NULL           NULL ; NULL    pg_typeof  text
/// SELECT 1 UNION ALL SELECT NULL              1 ; NULL
/// SELECT 1 UNION ALL SELECT 'abc'             ERROR  invalid input syntax for type integer: "abc"
/// ```
///
/// The capture's header has listed these since 2026-09-05 and **nothing checked them against the
/// node** — the first of them passed by accident, because both its arms are `text` anyway. The
/// 2026-09-11 re-measurement is `tests/captures/pg19_set_operation_enums.txt`.
#[test]
fn an_unknown_arm_takes_the_other_arms_type() {
    let mut node = parity::Node::new(FIXTURE);

    // The one that already worked, and the reason it is not evidence on its own.
    assert_eq!(
        node.rows("SELECT 'lit' UNION ALL SELECT t FROM so"),
        vec![vec!["lit"], vec!["a"], vec!["b"]]
    );
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(x) FROM (SELECT 'lit' AS x UNION ALL SELECT t FROM so) q LIMIT 1"
        ),
        vec![vec!["text"]]
    );

    // A bare `NULL` beside an integer is an **integer**, not a refusal.
    assert_eq!(
        node.rows("SELECT NULL UNION ALL SELECT 1"),
        vec![vec!["\\N"], vec!["1"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(x) FROM (SELECT NULL AS x UNION ALL SELECT 1) q LIMIT 1"),
        vec![vec!["integer"]],
        "the typed arm decides, whichever side it is written on"
    );
    assert_eq!(
        node.rows("SELECT 1 UNION ALL SELECT NULL"),
        vec![vec!["1"], vec!["\\N"]]
    );

    // Every arm unknown leaves the column `text`, which is where this node already was.
    assert_eq!(
        node.rows("SELECT NULL UNION ALL SELECT NULL"),
        vec![vec!["\\N"], vec!["\\N"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(x) FROM (SELECT NULL AS x UNION ALL SELECT NULL) q LIMIT 1"),
        vec![vec!["text"]]
    );

    // And a literal that will not read as the settled type fails **as that type**: the value is
    // wrong, not the pair of types. It is `22P02`, where a mismatch would be `42804`.
    let error = node.run("SELECT 1 UNION ALL SELECT 'abc'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type integer: \"abc\""
    );
}

/// **What must not move**: the two refusals that are about *types* are still about types.
///
/// An unknown arm not voting must not turn a real mismatch into an answer — both arms below have
/// a type of their own, so nothing here is unknown and the sentences are the ones 19beta1 writes.
#[test]
fn a_real_mismatch_is_still_a_mismatch() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.run("SELECT t FROM so UNION ALL SELECT i FROM so")
            .unwrap_err()
            .to_string(),
        "UNION types text and integer cannot be matched"
    );
    assert_eq!(
        node.run("SELECT 1, 2 UNION ALL SELECT 3")
            .unwrap_err()
            .to_string(),
        "each UNION query must have the same number of columns"
    );
    // A cast is not unknown, whatever it casts: `'abc'::text` has a type and keeps its vote.
    assert_eq!(
        node.run("SELECT i FROM so UNION ALL SELECT 'abc'::text")
            .unwrap_err()
            .to_string(),
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

/// **The `Describe` of a set operation answers the first arm's type, not the set's.**
///
/// Found by r1's wire-types gate on run 127 (`esker-coord/r1-127-wire-caveat.txt` §4): seventeen
/// `UNION` pairs where the `RowDescription` this node sends is the **left** arm's oid and 19beta1
/// sends the promotion. Diagnosed here rather than assumed, by asking the two paths the same
/// seventeen questions:
///
/// ```text
///   pair                    simple  describe   pg_typeof   pg19
///   int2  UNION int4            23        21   integer       23
///   int8  UNION float4         700        20   real          700
///   numeric UNION float8       701      1700   double …      701
///   varchar UNION name          19      1043   name           19
/// ```
///
/// **The simple path is right in every one of the seventeen, and so is the value** —
/// `exec::query::common_of` is `select_common_type`, it *does* reach `UNION`, and it answers
/// exactly what 19beta1 answers. What the extended protocol sends is the head arm's, because
/// `Executor::described_in` calls `query::plan` directly where `plan_select` would have
/// dispatched on `set_arms` to `plan_set_operation` — the one place the unification lives. A set
/// operation **nested** anywhere is right, measured: inside a derived table and inside a `WITH`
/// body both answer 23, because those are planned by `subquery::plan_subqueries`, which calls
/// `append`. It is the top level and only the top level.
///
/// Two shapes beyond the gate's seventeen, because the gate records the type oid and nothing else:
///
/// * the **typmod** is the first arm's too — `varchar(3) UNION varchar(5)` describes as
///   `1043/7` where the simple path says `1043/-1`, which is what a real server says;
/// * `INTERSECT` and `EXCEPT` are `0A000` on the simple path and **answer a shape** here, so a
///   client that prepares one is told its columns and refused at `Execute` instead of at
///   `Describe`.
///
/// **Fixed** by dispatching on `set_arms` where `described_in` called `query::plan` — one call,
/// after the parameters are typed and the views expanded, which is the order that function
/// documents at length.
#[test]
fn a_prepared_set_operation_describes_the_common_type() {
    let mut node = parity::Node::new(FIXTURE);
    // Every pair r1's gate measured, with 19beta1's answer beside it.
    for (left, right, expected) in [
        ("int2", "int4", 23_u32),
        ("int2", "int8", 20),
        ("int2", "numeric", 1700),
        ("int2", "float4", 700),
        ("int2", "float8", 701),
        ("int4", "int8", 20),
        ("int4", "numeric", 1700),
        ("int4", "float4", 700),
        ("int4", "float8", 701),
        ("int8", "numeric", 1700),
        ("int8", "float4", 700),
        ("int8", "float8", 701),
        ("numeric", "float4", 700),
        ("numeric", "float8", 701),
        ("float4", "float8", 701),
        ("varchar", "name", 19),
        ("bpchar", "name", 19),
    ] {
        let sql = format!("SELECT '1'::{left} AS v UNION SELECT '1'::{right}");
        assert_eq!(described(&mut node, &sql).0, expected, "Describe of {sql}");
    }
}

/// **The typmod is the set's too, and the wire gate cannot see it** — it records the
/// `RowDescription`'s type oid and nothing else, so this shape had to be asked for rather than
/// found.
///
/// A `varchar(3)` beside a `varchar(5)` is a `varchar` with no length on a real server; the
/// `Describe` path was answering the head arm's `7` (3 + `VARHDRSZ`). Same dispatch, because
/// `query::append` is where a typmod survives only when both arms agree on it.
#[test]
fn a_prepared_set_operation_describes_the_common_typmod() {
    let mut node = parity::Node::new(FIXTURE);
    let widths = "SELECT '1'::varchar(3) AS v UNION SELECT '1'::varchar(5)";
    assert_eq!(
        described(&mut node, widths),
        (1043, -1),
        "a varchar(3) beside a varchar(5) is a varchar with no length"
    );
    // **Both arms agreeing keeps it**, which is what says the rule is agreement and not "drop it".
    assert_eq!(
        described(
            &mut node,
            "SELECT '1'::varchar(3) AS v UNION SELECT '2'::varchar(3)"
        ),
        (1043, 7)
    );
    // And the simple path, which was right all along — the two answers now match.
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = node.run(widths).unwrap() else {
        panic!("{widths} returned no rows")
    };
    assert_eq!((fields[0].type_oid, fields[0].type_modifier), (1043, -1));
}

/// **A set operation this node does not have is refused at `Describe`, not after it.**
///
/// `INTERSECT` and `EXCEPT` are `0A000` here (`set_arm_supported`), and the `Describe` path never
/// reached that check: it planned the head arm and answered a shape, so a client that prepares one
/// was told its columns and then refused at `Execute`. The protocol's own order is that a
/// statement which cannot run does not describe.
#[test]
fn a_prepared_intersect_is_refused_before_it_is_described() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "SELECT i FROM so INTERSECT SELECT i FROM so",
        "SELECT i FROM so EXCEPT SELECT i FROM so",
    ] {
        let described = node.describe(sql).expect_err("describe answered a shape");
        assert_eq!(
            described.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
        // The same refusal the simple path gives, which is the point: one answer per statement.
        let executed = node.run(sql).expect_err("execute answered rows");
        assert_eq!(described.to_string(), executed.to_string(), "{sql}");
    }
}

/// The `Describe`'s first field, as `(type_oid, type_modifier)`.
fn described(node: &mut parity::Node, sql: &str) -> (u32, i32) {
    let fields = node
        .describe(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .fields
        .unwrap_or_else(|| panic!("{sql} described no fields"));
    (fields[0].type_oid, fields[0].type_modifier)
}

/// **One statement, one resolution pass — and the arms were not in it.**
///
/// Everything `Executor::bound` does runs through `exec::bind`'s walkers, and none of them
/// descended into `select.set_arms`. So a `UNION`'s second arm got none of it, and each pass said
/// so from the row evaluator as an `XX000` — this crate reporting a state it does not handle, for
/// statements whose **first** arm answers the same expression perfectly. Measured before the fix,
/// five of them:
///
/// ```text
/// SELECT 'pg_class'::regclass UNION SELECT 'pg_type'::regclass
///     XX000 internal error: regclass() reached the row evaluator unresolved
/// SELECT current_schema() UNION SELECT current_schema()      XX000 a current_schema …
/// SELECT current_database() UNION SELECT current_database()  XX000 a current_schema …
/// SELECT m FROM t UNION SELECT 'sad'::mood FROM t            XX000 a cast to a user-defined type …
/// SELECT n FROM t UNION SELECT ('mood'::regtype)::int4       XX000 a regtype over a user-defined type …
/// ```
///
/// The third walker with this hole and the same fix: a `FROM` function's arguments were the second
/// (wire v3 family F10) and a `FROM`'s `VALUES` rows the first. `debts-v1.1.md` #57.
#[test]
fn a_set_arm_gets_the_same_resolution_the_first_one_does() {
    let mut node = parity::Node::new(FIXTURE);
    // Sorted here rather than by `ORDER BY 1`, which would sort by the **oid** a `regclass` is.
    let mut relations =
        node.rows("SELECT 'pg_class'::regclass AS r UNION SELECT 'pg_type'::regclass");
    relations.sort();
    assert_eq!(relations, vec![vec!["pg_class"], vec!["pg_type"]]);
    assert_eq!(
        node.rows("SELECT current_schema() AS s UNION SELECT current_schema()"),
        vec![vec!["public"]]
    );
    assert_eq!(
        node.rows("SELECT current_database() AS d UNION SELECT current_database()"),
        vec![vec!["esker"]]
    );
    // **Only in the second arm**, which is the shape that needs the *read-only* walker as well:
    // `resolve_current_database` and its siblings ask `bind::any` first and return early when the
    // answer is no, so a construct the question could not see was never resolved even once the
    // mutable walk could have reached it. Two walkers, written as a pair, exactly as
    // `walk_table_ref_mut`'s own comment says of the `FROM` shapes.
    assert_eq!(
        node.rows("SELECT 'esker' AS d UNION SELECT current_database()"),
        vec![vec!["esker"]]
    );
    assert_eq!(
        node.rows("SELECT 'public' AS s UNION SELECT current_schema()"),
        vec![vec!["public"]]
    );
    // A `regtype` over a **user** type, which is the same pass one function over.
    node.run("CREATE TYPE mood AS ENUM ('sad', 'ok')").unwrap();
    let oid = node.rows("SELECT ('mood'::regtype)::int4")[0][0].clone();
    assert_eq!(
        node.rows("SELECT 0 AS n UNION SELECT ('mood'::regtype)::int4 ORDER BY 1"),
        vec![vec!["0".to_owned()], vec![oid]]
    );
}

/// **A parameter in a non-first arm is typed by that arm's own columns**, which needs two more of
/// the same walkers: the one that collects the statement's relations and the one that types.
///
/// `SELECT c FROM b UNION SELECT c FROM b WHERE c = $1` is one statement with one `$n` sequence.
/// Before the walk reached the arms, `bind::infer` saw neither the predicate nor — for a relation
/// only an arm names — the table to type it against, and the parameter fell back to `text`. It
/// happens to be `text` here, so the shape that proves it is a column of another type.
#[test]
fn a_parameter_in_a_set_arm_is_typed_by_its_own_arm() {
    let mut node = parity::Node::new(FIXTURE);
    // `so.i` is an `int4`: a parameter compared against it is an `int4` (23) and not `text` (25).
    let described = node
        .describe("SELECT 0 AS i UNION SELECT i FROM so WHERE i = $1")
        .unwrap();
    assert_eq!(described.parameters, vec![23]);
    // And the same statement runs, with the parameter filled — a `$n` the substitution never
    // reached is an `XX000` from the row evaluator.
    assert_eq!(
        node.rows("SELECT 0 AS i UNION SELECT i FROM so WHERE i = 1 ORDER BY 1"),
        vec![vec!["0"], vec!["1"]]
    );
}
