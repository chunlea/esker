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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_sql::backend::{Backend, MemoryBackend, StepInterval, Txn};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::exec::redrive::{Pass, ReDriver};
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
    /// Where this node's transactions are held, so a test can decide an interleaving instead of
    /// hoping for one. [`Hold::Open`] for every node until a test says otherwise.
    gate: Arc<Gate>,
}

impl Backend for NodeBackend {
    fn begin(&self) -> esker_sql::Result<Box<dyn Txn>> {
        // Reported **before** the snapshot is taken, which is what makes `Hold::Begin` mean what
        // it says: a driver released from there reads whatever the other one has committed since.
        self.gate.arrive(Edge::Begin);
        Ok(Box::new(GatedTxn {
            inner: self.storage.begin()?,
            gate: Arc::clone(&self.gate),
        }))
    }
    fn begin_at(&self, start_ts: u64) -> esker_sql::Result<Box<dyn Txn>> {
        self.storage.begin_at(start_ts)
    }
    fn now(&self) -> esker_sql::Result<u64> {
        self.storage.now()
    }
    fn schema_lease_remaining(&self) -> Option<Duration> {
        self.held
            .load(Ordering::Relaxed)
            .then_some(Duration::from_millis(STEP_MS))
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
            gate: Arc::new(Gate::default()),
        });
        let catalog = Arc::new(Catalog::new());
        Node {
            executor: Executor::new(
                Arc::clone(&backend) as Arc<dyn Backend>,
                Arc::clone(&catalog),
                TENANT,
                esker_sql::session::register(),
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

// --- The gate: an interleaving the test decides, rather than one it hopes for --------------------

/// Which edge of a transaction a driver reports before it takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// About to take a snapshot.
    Begin,
    /// About to commit, with its snapshot already taken and its writes already buffered.
    Commit,
}

/// Where a held driver is stopped.
///
/// # Why a `Barrier` around two passes was not enough
///
/// The racing test below used to release two `pass()` calls from a `std::sync::Barrier` and count
/// what came back. That synchronises the moment each pass *starts*, and a pass is not a step: it is
/// a catalog scan, a table read and, at write-only, a whole backfill. On a loaded machine one pass
/// runs to completion before the other is scheduled at all — and the second then finds the job's
/// fingerprint changed, starts its wait over and takes **no step**, so nothing collides. Sixty-eight
/// of eighty runs under load ended `2 transitions, 0 overtaken, 0 rolled back`, which is the test's
/// own "nothing about racing was tested" guard firing: correct, and useless as a gate.
///
/// What "these two overlap" is actually about is the two edges of a *transaction*, so those are
/// what is held. Nothing here waits on a duration and nothing is probabilistic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Hold {
    /// Nothing is held. The default, so every other test in this file runs exactly as before.
    #[default]
    Open,
    /// Let `n` transactions begin, then hold the driver **before it takes the next snapshot**.
    ///
    /// Released, it reads whatever the other driver committed while it waited — the *sequential*
    /// half of the race, which `job::advance`'s `from` check and `step_job`'s `expected` are what
    /// refuse.
    Begin(usize),
    /// Let `n` transactions commit, then hold the driver **between its last read and its commit**.
    ///
    /// Its snapshot is already taken, so releasing it commits on top of a key the other driver has
    /// written since — the *concurrent* half, which first-committer-wins is what refuses.
    Commit(usize),
}

/// One node's transactions, held where the test says.
#[derive(Debug, Default)]
struct Gate {
    state: Mutex<GateState>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    hold: Hold,
    begins: usize,
    commits: usize,
    /// Whether a transaction is parked right now. What [`Gate::wait_until_parked`] waits for, so
    /// the other thread waits on the **event** rather than on a duration.
    parked: bool,
}

/// Long enough that a loaded machine is not mistaken for a driver that will never arrive, and
/// short enough to fail a run rather than hang a suite.
const GATE_DEADLINE: Duration = Duration::from_secs(30);

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts holding, counting from zero.
    fn arm(&self, hold: Hold) {
        let mut state = self.lock();
        *state = GateState {
            hold,
            ..GateState::default()
        };
    }

    /// Lets everything through, now and afterwards.
    fn open(&self) {
        let mut state = self.lock();
        state.hold = Hold::Open;
        state.parked = false;
        self.wake.notify_all();
    }

    /// Blocks until a transaction is actually parked.
    ///
    /// **Panics rather than carrying on** if none arrives: a test that proceeded anyway would be
    /// measuring the schedule this whole mechanism exists to replace, and it would report that as
    /// a pass.
    fn wait_until_parked(&self) {
        let deadline = Instant::now() + GATE_DEADLINE;
        let mut state = self.lock();
        while !state.parked {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "the held driver never reached {:?}: {} begins, {} commits",
                state.hold,
                state.begins,
                state.commits
            );
            state = self
                .wake
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Called by a transaction at each of its edges; parks the caller when the edge is the held
    /// one.
    fn arrive(&self, edge: Edge) {
        let mut state = self.lock();
        match edge {
            Edge::Begin => state.begins += 1,
            Edge::Commit => state.commits += 1,
        }
        let park = match (state.hold, edge) {
            (Hold::Begin(n), Edge::Begin) => state.begins > n,
            (Hold::Commit(n), Edge::Commit) => state.commits > n,
            _ => false,
        };
        if !park {
            return;
        }
        state.parked = true;
        self.wake.notify_all();
        while state.parked {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A transaction that tells its node's gate where it is before it goes there.
#[derive(Debug)]
struct GatedTxn {
    inner: Box<dyn Txn>,
    gate: Arc<Gate>,
}

impl Txn for GatedTxn {
    fn get(&self, key: &[u8]) -> esker_sql::Result<Option<Bytes>> {
        self.inner.get(key)
    }
    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> esker_sql::Result<Vec<(Bytes, Bytes)>> {
        self.inner.scan(start, end, limit)
    }
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.inner.put(key, value);
    }
    fn delete(&mut self, key: &[u8]) {
        self.inner.delete(key);
    }
    fn buffered(&self, key: &[u8]) -> esker_sql::backend::Buffered {
        self.inner.buffered(key)
    }
    fn restore(&mut self, key: &[u8], prior: esker_sql::backend::Buffered) {
        self.inner.restore(key, prior);
    }
    fn holds(&self, key: &[u8]) -> bool {
        self.inner.holds(key)
    }
    fn unlock(&mut self, key: &[u8]) {
        self.inner.unlock(key);
    }
    fn read_set(&self) -> esker_sql::backend::ReadSet {
        self.inner.read_set()
    }
    fn restore_read_set(&mut self, set: esker_sql::backend::ReadSet) {
        self.inner.restore_read_set(set);
    }
    fn start_ts(&self) -> u64 {
        self.inner.start_ts()
    }
    fn has_written(&self) -> bool {
        self.inner.has_written()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn commit(self: Box<Self>) -> esker_sql::Result<Option<u64>> {
        self.gate.arrive(Edge::Commit);
        self.inner.commit()
    }
    fn rollback(self: Box<Self>) -> esker_sql::Result<()> {
        self.inner.rollback()
    }
}

/// Runs two drivers' passes so that `running`'s **whole pass** happens inside one of `held`'s
/// transactions, and answers what each of them did.
///
/// The held driver goes first as far as `hold` and stops there; the running driver then takes its
/// entire pass, uncontended; and only then is the held one let go. There is no window to lose and
/// no order to get lucky about — which is the difference between this and the barrier it replaces.
fn interleave(running: &mut Node, paused: &mut Node, stop_at: Hold) -> (Pass, Pass) {
    let gate = Arc::clone(&paused.backend.gate);
    gate.arm(stop_at);
    let paused_driver = &mut paused.redriver;
    let (from_running, from_paused) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| paused_driver.pass());
        gate.wait_until_parked();
        let from_running = running.redriver.pass();
        gate.open();
        (from_running, worker.join().unwrap())
    });
    (
        from_running.expect("a pass itself must not fail"),
        from_paused.expect("a pass itself must not fail"),
    )
}

/// Adds one pass to a running count of what the drivers between them did.
///
/// Every failure must be a **lost race**: `40001` is two nodes overlapping on the same catalog
/// record, which is the serialisation this module leans on instead of a lock. Anything else is a
/// driver reporting an ordinary outcome as a fault, and it is asserted here rather than counted.
fn tally(pass: &Pass, moved: &mut Vec<String>, overtaken: &mut usize, conflicts: &mut usize) {
    for failure in &pass.failed {
        assert_eq!(
            failure.sqlstate,
            esker_sql::sqlstate::SERIALIZATION_FAILURE,
            "a racing step failed for a reason that is not a lost race: {failure:?}"
        );
        *conflicts += 1;
    }
    for step in &pass.steps {
        if step.moved {
            moved.push(step.said.clone());
        } else {
            *overtaken += 1;
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
        assert_eq!(pass, Pass::default());
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
/// # And why the race is constructed rather than run
///
/// This test used to release two `pass()` calls from a `Barrier` and count what came back, and it
/// failed **68 of 80 runs under load** — never on the count, always on its own last assertion,
/// "the two re-drivers never actually collided, so nothing about racing was tested". A barrier
/// aligns where two passes *start*, and one pass can finish before the other is scheduled; the
/// second then sees a changed fingerprint, starts its wait over and takes no step at all. Which
/// is the re-driver working exactly as designed, and a test proving nothing.
///
/// So the collision is built rather than hoped for ([`interleave`]): the held driver stops with
/// its snapshot taken, the other driver's whole pass commits inside that window, and the held one
/// is then released onto it. Both rounds below collide on every run, and the `overtaken +
/// conflicts > 0` guard is kept as the thing that says so.
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

    // `delete-only` -> `write-only`. C reads the job at delete-only and is held between that read
    // and the commit that would write write-only; B's whole pass runs and commits in that window.
    let (from_b, from_c) = interleave(&mut b, &mut c, Hold::Commit(0));
    tally(&from_b, &mut moved, &mut overtaken, &mut conflicts);
    tally(&from_c, &mut moved, &mut overtaken, &mut conflicts);
    assert_eq!(
        moved,
        ["write-only"],
        "one transition, taken once: b={from_b:?} c={from_c:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the held driver's commit was not refused, so nothing overlapped: {from_c:?}"
    );
    assert_eq!(
        b.index_state("t"),
        esker_sql::catalog::SchemaState::WriteOnly,
        "the state moved twice for one transition"
    );

    // Neither steps again at once. Both acted last round, so both forget what the job looked like
    // and start their wait over — being late is the safe direction (`redrive`, "it can only ever
    // be late").
    for pass in [b.redriver.pass().unwrap(), c.redriver.pass().unwrap()] {
        assert_eq!(pass.jobs, 1);
        assert!(
            pass.steps.is_empty() && pass.failed.is_empty(),
            "a driver stepped again in the pass right after it stepped: {pass:?}"
        );
    }

    // `write-only` -> `public`, with the roles swapped so the held driver is the one part-way
    // through the backfill: B takes its first batch's snapshot and is held before committing it,
    // while C runs the backfill out, takes the transition and forgets the job.
    let (from_c, from_b) = interleave(&mut c, &mut b, Hold::Commit(0));
    tally(&from_c, &mut moved, &mut overtaken, &mut conflicts);
    tally(&from_b, &mut moved, &mut overtaken, &mut conflicts);

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
    // And it is now a constructed number rather than a lucky one: both rounds collide, every run.
    assert_eq!(
        (overtaken, conflicts),
        (0, 2),
        "the interleaving was not the one this test builds"
    );

    // The job is gone for both of them, and the index answers, which is the point of building it.
    for pass in [b.redriver.pass().unwrap(), c.redriver.pass().unwrap()] {
        assert_eq!(pass.jobs, 0, "{pass:?}");
    }
    assert_eq!(
        b.rows("SELECT id FROM t WHERE a = 3000"),
        [[Some("300".to_owned())]]
    );
}

/// The other half of the same refusal, and it fails differently: a driver that **reads after** the
/// winner committed has no race to lose, it simply finds the step already taken.
///
/// `40001` refuses two drivers whose transactions overlap. Nothing about that refuses two that
/// merely *follow* each other — the second's writes conflict with nobody — and what stops it is
/// that it re-reads the state inside the transaction that takes the step (`job::advance`'s `from`,
/// `step_job`'s `expected`). Both are needed and either alone leaves the other open, so both are
/// built here rather than left to whichever the scheduler happens to produce.
///
/// The driver is held **before the snapshot** that would take the step, with the job list it read
/// a moment earlier still saying `delete-only`. That is exactly the state ADR 0020's interval is
/// about: stepping here would move the state a moment after the last move rather than an interval
/// after it, and nothing at the time would say so.
#[test]
fn a_re_driver_that_reads_after_the_winner_is_overtaken_rather_than_stepping() {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();
    let mut c = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    drop(a);
    assert!(b.redriver.pass().unwrap().steps.is_empty());
    assert!(c.redriver.pass().unwrap().steps.is_empty());

    // One transaction through — the scan that reads the job list — and then held.
    let (from_b, from_c) = interleave(&mut b, &mut c, Hold::Begin(1));

    assert_eq!(from_b.steps.len(), 1, "{from_b:?}");
    assert_eq!(from_b.steps[0].said, "write-only");
    assert!(from_b.steps[0].moved);
    assert!(
        from_c.failed.is_empty(),
        "a driver that read afterwards lost a race it was never in: {:?}",
        from_c.failed
    );
    assert_eq!(from_c.steps.len(), 1, "{from_c:?}");
    assert_eq!(
        from_c.steps[0].said, "overtaken",
        "a driver that read after the winner did not notice: {from_c:?}"
    );
    assert!(
        !from_c.steps[0].moved,
        "the state moved twice for one transition: {from_c:?}"
    );
    assert_eq!(
        b.index_state("t"),
        esker_sql::catalog::SchemaState::WriteOnly
    );
}

/// A pass's transactions, in the order it opens them, as counts of what to let through.
///
/// Named rather than counted at the call site, so a test says *where* it holds a driver. A pass
/// scans the job list, then re-reads to choose the step, and only then opens the transaction that
/// takes it — three, and the last two are re-opened per backfill batch.
const AFTER_THE_JOB_SCAN: usize = 1;
const AFTER_THE_READ_THAT_CHOOSES_THE_STEP: usize = 2;

/// Drives an orphaned job to `write-only`, then finishes it on one node while the other is held at
/// `hold`, and answers what the held one said.
///
/// The window this builds is the one the barrier version could not reach and did not in 140 runs.
/// A backfill is many transactions and the change ends between two of them: the other driver runs
/// it out, takes `public` and forgets the job — a job record outlives its change by exactly as long
/// as it takes to delete it — and the next thing the held driver reads is a job that is gone.
fn a_job_finished_under_a_held_driver(stop_at: Hold) -> (Pass, Node) {
    let cluster = Cluster::new();
    let mut a = cluster.node();
    let mut b = cluster.node();
    let mut c = cluster.node();

    table_with_rows(&mut a);
    a.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    assert_eq!(a.step("ti"), "delete-only");
    assert_eq!(a.step("ti"), "write-only");
    drop(a);
    assert!(b.redriver.pass().unwrap().steps.is_empty());
    assert!(c.redriver.pass().unwrap().steps.is_empty());

    let (from_c, from_b) = interleave(&mut c, &mut b, stop_at);

    assert_eq!(
        from_c.steps.iter().filter(|step| step.moved).count(),
        1,
        "the driver that finished it did not report the transition: {from_c:?}"
    );
    assert_eq!(
        from_c.steps.last().map(|step| step.said.as_str()),
        Some("public")
    );
    assert!(
        !from_b.steps.is_empty() || !from_b.failed.is_empty(),
        "the held driver was never inside a step, so this window was not the one built: {from_b:?}"
    );
    assert!(
        from_b.steps.iter().all(|step| !step.moved),
        "the held driver moved a state after the change had finished: {from_b:?}"
    );
    (from_b, b)
}

/// A driver whose job is finished under it, **before the read that takes its step**, reports an
/// ordinary lost race rather than an internal error.
///
/// Which is the most ordinary outcome there is. Two nodes re-driving one job is what this module is
/// *for*, and the one that did not finish it has nothing left to do. Answering `XX000` for it puts
/// an internal error in front of an operator for a cluster behaving exactly as designed, and buries
/// the `23505` that is the one failure here that really is one.
#[test]
fn a_driver_whose_job_is_finished_before_its_step_says_it_lost_a_race() {
    let (from_b, mut b) = a_job_finished_under_a_held_driver(Hold::Begin(AFTER_THE_JOB_SCAN));
    for failure in &from_b.failed {
        assert_eq!(
            failure.sqlstate,
            esker_sql::sqlstate::SERIALIZATION_FAILURE,
            "a job finished by another node is reported as a fault: {failure:?}"
        );
    }
    assert_eq!(b.index_state("t"), esker_sql::catalog::SchemaState::Public);
    assert_eq!(b.index_entries("t"), usize::try_from(ROWS).unwrap());
}

/// The same, one transaction later: held **inside** the step, at the batch it was about to run.
///
/// Worth its own test because this is the arm that unwinds. `adding_step` treats anything but
/// `40001` out of a backfill batch as a change that failed on data, and unwinds the whole thing
/// back to `absent` — so a job finishing under a driver arriving here as an internal error is one
/// transaction away from tearing down an index that is already `public`. What saves it today is
/// that `unwind` finds no job and stops, which is a guard rather than a reason.
#[test]
fn a_driver_whose_job_is_finished_inside_its_step_does_not_unwind_the_change() {
    let (from_b, mut b) =
        a_job_finished_under_a_held_driver(Hold::Begin(AFTER_THE_READ_THAT_CHOOSES_THE_STEP));
    for failure in &from_b.failed {
        assert_eq!(
            failure.sqlstate,
            esker_sql::sqlstate::SERIALIZATION_FAILURE,
            "a job finished by another node is reported as a fault: {failure:?}"
        );
    }
    assert_eq!(
        b.index_state("t"),
        esker_sql::catalog::SchemaState::Public,
        "the change was unwound after it had already finished"
    );
    assert_eq!(b.index_entries("t"), usize::try_from(ROWS).unwrap());
    assert!(b.jobs().is_empty(), "a finished job is forgotten");
}
