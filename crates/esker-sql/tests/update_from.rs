//! `UPDATE … FROM` with a self-alias — what a **joined** `update_all` sends.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "UPDATE vl_comments a SET body = c.body || '!' FROM vl_comments c WHERE c.id = a.id",
            "**`||` is a shape of its own, not a corner of this one.** The operator is not in \
             `plan::BinaryOp` at all, and its rules are its own: PostgreSQL's `||` is NULL when \
             either side is, where the `concat` function this node *does* have skips NULLs, and \
             `'a' || 1` resolves through `anynonarray` where `1 || 2` is `42883 operator is not \
             unique`. None of that is measured here, so it is refused by name rather than \
             guessed at — and the rule this line was carrying, that the `FROM` scan sees the \
             pre-statement rows, is pinned by `s10b` in operators the node has.",
        ),
        (
            "DELETE FROM vl_comments a USING vl_posts p WHERE p.id = a.vl_post_id AND p.title = 'x'",
            "**`DELETE … USING` is the same mechanism spelled for the other verb**, and it is the \
         other verb that makes it a unit of its own: `plan::Delete` needs the three fields \
         `plan::Update` gained here, and `lower_delete` refuses the clause by name. It is in this \
         capture so that the day it is needed its answer is already measured — `ActiveRecord` \
         sends the subquery form for `delete_all` (`tests/write_in_subquery.rs`), not this one.",
        ),
    ],
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
