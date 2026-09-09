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
    // **Empty, and it took two lanes to empty it.** The one entry read `conname`, `contype` and
    // `pg_get_constraintdef(oid)`. Its two declared types agreed once `name` (ADR 0084) and
    // `"char"` (ADR 0095) were built, and it stayed listed all the same, because the *rows* still
    // differed — the harness reads a `types` entry only when they agree, and this statement was
    // also an `answers` divergence about the deparsed predicate. `exec::ddl::normalise_checks`
    // closed that half, and the two halves were on different branches: each side was green alone
    // and the entry only became stale on the merged tree.
    types: &[],
    // **Nothing.** The entry that stood here said the predicate was printed as written where
    // PostgreSQL prints its deparsed tree — `CHECK ((q <> 'no'))` against
    // `CHECK ((q <> 'no'::text))` — with the reasoning that closing it meant deparsing a lowered
    // expression rather than keeping the text. That is what `exec::ddl::normalise_checks` now
    // does, at the statement that writes the check, and the text is still text: the deparser
    // prints into the same `CheckDef::expr` the reader already used. It was also this file's one
    // `UNMEASURED` provenance.
    answers: &[],
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

/// A constraint added by `ALTER` is checked against the rows already there.
///
/// **This test used to pin the opposite**, and said so: the rows were not validated, an
/// `ADD CONSTRAINT` did not go through ADR 0020's backfill, and it recorded what the node did
/// rather than what PostgreSQL does. Closing that is what `NOT VALID` needed — the clause skips a
/// scan, and there was no scan to skip.
#[test]
fn a_constraint_added_later_is_checked_against_the_rows_already_there() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ck (id int8 PRIMARY KEY, p int8)",
        "INSERT INTO ck VALUES (1, -5)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node
        .run("ALTER TABLE ck ADD CONSTRAINT ck_p CHECK (p > 0)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    // **Its own sentence**, and not the one an `INSERT` gets: the scan found *some* row, and
    // naming one of many would suggest it was the only one.
    assert_eq!(
        error.to_string(),
        "check constraint \"ck_p\" of relation \"ck\" is violated by some row"
    );
    // The row is untouched and the constraint was not added.
    assert_eq!(node.rows("SELECT p FROM ck"), vec![vec!["-5"]]);
}

/// `NOT VALID` takes the constraint without the scan — and it still binds every later row.
///
/// The half a reader gets backwards: `NOT VALID` says what was **skipped**, not what is enforced.
#[test]
fn a_not_valid_constraint_skips_the_scan_and_binds_later_rows() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ckn (id int8 PRIMARY KEY, p int8)",
        "INSERT INTO ckn VALUES (1, -5)",
        "ALTER TABLE ckn ADD CONSTRAINT ckn_p CHECK (p > 0) NOT VALID",
    ] {
        node.run(statement).unwrap();
    }
    // The row that predates it is still there, and the catalog says the scan was skipped.
    assert_eq!(node.rows("SELECT p FROM ckn"), vec![vec!["-5"]]);
    assert_eq!(
        node.rows("SELECT convalidated FROM pg_constraint WHERE conname = 'ckn_p'"),
        vec![vec!["f"]]
    );
    // And it binds from here on regardless.
    let error = node.run("INSERT INTO ckn VALUES (2, -1)").unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    // `VALIDATE CONSTRAINT` runs the scan that was skipped, and finds the row.
    let error = node
        .run("ALTER TABLE ckn VALIDATE CONSTRAINT ckn_p")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    node.run("DELETE FROM ckn WHERE p < 0").unwrap();
    node.run("ALTER TABLE ckn VALIDATE CONSTRAINT ckn_p")
        .unwrap();
    assert_eq!(
        node.rows("SELECT convalidated FROM pg_constraint WHERE conname = 'ckn_p'"),
        vec![vec!["t"]]
    );
}
