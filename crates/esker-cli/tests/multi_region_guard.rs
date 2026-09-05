//! The interim guard that keeps a silent wrong answer off the columnar path.
//!
//! # What it is holding back
//!
//! Measured on a real four-store cluster on 2026-09-05: a table in four regions, a columnar
//! learner on each, **every fragment answered**, and every aggregate came back **four times** its
//! true value — `count(*)` of twenty thousand rows answered eighty thousand, and a join answered
//! 10,080 for 2,520. The multiplier is the region count and the learners were co-located two to a
//! store, so a fragment reads its store's columnar runs rather than only its own region's and
//! every row is counted once per region (`docs/bench/mpp-baseline.md` §10).
//!
//! [ADR 0040](../../../docs/adr/0040-the-engine-a-query-runs-on.md) foresaw the shape and called it
//! *"a performance bound, not a wrong answer"*, on the reasoning that the epoch refuses a shard
//! whose region has split under it. That does not apply: the splits happen **before** the learner
//! is placed, so no shard is stale, every fragment is legitimately routed, and it still reads too
//! much.
//!
//! So until the columnar copy is region-scoped — `esker-store`'s, by ADR 0040's own sentence — a
//! table in more than one region reads rows.
//!
//! # Why this test asserts the *reason* and not the answer
//!
//! An answer that agrees proves nothing here: the row engine is correct, so *any* fallback agrees
//! with it, including one that happened for an unrelated reason. What has to be true is that
//! **this** rule fired, and `EXPLAIN` is the only place that says so. It is the same discipline
//! `routing_differential` uses in the other direction, and the same one this lane got wrong once
//! by reading a column headed `columnar` that held numbers the row engine produced.
//!
//! This test does **not** need a columnar learner — the guard is about region *count* — but it does
//! need a cluster the SQL node sees more than one **shard** in, and a single-store one showed it a
//! single shard however many regions PD held. Four stores, which is also the shape the defect was
//! measured on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster_harness;

use cluster_harness::{Cluster, rows_of};

/// Rows loaded before the assertions: few and fat, so the table splits in seconds.
const ROWS: i64 = 200;

/// Bytes of payload per row.
const PAYLOAD: usize = 4_096;

/// The region-split threshold the store runs with.
const SPLIT_SIZE: u64 = 262_144;

#[test]
fn a_table_in_more_than_one_region_reads_rows_and_says_why() {
    // Four stores: the guard turns on the number of shards the *SQL node* sees, and a
    // single-store cluster shows it one however many regions PD holds.
    let cluster = Cluster::start_with(4, SPLIT_SIZE);
    cluster.run("CREATE TABLE ledger (id int8 PRIMARY KEY, amount int8, note text)");
    cluster.run("ALTER TABLE ledger SET (columnar_replicas = 1)");
    let payload = "x".repeat(PAYLOAD);
    for batch in 0..ROWS / 20 {
        let values: Vec<String> = (0..20)
            .map(|row| {
                let id = batch * 20 + row + 1;
                format!("({id},{id},'{payload}')")
            })
            .collect();
        cluster.run(&format!("INSERT INTO ledger VALUES {}", values.join(",")));
    }

    let regions = cluster.wait_for_a_split(120);

    // The answer is still right — that is the point of falling back — but it is not the assertion.
    let answer = cluster.query_on("auto", "SELECT count(*) FROM ledger");
    assert_eq!(
        rows_of(&answer),
        vec![ROWS.to_string()],
        "the fallback did not answer correctly: {answer}"
    );

    // **The assertion, and it is polled.** The guard turns on the shards the *SQL node* sees, and
    // its region cache is a hint repaired by the refusals it causes — so straight after a split it
    // still holds the pre-split view and the plan refuses for the previous reason instead. Waiting
    // for the cache is honest: the guard is eventually consistent with it, and a test that read
    // once would be asserting on whichever view happened to be current.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let plan = loop {
        let plan = cluster.query_on("auto", "EXPLAIN SELECT count(*) FROM ledger");
        if plan.contains("more than one region") {
            break plan;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a {regions}-region table never met the guard; the last plan said:\n{plan}"
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    };
    assert!(
        plan.contains("Engine: rows"),
        "the guard named itself but the plan did not run on rows:\n{plan}"
    );
}
