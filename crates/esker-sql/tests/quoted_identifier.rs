//! **How a column's name is quoted in a printed expression**, across all five readers.
//!
//! The rule is already measured and already implemented: `catalog::quote_identifier` carries
//! PostgreSQL 19's own `pg_get_keywords()` answer — every keyword whose `quote_ident(word) <> word`
//! — and the pair that makes it a list rather than a guess is `value` and `name`, which are
//! keywords and print **bare**. So this file is not about the rule. It is about **who calls it**.
//!
//! ```text
//! "primary"  reserved                        quoted
//! "select"   reserved                        quoted
//! "Foo"      not all-lower-case              quoted
//! "a b"      a character a name cannot hold  quoted
//! value      a keyword quote_ident leaves    bare
//! name       the same                        bare
//! ```
//!
//! The five are `pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_expr(indexprs)`,
//! `pg_get_expr(indpred)` and `pg_get_expr(adbin)` — the same five this node routes through one
//! deparser, so one call in one place is all of it. **Two of the five printed the name bare and
//! three did not**, which is the shape worth recording: the rule was implemented, the readers were
//! already routed through one deparser, and it still reached only some of them. The **view** is asked a narrower
//! question (`strpos(pg_get_viewdef(...), '"primary"') > 0`) because its layout is a standing
//! divergence of its own and a whole-text comparison would swallow the quoting question.

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
fn every_printed_name_is_quoted_the_way_postgresql_19_quotes_it() {
    let checked = parity::replay(
        include_str!("corpus/pg19_quoted_identifier.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus is not being read"
    );
}
