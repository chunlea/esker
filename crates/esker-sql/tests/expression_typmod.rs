//! **Which expressions carry a typmod into the `RowDescription`.**
//!
//! `docs/plans/debts-v1.1.md` #28. A `character(4)` column is `character(4)` to a client, and an
//! expression over it was `bpchar` here whatever it was there. The row was found on
//! `nullif(c, 'x')` and a second reader turned up while closing #20: five statements in
//! `tests/numeric.rs` were declared because `1.0::numeric(10,3)` described itself as bare
//! `numeric`.
//!
//! The row was sized "medium — a type-surface change, the same shape as #19, and touches every
//! reader of a `RowDescription`". It is neither. `OutputColumn` already carries a `typmod` and
//! `plan::Expr::Cast` already holds the one it was written with; what was missing is the rule
//! saying which expressions pass one along, and that rule lives in one function.
//!
//! # The rule, measured
//!
//! ```text
//! a plain column           its own                  SELECT c            character(4)
//! a cast                   the cast's own           c::char(2)          character(2)
//! NULLIF                   its left argument's      nullif(c, 'x')      character(4)
//!   ... unless the comparison changed the type:     nullif(v, 'x')      text
//! COALESCE/GREATEST/LEAST  every input's, if they all agree; else none
//! CASE                     none, always
//! everything else          none                     c || 'x', min(c), n + 1
//! ```
//!
//! **Two oracles disagree about `CASE`**, and the corpus follows the one a client reads:
//! `CREATE TABLE AS` gives its column the *first branch's* modifier, `\gdesc` on the same
//! expression gives none. The `RowDescription` is what this row is about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Nothing.** Four rows stood here and none of them was a typmod: they were a
    // `character(n)`'s trailing blanks reached by a comparison, an operator and two functions,
    // which is `docs/plans/debts-v1.1.md` #31 and closed by `exec::query::read_as_text` in the
    // commit after this file landed.
    answers: &[],
};

#[test]
fn every_declared_typmod_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_expression_typmod.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus is not being read"
    );
}
