//! `UNIQUE` constraints and **when** they are checked, against PostgreSQL 19beta1.
//!
//! Statement 779 of `postgresql_specific_schema.rb` declares four of them on one table, and the
//! fourth is `DEFERRABLE INITIALLY DEFERRED` — the form that does not check at the statement. It
//! was refused by name until the transaction could owe a check (`crate::exec::deferred`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `information_schema` reports `name` and `character varying(3)` where this node answers `text` — the trade every catalog column makes, with identical values. The row agrees, `is_deferrable` and `initially_deferred` included, which is what this unit changed: they were hardcoded `NO` while nothing could be deferred.
    types: &[
        "SELECT 'r', constraint_name, constraint_type, is_deferrable, initially_deferred FROM \
         information_schema.table_constraints WHERE table_name = 'test_unique_constraints' ORDER BY \
         constraint_name",
    ],
    answers: &[(
        "ALTER TABLE \"test_unique_constraints\" ADD CONSTRAINT \"u_nnd_deferred\" UNIQUE NULLS NOT DISTINCT (\"position_2\") DEFERRABLE INITIALLY DEFERRED",
        "**`ALTER TABLE … ADD CONSTRAINT … UNIQUE` is a different unit**: adding a unique constraint to a table that already has rows has to validate them, which is the staged backfill of ADR 0020 rather than anything about *when* a check runs. Both rows are here because the capture builds up to them; the deferrable half of each — `DEFERRABLE INITIALLY DEFERRED` on the new constraint — is what this unit built and is exercised by the `CREATE TABLE` forms above. It is also the **last blocker on this corpus**: it aborts the block, so the seven statements after it are swallowed rather than checked.",
    )],
};

#[test]
fn every_unique_constraint_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_unique_constraint.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 50,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `SET CONSTRAINTS`, and the three answers it has.
///
/// The capture reaches these through `SAVEPOINT`/`ROLLBACK TO`, because each of them aborts the
/// block it is in; asserted here directly so that each is its own failure rather than one long
/// sequence whose first error hides the rest.
#[test]
fn set_constraints_switches_the_mode_for_the_transaction() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE sc (id int8 PRIMARY KEY, p1 int4, p2 int4, CONSTRAINT sc_plain UNIQUE (p1), \
         CONSTRAINT sc_di UNIQUE (p2) DEFERRABLE INITIALLY IMMEDIATE)",
        "INSERT INTO sc VALUES (1, 1, 1)",
    ]);

    // **Not deferrable is `42809`**, and it is raised for `IMMEDIATE` too — which would change
    // nothing — because PostgreSQL refuses the statement either way.
    let error = node.run("SET CONSTRAINTS sc_plain DEFERRED").unwrap_err();
    assert_eq!(error.sqlstate(), "42809");
    assert_eq!(
        error.to_string(),
        "constraint \"sc_plain\" is not deferrable"
    );

    // A name nothing has is `42704`, and the check for *existence* comes first.
    let error = node.run("SET CONSTRAINTS nosuch IMMEDIATE").unwrap_err();
    assert_eq!(error.sqlstate(), "42704");
    assert_eq!(error.to_string(), "constraint \"nosuch\" does not exist");

    // **A constraint declared `INITIALLY IMMEDIATE` can be deferred for one transaction**, which
    // is the half of `SET CONSTRAINTS` that is not about running checks early.
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS sc_di DEFERRED").unwrap();
    node.run("INSERT INTO sc VALUES (2, 2, 1)").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM sc"), vec![vec!["2"]]);

    // And `IMMEDIATE` runs what is owed **at once**, which is where the violation appears.
    let error = node.run("SET CONSTRAINTS sc_di IMMEDIATE").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"sc_di\""
    );
    node.run("ROLLBACK").unwrap();

    // **`ALL` reaches only the deferrable ones.** A plain `UNIQUE` still refuses its duplicate at
    // the statement, with `SET CONSTRAINTS ALL DEFERRED` in force — measured, and the reason
    // `ALL` is not "every constraint".
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS ALL DEFERRED").unwrap();
    let error = node.run("INSERT INTO sc VALUES (3, 1, 3)").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"sc_plain\""
    );
    node.run("ROLLBACK").unwrap();

    // A mode set outside a block belongs to that statement's own transaction and no other: this
    // is the leak that made a later `BEGIN` inherit it and stopped an immediate constraint
    // checking at the statement.
    node.run("SET CONSTRAINTS ALL DEFERRED").unwrap();
    node.run("BEGIN").unwrap();
    let error = node.run("INSERT INTO sc VALUES (4, 4, 1)").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    node.run("ROLLBACK").unwrap();
}
