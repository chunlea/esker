//! No schema job is ever orphaned.
//!
//! `docs/plans/debt-c2.md`, and the sentence it retires from
//! [ADR 0020](../../docs/adr/0020-online-schema-change.md): *"Automatic re-drive ... is future
//! work"*. Until now a `CREATE INDEX CONCURRENTLY` whose node died sat at whatever state it had
//! reached until a human called `esker_schema_step`. Nothing was lost and nothing was unsafe —
//! the record and its cursor are durable and the index is not readable until `public` — but the
//! change never finished, and "a human notices" is not a liveness mechanism.
//!
//! Every node now runs a [`ReDriver`]. These are the five things it has to get right, and the
//! two it must not do.
//!
//! # The cluster here is one process
//!
//! Two nodes over one `MemoryBackend`: separate catalog caches, separate re-drivers, separate
//! leases, shared storage — which is what "two nodes" means for everything these tests are
//! about. The storage is real MVCC with real write-write conflict detection, which is what makes
//! the racing test a race rather than a mime of one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use esker_sql::backend::{Backend, MemoryBackend, StepInterval, Txn};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::exec::redrive::ReDriver;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};

const TENANT: u64 = 1;
/// Long enough to be recognisable in a report and never actually waited: these tests drive
/// passes by hand, which is the whole reason `pass()` is separate from `run()`.
const STEP_MS: u64 = 8_000;
/// The retention window, which only a *removing* change's last step waits.
const REMOVAL_EXTRA_MS: u64 = 24_000;

/// One node's backend: the cluster's storage, and this node's own lease and interval.
///
/// Separate per node because both are per node in the real thing — one node losing PD is exactly
/// the case the lease exists for, and a test that could not take one node's lease away could not
/// tell fail-closed from fail-open.
#[derive(Debug)]
struct NodeBackend {
    storage: Arc<MemoryBackend>,
    held: AtomicBool,
    /// Whether this node is told an interval at all. A node with no placement driver writes
    /// freely and must not re-drive; the two defaults go opposite ways on purpose.
    publishes: bool,
}

impl Backend for NodeBackend {
    fn begin(&self) -> esker_sql::Result<Box<dyn Txn>> {
        self.storage.begin()
    }
    fn begin_at(&self, start_ts: u64) -> esker_sql::Result<Box<dyn Txn>> {
        self.storage.begin_at(start_ts)
    }
    fn now(&self) -> esker_sql::Result<u64> {
        self.storage.now()
    }
    fn schema_lease_remaining(&self) -> Option<std::time::Duration> {
        self.held
            .load(Ordering::Relaxed)
            .then_some(std::time::Duration::from_millis(STEP_MS))
    }
    fn schema_step_interval(&self) -> Option<StepInterval> {
        (self.publishes && self.held.load(Ordering::Relaxed)).then_some(StepInterval {
            step_ms: STEP_MS,
            removal_extra_ms: REMOVAL_EXTRA_MS,
        })
    }
}

/// One SQL node: a session executor and the re-driver that runs beside it.
struct Node {
    backend: Arc<NodeBackend>,
    catalog: Arc<Catalog>,
    executor: Executor,
    redriver: ReDriver,
}

impl Node {
    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.map(|bytes| String::from_utf8(bytes).unwrap()))
                        .collect()
                })
                .collect(),
            other @ Outcome::Done { .. } => panic!("expected rows, got {other:?}"),
        }
    }

    /// One `esker_schema_step`, as an interactive driver takes it.
    fn step(&mut self, index: &str) -> String {
        self.rows(&format!("SELECT esker_schema_step('{index}')"))[0][0]
            .clone()
            .unwrap()
    }

    fn table(&mut self, name: &str) -> esker_sql::catalog::TableDef {
        let txn = self.backend.begin().unwrap();
        let view = self.catalog.view(&*txn, TENANT).unwrap();
        (*view.table(name).unwrap().unwrap()).clone()
    }

    fn index_state(&mut self, table: &str) -> esker_sql::catalog::SchemaState {
        self.table(table).indexes[0].state
    }

    /// The job's durable backfill cursor, or `None` once the job is forgotten.
    fn cursor(&mut self, table: &str) -> Option<Vec<u8>> {
        let index_id = self.table(table).indexes.first()?.id;
        let txn = self.backend.begin().unwrap();
        Some(
            esker_sql::catalog::job(&*txn, TENANT, index_id)
                .unwrap()?
                .cursor,
        )
    }

    fn jobs(&mut self) -> Vec<Vec<Option<String>>> {
        self.rows("SELECT * FROM esker_schema_jobs()")
    }

    fn index_entries(&mut self, table: &str) -> usize {
        let def = self.table(table);
        let txn = self.backend.begin().unwrap();
        let (start, end) = esker_sql::row::index_range(TENANT, def.id, def.indexes[0].id);
        txn.scan(&start, &end, 0).unwrap().len()
    }
}

/// One store, however many nodes over it.
struct Cluster {
    storage: Arc<MemoryBackend>,
}

impl Cluster {
    fn new() -> Self {
        Cluster {
            storage: Arc::new(MemoryBackend::new()),
        }
    }

    /// A node that is told the interval, and so may re-drive.
    fn node(&self) -> Node {
        self.node_with(true)
    }

    fn node_with(&self, publishes: bool) -> Node {
        let backend = Arc::new(NodeBackend {
            storage: Arc::clone(&self.storage),
            held: AtomicBool::new(true),
            publishes,
        });
        let catalog = Arc::new(Catalog::new());
        Node {
            executor: Executor::new(
                Arc::clone(&backend) as Arc<dyn Backend>,
                Arc::clone(&catalog),
                TENANT,
            ),
            redriver: ReDriver::new(
                Arc::clone(&backend) as Arc<dyn Backend>,
                Arc::clone(&catalog),
                TENANT,
            ),
            backend,
            catalog,
        }
    }
}

/// Rows enough that the backfill is several batches, so "resumed" and "restarted" differ.
///
/// `job::BATCH_ROWS` is 256, so 600 rows is three batches: one before the orphaning and two
/// after, and a restart would be three after.
const ROWS: i64 = 600;

fn table_with_rows(node: &mut Node) {
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    let values: Vec<String> = (1..=ROWS)
        .map(|id| format!("({id}, {})", id * 10))
        .collect();
    node.run(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
}

/// Passes until the job is gone, or a panic. Returns every step taken, in order.
fn drive_out(node: &mut Node) -> Vec<String> {
    let mut said = Vec::new();
    for _ in 0..200 {
        let pass = node.redriver.pass().unwrap();
        assert!(pass.failed.is_empty(), "{:?}", pass.failed);
        said.extend(pass.steps.iter().map(|step| step.said.clone()));
        if pass.jobs == 0 {
            return said;
        }
    }
    panic!("the re-driver never finished the job: {said:?}");
}

// --- (a) A job orphaned between the states -----------------------------------------------------

/// The node that started the change dies between two states. Another node's re-driver finishes
/// it, all the way to `public`, and the index it ends with is the complete one.
#[test]
fn a_job_orphaned_between_states_is_finished_by_another_node() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    // A dies here, holding nothing: the job record and its cursor are in the catalog.
    drop(a);

    let said = drive_out(&mut b);
    assert_eq!(
        said.last().map(String::as_str),
        Some("public"),
        "the re-driver did not take it to public: {said:?}"
    );
    assert_eq!(
        b.index_state("t"),
        esker_sql::catalog::SchemaState::Public,
        "the states did not end where the answer said they did"
    );
    assert_eq!(
        b.index_entries("t"),
        usize::try_from(ROWS).unwrap(),
        "every row that predated the index is in it"
    );
    assert!(b.jobs().is_empty(), "a finished job is forgotten");
    // And the index answers, which is the point of building it.
    assert_eq!(
        b.rows("SELECT id FROM t WHERE a = 3000"),
        [[Some("300".to_owned())]]
    );
}

// --- (b) A job orphaned inside the backfill ----------------------------------------------------

/// The same, orphaned mid-backfill — and the backfill is **resumed from the durable cursor**
/// rather than started again.
///
/// That distinction is the whole reason the cursor is in the catalog rather than in the node's
/// memory (`crate::exec::job`, "the backfill is many transactions"): on a table big enough to
/// need a job at all, a backfill that restarts on every failure is one that never finishes. So
/// the assertion is a count, not a shrug — three batches of work exist, one was done before the
/// orphaning, and the re-driver must do two.
#[test]
fn a_job_orphaned_mid_backfill_resumes_from_its_cursor() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    assert_eq!(a.step("ti"), "write-only");
    assert_eq!(a.step("ti"), "backfilling", "one batch, and then A dies");

    let orphaned_at = a.cursor("t").unwrap();
    assert!(
        !orphaned_at.is_empty(),
        "the batch left no cursor, so this test cannot tell resuming from restarting"
    );
    let done_before = b.index_entries("t");
    assert!(done_before > 0 && done_before < usize::try_from(ROWS).unwrap());
    drop(a);

    // Two passes to adopt it — the first is only a sighting — and then it drives.
    let mut batches = 0;
    let mut resumed_from = None;
    for _ in 0..200 {
        if resumed_from.is_none() && b.cursor("t").is_some() {
            resumed_from = b.cursor("t");
        }
        let pass = b.redriver.pass().unwrap();
        assert!(pass.failed.is_empty(), "{:?}", pass.failed);
        batches += pass.steps.iter().map(|step| step.batches).sum::<usize>();
        if pass.jobs == 0 {
            break;
        }
    }

    assert_eq!(
        resumed_from.as_deref(),
        Some(orphaned_at.as_slice()),
        "the re-driver did not pick the cursor up where the dead node left it"
    );
    assert_eq!(b.index_state("t"), esker_sql::catalog::SchemaState::Public);
    assert_eq!(b.index_entries("t"), usize::try_from(ROWS).unwrap());
    let a_whole_backfill = (usize::try_from(ROWS).unwrap()).div_ceil(256);
    assert!(
        batches < a_whole_backfill,
        "the backfill was restarted, not resumed: {batches} batches for a table that needs \
         {a_whole_backfill} from scratch, and one of them was already done"
    );
}

// --- (d) A live driver is not interrupted ------------------------------------------------------

/// While the node that owns the change is still stepping it, every other node's re-driver does
/// **nothing**: no double-stepping, and no stepping early.
///
/// Both halves matter and they fail differently. A double step would move a state a second time
/// — `advance_index_state` refuses two at once, so it would fail loudly — but stepping *early*
/// would not: it would move a state before the interval that bounds how stale a writer can be
/// had passed, which is exactly the two-version invariant ADR 0020 rests on, and nothing would
/// say so at the time.
///
/// So the assertion is on the states themselves, after every pass: the job is where A left it
/// and nowhere further.
#[test]
fn a_live_driver_is_never_second_guessed() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();

    // A is alive and stepping, and B passes between every one of A's steps.
    for expected in ["delete-only", "write-only"] {
        assert_eq!(a.step("ti"), expected);
        let after_a = b.index_state("t");
        let pass = b.redriver.pass().unwrap();
        assert_eq!(pass.jobs, 1, "the re-driver did not see the job at all");
        assert!(
            pass.steps.is_empty(),
            "the re-driver stepped a job whose driver is alive: {:?}",
            pass.steps
        );
        assert_eq!(
            b.index_state("t"),
            after_a,
            "the state moved without A moving it"
        );
    }

    // And when A does die, B steps at its very next pass — not sooner. B watched the job across
    // A's last step, so it knows a whole pass has gone by since; that is the difference between
    // this node and the one below.
    drop(a);
    let adopted = b.redriver.pass().unwrap();
    assert_eq!(
        adopted.steps.len(),
        1,
        "a job that has been idle for a whole pass was not adopted"
    );
}

/// A node seeing a job for the **first time** does not step it, however idle it looks.
///
/// The one thing a first sighting cannot tell you is how long it has been that way. A job stepped
/// a millisecond ago and a job stepped an hour ago look identical, and only one of them may be
/// stepped — so a re-driver that has just started, or has just learned about a job, waits a whole
/// pass before it touches anything. That costs one interval on a node restart and it is the
/// difference between "late" and "early", which is the difference between slow and wrong.
#[test]
fn a_job_seen_for_the_first_time_is_never_stepped_on_sight() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut fresh = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    drop(a);

    let first = fresh.redriver.pass().unwrap();
    assert_eq!(first.jobs, 1, "the job was not even seen");
    assert!(
        first.steps.is_empty(),
        "stepped on first sight, when it could not know the interval had passed"
    );
    assert_eq!(
        fresh.index_state("t"),
        esker_sql::catalog::SchemaState::DeleteOnly
    );
    let second = fresh.redriver.pass().unwrap();
    assert_eq!(
        second.steps.len(),
        1,
        "a job idle for a whole pass was not adopted"
    );
}

// --- (e) Fail closed ---------------------------------------------------------------------------

/// A node whose lease has lapsed does not re-drive, however idle the job looks.
///
/// ADR 0028: a node that has stopped hearing from PD may be acting on a schema the cluster has
/// moved two states beyond, and stepping is the most consequential thing it could do with a
/// stale one. The executor refuses its writes anyway — but a re-driver that tried and was
/// refused one layer down would be a node doing work it must not do and finding out afterwards,
/// which is fail-open with a safety net rather than fail-closed.
#[test]
fn a_node_past_its_lease_does_not_re_drive() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    drop(a);

    b.backend.held.store(false, Ordering::Relaxed);
    for _ in 0..10 {
        let pass = b.redriver.pass().unwrap();
        assert!(
            pass.steps.is_empty() && pass.failed.is_empty(),
            "a node past its lease re-drove: {pass:?}"
        );
    }
    assert_eq!(
        b.index_state("t"),
        esker_sql::catalog::SchemaState::DeleteOnly,
        "the job moved while every node that could move it had lapsed"
    );

    // And it picks up again when the lease comes back, so this is fail-*closed* and not fail-dead.
    b.backend.held.store(true, Ordering::Relaxed);
    let said = drive_out(&mut b);
    assert_eq!(said.last().map(String::as_str), Some("public"), "{said:?}");
}

/// A node that is told no interval does not re-drive either, and for a different reason: it is
/// not that it may not step, it is that it does not know how long to wait first.
///
/// The two defaults go opposite ways on purpose. A node with no lease source **writes** — "nobody
/// is coordinating" is not a reason to stop. A node with no interval source does not **step**,
/// because the only way to step without an interval is to invent one, and an invented interval
/// that is short is the unsafety the number exists to prevent.
#[test]
fn a_node_with_no_interval_does_not_guess_one() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut nodriver = cluster.node_with(false);

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    drop(a);

    for _ in 0..10 {
        let pass = nodriver.redriver.pass().unwrap();
        assert_eq!(pass, esker_sql::exec::redrive::Pass::default());
    }
    assert_eq!(
        nodriver.index_state("t"),
        esker_sql::catalog::SchemaState::DeleteOnly
    );
    // It still writes, which is the half that must not change.
    nodriver.run("INSERT INTO t VALUES (99999, 1)").unwrap();
}

// --- (c) Two re-drivers racing -----------------------------------------------------------------

/// Every node runs a re-driver, so every orphaned job has as many drivers as the cluster has
/// nodes. Nothing coordinates them, on purpose.
///
/// # Why there is no lock
///
/// Because there is already one. A step is a **catalog transaction**: two that overlap write the
/// same table record, first-committer-wins, and the loser gets `40001` and looks again. A lock
/// would be a second mechanism that has to agree with the first, and it would have a holder that
/// can die — which is the failure the re-driver exists to survive.
///
/// # What is asserted, and why it is a count
///
/// The dangerous outcome of a race is not a crash, it is a state moving **twice as fast**: two
/// drivers each taking one legal step, an interval apart in nobody's clock, and the two-version
/// invariant broken with nothing saying so. It is invisible at the time and it is only visible
/// as a count. So the count is the assertion: from `delete-only`, exactly two transitions exist
/// on the way to `public`, and exactly two are taken however many passes race.
///
/// `advance` refuses the sequential half of that race — a driver that read `delete-only` will not
/// write a state on top of somebody else's `write-only` — and `40001` refuses the concurrent
/// half. Both are needed; either alone leaves the other open.
#[test]
fn two_re_drivers_racing_take_each_step_exactly_once() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();
    let mut c = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    drop(a);

    // Both sight it in the same pass, so both find it idle in the next one and go at once.
    assert!(b.redriver.pass().unwrap().steps.is_empty());
    assert!(c.redriver.pass().unwrap().steps.is_empty());

    let mut moved: Vec<String> = Vec::new();
    let mut overtaken = 0_usize;
    let mut conflicts = 0_usize;
    for _ in 0..200 {
        let gate = std::sync::Barrier::new(2);
        let (from_b, from_c) = std::thread::scope(|scope| {
            let bd = &mut b.redriver;
            let cd = &mut c.redriver;
            let one = scope.spawn(|| {
                gate.wait();
                bd.pass()
            });
            let two = scope.spawn(|| {
                gate.wait();
                cd.pass()
            });
            (one.join().unwrap(), two.join().unwrap())
        });

        let mut done = true;
        for pass in [&from_b, &from_c] {
            let pass = pass.as_ref().expect("a pass itself must not fail");
            for failure in &pass.failed {
                assert_eq!(
                    failure.sqlstate,
                    esker_sql::sqlstate::SERIALIZATION_FAILURE,
                    "a racing step failed for a reason that is not a lost race: {failure:?}"
                );
                conflicts += 1;
            }
            for step in &pass.steps {
                if step.moved {
                    moved.push(step.said.clone());
                } else {
                    overtaken += 1;
                }
            }
            done &= pass.jobs == 0;
        }
        if done {
            break;
        }
    }

    println!(
        "two re-drivers: {} transitions, {overtaken} overtaken, {conflicts} rolled back",
        moved.len()
    );
    assert_eq!(
        moved,
        ["write-only", "public"],
        "the two transitions between delete-only and public were not taken exactly once each"
    );
    assert_eq!(b.index_state("t"), esker_sql::catalog::SchemaState::Public);
    assert_eq!(b.index_entries("t"), usize::try_from(ROWS).unwrap());
    assert!(b.jobs().is_empty(), "a finished job is forgotten");
    // Whichever way the race went, one of the two did not get to take the step — that is the
    // serialisation working, and a run where it never happened has not tested it.
    assert!(
        overtaken + conflicts > 0,
        "the two re-drivers never actually collided, so nothing about racing was tested"
    );
}
