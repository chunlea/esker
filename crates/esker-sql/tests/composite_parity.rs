//! The composite type replayed against what PostgreSQL 19beta1 answered.
//!
//! The capture is `captures/pg19_composite.txt` and was taken **before** the parser was written,
//! which is what `docs/plans/composite-type.md`'s risk section asked for — and it earned its
//! keep: whitespace inside the parens, `NULL` as a four-character string, and an empty *quoted*
//! field being the empty string are all rules a reader would have guessed wrong.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own type and table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **Field access is deliberately not built** — the approved scope says so, and the four
        // Rails tests do not need it: `composite_test.rb` splits the string in Ruby. These two
        // rows are in the corpus because they are the *evidence* for the rule the rest of the file
        // rests on — a NULL field and an empty-string field are different values — and that rule
        // is asserted directly in `tests/composite_text.rs` where no `(x).f` is needed.
        (
            "SELECT 'i17', ('(,x)'::full_address).city IS NULL;",
            "`(x).field` is not built; the NULL-vs-empty rule it demonstrates is asserted on \
             `composite::parse` instead",
            "pg19_composite.txt:79",
        ),
        (
            "SELECT 'i18', ('(\"\",x)'::full_address).city IS NULL;",
            "the other half of the same pair, and the same reason",
            "pg19_composite.txt:82",
        ),
    ],
};

#[test]
fn every_composite_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_composite.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
