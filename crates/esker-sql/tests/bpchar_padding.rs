//! **A `character(n)`'s trailing blanks: which readers see them and which do not.**
//!
//! `docs/plans/debts-v1.1.md` #31. A `bpchar` is stored blank-padded and printed padded —
//! `SELECT c` over a `character(4)` holding `x` is `x   ` on both servers — and that is where the
//! agreement ended. The row was found in `tests/typmod.rs` behind a mis-parse (#20) and four more
//! readers turned up while measuring #28.
//!
//! # It is not one rule about `text`
//!
//! ```text
//! the `text` coercion trims       '[' || c || ']'   [x]      c::text and c::varchar too
//! ... so a text function trims    upper(c)          X        substr, replace, btrim, strpos
//! a comparison ignores them       c = 'x'           t        and c = 'x   ' is t as well
//! `length` has its own            length(c)         1
//! `octet_length` has its own      octet_length(c)   4        the storage, not the value
//! `concat` takes `any`            concat(c, 'z')    x   z    the output function, so padded
//! `LIKE` has its own              c LIKE 'x'        f        the padded value is matched
//! ```
//!
//! `octet_length`, `concat` and `LIKE` keep the padding, which is why they are in the corpus: a
//! fix that trimmed everywhere would break them and a corpus without them would not say so.
//! `c LIKE 'x'` being **false** while `c = 'x'` is **true** is the sharpest pair in the file.

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
fn every_bpchar_padding_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_bpchar_padding.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus is not being read"
    );
}
