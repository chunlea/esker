//! `pg_extension` and `pg_inherits` — boot statements 23 and 30.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The table boot statement 30's join looks for, so the empty answer is empty for the right
/// reason: the relation is there and nothing inherits it.
const CORPUS_FIXTURE: &[&str] = &["CREATE TABLE ak (id int8 PRIMARY KEY)"];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **A relation's oid is a `bigint` here** (ADR 0097): `inhrelid` and `inhparent` name
    // relations, and a catalog view's own id comes from `VIEW_ID_BASE` near `i64::MAX` while a
    // primary key's index oid comes from `PRIMARY_KEY_OID_BASE` — neither fits four bytes. Every
    // other oid in the catalog is declared one. The rows agree: all three are empty.
    types: &["SELECT inhrelid, inhparent FROM pg_inherits"],
    // **Two, and `plpgsql` is no longer one of them.** This file used to declare `pg_extension`
    // empty — a row would have claimed `CREATE FUNCTION … LANGUAGE plpgsql` works — and the
    // `CREATE EXTENSION` unit made the claim true in the only sense that matters here: `plpgsql`
    // is installed, both extension views say so, and `count(*)` agrees with a real server. That
    // entry was deleted rather than kept, which is ADR 0031's rule 2.
    //
    // What is left are the two rows a real server's own `pg_extension` has because the *oracle*
    // installed them for `postgresql_specific_schema.rb`, and this node has not: they are
    // available here and not installed until a `CREATE EXTENSION` says so, which is the state a
    // fresh database is in on both.
    //
    // `pg_inherits` agrees on every line, because nothing inherits on either side.
    answers: &[(
        "SELECT extname, extnamespace FROM pg_extension",
        "The same row, read directly.",
        "UNMEASURED",
    )],
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

/// A relation that is **there**, not a relation that is missing.
///
/// The difference is the whole unit: `42P01` makes `ActiveRecord` raise, and no rows makes it
/// carry on with an empty list. Both views are read on every boot, and neither answer depends on
/// anything this node has.
#[test]
fn both_views_resolve_and_answer() {
    let mut node = parity::Node::new(&[]);
    for view in ["pg_extension", "pg_inherits"] {
        // `pg_inherits` holds nothing; `pg_extension` holds `plpgsql`, which every PostgreSQL
        // database has installed before anything runs.
        assert_eq!(
            node.rows(&format!("SELECT count(*) FROM {view}")),
            [[if view == "pg_extension" { "1" } else { "0" }]],
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
