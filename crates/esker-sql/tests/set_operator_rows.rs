//! **`EXCEPT` and `INTERSECT` answer by count, not by membership** — `debts-v1.1.md` #105.
//!
//! The corpus in `tests/set_operators.rs` replays eighteen statements and is enforced from the
//! same commit this file arrives in. This file is the slice's own red test: it refused with
//! `0A000` before the row combination existed and passes after it, so the slice has something
//! that goes red for the reason it is being built — fast, and naming each rule where a corpus
//! failure names a line number. It **overlaps the corpus deliberately**; that is the trade.
//!
//! Every expectation is measured on 19beta1, over `{1,2,2,3}`, `{2,3,3,4}` and `{4}`
//! (`esker-coord/s2-h105.out` and `s2-h105-precedence.out`). Three are things reasoning gets wrong:
//!
//! * **`EXCEPT ALL` is multiset subtraction**, so `1 ; 2` and not `1` — the left holds `2` twice
//!   and the right holds it once, so one survives.
//! * **`INTERSECT ALL` is the minimum**, which on this data is `2 ; 3` and *not* `2 ; 3 ; 3`: the
//!   left holds `3` once even though the right holds it twice.
//! * **`INTERSECT` binds tighter than both `EXCEPT` and `UNION`**, and the third table is what
//!   lets the test say so: over the first two alone, a left-to-right reading answers the same
//!   thing and the assertion would have proved nothing.
//!
//! The two `UNION` assertions are the control: it is the operator that already worked, and they
//! say the change to `exec::query::combine` left it alone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn three_sets() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE sa (id int)",
        "INSERT INTO sa VALUES (1), (2), (2), (3)",
        "CREATE TABLE sb (id int)",
        "INSERT INTO sb VALUES (2), (3), (3), (4)",
        // Only `4`: a value `sb` holds and `sa` does not, which is the whole reason it exists —
        // it makes `sa EXCEPT (sb INTERSECT sc)` and `(sa EXCEPT sb) INTERSECT sc` answer
        // differently, so the precedence test below can tell them apart.
        "CREATE TABLE sc (id int)",
        "INSERT INTO sc VALUES (4)",
    ])
}

#[test]
fn except_subtracts_multiplicities_and_except_all_keeps_the_survivor() {
    let mut node = three_sets();

    assert_eq!(
        node.rows("SELECT id FROM sa EXCEPT SELECT id FROM sb ORDER BY 1"),
        [["1"]]
    );
    // The `2` that survives is the second one: the left has two and the right spends one.
    assert_eq!(
        node.rows("SELECT id FROM sa EXCEPT ALL SELECT id FROM sb ORDER BY 1"),
        [["1"], ["2"]]
    );
    // **`EXCEPT` deduplicates, and the two assertions above cannot show it** — a counterfactual
    // that stopped the deduplication left both of them passing, because the only value surviving
    // `sa EXCEPT sb` is `1`, which `sa` holds once. Against `sc` the whole of `sa` survives, and
    // `sa` holds `2` twice: deduplicating answers three rows, not four. Measured on 19beta1,
    // `esker-coord/s2-h105-except-dedup.out`.
    assert_eq!(
        node.rows("SELECT id FROM sa EXCEPT SELECT id FROM sc ORDER BY 1"),
        [["1"], ["2"], ["3"]],
        "EXCEPT dedups before subtracting; without that this is 1 ; 2 ; 2 ; 3"
    );
    assert_eq!(
        node.rows("SELECT id FROM sa EXCEPT ALL SELECT id FROM sc ORDER BY 1"),
        [["1"], ["2"], ["2"], ["3"]],
        "and ALL keeps the duplicate, which is what says the rule is the quantifier's"
    );
}

#[test]
fn intersect_takes_the_minimum_of_the_two_counts() {
    let mut node = three_sets();

    assert_eq!(
        node.rows("SELECT id FROM sa INTERSECT SELECT id FROM sb ORDER BY 1"),
        [["2"], ["3"]]
    );
    // `min` and not "everything the right side has": the right holds `3` twice, the left once.
    assert_eq!(
        node.rows("SELECT id FROM sa INTERSECT ALL SELECT id FROM sb ORDER BY 1"),
        [["2"], ["3"]]
    );
}

/// **`INTERSECT` binds tighter than `EXCEPT` and than `UNION`** — on data that can tell.
///
/// The first draft of this asserted `a EXCEPT b INTERSECT b` is `1`, which a left-to-right reading
/// answers too: it was named for the thing it could not show. `sc` is what fixes that. Measured on
/// 19beta1 in one `BEGIN … ROLLBACK`, self-cleaning (`esker-coord/s2-h105-precedence.out`):
///
/// ```text
/// sa EXCEPT sb INTERSECT sc        1 ; 2 ; 3
/// sa EXCEPT (sb INTERSECT sc)      1 ; 2 ; 3      <- the bare form is this one
/// (sa EXCEPT sb) INTERSECT sc      (0 rows)       <- what reading left to right would answer
///
/// sa UNION sb INTERSECT sc         1 ; 2 ; 3 ; 4
/// sa UNION (sb INTERSECT sc)       1 ; 2 ; 3 ; 4  <- again the bare form is this one
/// (sa UNION sb) INTERSECT sc       4              <- what reading left to right would answer
/// ```
///
/// **Both parenthesisations of both chains were captured, not derived.** The `UNION` half of this
/// table was reasoned out first and sat under a "measured on 19beta1" heading for ten minutes,
/// which is unreadable: a table that mixes measured rows with reasoned ones tells a reader nothing
/// about which is which. It was captured before it shipped.
#[test]
fn intersect_binds_tighter_than_except_and_union() {
    let mut node = three_sets();

    assert_eq!(
        node.rows(
            "SELECT id FROM sa EXCEPT SELECT id FROM sb \
             INTERSECT SELECT id FROM sc ORDER BY 1"
        ),
        [["1"], ["2"], ["3"]],
        "read left to right this answers no rows at all"
    );
    assert_eq!(
        node.rows(
            "SELECT id FROM sa UNION SELECT id FROM sb \
             INTERSECT SELECT id FROM sc ORDER BY 1"
        ),
        [["1"], ["2"], ["3"], ["4"]],
        "read left to right this answers `4` alone"
    );
    // The `ALL` chain, where multiplicity and precedence meet: `sb INTERSECT ALL sc` is `4`, and
    // `sa` holds no `4`, so every row of `sa` survives with its multiplicity.
    assert_eq!(
        node.rows(
            "SELECT id FROM sa EXCEPT ALL SELECT id FROM sb \
             INTERSECT ALL SELECT id FROM sc ORDER BY 1"
        ),
        [["1"], ["2"], ["2"], ["3"]]
    );
}

/// **The operator takes everything to its left, not just the arm beside it** — the claim
/// `exec::query::combine` is built on, and until now nothing tested it.
///
/// A counterfactual that made `combine` fold only the last arm into the left side went **green**
/// across the whole suite: with two arms `take` and `pop` are the same thing, and precedence is
/// expressed by nesting in the lowering, so `combine` never saw more than one pending node in any
/// test that existed. The shape that tells them apart needs a `UNION ALL` run *before* the
/// operator.
///
/// Measured on 19beta1 (`esker-coord/s2-h105-accumulated-left.out`), and the two readings are
/// three rows against six:
///
/// ```text
/// sa UNION ALL sb EXCEPT sc        1 ; 2 ; 3
/// (sa UNION ALL sb) EXCEPT sc      1 ; 2 ; 3            <- the bare form is this one
/// sa UNION ALL (sb EXCEPT sc)      1 ; 2 ; 2 ; 2 ; 3 ; 3
/// ```
#[test]
fn the_operator_takes_the_whole_accumulated_left_side() {
    let mut node = three_sets();

    assert_eq!(
        node.rows(
            "SELECT id FROM sa UNION ALL SELECT id FROM sb \
             EXCEPT SELECT id FROM sc ORDER BY 1"
        ),
        [["1"], ["2"], ["3"]],
        "folding only the last arm into the left side answers six rows here"
    );
}

#[test]
fn union_still_answers_what_it_answered() {
    let mut node = three_sets();

    assert_eq!(
        node.rows("SELECT id FROM sa UNION SELECT id FROM sb ORDER BY 1"),
        [["1"], ["2"], ["3"], ["4"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM sa UNION ALL SELECT id FROM sb ORDER BY 1"),
        [["1"], ["2"], ["2"], ["2"], ["3"], ["3"], ["3"], ["4"]]
    );
}
