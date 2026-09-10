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
/// This is the path where `reconcile` retypes a literal, and it does not consult `same_family` at
/// all: it asks `Literal::comparable_with` → `Datum::fits` → `one_representation`. Three gates,
/// none of them the column path's, which is why the same build answered
/// `'x'::citext = 'x'::text` over columns and refused it over literals until `one_representation`
/// gained the pair — **in the direction the gate actually asks**, which is `citext` *held* against
/// a text-shaped target and not the reverse. The first attempt added the reverse, moved nothing,
/// and was reverted.
///
/// **27 rows remain and they are one family**: a `tsrange` literal compared against a text-shaped
/// literal — `name`, `"char"`, `character`, `character varying`, `xml`, `jsonb`, `bit`, `oidvector`,
/// `lquery`, `void` — where this node answers and a real server refuses. Neither gate fires on
/// them: `Literal::String(_)` is comparable with everything, deliberately, because an untyped
/// literal is supposed to take the other side's type; and the pairwise `same_family` checks want
/// `literal_type` on both sides, which a string literal does not have. So the operand escapes both,
/// which is #43's mechanism in its purest form — the fix is that a *cast* literal should not still
/// be an untyped string by the time it reaches here.
#[test]
#[ignore = "27 rows left, one family: a tsrange literal escapes both gates as an untyped string"]
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
