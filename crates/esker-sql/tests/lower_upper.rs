//! `lower` and `upper` — the two scalar functions `schema.rb` indexes on.
//!
//! **This capture sat in the repo replayed by nothing.** `corpus/pg19_lower.txt` has been here
//! since the scalar-function unit and no test read it; four corpora were in that state, and this
//! one is the second to be picked up. A capture nobody replays is a measurement nobody is holding
//! the node to.
//!
//! It is replayable exactly because of what it is not: **28 statements and no transaction** — no
//! `BEGIN`, no savepoints — so a refusal here aborts nothing and cannot hide the file after it.
//! That is the property `corpus/pg19_do_block.txt` lacks, which is why that one still cannot be
//! replayed and this one can.
//!
//! What the capture pins, in its own words:
//!
//! * **the case mapping is full Unicode** — `upper('àéî')` is `ÀÉÎ`, which is the line that decides
//!   whether these functions can exist here at all. A byte-wise implementation passes every ASCII
//!   row and fails that one;
//! * **`42883` has two DETAILs** — the wrong argument *type* and the wrong argument *count* are
//!   different conditions, and PostgreSQL says so with different sentences.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **One row, and it is not about `lower`.** Both refuse with `42883`, both give the same
    // DETAIL and HINT, and they name a different integer: PostgreSQL says `lower(integer)` because
    // a bare `1` is an `int4` there, and this node says `lower(bigint)` because it types an
    // unsuffixed integer literal as `int8`. A divergence of the *literal*, which every type's
    // `= 1` says — `tests/uuid.rs` carries the same sentence for `uuid_cmp`.
    answers: &[(
        "SELECT lower(1)",
        "`lower(bigint)` where PostgreSQL says `lower(integer)`: an unsuffixed integer literal is \
         an `int8` here and an `int4` there. The refusal, its DETAIL and its HINT all agree; only \
         the literal's type name differs",
    )],
};

#[test]
fn every_lower_and_upper_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_lower.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 8,
        "only {checked} statements ran; the corpus did not load"
    );
}
