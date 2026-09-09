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
    // **Four, and none of them is a typmod.** The type agrees in every row of this file now;
    // what is left is a `character(n)`'s trailing blanks, which is `docs/plans/debts-v1.1.md` #31
    // and a different rule reached by four different readers.
    answers: &[
        (
            "SELECT nullif(c, 'x') FROM g1tm",
            "**The type agrees now; the value is `docs/plans/debts-v1.1.md` #31.** `character(4)` on both sides — that is this unit — and PostgreSQL answers NULL where this node answers `x   `, because a `bpchar` comparison **ignores trailing blanks**: the padded `x   ` equals `'x'` there, so `NULLIF` returns nothing. Here they are two different strings. Same rule as the three rows below, reached through a comparison instead of a function.",
            "pg19_expression_typmod.txt:56",
        ),
        (
            "SELECT c || 'x' FROM g1tm",
            "**#31.** `xx` there, `x   x` here. A `bpchar` is stored blank-padded and printed padded — `SELECT c` is `x   ` on both — but concatenation coerces to `text` and the coercion right-trims. This node carries the padding through.",
            "pg19_expression_typmod.txt:70",
        ),
        (
            "SELECT length(c) FROM g1tm",
            "**#31.** `1` there, `4` here: `length` over a `character(n)` ignores the padding, which is the same rule as the concatenation above seen from the other side — the value is `x`, the storage is `x   `.",
            "pg19_expression_typmod.txt:71",
        ),
        (
            "SELECT upper(c) FROM g1tm",
            "**#31.** `X` there, `X   ` here — a text function reads the `bpchar` as `text`, which right-trims first. Three functions, one rule, and `tests/typmod.rs` carries the row that found it.",
            "pg19_expression_typmod.txt:75",
        ),
    ],
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
