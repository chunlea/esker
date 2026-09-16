//! **`EXCEPT` and `INTERSECT`, enforced against 19beta1** — `debts-v1.1.md` #105.
//!
//! While `UNION` was the only operator this node had, every statement here was one it refused
//! outright, so the file was a capture of what it *must* answer and the runner was `#[ignore]`d
//! with #105's number in the attribute — ignored rather than declared as eighteen divergences,
//! because a divergence says "this node deliberately answers differently" and eighteen statements
//! that are one unimplemented feature are not eighteen decisions. **#105 landed, so the attribute
//! came off and all eighteen are enforced at once**, which is what the ignore was written to make
//! happen.
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

/// **Empty, and now it is a claim rather than a placeholder.**
///
/// It was empty because the runner was ignored instead: eighteen statements that are one missing
/// feature are not eighteen decisions, and a declared divergence that is really a debt hides the
/// debt behind a green run. Since #105 it is empty for the opposite reason — every statement here
/// is one this node is expected to answer exactly as 19beta1 does. The two `EXCEPT`/`INTERSECT`
/// rows that *were* declared, in `tests/enum_unknown_literal.rs`, were deleted in the same commit.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// Every statement of the capture, replayed.
///
/// The count is asserted because a corpus that fails to load replays nothing and passes: "all zero
/// statements agreed" is the shape of a green run that measured nothing at all.
#[test]
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
