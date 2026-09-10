//! **`~=`, "same as"** — the last operator of the (b) direction this node refused outright.
//!
//! It runs the opposite way from its neighbours: `point` and `polygon` have it and the document
//! types do not, which is why the no-equality census needed a table keyed by operator *and* type
//! rather than a list of broken types.
//!
//! **A polygon is the same as its own rotation and its own reversal.** PostgreSQL does not
//! normalise the vertex list — a rotation prints differently and compares same — so comparing
//! canonical text answers `f` on every rotation, and comparing canonical text is exactly what a
//! node that stores canonical text reaches for first.
//!
//! Measured in `tests/captures/pg19_same_as.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Every measured cell**, read out of the capture rather than restated.
#[test]
fn every_same_as_answer_is_postgresql_19_s() {
    let mut node = parity::Node::new(&[]);
    let capture = include_str!("captures/pg19_same_as.txt");
    let mut checked = 0;
    for line in capture.lines().filter(|line| !line.starts_with('#')) {
        let mut fields = line.split('\t');
        let (Some(statement), Some(_), Some(expected)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        assert_eq!(
            node.rows(statement),
            vec![vec![expected.to_owned()]],
            "{statement}"
        );
        checked += 1;
    }
    assert!(
        checked >= 16,
        "only {checked} cells read; the capture did not load"
    );
}

/// **The document types still refuse it**, which is the half that must not move.
#[test]
fn a_document_type_still_has_no_same_as() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "SELECT '{\"a\":1}'::json ~= '{\"a\":1}'::json",
        "SELECT '{\"a\":1}'::jsonb ~= '{\"a\":1}'::jsonb",
        "SELECT '<a/>'::xml ~= '<a/>'::xml",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42883", "{sql}");
    }
}
