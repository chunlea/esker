//! `ALTER TABLE … DROP COLUMN` — 34 tests over 11 files of run 47's ranking.
//!
//! **The column is tombstoned, not removed** ([ADR 0051](../../../docs/adr/0051-a-dropped-column-keeps-its-slot.md)).
//! A row is decoded by position, so taking a column's slot out of the schema would turn every row
//! written before the `ALTER` into a decode error — `decode_row` refuses a row carrying more
//! columns than the schema, deliberately. Keeping the slot is what makes this statement touch no
//! row at all, which is also what PostgreSQL does.
//!
//! The corpus is r1's capture of what `ActiveRecord` sends. Two of its lines are the ones an
//! implementation gets wrong by reasoning: `remove_columns` sends **one** statement with several
//! `DROP COLUMN` clauses, and a re-added column of the same name gets a **new** `attnum` rather
//! than the tombstone's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_indexes.indexname` is a `name` on a real server and `text` here, with the same
    // characters in it — the standing trade every `pg_catalog` column makes. Both occurrences of
    // the statement, because the entry is per statement text and the capture asks it twice.
    types: &[],
    answers: &[
        // **`CREATE VIEW` landed (`tests/view.rs`) and deleted the three entries that used to be
        // here** — the view is created, and `pg_class` reports it. What replaces them is the gap
        // that was hiding behind the refusal, and one bug that was hiding behind the *abort*.
        // **Both entries here were the missing view-to-column dependency edge**, and this file
        // named the follow-on unit that would close them: "that edge is the follow-on unit, and
        // this capture already says what both answers must become." It was built
        // (`tests/view_debts.rs`), both lines agree, and both entries are deleted (ADR 0031
        // rule 2).
    ],
};

#[test]
fn every_drop_column_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_drop_column.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The property the corpus cannot reach: a row written *before* the drop and one written
/// *after* it, read by one `SELECT`.**
///
/// This is the test ADR 0051 names as the unit's obligation, and it is the one that fails if the
/// slot is taken out rather than tombstoned — the old row would decode as corrupt, or worse, its
/// columns would shift by one and every value after the gap would be read as its neighbour. A
/// corpus replays statements against a fresh node and would not notice: it is the *mixture* that
/// is the subject.
#[test]
fn rows_from_both_sides_of_a_drop_read_the_same() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY, keep text, gone bigint, tail text)")
        .unwrap();
    node.run("INSERT INTO dc VALUES (1, 'a', 10, 'x')").unwrap();
    node.run("INSERT INTO dc VALUES (2, 'b', 20, 'y')").unwrap();

    node.run("ALTER TABLE dc DROP COLUMN gone").unwrap();

    // A row written after the drop, in the same table, beside two written before it.
    node.run("INSERT INTO dc VALUES (3, 'c', 'z')").unwrap();

    // Every row reads its own columns, and `tail` is `tail` in all three — the assertion that
    // fails by one column if the dropped slot is not held.
    assert_eq!(
        node.rows("SELECT * FROM dc ORDER BY id"),
        [["1", "a", "x"], ["2", "b", "y"], ["3", "c", "z"],]
    );
    // Named rather than starred, because `SELECT *` and a column list resolve by different paths.
    assert_eq!(
        node.rows("SELECT tail, keep FROM dc ORDER BY id"),
        [["x", "a"], ["y", "b"], ["z", "c"]]
    );

    // An `UPDATE` re-encodes a pre-drop row from the live schema. It must still read back the same.
    node.run("UPDATE dc SET tail = 'X' WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id, keep, tail FROM dc ORDER BY id"),
        [["1", "a", "X"], ["2", "b", "y"], ["3", "c", "z"]]
    );
}

/// **A dropped column is not a column**, in every clause that can name one.
///
/// One lookup — `TableDef::column` — is what makes all of these `42703` at once, and this is the
/// test that says so: a fix applied clause by clause would leave whichever clause nobody thought
/// of resolving a column the user cannot see.
#[test]
fn a_dropped_column_cannot_be_named() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY, keep text, gone bigint)")
        .unwrap();
    node.run("INSERT INTO dc VALUES (1, 'a', 10)").unwrap();
    node.run("ALTER TABLE dc DROP COLUMN gone").unwrap();

    for sql in [
        "SELECT gone FROM dc",
        "SELECT id FROM dc WHERE gone = 10",
        "SELECT id FROM dc ORDER BY gone",
        "SELECT count(*) FROM dc GROUP BY gone",
        "UPDATE dc SET gone = 1",
        "INSERT INTO dc (id, gone) VALUES (2, 3)",
        "DELETE FROM dc WHERE gone IS NULL",
    ] {
        let answer = node.answer(sql).to_string();
        assert!(
            answer.starts_with("!42703"),
            "{sql} answered {answer}, not a 42703"
        );
    }

    // And `INSERT` with no column list takes the live columns only — two values, not three.
    node.run("INSERT INTO dc VALUES (2, 'b')").unwrap();
    assert_eq!(
        node.rows("SELECT * FROM dc ORDER BY id"),
        [["1", "a"], ["2", "b"]]
    );
}

/// **The name is free again, and the slot is not.**
///
/// Measured: `ADD COLUMN gone` after dropping `gone` gives attnum 5 in a table whose tombstone is
/// 3. A node that reused the slot would answer `column_definitions` correctly and still be wrong,
/// because the re-added column would inherit the old one's stored bytes in every row that has them.
#[test]
fn a_re_added_column_gets_a_new_slot_and_reads_null() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY, keep text, gone bigint)")
        .unwrap();
    node.run("INSERT INTO dc VALUES (1, 'a', 10)").unwrap();
    node.run("ALTER TABLE dc DROP COLUMN gone").unwrap();
    node.run("ALTER TABLE dc ADD COLUMN gone text").unwrap();

    // **NULL, not `10`.** The old value is still in the row's bytes at slot 2; reading it here
    // would be the bug the new slot exists to prevent — and it would be a value of the wrong type.
    assert_eq!(
        node.rows("SELECT id, keep, gone FROM dc"),
        [["1", "a", "\\N"]]
    );
    node.run("UPDATE dc SET gone = 'new' WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id, keep, gone FROM dc"),
        [["1", "a", "new"]]
    );
}

/// **Several clauses in one statement**, which is what `remove_columns` and `remove_timestamps`
/// send — and `change_table` can put an `ADD COLUMN` in the same one.
#[test]
fn one_alter_table_can_drop_several_columns() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY, a text, b text, c text)")
        .unwrap();
    node.run("INSERT INTO dc VALUES (1, 'x', 'y', 'z')")
        .unwrap();
    node.run(r#"ALTER TABLE "dc" DROP COLUMN "b", DROP COLUMN "c""#)
        .unwrap();
    assert_eq!(node.rows("SELECT * FROM dc"), [["1", "x"]]);

    node.run(r#"ALTER TABLE "dc" ADD COLUMN "d" text, DROP COLUMN "a""#)
        .unwrap();
    assert_eq!(node.rows("SELECT * FROM dc"), [["1", "\\N"]]);
}

/// **What goes with the column, and what refuses.** The line is where the dependent lives, not
/// what kind it is: everything on this table goes silently, and only another table's foreign key
/// raises `2BP01`.
#[test]
fn the_drop_takes_its_dependents_and_refuses_the_others() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY, keep text, gone bigint NOT NULL DEFAULT 7)")
        .unwrap();
    node.run("CREATE INDEX dc_gone_idx ON dc (gone)").unwrap();
    node.run("CREATE INDEX dc_pair_idx ON dc (keep, gone)")
        .unwrap();
    node.run("ALTER TABLE dc ADD CONSTRAINT dc_gone_check CHECK (gone >= 0)")
        .unwrap();
    node.run("ALTER TABLE dc DROP COLUMN gone").unwrap();

    // Both indexes, including the one where only one of two key columns was dropped.
    assert_eq!(
        node.rows(
            "SELECT 'r', indexname FROM pg_indexes WHERE tablename = 'dc' ORDER BY indexname"
        ),
        [["r", "dc_pkey"]]
    );
    // And the CHECK, which is stored as text and is found by no longer resolving.
    assert!(
        !node
            .rows("SELECT conname FROM pg_constraint WHERE conrelid = 'dc'::regclass")
            .iter()
            .any(|row| row[0] == "dc_gone_check"),
        "the CHECK over the dropped column is still there"
    );
    // A row still writes, which it could not if the NOT NULL had survived the column.
    node.run("INSERT INTO dc VALUES (1, 'a')").unwrap();

    // Another table's foreign key is the one dependent that lives elsewhere.
    let mut other = parity::Node::new(&[]);
    other
        .run("CREATE TABLE p (id bigint PRIMARY KEY, u bigint UNIQUE)")
        .unwrap();
    other
        .run("CREATE TABLE c (id bigint PRIMARY KEY, pu bigint REFERENCES p(u))")
        .unwrap();
    let refused = other.answer("ALTER TABLE p DROP COLUMN u").to_string();
    assert!(
        refused.starts_with("!2BP01"),
        "a referenced column must refuse: {refused}"
    );
    other.run("ALTER TABLE p DROP COLUMN u CASCADE").unwrap();
    // The child's key went with the CASCADE, so the child still writes.
    other.run("INSERT INTO c VALUES (1, 9)").unwrap();
}

/// `DROP COLUMN` of a column that is not there, and of a table that is not there.
#[test]
fn a_column_that_is_not_there_is_42703_or_a_notice() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc (id bigint PRIMARY KEY)").unwrap();

    assert_eq!(
        node.answer(r#"ALTER TABLE "dc" DROP COLUMN "nosuchcolumn""#)
            .to_string(),
        "!42703 column \"nosuchcolumn\" of relation \"dc\" does not exist"
    );
    // `IF EXISTS` succeeds, and says so as a notice rather than an error.
    node.run(r#"ALTER TABLE "dc" DROP COLUMN IF EXISTS "nosuchcolumn""#)
        .unwrap();
    assert_eq!(
        node.answer(r#"ALTER TABLE "nosuchtable" DROP COLUMN "x""#)
            .to_string(),
        "!42P01 relation \"nosuchtable\" does not exist"
    );
}

/// **A table may lose its last column, and keep its rows.** Refusing that is a divergence, and it
/// is the case a "there must be at least one column" guard would break.
#[test]
fn the_last_column_can_go_and_the_rows_stay() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dc1 (only_col bigint)").unwrap();
    node.run("INSERT INTO dc1 VALUES (1)").unwrap();
    node.run("INSERT INTO dc1 VALUES (2)").unwrap();
    node.run(r#"ALTER TABLE "dc1" DROP COLUMN "only_col""#)
        .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM dc1"), [["2"]]);
}

/// **Run 50's regression: a name must not outlive the table it points at.**
///
/// `t.references :rocket, foreign_key: true` makes a column, an **index** over it and a foreign
/// key. `remove_column` takes all three, and the migration's own teardown then drops the tables —
/// which came back `XX001 corrupt data: a name points at table N, which is not there`, 66 tests
/// over 2 files (`ForeignKeyChangeColumnWithPrefixTest#test_remove_reference_column_of_child_table`
/// and its siblings). The sequence is that test's, with the prefix its class sets.
#[test]
fn dropping_a_referenced_column_leaves_no_name_behind() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE p_rockets (id bigint PRIMARY KEY, name text)")
        .unwrap();
    node.run("CREATE TABLE p_astronauts (id bigint PRIMARY KEY, name text, rocket_id bigint)")
        .unwrap();
    node.run("CREATE INDEX index_p_astronauts_on_rocket_id ON p_astronauts (rocket_id)")
        .unwrap();
    node.run(
        "ALTER TABLE p_astronauts ADD CONSTRAINT fk_rails_a1 FOREIGN KEY (rocket_id) \
         REFERENCES p_rockets (id)",
    )
    .unwrap();

    node.run("ALTER TABLE p_astronauts DROP COLUMN rocket_id")
        .unwrap();

    // The teardown the migration replays.
    node.run("DROP TABLE p_astronauts").unwrap();
    node.run("DROP TABLE p_rockets").unwrap();

    // And nothing is left claiming a relation: the next statement to walk the names must not meet
    // one that points at a table which is gone.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname LIKE 'p\\_%'"),
        [["0"]]
    );
    node.run("CREATE TABLE p_rockets (id bigint PRIMARY KEY)")
        .unwrap();
}

/// **The back-reference is per `(parent, child)` pair, not per constraint**, which is what makes
/// the fix for the above a rule rather than a delete.
///
/// A child holding two foreign keys into one parent has **one** back-reference between them. If
/// dropping the column under the first key deleted it, the parent would be told nothing references
/// it while the second key still does — and the `DROP TABLE` that must be `2BP01` would go
/// through. That is a wrong answer, where the bug this pairs with was only a stale key.
#[test]
fn two_keys_into_one_parent_keep_the_back_reference_until_the_last_goes() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE p (id bigint PRIMARY KEY, u bigint UNIQUE)")
        .unwrap();
    node.run(
        "CREATE TABLE c (id bigint PRIMARY KEY, a bigint REFERENCES p (id), \
         b bigint REFERENCES p (id))",
    )
    .unwrap();

    // One of the two goes with its column; the other still points at `p`.
    node.run("ALTER TABLE c DROP COLUMN a").unwrap();
    let refused = node.answer("DROP TABLE p").to_string();
    assert!(
        refused.starts_with("!2BP01"),
        "the surviving key must still protect the parent: {refused}"
    );

    // The last one goes, and only now is the parent free.
    node.run("ALTER TABLE c DROP COLUMN b").unwrap();
    node.run("DROP TABLE p").unwrap();
    // And the child outliving it must leave nothing behind either.
    node.run("DROP TABLE c").unwrap();
    node.run("CREATE TABLE p (id bigint PRIMARY KEY)").unwrap();
}
