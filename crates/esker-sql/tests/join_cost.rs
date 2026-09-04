//! A materialised join costs the rows it pairs, not the cross product it could.
//!
//! Run 53's runner caught the node holding a full core for its whole 120 s watchdog, still going
//! after the client was killed, on one statement from `eager_test.rb`:
//!
//! ```sql
//! SELECT DISTINCT "citations"."id" FROM "citations"
//!   LEFT OUTER JOIN "citations" "citations_citations"
//!     ON "citations_citations"."citation_id" = "citations"."id" OFFSET $1
//! ```
//!
//! **There is no loop.** The suite's `citations` fixture is generated — `fixtures/citations.yml`
//! is `<% 65536.times do |i| %>` — and a join with no usable probe pairs every outer row with
//! every inner row and lets the `ON` decide. That is correct, and it is 65,536 × 65,536 = 4.3
//! **billion** pairs for a statement whose answer is 65,536 rows.
//!
//! `t.references :citation` makes a **non-unique** index, and `exec::query::probe_for` will only
//! probe a unique one, so this join gets `Probe::Materialize` — and the engine has no non-unique
//! index scan to offer it either. What the fix does instead is stop *scanning* the inner side that
//! was already materialised: it is grouped once by its half of the `ON`'s equality, so an outer
//! row visits the rows that can match it rather than all of them.
//!
//! # Asserted as a ratio, with a control
//!
//! The discipline `pk_and_sequence_cost.rs` established, and for the same reason: this is not a
//! wrong-answer bug, so no corpus can catch it — only the shape of the curve can. Eight times the
//! rows is eight times the work for a plan that pairs what matches, and sixty-four times for one
//! that pairs everything. Measured on the code this test was written against: **25 rows 400 µs,
//! 50 1.08 ms, 100 3.74 ms, 200 14.5 ms** — four times the work for twice the rows, all the way up.
//!
//! The control is a statement over the same table that *must* grow with it, so a slow machine or a
//! busy container moves both numbers and the comparison still says what it says.
//!
//! # The shape of the measurement, which was the bug
//!
//! This test was red on a loaded machine and green on an idle one, and `docs/plans/debt-c7.md` §7
//! made it deterministic: green at load 0, **red at load 14**, green again at 40 and 80.
//! Non-monotonic, so a race in the measurement rather than a slow test. It took all four cells in
//! sequence — subject small, subject large, *then* control small, control large — so **the control
//! was measured after the subject rather than beside it**, and load that arrived or departed
//! between the arms moved them apart. Cancelling load common to both arms is the one thing a
//! control is for.
//!
//! So the arms are interleaved: each round takes the subject and its control **adjacently at each
//! size**, and the reported figure is `(subject ÷ control at LARGE) ÷ (subject ÷ control at
//! SMALL)` — algebraically the same growth-against-growth number, arranged so each division is
//! between two measurements taken next to each other. A burst during either pair is in that pair's
//! numerator and denominator both. Rounds are repeated and the **median** is asserted, so a round
//! that catches a preemption is outvoted rather than averaged in.
//!
//! And the small case is lifted off the noise floor. At 250 rows the denominator was a 1.6 ms
//! sample, where one scheduler preemption of a few milliseconds is an error of over 100%.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

#[path = "parity_harness/mod.rs"]
mod parity;

/// The statement, with the bind written out — `OFFSET 0` is what `eager_load(:citations).offset(0)`
/// sends and it is not what makes this slow.
const JOIN: &str = "SELECT DISTINCT \"citations\".\"id\" FROM \"citations\" \
     LEFT OUTER JOIN \"citations\" \"citations_citations\" \
     ON \"citations_citations\".\"citation_id\" = \"citations\".\"id\" OFFSET 0";

/// A statement over the same rows whose cost is unavoidably linear in them.
const CONTROL: &str = "SELECT count(*) FROM \"citations\"";

/// The suite's fixture in miniature: `citation_id` is never set, so the self-join matches nothing
/// and the answer is every row — which is exactly the case that pairs everything to find out.
///
/// The rows arrive in batches: the fixture is not what is being measured, and one `INSERT` per row
/// was most of this file's wall clock once the sizes went up.
fn node_of(rows: usize) -> parity::Node {
    const PER_STATEMENT: usize = 500;
    let mut fixture =
        vec!["CREATE TABLE citations (id bigserial primary key, citation_id bigint)".to_owned()];
    let mut written = 0;
    while written < rows {
        let batch = PER_STATEMENT.min(rows - written);
        let values = vec!["(NULL)"; batch].join(", ");
        fixture.push(format!(
            "INSERT INTO citations (citation_id) VALUES {values}"
        ));
        written += batch;
    }
    let refs: Vec<&str> = fixture.iter().map(String::as_str).collect();
    parity::Node::new(&refs)
}

/// The small case. Big enough that the subject takes tens of milliseconds, because it is the
/// denominator of the ratio and at 250 rows it was a 1.6 ms sample.
const SMALL: usize = 1_000;

/// **Eight times `SMALL`**, which is the whole question: eight times the work, or sixty-four.
const LARGE: usize = 8 * SMALL;

/// How many times the four cells are taken. The median of the per-round figures is what is
/// asserted, so one preempted round is outvoted.
const ROUNDS: usize = 5;

fn elapsed(node: &mut parity::Node, sql: &str) -> (Duration, usize) {
    let start = Instant::now();
    let answer = node.rows(sql);
    (start.elapsed(), answer.len())
}

/// Guards the division, and nothing more: a duration this small is a measurement that did not
/// happen.
fn seconds(of: Duration) -> f64 {
    of.as_secs_f64().max(1e-6)
}

fn median(mut of: Vec<f64>) -> f64 {
    of.sort_by(f64::total_cmp);
    of[of.len() / 2]
}

/// **Eight times the rows must not be sixty-four times the work.**
#[test]
fn a_materialised_join_costs_what_it_pairs_and_not_the_cross_product() {
    let mut small_node = node_of(SMALL);
    let mut large_node = node_of(LARGE);

    // The answer first: a cheap plan that is wrong is not the thing being asked for. This is also
    // the warm-up — its timings are thrown away, because the first statement against a fresh node
    // pays for state every later one finds already built.
    assert_eq!(elapsed(&mut small_node, JOIN).1, SMALL);
    assert_eq!(elapsed(&mut large_node, JOIN).1, LARGE);
    elapsed(&mut small_node, CONTROL);
    elapsed(&mut large_node, CONTROL);

    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut witness = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        // Subject and control adjacent at each size. `small_cost` and `large_cost` are each a
        // division between two measurements taken next to each other, so load that arrives during
        // a pair lands in both halves of it.
        let (small, _) = elapsed(&mut small_node, JOIN);
        let (small_control, _) = elapsed(&mut small_node, CONTROL);
        let (large, _) = elapsed(&mut large_node, JOIN);
        let (large_control, _) = elapsed(&mut large_node, CONTROL);

        let small_cost = seconds(small) / seconds(small_control);
        let large_cost = seconds(large) / seconds(large_control);
        // **Against the control's growth, not scaled by it.** The control grows with the rows too
        // — that is what makes it a control — so it measures the same eight-fold this join should
        // show: a plan that pairs what matches lands near `1.0` here and one that pairs everything
        // near `8.0`. Multiplying the bound by the control instead of dividing by it was this
        // test's own first bug, and it let the unfixed executor pass.
        rounds.push(large_cost / small_cost);
        witness.push(format!(
            "{small:?}/{small_control:?} then {large:?}/{large_control:?}"
        ));
    }

    let relative = median(rounds.clone());
    assert!(
        relative < 3.0,
        "8x the rows cost {relative:.1}x as much work relative to the control (median of \
         {rounds:.1?}). A cross product grows about eight times faster than the control, not \
         once. Each round, as subject/control at {SMALL} rows then at {LARGE}: {}",
        witness.join("; ")
    );
}

/// The rows and their order are what a full pass produces — grouping the inner side decides *which*
/// pairs are worth evaluating, never what a pair evaluates to.
#[test]
fn grouping_the_inner_side_changes_no_answer() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE l (id bigint, tag text)",
        "CREATE TABLE r (id bigint, note text)",
        // Duplicate keys on both sides, a NULL on each, and a key that matches nothing.
        "INSERT INTO l VALUES (1,'a'), (1,'b'), (2,'c'), (NULL,'d'), (3,'e')",
        "INSERT INTO r VALUES (1,'x'), (1,'y'), (2,'z'), (NULL,'w')",
    ]);

    // An inner join: every pair, in outer-major order, inner in insertion order.
    assert_eq!(
        node.rows("SELECT l.tag, r.note FROM l JOIN r ON l.id = r.id"),
        vec![
            vec!["a".to_owned(), "x".to_owned()],
            vec!["a".to_owned(), "y".to_owned()],
            vec!["b".to_owned(), "x".to_owned()],
            vec!["b".to_owned(), "y".to_owned()],
            vec!["c".to_owned(), "z".to_owned()],
        ]
    );
    // A left join keeps the outer rows that matched nothing — the NULL key and the absent one —
    // and a NULL key matches nothing rather than matching the other NULL.
    assert_eq!(
        node.rows("SELECT l.tag, r.note FROM l LEFT JOIN r ON l.id = r.id"),
        vec![
            vec!["a".to_owned(), "x".to_owned()],
            vec!["a".to_owned(), "y".to_owned()],
            vec!["b".to_owned(), "x".to_owned()],
            vec!["b".to_owned(), "y".to_owned()],
            vec!["c".to_owned(), "z".to_owned()],
            vec!["d".to_owned(), "\\N".to_owned()],
            vec!["e".to_owned(), "\\N".to_owned()],
        ]
    );
    // An `ON` with more than the equality still applies all of it: the grouping picks candidates,
    // the condition decides.
    assert_eq!(
        node.rows("SELECT l.tag, r.note FROM l JOIN r ON l.id = r.id AND r.note = 'y'"),
        vec![
            vec!["a".to_owned(), "y".to_owned()],
            vec!["b".to_owned(), "y".to_owned()],
        ]
    );
    // And an `ON` with no equality to group by is untouched — every pair, as before: `2` beats
    // the two `1`s and `3` beats all three non-NULL keys, while a NULL on either side beats
    // nothing.
    assert_eq!(
        node.rows("SELECT l.tag, r.note FROM l JOIN r ON l.id > r.id")
            .len(),
        5
    );
}
