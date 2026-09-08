//! `IN (subquery)` in the `WHERE` of a statement that writes — `delete_all` and `update_all`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    types: &[],
    answers: &[
        // **Three entries left here together**, and rule 2 named all three at once: the row
        // constructor `(a, b) IN (SELECT a, b …)` runs now (`tests/row_value_in_subquery.rs`), so
        // the `DELETE` removes its comment on both sides and the two counts that followed it agree
        // as well. They were written down as follow-ons of one refusal, and they went with it.
    ],
};

#[test]
fn every_write_in_subquery_answer_is_postgresql_19_s() {
    let checked = bind::replay(
        include_str!("corpus/pg19_write_in_subquery.txt"),
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
