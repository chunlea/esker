//! **A domain survives `array_agg` and does not survive `min`/`max`** — `debts-v1.1.md` #106.
//!
//! Two corpora already cover the two axes and neither covers the pair: `pg19_domain.txt` and
//! `pg19_domain_schema.txt` never aggregate, and the twenty-odd files that name `array_agg` never
//! declare a domain. That cross product is where this defect lives, which is where the last one
//! lived too — the capture for #103 found its defect the same way, by varying an axis nobody had
//! varied.
//!
//! **The three answers are different and all three are right**, which is what makes this a rule
//! rather than a coincidence: `min(s)` is `text`, because the aggregate resolves to the base type's
//! operator family; `array_agg(s)` is `d[]`, because it compares nothing and collects; and
//! `array_agg(t)` over a plain `text` column is `text[]`, the control that says the divergence is
//! about the *domain* and not about `array_agg`. A fix that made the first two agree would break a
//! line to pay a line.
//!
//! `array_agg(DISTINCT s)` is `d[]` too, although `DISTINCT` does compare — the comparison is
//! inside the aggregate, and the type it answers is still an array of what it collected.
//!
//! Measured 2026-09-16, `esker-coord/s2-h106.out`, created and dropped outside a transaction with
//! the drop as a second pass, so the declared types survive the `\gdesc` pass that runs afterwards
//! in a fresh session; its own last line counts the domain back to zero.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus declares its own domain and table.
const CORPUS_FIXTURE: &[&str] = &[];

/// **Empty, and that is the claim.** Every statement here is one this node is expected to answer
/// exactly as 19beta1 does. An entry would say "this node deliberately answers differently", which
/// is not what a defect is — the difference is the bug, so it belongs in a red test rather than in
/// a declared exception.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// Every statement of the capture, replayed.
///
/// The count is asserted because a corpus that fails to load replays nothing and passes: "all zero
/// statements agreed" is the shape of a green run that measured nothing at all.
#[test]
fn every_domain_aggregate_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_domain_aggregate.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}
