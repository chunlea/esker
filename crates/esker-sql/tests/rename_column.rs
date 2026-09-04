//! `ALTER TABLE … RENAME COLUMN` and its neighbour `RENAME TO` — 18 tests over 5 files, and 12
//! more over 3 for the table form.
//!
//! **A rename does not move the column.** Its ordinal is unchanged, so every index, constraint,
//! default and primary key goes on pointing at the same attribute — and everything that *renders*
//! a definition comes back with the new name because it renders from the ordinal. That is what
//! makes this a one-field write here as well: only `attname` is stored as text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **`CREATE VIEW` is a named refusal older than this unit**, so the three lines that need
        // a view are its consequences and not gaps of their own. The property the view is here to
        // show — that a rename is *rendered* through and does not break what points at the column
        // — is proved by the index and constraint definitions above it, which do the same job by
        // the same mechanism.
        (
            "CREATE VIEW rc_view AS SELECT id, name FROM rc",
            "`0A000 CREATE VIEW is not supported`. Every line below that names `rc_view` follows \
             from it.",
        ),
        (
            "SELECT 'r', pg_get_viewdef('rc_view'::regclass, true)",
            "The view was never created.",
        ),
        ("SELECT 'r', count(*) FROM rc_view", "The same."),
        ("DROP VIEW rc_view", "The same."),
    ],
};

#[test]
fn every_rename_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_rename_column.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The one thing this node refuses that a real server does not**, pinned so it is a decision
/// rather than a surprise.
///
/// A `CHECK`, an `EXCLUDE` key and a partial index's predicate are stored as the **text** the user
/// wrote and re-lowered on every write. PostgreSQL re-renders those from the attnum, so a rename
/// carries them across; here the text would go on naming a column that is gone, and the constraint
/// would stop resolving — a table that silently stops accepting rows, which is worse than a
/// refusal. So the rename is refused by name, and the message says which constraint is in the way.
///
/// The capture never reaches this because AR renames plain columns. It is written down because the
/// day those expressions are stored resolved rather than as text, this refusal should go.
#[test]
fn renaming_a_column_an_expression_names_is_refused() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE rx (id bigint PRIMARY KEY, qty bigint)")
        .unwrap();
    node.run("ALTER TABLE rx ADD CONSTRAINT rx_qty_check CHECK (qty >= 0)")
        .unwrap();

    let refused = node
        .answer("ALTER TABLE rx RENAME COLUMN qty TO amount")
        .to_string();
    assert!(
        refused.starts_with("!0A000") && refused.contains("rx_qty_check"),
        "the refusal must name the constraint in the way: {refused}"
    );
    // Refused means unchanged: the column is still there under its old name and still checked.
    assert!(
        node.answer("INSERT INTO rx VALUES (1, -1)")
            .to_string()
            .starts_with("!23514"),
        "the constraint still works"
    );

    // A column no expression mentions renames freely, even on the same table.
    node.run("ALTER TABLE rx RENAME COLUMN id TO pk").unwrap();
    node.run("INSERT INTO rx VALUES (1, 5)").unwrap();
    assert_eq!(node.rows("SELECT pk, qty FROM rx"), [["1", "5"]]);
}

/// **A rename keeps the rows**, which is the half a corpus of catalog reads does not prove: the
/// column's ordinal is what the row codec decodes at, and the rename does not touch it.
#[test]
fn the_rows_written_before_a_rename_read_after_it() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE rr (id bigint PRIMARY KEY, name text, qty bigint)")
        .unwrap();
    node.run("INSERT INTO rr VALUES (1, 'one', 10)").unwrap();
    node.run("ALTER TABLE rr RENAME COLUMN name TO title")
        .unwrap();
    node.run("INSERT INTO rr VALUES (2, 'two', 20)").unwrap();

    // Both rows, and the renamed column reads the value it was written with under the old name.
    assert_eq!(
        node.rows("SELECT id, title, qty FROM rr ORDER BY id"),
        [["1", "one", "10"], ["2", "two", "20"]]
    );
    // The old name is gone from every clause at once.
    assert!(
        node.answer("SELECT name FROM rr")
            .to_string()
            .starts_with("!42703")
    );
    // And renaming the table leaves the rows and the index names alone.
    node.run("CREATE INDEX index_rr_on_qty ON rr (qty)")
        .unwrap();
    node.run("ALTER TABLE rr RENAME TO rr2").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM rr2"), [["2"]]);
    assert_eq!(
        node.rows("SELECT indexname FROM pg_indexes WHERE tablename = 'rr2' ORDER BY indexname"),
        [["index_rr_on_qty"], ["rr_pkey"]]
    );
    // The old table name is free again, which it would not be if the name record had leaked.
    node.run("CREATE TABLE rr (id bigint PRIMARY KEY)").unwrap();
}
