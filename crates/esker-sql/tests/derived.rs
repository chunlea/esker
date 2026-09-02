//! Contract C3 for a derived table — `FROM (SELECT …) AS t` — and the three rules a reader would
//! have guessed wrong.
//!
//! `tests/corpus/pg19_subquery_from.txt` is 65 statements put to a real PostgreSQL 19beta1 and
//! replayed here. The three at the top of that file are the ones this test file exists to keep:
//! the alias is **optional** on 19, a column alias list may be **shorter** than the target list,
//! and a `FROM` item cannot see the ones beside it — which is what `LATERAL` is for, and this
//! phase does not have it.
//!
//! The shape underneath is one idea: a derived table becomes a **synthetic table definition**
//! whose columns are the sub-select's output columns, so name resolution, `SELECT *`, `EXPLAIN`
//! and the join machinery all work against a relation and never learn that nothing stores it
//! (`docs/plans/phase-12-subquery.md` §3 unit 2).

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

/// The **names** of a query's output columns, which the corpus format records nowhere.
fn names(node: &mut parity::Node, sql: &str) -> Vec<String> {
    match node
        .run(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        esker_sql::pgwire::session::Outcome::Rows { fields, .. } => {
            fields.into_iter().map(|field| field.name).collect()
        }
        // Named rather than a wildcard: `Outcome` has two variants, so a `_` here would
        // silently swallow a third if one is ever added.
        other @ esker_sql::pgwire::session::Outcome::Done { .. } => {
            panic!("{sql} returned no result set: {other:?}")
        }
    }
}

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same tables the corpus builds.
const FIXTURE: &[&str] = &[
    "CREATE TABLE dt_a (id int8 PRIMARY KEY, n text, k int8)",
    "INSERT INTO dt_a VALUES (1, 'one', 7), (2, 'two', NULL), (3, NULL, 9)",
    "CREATE TABLE dt_b (id int8 PRIMARY KEY, a_id int8, v int8)",
    "INSERT INTO dt_b VALUES (10, 1, 100), (11, 1, 200), (12, 3, NULL)",
];

/// What this node answers differently, and why. **Not one of them is about a derived table.**
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A bare integer constant is `integer` on a real server and `int8` here. The rows agree.
        "SELECT * FROM (SELECT 1) AS t",
        "SELECT * FROM (SELECT 1 AS a) AS t",
        "SELECT * FROM (SELECT 1 AS a, 2 AS a) AS t",
        // `sum(bigint)` is `numeric` there and `int8` here — ADR 0031, because the text agrees for
        // every input that does not overflow.
        "SELECT sum(x) FROM (SELECT k AS x FROM dt_a) AS t",
    ],
    answers: &[(
        "SELECT * FROM (SELECT id FROM dt_a WHERE id = a.id) AS t, dt_a a",
        "a comma-separated `FROM` list is `0A000` naming itself and was before this unit — refused \
         rather than lowered to a cross join, because the comma form usually means a `WHERE` was \
         meant to join them. PostgreSQL stops one step earlier, on the `a.id` that no `LATERAL` \
         makes visible; both refuse the statement and neither runs it.",
    )],
};

#[test]
fn every_derived_table_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_subquery_from.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The alias is **optional** on PostgreSQL 19, and then the relation has no name at all.
///
/// It was mandatory before 16 and most of the documentation a reader finds still says so, which is
/// why this is measured rather than assumed. Without an alias there is nothing to qualify with —
/// and an implementation that generated a name would be inventing one a user could collide with.
#[test]
fn a_derived_table_needs_no_alias() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT * FROM (SELECT id FROM dt_a) ORDER BY id"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM (SELECT id FROM dt_a) ORDER BY id"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    assert_eq!(
        node.rows("SELECT * FROM (SELECT id FROM dt_a) WHERE id = 2"),
        vec![vec!["2"]]
    );
    // And with one, the alias is the **only** name: the inner table's own is gone, which is the
    // same rule `tests/corpus/pg19_alias.txt` pinned for a real table.
    let error = refusal(&mut node, "SELECT dt_a.id FROM (SELECT id FROM dt_a) AS t");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}

/// A column alias list may be **shorter** than the target list, and a longer one is `42P10`.
///
/// The asymmetry is the finding. `AS t (a)` over two columns renames the first and leaves the
/// second under its own name, so `SELECT a, n FROM … AS t (a)` returns both — where refusing it,
/// which reads like the obvious symmetry, would refuse a statement a real server runs.
#[test]
fn a_column_alias_list_may_be_short_but_not_long() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        names(&mut node, "SELECT * FROM (SELECT id, n FROM dt_a) AS t (a)"),
        ["a", "n"]
    );
    assert_eq!(
        node.rows("SELECT a, n FROM (SELECT id, n FROM dt_a) AS t (a) ORDER BY a"),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "\\N"],]
    );

    // And it **replaces**: the column's own name is gone.
    let error = refusal(
        &mut node,
        "SELECT id FROM (SELECT id, n FROM dt_a) AS t (a, b)",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);

    // Longer than the target list is `42P10`, with a sentence that counts both sides.
    let error = refusal(&mut node, "SELECT * FROM (SELECT id FROM dt_a) AS t (a, b)");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        error.to_string(),
        "table \"t\" has 1 columns available but 2 columns specified"
    );
}

/// `SELECT *` over a derived table returns **every** column of its target list.
///
/// The regression this guards is specific and was one line away: a table with no declared primary
/// key hides column 0 as an internal row id, and a synthetic definition has no primary key — so
/// without `TableDef::row_id` naming the derived id, `SELECT * FROM (SELECT id, n …) AS t` comes
/// back one column short. The same wrong answer a `pg_catalog` view gave before the line above it.
#[test]
fn select_star_over_a_derived_table_hides_nothing() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        names(&mut node, "SELECT * FROM (SELECT id, n, k FROM dt_a) AS t"),
        ["id", "n", "k"]
    );
    assert_eq!(
        node.rows("SELECT * FROM (SELECT id, n FROM dt_a) AS t ORDER BY id"),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "\\N"],]
    );
    assert_eq!(
        node.rows("SELECT t.* FROM (SELECT id, n FROM dt_a) AS t ORDER BY 1"),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "\\N"],]
    );
}

/// An aggregate over a derived table — the shape `ActiveRecord`'s `.from(subquery)` and its
/// `count` over a `distinct` relation emit.
#[test]
fn an_aggregate_over_a_derived_table_counts_its_rows() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT id FROM dt_a) AS t"),
        vec![vec!["3"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT DISTINCT a_id FROM dt_b) AS t"),
        vec![vec!["2"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT id FROM dt_a LIMIT 2) AS t"),
        vec![vec!["2"]]
    );
    // The `LIMIT` inside is the interesting one: it is the sub-plan's, so the count is of what the
    // sub-plan returned and not of the table.
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT id FROM dt_a WHERE id > 99) AS t"),
        vec![vec!["0"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT id FROM dt_a) AS t WHERE t.id > 1"),
        vec![vec!["2"]]
    );
}

/// A join with a derived table on either side, including the one that must not drop rows.
#[test]
fn a_derived_table_joins_on_either_side() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "SELECT t.id, b.v FROM (SELECT id FROM dt_a) AS t JOIN dt_b b ON b.a_id = t.id \
             ORDER BY t.id, b.v"
        ),
        vec![vec!["1", "100"], vec!["1", "200"], vec!["3", "\\N"],]
    );
    assert_eq!(
        node.rows(
            "SELECT a.id, t.v FROM dt_a a JOIN (SELECT a_id, v FROM dt_b) AS t \
             ON t.a_id = a.id ORDER BY a.id, t.v"
        ),
        vec![vec!["1", "100"], vec!["1", "200"], vec!["3", "\\N"],]
    );
    // A left join keeps the row that matched nothing, with every derived column NULL.
    assert_eq!(
        node.rows(
            "SELECT a.id, t.v FROM dt_a a LEFT JOIN (SELECT a_id, v FROM dt_b) AS t \
             ON t.a_id = a.id ORDER BY a.id, t.v"
        ),
        vec![
            vec!["1", "100"],
            vec!["1", "200"],
            vec!["2", "\\N"],
            vec!["3", "\\N"],
        ]
    );
    // Two derived tables joined to each other.
    assert_eq!(
        node.rows(
            "SELECT * FROM (SELECT id FROM dt_a) AS t JOIN (SELECT a_id FROM dt_b) AS u \
             ON u.a_id = t.id ORDER BY t.id"
        ),
        vec![vec!["1", "1"], vec!["1", "1"], vec!["3", "3"]]
    );
    // And a name used twice is the same `42712` any two `FROM` entries get.
    let error = refusal(
        &mut node,
        "SELECT * FROM (SELECT id FROM dt_a) AS t JOIN dt_b AS t ON t.id = t.id",
    );
    assert_eq!(error.sqlstate(), "42712");
}

/// A `FROM` item cannot see the ones beside it — which is what `LATERAL` is for, and this phase
/// refuses it by name.
#[test]
fn a_derived_table_cannot_see_its_neighbours() {
    let mut node = parity::Node::new(FIXTURE);

    // `LATERAL` names itself rather than being approximated.
    let error = refusal(
        &mut node,
        "SELECT * FROM dt_a a JOIN LATERAL (SELECT id FROM dt_b WHERE id = a.id) AS t ON true",
    );
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("LATERAL"), "{error}");

    // And without it, a reference to the table beside it does not resolve. PostgreSQL says
    // `42P01 missing FROM-clause entry`; this node stops one step earlier on the comma-separated
    // `FROM` list it refuses by name, which the corpus records as a declared divergence.
    let error = refusal(
        &mut node,
        "SELECT t.id FROM dt_a a JOIN (SELECT id FROM dt_b WHERE id = a.id) AS t ON t.id = a.id",
    );
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}

/// A derived table is never routed to the columnar engine, and neither is a join against one.
#[test]
fn a_derived_table_plans_on_rows() {
    let mut node = parity::Node::new(FIXTURE);

    let plan = node.rows("EXPLAIN SELECT count(*) FROM (SELECT id FROM dt_a) AS t");
    let text = plan
        .iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("Columnar"),
        "a derived table reached the columnar engine:\n{text}"
    );
    // The sub-plan is printed as a plan, which is the whole reason it is a `Node` and not a value:
    // a reader sees the scan the derived table costs.
    assert!(text.contains("Seq Scan on dt_a"), "{text}");
}
