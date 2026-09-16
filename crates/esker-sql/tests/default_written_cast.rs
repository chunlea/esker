//! **What `pg_get_expr` prints for a column default depends on how the default was *written*.**
//!
//! `tests/captures/pg19_typmod_default.txt` already measured which *types* carry their modifier
//! into a rendered default — `interval` and nothing else — but every row of that matrix is a bare
//! literal. This file measures the other axis: **one column type, three spellings.** On a single
//! `interval(3)` column, 19beta1 answers three different strings for `DEFAULT '3 years'`,
//! `DEFAULT '3 years'::interval` and `DEFAULT '3 years'::interval(3)`, because the cast it prints
//! is the one that is *in the stored tree* and not the column's.
//!
//! The defect this was opened for is the third of those: a written `::interval(3)` printed
//! `::interval` here, because the printer for a literal default named its type through a helper
//! that drops the typmod. One path over, the *expression* deparser already prints a written cast
//! with its modifier — `tests/corpus/pg19_deparse_census.txt` pins `(v)::character varying(5)` and
//! `(n)::numeric(10,2)` — so this was one rule with two implementations and only one of them had
//! it.
//!
//! **Two things about the capture that are not tidiness.** Its table is created and dropped
//! *outside* a transaction, and the drop is a second pass: the declared types come from a `\gdesc`
//! pass that runs in a fresh session **after** the whole script, so anything the script destroys —
//! by `ROLLBACK` or by `DROP` — is invisible to it and the types field comes back empty. And
//! `interval hour to second(3)` is deliberately absent: its default reads
//! `'01:02:03.457'::interval hour to second(3)` and this node has no representation for an
//! interval's field list at all, so including it would make this file red for a different defect.
//! It is measured in `esker-coord/s2-h103b.out` and carried by its own row.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// **Empty, and that is the claim.** Every statement in this corpus is one this node is expected
/// to answer exactly as 19beta1 does. A row here would say "this node deliberately answers
/// differently", which is not what a defect is — the difference is the bug, so it belongs in a red
/// test rather than in a declared divergence.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// Every statement of the capture, replayed.
///
/// The count is asserted because a corpus that fails to load replays nothing and passes: "all zero
/// statements agreed" is the shape of a green run that measured nothing at all.
#[test]
fn every_written_cast_default_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_default_written_cast.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 4,
        "only {checked} statements ran; the corpus did not load"
    );
}
