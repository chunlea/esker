//! Building a `UNIQUE` index or constraint over rows that already break it.
//!
//! **ADR 0020's recorded gap.** `unique_constraint.rs` carried it as a named wrong answer:
//! "Nothing here validates existing rows when an index is created." Measured, that was half true —
//! `CREATE UNIQUE INDEX` refused (with the wrong sentence), `ALTER TABLE … ADD CONSTRAINT …
//! UNIQUE` did not refuse at all, and neither noticed a duplicate that exists only under
//! `NULLS NOT DISTINCT`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **This list was one entry and is empty.** It declared that
    // `ALTER TABLE … ADD CONSTRAINT … PRIMARY KEY` came back `0A000` rather than PostgreSQL's
    // `42P16 multiple primary keys for table "uoe" are not allowed`, and said in its own words
    // that "adding a primary key to a table without one is its own unit". That unit landed, so the
    // row agrees and the entry goes — ADR 0031 rule 2, and the harness is what noticed.
    answers: &[],
};

#[test]
fn every_unique_over_existing_rows_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_unique_over_existing_rows.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The half that is not about validation: the rows already there have to be *in* the index.**
///
/// `ADD CONSTRAINT … UNIQUE` declared the index and never filled it, so the constraint was a
/// catalog entry over an empty index. Two things followed, and the second is the worse one:
///
/// * a later duplicate of an existing row was **accepted**, because the scan found nothing to
///   collide with;
/// * `SELECT … WHERE a = <existing value>` answered with only the *new* row — the pre-existing one
///   was invisible through the index while `count(*)` still counted it.
///
/// That second is a wrong answer to an ordinary query, reached through a statement that succeeded,
/// and no amount of validation-at-DDL-time would have found it: the table was empty of duplicates
/// in every test that only checked the refusal.
#[test]
fn a_unique_constraint_indexes_the_rows_that_were_already_there() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ux (id bigint PRIMARY KEY, a int)",
        "INSERT INTO ux VALUES (1, 10), (2, 20)",
    ]);
    node.run("ALTER TABLE ux ADD CONSTRAINT ux_a_uq UNIQUE (a)")
        .unwrap();

    // The pre-existing row is findable through the index it was added to.
    assert_eq!(node.rows("SELECT id FROM ux WHERE a = 10"), [["1"]]);
    assert_eq!(node.rows("SELECT count(*) FROM ux"), [["2"]]);

    // And it is what a later duplicate collides with.
    let error = node.run("INSERT INTO ux VALUES (3, 10)").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(node.rows("SELECT count(*) FROM ux"), [["2"]]);

    // The same for a constraint added over a table that is empty at the time: the index is filled
    // by the writes that follow, which is the path that always worked.
    node.run("CREATE TABLE uy (id bigint PRIMARY KEY, a int)")
        .unwrap();
    node.run("ALTER TABLE uy ADD CONSTRAINT uy_a_uq UNIQUE (a)")
        .unwrap();
    node.run("INSERT INTO uy VALUES (1, 10)").unwrap();
    assert_eq!(node.rows("SELECT id FROM uy WHERE a = 10"), [["1"]]);
    assert_eq!(
        node.run("INSERT INTO uy VALUES (2, 10)")
            .unwrap_err()
            .sqlstate(),
        "23505"
    );
}
