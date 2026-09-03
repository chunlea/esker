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
            "UPDATE \"vl_comments\" \"__active_record_update_alias\" SET \"body\" = $1 FROM \"vl_comments\" INNER JOIN \"vl_posts\" ON \"vl_posts\".\"id\" = \"vl_comments\".\"vl_post_id\" WHERE \"vl_comments\".\"id\" = \"__active_record_update_alias\".\"id\"",
            "**`UPDATE … FROM` is a different statement, not this one with a subquery in it**, and \
             the capture's own header says so: for a *joined* `update_all` `ActiveRecord` does not \
             wrap the selection in a subquery at all — it aliases the target, puts the join in a \
             `FROM`, and ties the two together in the `WHERE`. `plan::Update` has a table, \
             assignments, a filter and a `RETURNING`; it has no `FROM`, no joins and no alias for \
             the table it writes, and every one of those is a field. Its own unit, and it is in \
             the same test files as the shape above",
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
            "A follow-on of `UPDATE … FROM` above: it set `body` to `joined` there and left it              alone here",
        ),
        (
            "SELECT count(*) FROM vl_posts",
            "A follow-on of `UPDATE … FROM` above, five lines over. The `DELETE` after it selects              posts by `body = 'joined'`, which no row has here because the `UPDATE` did not run —              so one post outlives the rest of the file. The **rule** each of those lines is              pinning is checked elsewhere and holds: an empty subquery deletes nothing              (`WHERE 1=0`, two lines that agree), and `NOT IN` over a real value set does delete              (`title = 'n1'`, the last statement, which agrees on `min(title)`)",
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
