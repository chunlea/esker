//! **`EXCEPT` and `INTERSECT`, the two set operators this node does not have** — `debts-v1.1.md`
//! #105.
//!
//! `UNION` is implemented and these two are `0A000`, so every statement this corpus replays is one
//! the node refuses outright today. The file is a capture of what it must answer, not of what it
//! answers, and it is **`#[ignore]`d until #105 is paid**. Ignored rather than declared as a
//! divergence: a divergence says "this node deliberately answers differently", and eighteen
//! statements that are all one unimplemented feature are not eighteen decisions. The ignore names
//! the row, so the day the feature lands the attribute comes off and the whole capture starts
//! being enforced at once.
//!
//! Three things the capture settles that reasoning would get wrong, all measured on 19beta1
//! (`esker-coord/s2-h105.out`):
//!
//! * **`EXCEPT ALL` is multiset subtraction.** The left side holds `2` twice and the right holds it
//!   once, so one `2` survives; `EXCEPT` dedups first and answers `1` alone. `INTERSECT ALL` is the
//!   same arithmetic from the other side — each value's count is the *minimum* of the two sides.
//! * **`INTERSECT` binds tighter than `EXCEPT`**: `a EXCEPT b INTERSECT b` is
//!   `a EXCEPT (b INTERSECT b)`. On this data a left-to-right reading happens to agree, which is
//!   exactly why the precedence has to come from the grammar and not from an example.
//! * **The arms resolve by the rule `UNION` already uses** — `integer INTERSECT numeric` settles on
//!   `numeric` — so the type unification is reusable and the row combination is what is missing.
//!
//! Both refusals name the operator (`each EXCEPT query must have the same number of columns`,
//! `INTERSECT types integer and text cannot be matched`), where this node's `SetOperationTypes`
//! hard-codes `UNION` — correct only while the other two never reach it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// **Empty, and the test is ignored instead.**
///
/// The two `EXCEPT`/`INTERSECT` statements that *are* declared as divergences live in
/// `tests/enum_unknown_literal.rs`, where they sit among statements that do pass and would
/// otherwise stop the file. Here every statement is the same missing feature, so declaring each one
/// would be eighteen copies of a single fact — and a declared divergence that is really a debt
/// hides the debt behind a green run.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// Every statement of the capture, replayed.
///
/// The count is asserted because a corpus that fails to load replays nothing and passes: "all zero
/// statements agreed" is the shape of a green run that measured nothing at all.
#[test]
#[ignore = "#105: EXCEPT and INTERSECT are 0A000 on this node; the corpus is what they must answer"]
fn every_set_operator_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_set_operators.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 17,
        "only {checked} statements ran; the corpus did not load"
    );
}
