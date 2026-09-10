//! **Which pairs of types compare at all** — `debts-v1.1.md` #43's second mechanism, measured as a
//! matrix before any of it was touched.
//!
//! The row's named symptom is `1::oid = 1::int8` becoming `42883` once the constant fold stops
//! producing a literal for `reconcile` to retype. **It does not reproduce today**: with #42's
//! text-only fold both operands still fold, so `reconcile` retypes them and the comparison
//! answers. The defect is latent, waiting for the fold to narrow further — so measuring the row's
//! own example would have measured nothing.
//!
//! What this measures instead is the surface that mechanism lives in: every ordered pair of the 52
//! scalar types both sides have, compared **over columns** so the answer comes from the resolver
//! and not from the fold. 2,704 pairs, and 59 of them disagreed:
//!
//! ```text
//! 12  PG answers, node refuses   citext against the whole text family, and interval/time
//! 47  PG refuses, node ANSWERS   oid and reg* against numeric and the floats; the two vectors
//!                                against the text family; lquery against itself
//! ```
//!
//! **The worse direction was the larger one**, and none of it was in the row: it came of a flat
//! family tag being asked a question it cannot answer. `oid` compares with the integers and not
//! with `numeric` or the floats, and the integers, floats and `numeric` are one family because
//! they all compare with each other — no single tag expresses that overlap, so the pair is asked
//! before the tags now, the way `json`'s is.
//!
//! Measured in `tests/captures/pg19_comparison_matrix.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Every measured pair, in the literal form** — red, and handed over measured.
///
/// 37 of the 2,809 disagree and they are the *other* half of #43's second mechanism: this is the
/// path where `reconcile` retypes a literal, and it does not consult `same_family` at all — it asks
/// `Literal::comparable_with`, which asks `Datum::fits`, which asks `one_representation`. Three
/// gates, none of them the one the column path uses, which is why the same build answers
/// `'x'::citext = 'x'::text` over columns and refuses it over literals.
///
/// The remainder: ten `citext` pairs against the text family, and roughly two dozen where a
/// text-shaped literal is compared against a `tsrange` and this node answers. Adding `Citext` to
/// `one_representation` was tried and moved none of them, so the gate is earlier still and is not
/// found yet — recorded rather than guessed at.
#[test]
#[ignore = "the literal path is the other half of #43: measured at 37 rows, gate not yet located"]
fn every_comparison_pair_agrees_with_postgresql_19() {
    let mut node = parity::Node::new(&[]);
    let capture = include_str!("captures/pg19_comparison_matrix.txt");
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in capture.lines().filter(|line| !line.starts_with('#')) {
        let Some((statement, expected)) = line.split_once('\t') else {
            continue;
        };
        let answered = node.run(statement).is_ok();
        if answered == expected.starts_with('!') {
            wrong.push(format!(
                "{statement}\n  PostgreSQL {} · node {}",
                if expected.starts_with('!') {
                    "refuses"
                } else {
                    "answers"
                },
                if answered { "answers" } else { "refuses" }
            ));
        }
        checked += 1;
    }
    assert!(
        checked > 2_600,
        "only {checked} pairs read; the capture did not load"
    );
    assert!(
        wrong.is_empty(),
        "{} of {checked} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
