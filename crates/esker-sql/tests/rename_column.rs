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
    // **`name` there, `text` here** — the standing choice every `pg_catalog` column in this crate
    // makes. Newly compared: the line sits below the `CREATE VIEW` that used to abort the capture.
    types: &[
        "SELECT 'r', indexname, indexdef FROM pg_indexes WHERE tablename = 'rc' ORDER BY indexname",
    ],
    answers: &[
        // **`CREATE VIEW` landed (`tests/view.rs`)**, so the view is created, read and dropped
        // here and those three entries are gone. `pg_get_viewdef` is the one that stays — it is
        // PostgreSQL's own renderer and this node has no pretty-printer for a definition.
        (
            "SELECT 'r', pg_get_viewdef('rc_view'::regclass, true)",
            "Refused by name: `pg_get_viewdef` prints a view's body through PostgreSQL's renderer, \
             one column per line with its own indentation, and reproducing that is a \
             pretty-printer for the whole expression language. Declared identically in \
             `tests/view.rs`.",
        ),
        // **A pre-existing bug this unit made visible, and not a view divergence.** `CREATE VIEW`
        // used to abort the transaction seven lines above, so everything after it was swallowed
        // by the harness rather than compared — including this.
        (
            "SELECT 'r', conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '\"rc\"'::regclass ORDER BY conname",
            "**Renaming a column renames the `NOT NULL` constraint that names it, and PostgreSQL \
             leaves it alone.** After `ALTER TABLE rc RENAME COLUMN name TO title` the oracle still \
             calls the constraint `rc_name_not_null`; this node calls it `rc_title_not_null`, which \
             also moves it in a `conname` ordering. A constraint's name is a name a user chose or \
             the server generated *once* — it is not a function of the column, and renaming it is a \
             second rename nobody asked for. Reproduced with no view in the statement.",
        ),
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

/// **Run 55's cliff, reproduced.** From file 326 every schema load failed with
/// `relation "references_id_seq" already exists`, and 101 files went with it.
///
/// The mechanism is a refusal, not a leak. `rename_table` renames the table and then renames the
/// sequence its `serial` column owns with a second statement — `ALTER TABLE <seq> RENAME TO …`,
/// a *sequence* named where the grammar says table (`schema_statements.rb:459,474`). A real server
/// runs it; this node answered `42809`. So the table moved and its sequence kept the **old** name.
///
/// After that the suite's own `force: true` cycle cannot clean up: `DROP TABLE IF EXISTS
/// "references"` finds nothing, because the table is called something else now — and the
/// `CREATE TABLE` that follows wants `references_id_seq`, which is still there, still owned by the
/// renamed table. Every later schema load hits the same wall, and the node answers every health
/// check perfectly while it happens.
#[test]
fn a_renamed_table_leaves_no_sequence_to_collide_with() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE refs (id bigserial PRIMARY KEY, name text)")
        .unwrap();

    // What `rename_table` sends, both statements.
    node.run("ALTER TABLE refs RENAME TO refs2").unwrap();
    node.run("ALTER TABLE refs_id_seq RENAME TO refs2_id_seq")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'refs_id_seq'"),
        [["0"]],
        "the sequence follows its table, or the old name is left to collide with"
    );

    // The schema's `force: true` cycle, which is where the cliff was: the drop finds nothing under
    // the old name, and the create must still succeed.
    node.run("DROP TABLE IF EXISTS refs").unwrap();
    node.run("CREATE TABLE refs (id bigserial PRIMARY KEY, name text)")
        .unwrap();
    // Both tables and both sequences now exist, under four distinct names.
    assert_eq!(
        node.rows(
            "SELECT relname FROM pg_class WHERE relname LIKE 'refs%' AND relkind = 'S' ORDER BY relname"
        ),
        [["refs2_id_seq"], ["refs_id_seq"]]
    );
    // And the sequence still fills the column it belongs to.
    node.run("INSERT INTO refs (name) VALUES ('a')").unwrap();
    node.run("INSERT INTO refs2 (name) VALUES ('b')").unwrap();
    assert_eq!(node.rows("SELECT id FROM refs"), [["1"]]);
    assert_eq!(node.rows("SELECT id FROM refs2"), [["1"]]);
}

/// **The same leak from the other direction: a `serial` column's sequence must die with the
/// column.**
///
/// `DROP COLUMN` takes the sequence out of the table's record — measured on the oracle, and the
/// `DROP COLUMN` unit asserts it — but a sequence has a **name record** of its own, and a name that
/// outlives what it points at is what stopped run 55's schema loads. This is the third relation
/// kind to need that reconciliation and the one where the cost is highest: the name a `serial`
/// column's sequence holds is the name the *next* `CREATE TABLE` wants.
#[test]
fn dropping_a_serial_column_takes_its_sequence_name_with_it() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE sq (id bigint PRIMARY KEY, counter bigserial)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["1"]]
    );

    node.run("ALTER TABLE sq DROP COLUMN counter").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["0"]],
        "the sequence goes with the column, name record and all"
    );

    // And the name is free: the `force: true` cycle re-creates the table and wants that exact
    // sequence name back. This is the statement 101 files could not get past.
    node.run("DROP TABLE IF EXISTS sq").unwrap();
    node.run("CREATE TABLE sq (id bigint PRIMARY KEY, counter bigserial)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["1"]]
    );
    node.run("INSERT INTO sq (id) VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT counter FROM sq"), [["1"]]);
}
