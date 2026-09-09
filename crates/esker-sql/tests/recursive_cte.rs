//! `WITH RECURSIVE` whose body names itself, against PostgreSQL 19beta1.
//!
//! `with_test.rb#test_with_recursive` is the statement:
//!
//! ```sql
//! WITH RECURSIVE top_companies_and_children AS (
//!   SELECT companies.* FROM companies WHERE companies.firm_id IS NULL
//!   UNION ALL
//!   SELECT companies.* FROM companies JOIN top_companies_and_children
//!                                       ON companies.firm_id = top_companies_and_children.id
//! ) SELECT companies.id FROM top_companies_and_children AS companies ORDER BY companies.id
//! ```
//!
//! A CTE in this node is **inlined** — `plan::cte` substitutes the body for the name — and a body
//! that names itself cannot be, because substituting it never terminates. So this is the one CTE
//! shape that needs a second evaluation model: a working table, iterated to a fixed point.
//!
//! Three rules here are not what reasoning gives, and each is a test below. The **seed's** type is
//! the answer's type, where a plain `UNION` would promote both arms. `UNION` deduplicates against
//! everything already produced rather than against the last iteration, which is what makes it a
//! termination rule. And PostgreSQL **streams** the working table, so a `LIMIT` stops an infinite
//! recursion there and cannot here — the one declared divergence in this file.
//!
//! Everything asserted is measured in `tests/captures/pg19_recursive_cte.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The suite's `companies`, under a name this database does not already have.
const CORPUS_FIXTURE: &[&str] = &[
    "CREATE TABLE rec_co (id int8 PRIMARY KEY, firm_id int8, name varchar(255))",
    "INSERT INTO rec_co VALUES (1,NULL,'top a'),(2,NULL,'top b'),(3,1,'child of 1'),\
     (4,1,'child of 1 too'),(5,3,'grandchild'),(6,NULL,'top c'),(7,6,'child of 6')",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One fact, eight times, and it is not about recursion**: an unadorned integer literal
    // is an `int8` in this crate and an `integer` to PostgreSQL's resolver, so a column
    // seeded by `SELECT 1` is `bigint` here. The rows are identical; `sum` over it is
    // `numeric` for the same reason, being the sum of a `bigint`.
    types: &[
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 5) SELECT i FROM n",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 5) SELECT sum(i), count(*) FROM n",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION SELECT 1 FROM n) SELECT i FROM n",
        "WITH RECURSIVE t AS (SELECT 1 AS a, 'x'::text AS b UNION ALL SELECT a+1, b FROM t WHERE a < 2) SELECT * FROM t",
        "WITH RECURSIVE t(p, q) AS (SELECT 1, 'x'::text UNION ALL SELECT p+1, q FROM t WHERE p < 2) SELECT * FROM t",
        "WITH RECURSIVE plain AS (SELECT 1 AS n), rec(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM rec WHERE i < 3) SELECT (SELECT n FROM plain) AS p, i FROM rec",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 3) SELECT (SELECT count(*) FROM n) AS c, i FROM n ORDER BY i",
        "WITH RECURSIVE t AS (SELECT 1 AS n) SELECT n FROM t",
    ],
    // **PostgreSQL streams the working table**, so a `LIMIT` over an unbounded recursion stops it
    // and answers `1, 1, 1`. This node materialises each iteration, so the same statement reaches
    // the iteration cap and raises. The divergence is the *cap*, not the arithmetic: any
    // implementation that does not stream has to raise here, and the alternative to raising is
    // looping forever, which invariant 9 forbids in spirit. Measured, and recorded before the cap
    // was written so the number could not be chosen to make a test pass.
    answers: &[
        (
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i FROM n) SELECT i FROM n LIMIT 3",
            "PostgreSQL streams the working table, so its LIMIT stops an unbounded recursion and it \
         answers 1, 1, 1. This node materialises each iteration, reaches the cap and raises. The \
         divergence is the cap, and the alternative to raising is looping forever.",
            "pg19_recursive_cte.txt:100",
        ),
        (
            "WITH RECURSIVE t AS (SELECT 1 AS a UNION ALL SELECT 'x'::text FROM t) SELECT * FROM t",
            "The same refusal, naming a different type: an unadorned integer literal is `int8` in \
         this crate and `integer` to PostgreSQL's resolver, so the message reads `bigint and \
         text` where a real server says `integer and text`. The rows agree — there are none — \
         and the class and code are the same; what differs is the literal's width, which is a \
         property of this node everywhere and not of recursion.",
            "pg19_recursive_cte.txt:101",
        ),
    ],
};

#[test]
fn every_recursive_cte_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_recursive_cte.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The statement the suite sends**, and the transitive answer that makes it a fixpoint.
///
/// 5 is a *grandchild*: it is reachable only through 3, which the seed did not produce. A
/// self-join would stop one level down and answer six ids.
#[test]
fn the_suites_statement_reaches_a_grandchild() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows(
            "WITH RECURSIVE \"rec_tree\" AS ( \
               SELECT \"rec_co\".* FROM \"rec_co\" WHERE \"rec_co\".\"firm_id\" IS NULL \
               UNION ALL \
               SELECT \"rec_co\".* FROM \"rec_co\" JOIN rec_tree \
                 ON rec_co.firm_id = rec_tree.id \
             ) SELECT \"rec_co\".\"id\" FROM rec_tree AS rec_co ORDER BY \"rec_co\".\"id\" ASC"
        ),
        vec![
            vec!["1"],
            vec!["2"],
            vec!["3"],
            vec!["4"],
            vec!["5"],
            vec!["6"],
            vec!["7"]
        ]
    );
}

/// **The names and the types come from the non-recursive term**, and the typmod comes with them.
#[test]
fn the_seed_names_and_types_the_relation() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    let outcome = node
        .run(
            "WITH RECURSIVE t AS (SELECT id, firm_id, name FROM rec_co WHERE firm_id IS NULL \
              UNION ALL SELECT c.id, c.firm_id, c.name FROM rec_co c JOIN t ON c.firm_id = t.id) \
             SELECT * FROM t ORDER BY id",
        )
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, rows, .. } = outcome else {
        panic!("the recursive CTE answered no rows");
    };
    assert_eq!(
        fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        vec!["id", "firm_id", "name"]
    );
    // 20 is `bigint` and 1043 is `character varying`; the typmod is the declared 255 plus the
    // four-byte header, which is what `varchar(255)` is on the wire.
    assert_eq!(fields[0].type_oid, 20);
    assert_eq!(fields[2].type_oid, 1043);
    assert_eq!(fields[2].type_modifier, 259);
    assert_eq!(rows.len(), 7);

    // The column alias list renames them outright.
    let outcome = node
        .run(
            "WITH RECURSIVE t(p, q) AS (SELECT 1, 'x'::text UNION ALL \
              SELECT p+1, q FROM t WHERE p < 2) SELECT * FROM t",
        )
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].name, "p");
    assert_eq!(fields[1].name, "q");
}

/// **`UNION` deduplicates against everything already produced**, which is what makes it terminate.
///
/// `SELECT 1 UNION SELECT 1 FROM n` would run forever against a dedup that only compared with the
/// previous iteration: every round would produce a `1` that round had not seen. It answers one row.
#[test]
fn union_dedups_against_the_whole_result_and_that_is_what_terminates() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows("WITH RECURSIVE n(i) AS (SELECT 1 UNION SELECT 1 FROM n) SELECT i FROM n"),
        vec![vec!["1"]]
    );
    // And the ordinary counting shape, which terminates by its own predicate under either operator.
    assert_eq!(
        node.rows(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 5) \
             SELECT i FROM n"
        ),
        vec![vec!["1"], vec!["2"], vec!["3"], vec!["4"], vec!["5"]]
    );
}

/// **A recursive query does not promote its arms**: the seed's type is the answer's type, and a
/// wider recursive term is an error naming the seed as the thing to fix.
#[test]
fn the_recursive_term_must_already_fit_the_seed() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    let error = node
        .run(
            "WITH RECURSIVE t AS (SELECT 1::int4 AS a UNION ALL SELECT (a+1)::int8 FROM t \
              WHERE a < 2) SELECT * FROM t",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        error.to_string(),
        "recursive query \"t\" column 1 has type integer in non-recursive term \
         but type bigint overall"
    );
    assert_eq!(
        error.hint().as_deref(),
        Some("Cast the output of the non-recursive term to the correct type.")
    );
    // Two arms with **no** common type keep the ordinary union message: this rule is about a
    // promotion that a recursive query declines to make, not about every mismatch.
    let error = node
        .run(
            "WITH RECURSIVE t AS (SELECT 1 AS a UNION ALL SELECT 'x'::text FROM t) SELECT * FROM t",
        )
        .unwrap_err();
    // **`bigint`, not `integer`**: an unadorned literal is an `int8` in this crate. The rule
    // under test is that a mismatch with no common type keeps the *ordinary* union message rather
    // than the recursive one, and that is what this asserts; the width is a declared divergence
    // of its own (see `DIVERGENCES`).
    assert_eq!(
        error.to_string(),
        "UNION types bigint and text cannot be matched"
    );
}

/// **Ten refusals in two classes**, and which class a spelling gets is measured.
#[test]
fn the_refusals_are_postgresqls_own_and_in_two_classes() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    for (statement, message) in [
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n FROM t, t AS u) SELECT n FROM t",
            "recursive reference to query \"t\" must not appear more than once",
        ),
        (
            "WITH RECURSIVE t AS (SELECT n FROM t) SELECT n FROM t",
            "recursive query \"t\" does not have the form \
             non-recursive-term UNION [ALL] recursive-term",
        ),
        (
            "WITH RECURSIVE t AS (SELECT n FROM t UNION ALL SELECT 1 AS n) SELECT n FROM t",
            "recursive reference to query \"t\" must not appear within its non-recursive term",
        ),
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT count(*) FROM t) SELECT n FROM t",
            "aggregate functions are not allowed in a recursive query's recursive term",
        ),
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT t.n FROM rec_co LEFT JOIN t \
              ON true) SELECT n FROM t",
            "recursive reference to query \"t\" must not appear within an outer join",
        ),
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n INTERSECT SELECT n FROM t) SELECT n FROM t",
            "recursive query \"t\" does not have the form \
             non-recursive-term UNION [ALL] recursive-term",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::INVALID_RECURSION,
            "{statement} was not 42P19"
        );
        assert_eq!(error.to_string(), message, "{statement}");
    }

    // **The second class is `0A000`**: not illegal, unimplemented, and PostgreSQL says so.
    for (statement, message) in [
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n FROM t ORDER BY n) \
             SELECT n FROM t",
            "ORDER BY in a recursive query is not implemented",
        ),
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n FROM t LIMIT 1) SELECT n FROM t",
            "LIMIT in a recursive query is not implemented",
        ),
        (
            "WITH RECURSIVE a AS (SELECT 1 AS n UNION ALL SELECT n FROM b), \
             b AS (SELECT n FROM a) SELECT n FROM a",
            "mutual recursion between WITH items is not implemented",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement} was not 0A000"
        );
        assert_eq!(error.to_string(), message, "{statement}");
    }
}

/// The rule about appearing once is about the **recursive term**, not the statement: the outer
/// query may name the CTE as often as it likes, and sees every row each time.
#[test]
fn the_outer_query_may_name_it_twice_and_sees_every_row() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 3) \
             SELECT (SELECT count(*) FROM n) AS c, i FROM n ORDER BY i"
        ),
        vec![vec!["3", "1"], vec!["3", "2"], vec!["3", "3"]]
    );
    // A non-recursive body in the same `RECURSIVE` list still runs, beside a recursive one.
    assert_eq!(
        node.rows(
            "WITH RECURSIVE plain AS (SELECT 1 AS n), \
             rec(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM rec WHERE i < 3) \
             SELECT (SELECT n FROM plain) AS p, i FROM rec"
        ),
        vec![vec!["1", "1"], vec!["1", "2"], vec!["1", "3"]]
    );
}

/// **The cap is an error, never a loop.** Invariant 9 in spirit: a bound that is reached raises
/// and names itself, rather than running until the process dies.
///
/// This is the one statement in this file where PostgreSQL and this node disagree, and the
/// disagreement was measured before the cap existed: PostgreSQL streams its working table, so its
/// `LIMIT` stops the recursion and answers `1, 1, 1`.
#[test]
fn an_unbounded_recursion_raises_rather_than_running_forever() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    let error = node
        .run("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i FROM n) SELECT i FROM n LIMIT 3")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::CONFIGURATION_LIMIT_EXCEEDED);
    assert!(
        error.to_string().contains("recursive query"),
        "the cap did not name what it was bounding: {error}"
    );
}
