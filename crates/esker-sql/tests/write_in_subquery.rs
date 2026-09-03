//! `IN (subquery)` in the `WHERE` of a statement that writes — `delete_all` and `update_all`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    types: &[],
    answers: &[
        (
            "DELETE FROM vl_comments WHERE (vl_comments.id, vl_comments.vl_post_id) IN (SELECT vl_comments.id, vl_comments.vl_post_id FROM vl_comments WHERE body = $1)",
            "**A multi-column row constructor**, which is one operand and one column everywhere \
             in this crate: `SubqueryExpr` carries `operand: Option<Box<Expr>>` and `column: \
             Option<(String, ColumnType)>`, both singular, and the comparison under them is \
             between two `Datum`s. Making them plural is a row-comparison unit of its own — the \
             NULL rule for a row is not the NULL rule for a value. **`ActiveRecord` never sends \
             it**: every one of the 39 tests writes the one-column form, which is the line above \
             this one and passes. It is in the capture because it is legal, not because it is \
             used",
        ),
        (
            "DELETE FROM vl_posts WHERE id IN (SELECT NULL::bigint)",
            "**A typed NULL loses its type at lowering** — `NULL::anything` becomes \
             `plan::Literal::Null`, which has none — so the subquery's column is `text` and the \
             comparison is `42883 operator does not exist: bigint = text` where a real server \
             matches nothing and deletes nothing. A refusal rather than a wrong answer, and the \
             fix is a type surface change: `Literal` has no typed-NULL spelling and `Datum::Null` \
             carries no type, so one of them has to learn one (ADR 0033's tier). The rule the \
             corpus is pinning survives it — `NOT IN` over a NULL yields nothing either, the line \
             below",
        ),
        (
            "DELETE FROM vl_posts WHERE id NOT IN (SELECT NULL::bigint)",
            "The same typed NULL, negated — see the line above. Both directions refuse here and \
             both delete nothing there",
        ),
        // **The wake of the three above, not divergences of their own.** A capture is one
        // session in one transaction: a statement this node refuses is a row it did not write,
        // and every count after it is off by exactly that row. `tests/on_conflict.rs` declares a
        // follow-on line for the same reason. Each entry is deleted when the shape above it
        // lands, and rule 2 fails the test if it is not.
        (
            "SELECT count(*) FROM vl_comments",
            "A follow-on of the row constructor above: the `DELETE` it names removed a comment              there and none here",
        ),
        (
            "SELECT id, body FROM vl_comments ORDER BY id",
            "A follow-on of **the row constructor**, which is the one refusal left above it: the \
             `DELETE` removed the comment with `body = 'a'` there and none here, so this line \
             answers with one row too many. The `UPDATE … FROM` two lines up landed \
             (`tests/update_from.rs`) and both sides set `body` to `joined`, which is why the \
             values agree and only the count does not",
        ),
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
