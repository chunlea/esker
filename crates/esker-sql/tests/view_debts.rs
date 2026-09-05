//! The dependency edge the `CREATE VIEW` unit left behind.
//!
//! A view is a **dependency** of what it reads, not a copy of it. Without that edge the base could
//! be dropped and the view left naming a relation that is gone — the same shape as a name record
//! outliving its object, reached from an ordinary `DROP TABLE`.
//!
//! `tests/drop_column.rs` carried the other half as two divergence entries and named this unit:
//! *"that edge is the follow-on unit, and this capture already says what both answers must
//! become."* Both are deleted now.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables and views.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // `UNION` in a view body is refused by name — a query feature, not a view one, and the same
    // `0A000` a bare `SELECT … UNION …` gets. The two lines that read that view are swallowed with
    // it; everything the dependency edge is about is measured on the other nine views.
    answers: &[(
        "CREATE VIEW v_union AS SELECT id, a FROM vb UNION SELECT id, c FROM vb2;",
        "`0A000 UNION is not supported`: a set operation is a query feature this node does not \
         have, so the view cannot be created. Nothing about the dependency edge differs.",
        "pg19_view_debts.txt:40",
    )],
};

#[test]
fn every_view_dependency_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_view_debts.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A cascade that stopped at the first level would reintroduce what the edge prevents.**
///
/// A view over a view is a dependency too, so `DROP TABLE … CASCADE` has to walk the chain —
/// depth first, so a view goes only after everything that reads it. Dropping one view and leaving
/// the one built on it would leave exactly the dangling reference the refusal exists to stop, this
/// time created by the statement that was meant to clean up.
#[test]
fn a_cascade_walks_the_whole_chain() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE vb (id bigint PRIMARY KEY, a int)",
        "CREATE VIEW v_inner AS SELECT id, a FROM vb",
        "CREATE VIEW v_outer AS SELECT id FROM v_inner",
    ]);

    // Each level refuses on its own, and the DETAIL names the pair.
    let error = node.run("DROP VIEW v_inner").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.detail().as_deref(),
        Some("view v_outer depends on view v_inner")
    );
    let error = node.run("DROP TABLE vb").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.detail().as_deref(),
        Some("view v_inner depends on table vb")
    );

    // And the cascade takes both, leaving nothing behind.
    node.run("DROP TABLE vb CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_views WHERE schemaname = 'public'"),
        [["0"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'vb'"),
        [["0"]]
    );
}

/// **`DROP COLUMN` names the column, not the table** — so a view that reads the table but not the
/// column does not stop it.
#[test]
fn a_column_a_view_does_not_read_is_droppable() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dc (id bigint PRIMARY KEY, shown int, hidden int)",
        "CREATE VIEW dc_view AS SELECT id, shown FROM dc",
    ]);
    // `hidden` is not in the view: the drop goes through.
    node.run("ALTER TABLE dc DROP COLUMN hidden").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_views WHERE viewname = 'dc_view'"),
        [["1"]]
    );

    // `shown` is, and the message says which column of which table.
    let error = node.run("ALTER TABLE dc DROP COLUMN shown").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.detail().as_deref(),
        Some("view dc_view depends on column shown of table dc")
    );

    // `CASCADE` takes the view, and the column goes with it.
    node.run("ALTER TABLE dc DROP COLUMN shown CASCADE")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_views WHERE viewname = 'dc_view'"),
        [["0"]]
    );
}
