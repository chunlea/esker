//! Contract C3 for a correlated subquery, and the rule that is about **names** rather than rows.
//!
//! `tests/corpus/pg19_subquery_correlated.txt` is 34 statements put to a real PostgreSQL 19beta1
//! and replayed here. The one to read first is [`the_inner_scope_shadows_the_outer`]: an
//! unqualified name that the *inner* query has is the inner one's, silently, and the same
//! statement written with a qualifier asks a different question.
//!
//! The executor underneath is a nested loop. The sub-plan is copied per outer row with every outer
//! reference in it replaced by the value that row has there, so what a cursor is opened on is an
//! ordinary plan with nothing correlated left in it (`docs/plans/phase-12-subquery.md` §1).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The refusal a statement answers with, or a panic if it did not refuse.
fn refusal(node: &mut parity::Node, sql: &str) -> esker_sql::error::SqlError {
    node.run(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} did not refuse"))
}

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same tables the corpus builds.
const FIXTURE: &[&str] = &[
    "CREATE TABLE co_a (id int8 PRIMARY KEY, n text, k int8)",
    "INSERT INTO co_a VALUES (1, 'one', 7), (2, 'two', NULL), (3, NULL, 9)",
    "CREATE TABLE co_b (id int8 PRIMARY KEY, a_id int8, v int8, n text)",
    "INSERT INTO co_b VALUES (10, 1, 100, 'x'), (11, 1, 200, 'y'), (12, 3, NULL, NULL)",
];

/// What this node answers differently, and why. **Not one of them is about correlation.**
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_correlated_subquery_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_subquery_correlated.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The inner scope shadows the outer, silently.**
///
/// An unqualified name the sub-select's own tables have is theirs, and the statement is not
/// correlated at all. There is no warning and no error — a resolution rule that looked outward
/// first, or outward when the inner column "did not look intended", would answer a different
/// question and say nothing about it. Four statements, from four directions.
#[test]
fn the_inner_scope_shadows_the_outer() {
    let mut node = parity::Node::new(FIXTURE);

    // `id` is `co_b`'s, so this is `b.a_id = b.id` — and `co_b`'s ids are 10, 11, 12.
    assert_eq!(
        node.rows("SELECT id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = id) ORDER BY id"),
        Vec::<Vec<String>>::new()
    );
    // The same statement with the qualifier is the correlated one, and returns two rows.
    assert_eq!(
        node.rows(
            "SELECT id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = a.id) \
             ORDER BY id"
        ),
        vec![vec!["1"], vec!["3"]]
    );
    // `n` is `co_b`'s too, so `b.n = n` is `b.n = b.n`: true for every row that has one, which is
    // every outer row.
    assert_eq!(
        node.rows("SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.n = n) ORDER BY a.id"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    // And with the outer one qualified, `n` is still `co_b`'s: `b.n = a.n` matches nothing.
    assert_eq!(
        node.rows("SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE n = a.n) ORDER BY a.id"),
        Vec::<Vec<String>>::new()
    );
    // `id = 1` is `b.id = 1`, and no row of `co_b` has id 1.
    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE id = 1) ORDER BY a.id"
        ),
        Vec::<Vec<String>>::new()
    );
    // A name the inner query does **not** have walks out, with no qualifier needed.
    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b WHERE a_id = a.id) \
             ORDER BY a.id"
        ),
        vec![vec!["1"], vec!["3"]]
    );
}

/// A correlated scalar subquery: one value per outer row, NULL where it matched nothing, and
/// `21000` when it matched two.
#[test]
fn a_correlated_scalar_answers_per_row() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "SELECT a.id, (SELECT max(v) FROM co_b b WHERE b.a_id = a.id) FROM co_a a ORDER BY a.id"
        ),
        vec![vec!["1", "200"], vec!["2", "\\N"], vec!["3", "\\N"],]
    );
    // `count` over no matching rows is 0, not NULL — the ungrouped empty-input rule, reached once
    // per outer row.
    assert_eq!(
        node.rows("SELECT a.id, (SELECT count(*) FROM co_b b WHERE b.a_id = a.id) FROM co_a a ORDER BY a.id"),
        vec![vec!["1", "2"], vec!["2", "0"], vec!["3", "1"]]
    );
    // Two rows for **one** outer row is the error, which is why it is per execution: `a.id = 1`
    // matches two `co_b` rows and the other two outer rows would have been fine.
    let error = refusal(
        &mut node,
        "SELECT a.id, (SELECT v FROM co_b b WHERE b.a_id = a.id) FROM co_a a ORDER BY a.id",
    );
    assert_eq!(error.sqlstate(), sqlstate::CARDINALITY_VIOLATION);

    // An `ORDER BY … LIMIT 1` inside makes the same statement legal, per row.
    assert_eq!(
        node.rows(
            "SELECT a.id, (SELECT n FROM co_b b WHERE b.a_id = a.id ORDER BY b.id LIMIT 1) \
             FROM co_a a ORDER BY a.id"
        ),
        vec![vec!["1", "x"], vec!["2", "\\N"], vec!["3", "\\N"]]
    );
}

/// The smallest correlated query there is: a subquery with **no `FROM` at all**.
#[test]
fn a_subquery_with_no_from_can_still_be_correlated() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT a.id, (SELECT a.id) FROM co_a a ORDER BY a.id"),
        vec![vec!["1", "1"], vec!["2", "2"], vec!["3", "3"]]
    );
    assert_eq!(
        node.rows("SELECT (SELECT a.n) FROM co_a a ORDER BY 1"),
        vec![vec!["one"], vec!["two"], vec!["\\N"]]
    );
}

/// Correlation reaches **two levels out**, which is why an outer reference carries how far.
///
/// The innermost query names the middle table and the outermost one. An implementation with a
/// single "outer row" would substitute the wrong one into the wrong place — and would do it
/// silently, because both are `bigint`.
#[test]
fn correlation_reaches_past_one_level() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE EXISTS \
             (SELECT 1 FROM co_a c WHERE c.id = b.a_id AND c.id = a.id)) ORDER BY a.id"
        ),
        vec![vec!["1"], vec!["3"]]
    );
    // A table correlated to itself under two aliases.
    assert_eq!(
        node.rows("SELECT x.id FROM co_a x WHERE EXISTS (SELECT 1 FROM co_a y WHERE y.id > x.id) ORDER BY x.id"),
        vec![vec!["1"], vec!["2"]]
    );
}

/// Every shape a correlated subquery comes in, not only `EXISTS`.
#[test]
fn correlation_works_in_every_shape() {
    let mut node = parity::Node::new(FIXTURE);

    // `NOT EXISTS`.
    assert_eq!(
        node.rows("SELECT id FROM co_a a WHERE NOT EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = a.id) ORDER BY id"),
        vec![vec!["2"]]
    );
    // `IN`, with the correlation on the right.
    assert_eq!(
        node.rows("SELECT a.id FROM co_a a WHERE a.k IN (SELECT b.v FROM co_b b WHERE b.a_id = a.id) ORDER BY a.id"),
        Vec::<Vec<String>>::new()
    );
    // `= ANY`, with the correlation in the sub-select's own `WHERE`.
    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a WHERE a.id = ANY (SELECT b.a_id FROM co_b b WHERE b.id > a.id) \
             ORDER BY a.id"
        ),
        vec![vec!["1"], vec!["3"]]
    );
    // A scalar in the `WHERE`, compared.
    assert_eq!(
        node.rows("SELECT a.id FROM co_a a WHERE (SELECT count(*) FROM co_b b WHERE b.a_id = a.id) > 1 ORDER BY a.id"),
        vec![vec!["1"]]
    );
    // In `HAVING`, over a grouped row.
    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a GROUP BY a.id HAVING count(*) > \
             (SELECT count(*) FROM co_b b WHERE b.a_id = a.id) ORDER BY a.id"
        ),
        vec![vec!["2"]]
    );
    // In `ORDER BY`, which sorts by a value computed per row.
    assert_eq!(
        node.rows(
            "SELECT a.id FROM co_a a ORDER BY (SELECT max(v) FROM co_b b WHERE b.a_id = a.id) \
             NULLS FIRST, a.id"
        ),
        vec![vec!["2"], vec!["3"], vec!["1"]]
    );
    // Correlated to the **joined** row, which is what makes the outer scope a row and not a table.
    assert_eq!(
        node.rows(
            "SELECT a.id, b.v FROM co_a a JOIN co_b b ON b.a_id = a.id WHERE EXISTS \
             (SELECT 1 FROM co_a c WHERE c.id = b.a_id) ORDER BY a.id, b.v"
        ),
        vec![vec!["1", "100"], vec!["1", "200"], vec!["3", "\\N"],]
    );
    // And under an aggregate, where the filter runs before the fold.
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = a.id)"
        ),
        vec![vec!["2"]]
    );
}

/// The three name errors keep the shapes they already had, at whichever level they are made.
#[test]
fn a_name_no_level_has_is_the_error_it_already_was() {
    let mut node = parity::Node::new(FIXTURE);

    let error = refusal(
        &mut node,
        "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.nope = a.id)",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    assert_eq!(error.to_string(), "column b.nope does not exist");

    let error = refusal(
        &mut node,
        "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = a.nope)",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    assert_eq!(error.to_string(), "column a.nope does not exist");

    let error = refusal(
        &mut node,
        "SELECT a.id FROM co_a a WHERE EXISTS (SELECT 1 FROM co_b b WHERE b.a_id = z.id)",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert!(
        error.to_string().contains("missing FROM-clause entry"),
        "{error}"
    );
}

/// A **correlated** `LIMIT` or `OFFSET` is `42P10`, and it has to be.
///
/// A limit that changed per row would not be a limit, and there is no row there to change with —
/// PostgreSQL says so in a sentence that names the clause, and this node sends the same one. It is
/// the one place a correlated subquery is refused rather than run.
#[test]
fn a_limit_may_not_contain_a_variable() {
    let mut node = parity::Node::new(FIXTURE);

    for clause in ["LIMIT", "OFFSET"] {
        let error = refusal(
            &mut node,
            &format!(
                "SELECT a.id FROM co_a a {clause}                  (SELECT count(*) FROM co_b b WHERE b.a_id = a.id)"
            ),
        );
        assert_eq!(error.sqlstate(), sqlstate::INVALID_COLUMN_REFERENCE);
        assert_eq!(
            error.to_string(),
            format!("argument of {clause} must not contain variables")
        );
    }

    // The **uncorrelated** one in the same place runs, which is what says the refusal is about
    // the variable and not about the subquery.
    assert_eq!(
        node.rows("SELECT a.id FROM co_a a ORDER BY a.id LIMIT (SELECT count(*) FROM co_b b)"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
}

/// `LATERAL` — a `FROM` item that sees the ones beside it — is refused by name, and is the one
/// place correlation is *not* available.
#[test]
fn lateral_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);

    let error = refusal(
        &mut node,
        "SELECT a.id FROM co_a a JOIN LATERAL (SELECT b.id FROM co_b b WHERE b.a_id = a.id) AS t \
         ON true",
    );
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("LATERAL"), "{error}");

    // And without it a derived table cannot see the row beside it, which is the same `42P01` a
    // real server gives.
    let error = refusal(
        &mut node,
        "SELECT t.id FROM co_a a JOIN (SELECT b.id FROM co_b b WHERE b.a_id = a.id) AS t \
         ON t.id = a.id",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}
