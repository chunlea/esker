//! **`point <>`, the inequality that exists without its equality.**
//!
//! `point_ne` exists on a real server and `point_eq` does not, so `<>` answers and `=` is
//! `42883 operator does not exist: point = point`. That single cell is what made the no-equality
//! census need a table keyed by operator *and* type rather than a list of broken types — a rule
//! deriving one from the other is wrong here and nowhere else.
//!
//! **And the comparison is fuzzy.** `|ax-bx| > 1e-6 || |ay-by| > 1e-6`, so two points differing by
//! `1e-6` are not different and by `1e-5` they are; and every float comparison against a `NaN` is
//! false, so a `NaN` is not different from itself. This crate's `Datum` compares a `point`
//! **bitwise** and its own comment says that is not a SQL equality — so the two must not be
//! collapsed, in either direction.
//!
//! Measured in `tests/captures/pg19_point_ne.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Every measured cell**, read out of the capture rather than restated.
#[test]
fn every_point_ne_answer_is_postgresql_19_s() {
    let mut node = parity::Node::new(&[]);
    let capture = include_str!("captures/pg19_point_ne.txt");
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
        checked >= 8,
        "only {checked} cells read; the capture did not load"
    );
}

/// **`=` is still refused**, which is the half that must not move when the other half lands.
#[test]
fn a_point_still_has_no_equality() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "SELECT '(1,1)'::point = '(1,1)'::point",
        "SELECT '(1,1)'::point < '(1,1)'::point",
        "SELECT '(1,1)'::point IS DISTINCT FROM '(1,1)'::point",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42883", "{sql}");
        // `IS DISTINCT FROM` names the operator it is missing, which is `=`.
        assert_eq!(
            error.to_string(),
            format!(
                "operator does not exist: point {} point",
                if sql.contains('<') && !sql.contains("DISTINCT") {
                    "<"
                } else {
                    "="
                }
            ),
            "{sql}"
        );
    }
}
