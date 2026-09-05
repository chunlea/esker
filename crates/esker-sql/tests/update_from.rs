//! `UPDATE … FROM` with a self-alias — what a **joined** `update_all` sends.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "DELETE FROM vl_comments a USING vl_posts p WHERE p.id = a.vl_post_id AND p.title = 'x'",
        "**`DELETE … USING` is the same mechanism spelled for the other verb**, and it is the \
         other verb that makes it a unit of its own: `plan::Delete` needs the three fields \
         `plan::Update` gained here, and `lower_delete` refuses the clause by name. It is in this \
         capture so that the day it is needed its answer is already measured — `ActiveRecord` \
         sends the subquery form for `delete_all` (`tests/write_in_subquery.rs`), not this one.",
        "UNMEASURED",
    )],
};

#[test]
fn every_update_from_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_update_from.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}
