//! Contract C3 for a non-recursive CTE — `WITH a AS (…) SELECT …` — and the four rules that
//! decided how it is built.
//!
//! `tests/corpus/pg19_cte.txt` is 51 statements put to a real PostgreSQL 19beta1 and replayed
//! here. A CTE is **inlined at each reference**, which makes it a derived table and gives it unit
//! 2's whole executor for nothing (`crate::plan::cte`); the four things below are what that
//! decision has to answer for, and every one of them was measured before it was built.
//!
//! The one that shapes the code is [`an_unreferenced_cte_is_still_analysed`]: inlining alone never
//! looks at a CTE nobody references, and a real server does.

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
    "CREATE TABLE ct_a (id int8 PRIMARY KEY, n text, k int8)",
    "INSERT INTO ct_a VALUES (1, 'one', 7), (2, 'two', NULL), (3, NULL, 9)",
    "CREATE TABLE ct_b (id int8 PRIMARY KEY, a_id int8, v int8)",
    "INSERT INTO ct_b VALUES (10, 1, 100), (11, 1, 200), (12, 3, NULL)",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A bare integer constant is `integer` on a real server and `int8` here. The rows agree.
        "WITH ct_a AS (SELECT 99 AS id) SELECT * FROM ct_a",
        "WITH ct_a AS (SELECT 99 AS id) SELECT id FROM ct_a",
        "WITH t AS (SELECT 1 AS a, 2 AS a) SELECT * FROM t",
        "WITH t AS (SELECT id FROM ct_a) SELECT 1",
    ],
    answers: &[
        (
            "WITH RECURSIVE t AS (SELECT 1 AS n) SELECT * FROM t",
            "`WITH RECURSIVE` is `0A000` naming itself — and this line is the reason the refusal \
             is on the keyword rather than on a recursive *body*: a real server runs this one, \
             because the body does not recurse. A second evaluation model is a phase, not a unit \
             (`docs/plans/phase-12-subquery.md` §4).",
        ),
        (
            "WITH RECURSIVE t (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
             SELECT * FROM t",
            "`WITH RECURSIVE`, and `UNION ALL` under it, both `0A000` by name.",
        ),
        (
            "WITH t AS (DELETE FROM ct_b WHERE id = 12 RETURNING id) SELECT * FROM t",
            "a data-modifying `WITH` item is `0A000` naming itself: the read path is this phase \
             and the write path needs the same expressions resolved against a statement that is \
             already writing.",
        ),
        (
            "WITH t AS (INSERT INTO ct_b VALUES (99, 1, 1) RETURNING id) SELECT * FROM t",
            "a data-modifying `WITH` item, as above.",
        ),
        (
            "WITH t AS (UPDATE ct_b SET v = 1 WHERE id = 10 RETURNING id) SELECT * FROM t",
            "a data-modifying `WITH` item, as above.",
        ),
        (
            "WITH t AS (SELECT id FROM ct_a) SELECT * FROM t, t",
            "a comma-separated `FROM` list is `0A000` naming itself and was before this unit. \
             PostgreSQL gets one step further and answers `42712`; both refuse the statement.",
        ),
    ],
};

#[test]
fn every_cte_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_cte.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **An unreferenced CTE is still analysed**, and inlining alone would never look at one.
///
/// This is the finding that shapes `crate::plan::cte`: a CTE is inlined where it is referenced,
/// so a CTE nobody references contributes nothing to the plan — and a real server still reports
/// its missing column, its missing relation and its wrong-length alias list. Every `WITH` item is
/// therefore carried on the statement and planned, referenced or not, and the plan thrown away.
#[test]
fn an_unreferenced_cte_is_still_analysed() {
    let mut node = parity::Node::new(FIXTURE);

    let error = refusal(&mut node, "WITH t AS (SELECT nope FROM ct_a) SELECT 1");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);

    let error = refusal(&mut node, "WITH t AS (SELECT id FROM nosuch) SELECT 1");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);

    let error = refusal(&mut node, "WITH t (p, q) AS (SELECT id FROM ct_a) SELECT 1");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_COLUMN_REFERENCE);

    // Including one an earlier CTE fed: `b` is unreferenced, and `a` inlined into it.
    let error = refusal(
        &mut node,
        "WITH a AS (SELECT id FROM ct_a), b AS (SELECT nope FROM a) SELECT 1",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);

    // And a `WITH` whose items are all sound runs, which is what says the analysis is not simply
    // rejecting everything.
    assert_eq!(
        node.rows("WITH t AS (SELECT id FROM ct_a) SELECT 1"),
        vec![vec!["1"]]
    );
}

/// A forward reference, and a CTE referring to itself, are the same mistake with the same
/// three-part answer.
///
/// The `DETAIL` is the half that matters. `relation "a" does not exist` on its own sends a reader
/// looking for a missing table, when what is wrong is the order of two things they wrote — and the
/// `HINT` names the feature that would make it legal.
#[test]
fn a_cte_cannot_be_referenced_before_it_is_written() {
    let mut node = parity::Node::new(FIXTURE);

    for (statement, item) in [
        (
            "WITH b AS (SELECT id FROM a), a AS (SELECT id FROM ct_a) SELECT * FROM b",
            "a",
        ),
        // A CTE referring to **itself** is the same mistake: an item cannot see its own name.
        ("WITH t AS (SELECT id FROM t) SELECT * FROM t", "t"),
        // And it reaches through a derived table, and through a subquery.
        (
            "WITH b AS (SELECT id FROM (SELECT id FROM a) AS z), a AS (SELECT 1 AS id) \
             SELECT * FROM b",
            "a",
        ),
        (
            "WITH b AS (SELECT 1 AS id WHERE 1 IN (SELECT id FROM a)), a AS (SELECT 1 AS id) \
             SELECT * FROM b",
            "a",
        ),
    ] {
        let error = refusal(&mut node, statement);
        assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE, "{statement}");
        assert_eq!(
            error.to_string(),
            format!("relation \"{item}\" does not exist"),
            "{statement}"
        );
        assert_eq!(
            error.detail().as_deref(),
            Some(
                format!(
                    "There is a WITH item named \"{item}\", but it cannot be referenced from \
                     this part of the query."
                )
                .as_str()
            ),
            "{statement}"
        );
        assert_eq!(
            error.hint().as_deref(),
            Some("Use WITH RECURSIVE, or re-order the WITH items to remove forward references."),
            "{statement}"
        );
    }

    // But a **later CTE does not hide a real table** of the same name from an earlier body: the
    // flag only changes the message, and only when the catalog has nothing.
    assert_eq!(
        node.rows(
            "WITH b AS (SELECT id FROM ct_a), ct_a AS (SELECT 99 AS id) SELECT * FROM b \
             ORDER BY id"
        ),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
}

/// The nouns: four sentences under two SQLSTATEs, because PostgreSQL says `WITH query` where it
/// says `table`.
#[test]
fn a_cte_is_named_a_with_query_and_not_a_table() {
    let mut node = parity::Node::new(FIXTURE);

    let error = refusal(
        &mut node,
        "WITH t (p, q) AS (SELECT id FROM ct_a) SELECT * FROM t",
    );
    assert_eq!(error.sqlstate(), sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        error.to_string(),
        "WITH query \"t\" has 1 columns available but 2 columns specified"
    );

    // The same shape as a derived table, with the other noun.
    let error = refusal(&mut node, "SELECT * FROM (SELECT id FROM ct_a) AS t (p, q)");
    assert_eq!(
        error.to_string(),
        "table \"t\" has 1 columns available but 2 columns specified"
    );

    let error = refusal(
        &mut node,
        "WITH t AS (SELECT id FROM ct_a), t AS (SELECT 1) SELECT * FROM t",
    );
    assert_eq!(error.sqlstate(), "42712");
    assert_eq!(
        error.to_string(),
        "WITH query name \"t\" specified more than once"
    );
}

/// A CTE **shadows a real table of the same name**, and is visible from a subquery and from a
/// derived table inside the statement.
#[test]
fn a_cte_shadows_a_table_and_reaches_every_corner() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("WITH ct_a AS (SELECT 99 AS id) SELECT id FROM ct_a"),
        vec![vec!["99"]]
    );
    // A subquery in the `WHERE`.
    assert_eq!(
        node.rows(
            "WITH t AS (SELECT id FROM ct_a) SELECT id FROM ct_a WHERE id IN \
             (SELECT id FROM t) ORDER BY id"
        ),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    // A derived table.
    assert_eq!(
        node.rows(
            "WITH t AS (SELECT id FROM ct_a) SELECT * FROM (SELECT id FROM t) AS u ORDER BY id"
        ),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    // A `WITH` nested inside a subquery, which shadows nothing here and resolves on its own.
    assert_eq!(
        node.rows(
            "SELECT id FROM ct_a WHERE id IN (WITH t AS (SELECT a_id FROM ct_b) \
             SELECT a_id FROM t) ORDER BY id"
        ),
        vec![vec!["1"], vec!["3"]]
    );
}

/// Referenced twice, and joined to itself under two aliases — the case PostgreSQL materialises and
/// this node reads twice.
///
/// Reading it twice is a **cost**, not a different answer: a non-recursive CTE over a read-only
/// statement has nothing in it that can run twice to different effect, and this node refuses every
/// volatile function inside a subquery. Recorded rather than hidden (`crate::plan::cte`).
#[test]
fn a_cte_referenced_twice_answers_the_same_twice() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "WITH t AS (SELECT id FROM ct_a) SELECT (SELECT count(*) FROM t), \
             (SELECT max(id) FROM t)"
        ),
        vec![vec!["3", "3"]]
    );
    assert_eq!(
        node.rows(
            "WITH t AS (SELECT id FROM ct_a) SELECT * FROM t x JOIN t y ON x.id = y.id \
             ORDER BY x.id"
        ),
        vec![vec!["1", "1"], vec!["2", "2"], vec!["3", "3"]]
    );
    // And the hint that chooses between inlining and materialising changes neither answer.
    for statement in [
        "WITH t AS MATERIALIZED (SELECT id FROM ct_a) SELECT * FROM t ORDER BY id",
        "WITH t AS NOT MATERIALIZED (SELECT id FROM ct_a) SELECT * FROM t ORDER BY id",
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec!["1"], vec!["2"], vec!["3"]],
            "{statement}"
        );
    }
}

/// One CTE reading an earlier one, which is the chain the inlining walks in order.
#[test]
fn a_cte_may_read_an_earlier_one() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "WITH a AS (SELECT id FROM ct_a), b AS (SELECT id FROM a WHERE id > 1) \
             SELECT * FROM b ORDER BY id"
        ),
        vec![vec!["2"], vec!["3"]]
    );
    assert_eq!(
        node.rows(
            "WITH a AS (SELECT id FROM ct_a), b AS (SELECT count(*) AS c FROM a) SELECT * FROM b"
        ),
        vec![vec!["3"]]
    );
}
