//! `FLOAT` and `FLOAT(p)`, against PostgreSQL 19beta1 — statement 73 of `schema.rb`.
//!
//! A spelling, not a type: one keyword naming two types this node already had.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `oid` is a four-byte type this node does not have; a `::oid` answers `bigint`, whose
        // value is identical. Already declared in `tests/regtype.rs` and repeated here because
        // this corpus asks the same question about `float`'s three spellings.
        "SELECT 'float'::regtype::oid, 'float4'::regtype::oid, 'float8'::regtype::oid",
    ],
    answers: &[],
};

#[test]
fn every_float_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_float.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 6,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `float(p)` picks the width by bits: 24 is the last `real` and 25 is the first `double`.
///
/// The boundary is asserted on both sides, because a rule with a threshold is a rule two examples
/// can get wrong in the same direction.
#[test]
fn the_precision_picks_the_width_at_twenty_four_bits() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE fl (id int8 PRIMARY KEY, a FLOAT, b FLOAT(1), c FLOAT(24), d FLOAT(25), \
         e FLOAT(53), f REAL, g DOUBLE PRECISION)",
    )
    .unwrap();

    // `real` prints the shortest digits that round-trip at 32 bits, `double` at 64 — so one value
    // told apart by its own output is what says which type each column got.
    node.run("INSERT INTO fl VALUES (1, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT a, b, c, d, e, f, g FROM fl"),
        vec![vec!["1", "1", "1", "1", "1", "1", "1"]]
    );

    // The declared types are what the spelling actually decides, and the corpus replay above
    // checks those against a real server's `\\gdesc` — asserting them a second time here would be
    // a copy of the oracle rather than a second check of it.
}

/// Both ends of the range, with the message PostgreSQL gives — "precision" and "bits", not
/// "length".
#[test]
fn a_precision_outside_the_range_is_refused_at_both_ends() {
    let mut node = parity::Node::new(&[]);
    for (sql, message) in [
        (
            "CREATE TABLE bad (a FLOAT(0))",
            "precision for type float must be at least 1 bit",
        ),
        (
            "CREATE TABLE bad (a FLOAT(54))",
            "precision for type float must be less than 54 bits",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "22023", "{sql}");
        assert_eq!(error.to_string(), message, "{sql}");
    }
}
