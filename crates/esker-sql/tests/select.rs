//! `SELECT`, and the three-valued logic that makes it easy to get subtly wrong.
//!
//! Every expectation here was taken from a real PostgreSQL 19 running the same statements over the
//! same rows. The ones worth naming: `WHERE n = NULL` matches nothing (that is what `IS NULL` is
//! for), `NOT (n = 10)` drops the NULL row as well as the 10, and `ORDER BY` puts NULLs last
//! ascending and first descending — which is one rule about where NULL sits in the value order,
//! not two rules about direction.
//!
//! The one place we deliberately answer differently is `ORDER BY` on `text`: the fixture's server
//! sorts `a, b, B` under `en_US.utf8` and a byte-ordered key space sorts `B, a, b`. That is
//! `crate::row`'s declared divergence, and it is asserted here in the form it actually takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;
use esker_sql::value::ColumnType;
use esker_sql::value::PgType;

struct Node {
    executor: Executor,
}

impl Node {
    /// A node with `s1` loaded: the same four rows the capture used.
    fn loaded() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Node {
            executor: Executor::new(
                backend,
                Arc::new(Catalog::new()),
                1,
                esker_sql::session::register(),
            ),
        };
        node.run("CREATE TABLE s1 (id int8 PRIMARY KEY, n int8, t text)")
            .unwrap();
        node.run("INSERT INTO s1 VALUES (1, 10, 'a'), (2, NULL, 'B'), (3, 30, NULL), (4, 20, 'b')")
            .unwrap();
        node
    }

    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    }

    /// The rows a query returns, each column as the text a client would see, `NULL` for a NULL.
    fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| {
                            value.map_or_else(
                                || "NULL".to_owned(),
                                |bytes| String::from_utf8(bytes).unwrap(),
                            )
                        })
                        .collect()
                })
                .collect(),
            Outcome::Done { tag } => panic!("not rows: {tag}"),
        }
    }

    /// The first column of every row, joined — the shape most of these assertions want.
    fn column(&mut self, sql: &str) -> Vec<String> {
        self.rows(sql)
            .into_iter()
            .map(|row| row[0].clone())
            .collect()
    }
}

#[test]
fn a_star_projection_returns_every_column_with_its_own_name_and_type() {
    let mut node = Node::loaded();
    let Outcome::Rows { fields, rows, tag } = node.run("SELECT * FROM s1 WHERE id = 1").unwrap()
    else {
        panic!("not rows")
    };
    assert_eq!(
        fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        ["id", "n", "t"]
    );
    assert_eq!(fields[0].type_oid, ColumnType::Int8.oid());
    assert_eq!(fields[2].type_oid, ColumnType::Text.oid());
    assert_eq!(tag, "SELECT 1");
    assert_eq!(rows.len(), 1);
}

/// A bare column keeps its name; anything else is `?column?`, which is what `psql` prints.
#[test]
fn output_column_names_are_postgresqls_own() {
    let mut node = Node::loaded();
    let Outcome::Rows { fields, .. } = node
        .run("SELECT id, id AS renamed, 1, 'x' FROM s1 WHERE id = 1")
        .unwrap()
    else {
        panic!("not rows")
    };
    assert_eq!(
        fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        ["id", "renamed", "?column?", "?column?"]
    );
}

/// `SELECT 1` needs no table. Drivers probe a connection with it.
#[test]
fn a_select_with_no_from_returns_one_row() {
    let mut node = Node::loaded();
    assert_eq!(node.rows("SELECT 1, 'x', true"), [["1", "x", "t"]]);
    assert_eq!(
        node.run("SELECT 1").unwrap(),
        Outcome::Rows {
            fields: vec![esker_sql::pgwire::message::FieldDescription::computed(
                "?column?",
                // **`Int4`, since the literal ladder gained its `int4` rung**: a driver
                // probing a connection with `SELECT 1` is told `integer`, as a real server tells
                // it.
                ColumnType::Int4
            )],
            rows: vec![vec![Some(b"1".to_vec())]],
            tag: "SELECT 1".to_owned(),
        }
    );
}

/// The heart of it. `= NULL` is never true, and `NOT` of unknown is unknown, so the NULL row is
/// dropped by both — which is why `IS NULL` has to exist at all.
#[test]
fn null_is_never_equal_to_anything_including_null() {
    let mut node = Node::loaded();
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n = NULL"),
        [] as [String; 0]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n <> NULL"),
        [] as [String; 0]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE NOT (n = 10) ORDER BY id"),
        ["3", "4"],
        "the NULL row is dropped by NOT too, because NOT unknown is unknown"
    );
    assert_eq!(node.column("SELECT id FROM s1 WHERE n IS NULL"), ["2"]);
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n IS NOT NULL ORDER BY id"),
        ["1", "3", "4"]
    );
}

/// Three-valued `AND` and `OR`, which are not symmetric: a definite `false` settles an `AND` and a
/// definite `true` settles an `OR`, whatever the other side is.
#[test]
fn and_and_or_settle_on_a_definite_answer() {
    let mut node = Node::loaded();
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n = 10 OR t IS NULL ORDER BY id"),
        ["1", "3"]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n = 10 AND t IS NULL"),
        [] as [String; 0]
    );
    // `n = 10` is unknown for row 2, but `id = 2` is false, so the AND is definitely false.
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n = 10 AND id = 99"),
        [] as [String; 0]
    );
    // And unknown OR true is true.
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n = 99 OR id = 2"),
        ["2"]
    );
}

/// NULLs last ascending, first descending, and an explicit `NULLS FIRST` overrides.
#[test]
fn order_by_puts_nulls_where_postgresql_puts_them() {
    let mut node = Node::loaded();
    assert_eq!(
        node.column("SELECT n FROM s1 ORDER BY n"),
        ["10", "20", "30", "NULL"]
    );
    assert_eq!(
        node.column("SELECT n FROM s1 ORDER BY n DESC"),
        ["NULL", "30", "20", "10"]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 ORDER BY n NULLS FIRST, id"),
        ["2", "1", "4", "3"]
    );
    assert_eq!(
        node.column("SELECT n FROM s1 ORDER BY n DESC NULLS LAST"),
        ["30", "20", "10", "NULL"]
    );
}

/// The declared divergence, in the form it takes. A real PostgreSQL with an `en_US.utf8` database
/// returns `a, b, B`; a byte-ordered key space returns `B, a, b`, which is what that server returns
/// under `COLLATE "C"`.
#[test]
fn text_sorts_by_bytes_which_is_collate_c_and_not_the_database_default() {
    let mut node = Node::loaded();
    assert_eq!(
        node.column("SELECT t FROM s1 ORDER BY t"),
        ["B", "a", "b", "NULL"]
    );
}

#[test]
fn limit_and_offset_take_a_window_of_the_ordered_rows() {
    let mut node = Node::loaded();
    assert_eq!(
        node.column("SELECT id FROM s1 ORDER BY id LIMIT 2 OFFSET 1"),
        ["2", "3"]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 ORDER BY id LIMIT 0"),
        [] as [String; 0]
    );
    assert_eq!(node.column("SELECT id FROM s1 ORDER BY id OFFSET 3"), ["4"]);
    // `LIMIT NULL` is no limit, which is PostgreSQL's rule.
    assert_eq!(
        node.column("SELECT id FROM s1 ORDER BY id LIMIT NULL")
            .len(),
        4
    );
    assert_eq!(
        node.column("SELECT id FROM s1 ORDER BY id LIMIT 99"),
        ["1", "2", "3", "4"]
    );
}

/// Two clauses, two codes: a client is told which one it got wrong.
#[test]
fn a_negative_limit_and_a_negative_offset_have_different_codes() {
    let mut node = Node::loaded();
    let error = node.run("SELECT id FROM s1 LIMIT -1").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE
    );
    assert_eq!(error.to_string(), "LIMIT must not be negative");

    let error = node.run("SELECT id FROM s1 OFFSET -1").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE
    );
    assert_eq!(error.to_string(), "OFFSET must not be negative");
}

#[test]
fn comparisons_read_a_quoted_literal_as_the_columns_type() {
    let mut node = Node::loaded();
    assert_eq!(node.column("SELECT id FROM s1 WHERE n = '10'"), ["1"]);
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE n > 15 ORDER BY id"),
        ["3", "4"]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE t >= 'a' ORDER BY id"),
        ["1", "4"],
        "byte order again: 'B' is below 'a'"
    );
}

#[test]
fn a_name_that_is_not_there_is_reported_before_anything_runs() {
    let mut node = Node::loaded();
    assert_eq!(
        node.run("SELECT * FROM nope").unwrap_err().to_string(),
        "relation \"nope\" does not exist"
    );
    assert_eq!(
        node.run("SELECT nope FROM s1").unwrap_err().sqlstate(),
        sqlstate::UNDEFINED_COLUMN
    );
    assert_eq!(
        node.run("SELECT id FROM s1 ORDER BY nope")
            .unwrap_err()
            .sqlstate(),
        sqlstate::UNDEFINED_COLUMN
    );
    assert_eq!(
        node.run("SELECT id FROM s1 WHERE nope = 1")
            .unwrap_err()
            .sqlstate(),
        sqlstate::UNDEFINED_COLUMN
    );
}

/// `EXPLAIN` shows the access path, which is the part a user changes their schema over. A whole
/// primary key pinned to a constant is a point read, not a scan of the table.
#[test]
fn explain_shows_which_access_path_was_chosen() {
    let mut node = Node::loaded();
    node.run("CREATE UNIQUE INDEX s1_t_key ON s1 (t)").unwrap();

    let plan = |node: &mut Node, sql: &str| node.column(sql).join("\n");

    let point = plan(&mut node, "EXPLAIN SELECT * FROM s1 WHERE id = 1");
    assert!(point.contains("Point Get on s1"), "{point}");
    assert!(!point.contains("Seq Scan"), "{point}");

    let lookup = plan(&mut node, "EXPLAIN SELECT * FROM s1 WHERE t = 'a'");
    assert!(lookup.contains("Index Lookup on s1"), "{lookup}");
    assert!(lookup.contains("Index: s1_t_key"), "{lookup}");

    let scan = plan(&mut node, "EXPLAIN SELECT * FROM s1 WHERE n = 10");
    assert!(scan.contains("Seq Scan on s1"), "{scan}");
    assert!(scan.contains("Filter"), "{scan}");

    let narrowed = plan(&mut node, "EXPLAIN SELECT * FROM s1 WHERE id > 2");
    assert!(narrowed.contains("Range: narrowed"), "{narrowed}");

    let sorted = plan(
        &mut node,
        "EXPLAIN SELECT id FROM s1 ORDER BY id DESC LIMIT 2",
    );
    assert!(sorted.contains("Limit 2 Offset 0"), "{sorted}");
    assert!(sorted.contains("Sort"), "{sorted}");
}

/// A pinned key under an `OR` is not pinned at all: the rows the other branch matches are not in
/// that key's range, and reading only that range would silently lose them.
#[test]
fn a_constant_under_an_or_does_not_become_an_access_path() {
    let mut node = Node::loaded();
    let plan = node
        .column("EXPLAIN SELECT * FROM s1 WHERE id = 1 OR n = 30")
        .join("\n");
    assert!(plan.contains("Seq Scan"), "{plan}");
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE id = 1 OR n = 30 ORDER BY id"),
        ["1", "3"]
    );
}

/// Whichever path the planner picks, the answer is the same. This is the property that makes the
/// pushdown rules safe to add to.
#[test]
fn every_access_path_returns_the_same_rows() {
    let mut node = Node::loaded();
    node.run("CREATE UNIQUE INDEX s1_t_key ON s1 (t)").unwrap();
    assert_eq!(
        node.rows("SELECT id, n, t FROM s1 WHERE id = 1"),
        [["1", "10", "a"]]
    );
    assert_eq!(
        node.rows("SELECT id, n, t FROM s1 WHERE t = 'a'"),
        [["1", "10", "a"]]
    );
    assert_eq!(
        node.rows("SELECT id, n, t FROM s1 WHERE n = 10"),
        [["1", "10", "a"]]
    );
    assert_eq!(
        node.rows("SELECT id, n, t FROM s1 WHERE id > 0 AND id < 2"),
        [["1", "10", "a"]]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE id = 99"),
        [] as [String; 0]
    );
    assert_eq!(
        node.column("SELECT id FROM s1 WHERE t = 'nope'"),
        [] as [String; 0]
    );
}

/// A narrowed range must not cut off a row that belongs in it. Every combination of bound is
/// checked against the same rows read with no narrowing at all.
#[test]
fn narrowing_a_range_never_loses_a_row() {
    let mut node = Node::loaded();
    for (predicate, expected) in [
        ("id > 2", vec!["3", "4"]),
        ("id >= 2", vec!["2", "3", "4"]),
        ("id < 3", vec!["1", "2"]),
        ("id <= 3", vec!["1", "2", "3"]),
        ("id > 1 AND id < 4", vec!["2", "3"]),
        ("id >= 1 AND id <= 4", vec!["1", "2", "3", "4"]),
        ("2 < id", vec!["3", "4"]),
        ("id > 4", vec![]),
        ("id > 0", vec!["1", "2", "3", "4"]),
    ] {
        let sql = format!("SELECT id FROM s1 WHERE {predicate} ORDER BY id");
        assert_eq!(node.column(&sql), expected, "{predicate}");
    }
}

/// An expression the executor does not evaluate is named, not guessed at.
///
/// **Arithmetic used to head this list and no longer does** — `+ - * / % ^` and `abs` answer now
/// (`tests/arithmetic.rs`), which is what statement 741 of the specific schema needed. What is
/// left here is the same contract for everything else: a construct this node does not run says so
/// by name, because a wrong number is worse than a refusal.
#[test]
fn an_expression_we_do_not_evaluate_is_refused_by_name() {
    let mut node = Node::loaded();
    for (sql, expected) in [
        // The five aggregates, `GROUP BY`, `HAVING` and `DISTINCT` run now (phase 9 unit 1);
        // what is next to them still does not, and each still names itself.
        ("SELECT count(*) OVER () FROM s1", "a window function"),
        ("SELECT count(*) FILTER (WHERE n > 0) FROM s1", "FILTER"),
        // `length` was the example here and is implemented now (the generated columns the suite
        // declares use it); the property under test is that an absent function names itself.
        ("SELECT soundex(t) FROM s1", "soundex"),
        ("SELECT DISTINCT ON (n) n FROM s1", "SELECT DISTINCT ON"),
        ("SELECT id FROM s1 GROUP BY ROLLUP (id)", "GROUP BY"),
        // A table alias runs now (phase 9 unit 5, `tests/alias.rs`). What it does not carry is the
        // **column** alias list, which renames the table's columns and cannot be ignored.
        ("SELECT c FROM s1 AS a (c)", "a column alias list"),
        // `INNER` and `LEFT` run now, with `ON` and with `USING` (phase 9 unit 4). The ones that
        // keep rows the *left* side does not have still do not: running a `RIGHT` as a `LEFT`
        // would answer with the same rows in the wrong places.
        ("SELECT * FROM s1 RIGHT JOIN s2 ON true", "RIGHT JOIN"),
        // `FULL JOIN` left this list when it landed: it runs now, and
        // `tests/full_outer_join.rs` is where its behaviour is pinned. `NATURAL JOIN` below it
        // is still refused, and for a different reason — it has no `ON` to read at all.
        ("SELECT * FROM s1 NATURAL JOIN s2", "NATURAL JOIN"),
        (
            "SELECT id FROM s1 JOIN s1 AS b USING (id) JOIN s1 AS c USING (id)",
            "USING in a chain of more than one JOIN",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{sql} -> {error}"
        );
        assert!(
            error.to_string().contains(expected),
            "{sql} -> `{error}`, which does not name `{expected}`"
        );
    }
}

/// Comparison is stricter than assignment, and the gap between them is a wrong answer if it is
/// missed. PostgreSQL stores `42` in a `text` column happily and refuses to *compare* the two:
/// there is no `text = integer` operator to call. Using the assignment rule for both would turn
/// that error into a silent `false`.
#[test]
fn a_comparison_between_types_with_no_operator_is_42883() {
    let mut node = Node::loaded();
    let error = node.run("SELECT t FROM s1 WHERE t = 1").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(error.to_string(), "operator does not exist: text = integer");
    assert_eq!(
        error.detail().as_deref(),
        Some("No operator of that name accepts the given argument types.")
    );
    assert_eq!(
        error.hint().as_deref(),
        Some("You might need to add explicit type casts.")
    );

    // The same statement as an INSERT is fine, which is the whole point.
    node.run("INSERT INTO s1 (id, t) VALUES (9, 42)").unwrap();
    assert_eq!(node.column("SELECT t FROM s1 WHERE id = 9"), ["42"]);

    // And the operands the other way round name the types the other way round.
    assert_eq!(
        node.run("SELECT t FROM s1 WHERE 1 = t")
            .unwrap_err()
            .to_string(),
        "operator does not exist: integer = text"
    );
}

/// A literal of the right category that still will not read keeps its input function's error,
/// which says what is actually wrong with it.
#[test]
fn an_unreadable_literal_keeps_its_own_error() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE ts (id int8 PRIMARY KEY, at timestamptz)")
        .unwrap();
    let error = node
        .run("SELECT id FROM ts WHERE at = 'not a date'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "22007");
}

/// `numeric` has no signed zero. A `-0.0` literal in a `double precision` column is `0`, not `-0`,
/// because the literal is a `numeric` on its way in — measured against a real server, which is the
/// only way this would ever have been noticed.
#[test]
fn a_negative_zero_literal_loses_its_sign_but_a_quoted_one_does_not() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE z (id int8 PRIMARY KEY, d double precision)")
        .unwrap();
    node.run("INSERT INTO z VALUES (1, -0.0), (2, '-0')")
        .unwrap();
    assert_eq!(node.column("SELECT d FROM z ORDER BY id"), ["0", "-0"]);
}

/// The join, and the choice `EXPLAIN` has to show: whether the inner side costs one key read per
/// outer row or a pass over the whole table. That is the difference a user changes their schema
/// over, so a plan that did not say which would be a plan they could not act on.
#[test]
fn explain_shows_how_the_inner_side_of_a_join_is_reached() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE c (id int8 PRIMARY KEY, email text UNIQUE, note text)")
        .unwrap();
    node.run("CREATE TABLE o (id int8 PRIMARY KEY, cid int8, tag text)")
        .unwrap();

    let plan = |node: &mut Node, sql: &str| node.column(sql).join("\n");

    // The inner table's primary key: one point read per outer row.
    let key = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM o JOIN c ON o.cid = c.id",
    );
    assert!(key.contains("Nested Loop"), "{key}");
    assert!(key.contains("Inner: Point Get on c"), "{key}");
    assert!(
        !key.contains("Join Filter"),
        "the probe answers the condition"
    );

    // Written the other way round, which is the same plan: the planner decides which side is
    // inner, not the order the user typed the operands in.
    let flipped = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM o JOIN c ON c.id = o.cid",
    );
    assert_eq!(flipped, key);

    // A unique index on the inner table, named so a user can see which one was used.
    let unique = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM o JOIN c ON o.tag = c.email",
    );
    assert!(
        unique.contains("Inner: Index Lookup on c using c_email_key"),
        "{unique}"
    );

    // Nothing usable: the inner table is read once and every pair is checked, and the condition
    // that could not become a probe is shown as the filter it became instead.
    let materialize = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM o JOIN c ON o.tag = c.note",
    );
    assert!(
        materialize.contains("Inner: Materialize on c"),
        "{materialize}"
    );
    assert!(materialize.contains("Join Filter"), "{materialize}");

    // A CROSS JOIN has no condition at all, so there is no filter to show either.
    let cross = plan(&mut node, "EXPLAIN SELECT o.id FROM o CROSS JOIN c");
    assert!(cross.contains("Inner: Materialize on c"), "{cross}");
    assert!(!cross.contains("Join Filter"), "{cross}");

    // Written with the probeable table *first*, the planner drives the loop from the other side
    // rather than materialising. An inner join is commutative, so this is free -- and without it
    // the same query costs a pass over the whole of `o` per row of `c`, which is a plan a user
    // would have had to know to avoid by typing the tables in the other order.
    let swapped = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM c JOIN o ON c.id = o.cid",
    );
    assert!(swapped.contains("Inner: Point Get on c"), "{swapped}");
    assert!(swapped.contains("Seq Scan on o"), "{swapped}");

    // The outer side is still planned: a `WHERE` that belongs to it narrows the scan under the
    // join rather than filtering above it.
    let outer = plan(
        &mut node,
        "EXPLAIN SELECT o.id FROM o JOIN c ON o.cid = c.id WHERE o.id = 3",
    );
    assert!(outer.contains("Point Get on o"), "{outer}");
}

/// Resolution across two tables, with the three answers PostgreSQL gives — each captured, because
/// they are three different sentences for what looks like one condition.
#[test]
fn a_column_reference_across_two_tables_resolves_the_way_postgresql_resolves_it() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE c (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("CREATE TABLE o (id int8 PRIMARY KEY, cid int8)")
        .unwrap();

    let error = node
        .run("SELECT id FROM o JOIN c ON o.cid = c.id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::AMBIGUOUS_COLUMN);
    assert_eq!(error.to_string(), "column reference \"id\" is ambiguous");

    let error = node
        .run("SELECT wrong.id FROM o JOIN c ON o.cid = c.id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        error.to_string(),
        "missing FROM-clause entry for table \"wrong\""
    );

    // Qualified and missing: dotted and unquoted, which is not how the bare form reads.
    let error = node
        .run("SELECT o.nosuch FROM o JOIN c ON o.cid = c.id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    assert_eq!(error.to_string(), "column o.nosuch does not exist");

    // A name only one of them has needs no qualifier.
    node.run("SELECT note, cid FROM o JOIN c ON o.cid = c.id")
        .unwrap();

    // And the qualifier is checked even with one table, which it used to be dropped for.
    let error = node.run("SELECT wrong.id FROM o").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}
