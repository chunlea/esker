//! A SQL scan over a table that spans **more than one region**, against real binaries.
//!
//! # Why this test exists
//!
//! Until [ADR 0073](../../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md) a SQL
//! table never split: `approximate_size` measured the `RawKV` namespace while SQL rows live under
//! `'x'`, so a region holding two hundred thousand rows reported `~0 bytes` and crossed no
//! threshold at any setting. That was found by the `mpp` lane's benchmark and fixed by the `split`
//! lane, and this is the first build in which a SQL table occupies more than one region.
//!
//! The first run against one found this, on **both engines**:
//!
//! ```text
//! SELECT count(*) FROM ledger: ERROR 08006: could not reach the store:
//!                              key is not in region 1
//! ```
//!
//! A point read at either end of the table answers, and an `INSERT` past the split commits — so
//! routing, the region cache and the write path all follow a boundary. A **scan** does not: it
//! asks the first region for keys the first region does not hold, and `KeyNotInRegion` reaches the
//! client as `08006` instead of moving on to the next region.
//!
//! What that costs is the whole of scale-out from SQL: the moment a table grows past the split
//! threshold, every `SELECT` that is not a point read stops working.
//!
//! # What this test is, and what it is not
//!
//! It is a **red test handed to whoever owns the fix**. The scan path is `esker-client`'s and
//! `esker-sql`'s cursor, neither of which is the `mpp` lane's; this file asserts the symptom from
//! outside, in the crate that owns the binaries, so the owning lane has a failing test rather than
//! a paragraph.
//!
//! It follows `pgwire_cluster.rs` deliberately, including its lesson: **it is not `#[ignore]`d and
//! it cannot skip.** That file records why — the test which would have caught its own bug was
//! ignored *and* skipped when `psql` was absent, "coverage that exists and cannot run is what let
//! this reach a release" — so this one speaks the wire itself and runs wherever the gate runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster_harness;

use cluster_harness::{Cluster, rows_of};

/// Rows loaded before the scan.
///
/// **Few and fat, rather than many and thin.** A region splits on its *size*, and the thing under
/// test is a boundary rather than a volume — so two hundred rows carrying four kilobytes each
/// cross the threshold below in a couple of statements, where the twelve thousand thin rows this
/// started with took longer than a gate will wait in a debug build.
const ROWS: i64 = 200;

/// Bytes of payload per row.
const PAYLOAD: usize = 4_096;

/// The region-split threshold the stores run with, in bytes.
///
/// Far below the shipped 96 MiB so the table splits after seconds of loading rather than minutes.
/// It is the same code path at either value — what a threshold changes is when, not whether.
const SPLIT_SIZE: u64 = 262_144;

#[test]
fn a_scan_reads_a_table_that_spans_more_than_one_region() {
    let phase = std::time::Instant::now();
    let cluster = Cluster::start(SPLIT_SIZE);
    cluster.run("CREATE TABLE ledger (id int8 PRIMARY KEY, amount int8, note text)");
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
    eprintln!(
        "{ROWS} rows of {PAYLOAD} bytes loaded in {:.1?}; {} region(s)",
        phase.elapsed(),
        cluster.regions()
    );

    // **The table must actually have split, or this test proves nothing.** A single-region table
    // scans fine, so an assertion about the scan below would pass for the wrong reason — the
    // failure mode `docs/bench/columnar-m2.md` calls a green that a naive implementation also
    // produces.
    let regions = cluster.regions();
    assert!(
        regions > 1,
        "the table did not split at a {SPLIT_SIZE}-byte threshold after {ROWS} rows, so this \
         test says nothing about a region boundary: {regions} region(s)"
    );

    let answer = cluster.query("SELECT count(*) FROM ledger");
    assert!(
        !answer.contains("key is not in region"),
        "a scan over a table spanning {regions} regions asked one region for the whole table:\n\
         {answer}"
    );
    assert_eq!(
        rows_of(&answer),
        vec![ROWS.to_string()],
        "the scan did not count every row of a {regions}-region table"
    );
}
