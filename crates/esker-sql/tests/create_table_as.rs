//! **`CREATE TABLE … AS SELECT` — a table whose shape is the query's.**
//!
//! `MigrationTest#test_create_table_with_query` and `#test_create_table_with_query_from_relation`
//! both send the bare form, which is all `ActiveRecord`'s generator can produce for `as:`:
//!
//! ```text
//! CREATE TABLE "table_from_query_testings" AS SELECT id FROM people WHERE id = 1
//! ```
//!
//! No `IF NOT EXISTS` (neither test sets it), no `WITH DATA` / `WITH NO DATA` (the string appears
//! nowhere in `activerecord`), and no parenthesised column list — `as:` with no block produces no
//! column statements, so `visit_TableDefinition` emits none.
//!
//! # The primitive already exists
//!
//! `exec::ddl::create_materialized_view` does these four steps: `planned_rows` plans **and** runs
//! the query, `matview_columns` types the new relation **from the plan** — which its own comment
//! insists on, because a relation typed from anywhere but the query that fills it can disagree
//! with its own rows — then `create_table`, then the fill. ADR 0064 already committed this node to
//! "a materialized view is a table whose rows are recomputed", so this is that path without the
//! matview marker.
//!
//! # What the oracle says, measured on 19beta1
//!
//! ```text
//! CREATE TABLE q1 AS SELECT id FROM people WHERE id = 1
//!     id       bigint              attnotnull f   atthasdef f
//!
//! CREATE TABLE q2 AS SELECT id + 1 AS n, name, age * 2 AS doubled, 'lit' AS s FROM people
//!     n        bigint          -- int8 + int4
//!     name     character varying
//!     doubled  integer         -- int4 * int4
//!     s        text            -- an unknown literal resolves to text
//! ```
//!
//! **The source's `NOT NULL`, its default, its primary key and its indexes are all dropped** —
//! `pg_constraint` and `pg_index` are empty for the new table, and `id` is nullable there although
//! it is a `bigserial primary key` in the source. A table from a query keeps the query's *types*
//! and nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1_people (id bigserial primary key, name character varying, age integer)",
    "INSERT INTO g1_people (id, name, age) VALUES (1, 'a', 30), (2, 'b', 40)",
];

fn columns(node: &mut parity::Node, table: &str) -> Vec<Vec<String>> {
    node.rows(&format!(
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, a.atthasdef \
         FROM pg_attribute a WHERE a.attrelid = '{table}'::regclass AND a.attnum > 0 \
         ORDER BY a.attnum"
    ))
}

/// The corpus statement, and the two things the Rails test then asserts: one column called `id`,
/// and `SELECT *` returning the one row the `WHERE` kept.
#[test]
fn the_statement_activerecord_sends() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE g1_q1 AS SELECT id FROM g1_people WHERE id = 1")
        .unwrap();
    assert_eq!(
        columns(&mut node, "g1_q1"),
        [[
            "id".to_owned(),
            "bigint".to_owned(),
            "f".to_owned(),
            "f".to_owned(),
        ]],
        "one column, the query's type, and nullable — the source's NOT NULL does not come with it"
    );
    assert_eq!(node.rows("SELECT * FROM g1_q1"), [["1".to_owned()]]);
}

/// **The expression columns, which are what prove the plan typed them.** A bare column could pass
/// by inheriting the source column's type without the typing path being exercised at all.
#[test]
fn an_expression_column_is_typed_by_the_plan() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "CREATE TABLE g1_q2 AS SELECT id + 1 AS n, name, age * 2 AS doubled, 'lit' AS s \
         FROM g1_people",
    )
    .unwrap();
    assert_eq!(
        columns(&mut node, "g1_q2"),
        [
            // `int8 + int4` widens; `int4 * int4` does not; an unknown literal is text.
            [
                "n".to_owned(),
                "bigint".to_owned(),
                "f".to_owned(),
                "f".to_owned()
            ],
            [
                "name".to_owned(),
                "character varying".to_owned(),
                "f".to_owned(),
                "f".to_owned()
            ],
            [
                "doubled".to_owned(),
                "integer".to_owned(),
                "f".to_owned(),
                "f".to_owned()
            ],
            [
                "s".to_owned(),
                "text".to_owned(),
                "f".to_owned(),
                "f".to_owned()
            ],
        ]
    );
    assert_eq!(
        node.rows("SELECT n, name, doubled, s FROM g1_q2 ORDER BY n"),
        [
            [
                "2".to_owned(),
                "a".to_owned(),
                "60".to_owned(),
                "lit".to_owned()
            ],
            [
                "3".to_owned(),
                "b".to_owned(),
                "80".to_owned(),
                "lit".to_owned()
            ],
        ]
    );
}

/// **An ordinary table, and nothing came across with the rows.** Measured: `pg_constraint` and
/// `pg_index` are both empty for the new relation, although the source column is a
/// `bigserial primary key`.
#[test]
fn the_key_and_the_indexes_are_not_copied() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE g1_q1 AS SELECT id FROM g1_people WHERE id = 1")
        .unwrap();
    assert_eq!(
        node.rows("SELECT relkind FROM pg_class WHERE relname = 'g1_q1'"),
        [["r".to_owned()]],
        "an ordinary table, not a view and not a matview"
    );
    assert_eq!(
        node.rows("SELECT conname FROM pg_constraint WHERE conrelid = 'g1_q1'::regclass"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(
        node.rows(
            "SELECT indexrelid::regclass::text FROM pg_index WHERE indrelid = 'g1_q1'::regclass"
        ),
        Vec::<Vec<String>>::new()
    );
}

/// It is a table, so it is writable — which is the difference from a view and the reason
/// `relkind` above is asserted rather than assumed.
#[test]
fn the_result_is_an_ordinary_writable_table() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE g1_q1 AS SELECT id FROM g1_people WHERE id = 1")
        .unwrap();
    node.run("INSERT INTO g1_q1 (id) VALUES (99)").unwrap();
    node.run("UPDATE g1_q1 SET id = 100 WHERE id = 99").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM g1_q1 ORDER BY id"),
        [["1".to_owned()], ["100".to_owned()]]
    );
    // And it does not track the source: a later insert into `g1_people` does not appear.
    node.run("INSERT INTO g1_people (id, name, age) VALUES (3, 'c', 50)")
        .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1_q1"), [["2".to_owned()]]);
}

/// The name competes in the relation namespace like any table's.
#[test]
fn a_second_one_of_the_same_name_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE g1_q1 AS SELECT id FROM g1_people")
        .unwrap();
    assert_eq!(
        node.answer("CREATE TABLE g1_q1 AS SELECT id FROM g1_people")
            .to_string(),
        "!42P07 relation \"g1_q1\" already exists"
    );
}

/// **The command tag, which the parity harness cannot see.**
///
/// It discards the tag on purpose — `psql` prints a result set where it would have printed one, so
/// the container cannot be asked what tag it sent. This one is worth an assertion anyway, because
/// PostgreSQL's answer is the surprising one: `CREATE TABLE … AS` reports **`SELECT <n>`**, the
/// count of rows it wrote, exactly as `INSERT … SELECT` does. A client counting rows off the tag
/// would learn nothing from a `CREATE` tag.
mod tag {
    use std::sync::Arc;

    use esker_sql::backend::{Backend, MemoryBackend};
    use esker_sql::catalog::Catalog;
    use esker_sql::exec::Executor;
    use esker_sql::parse::parse_statements;
    use esker_sql::pgwire::session::{Execute, Outcome, Params};

    /// The smallest node that answers with an `Outcome`, which is all this needs.
    fn tag_of(statements: &[&str]) -> String {
        let mut executor = Executor::new(
            Arc::new(MemoryBackend::new()) as Arc<dyn Backend>,
            Arc::new(Catalog::new()),
            1,
            esker_sql::session::register(),
        );
        let mut last = String::new();
        for sql in statements {
            for parsed in parse_statements(sql).unwrap() {
                match executor.execute(&parsed, &Params::NONE).unwrap() {
                    Outcome::Done { tag } | Outcome::Rows { tag, .. } => last = tag,
                }
            }
        }
        last
    }

    #[test]
    fn the_tag_is_select_and_the_row_count() {
        assert_eq!(
            tag_of(&[
                "CREATE TABLE p (id bigint)",
                "INSERT INTO p VALUES (1), (2), (3)",
                "CREATE TABLE one AS SELECT id FROM p WHERE id = 1",
            ]),
            "SELECT 1",
            "the corpus statement writes one row"
        );
        assert_eq!(
            tag_of(&[
                "CREATE TABLE p (id bigint)",
                "INSERT INTO p VALUES (1), (2), (3)",
                "CREATE TABLE all_of_them AS SELECT id FROM p",
            ]),
            "SELECT 3"
        );
        assert_eq!(
            tag_of(&[
                "CREATE TABLE p (id bigint)",
                "CREATE TABLE none AS SELECT id FROM p WHERE false",
            ]),
            "SELECT 0",
            "an empty query still makes the table"
        );
    }
}
