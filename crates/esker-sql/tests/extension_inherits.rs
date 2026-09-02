//! `pg_extension` and `pg_inherits` — boot statements 23 and 30.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The table boot statement 30's join looks for, so the empty answer is empty for the right
/// reason: the relation is there and nothing inherits it.
const CORPUS_FIXTURE: &[&str] = &["CREATE TABLE ak (id int8 PRIMARY KEY)"];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade: an `oid` is a `bigint` here for the reason `pg_class.oid`
    // is one, and a `name` is `text`. The rows agree — all three are empty.
    types: &[
        "SELECT inhrelid, inhparent FROM pg_inherits",
        "SELECT parent.relname FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class child ON i.inhrelid = child.oid JOIN pg_catalog.pg_class parent ON i.inhparent = parent.oid LEFT JOIN pg_namespace n ON n.oid = child.relnamespace WHERE child.relname = 'ak' AND child.relkind IN ('r','p') AND n.nspname = ANY (current_schemas(false))",
        "SELECT extname FROM pg_extension WHERE extname = 'nope'",
    ],
    // **Three, and all three are `plpgsql`.** Every PostgreSQL database has it installed, so a
    // real server's `pg_extension` holds one row and this node's holds none. That is a declared
    // divergence and not a gap: a row here would tell a client
    // `CREATE FUNCTION … LANGUAGE plpgsql` will work, and this node has no procedural language and
    // no `CREATE EXTENSION` — the same argument that keeps `pg_collation` and `pg_range` empty.
    // `ActiveRecord` turns the answer into `enable_extension` lines in a schema dump, and none is
    // the truth here.
    //
    // `pg_inherits` agrees on every line, because nothing inherits on either side.
    answers: &[
        (
            "SELECT pg_extension.extname, n.nspname AS schema FROM pg_extension JOIN pg_namespace n ON pg_extension.extnamespace = n.oid",
            "Boot statement 23. One row on a real server — `plpgsql` in `pg_catalog` — and none \
             here, because this node has no extensions to name.",
        ),
        (
            "SELECT extname, extnamespace FROM pg_extension",
            "The same row, read directly.",
        ),
        (
            "SELECT count(*) FROM pg_extension",
            "1 against 0, which is the same fact counted.",
        ),
    ],
};

#[test]
fn every_extension_and_inherits_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_extension_inherits.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 7,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A relation that is there and empty, not a relation that is missing.
///
/// The difference is the whole unit: `42P01` makes `ActiveRecord` raise, and no rows makes it
/// carry on with an empty list. Both views are read on every boot, and neither answer depends on
/// anything this node has.
#[test]
fn both_views_resolve_and_hold_nothing() {
    let mut node = parity::Node::new(&[]);
    for view in ["pg_extension", "pg_inherits"] {
        assert_eq!(
            node.rows(&format!("SELECT count(*) FROM {view}")),
            [["0"]],
            "for {view}"
        );
        // A stub that answered every column would hide the day one of them matters.
        let error = node
            .run(&format!("SELECT nosuchcol FROM {view}"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), "42703", "for {view}");
    }
}

/// The write refusal every `pg_catalog` relation gets, which a newly added one has to get too.
#[test]
fn neither_view_may_be_written() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "DROP TABLE pg_extension",
        "DROP TABLE pg_inherits",
        "CREATE INDEX px ON pg_extension (extname)",
    ] {
        let error = node.run(statement).unwrap_err();
        assert!(
            matches!(error.sqlstate(), "42501" | "42809"),
            "{statement} answered {}",
            error.sqlstate()
        );
    }
}
