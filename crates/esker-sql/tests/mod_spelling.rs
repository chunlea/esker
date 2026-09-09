//! **`mod(a, b)` prints as `mod`, and `a % b` prints as `%`** — two spellings of one remainder.
//!
//! Run 108's second new red. `postgresql_adapter_test#test_expression_index` builds
//! `add_index "ex", "mod(id, 10), abs(number)"` and asserts `index.columns` **equals that string**;
//! this node answered `id % 10`.
//!
//! `parse::lower` rewrote the call into the arithmetic node on purpose — PostgreSQL's `%` for
//! `int8` is `int8mod`, the same C function `mod()` calls, so the rewrite bought the evaluation,
//! the tests and the immutability for free. What it threw away is the **spelling**, and
//! `pg_get_indexdef` prints the node the tree holds. On a real server they are two nodes:
//!
//! ```text
//! mod(id, 10)   mod(id, 10)              whole, per column, plain and pretty — all four agree
//! id % 10       (id % 10)                the operator keeps its pair
//! mod(big, 10)  mod(big, (10)::bigint)   the arguments take the common type
//! ```
//!
//! So the call keeps its own node here too and the evaluator delegates: one remainder, two
//! spellings, which is what `%` and `mod` are on a real server.
//!
//! The corpus's last row is the **values**, in both spellings and both sign combinations, because
//! a node that prints one thing and computes another would pass every row above it.

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
fn mod_keeps_its_spelling_in_every_reader() {
    let checked = parity::replay(
        include_str!("corpus/pg19_mod_spelling.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus is not being read"
    );
}
