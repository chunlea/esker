//! `CHECK` constraints, against PostgreSQL 19beta1 — half of statement 52 of `schema.rb`.
//!
//! The other half is `FOREIGN KEY`, a unit of its own because it has to enforce.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `conname` is `name` and `contype` is `"char"`; this node has neither and answers `text`,
        // whose values are identical. The same trade every `pg_catalog` column makes.
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = \
         'ck'::regclass AND contype = 'c' ORDER BY conname",
        "SELECT conname FROM pg_constraint WHERE conrelid = 'ck'::regclass ORDER BY conname",
    ],
    answers: &[
        (
            "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE \
             conrelid = 'ck'::regclass AND contype = 'c' ORDER BY conname",
            "**The predicate is printed as written, where PostgreSQL prints its deparsed tree.** \
             A real server answers `CHECK ((q <> 'no'::text))` and this node `CHECK ((q <> \
             'no'))` — the doubled parentheses agree and the `::text` does not, because \
             PostgreSQL annotates each literal with the type it resolved to and this node stores \
             the text the user wrote. Closing it means deparsing a lowered expression rather than \
             keeping the text, which would also mean the catalog holds a serialised tree; the \
             text is what `pg_get_constraintdef` needs anyway, so the trade was made deliberately \
             (`catalog::CheckDef`). Semantics and values are identical; one string differs.",
        ),
        (
            "SELECT conname FROM pg_constraint WHERE conrelid = 'ck'::regclass ORDER BY conname",
            "The same statement without the `contype` filter, so it also lists the `NOT NULL` and \
             primary-key rows. Those are e2-catalog's and already agree; it is here only because \
             the `CHECK` rows had to join them in one ordering.",
        ),
    ],
};

#[test]
fn every_check_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_check.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A NULL **passes** a `CHECK`, because only `false` violates one.
///
/// The rule an implementation gets wrong by evaluating the predicate as a boolean and treating
/// "not true" as a violation. SQL's three-valued logic makes `NULL > 0` unknown, and unknown is
/// admitted.
#[test]
fn a_null_passes_because_only_false_violates() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ck (id int8 PRIMARY KEY, p int8, CONSTRAINT ck_p CHECK (p > 0))",
        "INSERT INTO ck VALUES (1, 5)",
        "INSERT INTO ck VALUES (2, NULL)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id, p FROM ck ORDER BY id"),
        vec![vec!["1", "5"], vec!["2", "\\N"]]
    );

    let error = node.run("INSERT INTO ck VALUES (3, -1)").unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    assert_eq!(
        error.to_string(),
        "new row for relation \"ck\" violates check constraint \"ck_p\""
    );
}

/// An `UPDATE` is checked too, not only an `INSERT`.
#[test]
fn an_update_is_checked_as_well_as_an_insert() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ck (id int8 PRIMARY KEY, p int8, CONSTRAINT ck_p CHECK (p > 0))",
        "INSERT INTO ck VALUES (1, 5)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("UPDATE ck SET p = -2 WHERE id = 1").unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    // And the row is unchanged, because the statement failed before it wrote.
    assert_eq!(node.rows("SELECT p FROM ck"), vec![vec!["5"]]);

    node.run("UPDATE ck SET p = 9 WHERE id = 1").unwrap();
    assert_eq!(node.rows("SELECT p FROM ck"), vec![vec!["9"]]);
}

/// The name is derived when nothing gives one, and a duplicate is `42710`.
#[test]
fn a_derived_name_and_a_duplicate_one() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ck (id int8 PRIMARY KEY, q text CHECK (q <> ''))")
        .unwrap();

    // `<table>_<column>_check`, which is what a violation names.
    let error = node.run("INSERT INTO ck VALUES (1, '')").unwrap_err();
    assert!(
        error.to_string().contains("ck_q_check"),
        "`{error}` does not carry the derived name"
    );

    node.run("ALTER TABLE ck ADD CONSTRAINT ck_extra CHECK (q <> 'no')")
        .unwrap();
    let error = node
        .run("ALTER TABLE ck ADD CONSTRAINT ck_extra CHECK (q <> 'x')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42710");
    assert_eq!(
        error.to_string(),
        "constraint \"ck_extra\" for relation \"ck\" already exists"
    );
}

/// A constraint added by `ALTER` binds the rows written after it.
///
/// **It is not validated against the rows already there**, which PostgreSQL does do. A backfill is
/// the schema-change machinery of ADR 0020 and an `ADD CONSTRAINT` does not go through it yet, so
/// this test pins what the node actually does rather than what it should eventually do — and says
/// so, which is the difference between a known gap and a surprise.
#[test]
fn a_constraint_added_later_binds_later_rows() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ck (id int8 PRIMARY KEY, p int8)",
        "INSERT INTO ck VALUES (1, -5)",
        "ALTER TABLE ck ADD CONSTRAINT ck_p CHECK (p > 0)",
    ] {
        node.run(statement).unwrap();
    }
    // The row that predates the constraint is still there — no validation pass ran.
    assert_eq!(node.rows("SELECT p FROM ck"), vec![vec!["-5"]]);
    // And the constraint binds from here on.
    let error = node.run("INSERT INTO ck VALUES (2, -1)").unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
}

