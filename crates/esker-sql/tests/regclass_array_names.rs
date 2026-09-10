//! **A name-bearing array literal into a `regclass[]` column** — and the report that led here,
//! which was not a defect.
//!
//! r1's v3 smoke reported `CREATE TEMP TABLE (c regclass)` refused on this node. It is not: the
//! create works, an index over the column works, a rename shows through it — all three measured
//! below. What actually failed in that probe was the statement after it, `INSERT INTO t VALUES
//! (1, '{ra}')` into a `regclass[]` column, with `0A000 a relation name read as a regclass without
//! a catalog`. The permanent twin at the end of the corpus fails identically, so the
//! temporariness was a coincidence of the probe rather than part of the fault.
//!
//! **The fault is one #41 left behind.** That unit gave the assignment path a name rule — `INSERT
//! INTO t VALUES ('ra')` into a `regclass` column resolves through `regclassin` — and it reached
//! the scalar column it was written for and not the `regclass[]` beside it. `ARRAY['ra'::regclass]`
//! already worked, because that goes through the cast; only the array *literal* did not. One rule,
//! one caller short.
//!
//! The fix reads the literal as a `text[]` first, so the array grammar keeps **one** parser —
//! braces, quoting and NULLs stay `array_in`'s — and only the per-element name resolution is the
//! assignment rule's.
//!
//! Measured in `tests/captures/pg19_regclass_array_names.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// **The whole measured family**, replayed against the corpus.
#[test]
fn every_regclass_array_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regclass_array_names.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The create itself**, which is the statement r1's smoke could not get past.
#[test]
fn a_temporary_table_takes_a_regclass_column() {
    let mut node = parity::Node::new(&["CREATE TABLE ra (id int8)"]);
    node.run("CREATE TEMP TABLE tk (id int8, r regclass)")
        .unwrap();
    node.run("INSERT INTO tk VALUES (1, 'ra'), (2, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, r FROM tk ORDER BY id"),
        vec![vec!["1", "ra"], vec!["2", "\\N"]]
    );
    // The stored form is the number, so a rename shows through here exactly as it does on a
    // permanent table (#35, #39) — the persistence of the relation changes nothing about it.
    node.run("ALTER TABLE ra RENAME TO rz").unwrap();
    assert_eq!(node.rows("SELECT r FROM tk WHERE id = 1"), vec![vec!["rz"]]);
}

/// **And an index over it**, which is #39's shape with the fixture that found this.
#[test]
fn a_temporary_regclass_column_takes_an_index() {
    let mut node = parity::Node::new(&["CREATE TABLE ra (id int8)"]);
    node.run("CREATE TEMP TABLE tk (id int8, r regclass)")
        .unwrap();
    node.run("CREATE INDEX tk_r ON tk (r)").unwrap();
    node.run("INSERT INTO tk VALUES (1, 'ra'), (2, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id FROM tk WHERE r = 'ra'::regclass"),
        vec![vec!["1"]]
    );
}
