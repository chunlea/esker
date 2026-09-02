//! A partial index, against PostgreSQL 19beta1 — statement 77 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_partial_index_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_partial_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A partial index is **maintained and never read**.
///
/// Choosing one for a scan is correct only when the query's predicate implies the index's, and
/// this crate has no implication prover — so it never narrows a read. `WHERE b = 7` returns every
/// row with `b = 7`, not the subset the index holds, which is the anomaly this refusal prevents.
#[test]
fn a_partial_index_constrains_without_narrowing_a_read() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE px (id int8 PRIMARY KEY, b int8, published_on int8)",
        "CREATE UNIQUE INDEX px_partial ON px (b) WHERE published_on IS NOT NULL",
        "INSERT INTO px VALUES (1, 7, 100)",
        // Out of the index, because a NULL is not *true* — so their duplicate `b` is fine.
        "INSERT INTO px VALUES (2, 7, NULL)",
        "INSERT INTO px VALUES (3, 7, NULL)",
    ] {
        node.run(statement).unwrap();
    }

    // In the index, and colliding.
    let error = node.run("INSERT INTO px VALUES (4, 7, 200)").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");

    // Every row with `b = 7`, not the one the index holds.
    assert_eq!(
        node.rows("SELECT id FROM px WHERE b = 7 ORDER BY id"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
}

/// An `UPDATE` moves a row into and out of the index, which is the half a write-only
/// implementation gets wrong.
#[test]
fn an_update_moves_a_row_across_the_predicate() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE px (id int8 PRIMARY KEY, b int8, published_on int8)",
        "CREATE UNIQUE INDEX px_partial ON px (b) WHERE published_on IS NOT NULL",
        "INSERT INTO px VALUES (1, 7, 100)",
        "INSERT INTO px VALUES (2, 7, NULL)",
    ] {
        node.run(statement).unwrap();
    }

    // Moving row 2 *in* collides with row 1, which is already there.
    let error = node
        .run("UPDATE px SET published_on = 300 WHERE id = 2")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");

    // Move row 1 *out*, and the same update now succeeds.
    node.run("UPDATE px SET published_on = NULL WHERE id = 1")
        .unwrap();
    node.run("UPDATE px SET published_on = 300 WHERE id = 2")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, published_on FROM px ORDER BY id"),
        vec![vec!["1", "\\N"], vec!["2", "300"]]
    );
}

/// A predicate naming a column the table does not have is `42703` at `CREATE INDEX`.
///
/// Not an internal error at the first write, which is what a predicate lowered only per row would
/// have given — the same rule, and the same reason, as a `CHECK`'s.
#[test]
fn a_predicate_that_does_not_resolve_is_refused_when_the_index_is_made() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE px (id int8 PRIMARY KEY, b int8)")
        .unwrap();
    let error = node
        .run("CREATE INDEX px_bad ON px (b) WHERE nosuchcol > 1")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42703");
}
