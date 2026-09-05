//! **A multi-region table never answers wrong** — whichever engine the system says it will use.
//!
//! # What this is for
//!
//! Measured on a real four-store cluster on 2026-09-05: a table in four regions with a columnar
//! learner on each, every fragment answered, and every aggregate **four times** its true value —
//! `count(*)` of twenty thousand rows answering eighty thousand. A learner's columnar runs were
//! not scoped to the region the fragment asked about, so every row was counted once per region
//! (`docs/bench/mpp-baseline.md` §10). That is the failure
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) names as the worst this feature
//! can have: the two engines disagreeing, silently.
//!
//! # Why it asserts the declared state rather than one engine
//!
//! The store-side fix arrives in halves, and a half of it is already on `main`: with the copy
//! built from the region's range but the scan range not yet applied, a two-hundred-row table
//! answers 357; with the scan range on and the build scoping absent, 52. So the system's honest
//! answer today is *"do not route this"*, and `FragmentSource::runs_are_region_scoped` is where it
//! says so.
//!
//! This test reads that declaration **end to end**, out of `EXPLAIN`, and asserts what the
//! declaration promises:
//!
//! * **declared `false`** — the guard must have fired, `EXPLAIN` must name the rule, and the
//!   answers must still be *right*, because a fallback that answers wrongly is no better than the
//!   thing it guards;
//! * **declared `true`** — every comparison must have run on the columns (`Engine: columnar`) and
//!   agreed with the row engine.
//!
//! So it is green on `main` in the guarded state and its demand turns on the moment the flip
//! happens. **No `#[ignore]`**: it runs every time, and what it checks — that a multi-region table
//! is never answered wrongly — is true in both states and is the property that actually matters.
//!
//! # Retirement
//!
//! Nothing retires this test. What changes is which branch it takes: when `esker-store`'s columnar
//! copy is region-scoped in **both** halves and `ClientFragments::runs_are_region_scoped` returns
//! `true`, the `declared true` branch becomes the live one and this becomes the multi-region
//! differential ADR 0022 asks for by name. Until then it is the check that the guard is really
//! guarding.

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

/// The reason string the interim guard prints, from `esker_sql::exec::fragment`.
const GUARD: &str = "not region-scoped";

/// Every query compared, with the answer the fixture makes true.
///
/// **The expected values are computed from the fixture, not from one of the engines.** Agreement
/// between two engines is not correctness — they agreed at four times the right answer when both
/// were reading too much would have been caught only by knowing the number — and in the guarded
/// state both arms are the row engine, where agreement is free.
fn queries() -> Vec<(&'static str, Vec<Vec<String>>)> {
    let sum: i64 = (1..=ROWS).sum();
    vec![
        ("SELECT count(*) FROM ledger", vec![vec![ROWS.to_string()]]),
        (
            "SELECT count(amount), sum(amount) FROM ledger",
            vec![vec![ROWS.to_string(), sum.to_string()]],
        ),
        (
            "SELECT min(amount), max(amount) FROM ledger",
            vec![vec!["1".to_owned(), ROWS.to_string()]],
        ),
        (
            "SELECT bucket, count(*) FROM ledger GROUP BY bucket ORDER BY bucket",
            (0..4)
                .map(|bucket| vec![bucket.to_string(), (ROWS / 4).to_string()])
                .collect(),
        ),
    ]
}

#[test]
fn a_multi_region_table_is_never_answered_wrongly() {
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

    // **The declaration, read end to end.** `EXPLAIN` is where a `FragmentSource` that says it
    // cannot scope a fragment to one region becomes visible, and reading it here rather than
    // hard-coding a state is what lets one test hold in both.
    let probe = cluster.query_on("auto", "EXPLAIN SELECT count(*) FROM ledger");
    let guarded = probe.contains(GUARD);
    eprintln!(
        "the source declares runs_are_region_scoped = {}",
        if guarded { "false" } else { "true" }
    );

    for (query, expected) in queries() {
        let rows = rows_of(&cluster.query_on("row", query));
        let columns = rows_of(&cluster.query_on("columnar", query));
        let plan = cluster.query_on("auto", &format!("EXPLAIN ANALYZE {query}"));
        let flat: Vec<String> = expected
            .iter()
            .map(|row| row.first().cloned().unwrap_or_default())
            .collect();

        if guarded {
            assert!(
                plan.contains("Engine: rows") && plan.contains(GUARD),
                "the source cannot scope a fragment, so `{query}` over {regions} regions had to \
                 be guarded and was not:\n{plan}"
            );
        } else {
            assert!(
                plan.contains("Engine: columnar"),
                "the source declares its runs are region-scoped, so `{query}` over {regions} \
                 regions had to be answered by the columns:\n{plan}"
            );
        }

        // True in both states, and the whole point: the answer is right.
        assert_eq!(
            rows, flat,
            "the row engine answered `{query}` wrongly over {regions} regions"
        );
        assert_eq!(
            columns, flat,
            "the columnar arm answered `{query}` wrongly over {regions} regions"
        );
    }
}
