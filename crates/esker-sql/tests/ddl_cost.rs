//! **What a DDL statement costs on a real store**, in the units a write is priced by.
//!
//! Run 117 priced the `ActiveRecord` suite statement by statement on the real topology: `CREATE
//! TABLE` p50 86 ms, `DROP TABLE` p50 317 ms, `CREATE INDEX` 93 ms, and a `DISABLE/ENABLE TRIGGER
//! ALL` that becomes seven `ALTER`s at about 106 ms each — 11.5% of one file. Against `bench v1.1`'s
//! 21 ms for a single synchronous write, a `CREATE` costs four of those and a `DROP` fifteen.
//!
//! A duration does not say where the money goes. This does: `esker_client::stmt_stats` now counts
//! the write side — timestamps taken, `Prewrite` and `Commit` calls, mutations sent, and time spent
//! asleep behind somebody else's lock — and this file reads those counters around one statement at
//! a time.
//!
//! **Ignored by default.** It is a measurement, not an assertion: the numbers belong in
//! `esker-coord/h1-ddl-cost.md` and a test that failed when a DDL got cheaper would be worse than
//! useless. Run it with
//!
//! ```text
//! ESKER_STMT_STATS=1 cargo nextest run -p esker-sql --test ddl_cost --run-ignored all --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use cluster::Cluster;

/// One statement's write-side cost, as a difference of the totals around it.
fn priced(session: &mut cluster::Session, sql: &str) -> String {
    let before = esker_sql::stmt_stats::write_counts();
    let trips_before = esker_sql::stmt_stats::counts();
    let began = std::time::Instant::now();
    session.run(sql).unwrap();
    let took = began.elapsed();
    let after = esker_sql::stmt_stats::write_counts();
    let trips_after = esker_sql::stmt_stats::counts();
    let trips = trips_after.3 - trips_before.3;
    let phases = (after.1 - before.1) + (after.2 - before.2);
    format!(
        "{:>7.1} ms  trips {:>3} = reads {:>3} + scans {:>2} + phases {:>2}  |  tso {:>2}  \
         prewrites {:>2}  commits {:>2}  keys {:>3}  regions {:>2}  waited {:>5} us   {}",
        took.as_secs_f64() * 1_000.0,
        trips,
        trips_after.1 - trips_before.1,
        trips_after.2 - trips_before.2,
        phases,
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2,
        after.3 - before.3,
        trips_after.4 - trips_before.4,
        after.4 - before.4,
        sql.chars().take(56).collect::<String>(),
    )
}

/// Prints the round-trip decomposition of each DDL shape run 117 found expensive.
#[test]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
fn what_each_ddl_statement_costs() {
    assert!(
        esker_sql::stmt_stats::enabled(),
        "set ESKER_STMT_STATS=1, or every number here is zero"
    );
    let cluster = Cluster::start();
    let mut s = cluster.session();

    // A baseline to read the DDL against: one ordinary row write, which is what `bench v1.1`'s
    // 21 ms prices. Without it every number below is a ratio to nothing.
    println!("\n  -- baseline --");
    println!(
        "  {}",
        priced(
            &mut s,
            "CREATE TABLE base (id bigint primary key, n bigint)"
        )
    );
    println!(
        "  {}",
        priced(&mut s, "INSERT INTO base (id, n) VALUES (1, 1)")
    );
    println!("  {}", priced(&mut s, "UPDATE base SET n = 2 WHERE id = 1"));
    println!("  {}", priced(&mut s, "SELECT n FROM base WHERE id = 1"));

    println!("\n  -- the shapes run 117 priced --");
    println!(
        "  {}",
        priced(
            &mut s,
            "CREATE TABLE t (id bigint primary key, a bigint, b text)"
        )
    );
    println!("  {}", priced(&mut s, "CREATE INDEX t_a ON t (a)"));
    println!("  {}", priced(&mut s, "ALTER TABLE t ADD COLUMN c bigint"));
    println!("  {}", priced(&mut s, "ALTER TABLE t DISABLE TRIGGER ALL"));
    println!("  {}", priced(&mut s, "ALTER TABLE t ENABLE TRIGGER ALL"));

    // A `DROP` of a table with rows and an index, which is what Rails' fixtures leave behind: run
    // 117's 317 ms is a drop of a populated table, and an empty one would price the wrong thing.
    for i in 0..100u32 {
        s.run(&format!("INSERT INTO t (id, a, b) VALUES ({i}, {i}, 'x')"))
            .unwrap();
    }
    println!("\n  -- and a DROP of a table with 100 rows and an index --");
    println!("  {}", priced(&mut s, "DROP TABLE t"));

    // **What the scans scale with.** A `DROP` writes one key and scans a dozen times, so it is not
    // deleting rows one at a time — but "a dozen" has to be attributed to something. Three drops,
    // differing only in what the table has, say which.
    println!("\n  -- what a DROP's scans scale with --");
    s.run("CREATE TABLE d0 (id bigint primary key)").unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE d0"));

    s.run("CREATE TABLE d1 (id bigint primary key, a bigint)")
        .unwrap();
    for i in 0..100u32 {
        s.run(&format!("INSERT INTO d1 (id, a) VALUES ({i}, {i})"))
            .unwrap();
    }
    println!("  {}", priced(&mut s, "DROP TABLE d1"));

    s.run("CREATE TABLE d2 (id bigint primary key, a bigint, b bigint, c bigint)")
        .unwrap();
    s.run("CREATE INDEX d2_a ON d2 (a)").unwrap();
    s.run("CREATE INDEX d2_b ON d2 (b)").unwrap();
    s.run("CREATE INDEX d2_c ON d2 (c)").unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE d2"));

    // **Does the scan count track the *kinds* of record a table has, or their number?** The catalog
    // is kind-major — `'m' ++ "sql" ++ KIND ++ tenant ++ id` — so "everything belonging to table X"
    // is one prefix per kind, not one prefix. If that is what the scans are, adding a record of a
    // *new* kind costs a scan and adding more of the same kind costs none.
    println!("\n  -- does a DROP's scan count track kinds or counts? --");
    s.run("CREATE TABLE k0 (id bigint primary key)").unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE k0"));

    // A sequence: a record of a kind `k0` did not have (`'q'`, and its value under `'e'`).
    s.run("CREATE TABLE k1 (id bigserial primary key, a bigint)")
        .unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE k1"));

    // Two sequences: more of the same kind.
    s.run("CREATE TABLE k2 (id bigserial primary key, b bigserial)")
        .unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE k2"));

    // A foreign key: a back-reference, under `'k'`, another kind again.
    s.run("CREATE TABLE parent (id bigint primary key)")
        .unwrap();
    s.run("CREATE TABLE child (id bigint primary key, p bigint references parent (id))")
        .unwrap();
    println!("  {}", priced(&mut s, "DROP TABLE child"));

    println!("\n  {}\n", esker_sql::stmt_stats::summary());
}
