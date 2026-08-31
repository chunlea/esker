//! The re-driver: a job whose driver died is finished by whichever node notices.
//!
//! [ADR 0020](../../../docs/adr/0020-online-schema-change.md) as amended, and
//! `docs/plans/debt-c2.md`. A `CREATE INDEX CONCURRENTLY` is a job in the catalog that some node
//! steps; `esker_schema_step` is the verb. Until now, if the node that started one died, nothing
//! picked it up: the job sat at whatever state it had reached until a human called the verb. The
//! ADR called that liveness rather than safety and it was right — the record and its cursor are
//! durable, the index is not readable until `public`, and every node maintains it at whatever
//! state it is stuck in — but "a human notices" is not a liveness mechanism.
//!
//! So every node runs one of these. It is a background pass, not a leader election and not a
//! lock: a job that has not moved for a step interval is stepped by whoever sees it first.
//!
//! # Why there is no lock
//!
//! Because there is already one. A step is a **catalog transaction**, and two nodes stepping the
//! same job conflict on the catalog's version key: first-committer-wins, the loser gets
//! `40001`, and its next pass reads the state the winner wrote. Adding a lock would add a second
//! mechanism that has to agree with the first, and a lock has a holder that can die — which is
//! the failure this module exists to survive.
//!
//! # How "idle" is decided, and why it is not a clock
//!
//! A job is idle when **its fingerprint has not changed for a whole pass**. The pass period *is*
//! the step interval, so a fingerprint that survives a pass has been still for at least one
//! interval, and the step that produced it happened at or before the previous pass — so the wait
//! the interactive driver takes has already been taken.
//!
//! Counted in passes rather than measured against a clock, and that is deliberate twice over.
//! `CLAUDE.md` invariant 6 keeps wall clocks out of ordering. And the job record carries no
//! "stepped at" timestamp: adding one would be a catalog format change with a golden, to store a
//! number that is only ever compared against a local duration. Reaching the past by *token* —
//! the previous pass's observation — needs neither.
//!
//! The rule is one-sided in the safe direction. It can only ever be late: a job stepped a
//! moment before a pass observes it will not be touched until the pass after next, which is two
//! intervals rather than one. Being late is a slower schema change. Being early breaks the
//! two-version invariant the interval exists for, and this cannot be early.
//!
//! # What it will not do
//!
//! * **Step without an interval.** A node whose backend cannot say what the wait is does not
//!   re-drive at all ([`crate::backend::Backend::schema_step_interval`]). Guessing short is the
//!   unsafety the number exists to prevent.
//! * **Step past its lease.** A node that has lost PD stops writing (ADR 0028), and that
//!   includes stepping: it is checked here as well as in the executor, so a lapsed node does no
//!   work rather than doing work that is refused one layer down.
//! * **Interrupt a live driver.** A job that is being stepped changes its fingerprint every
//!   interval, so it is never idle and never adopted.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::backend::{Backend, StepInterval};
use crate::catalog::{self, Catalog, JobRecord, SchemaState};
use crate::error::Result;
use crate::exec::{Executor, for_each_page, verbs};

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    /// Jobs in flight, whether or not they were touched.
    pub jobs: usize,
    /// The steps taken, in the order they were taken.
    pub steps: Vec<Step>,
    /// Jobs that were idle and could not be stepped, with what stopped them.
    ///
    /// A pass does not fail because one job did. A `UNIQUE` backfill that meets a duplicate
    /// fails its own change and unwinds it, and a step that lost a race to another node's
    /// re-driver is an ordinary conflict — neither is a reason to stop looking at the others.
    pub failed: Vec<Failure>,
}

/// A step that did not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The index whose job it was.
    pub index_id: u64,
    /// The `SQLSTATE` it failed with.
    ///
    /// `40001` is the ordinary one and needs nothing done about it: two nodes overlapped on the
    /// same catalog record and one was rolled back, which is the serialisation this module leans
    /// on instead of a lock. Anything else is worth an operator's attention — `23505` is a
    /// `UNIQUE` backfill that met a duplicate, which fails and unwinds the whole change.
    pub sqlstate: &'static str,
    /// What it said.
    pub message: String,
}

/// One step this pass took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The index whose job it was.
    pub index_id: u64,
    /// What `esker_schema_step` calls it: `delete-only`, `write-only`, `public`, `absent`,
    /// `dropped` — or `overtaken`, when another node took the step first.
    pub said: String,
    /// Whether **this** pass moved the state, as opposed to finding it already moved.
    ///
    /// The two are worth telling apart in a report: a cluster where every pass is `overtaken` is
    /// one where every node is re-driving the same job, which is harmless and wasteful, and a
    /// number an operator can see is how it stops being invisible.
    pub moved: bool,
    /// How many backfill batches ran before it, all in this same pass.
    ///
    /// Batches move no state, so nothing waits between them — the interval is between state
    /// *transitions*. A pass that adopts a job mid-backfill runs the backfill out and then takes
    /// the transition, which is what makes a re-drive take intervals rather than intervals-per-row.
    pub batches: usize,
}

/// What a job looked like last time, and when that was.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// Everything a step changes. Two passes that agree on this saw no step between them.
    fingerprint: (SchemaState, Vec<u8>, bool),
    /// The pass at which this fingerprint was first seen.
    since: u64,
}

/// One node's re-driver.
///
/// Cheap to hold and cheap to run: it keeps one small record per job in flight and nothing else,
/// and a pass over a cluster with no schema change in flight is one range scan that finds
/// nothing.
#[derive(Debug)]
pub struct ReDriver {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    tenant: u64,
    /// Per index in flight, what it looked like and when.
    seen: BTreeMap<u64, Seen>,
    /// Which pass this is. One pass is one step interval.
    pass: u64,
}

impl ReDriver {
    /// A re-driver over the same backend and catalog cache the sessions use.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, catalog: Arc<Catalog>, tenant: u64) -> Self {
        ReDriver {
            backend,
            catalog,
            tenant,
            seen: BTreeMap::new(),
            pass: 0,
        }
    }

    /// The interval between passes, or `None` when this node must not re-drive.
    ///
    /// Read every pass rather than cached, because it is PD's number and PD may change it.
    #[must_use]
    pub fn interval(&self) -> Option<StepInterval> {
        self.backend.schema_step_interval()
    }

    /// One pass: look at every job, step the ones that have gone idle.
    ///
    /// Call it once per [`ReDriver::interval`]. Calling it faster does not make a job move
    /// faster — idleness is counted in passes, so a pass that comes early only makes "one pass"
    /// mean less time, which is the one thing that would make this unsafe. [`ReDriver::run`] is
    /// the loop that gets the period right; a test drives passes by hand.
    pub fn pass(&mut self) -> Result<Pass> {
        let mut report = Pass::default();
        let Some(interval) = self.interval() else {
            // Nothing publishes a wait here, so there is no safe step to take. Not an error: a
            // node with no placement driver is a node with no staged schema change to finish.
            return Ok(report);
        };
        // Fail closed, and before doing any work rather than after. A node past its lease may be
        // acting on a schema the cluster has moved two states beyond, which is the one thing the
        // four states do not make safe (ADR 0028).
        if self.backend.schema_lease_remaining().is_none() {
            return Ok(report);
        }

        self.pass += 1;
        let jobs = self.jobs()?;
        report.jobs = jobs.len();
        let live: std::collections::BTreeSet<u64> =
            jobs.iter().map(|(job, _)| job.index_id).collect();
        self.seen.retain(|index_id, _| live.contains(index_id));

        for (job, state) in jobs {
            let fingerprint = (state, job.cursor.clone(), job.done);
            let entry = self.seen.entry(job.index_id).or_insert_with(|| Seen {
                fingerprint: fingerprint.clone(),
                since: self.pass,
            });
            if entry.fingerprint != fingerprint {
                // Somebody stepped it since the last pass — the node that started it is alive, or
                // another re-driver got there first. Either way the wait starts again from here.
                *entry = Seen {
                    fingerprint,
                    since: self.pass,
                };
                continue;
            }
            if self.pass - entry.since < passes_to_wait(interval, &job, state) {
                continue;
            }
            match self.drive(job.index_id, state) {
                Ok(step) => report.steps.push(step),
                Err(error) => report.failed.push(Failure {
                    index_id: job.index_id,
                    sqlstate: error.sqlstate(),
                    message: error.to_string(),
                }),
            }
            // Whatever happened, this job has been acted on: forget what it looked like so the
            // next pass starts its wait over from what it finds. A step that failed is a step
            // that may have half-happened, and re-reading is how this stays a fact rather than
            // an assumption.
            self.seen.remove(&job.index_id);
        }
        Ok(report)
    }

    /// Every job in flight, with the state of the index it belongs to.
    fn jobs(&self) -> Result<Vec<(JobRecord, SchemaState)>> {
        let executor = self.executor();
        let mut txn = executor.plain_read()?;
        let (start, end) = catalog::job_range(self.tenant);
        let mut records = Vec::new();
        let tenant = self.tenant;
        for_each_page(&mut *txn, &start, &end, |_, page| {
            for (key, value) in page {
                records.push(catalog::decode_job(tenant, key, value)?);
            }
            Ok(())
        })?;

        let mut jobs = Vec::new();
        for job in records {
            let table = executor.table_by_id(&*txn, job.table_id)?;
            // An index that is gone has nothing left to step. `job::remove` drops the record and
            // the definition in one transaction, so this is a torn read or a job for an index
            // somebody dropped outright, and neither is this pass's to repair.
            if let Some(index) = table.indexes.iter().find(|index| index.id == job.index_id) {
                let state = index.state;
                jobs.push((job, state));
            }
        }
        Ok(jobs)
    }

    /// Takes one job as far as its next state transition, batches included.
    ///
    /// Stops on anything that starts the wait — a transition, or finding that another node made
    /// one first. Carrying on past `overtaken` would be the early step this whole module is
    /// careful not to take: the state moved a moment ago, just not here.
    fn drive(&self, index_id: u64, expected: SchemaState) -> Result<Step> {
        let mut executor = self.executor();
        let mut batches = 0_usize;
        loop {
            let mut txn = executor.plain_read()?;
            let stepped = verbs::step_job(&mut executor, &mut *txn, index_id, Some(expected))?;
            if stepped.starts_the_wait() {
                return Ok(Step {
                    index_id,
                    said: stepped.said().to_owned(),
                    moved: stepped.moved_a_state(),
                    batches,
                });
            }
            batches += 1;
            // A backfill can be long, and a pass that ran one to the end would hold this thread
            // for as long as the table takes. The ceiling is not a correctness bound — every
            // batch is its own transaction and the cursor is durable, so stopping between two of
            // them loses nothing — it is what keeps one job from starving the others. The next
            // pass resumes from the cursor.
            if batches >= MAX_BATCHES_PER_PASS {
                return Ok(Step {
                    index_id,
                    said: stepped.said().to_owned(),
                    moved: false,
                    batches,
                });
            }
        }
    }

    fn executor(&self) -> Executor {
        Executor::new(
            Arc::clone(&self.backend),
            Arc::clone(&self.catalog),
            self.tenant,
        )
    }

    /// Runs passes on a thread of its own, one per step interval, until the process ends.
    ///
    /// **This is the only place a wall clock is read, and it is a period rather than an
    /// ordering** — the sleep between passes, which is what makes "one pass" mean "one interval"
    /// and therefore what makes [`ReDriver::pass`]'s counting sound. Nothing here decides the
    /// order of anything.
    ///
    /// A node with no interval to be told sleeps a few seconds and asks again, because a
    /// placement driver that was unreachable at startup may not be later.
    pub fn run(mut self) {
        loop {
            let period = match self.interval() {
                Some(interval) => std::time::Duration::from_millis(interval.step_ms),
                None => IDLE_POLL,
            };
            std::thread::sleep(period);
            match self.pass() {
                Ok(report) => {
                    for step in &report.steps {
                        tracing::info!(
                            index_id = step.index_id,
                            said = step.said,
                            moved = step.moved,
                            batches = step.batches,
                            "re-drove a schema-change job whose driver had gone quiet"
                        );
                    }
                    for failure in &report.failed {
                        tracing::warn!(
                            index_id = failure.index_id,
                            sqlstate = failure.sqlstate,
                            message = failure.message,
                            "a re-driven step did not take"
                        );
                    }
                }
                Err(error) => tracing::warn!(%error, "a re-driver pass failed"),
            }
        }
    }
}

/// Backfill batches one pass will run before it hands the thread back.
///
/// Not a correctness bound: every batch is its own transaction and the cursor is durable, so a
/// pass that stops between two of them loses nothing and the next resumes from where it got to.
/// It is a fairness bound, so that one large table's backfill cannot starve every other job.
const MAX_BATCHES_PER_PASS: usize = 64;

/// How long a node with no interval waits before asking for one again.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// How many passes a job must sit unchanged before this one may step it.
///
/// One for an ordinary step, because a pass *is* an interval. More only for the last step of a
/// **removing** change, which waits the MVCC retention window on top: a reader that began while
/// the index was `public` still reads its entries, and retention is what keeps them readable, so
/// removing them earlier would break a live reader rather than a stale writer.
fn passes_to_wait(interval: StepInterval, job: &JobRecord, state: SchemaState) -> u64 {
    let step_ms = interval.step_ms.max(1);
    let extra = if job.removing && state == SchemaState::Absent {
        interval.removal_extra_ms
    } else {
        0
    };
    step_ms.saturating_add(extra).div_ceil(step_ms)
}
