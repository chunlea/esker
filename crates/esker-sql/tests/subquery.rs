//! Contract C3 for a subquery in an expression, and the two rules a corpus cannot carry.
//!
//! `tests/corpus/pg19_subquery_expr.txt` is 124 statements put to a real PostgreSQL 19beta1 and
//! replayed here against one node. What the corpus format records is the declared types and the
//! rows; what it cannot record is the **name** of an output column and the *order* two rules are
//! checked in, so both are asserted below with the statements that separate them.
//!
//! The one to read first is [`an_empty_subquery_beats_a_null`]. `NULL IN (SELECT … no rows)` is
//! **false** where `NULL IN (1)` is NULL — the empty case is decided before the three-valued rule
//! — and an implementation that short-circuits on a NULL operand, which is exactly what
//! `IN (list)` correctly does, answers NULL and drops the row.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The refusal a statement answers with, or a panic if it did not refuse.
///
/// Here rather than on the shared harness because it is two lines and the harness is another
/// lane's file on `main`; a helper in one test file is one hunk fewer to resolve at merge time.
fn refusal(node: &mut parity::Node, sql: &str) -> esker_sql::error::SqlError {
    node.run(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} did not refuse"))
}

/// The **names** of a query's output columns, which the corpus format records nowhere: it carries
/// the declared types and the rows, and a name is neither.
fn names(node: &mut parity::Node, sql: &str) -> Vec<String> {
    match node
        .run(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        esker_sql::pgwire::session::Outcome::Rows { fields, .. } => {
            fields.into_iter().map(|field| field.name).collect()
        }
        other => panic!("{sql} returned no result set: {other:?}"),
    }
}

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same tables the corpus builds, for the assertions a corpus cannot carry.
const FIXTURE: &[&str] = &[
    "CREATE TABLE sq_a (id int8 PRIMARY KEY, n text, k int8)",
    "INSERT INTO sq_a VALUES (1, 'one', 7), (2, 'two', NULL), (3, NULL, 9)",
    "CREATE TABLE sq_b (id int8 PRIMARY KEY, a_id int8, v int8)",
    "INSERT INTO sq_b VALUES (10, 1, 100), (11, 1, 200), (12, 3, NULL)",
    "CREATE TABLE sq_e (id int8 PRIMARY KEY, v int8)",
];

/// What this node answers differently, and why.
///
/// **Not one of them is about a subquery.** Every entry is a gap that was already there and that
/// this corpus walked into — which is what a capture is for, and the reason the list is written
/// out rather than summarised: an entry that starts agreeing fails this test too.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A bare integer constant is `integer` on a real server and `int8` here, so a subquery
        // whose target list is one is `bigint` rather than `integer`. The rows agree.
        // `tests/unknown_literal.rs` carries the eleven statements this costs.
        "SELECT (SELECT 1 FROM sq_a WHERE id = 1) AS one",
        // `sum(bigint)` is `numeric` on a real server and `int8` here — ADR 0031's decision, taken
        // because the *text* is the same characters for every input that does not overflow.
        "SELECT (SELECT sum(v) FROM sq_b)",
    ],
    answers: &[
        (
            "SELECT (SELECT max(id) FROM sq_a) + 0",
            "the operator `+` is `0A000` naming itself and was before this unit. The subquery in \
             it runs the moment arithmetic does.",
        ),
        (
            "SELECT EXISTS (SELECT 1/0 FROM sq_e)",
            "the operator `/`, as above — and the interesting half of this line is what a real \
             server does with it, which is **not** `22012`: a subquery over no rows evaluates no \
             expression, so the division never happens. When arithmetic lands, this line is the \
             one that says the evaluation is lazy rather than eager.",
        ),
    ],
};

#[test]
fn every_subquery_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_subquery_expr.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 118,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The trap: **empty is decided before NULL**, and the two rules disagree.
///
/// `NULL IN (1)` is NULL because nothing in the list can decide an unknown left-hand side.
/// `NULL IN (SELECT … no rows)` is **false**, because there is nothing to be unknown *about* — and
/// the same asymmetry runs through `= ANY` and `<> ALL`. An implementation that reused
/// `IN (list)`'s (correct) short-circuit on a NULL operand answers NULL to all six of these.
#[test]
fn an_empty_subquery_beats_a_null() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT NULL IN (SELECT v FROM sq_e)"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT NULL NOT IN (SELECT v FROM sq_e)"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT NULL = ANY (SELECT v FROM sq_e)"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT NULL <> ALL (SELECT v FROM sq_e)"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT NULL > ALL (SELECT v FROM sq_e)"),
        vec![vec!["t"]]
    );

    // And the contrast, which is the reason it is a trap at all: over a *non-empty* subquery the
    // NULL operand does make it unknown.
    assert_eq!(
        node.rows("SELECT NULL IN (SELECT a_id FROM sq_b)"),
        vec![vec!["\\N"]]
    );
    assert_eq!(
        node.rows("SELECT NULL = ANY (SELECT id FROM sq_a)"),
        vec![vec!["\\N"]]
    );
}

/// `NOT IN` over a subquery whose column has a NULL in it matches **nothing at all**.
///
/// The same rule `tests/in_list.rs` pins for a list, reached through a different door: here the
/// NULL is a *row of a table* rather than an item somebody typed, which is how it reaches a real
/// application. `sq_b.v` is `(100, 200, NULL)`.
#[test]
fn not_in_over_a_null_matches_nothing() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT id FROM sq_a WHERE id NOT IN (SELECT v FROM sq_b) ORDER BY id"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(
        node.rows("SELECT 1 NOT IN (SELECT v FROM sq_b)"),
        vec![vec!["\\N"]]
    );
    // A definite match still wins outright, whatever NULLs are beside it.
    assert_eq!(
        node.rows("SELECT 100 IN (SELECT v FROM sq_b)"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT 100 NOT IN (SELECT v FROM sq_b)"),
        vec![vec!["f"]]
    );
    // Which is the same statement as `<> ALL`, and the reason the two share one implementation.
    assert_eq!(
        node.rows("SELECT 100 <> ALL (SELECT v FROM sq_b)"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT 1 <> ALL (SELECT v FROM sq_b)"),
        vec![vec!["\\N"]]
    );
}

/// A scalar subquery: no rows is a NULL, two rows is an error, and the error is per **execution**.
#[test]
fn a_scalar_subquery_answers_one_value_or_raises() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT (SELECT max(id) FROM sq_a)"),
        vec![vec!["3"]]
    );
    assert_eq!(node.rows("SELECT (SELECT id FROM sq_e)"), vec![vec!["\\N"]]);
    assert_eq!(
        node.rows("SELECT (SELECT id FROM sq_a WHERE id = 99)"),
        vec![vec!["\\N"]]
    );

    // Two rows: `21000`, PostgreSQL's own sentence.
    let error = refusal(&mut node, "SELECT (SELECT id FROM sq_a)");
    assert_eq!(error.sqlstate(), sqlstate::CARDINALITY_VIOLATION);
    assert_eq!(
        error.to_string(),
        "more than one row returned by a subquery used as an expression"
    );

    // The **same statement** with a `LIMIT 1` is fine, which is what "per execution" means: the
    // shape of the query did not change, the number of rows did.
    assert_eq!(
        node.rows("SELECT (SELECT id FROM sq_a ORDER BY id LIMIT 1)"),
        vec![vec!["1"]]
    );
}

/// One mistake, two sentences: `42601` says different things about a scalar and about an `IN`.
///
/// Measured, and it would have been one message if it had been reasoned about. A client that
/// branches on the SQLSTATE sees one condition; a user reading the message sees which construct
/// they wrote.
#[test]
fn a_subquery_with_two_columns_is_refused_by_its_own_sentence() {
    let mut node = parity::Node::new(FIXTURE);

    let scalar = refusal(&mut node, "SELECT (SELECT id, n FROM sq_a WHERE id = 1)");
    assert_eq!(scalar.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert!(
        scalar
            .to_string()
            .contains("subquery must return only one column"),
        "{scalar}"
    );

    for statement in [
        "SELECT id FROM sq_a WHERE id IN (SELECT id, a_id FROM sq_b)",
        "SELECT 1 = ANY (SELECT id, a_id FROM sq_b)",
    ] {
        let error = refusal(&mut node, statement);
        assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
        assert!(
            error.to_string().contains("subquery has too many columns"),
            "{statement} -> {error}"
        );
    }

    // And the kind that reads no value at all takes as many columns as it likes.
    assert_eq!(
        node.rows("SELECT EXISTS (SELECT id, n FROM sq_a)"),
        vec![vec!["t"]]
    );
}

/// What a subquery's output column is **called**, which the corpus format cannot record.
///
/// Measured with `psql` against 19beta1. `ActiveRecord` reads results by name, so this is a
/// compatibility surface and not a cosmetic one.
#[test]
fn a_subquery_is_named_after_what_it_returns() {
    let mut node = parity::Node::new(FIXTURE);

    // A scalar subquery takes the subquery's **own** column name — including the inner alias.
    assert_eq!(
        names(&mut node, "SELECT (SELECT max(id) FROM sq_a)"),
        ["max"]
    );
    assert_eq!(
        names(&mut node, "SELECT (SELECT count(*) FROM sq_a)"),
        ["count"]
    );
    assert_eq!(
        names(&mut node, "SELECT (SELECT id FROM sq_a WHERE id = 1)"),
        ["id"]
    );
    assert_eq!(
        names(&mut node, "SELECT (SELECT id AS zz FROM sq_a WHERE id = 1)"),
        ["zz"]
    );
    // An outer alias still wins, as it does over any expression.
    assert_eq!(
        names(&mut node, "SELECT (SELECT max(id) FROM sq_a) AS top"),
        ["top"]
    );

    // `EXISTS` is called `exists`; its negation is an operator and is not.
    assert_eq!(
        names(&mut node, "SELECT EXISTS (SELECT 1 FROM sq_a)"),
        ["exists"]
    );
    assert_eq!(
        names(&mut node, "SELECT NOT EXISTS (SELECT 1 FROM sq_a)"),
        ["?column?"]
    );
    assert_eq!(
        names(&mut node, "SELECT 1 IN (SELECT a_id FROM sq_b)"),
        ["?column?"]
    );
    assert_eq!(
        names(&mut node, "SELECT 1 = ANY (SELECT a_id FROM sq_b)"),
        ["?column?"]
    );
}

/// Where a subquery is refused, and by what name — contract C2.
///
/// Each of these is a statement a real server runs, so each is a `0A000` that has to name the
/// construct rather than the subquery around it. They are the list `docs/plans/phase-12-subquery.md`
/// §4 promises, checked from the side that matters: the message.
#[test]
fn the_shapes_this_phase_does_not_run_name_themselves() {
    let mut node = parity::Node::new(FIXTURE);

    for (statement, named) in [
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n) SELECT n FROM t",
            "WITH",
        ),
        // The boundary with the type lane's arrays is decided by the **right-hand side** and not
        // by the quantifier: `= ANY (array)` is `IN (list)` and runs, any other operator over an
        // array is a quantifier this node does not have, and `ALL (array)` is named the same way.
        // Every one of the six over a *subquery* runs, which is what this unit built.
        (
            "SELECT id FROM sq_a WHERE id > ANY ('{1,2}')",
            "the quantifier > ANY",
        ),
        (
            "SELECT id FROM sq_a WHERE id > ALL ('{1,2}')",
            "ALL over an array",
        ),
        (
            "SELECT id FROM sq_a, LATERAL (SELECT 1) AS x",
            "a comma-separated FROM list",
        ),
    ] {
        let error = refusal(&mut node, statement);
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement} -> {error}"
        );
        assert!(
            error.to_string().contains(named),
            "{statement} -> `{error}`, which does not name `{named}`"
        );
    }

    // And the two either side of that boundary, which both run.
    assert_eq!(
        node.rows("SELECT id FROM sq_a WHERE id = ANY ('{1,2}') ORDER BY id"),
        vec![vec!["1"], vec!["2"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM sq_a WHERE id > ANY (SELECT a_id FROM sq_b) ORDER BY id"),
        vec![vec!["2"], vec!["3"]]
    );
}

/// A subquery is never routed to the columnar engine, and the refusal is by construction.
///
/// ADR 0040 and `docs/plans/phase-12-subquery.md` §4. `exec::fragment::push_filter` has one arm per
/// expression this crate has and a subquery's is a refusal, so the property is a compile error away
/// rather than a check somebody remembers — but the property itself is what a reader wants to see
/// asserted, so `EXPLAIN` is asked.
#[test]
fn a_query_with_a_subquery_plans_on_rows() {
    let mut node = parity::Node::new(FIXTURE);

    let plan = node.rows("EXPLAIN SELECT count(*) FROM sq_a WHERE id IN (SELECT a_id FROM sq_b)");
    let text = plan
        .iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("Columnar"),
        "a subquery reached the columnar engine:\n{text}"
    );
    // And the condition is still printed, because a plan a user cannot read is a plan they cannot
    // fix — the sub-plan is named rather than inlined on one line.
    assert!(text.contains("IN (subquery)"), "{text}");
}
