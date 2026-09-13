//! **`INSERT … SELECT`**, against every answer PostgreSQL 19 gave for the statement's shapes
//! (`corpus/pg19_insert_select.txt`, captured by the Rails harness's `capture.sh insert_select` from
//! `captures-src/pg19_insert_select.sql`).
//!
//! The node refused the statement where it is lowered. The corpus is the statement's own ground: a
//! column list and none; an omitted column's default, a generated column and an identity;
//! `RETURNING`; no rows; a source query with `ORDER BY`, `LIMIT`, `OFFSET`, a derived table,
//! `UNION ALL`, `generate_series`, `WITH`, the parenthesised form Arel writes and a `VALUES` that is a
//! query; a source over the table the statement writes; `ON CONFLICT` with a query as its source; the
//! arity and type refusals; and row triggers, with s2's statement from `pg19_plpgsql_trigger.txt`
//! verbatim on s2's table shape.
//!
//! **A corpus holds no command tags** — `psql -q` prints none — so the second test compares them:
//! the corpus replayed in order on one node, and every `INSERT` that succeeded on PostgreSQL 19 told
//! the tag it sent there in the same session (`esker-coord/s1-unit-j/tags-2.out`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::session::Outcome;

const CORPUS: &str = include_str!("corpus/pg19_insert_select.txt");

/// The reason a `RETURNING` that returned no row is listed — `pg19_plpgsql_trigger.txt`'s shape.
const RETURNED_NO_ROW: &str = "the capture records a `RETURNING` that returned no row as a command: \
                               `\\gdesc` describes each statement on a connection of its own, \
                               where the capture's uncommitted tables do not exist, so it has no \
                               types to tell a result set of no rows from a command. PostgreSQL \
                               19 sent the result set — `psql` printed the column `id` and \
                               `(0 rows)` in the same session (`esker-coord/s1-unit-j/tags-2.out`) \
                               — and this node answers it: `integer`, and no row";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "INSERT INTO ti (id) SELECT 100 WHERE false RETURNING id",
            RETURNED_NO_ROW,
            "pg19_insert_select.txt:49",
        ),
        (
            "INSERT INTO ti (id, name) SELECT id, 'again' FROM s ORDER BY id ON CONFLICT (id) DO \
             NOTHING RETURNING id",
            RETURNED_NO_ROW,
            "pg19_insert_select.txt:72",
        ),
        (
            "INSERT INTO ti VALUES (1, 'v') ON CONFLICT (id) DO NOTHING RETURNING id",
            RETURNED_NO_ROW,
            "pg19_insert_select.txt:77",
        ),
        (
            "INSERT INTO ti (id, name) SELECT 1, 'x' UNION ALL SELECT 1, 'y' ON CONFLICT (id) DO UPDATE \
         SET name = EXCLUDED.name",
            "sqlparser 0.62.0 does not read this statement — it stops at `CONFLICT` (`Expected: end of \
         statement, found: CONFLICT`) — so this node answers 42601 before the statement reaches \
         the planner. The same set operation parenthesised, four lines down, is read and answers \
         PostgreSQL 19's 21000: the rule this row measures is built, and the statement is the \
         parser's",
            "pg19_insert_select.txt:79",
        ),
    ],
};

#[test]
fn every_insert_select_answer_is_postgresql_19_s() {
    let checked = parity::replay(CORPUS, &[], &DIVERGENCES);
    assert!(
        checked > 90,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The tag PostgreSQL 19 sent for each `INSERT` of the corpus that succeeded, in the corpus's order.
const TAGS: &[(&str, &str)] = &[
    (
        "INSERT INTO s VALUES (1, 'one'), (2, 'two'), (3, 'three')",
        "INSERT 0 3",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id, name FROM s WHERE id = 1",
        "INSERT 0 1",
    ),
    ("INSERT INTO ti SELECT 2, 'two'", "INSERT 0 1"),
    ("INSERT INTO ti (id) SELECT 3", "INSERT 0 1"),
    (
        "INSERT INTO ident (v) SELECT 'a' UNION ALL SELECT 'b' RETURNING id, v",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id + 10, upper(name) FROM s ORDER BY id RETURNING id, name, \
         tenfold",
        "INSERT 0 3",
    ),
    (
        "INSERT INTO ti (id) SELECT id FROM s WHERE false",
        "INSERT 0 0",
    ),
    (
        "INSERT INTO ti (id) SELECT 100 WHERE false RETURNING id",
        "INSERT 0 0",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id + 20, name FROM s ORDER BY id DESC LIMIT 2 RETURNING id, \
         name",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id + 30, name FROM s ORDER BY id OFFSET 1 RETURNING id",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO ti (id, name) SELECT x.id + 40, x.name FROM (SELECT * FROM s WHERE id > 1) x \
         ORDER BY x.id RETURNING id",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO ti (id, name) SELECT 51, 'u' UNION ALL SELECT 52, 'v' RETURNING id, name",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO ti (id) SELECT n FROM generate_series(60, 62) n RETURNING id",
        "INSERT 0 3",
    ),
    (
        "INSERT INTO ti (id, name) WITH w AS (SELECT 70 AS id, 'w' AS name) SELECT * FROM w \
         RETURNING id, name",
        "INSERT 0 1",
    ),
    (
        "INSERT INTO ti (id, name) (SELECT 71, 'paren') RETURNING id, name",
        "INSERT 0 1",
    ),
    (
        "INSERT INTO \"ti\" (\"id\", \"name\") (SELECT 72, 'arel') RETURNING id, name",
        "INSERT 0 1",
    ),
    (
        "INSERT INTO ti (id) VALUES (73), (74) LIMIT 1 RETURNING id",
        "INSERT 0 1",
    ),
    ("INSERT INTO r VALUES (1)", "INSERT 0 1"),
    ("INSERT INTO r SELECT n + 1 FROM r", "INSERT 0 1"),
    ("INSERT INTO r SELECT n + 10 FROM r", "INSERT 0 2"),
    (
        "INSERT INTO r SELECT n + 100 FROM r WHERE n < 1000",
        "INSERT 0 4",
    ),
    (
        "INSERT INTO r SELECT n FROM r ORDER BY n RETURNING n",
        "INSERT 0 8",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id, 'again' FROM s ORDER BY id ON CONFLICT (id) DO NOTHING \
         RETURNING id",
        "INSERT 0 0",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id + 80, name FROM s ORDER BY id ON CONFLICT DO NOTHING \
         RETURNING id",
        "INSERT 0 3",
    ),
    (
        "INSERT INTO ti (id, name) SELECT id, name || '!' FROM s ORDER BY id ON CONFLICT (id) DO \
         UPDATE SET name = EXCLUDED.name RETURNING id, name",
        "INSERT 0 3",
    ),
    (
        "INSERT INTO ti VALUES (1, 'v'), (200, 'v') ON CONFLICT (id) DO NOTHING",
        "INSERT 0 1",
    ),
    (
        "INSERT INTO ti VALUES (1, 'v') ON CONFLICT (id) DO NOTHING RETURNING id",
        "INSERT 0 0",
    ),
    (
        "INSERT INTO ti (id, name) SELECT 90, 91 RETURNING id, name",
        "INSERT 0 1",
    ),
    ("INSERT INTO ti (id) SELECT '92' RETURNING id", "INSERT 0 1"),
    (
        "INSERT INTO ti (id) SELECT 95::bigint RETURNING id",
        "INSERT 0 1",
    ),
    (
        "INSERT INTO t SELECT n, 'sel' FROM generate_series(20, 21) n RETURNING id, name",
        "INSERT 0 2",
    ),
    (
        "INSERT INTO t SELECT n, 'k' FROM generate_series(30, 33) n RETURNING id, name",
        "INSERT 0 2",
    ),
];

/// The corpus replayed in order, with every statement [`TAGS`] names told the tag PostgreSQL 19 sent.
///
/// **Only the statements the list names are asked**, and each of those must succeed: a refusal of
/// one PostgreSQL 19 accepted is reported here with its own error rather than skipped, and a list
/// that runs out of step with the corpus fails rather than comparing the wrong pair.
#[test]
fn every_insert_select_tag_is_postgresql_19_s() {
    let mut node = parity::Node::new(&[]);
    let mut expected = TAGS.iter().peekable();
    for line in CORPUS.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let statement = line.split('\t').next().expect("a statement field");
        let answer = node.run(statement);
        let Some((named, tag)) = expected.peek() else {
            continue;
        };
        if statement != *named {
            continue;
        }
        expected.next();
        match &answer {
            Ok(Outcome::Done { tag: got } | Outcome::Rows { tag: got, .. }) => {
                assert_eq!(got, tag, "{statement}");
            }
            Err(error) => {
                panic!("{statement}: PostgreSQL 19 said {tag}, this node refused: {error}")
            }
        }
    }
    assert!(
        expected.next().is_none(),
        "the tag list names a statement the corpus never reached"
    );
}
