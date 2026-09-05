//! `esker bench-mpp` — what a distributed aggregate costs, and where the time goes.
//!
//! [ADR 0022](../../../../docs/adr/0022-columnar-learner-replica.md) milestone 5 is MPP exchange,
//! and the ADR says it is *"last, and only if measured"*: two-level aggregation already finishes
//! on the SQL node, and building a shuffle before the numbers say that finish is the bottleneck
//! is optimising before profiling, which `CLAUDE.md` forbids. This command is that measurement.
//!
//! It starts a real cluster — a placement driver, N stores, one SQL node, every one of them a
//! child process — loads a seeded table, asks for a columnar copy with the same `ALTER TABLE` a
//! user would write, and then times four queries on both engines with the machine's own
//! accounting of where the CPU went.
//!
//! # The four queries, and why the control is the important one
//!
//! * `control-scan` — a full scan with a filter and **one** output row. No exchange can make it
//!   faster: there is nothing to shuffle. It is the arm that proves the others.
//! * `group-low` — a `GROUP BY` over 32 groups. The finish is a handful of rows.
//! * `group-high` — a `GROUP BY` whose answer is comparable in size to the scan. Every region
//!   ships nearly the whole group set and one node merges them all.
//! * `join` — two large tables on a key neither is partitioned by. It runs on rows whatever the
//!   session says, because [ADR 0040](../../../../docs/adr/0040-the-engine-a-query-runs-on.md)
//!   Decision 4 substitutes exactly one plan shape and a join is not it.
//!
//! Both engines run every query, interleaved and adjacent in time, three times, because a
//! block-at-a-time A/B on a shared machine measures the machine
//! (`docs/bench/columnar-m2.md`'s successor lesson, and this lane's brief).
//!
//! # What is asserted on every single run
//!
//! **The engine.** A routed plan that quietly fell back to the row engine makes every number in
//! the report meaningless, and a fallback is silent to the client by design. So every timed
//! statement is preceded by an `EXPLAIN ANALYZE` of the same statement, and a columnar arm whose
//! plan says `Engine: rows` fails the run rather than reporting it.
//!
//! **The answer.** Both arms must return the same number of rows as each other and as the shape
//! predicts. Two engines that disagree is the worst failure this feature can have (ADR 0022, "the
//! honest cost"), and a benchmark is a place it would otherwise pass unnoticed.

mod pg;
mod probe;
mod topology;
mod workload;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pg::Pg;
use topology::{Cluster, Layout};
use workload::{Kind, Query, Shape};

/// What `bench-mpp` was asked to measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchMppOptions {
    /// Where the cluster's data lives. A temporary directory when absent.
    pub(crate) dir: Option<PathBuf>,
    /// How many stores.
    pub(crate) stores: u64,
    /// Rows in the fact table.
    pub(crate) rows: u64,
    /// Distinct values of the high-cardinality grouping key.
    pub(crate) groups_high: u64,
    /// Approximate region bytes past which a leader looks for a split key — how the number of
    /// fragments is chosen.
    pub(crate) region_split_size: u64,
    /// Timed repeats of the whole interleaved set.
    pub(crate) repeats: u64,
    /// Rows per `INSERT` during the load.
    pub(crate) batch: u64,
    /// The lowest port the cluster uses.
    pub(crate) base_port: u16,
    /// The seed for the generated values.
    pub(crate) seed: u64,
    /// Keep the data directory after the run.
    pub(crate) keep: bool,
    /// Report what each statement does on each engine instead of timing anything.
    ///
    /// The correctness half of a multi-region run: it never fails on a statement, it records what
    /// the statement did — an answer or an error, with the code — so a path that breaks across a
    /// region boundary is *evidence* rather than a benchmark that stopped.
    pub(crate) diagnose: bool,
    /// Leave the join out of the timed set.
    ///
    /// It costs tens of seconds where the aggregates cost tens of milliseconds, and it cannot
    /// reach the columnar path at all, so under a time budget it is the first thing to drop:
    /// what it measures is the row engine, and the verdict turns on the aggregates.
    pub(crate) no_join: bool,
    /// How often a region's leader reports it to PD, in milliseconds.
    ///
    /// A flag rather than a constant because it is the knob that decides how fast placement
    /// happens *and* how often PD acts on a region, and those are not the same thing: the
    /// reference (`esker cluster start`) leaves it at the shipped 60 s, and a benchmark that
    /// shortens it is asking PD to make thirty times as many decisions per minute.
    pub(crate) region_heartbeat_ms: u64,
    /// The tick the heartbeat intervals are counted in, in milliseconds.
    pub(crate) heartbeat_tick_ms: u64,
}

impl Default for BenchMppOptions {
    fn default() -> Self {
        Self {
            dir: None,
            stores: 6,
            rows: 2_000_000,
            groups_high: 100_000,
            region_split_size: 32 * 1024 * 1024,
            repeats: 3,
            batch: 500,
            base_port: 24_160,
            seed: 20_260_904,
            keep: false,
            diagnose: false,
            no_join: false,
            // **The shipped defaults, which is what the working reference uses.** Shortening
            // them to 2 s places a columnar learner in seconds rather than fifty, and asks PD to
            // act on every region thirty times as often; matching `esker cluster start` keeps
            // one fewer thing different between this benchmark and the path that is known to
            // work. Setup is slower and the measured statements are unaffected.
            //
            // It is *not* known to fix the schema-lease expiry seen at 2 s: the same
            // `25006 ... schema lease has expired` came back at these intervals on a loaded
            // machine. What discriminates those runs is the machine's load, not this knob.
            region_heartbeat_ms: 60_000,
            heartbeat_tick_ms: 1_000,
        }
    }
}

/// Which engine an arm asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// `SET esker.engine = 'columnar'` — the estimate is skipped, the correctness rules are not.
    Columnar,
    /// `SET esker.engine = 'row'`, which is always possible.
    Row,
}

impl Arm {
    fn setting(self) -> &'static str {
        match self {
            Self::Columnar => "columnar",
            Self::Row => "row",
        }
    }
}

/// One timed statement.
#[derive(Debug, Clone)]
struct Run {
    query: &'static str,
    arm: Arm,
    wall: Duration,
    sql_cpu: Duration,
    store_cpu: Duration,
    loopback_bytes: u64,
    sql_peak_rss: u64,
    rows: u64,
    engine: String,
    fragments_asked: u64,
    fragments_answered: u64,
}

/// How long a columnar copy has to appear and catch up before the run gives up.
const PLACEMENT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the SQL node has to become writable before the run gives up.
const WRITABLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the cluster has to reach its replica target before the run gives up.
///
/// Short on purpose. With the heartbeat intervals this benchmark sets, a healthy cluster is at
/// three voters in seconds; a minute of it not happening is a wedge, not slowness.
const REPLICA_TARGET_TIMEOUT: Duration = Duration::from_secs(90);

/// Runs the whole measurement and prints the record.
pub(crate) fn run(options: &BenchMppOptions) -> Result<(), String> {
    if !probe::is_available() {
        return Err(
            "no /proc: this benchmark reads each process's CPU and bytes from the kernel and \
             must be run on Linux — use the container (~/workspace/lab/esker-docker/in.sh)"
                .to_owned(),
        );
    }
    if options.stores < 4 {
        return Err(format!(
            "--stores {} cannot hold a columnar learner: a region has three voters and PD places \
             a learner on a store with no peer of that region, so four is the minimum \
             (docs/bench/columnar-learner.md)",
            options.stores
        ));
    }

    let temporary = options.dir.is_none();
    let data_dir = options.dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("esker-bench-mpp-{}", std::process::id()))
    });
    let layout = Layout {
        data_dir: data_dir.clone(),
        stores: options.stores,
        base_port: options.base_port,
        region_split_size: options.region_split_size,
        region_heartbeat_ms: options.region_heartbeat_ms,
        heartbeat_tick_ms: options.heartbeat_tick_ms,
    };
    let shape = Shape {
        rows: options.rows,
        groups_high: options.groups_high,
        batch: options.batch,
        seed: options.seed,
    };

    println!("# esker bench-mpp");
    println!();
    describe(options, &layout);

    let cluster = Cluster::start(&layout)?;
    let outcome = measure(&cluster, options, shape).map_err(|why| {
        // Whatever failed, some child probably said why. Naming which one is the whole point of
        // giving each its own file.
        format!("{why}{}", cluster.logs())
    });
    let stopped = cluster.stop();
    if temporary && !options.keep {
        let _ = std::fs::remove_dir_all(&data_dir);
    } else {
        println!("data left in {}", data_dir.display());
    }
    outcome?;
    stopped
}

fn describe(options: &BenchMppOptions, layout: &Layout) {
    println!("| setting | value |");
    println!("|---|---|");
    println!("| commit | {} |", env!("CARGO_PKG_VERSION"));
    println!("| stores | {} |", options.stores);
    println!("| rows | {} |", options.rows);
    println!("| groups (high) | {} |", options.groups_high);
    println!("| groups (low) | {} |", workload::GROUPS_LOW);
    println!(
        "| region split size | {} bytes |",
        options.region_split_size
    );
    println!("| repeats | {} |", options.repeats);
    println!("| seed | {} |", options.seed);
    println!("| placement driver | {} |", layout.pd_address());
    println!("| SQL node | {} |", layout.sql_address());
    println!("| machine | {} |", probe::machine());
    println!("| load average at start | {} |", loadavg());
    println!();
}

/// Everything between a started cluster and a printed report.
fn measure(cluster: &Cluster, options: &BenchMppOptions, shape: Shape) -> Result<(), String> {
    // **Before a single row is loaded, prove the cluster reached its replica target.** A
    // benchmark whose regions have one voter is not measuring this system, and the failure is
    // silent: the SQL node answers, the load succeeds, and only the columnar wait -- five minutes
    // later, after the load -- says anything is wrong. It was wrong here for exactly that reason,
    // and this is the check that turns it into fifteen seconds and a topology.
    if options.diagnose {
        return diagnose(cluster, options, shape);
    }
    let voters = wait_for_the_replica_target(cluster)?;
    // Said as "carries", not "reached": with `--peer` the leader has the membership in hand at
    // once, so a few milliseconds here is the peer list being honoured rather than PD having
    // done anything. What it rules out is the wedge — a region stuck below its target for ever.
    println!(
        "every region carries {} voters after {voters:.1?}",
        esker_pd::schedule::TARGET_REPLICAS
    );

    // **Readiness is a node that accepts a WRITE, not one that answers.** `SELECT 1` passes
    // while the node is read-only, and a SQL node holds its schema lease for 5 s and renews it;
    // if the first renewal has not landed when the first statement arrives, every write is
    // `25006 ... schema lease has expired and the placement driver is unreachable`. That killed
    // six runs before this loop existed, including three inside a 20-minute quiet window granted
    // for the measurement. So `CREATE TABLE` is retried on a fresh connection until the node is
    // writable or the deadline passes -- a wait, not a failure.
    let mut pg = Pg::connect(&cluster.sql_address, "esker", "esker")?;
    let writable = Instant::now();
    loop {
        match workload::create(&mut pg) {
            Ok(()) => break,
            Err(why) if why.contains("25006") && writable.elapsed() < WRITABLE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(500));
                pg = Pg::connect(&cluster.sql_address, "esker", "esker")?;
            }
            Err(why) => return Err(why),
        }
    }
    println!(
        "the node accepted its first write after {:.1?}",
        writable.elapsed()
    );

    let started = Instant::now();
    let statements = workload::load(&mut pg, shape)?;
    let load = started.elapsed();
    println!(
        "loaded {} rows in {} statements in {:.1?} ({:.0} rows/s)",
        shape.rows,
        statements,
        load,
        as_float(shape.rows) / load.as_secs_f64().max(1e-9)
    );

    // The copy is asked for exactly as a user asks for it, and then waited for by *asking the
    // planner*: a learner that PD has placed but that has not caught up refuses the fragment, and
    // a run that began there would time the fallback.
    pg.run(&format!(
        "ALTER TABLE {} SET (columnar_replicas = 1)",
        workload::FACT
    ))?;
    let mut queries = workload::queries(shape);
    if options.no_join {
        queries.retain(|query| query.kind != Kind::Join);
        println!("the join is not in this run's timed set (--no-join)");
    }
    let probe_query = queries
        .iter()
        .find(|query| query.kind == Kind::LowCardinality)
        .ok_or("no low-cardinality query to wait on")?;
    let waited = wait_for_the_columns(&mut pg, probe_query)?;
    println!(
        "a columnar copy answered {probe_query:?} after {waited:.1?}",
        probe_query = probe_query.name
    );

    let regions = cluster.regions()?;
    let (with_a_learner, on_stores) = topology::columnar_spread(&regions);
    println!();
    println!(
        "| regions | {} | with a columnar learner | {with_a_learner} | on distinct stores | {on_stores} |",
        regions.len()
    );
    println!();

    // One untimed pass so the first measured repeat is not paying for a cold page cache on either
    // engine. Discarded rather than reported: a warm-up in the medians would be a slow first run
    // wearing the same name as three fast ones.
    for query in &queries {
        if query.kind == Kind::Join {
            // The warm-up exists so the first measured repeat is not paying for a cold cache,
            // and the join is measured once anyway — warming it costs two of the most expensive
            // executions in the run for a median of one that does not need them.
            continue;
        }
        for arm in [Arm::Columnar, Arm::Row] {
            let _ = one(&mut pg, cluster, query, arm)?;
        }
    }

    let mut runs: Vec<Run> = Vec::new();
    for repeat in 0..options.repeats {
        for query in &queries {
            // **The join runs once, however many repeats the aggregates get.** It is the most
            // expensive statement here by an order of magnitude — tens of seconds where the
            // aggregates are tens of milliseconds — and it is the one query that cannot reach
            // the columnar path at all (ADR 0040 Decision 4), so its repeats buy context rather
            // than evidence. Spending them would cost the aggregates their repeats, and the
            // aggregates are what the verdict turns on.
            if query.kind == Kind::Join && repeat > 0 {
                continue;
            }
            for arm in [Arm::Columnar, Arm::Row] {
                runs.push(one(&mut pg, cluster, query, arm)?);
            }
        }
    }
    println!("| load average at end | {} |", loadavg());
    println!();
    report(&runs, &queries);
    Ok(())
}

/// Statements run on each engine, with what each did, and the region map they ran against.
///
/// **Nothing here fails the run.** A statement that errors is the result; the point is to say what
/// the path does across a region boundary, and a harness that stopped at the first error would
/// report one fact where there are several.
fn diagnose(cluster: &Cluster, options: &BenchMppOptions, shape: Shape) -> Result<(), String> {
    wait_for_the_replica_target(cluster)?;
    let mut pg = Pg::connect(&cluster.sql_address, "esker", "esker")?;
    let writable = Instant::now();
    while let Err(why) = workload::create(&mut pg) {
        if !why.contains("25006") || writable.elapsed() >= WRITABLE_TIMEOUT {
            return Err(why);
        }
        std::thread::sleep(Duration::from_millis(500));
        pg = Pg::connect(&cluster.sql_address, "esker", "esker")?;
    }
    let started = Instant::now();
    let statements = workload::load(&mut pg, shape)?;
    println!(
        "loaded {} rows in {statements} statements in {:.1?}",
        shape.rows,
        started.elapsed()
    );
    let _ = pg.run(&format!(
        "ALTER TABLE {} SET (columnar_replicas = 1)",
        workload::FACT
    ));

    println!();
    println!("## The region map, from each region's own leader");
    println!();
    let regions = cluster.regions()?;
    println!("| region | start | end | peers |");
    println!("|---|---|---|---|");
    for region in &regions {
        let peers: Vec<String> = region
            .peers
            .iter()
            .map(|peer| {
                format!(
                    "{}@store{}{}",
                    peer.peer_id,
                    peer.store_id,
                    role_tag(peer.role)
                )
            })
            .collect();
        println!(
            "| {} | {} | {} | {} |",
            region.id,
            hex_head(&region.start_key),
            hex_head(&region.end_key),
            peers.join(" ")
        );
    }
    let (with_a_learner, on_stores) = topology::columnar_spread(&regions);
    println!();
    println!(
        "**{} regions**, {with_a_learner} with a columnar learner, on {on_stores} distinct stores.",
        regions.len()
    );
    if regions.len() < 2 {
        println!();
        println!(
            "> The table did **not** split. Everything below is single-region and says nothing \
             about a boundary."
        );
    }

    println!();
    println!("## What each statement does, on each engine");
    println!();
    println!("| statement | engine | outcome |");
    println!("|---|---|---|");
    for (label, sql) in workload::diagnostics(shape) {
        for engine in ["row", "columnar"] {
            let outcome = match pg.run(&format!("SET esker.engine = '{engine}'")) {
                Ok(()) => match pg.query(&sql) {
                    Ok(answer) => format!("{} row(s)", answer.rows.len()),
                    Err(why) => format!("**{}**", first_line(&why)),
                },
                Err(why) => format!("**SET failed: {}**", first_line(&why)),
            };
            println!("| {label} | {engine} | {outcome} |");
        }
    }
    println!();
    println!("| load average at end | {} |", loadavg());
    if options.keep {
        println!("(the cluster's data was kept)");
    }
    Ok(())
}

/// The role letter `esker region ls` uses.
fn role_tag(role: esker_proto::PeerRole) -> &'static str {
    match role {
        esker_proto::PeerRole::Voter => "",
        esker_proto::PeerRole::Learner => "L",
        esker_proto::PeerRole::ColumnarLearner => "C",
    }
}

/// The first eight bytes of a region bound, as hex; `+inf` for the empty upper bound.
fn hex_head(key: &[u8]) -> String {
    if key.is_empty() {
        return "(unbounded)".to_owned();
    }
    let head = key.iter().take(8).fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    });
    if key.len() > 8 {
        format!("{head}…")
    } else {
        head
    }
}

/// One line of an error, so a table cell stays a table cell.
fn first_line(text: &str) -> String {
    text.lines().last().unwrap_or(text).trim().to_owned()
}

/// Waits until every region has the cluster's replica target in voters.
///
/// PD reaches a store only by answering its region heartbeat, and it gives a region one operator
/// at a time, so a region short of voters holds the slot that a columnar learner would need
/// (`esker_pd::pd::repair`). Waiting here rather than discovering it later is the difference
/// between a named failure and a benchmark that quietly measured a single-replica cluster.
fn wait_for_the_replica_target(cluster: &Cluster) -> Result<Duration, String> {
    let started = Instant::now();
    loop {
        let regions = cluster.regions()?;
        let short: Vec<String> = regions
            .iter()
            .filter_map(|region| {
                let voters = region
                    .peers
                    .iter()
                    .filter(|peer| peer.role == esker_proto::PeerRole::Voter)
                    .count();
                (voters < esker_pd::schedule::TARGET_REPLICAS)
                    .then(|| format!("region {} has {voters}", region.id))
            })
            .collect();
        if short.is_empty() && !regions.is_empty() {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= REPLICA_TARGET_TIMEOUT {
            return Err(format!(
                "the cluster did not reach {} voters a region within {REPLICA_TARGET_TIMEOUT:?}: \
                 {}. PD places a columnar learner only after a region has its voters, so nothing \
                 would ever have been measured",
                esker_pd::schedule::TARGET_REPLICAS,
                if short.is_empty() {
                    "no regions at all".to_owned()
                } else {
                    short.join(", ")
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Waits until the planner answers `query` from the columns, or says what it kept saying instead.
fn wait_for_the_columns(pg: &mut Pg, query: &Query) -> Result<Duration, String> {
    let started = Instant::now();
    pg.run("SET esker.engine = 'auto'")?;
    loop {
        let plan = pg.query(&format!("EXPLAIN ANALYZE {}", query.sql))?.text();
        if engine_of(&plan) == "columnar" {
            return Ok(started.elapsed());
        }
        let last = engine_line(&plan);
        if started.elapsed() >= PLACEMENT_TIMEOUT {
            return Err(format!(
                "no columnar copy answered within {PLACEMENT_TIMEOUT:?}; the plan still says: {last}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// One `EXPLAIN ANALYZE` for evidence, then one timed statement, with the machine's accounting.
fn one(pg: &mut Pg, cluster: &Cluster, query: &Query, arm: Arm) -> Result<Run, String> {
    pg.run(&format!("SET esker.engine = '{}'", arm.setting()))?;

    let plan = pg.query(&format!("EXPLAIN ANALYZE {}", query.sql))?.text();
    let engine = engine_of(&plan);
    // **Asked for columnar, or asked for anything but.** Not an equality against one spelling:
    // a plan that never reaches the router prints no `Engine:` line at all, and a join is one.
    // `crates/esker-sql/src/exec/mod.rs`'s guard is `inners.is_empty()`, so `route` is not called
    // for a join and `EXPLAIN` says nothing about an engine — which is correct behaviour and
    // *not* one of the two silences ADR 0040 Decision 3 lists. An equality against "rows" here
    // would fail the join arm of every run and report it as a routing fault.
    let must_be_columnar = arm == Arm::Columnar && query.columnar_is_possible;
    if must_be_columnar != (engine == "columnar") {
        let wanted = if must_be_columnar {
            "the columns"
        } else {
            "the rows"
        };
        return Err(format!(
            "{} on the {} arm did not run on {wanted} — the plan's engine line says `{engine}`, \
             so every number in this run would be about a plan nobody asked for. The plan \
             said:\n{plan}",
            query.name,
            arm.setting()
        ));
    }
    let (asked, answered) = fragments_of(&plan);

    let (sql_before, stores_before) = cluster.sample()?;
    let net_before = probe::loopback_bytes()?;
    let (wall, rows) = pg.timed(&query.sql)?;
    let net_after = probe::loopback_bytes()?;
    let (sql_after, stores_after) = cluster.sample()?;

    let returned = u64::try_from(rows.rows.len()).unwrap_or(u64::MAX);
    if returned != query.expected_rows {
        return Err(format!(
            "{} on the {} arm answered {returned} rows and the data holds {}: the two engines \
             are not answering the same question",
            query.name,
            arm.setting(),
            query.expected_rows
        ));
    }

    let sql = sql_after.since(sql_before);
    let store_cpu = stores_after
        .iter()
        .zip(&stores_before)
        .map(|(after, before)| after.since(*before).cpu)
        .sum();
    Ok(Run {
        query: query.name,
        arm,
        wall,
        sql_cpu: sql.cpu,
        store_cpu,
        loopback_bytes: net_after.saturating_sub(net_before),
        sql_peak_rss: sql.peak_rss_bytes,
        rows: returned,
        engine,
        fragments_asked: asked,
        fragments_answered: answered,
    })
}

/// `columnar`, `rows`, or what the plan said when it said neither.
///
/// Two silences are deliberate in `EXPLAIN` — a table nobody asked for a copy of, and a node with
/// no way to ask a fragment (ADR 0040 Decision 3) — and both of them are, for this benchmark, a
/// misconfiguration rather than a result. So an absent line answers `no engine line`, which no arm
/// expects and every arm therefore refuses.
fn engine_of(plan: &str) -> String {
    let line = engine_line(plan);
    if line.contains("Engine: columnar") {
        "columnar".to_owned()
    } else if line.contains("Engine: rows") {
        "rows".to_owned()
    } else {
        line
    }
}

fn engine_line(plan: &str) -> String {
    plan.lines()
        .find(|line| line.trim_start().starts_with("Engine:"))
        .map_or_else(
            || "no engine line".to_owned(),
            |line| line.trim().to_owned(),
        )
}

/// `Fragments: N asked, M answered`, or zeroes for a plan that has no such line.
fn fragments_of(plan: &str) -> (u64, u64) {
    let Some(line) = plan
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Fragments:"))
    else {
        return (0, 0);
    };
    let numbers: Vec<u64> = line
        .split_whitespace()
        .filter_map(|word| word.parse::<u64>().ok())
        .collect();
    match numbers.as_slice() {
        [asked, answered, ..] => (*asked, *answered),
        _ => (0, 0),
    }
}

/// What the engine column prints.
///
/// A plan that never reached the router ran on the rows and said nothing about it, which is a
/// true and unhelpful thing to put in a table cell.
fn engine_text(engine: &str) -> String {
    if engine == "no engine line" {
        "rows (not routed)".to_owned()
    } else {
        engine.to_owned()
    }
}

/// The kernel's one-minute load average, or why it could not be read.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg").map_or_else(
        |error| format!("unreadable: {error}"),
        |text| {
            text.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        },
    )
}

/// The record, as the tables `docs/bench/mpp-baseline.md` carries.
fn report(runs: &[Run], queries: &[Query]) {
    println!("## Wall time, and where the CPU went");
    println!();
    println!(
        "| query | engine | fragments | rows out | wall median | wall min–max | SQL-node CPU | \
         stores' CPU | SQL-node CPU share | cluster loopback bytes | SQL-node peak RSS | runs |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for query in queries {
        for arm in [Arm::Columnar, Arm::Row] {
            let mine: Vec<&Run> = runs
                .iter()
                .filter(|run| run.query == query.name && run.arm == arm)
                .collect();
            let Some(first) = mine.first() else { continue };
            let walls = medianed(mine.iter().map(|run| run.wall.as_secs_f64()));
            let sql_cpu = median(mine.iter().map(|run| run.sql_cpu.as_secs_f64()));
            let store_cpu = median(mine.iter().map(|run| run.store_cpu.as_secs_f64()));
            let bytes = median(mine.iter().map(|run| as_float(run.loopback_bytes)));
            let peak = median(mine.iter().map(|run| as_float(run.sql_peak_rss)));
            println!(
                "| {} | {} | {} of {} | {} | {:.3} s | {:.3}–{:.3} s | {sql_cpu:.2} s | \
                 {store_cpu:.2} s | {:.0}% | {} | {} | {} |",
                query.name,
                engine_text(&first.engine),
                first.fragments_answered,
                first.fragments_asked,
                first.rows,
                walls.1,
                walls.0,
                walls.2,
                100.0 * sql_cpu / walls.1.max(1e-9),
                bytes_text(bytes),
                bytes_text(peak),
                mine.len(),
            );
        }
    }
    println!();
    println!(
        "Loopback bytes are this container's whole network namespace — the driver, the stores \
         and the SQL node — so heartbeats are in them, and the control query is what says how \
         much of that is background. Read every share beside the seconds it came from. The SQL-node CPU share is that \
         process's own `utime + stime` over the statement's wall time, so it is what one node did \
         while the query ran — the quantity an exchange moves elsewhere — and it is not a \
         speed-up, a ratio between engines, or a claim about any other workload."
    );
}

/// The min, median and max of a set of samples.
fn medianed(values: impl Iterator<Item = f64>) -> (f64, f64, f64) {
    let mut sorted: Vec<f64> = values.collect();
    sorted.sort_by(f64::total_cmp);
    let low = sorted.first().copied().unwrap_or(f64::NAN);
    let high = sorted.last().copied().unwrap_or(f64::NAN);
    (low, middle(&sorted), high)
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut sorted: Vec<f64> = values.collect();
    sorted.sort_by(f64::total_cmp);
    middle(&sorted)
}

/// The middle of a sorted slice: the lower of the two for an even count, never a mean.
///
/// A mean of two timings is a number neither run produced, and this report is about what
/// happened rather than about what would have.
fn middle(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(sorted.len() - 1) / 2]
}

/// A count as a float, for a median and a unit.
///
/// `u64` past 2^53 does not land on an `f64` exactly. A count that large here is eight petabytes
/// through one socket or eight quadrillion rows in one table, so the precision this gives up is
/// precision no run of this benchmark has — and rounding a byte total in its sixteenth digit
/// changes no reading of it.
#[allow(clippy::cast_precision_loss)]
fn as_float(count: u64) -> f64 {
    count as f64
}

fn bytes_text(bytes: f64) -> String {
    // **The exact count in brackets, always.** A byte column that rounds to `0 B` reads the same
    // whether the quantity is genuinely tiny or the instrument is broken, and this benchmark
    // printed a column of `0 B` for a join moving megabytes before the raw number said which.
    if bytes >= 1024.0 * 1024.0 {
        format!("{:.1} MiB ({bytes:.0})", bytes / (1024.0 * 1024.0))
    } else if bytes >= 1024.0 {
        format!("{:.1} KiB ({bytes:.0})", bytes / 1024.0)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{engine_of, engine_text, fragments_of, middle};

    const COLUMNAR: &str = "\
Columnar Aggregate on ledger  (4 fragments)
  Engine: columnar  (2 of 7 columns projected)
  Group Key: g32
  Fragments: 4 asked, 4 answered
  Stripes: 31 of 31 read   Chunks: 62   Rows: 2000000 scanned, 2000000 matched";

    #[test]
    fn a_plan_that_fell_back_is_not_read_as_the_one_that_was_asked_for() {
        let fell_back = "Aggregate on ledger\n  Engine: rows  (columnar refused: too far behind)\n  Seq Scan on ledger";
        assert_eq!(engine_of(COLUMNAR), "columnar");
        assert_eq!(engine_of(fell_back), "rows");
    }

    #[test]
    fn a_plan_with_no_engine_line_is_neither_engine() {
        // ADR 0040 Decision 3's two deliberate silences. Neither arm expects this string, so a
        // node with no fragment service fails the run instead of reporting a number about rows
        // while claiming to be about columns.
        let silent = "Aggregate on ledger\n  Seq Scan on ledger";
        assert_eq!(engine_of(silent), "no engine line");
    }

    #[test]
    fn the_fragment_counts_are_the_two_numbers_on_their_own_line() {
        assert_eq!(fragments_of(COLUMNAR), (4, 4));
        assert_eq!(fragments_of("Aggregate on ledger"), (0, 0));
    }

    #[test]
    fn a_join_is_not_read_as_a_routing_fault() {
        // `exec/mod.rs` only calls `route` when the select has no joins, so a join's plan carries
        // no engine line. That is the rows, not a failure, and the report says so.
        assert_eq!(
            engine_of("Aggregate\n  Nested Loop\n    Seq Scan on ledger"),
            "no engine line"
        );
        assert_eq!(engine_text("no engine line"), "rows (not routed)");
        assert_eq!(engine_text("columnar"), "columnar");
    }

    #[test]
    fn a_median_is_a_value_that_was_measured() {
        assert!((middle(&[1.0, 2.0, 3.0]) - 2.0).abs() < f64::EPSILON);
        assert!(
            (middle(&[1.0, 4.0]) - 1.0).abs() < f64::EPSILON,
            "an even count takes the lower sample, never their mean"
        );
        assert!(middle(&[]).is_nan());
    }
}
