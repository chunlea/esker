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

/// **#58 round 3 — a thousand rounds, catalog pinned, history growing.**
///
/// Rounds 1 and 2 both came back flat, and both were too short to be evidence: ten rounds of
/// `CREATE` / `DROP` at a fixed catalog size moved nothing, which says only that ten rounds is not
/// history. r1 measures the climb across eight *files* — tens of thousands of statements — so this
/// is the same shape run long enough to have a chance of showing it, in process, where a checkpoint
/// costs nothing.
///
/// # What grows and what is held still
///
/// Each round creates a table, writes rows into it, and drops it. The **catalog size is pinned** —
/// the create and the drop cancel — while the **stored history grows without bound**, because
/// nothing is ever collected: no safepoint is ever published, so every version and every tombstone
/// of every round is still on disk (ADR 0110). That is r1's arm A in miniature: identical work per
/// round against an ever-deeper store.
///
/// # What is recorded, and why steps and seeks rather than time
///
/// Every hundredth round, around the `DROP` alone: the reads and scans the statement issued
/// **grouped by catalog kind**, the **entries the engine stepped**, the **seeks it made**, and the
/// milliseconds. The first three are deterministic; the last is the one a loaded machine ruins, and
/// r1 has already lost two arms to it.
///
/// The three answer different questions. Steps rising means each read walks further — data. Seeks
/// rising with steps flat means the tree got deeper or more fragmented — structure. Both flat with
/// the milliseconds rising means neither, and the cost is per-operation: allocation, cache
/// residency, arena growth.
///
/// Ignored by default: it is a measurement, and a long one. `ROUNDS`, `BACKGROUND`, `ROWS` and
/// `CHECKPOINT` override the defaults, which is how the slope was bisected against the rows.
#[test]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
fn a_thousand_rounds_of_history() {
    // Env-driven so the slope can be bisected against the one input that plausibly drives it —
    // rows written per round — without a rebuild between arms.
    let rounds: usize = env_or("ROUNDS", 1_000);
    let background: usize = env_or("BACKGROUND", 40);
    let rows_per_round: usize = env_or("ROWS", 20);
    let checkpoint_every: usize = env_or("CHECKPOINT", 100);
    // Zero, the default, never fires: `round` counts from one.
    let collect_at: usize = env_or("COLLECT_AT", 0);

    esker_sql::stmt_stats::trace_every_read();
    esker_client::stmt_stats::force_on();
    let cluster = Cluster::start();
    let mut s = cluster.session();
    for at in 0..background {
        s.run(&format!(
            "CREATE TABLE bg{at} (id bigserial primary key, a bigint, b text)"
        ))
        .unwrap();
    }

    println!("  rounds={rounds} background={background} rows={rows_per_round}");
    println!("  round      ms  reads scans   steps  seeks  steps/read");
    for round in 1..=rounds {
        s.run("CREATE TABLE t (id bigserial primary key, a bigint, b text)")
            .unwrap();
        for row in 0..rows_per_round {
            s.run(&format!("INSERT INTO t (a, b) VALUES ({row}, 'r{round}')"))
                .unwrap();
        }

        // **The decisive arm**: collect at a chosen round and see whether the slope resets. If it
        // does, what grows is reclaimable history rather than anything the read path can fix.
        if round == collect_at {
            cluster.collect_everything();
        }
        let checkpoint = round % checkpoint_every == 0 || round == 1;
        let (steps_before, seeks_before) = (
            cluster.engine_counter("esker.entries-stepped"),
            cluster.engine_counter("esker.seeks"),
        );
        esker_client::stmt_stats::reset();
        let began = std::time::Instant::now();
        s.run("DROP TABLE IF EXISTS t").unwrap();
        let took = began.elapsed();
        if !checkpoint {
            continue;
        }
        let cost = esker_client::stmt_stats::taken();
        let reads: u64 = cost.read_heads.values().sum();
        let scans: u64 = cost.scan_heads.values().sum();
        let steps = cluster.engine_counter("esker.entries-stepped") - steps_before;
        let seeks = cluster.engine_counter("esker.seeks") - seeks_before;
        let per_read = steps.checked_div(reads + scans).unwrap_or(0);
        println!(
            "  {round:>5}  {:>6.1}  {reads:>5} {scans:>5}  {steps:>6} {seeks:>6}  {per_read:>10}",
            took.as_secs_f64() * 1_000.0,
        );
    }
}

/// **#63 — the three catalog-walking shapes, by kind, against a catalog that grows.**
///
/// r1 priced them in run 127 attempt 3: `DROP EXTENSION … CASCADE` **17.0 s** a statement over 563
/// round trips, the `pg_class` listing 3.77 s over 382.7, column introspection 2.99 s over 102 —
/// against `CREATE EXTENSION`'s **3.2 ms**, which is the same catalog and the opposite direction.
/// Round trips tracked keys one for one in all three (279 reads + 280 scans = 559 against 563
/// trips), and the count barely moved across all 44 `DROP EXTENSION`s, which is the signature of a
/// fixed region being walked rather than a dependency set being followed.
///
/// # Why by kind, and why three sizes
///
/// This is #61's instrument pointed at them, and #61 was found exactly this way: `'t' x152` and
/// `'q' x152` at 150 relations is a per-relation walk, and no amount of staring at a duration says
/// so. One count is ambiguous — it could be the statement's own fixed price — and a **slope** is
/// not: reads that rise with the catalog are a walk of it, reads that do not are a fixed cost that
/// has to be found somewhere else. So `BACKGROUND` is read from the environment and the arms are
/// three runs of one binary rather than one run that builds three catalogs and measures the last.
///
/// The control is in the list on purpose. `CREATE EXTENSION` costs 5,300× less than the `DROP` of
/// the same name on the same catalog; if its counts stay flat while the others climb, the climb
/// belongs to what dropping does and not to extensions, to the session, or to the store.
#[test]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
fn what_the_catalog_walkers_read() {
    let background: usize = env_or("BACKGROUND", 150);

    // Both switches, for the reason `dropping_one_table_does_not_read_every_relation` gives at
    // length: with either one off every number below is zero, and zero is a very calm-looking
    // measurement.
    esker_sql::stmt_stats::trace_every_read();
    esker_client::stmt_stats::force_on();
    assert!(
        esker_sql::stmt_stats::enabled(),
        "the read tap is off, so every count below would be zero"
    );

    let cluster = Cluster::start();
    let mut s = cluster.session();
    for at in 0..background {
        s.run(&format!(
            "CREATE TABLE bg{at} (id bigserial primary key, a bigint, b text)"
        ))
        .unwrap();
    }
    // One table whose column the extension's type owns, so the `CASCADE` has something to cascade
    // to. Without it the drop is a no-op and measures nothing — `drop_extension_columns` returns
    // early when the extension provides no types at all, and a drop with nothing to take is not
    // the statement the suite sends.
    s.run("CREATE EXTENSION IF NOT EXISTS citext").unwrap();
    s.run("CREATE TABLE holder (id bigserial primary key, tag citext)")
        .unwrap();

    println!("\n  BACKGROUND={background} relations in the catalog");
    println!(
        "\n  -- the control: CREATE EXTENSION (3.2 ms in run 127) --\n  {}",
        where_the_reads_went(&mut s, "CREATE EXTENSION IF NOT EXISTS hstore")
    );
    println!(
        "\n  -- pg_class listing (3,770 ms, 382.7 trips) --\n  {}",
        where_the_reads_went(
            &mut s,
            "SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = ANY (current_schemas(false)) AND c.relkind IN ('r','v','m','p','f')"
        )
    );
    println!(
        "\n  -- column introspection of ONE table (2,990 ms, 102 trips) --\n  {}",
        where_the_reads_went(
            &mut s,
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
             a.attrelid = 'holder'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY \
             a.attnum"
        )
    );
    // Last, because it is the one that changes the catalog it is measured against.
    println!(
        "\n  -- DROP EXTENSION … CASCADE (16,999 ms, 563 trips) --\n  {}",
        where_the_reads_went(&mut s, "DROP EXTENSION IF EXISTS citext CASCADE")
    );

    // And the same four in the units a duration is priced in, so the round-trip half of r1's
    // observation — one key per trip — can be read off the same run.
    println!("\n  -- the same four, priced --");
    for sql in [
        "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",
        "SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = ANY (current_schemas(false)) AND c.relkind IN ('r','v','m','p','f')",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
         a.attrelid = 'holder'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "DROP EXTENSION IF EXISTS hstore CASCADE",
    ] {
        println!("  {}", priced(&mut s, sql));
    }
}

/// One `usize` from the environment, or `fallback`. For the probe above, whose arms differ only in
/// their inputs — a rebuild between them would measure the compiler as well.
fn env_or(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|it| it.parse().ok())
        .unwrap_or(fallback)
}
