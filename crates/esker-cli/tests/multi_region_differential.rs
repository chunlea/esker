//! **The two engines, across a region boundary.** Red until the columnar copy is region-scoped.
//!
//! # What it is red for
//!
//! Measured on a real four-store cluster on 2026-09-05: a table in four regions with a columnar
//! learner on each, every fragment answered, `Engine: columnar` — and every aggregate came back
//! **four times** its true value. `count(*)` of twenty thousand rows answered eighty thousand; a
//! join answered 10,080 for 2,520. The multiplier is the region count and the learners were
//! co-located two to a store, so a fragment reads its store's columnar runs rather than only its
//! own region's, and every row is counted once per region (`docs/bench/mpp-baseline.md` §10).
//!
//! That is the failure [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) names as the
//! worst this feature can have — the two engines disagreeing, silently — and the defence it asks
//! for by name is a differential. The one that exists,
//! `esker-sql/tests/routing_differential.rs`, is **single-region** and structurally cannot see it.
//! This is that differential across a boundary.
//!
//! # Owner, and what makes it green
//!
//! `esker-store`: the columnar copy has to be scoped to the region a fragment asks about. ADR 0040
//! says so in the same sentence where it priced this as *"a performance bound, not a wrong
//! answer"* — which it is not, because the splits happen **before** the learner is placed, so no
//! shard is ever stale, every fragment is legitimately routed, and it still reads too much.
//!
//! When the store fix lands, `ClientFragments::runs_are_region_scoped` flips to `true` and the
//! interim guard in `esker-sql`'s planner stops firing. **This test going green is what earns that
//! flip**, and it is why it asserts `Engine: columnar` on every comparison: with the guard in
//! place both arms run on rows and agree for free, which is worth nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster_harness;

use cluster_harness::{Cluster, rows_of};

/// Four stores: a region has three voters and PD places a columnar learner on a store with no peer
/// of that region, so four is the fewest that can hold one.
const STORES: u16 = 4;

/// Rows loaded before the comparison: few and fat, so the table splits in seconds.
const ROWS: i64 = 200;

/// Bytes of payload per row.
const PAYLOAD: usize = 4_096;

/// The region-split threshold the stores run with.
const SPLIT_SIZE: u64 = 262_144;

/// Every query compared. Aggregates, because that is the shape a fragment expresses.
const QUERIES: &[&str] = &[
    "SELECT count(*) FROM ledger",
    "SELECT count(amount), sum(amount) FROM ledger",
    "SELECT min(amount), max(amount) FROM ledger",
    "SELECT bucket, count(*) FROM ledger GROUP BY bucket ORDER BY bucket",
];

#[test]
fn the_two_engines_agree_across_a_region_boundary() {
    let cluster = Cluster::start_with(STORES, SPLIT_SIZE);
    cluster.run("CREATE TABLE ledger (id int8 PRIMARY KEY, bucket int8, amount int8, note text)");
    cluster.run("ALTER TABLE ledger SET (columnar_replicas = 1)");
    let payload = "x".repeat(PAYLOAD);
    for batch in 0..ROWS / 20 {
        let values: Vec<String> = (0..20)
            .map(|row| {
                let id = batch * 20 + row + 1;
                format!("({id},{},{id},'{payload}')", id % 4)
            })
            .collect();
        cluster.run(&format!("INSERT INTO ledger VALUES {}", values.join(",")));
    }

    let regions = cluster.regions();
    assert!(
        regions > 1,
        "the table did not split, so this says nothing about a boundary: {regions} region(s)"
    );
    cluster.wait_for_learners(180);

    for query in QUERIES {
        let rows = cluster.query_on("row", query);
        let columns = cluster.query_on("columnar", query);
        // **The engine first.** A query that fell back agrees with the row engine for free, so a
        // comparison whose denominator is not checked is not evidence — the lesson this lane paid
        // for twice on 2026-09-05, once by reading a column headed `columnar` that held numbers
        // the row engine produced.
        let plan = cluster.query_on("auto", &format!("EXPLAIN ANALYZE {query}"));
        assert!(
            plan.contains("Engine: columnar"),
            "`{query}` was not answered by the columns over {regions} regions, so its agreement \
             would be free:\n{plan}"
        );
        assert_eq!(
            rows_of(&rows),
            rows_of(&columns),
            "the two engines disagree on `{query}` over {regions} regions"
        );
    }
}
