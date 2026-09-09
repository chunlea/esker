//! The four foreign-key and column options `migration/foreign_key_test.rb` needs.
//!
//! The file is 88 tests. Run 56 measured 3 failures and 79 errors; 69 of those errors were the
//! renamed-index name leak and closed with `00203bf`, leaving **6 failures and 10 errors** in four
//! shapes — `SET NOT NULL`, `ON DELETE SET NULL`, `NOT VALID`/`VALIDATE CONSTRAINT`, and
//! `FOREIGN KEY … INITIALLY DEFERRED`. On PostgreSQL 19 the whole file is 88 runs / 244 assertions
//! / 0 errors, which is where `pg19_foreign_key_options.txt` was captured.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Nothing left to declare.** Three shapes of the `pg_catalog` trade lived here and each was
    // closed by the unit that built the type it named: `conname` is a `name` (ADR 0084),
    // `array_agg(attname)` a `name[]` (ADR 0086), and `confupdtype`/`confdeltype` are `"char"`
    // (ADR 0095). The characters always agreed; the declared types do now as well.
    types: &[],
    answers: &[],
};

#[test]
fn every_foreign_key_option_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_foreign_key_options.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **What `INITIALLY DEFERRED` is for: the child before the parent.**
///
/// The capture proves the violation is allowed to stand inside the transaction; this proves the
/// half that matters to a schema loader — a transaction that breaks the constraint in the middle
/// and repairs it before `COMMIT` **commits**, and one that does not is refused at the `COMMIT`
/// rather than at the statement. An implementation that accepted the clause and checked
/// immediately would refuse the first, which is a wrong answer rather than a missing feature.
#[test]
fn a_deferred_foreign_key_is_checked_at_commit() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dfp (id bigint PRIMARY KEY)",
        "CREATE TABLE dfc (id bigint PRIMARY KEY, p bigint REFERENCES dfp (id) DEFERRABLE \
         INITIALLY DEFERRED)",
    ]);

    // Repaired before the end: the child is written first and its parent arrives after.
    for statement in [
        "BEGIN",
        "INSERT INTO dfc VALUES (1, 10)",
        "INSERT INTO dfp VALUES (10)",
        "COMMIT",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(node.rows("SELECT count(*) FROM dfc"), [["1"]]);

    // Never repaired: the statement is admitted and the **`COMMIT`** is what refuses.
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO dfc VALUES (2, 99)").unwrap();
    let error = node.run("COMMIT").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    assert!(
        error
            .to_string()
            .contains("violates foreign key constraint"),
        "{error}"
    );
    // And the transaction took the row with it.
    assert_eq!(node.rows("SELECT count(*) FROM dfc"), [["1"]]);

    // `SET CONSTRAINTS … IMMEDIATE` runs what is owed at once, which is where it surfaces early.
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO dfc VALUES (3, 98)").unwrap();
    let error = node.run("SET CONSTRAINTS ALL IMMEDIATE").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    node.run("ROLLBACK").unwrap();
}
