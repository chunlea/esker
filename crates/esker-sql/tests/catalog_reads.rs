//! **How many times one statement reads the catalog version** — the measurement
//! [ADR 0102](../../../docs/adr/0102-the-catalogs-read-path.md) says must come before its own.
//!
//! `Catalog::view_at`'s module doc says a transaction reads the version **once**.
//! `Executor::catalog_view` is reached from thirteen places, and run 111 counted **4,790,406**
//! views in one `ActiveRecord` pass. This is that difference, per statement class, so the merge that
//! follows can be reported as a difference rather than as an intention.
//!
//! **Topology-independent on purpose.** A count of code paths taken does not care whether the
//! backend is in this process or across a socket — which is exactly why this half of ADR 0102's
//! evidence can be taken here, in milliseconds, while the *cost* half needs a cluster.
//!
//! Run it with the instrument on, which is not the default:
//!
//! ```text
//! ESKER_CATALOG_STATS=1 cargo nextest run -p esker-sql --test catalog_reads --run-ignored all --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::catalog::stats;

/// One statement's worth of catalog reads: what the counters did across it.
fn cost_of(node: &mut parity::Node, sql: &str) -> (u64, u64) {
    let (views, repeats) = stats::counts();
    node.run(sql)
        .unwrap_or_else(|error| panic!("`{sql}`: {error}"));
    let (views_after, repeats_after) = stats::counts();
    (views_after - views, repeats_after - repeats)
}

#[test]
#[ignore = "a measurement for ADR 0102: run it with ESKER_CATALOG_STATS=1"]
fn how_many_catalog_reads_each_statement_class_makes() {
    let mut node = parity::Node::new(&[]);
    if stats::counts() == (0, 0) && std::env::var_os("ESKER_CATALOG_STATS").is_none() {
        println!(
            "\n  ESKER_CATALOG_STATS is not set — every number below will be zero and the run \
             says nothing"
        );
    }

    // The fixture is measured too: a `CREATE TABLE` is the DDL row, and running it here means the
    // classes below all meet a catalog that already holds a table.
    let ddl = cost_of(
        &mut node,
        "CREATE TABLE t (id bigint PRIMARY KEY, n bigint, s text)",
    );
    let insert = cost_of(&mut node, "INSERT INTO t VALUES (1, 1, 'a')");
    let more = cost_of(&mut node, "INSERT INTO t VALUES (2, 2, 'b'), (3, 3, 'c')");
    let point = cost_of(&mut node, "SELECT n FROM t WHERE id = 1");
    let range = cost_of(&mut node, "SELECT n FROM t WHERE id BETWEEN 1 AND 3");
    let update = cost_of(&mut node, "UPDATE t SET n = n + 1 WHERE id = 1");
    let delete = cost_of(&mut node, "DELETE FROM t WHERE id = 3");
    let alter = cost_of(&mut node, "ALTER TABLE t ADD COLUMN extra bigint");
    let joined = cost_of(
        &mut node,
        "SELECT a.n FROM t a JOIN t b ON a.id = b.id WHERE a.id = 1",
    );
    let block = cost_of(&mut node, "BEGIN");
    let in_block = cost_of(&mut node, "SELECT n FROM t WHERE id = 2");
    let commit = cost_of(&mut node, "COMMIT");

    println!("\n  statement class                          views  repeats");
    for (label, (views, repeats)) in [
        ("CREATE TABLE", ddl),
        ("INSERT, one row", insert),
        ("INSERT, three rows", more),
        ("SELECT, point", point),
        ("SELECT, range", range),
        ("UPDATE, point", update),
        ("DELETE, point", delete),
        ("ALTER TABLE ADD COLUMN", alter),
        ("SELECT, self join", joined),
        ("BEGIN", block),
        ("SELECT inside a block", in_block),
        ("COMMIT", commit),
    ] {
        println!("  {label:<40} {views:>5}  {repeats:>7}");
    }
    let (total, repeats) = stats::counts();
    println!("  {:<40} {total:>5}  {repeats:>7}", "the whole run");
}
