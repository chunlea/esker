//! `CREATE UNLOGGED TABLE` — the top row of run 45's failure ranking.
//!
//! **198 tests across 24 files, and one setting rather than twenty-four needs.**
//! `activerecord/test/cases/helper.rb` sets `create_unlogged_tables = true` unconditionally, so
//! every `create_table` the suite issues is `UNLOGGED`. This is not a feature test; it is how
//! `ActiveRecord`'s suite creates every table it makes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `relpersistence` is a `"char"` on a real server and `text` here, holding the same single
    // character — the trade every `pg_catalog` column makes.
    types: &[
        "SELECT 'r', relname, relpersistence, relkind FROM pg_class WHERE relname IN \
         ('unlogged_probe','unlogged_noid','logged_probe') ORDER BY relname",
        "SELECT 'r', relname, relpersistence FROM pg_class WHERE relname IN \
         ('unlogged_probe_pkey','logged_probe_pkey','unlogged_probe_id_seq','logged_probe_id_seq') \
         ORDER BY relname",
        "SELECT 'r', relname, relpersistence FROM pg_class WHERE relname = 'unlogged_probe'",
        "SELECT 'r', relname, relpersistence FROM pg_class WHERE relname = 'temp_probe'",
        "SELECT 'r', relname, relpersistence FROM pg_class WHERE relname = 'fk_unlogged_to_unlogged'",
        // `information_schema`'s own domains again: `name` and `character varying` where this node
        // says `text`, with identical characters.
    ],
    answers: &[
        // **A temporary table is a different feature, not a kind of unlogged one**, and this file
        // is where the capture proves the three `relpersistence` values are three things. Both
        // `CREATE TEMP TABLE` and the row that reads it back agree outright since ADR 0054 — the
        // two entries that stood here are gone, deleted because the harness failed this test when
        // they started agreeing.
        (
            "ALTER TABLE \"temp_probe\" SET LOGGED",
            "A real server refuses this `42P16 cannot change logged status of table \"temp_probe\" \
             because it is temporary`; here the table does not exist, so the answer is `42P01`. \
             The rule cannot be reached without temporary tables, and it is about them rather than \
             about persistence.",
            "pg19_unlogged_table.txt:91",
        ),
        (
            "SELECT 'r', pg_typeof(relpersistence) FROM pg_class WHERE relname = 'logged_probe'",
            "`\"char\"` there and `text` here — the statement that would *prove* the declared-type \
             divergence above, and it diverges in the same direction.",
            "pg19_unlogged_table.txt:93",
        ),
    ],
};

#[test]
fn every_unlogged_table_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_unlogged_table.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The keyword changes one column and nothing else a client can read.**
///
/// `relpersistence` is `u` where a logged table is `p`, and `information_schema.tables` calls both
/// `BASE TABLE` — so a client reading the information schema cannot tell them apart at all. That
/// asymmetry is why this unit is `pg_class`'s column and not a new kind of relation.
#[test]
fn an_unlogged_table_differs_from_a_logged_one_in_one_column() {
    let mut node = parity::Node::new(&[
        "CREATE UNLOGGED TABLE unlogged_probe (id bigserial primary key, name varchar, n integer)",
        "CREATE TABLE logged_probe (id bigserial primary key, name varchar)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT relname, relpersistence, relkind FROM pg_class WHERE relname IN \
             ('unlogged_probe','logged_probe') ORDER BY relname"
        ),
        vec![
            vec!["logged_probe", "p", "r"],
            vec!["unlogged_probe", "u", "r"],
        ]
    );
    assert_eq!(
        node.rows(
            "SELECT table_name, table_type FROM information_schema.tables WHERE table_name IN \
             ('unlogged_probe','logged_probe') ORDER BY table_name"
        ),
        vec![
            vec!["logged_probe", "BASE TABLE"],
            vec!["unlogged_probe", "BASE TABLE"],
        ],
        "the information schema cannot see persistence, so both are BASE TABLE"
    );
    // And it is an ordinary table in every other respect.
    node.run("INSERT INTO unlogged_probe (name, n) VALUES ('a', 1)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, name, n FROM unlogged_probe"),
        [["1", "a", "1"]]
    );
}

/// **The persistence is inherited by everything the table owns**, which is four relations out of
/// five.
///
/// A `bigserial`'s sequence and every index including the primary key come out `u` as well. An
/// implementation that stores the flag on the table alone answers this wrongly for all of them,
/// and nothing in the table's own row would show it.
#[test]
fn what_the_table_owns_is_unlogged_too() {
    let mut node = parity::Node::new(&[
        "CREATE UNLOGGED TABLE unlogged_probe (id bigserial primary key, n integer)",
        "CREATE TABLE logged_probe (id bigserial primary key)",
        "CREATE INDEX unlogged_probe_n_idx ON unlogged_probe (n)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT relname, relpersistence FROM pg_class WHERE relname IN \
             ('unlogged_probe_pkey','logged_probe_pkey','unlogged_probe_id_seq',\
             'logged_probe_id_seq','unlogged_probe_n_idx') ORDER BY relname"
        ),
        vec![
            vec!["logged_probe_id_seq", "p"],
            vec!["logged_probe_pkey", "p"],
            vec!["unlogged_probe_id_seq", "u"],
            vec!["unlogged_probe_n_idx", "u"],
            vec!["unlogged_probe_pkey", "u"],
        ]
    );
}

/// **`SET LOGGED` flips the indexes in the same statement**, and `SET UNLOGGED` puts them back.
#[test]
fn set_logged_carries_the_indexes_with_it() {
    let mut node = parity::Node::new(&[
        "CREATE UNLOGGED TABLE unlogged_probe (id bigserial primary key, n integer)",
        "CREATE INDEX unlogged_probe_n_idx ON unlogged_probe (n)",
    ]);
    let persistence = |node: &mut parity::Node| {
        node.rows(
            "SELECT relname, relpersistence FROM pg_class WHERE relname LIKE 'unlogged_probe%' \
             ORDER BY relname",
        )
    };
    node.run("ALTER TABLE unlogged_probe SET LOGGED").unwrap();
    assert_eq!(
        persistence(&mut node),
        vec![
            vec!["unlogged_probe", "p"],
            vec!["unlogged_probe_id_seq", "p"],
            vec!["unlogged_probe_n_idx", "p"],
            vec!["unlogged_probe_pkey", "p"],
        ]
    );
    node.run("ALTER TABLE unlogged_probe SET UNLOGGED").unwrap();
    assert_eq!(
        persistence(&mut node),
        vec![
            vec!["unlogged_probe", "u"],
            vec!["unlogged_probe_id_seq", "u"],
            vec!["unlogged_probe_n_idx", "u"],
            vec!["unlogged_probe_pkey", "u"],
        ]
    );
}

/// **The foreign-key rule is one-directional**, which is the half a symmetric implementation gets
/// wrong.
///
/// A permanent table may not reference an unlogged one — its constraint would outlive the data it
/// points at. The reverse is fine: an unlogged table referencing a logged one is accepted, because
/// losing the child on a crash breaks nothing about the parent.
#[test]
fn only_a_permanent_table_referencing_an_unlogged_one_is_refused() {
    let mut node = parity::Node::new(&[
        "CREATE UNLOGGED TABLE unlogged_probe (id bigserial primary key)",
        "CREATE TABLE logged_probe (id bigserial primary key)",
    ]);
    let error = node
        .run("CREATE TABLE fk_to_unlogged (id bigserial primary key, u bigint REFERENCES unlogged_probe (id))")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P16");
    assert_eq!(
        error.to_string(),
        "constraints on permanent tables may reference only permanent tables"
    );
    // Both directions that are legal, and they are the majority of the rule.
    node.run("CREATE UNLOGGED TABLE fk_unlogged_to_unlogged (id bigserial primary key, u bigint REFERENCES unlogged_probe (id))")
        .unwrap();
    node.run("CREATE UNLOGGED TABLE unlogged_fk_to_logged (id bigserial primary key, l bigint REFERENCES logged_probe (id))")
        .unwrap();
}

/// `IF NOT EXISTS` on an existing table is a **plain success**, no error and no notice.
#[test]
fn if_not_exists_is_a_plain_success() {
    let mut node =
        parity::Node::new(&["CREATE UNLOGGED TABLE unlogged_probe (id bigserial primary key)"]);
    node.run("CREATE UNLOGGED TABLE IF NOT EXISTS unlogged_probe (id bigserial primary key)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT relpersistence FROM pg_class WHERE relname = 'unlogged_probe'"),
        [["u"]]
    );
}

/// **A view has no storage, so it cannot be unlogged** — `42601`, a syntax-class error rather than
/// the `0A000` an unimplemented feature gets.
#[test]
fn an_unlogged_view_is_refused_as_a_syntax_error() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("CREATE UNLOGGED VIEW unlogged_view AS SELECT 1")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42601");
    assert_eq!(
        error.to_string(),
        "views cannot be unlogged because they do not have storage"
    );
}
