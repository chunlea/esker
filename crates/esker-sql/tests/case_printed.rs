//! **How a `CASE` is printed back**, in both of `pg_get_constraintdef`'s forms.
//!
//! Run 108's first new red. `check_constraint_test#test_check_constraints` adds
//! `CHECK (CASE WHEN price IS NOT NULL THEN true ELSE false END)` and asserts the read-back
//! **includes `WHEN price IS NOT NULL`** — unparenthesised. This node answered
//! `WHEN (price IS NOT NULL)`, and the first thing the measurement said is that the node's answer
//! *is* PostgreSQL's — of the other form:
//!
//! ```text
//! plain    CHECK (⏎CASE⏎    WHEN (price IS NOT NULL) THEN true⏎    ELSE false⏎END)
//! pretty   CHECK (⏎CASE⏎    WHEN price IS NOT NULL THEN true⏎    ELSE false⏎END)
//! ```
//!
//! The plain form parenthesises the `WHEN` condition, the `THEN` and the `ELSE`; the pretty form
//! parenthesises none of them, and a bare column is the one operand they agree on. `ActiveRecord`
//! reads the pretty one.
//!
//! This node stores text where a real server stores a tree, so the first attempt was to emit the
//! pretty shape and declare the plain one. **That was wrong and a test said so**:
//! `index_deparse.rs` already pins the *plain* shape for an index key, measured, so choosing one
//! shape trades one reader for another. Both are rendered from the one stored text instead — the
//! plain shape is stored, and `catalog::pretty_case` strips at the single reader that asks for the
//! other. Nothing is declared below.
//!
//! `pretty_case` is deliberately **not** a general parenthesis remover: it is the inverse of one
//! emitter, matching the layout `deparse`'s `Case` arm writes, line by line.
//!
//! The regression is `76b90833`'s, which made a `CHECK` the fifth reader routed through the
//! deparser: before it, the written text was stored, and the written text had no parentheses in it
//! either.
//!
//! Two more defects surfaced while fixing it, both found by this corpus rather than by the suite:
//! the chain scanner split on the `AND` **inside** a `CASE`'s `WHEN`, turning `c3` into
//! `CHECK (((CASE WHEN a) AND (b THEN true ELSE false END)))` — a `CHECK` that no longer parses —
//! and the plain form's outer pair was decided by whether the body *starts* with `CASE`, where
//! `CASE … END > 0` is a comparison and takes the pair after all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn a_case_is_printed_the_way_postgresql_19_prints_it() {
    let checked = parity::replay(
        include_str!("corpus/pg19_case_printed.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 8,
        "only {checked} statements ran; the corpus is not being read"
    );
}
