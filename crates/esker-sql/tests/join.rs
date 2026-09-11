//! Contract C3 for `INNER` and `LEFT JOIN`, `ON` and `USING`.
//!
//! Thirty-five statements put to a real PostgreSQL 19beta1 in one session and replayed against one
//! node. The trap the file exists for is at the top of the corpus: the same predicate means
//! different things in an `ON` and in a `WHERE`, and an implementation that applied the left-join
//! extension after the filter would answer the second for both and lose rows with no error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What the bespoke tests start from — the same two tables the corpus uses.
const FIXTURE: &[&str] = &[
    "CREATE TABLE l (id int8 PRIMARY KEY, n text, k int8)",
    "CREATE TABLE r (id int8 PRIMARY KEY, m text, flag bool)",
    "INSERT INTO l VALUES (1, 'one', 1), (2, 'two', 9), (3, 'three', NULL)",
    "INSERT INTO r VALUES (1, 'uno', true), (3, 'tres', false), (4, 'cuatro', true)",
];

#[test]
fn every_join_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_join.txt"),
        CORPUS_FIXTURE,
        &parity::Divergences::default(),
    );
    assert!(
        checked > 33,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The trap, on its own, because it is the one thing about a left join that is easy to get wrong
/// and impossible to notice: the rows it loses are the rows the user asked the join to keep.
///
/// The corpus holds both queries too. This says what they mean.
#[test]
fn a_condition_in_on_keeps_the_row_and_the_same_one_in_where_removes_it() {
    let mut node = parity::Node::new(FIXTURE);

    // In the `ON`: the pair is refused, so the left row survives NULL-extended. All three.
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l LEFT JOIN r ON l.id = r.id AND r.flag ORDER BY l.id"),
        [["1", "1"], ["2", "\\N"], ["3", "\\N"]]
    );
    // In the `WHERE`: it runs over the already-extended row, and NULL is not true.
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l LEFT JOIN r ON l.id = r.id WHERE r.flag ORDER BY l.id"),
        [["1", "1"]]
    );
}

/// `ON false` keeps every left row and pairs none of them, and `ON true` is the cross product —
/// the two ends of the same rule, and the shapes where an implementation that special-cased
/// "matched nothing" by looking at the *condition* rather than at the rows would go wrong.
#[test]
fn a_left_join_that_matches_nothing_still_returns_every_left_row() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l LEFT JOIN r ON false ORDER BY l.id"),
        [["1", "\\N"], ["2", "\\N"], ["3", "\\N"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM l LEFT JOIN r ON true"),
        [["9"]]
    );
}

/// A NULL join key matches nothing on either side of the join, so a left join keeps the row and an
/// inner join drops it. `NULL = 1` is unknown, and unknown keeps no pair.
///
/// It matters here because the probe path builds a **key** out of the outer value, and a key
/// cannot hold a NULL — so three-valued logic has to be applied before the read rather than after,
/// and the left-join extension has to happen on that path too.
#[test]
fn a_null_key_matches_nothing_and_a_left_join_keeps_it_anyway() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l LEFT JOIN r ON l.k = r.id ORDER BY l.id"),
        [["1", "1"], ["2", "\\N"], ["3", "\\N"]]
    );
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l JOIN r ON l.k = r.id ORDER BY l.id"),
        [["1", "1"]]
    );
}

/// `USING` merges: one `id`, at the front, and a bare reference to it is unambiguous where the
/// same query written with `ON` is `42702`.
#[test]
fn using_merges_the_column_and_on_does_not() {
    let mut node = parity::Node::new(FIXTURE);

    // Five columns, not six, and `id` leads them.
    assert_eq!(
        node.rows("SELECT * FROM l JOIN r USING (id) ORDER BY id"),
        [
            ["1", "one", "1", "uno", "t"],
            ["3", "three", "\\N", "tres", "f"]
        ]
    );
    assert_eq!(
        node.rows("SELECT id FROM l JOIN r USING (id) ORDER BY id"),
        [["1"], ["3"]]
    );

    let error = node
        .run("SELECT id FROM l JOIN r ON l.id = r.id ORDER BY id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::AMBIGUOUS_COLUMN, "{error}");
}

/// The merged column of a **left** join takes the left side's value, so an unmatched row's `id` is
/// the one it always had rather than the NULL the right side contributed.
///
/// PostgreSQL defines it as `COALESCE(l.id, r.id)`; there is no `COALESCE` in this crate and none
/// is needed, because for an inner join the two are equal by the condition and for a left join the
/// right one is either equal or NULL. A `RIGHT` or `FULL` join would make a third case, and there
/// is none here to make one.
#[test]
fn the_merged_column_of_a_left_join_is_the_left_sides_value() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT id FROM l LEFT JOIN r USING (id) ORDER BY id"),
        [["1"], ["2"], ["3"]]
    );
    // And each side is still reachable under its own name.
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l LEFT JOIN r USING (id) ORDER BY l.id"),
        [["1", "1"], ["2", "\\N"], ["3", "3"]]
    );
    assert_eq!(
        node.rows("SELECT * FROM l LEFT JOIN r USING (id) ORDER BY id"),
        [
            ["1", "one", "1", "uno", "t"],
            ["2", "two", "9", "\\N", "\\N"],
            ["3", "three", "\\N", "tres", "f"]
        ]
    );
}

/// A `USING` column one side lacks names **which** side, because a typo and a join between the
/// wrong two tables look identical without it.
#[test]
fn a_using_column_names_the_side_that_lacks_it() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, expected) in [
        (
            "SELECT id FROM l JOIN r USING (nope)",
            "column \"nope\" specified in USING clause does not exist in left table",
        ),
        (
            "SELECT id FROM l JOIN r USING (m)",
            "column \"m\" specified in USING clause does not exist in left table",
        ),
        (
            "SELECT id FROM l JOIN r USING (id, n)",
            "column \"n\" specified in USING clause does not exist in right table",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::UNDEFINED_COLUMN,
            "{sql} -> {error}"
        );
        assert_eq!(error.to_string(), expected, "{sql}");
    }
}

/// A left join **may not swap which side drives the loop**, however much cheaper the other order
/// would be. An inner join is commutative and the planner is free to choose; a left join is not,
/// because which side keeps its unmatched rows is the whole of what it means.
///
/// Asserted through `EXPLAIN`, because the wrong choice here is not slower — it answers a
/// `RIGHT JOIN`: the same rows, in the wrong places, with nothing to say so.
#[test]
fn a_left_join_drives_from_the_left_even_when_the_other_way_would_probe() {
    let mut node = parity::Node::new(FIXTURE);
    // `r` has the primary key the condition names, so an inner join would probe it and keep `l`
    // outside; that is exactly the choice a left join may not make in reverse.
    let inner = node.rows("EXPLAIN SELECT l.id FROM l JOIN r ON l.k = r.id");
    let left = node.rows("EXPLAIN SELECT l.id FROM l LEFT JOIN r ON l.k = r.id");
    assert!(
        inner.iter().any(|line| line[0].contains("Point Get on r")),
        "an inner join probes r: {inner:?}"
    );
    assert!(
        left.iter().any(|line| line[0].contains("Seq Scan on l")),
        "a left join still drives from l: {left:?}"
    );
    assert!(
        !left.iter().any(|line| line[0].contains("Seq Scan on r")),
        "and it does not drive from r: {left:?}"
    );
}

/// **An extra conjunct in the `ON` does not cost the probe.** `ON l.k = r.id` and
/// `ON l.k = r.id AND r.flag` seek the same inner row; the second merely has something more to
/// check once it is found. A planner matching only a bare equality answers the first with one key
/// read and the *more* selective second by materialising the whole of `r` for every row of `l`.
///
/// Asserted through `EXPLAIN`, because this failure is invisible in every answer: both plans
/// return the same rows and only the cost differs. It is what regressed when the planner learned
/// to move a `WHERE` conjunct onto a join (#54) — the conjunct arrived, the probe left, and no
/// answer-checking test could see it.
#[test]
fn an_extra_condition_in_the_on_does_not_cost_the_probe() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "EXPLAIN SELECT l.id FROM l JOIN r ON l.k = r.id AND r.flag",
        // The same two conjuncts the other way round, because "the first one" is an
        // implementation detail and which of them is the key must not be.
        "EXPLAIN SELECT l.id FROM l JOIN r ON r.flag AND l.k = r.id",
        "EXPLAIN SELECT l.id FROM l LEFT JOIN r ON l.k = r.id AND r.flag",
        // A comma join has no `ON` to lose, and #54 synthesises one out of the `WHERE` — so this
        // shape *gains* a probe it never had, which is the same rule read the other way round.
        "EXPLAIN SELECT l.id FROM l, r WHERE l.k = r.id AND r.flag",
    ] {
        let plan = node.rows(sql);
        assert!(
            plan.iter().any(|line| line[0].contains("Point Get on r")),
            "{sql} materialised r instead of probing it: {plan:?}"
        );
    }
    // And the conjunct the probe did not express is still applied to the pair it built: `r`'s
    // third row has `flag = false`, so the pair the key read found is refused.
    assert_eq!(
        node.rows("SELECT l.id, r.id FROM l JOIN r ON l.id = r.id AND r.flag ORDER BY l.id"),
        [["1", "1"]]
    );
}
