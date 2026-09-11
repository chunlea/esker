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

/// One statement's reads and scans, grouped by the catalog kind each addressed.
fn where_the_reads_went(session: &mut cluster::Session, sql: &str) -> String {
    esker_client::stmt_stats::reset();
    session.run(sql).unwrap();
    let cost = esker_client::stmt_stats::taken();
    let show =
        |what: &str,
         heads: &std::collections::BTreeMap<[u8; esker_client::stmt_stats::HEAD], u64>| {
            let named = esker_sql::stmt_stats::name_heads(heads);
            let total: u64 = named.iter().map(|(_, n)| n).sum();
            let each: Vec<String> = named
                .iter()
                .map(|(name, n)| format!("{name} x{n}"))
                .collect();
            format!("{what} {total} = {}", each.join(", "))
        };
    format!(
        "{}\n      {}\n      {}",
        sql.chars().take(60).collect::<String>(),
        show("reads", &cost.read_heads),
        show("scans", &cost.scan_heads),
    )
}

/// **Which catalog kinds a DDL statement reads and scans** — the instrument
/// `esker-coord/h1-ddl-cost.md` § 2 asked for, to say whether `DROP`'s nine fixed scans are nine
/// kinds, a few kinds scanned repeatedly, or not catalog scans at all.
#[test]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
fn which_kinds_a_ddl_statement_reads() {
    assert!(
        esker_sql::stmt_stats::enabled(),
        "set ESKER_STMT_STATS=1, or every number here is zero"
    );
    let cluster = Cluster::start();
    let mut s = cluster.session();
    s.run("CREATE TABLE parent (id bigint primary key)")
        .unwrap();
    s.run(
        "CREATE TABLE wide (id bigserial primary key, a bigint, b text, c bigint, \
         p bigint references parent (id))",
    )
    .unwrap();
    s.run("CREATE INDEX wide_a ON wide (a)").unwrap();
    s.run("CREATE INDEX wide_c ON wide (c)").unwrap();

    println!(
        "\n  -- CREATE TABLE --\n  {}",
        where_the_reads_went(
            &mut s,
            "CREATE TABLE fresh (id bigint primary key, n bigint)"
        )
    );
    println!(
        "\n  -- ALTER TABLE … DISABLE TRIGGER ALL --\n  {}",
        where_the_reads_went(&mut s, "ALTER TABLE wide DISABLE TRIGGER ALL")
    );
    println!(
        "\n  -- DROP TABLE (2 indexes, 1 sequence, 1 fk, 5 columns) --\n  {}",
        where_the_reads_went(&mut s, "DROP TABLE wide")
    );

    // **Is `'q'` a loop over sequences, or one scan per table load?** `table_sequences` scans the
    // prefix whether or not the table has any — "a table with no sequences costs one empty read" —
    // so a plain table separates the two: a loop would drop to zero, a per-load scan would not.
    // The `'t'` count beside it is the table record itself, and the two moving together is what
    // says the table is being loaded more than once.
    s.run("CREATE TABLE plain (id bigint primary key, n bigint)")
        .unwrap();
    println!(
        "\n  -- DROP TABLE (nothing attached) --\n  {}",
        where_the_reads_went(&mut s, "DROP TABLE plain")
    );

    // A statement that reads a table without changing it, for the floor.
    s.run("CREATE TABLE reader (id bigint primary key)")
        .unwrap();
    s.run("INSERT INTO reader (id) VALUES (1)").unwrap();
    println!(
        "\n  -- SELECT, for the floor --\n  {}",
        where_the_reads_went(&mut s, "SELECT id FROM reader WHERE id = 1")
    );
    println!();
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

/// **#58 round 2 — does `DROP TABLE IF EXISTS` get dearer as history accumulates, and where?**
///
/// r1's shape diff put 87% of a Rails file's growth in this one statement: 277 reads apiece, 8.3×
/// the file mean, and a per-read cost that grew 2.54× between file 1 and file 8 — *after* the
/// engine-level read-path fixes made a scan's steps flat in the number of versions.
///
/// So the reads themselves are the suspect, and the question is which of them. This repeats the
/// `CREATE` / `DROP` pair a Rails setup and teardown make, and prints the `DROP`'s reads **grouped
/// by the catalog kind each addressed**, once per round. A kind whose count climbs with the round
/// is the mechanism; counts that are all flat mean each read got dearer rather than more numerous,
/// and the next place to look is under the client rather than in the catalog.
///
/// Ignored by default, like everything else in this file: it prints, and a test that failed when a
/// DDL got cheaper would be worse than useless.
#[test]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
fn what_a_drop_reads_as_the_history_grows() {
    const ROUNDS: usize = 10;
    // **A catalog the size the workload has.** The first run of this probe used an empty schema and
    // measured 10 reads a `DROP`, against r1's 277 — so the 277 is a function of how many relations
    // the catalog holds, not of the statement alone, and a probe without them measures a different
    // statement. r1's file holds 294; this is the same order, kept small enough to build in a test.
    const BACKGROUND: usize = 150;

    assert!(
        esker_sql::stmt_stats::enabled(),
        "set ESKER_STMT_STATS=1, or every number here is zero"
    );
    let cluster = Cluster::start();
    let mut s = cluster.session();
    for at in 0..BACKGROUND {
        s.run(&format!(
            "CREATE TABLE bg{at} (id bigserial primary key, a bigint, b text)"
        ))
        .unwrap();
    }

    for round in 0..ROUNDS {
        s.run("CREATE TABLE t (id bigserial primary key, a bigint, b text)")
            .unwrap();
        // The statement under test, priced and attributed. `IF EXISTS` because that is the form
        // `ActiveRecord` sends, and the form r1 measured.
        esker_client::stmt_stats::reset();
        let began = std::time::Instant::now();
        s.run("DROP TABLE IF EXISTS t").unwrap();
        let took = began.elapsed();
        let cost = esker_client::stmt_stats::taken();
        let reads: u64 = cost.read_heads.values().sum();
        let scans: u64 = cost.scan_heads.values().sum();
        let by_kind =
            |heads: &std::collections::BTreeMap<[u8; esker_client::stmt_stats::HEAD], u64>| {
                esker_sql::stmt_stats::name_heads(heads)
                    .iter()
                    .map(|(name, n)| format!("{name} x{n}"))
                    .collect::<Vec<String>>()
                    .join(", ")
            };
        println!(
            "  round {:>2}  {:>7.1} ms  reads {reads:>4}  scans {scans:>3}\n            reads: {}\n            scans: {}",
            round + 1,
            took.as_secs_f64() * 1_000.0,
            by_kind(&cost.read_heads),
            by_kind(&cost.scan_heads),
        );
    }
}

/// **#61 — dropping one table must not read the whole catalog.**
///
/// Measured before the fix, 150 background relations: `reads 160 = catalog 't' x152, …` and
/// `scans 158 = catalog 'q' x152, …`. One `KIND_TABLE` read and one `KIND_SEQUENCE` scan per
/// relation in the database, because `DROP` materialises the whole relations view
/// (`Catalog::relations` → `pg_relations::Relations::read`). At r1's 294 relations that is the 277
/// reads a statement the run-127c shape diff attributed **73% of a Rails file's time** to.
///
/// What a `DROP` actually needs is *who references this table* — foreign keys, owned sequences,
/// indexes, and the view and trigger dependencies — which is a question about one relation, not a
/// listing of all of them.
///
/// # The bound, and why it is a bound and not a ratio
///
/// Under twenty reads at **three hundred** relations. A ratio against the catalog size would pass
/// on a constant factor of two; a fixed ceiling at a catalog size twice the one that produced the
/// original number says the cost stopped depending on it at all. It is deliberately loose — this
/// asserts the shape, not a count somebody has to update whenever a `DROP` reads one more record.
///
/// **Not `#[ignore]`d**, unlike the rest of this file: it is an assertion about a complexity class,
/// and one that would have caught #61 the day it was written.
#[test]
fn dropping_one_table_does_not_read_every_relation() {
    const BACKGROUND: usize = 300;
    const CEILING: u64 = 20;

    // Turned on here rather than by the environment, for the reason both modules' docs give: a
    // test that runs only when somebody remembers a variable is one the gate never runs, and this
    // one is an assertion rather than a measurement.
    //
    // **Both switches.** The counters read below are the *client's*, and its `force_on` is a
    // separate flag from `esker-sql`'s — turning on only the one whose name came to mind first
    // left every counter at zero, and a bound of "fewer than twenty" is met very comfortably by
    // nothing at all. That is what the denominator assertion underneath is for.
    esker_sql::stmt_stats::trace_every_read();
    esker_client::stmt_stats::force_on();
    let cluster = Cluster::start();
    let mut s = cluster.session();
    for at in 0..BACKGROUND {
        s.run(&format!(
            "CREATE TABLE bg{at} (id bigserial primary key, a bigint, b text)"
        ))
        .unwrap();
    }
    s.run("CREATE TABLE doomed (id bigserial primary key, a bigint)")
        .unwrap();

    esker_client::stmt_stats::reset();
    s.run("DROP TABLE IF EXISTS doomed").unwrap();
    let cost = esker_client::stmt_stats::taken();
    let reads: u64 = cost.read_heads.values().sum();
    let scans: u64 = cost.scan_heads.values().sum();
    let breakdown =
        |heads: &std::collections::BTreeMap<[u8; esker_client::stmt_stats::HEAD], u64>| {
            esker_sql::stmt_stats::name_heads(heads)
                .iter()
                .map(|(name, n)| format!("{name} x{n}"))
                .collect::<Vec<String>>()
                .join(", ")
        };
    // **The denominator.** A counter that is off reports zero, and zero passes every ceiling. This
    // is the assertion that makes the one below mean something.
    assert!(
        reads + scans > 0,
        "the read counters are off, so the bound below would pass against any implementation"
    );
    assert!(
        reads + scans < CEILING,
        "dropping one table read {reads} and scanned {scans} with {BACKGROUND} relations in the \
         catalog, which is the whole catalog rather than this table's dependants.\n  \
         reads: {}\n  scans: {}",
        breakdown(&cost.read_heads),
        breakdown(&cost.scan_heads),
    );
}
