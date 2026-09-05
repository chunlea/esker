//! `COLLATE`, replayed against what PostgreSQL 19beta1 answered.
//!
//! The decision is [ADR
//! 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md): `C` and `POSIX`
//! are byte order under two names, a memcomparable key is already in byte order, and every other
//! name is refused rather than accepted and sorted differently from what it asked for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'q8', plain FROM co ORDER BY plain;",
            // **The declared divergence this whole unit is about.** PostgreSQL's default collation
            // is a locale, so a bare `ORDER BY` there is `a,A,b,B`; a memcomparable key sorts by
            // bytes and gives `A,B,a,b`. Which is why `COLLATE "C"` is *answerable* here and the
            // default is not — the clause asks for the order this node already has (ADR 0076).
            "the default collation is a locale on a real server and byte order here",
            "pg19_collation.txt:26",
        ),
        (
            "SELECT 'q14', n COLLATE \"C\" FROM co;",
            // **The type check reaches as far as the type does.** A `COLLATE` on a literal is
            // checked where it is lowered — `SELECT 1 COLLATE \"C\"` two rows below is the same
            // `42804` a real server gives — but a *column* has no type until the executor resolves
            // it against a scope, two layers past where the clause is read. So this one is
            // accepted and answers rows.
            //
            // Closing it means carrying the clause into the plan as a node resolution either
            // collapses or refuses: it is a no-op for every collatable type, so the node would
            // exist only to be deleted, and every match over `plan::Expr` would have to grow an
            // arm for it. Left as the narrower gap, declared rather than found later.
            "a COLLATE on a non-collatable column is accepted; the type is not known at lowering",
            "pg19_collation.txt:57",
        ),
    ],
};

#[test]
fn every_collation_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_collation.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
