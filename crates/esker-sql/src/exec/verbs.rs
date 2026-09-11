//! The time machine's verbs, run.
//!
//! Named for what [ADR 0021](../../../../docs/adr/0021-time-machine.md) Decision 3 calls them, and
//! not `time_machine`, so that the module this one leans on — `crate::time_machine`, which holds
//! the value grammar, the token and the window — can be named here without qualification.
//!
//! Three of them, and what they have in common is how little they do. A checkpoint is a **number
//! with a name**: taking one writes nine bytes and touches nothing else — no snapshot, no copy, no
//! flush — because the versions it refers to are kept by retention whether anybody named it or not.
//!
//! That is also the catch, and it is why the record is a **claim to check** rather than a
//! guarantee: a checkpoint older than the travel window names history that is gone. Nothing here
//! checks it, deliberately — the check belongs where the read happens, so that a name can outlive
//! the data it points at and say so at the moment somebody tries to use it.
//!
//! # Why a checkpoint is written in a transaction of its own
//!
//! Because the most useful checkpoint is one taken *while reading the past*: "I am looking at an
//! hour ago; remember this moment as `before-the-incident`." That transaction is read-only by
//! construction ([`crate::backend::Backend::begin_at`]), so a record written inside it could never
//! commit — and refusing the statement would mean the one moment a user most wants to name is the
//! one they cannot have.
//!
//! So the *value* is the reading transaction's `start_ts` and the *write* is an ordinary
//! present-time transaction of its own — the same shape, and for the same kind of structural
//! reason, as [`Executor::next_row_id`]. The cost is stated rather than hidden: a block that rolls
//! back leaves the name behind. A name is a bookmark and not data, and a bookmark surviving a
//! cancelled read is not a correctness problem; where it would be one,
//! `esker_drop_checkpoint('<name>')` removes it.

use crate::backend::Txn;
use crate::catalog;
use crate::error::Result;
use crate::error::SqlError;
use crate::exec::{Executor, flashback, for_each_page, job};
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::Outcome;
use crate::plan::TimeMachineVerb;
use crate::time_machine::{check_name, render, token};
use crate::value::PgDatum;
use crate::value::{ColumnType, Datum};

/// Runs one verb.
pub(super) fn run(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    verb: &TimeMachineVerb,
) -> Result<Outcome> {
    match verb {
        TimeMachineVerb::ExportSnapshot { name } => export(executor, txn, name.as_deref()),
        TimeMachineVerb::DropCheckpoint { name } => drop_one(executor, name),
        TimeMachineVerb::ListCheckpoints => list(executor, txn),
        TimeMachineVerb::Diff { table, from, to } => diff(executor, table, from, to.as_deref()),
        TimeMachineVerb::Flashback { table, to } => run_flashback(executor, txn, table, to),
        TimeMachineVerb::ListSchemaJobs => list_jobs(executor, txn),
        TimeMachineVerb::ListColumnarReplicas => list_columnar_replicas(executor, txn),
        TimeMachineVerb::SchemaStep { index } => schema_step(executor, txn, index),
    }
}

/// `pg_export_snapshot()`, and `esker_checkpoint('<name>')`.
///
/// The timestamp is **this transaction's own snapshot**, not a fresh one, and that is the whole
/// point of PostgreSQL's verb: what it exports is what the exporting transaction sees, so a second
/// session importing the token reads exactly the state the first one was reading. Allocating a new
/// timestamp here would export a moment nobody had looked at.
///
/// The anonymous form writes nothing at all — the token carries the timestamp — so it is the one
/// verb here that is free even of a record.
fn export(executor: &mut Executor, txn: &dyn Txn, name: Option<&str>) -> Result<Outcome> {
    let at = txn.start_ts();
    if let Some(name) = name {
        // **Not folded.** The name arrives as a string literal, and PostgreSQL does not case-fold
        // those; `esker_checkpoint('Nightly')` keeps its capital. Validated with the rule
        // `SET TRANSACTION SNAPSHOT` reads names by, so that whatever can be named can be
        // imported — a name too long to be a snapshot identifier would otherwise be writable and
        // then unusable.
        check_name(name)?;
        let tenant = executor.tenant;
        executor.in_its_own_transaction(|own| {
            catalog::set_checkpoint(own, tenant, name, at);
            Ok(())
        })?;
    }
    Ok(one_text("pg_export_snapshot", token(at)))
}

/// `esker_drop_checkpoint('<name>')` — forgets the name.
///
/// The versions it named are retention's business and are not touched, which is the difference
/// between forgetting a bookmark and burning the book. Dropping a name that is not there answers
/// `f` rather than raising: a client cleaning up after itself should not have to look first.
///
/// In its own transaction for the same reason [`export`] is: a session reading the past must be
/// able to tidy up a name it no longer wants.
fn drop_one(executor: &mut Executor, name: &str) -> Result<Outcome> {
    check_name(name)?;
    let tenant = executor.tenant;
    let mut existed = false;
    executor.in_its_own_transaction(|own| {
        existed = catalog::checkpoint_at(own, tenant, name)?.is_some();
        if existed {
            catalog::drop_checkpoint(own, tenant, name);
        }
        Ok(())
    })?;
    Ok(one_text(
        "esker_drop_checkpoint",
        if existed { "t" } else { "f" }.to_owned(),
    ))
}

/// `SELECT * FROM esker_checkpoints()` — the names, their tokens and their instants.
///
/// Three columns rather than one, because a name on its own does not answer the question a user
/// listing checkpoints actually has: *is this one still inside the window?* The instant is what
/// they compare against, and the token is what they paste into `SET TRANSACTION SNAPSHOT`.
///
/// Read in the statement's own transaction, unlike the two above: this writes nothing, so a
/// session reading the past sees the checkpoints **as they were at that snapshot**, which is the
/// consistent answer rather than a special case.
fn list(executor: &mut Executor, txn: &mut dyn Txn) -> Result<Outcome> {
    let tenant = executor.tenant;
    let (start, end) = catalog::checkpoint_range(tenant);
    let mut rows = Vec::new();
    for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            let (name, at) = catalog::decode_checkpoint(tenant, key, value)?;
            rows.push(vec![
                Some(name.into_bytes()),
                Some(token(at).into_bytes()),
                Some(render(at).into_bytes()),
            ]);
        }
        Ok(())
    })?;
    let tag = format!("SELECT {}", rows.len());
    Ok(Outcome::Rows {
        fields: ["name", "snapshot", "at"]
            .into_iter()
            .map(|name| FieldDescription::computed(name, ColumnType::Text))
            .collect(),
        rows,
        tag,
    })
}

/// `SELECT esker_flashback('<table>', '<snapshot>')` — put a table back, and say how many rows
/// moved.
///
/// **Compensating writes** (ADR 0021 Decision 3): the difference between now and the target,
/// written forwards at a fresh `commit_ts`. Nothing that already exists is touched, so every state
/// the table was in is still readable `AS OF` an instant before this — an undo is undoable, and
/// the audit trail survives the correction.
///
/// Runs its batches here rather than returning a handle, because a flashback is a data operation
/// the user is waiting on rather than a schema change the cluster has to agree about. The **cursor
/// is still durable**: a node that dies mid-flashback leaves one, and calling the verb again
/// resumes from it instead of starting over — which on a table big enough to need batching is the
/// difference between finishing and not.
fn run_flashback(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &str,
    to: &str,
) -> Result<Outcome> {
    let tenant = executor.tenant;
    let target = executor.snapshot_named(to)?;
    let def = executor.require_table(txn, table)?;

    // An existing record wins, and its **target** wins with it: resuming somebody else's flashback
    // with a different target would leave the table in a state it was never in.
    let existing = catalog::flashback(txn, tenant, def.id)?;
    if let Some(record) = &existing
        && record.target_ts != target
    {
        return Err(SqlError::FeatureNotSupported(format!(
            "a flashback of \"{table}\" to another snapshot is already in progress; \
             finish it by calling esker_flashback with that snapshot"
        )));
    }
    if existing.is_none() {
        let mut own = executor.plain_read()?;
        catalog::put_flashback(
            &mut *own,
            tenant,
            &catalog::FlashbackRecord {
                table_id: def.id,
                target_ts: target,
                cursor: Vec::new(),
                changed: 0,
            },
        );
        own.commit()?;
    }

    // Batch until it finishes. Each is its own transaction, so a crash anywhere leaves a cursor.
    loop {
        if let Some(changed) = flashback::batch(executor, def.id)? {
            return Ok(one_text("esker_flashback", changed.to_string()));
        }
    }
}

/// `SELECT * FROM esker_columnar_replicas()` — which tables want a columnar copy, and how many.
///
/// Two columns, because the question has two halves and an operator asking it is usually asking
/// the second: the table, and the number the catalog holds. What the *cluster* has actually
/// placed is PD's to report and deliberately not here — a catalog readout that guessed at
/// placement would be a second source of truth about where a replica is.
fn list_columnar_replicas(executor: &Executor, txn: &mut dyn Txn) -> Result<Outcome> {
    let tenant = executor.tenant;
    let (start, end) = catalog::columnar_range(tenant);
    let mut settings = Vec::new();
    for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            settings.push(catalog::decode_columnar(tenant, key, value)?);
        }
        Ok(())
    })?;
    let mut rows = Vec::with_capacity(settings.len());
    for (table_id, replicas) in settings {
        let table = executor.table_by_id(txn, table_id)?;
        rows.push(vec![
            Some(table.name.clone().into_bytes()),
            Some(replicas.to_string().into_bytes()),
        ]);
    }
    let tag = format!("SELECT {}", rows.len());
    Ok(Outcome::Rows {
        fields: ["table", "columnar_replicas"]
            .into_iter()
            .map(|name| FieldDescription::computed(name, ColumnType::Text))
            .collect(),
        rows,
        tag,
    })
}

/// `SELECT * FROM esker_schema_jobs()` — every schema change in flight, and where each one is.
///
/// The `psql`-visible progress ADR 0020 asks for. Four columns, because a human watching a
/// `CREATE INDEX CONCURRENTLY` needs to tell *slow* from *stuck*: the state says how far the
/// change has got, and the cursor says whether the backfill is still moving.
fn list_jobs(executor: &Executor, txn: &mut dyn Txn) -> Result<Outcome> {
    let tenant = executor.tenant;
    let (start, end) = catalog::job_range(tenant);
    let mut rows = Vec::new();
    let mut jobs = Vec::new();
    for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            jobs.push(catalog::decode_job(tenant, key, value)?);
        }
        Ok(())
    })?;
    for job in jobs {
        let table = executor.table_by_id(txn, job.table_id)?;
        let index = table.indexes.iter().find(|index| index.id == job.index_id);
        rows.push(vec![
            Some(table.name.clone().into_bytes()),
            Some(
                if job.removing { "removing" } else { "adding" }
                    .as_bytes()
                    .to_vec(),
            ),
            Some(
                index
                    .map_or_else(|| job.index_id.to_string(), |index| index.name.clone())
                    .into_bytes(),
            ),
            Some(
                index
                    .map_or("gone", |index| index.state.name())
                    .as_bytes()
                    .to_vec(),
            ),
            Some(
                if job.done {
                    "backfilled".to_owned()
                } else if job.cursor.is_empty() {
                    "not started".to_owned()
                } else {
                    format!("{} rows in", job.cursor.len())
                }
                .into_bytes(),
            ),
        ]);
    }
    let tag = format!("SELECT {}", rows.len());
    Ok(Outcome::Rows {
        fields: ["table", "direction", "index", "state", "backfill"]
            .into_iter()
            .map(|name| FieldDescription::computed(name, ColumnType::Text))
            .collect(),
        rows,
        tag,
    })
}

/// `SELECT esker_schema_step('<index>')` — one step of one job.
///
/// **The step, separated from the wait.** PD publishes how long a step must wait
/// ([ADR 0028](../../../../docs/adr/0028-the-schema-lease.md)); this takes it. Separating them is what
/// makes the state machine testable without a timer, and what lets an operator move a stuck job.
///
/// The sequence is the ADR's: `absent → delete-only → write-only`, then the backfill a batch at a
/// time, then `public`. A failed backfill unwinds the states and forgets the job, so a change that
/// fails on data leaves no half-built index behind.
///
/// # The wait is between **state transitions**, not between batches
///
/// The answer says which happened, and a driver must read it: `delete-only`, `write-only` and
/// `public` are transitions and the next step must wait the interval PD published;
/// `backfilling` changed no state at all and the next batch may start immediately.
///
/// Waiting between batches instead would be a real cost rather than a pedantic one. Measured on a
/// 20,000-row table: 82 steps, of which 79 are batches. At an eight-second interval, waiting after
/// every step is **656 seconds**; waiting only after the three transitions is **24 seconds and a
/// seventh** — the same safety, twenty-seven times faster. Every batch runs at write-only, so no
/// state moves and no node can fall a step behind while they run, which is why the wait buys
/// nothing there.
fn schema_step(executor: &mut Executor, txn: &mut dyn Txn, index: &str) -> Result<Outcome> {
    let relation = executor.catalog_view(txn)?.relation(index)?;
    let Some(catalog::Relation::Index { index_id, .. }) = relation else {
        return Err(SqlError::UndefinedIndex(index.to_owned()));
    };
    if catalog::job(txn, executor.tenant, index_id)?.is_none() {
        return Err(SqlError::Internal(format!(
            "index \"{index}\" has no schema-change job in flight"
        )));
    }
    let stepped = step_job(executor, txn, index_id, None)?;
    Ok(one_text("esker_schema_step", stepped.said().to_owned()))
}

/// What one step did, and therefore whether the next one has to wait.
///
/// **The wait belongs between state transitions, not between backfill batches**
/// (`docs/plans/phase-6e.md` §10). Every batch runs at write-only, so no state moves and no node
/// can fall a step behind while they run; waiting the interval after every step prices a
/// 20,000-row backfill at 656 seconds instead of 24.
///
/// `esker_schema_step` has always said which it took, in words. That was enough while every
/// driver was a human or a script, and it is not enough now: the re-driver
/// ([`crate::exec::redrive`]) is the first driver in this tree, it has to get the distinction
/// right on every step, and a driver that got it wrong by mistyping a word would step early —
/// which is the one way to break the two-version invariant the interval exists for. So it is a
/// type, and the words are what the type prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stepped {
    /// A state moved. The next step must wait the interval PD publishes.
    Transition(String),
    /// A backfill batch. No state moved, so the next step may start at once.
    Batch(String),
    /// Somebody else took this step first, between this driver reading the state and writing it.
    ///
    /// Not an error and not a step: the change is exactly one further on than it was, just not by
    /// this caller. A driver's next move is to look again — and to wait first, because the step
    /// that did happen happened just now (`crate::exec::job::advance`).
    ///
    /// **A job that has been forgotten is this too**, and it is the same sentence one step
    /// further: the change is over, and not by this caller. Every read of a job record on the
    /// step path answers it that way rather than erroring, because a driver that arrives after
    /// another has finished the change is what a cluster of re-drivers *is*.
    Overtaken,
    /// The change was already finished; this only cleared what it left behind.
    ///
    /// A job record outlives its change by exactly as long as it takes to delete it, and that
    /// delete is its own transaction which can lose a race. Whoever finds the leftover forgets
    /// it — and says that no state moved, because none did. Counting this as a transition would
    /// make a cluster look like it had stepped an index to `public` twice.
    Cleared(String),
}

impl Stepped {
    /// What `esker_schema_step` answers with.
    pub(crate) fn said(&self) -> &str {
        match self {
            Self::Transition(said) | Self::Batch(said) | Self::Cleared(said) => said,
            Self::Overtaken => "overtaken",
        }
    }

    /// Whether **this** call moved a state, and so whether it did any work at all.
    pub(crate) fn moved_a_state(&self) -> bool {
        matches!(self, Self::Transition(_))
    }

    /// Whether a driver must stop here rather than taking another step at once.
    ///
    /// Everything except a backfill batch. A transition and an overtaking both mean a state moved
    /// just now — the second is the one that is easy to get wrong, because it moved somewhere
    /// else — so the interval starts either way. A cleared job has nothing left to step.
    pub(crate) fn starts_the_wait(&self) -> bool {
        !matches!(self, Self::Batch(_))
    }
}

/// One step of the job on `index_id`, by id rather than by name.
///
/// The shared middle of `esker_schema_step` and the re-driver, so that the two cannot come to
/// differ about what a step is. A driver that reimplemented the state machine would be a second
/// definition of the invariant, and the whole point of ADR 0020's states is that there is one.
///
/// `expected` is what the caller believes the state to be, and it is how a driver that decided
/// to step *earlier* than now says so. The re-driver decides a job is idle by looking at it and
/// then looking again an interval later; between that decision and this call another node may
/// have stepped it, and taking the next step on top would be taking it moments after the last
/// one rather than an interval after — the two-version invariant broken with nothing to show
/// for it. `None` is a caller who is deciding right now, which is what `esker_schema_step` is:
/// a human or a script asking for one step of whatever it finds, and the wait is theirs.
pub(crate) fn step_job(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    index_id: u64,
    expected: Option<catalog::SchemaState>,
) -> Result<Stepped> {
    let tenant = executor.tenant;
    let Some(job) = catalog::job(txn, tenant, index_id)? else {
        // **A job that is gone was finished by somebody else**, and that is the ordinary outcome
        // of two nodes re-driving one change, not a fault. A job record outlives its change by
        // exactly as long as it takes to delete it, so a second driver arriving inside that
        // window reads nothing — and answering `XX000` for it would put an internal error in
        // front of an operator for a cluster behaving exactly as designed, and bury the `23505`
        // that is the one failure here that really is one
        // (`redrive.rs::a_driver_whose_job_is_finished_before_its_step_says_it_lost_a_race`).
        //
        // `esker_schema_step` still answers a human who names an index with no job at all: it
        // checks for one before it gets here, in the same transaction, so this arm is only ever
        // the race.
        return Ok(Stepped::Overtaken);
    };
    let table = executor.table_by_id(txn, job.table_id)?;
    let state = table
        .indexes
        .iter()
        .find(|def| def.id == index_id)
        .map(|def| def.state)
        .ok_or_else(|| SqlError::UndefinedIndex(index_id.to_string()))?;
    if expected.is_some_and(|expected| expected != state) {
        return Ok(Stepped::Overtaken);
    }

    if job.removing {
        removing_step(executor, index_id, state, &table)
    } else {
        adding_step(executor, index_id, state, tenant)
    }
}

/// A step of a change that is **adding** an index: forwards through the states, with the backfill
/// between write-only and public.
fn adding_step(
    executor: &mut Executor,
    index_id: u64,
    state: catalog::SchemaState,
    tenant: u64,
) -> Result<Stepped> {
    Ok(match state {
        catalog::SchemaState::Absent => {
            if job::advance(executor, index_id, state, catalog::SchemaState::DeleteOnly)? {
                Stepped::Transition("delete-only".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
        catalog::SchemaState::DeleteOnly => {
            if job::advance(executor, index_id, state, catalog::SchemaState::WriteOnly)? {
                Stepped::Transition("write-only".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
        catalog::SchemaState::WriteOnly => match job::backfill_batch(executor, index_id) {
            Ok(true) => {
                if job::advance(executor, index_id, state, catalog::SchemaState::Public)? {
                    // The index is readable from here; forgetting the job is bookkeeping in a
                    // transaction of its own, and it can lose a race with another driver's last
                    // batch. Reporting *that* as the failure of this step would tell a driver
                    // nothing moved at the moment the index became public. What is left behind
                    // is a record with no change attached, and the arm below clears it.
                    let _ = forget(executor, tenant, index_id);
                    Stepped::Transition("public".to_owned())
                } else {
                    Stepped::Overtaken
                }
            }
            Ok(false) => Stepped::Batch("backfilling".to_owned()),
            // **A lost race is not a failed change.** A backfill runs beside live traffic on
            // purpose — that is what running it at write-only buys — so a batch and a writer
            // touching the same row at the same moment is the ordinary case, and
            // `crate::exec::job` says as much: "a lost race is an ordinary conflict-and-retry".
            // The cursor is still where it was and the batch is idempotent, so the answer is to
            // run it again. Unwinding here would throw a whole schema change away because two
            // transactions overlapped, and on a table busy enough to need `CONCURRENTLY` that is
            // a change that can never finish.
            Err(error) if error.sqlstate() == crate::sqlstate::SERIALIZATION_FAILURE => {
                return Err(error);
            }
            Err(error) => {
                // A duplicate is the one way a schema change fails on *data*. The states unwind so
                // that a failed change leaves nothing half-built, and the error the user gets is
                // the one they caused.
                job::unwind(executor, index_id)?;
                return Err(error);
            }
        },
        catalog::SchemaState::Public => {
            forget(executor, tenant, index_id)?;
            Stepped::Cleared("public".to_owned())
        }
    })
}

/// A step of a change that is **removing** an index: backwards through the states, and only then
/// are the entries and the definition taken away.
///
/// # Why the last step is the expensive one
///
/// The three state moves are bounded by the same thing an adding change is: a *writer* can be one
/// step behind, and a writer's lifetime is the lock TTL. The **final removal** is different,
/// because what it has to outlast is a *reader*.
///
/// A transaction that began while the index was `public` reads through it, and its catalog and its
/// entries are both at its own snapshot — so it keeps working after the entries are deleted,
/// because MVCC keeps the versions it can see. What bounds *that* is retention: a read below the
/// GC safepoint is refused (`docs/txn-spec.md` §7). So waiting the retention window before removing
/// means no live reader can still be at `public` when the entries go, and correctness stops
/// resting on retained versions that a shorter retention would take away.
///
/// That is why `PdResp::SchemaLease::removal_extra_ms` exists and why it is **separate** from the
/// ordinary interval: an adding change never needs it, and folding it in would price every
/// `CREATE INDEX` at the retention window (ADR 0020, as amended).
fn removing_step(
    executor: &mut Executor,
    index_id: u64,
    state: catalog::SchemaState,
    table: &catalog::TableDef,
) -> Result<Stepped> {
    Ok(match state {
        catalog::SchemaState::Public => {
            if job::advance(executor, index_id, state, catalog::SchemaState::WriteOnly)? {
                Stepped::Transition("write-only".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
        catalog::SchemaState::WriteOnly => {
            if job::advance(executor, index_id, state, catalog::SchemaState::DeleteOnly)? {
                Stepped::Transition("delete-only".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
        catalog::SchemaState::DeleteOnly => {
            if job::advance(executor, index_id, state, catalog::SchemaState::Absent)? {
                Stepped::Transition("absent".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
        // At `absent` nothing reads it and nothing writes it, so the entries and the definition can
        // go. This is the step a driver waits `removal_extra_ms` *before*.
        catalog::SchemaState::Absent => {
            if job::remove(executor, index_id, table)? {
                Stepped::Transition("dropped".to_owned())
            } else {
                Stepped::Overtaken
            }
        }
    })
}

/// Forgets a finished job, in a transaction of its own.
fn forget(executor: &Executor, tenant: u64, index_id: u64) -> Result<()> {
    let mut own = executor.plain_read()?;
    catalog::drop_job(&mut *own, tenant, index_id);
    own.commit()?;
    Ok(())
}

/// One row of one text column, which is the shape every scalar verb here returns.
fn one_text(column: &str, value: String) -> Outcome {
    Outcome::Rows {
        fields: vec![FieldDescription::computed(column, ColumnType::Text)],
        rows: vec![vec![Some(value.into_bytes())]],
        tag: "SELECT 1".to_owned(),
    }
}

/// `esker_diff('<table>', '<from>'[, '<to>'])` — two scans and a merge.
///
/// ADR 0021 Decision 3, implemented as it is described there: open two read-only transactions,
/// scan the same row range in both, and walk the two sorted streams together. A key on the right
/// only is an `insert`, on the left only a `delete`, on both with different bytes an `update`, and
/// identical bytes are not a row at all. `O(rows)` with no buffering beyond one row per side,
/// because the key space is ordered and both sides come back in that order.
///
/// **Each side is rendered with its own snapshot's schema.** The two transactions each read the
/// catalog at their own timestamp, so a diff spanning an `ALTER TABLE ... ADD COLUMN` shows the
/// old row with the old columns and the new one with the new — which is what happened. Resolving
/// one schema and using it for both would print a row that never existed.
///
/// It is **not a changelog**, and the type's doc comment says so where a reader meets it first: a
/// key written and written back is invisible here, and five updates look like one.
fn diff(executor: &mut Executor, table: &str, from: &str, to: Option<&str>) -> Result<Outcome> {
    let left = executor.snapshot_named(from)?;
    let left_txn = executor.read_at(left)?;
    // `None` is the present, and the present is a fresh ordinary transaction rather than a
    // historical one: there is no timestamp to name for "now" that is not already stale.
    let right_txn = match to {
        Some(name) => {
            let right = executor.snapshot_named(name)?;
            executor.read_at(right)?
        }
        None => executor.plain_read()?,
    };

    let mut rows = Vec::new();
    let mut left_side = Side::open(&*left_txn, executor.tenant, table)?;
    let mut right_side = Side::open(&*right_txn, executor.tenant, table)?;
    let (mut left_row, mut right_row) = (left_side.next()?, right_side.next()?);
    loop {
        match (&left_row, &right_row) {
            (None, None) => break,
            (Some(old), None) => {
                rows.push(deleted(&left_side, &old.1));
                left_row = left_side.next()?;
            }
            (None, Some(new)) => {
                rows.push(inserted(&right_side, &new.1));
                right_row = right_side.next()?;
            }
            (Some(old), Some(new)) => match old.0.cmp(&new.0) {
                std::cmp::Ordering::Less => {
                    rows.push(deleted(&left_side, &old.1));
                    left_row = left_side.next()?;
                }
                std::cmp::Ordering::Greater => {
                    rows.push(inserted(&right_side, &new.1));
                    right_row = right_side.next()?;
                }
                std::cmp::Ordering::Equal => {
                    // Identical bytes are not a row. The two sides are the *same encoding* of the
                    // same values, so comparing bytes is comparing values — and it is what makes
                    // an unchanged key cost nothing but the comparison. It stays right across a
                    // schema change for a reason worth knowing: `ADD COLUMN` rewrites no row
                    // (ADR 0019), so a row nobody touched has the *same bytes* on both sides and
                    // is not a change — even though it now decodes to one more column. The column
                    // is new; the row is not.
                    if old.1 != new.1 {
                        rows.push(updated(&left_side, &old.1, &right_side, &new.1));
                    }
                    left_row = left_side.next()?;
                    right_row = right_side.next()?;
                }
            },
        }
    }

    let tag = format!("SELECT {}", rows.len());
    Ok(Outcome::Rows {
        fields: ["change", "key", "before", "after"]
            .into_iter()
            .map(|name| FieldDescription::computed(name, ColumnType::Text))
            .collect(),
        rows,
        tag,
    })
}

/// One side of a diff: a table as one snapshot sees it, and a cursor over its rows.
struct Side<'a> {
    txn: &'a dyn Txn,
    table: std::sync::Arc<catalog::TableDef>,
    schema: crate::row::RowSchema,
    next: Vec<u8>,
    end: Vec<u8>,
    batch: std::vec::IntoIter<(bytes::Bytes, bytes::Bytes)>,
}

impl<'a> Side<'a> {
    fn open(txn: &'a dyn Txn, tenant: u64, name: &str) -> Result<Self> {
        // The catalog is read through *this* transaction, so each side sees the schema its own
        // snapshot had. A table that did not exist yet is `42P01` from the side that cannot see
        // it, which is the honest answer: a diff of a table against a moment before it existed is
        // not an empty diff, it is a question about a table that was not there.
        let table = catalog::Catalog::new()
            .view_uncached(txn, tenant)?
            .require_table(name)?;
        let (next, end) = crate::row::table_row_range(tenant, table.id);
        Ok(Side {
            txn,
            schema: table.row_schema(),
            table,
            next,
            end,
            batch: Vec::new().into_iter(),
        })
    }

    /// The next `(key, value)`, a page at a time, stopping on an **empty** read.
    fn next(&mut self) -> Result<Option<(Vec<u8>, bytes::Bytes)>> {
        loop {
            if let Some((key, value)) = self.batch.next() {
                self.next = crate::exec::query::successor(&key);
                return Ok(Some((key.to_vec(), value)));
            }
            let read = self
                .txn
                .scan(&self.next, &self.end, crate::exec::SCAN_CHUNK)?;
            if read.is_empty() {
                return Ok(None);
            }
            self.batch = read.into_iter();
        }
    }

    /// A row's values, decoded with **this** side's schema.
    ///
    /// `Err` is not propagated: bytes a side cannot read are reported as a value rather than
    /// aborting the diff, because a row that will not decode is a fact about the data and hiding
    /// it would make the diff look complete.
    fn values(&self, value: &[u8]) -> std::result::Result<Vec<Datum>, String> {
        crate::row::decode_row(&self.schema, value, None).map_err(|error| error.to_string())
    }

    /// A row rendered the way PostgreSQL renders a composite: `(1, ann, t)`, `null` for a NULL.
    fn render_row(&self, value: &[u8]) -> String {
        match self.values(value) {
            Ok(row) => render_tuple(row.iter()),
            Err(error) => format!("(unreadable: {error})"),
        }
    }

    /// The primary key, rendered the way a `23505`'s `DETAIL` renders one.
    ///
    /// Taken from the **row**, not from the key bytes, and that is deliberate: the key columns are
    /// columns of the row, so reading them out of the decoded row costs nothing and couples this
    /// to no key format. The first version parsed the key and got it wrong — a row key encodes its
    /// primary key without the per-column markers an *index* key carries, so the index decoder
    /// this reached for fell through to hex on every row.
    fn render_key(&self, value: &[u8]) -> String {
        match self.values(value) {
            Ok(row) => render_tuple(self.table.primary_key.iter().filter_map(|&at| row.get(at))),
            // No key to show, and the row is already reported as unreadable beside it.
            Err(_) => "(unreadable)".to_owned(),
        }
    }
}

/// `(1, ann, t)` — the shape PostgreSQL prints a composite in, and the one a `23505`'s `DETAIL`
/// uses. No quoting, exactly as PostgreSQL does it.
fn render_tuple<'a>(values: impl Iterator<Item = &'a Datum>) -> String {
    let rendered = values
        .map(|datum| datum.to_text().unwrap_or_else(|| "null".to_owned()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("({rendered})")
}

/// The three rows a diff can produce, one constructor each.
///
/// Three rather than one function taking two `Option`s, so that "a change has at least one side"
/// is a fact about the signature rather than an `expect` at run time (`CLAUDE.md` invariant 9).
/// Each knows which side holds the row, and therefore which schema renders it and which row the
/// key comes out of.
fn deleted(left: &Side<'_>, value: &[u8]) -> Vec<Option<Vec<u8>>> {
    vec![
        Some(b"delete".to_vec()),
        Some(left.render_key(value).into_bytes()),
        Some(left.render_row(value).into_bytes()),
        None,
    ]
}

fn inserted(right: &Side<'_>, value: &[u8]) -> Vec<Option<Vec<u8>>> {
    vec![
        Some(b"insert".to_vec()),
        Some(right.render_key(value).into_bytes()),
        None,
        Some(right.render_row(value).into_bytes()),
    ]
}

fn updated(left: &Side<'_>, old: &[u8], right: &Side<'_>, new: &[u8]) -> Vec<Option<Vec<u8>>> {
    vec![
        Some(b"update".to_vec()),
        // Keyed from the newer side: the key is the same either way — the row key *is* the primary
        // key, so an update cannot move it — and taking the newer one keeps the rule "the key is
        // rendered with the schema of the row beside it" true in all three cases.
        Some(right.render_key(new).into_bytes()),
        Some(left.render_row(old).into_bytes()),
        Some(right.render_row(new).into_bytes()),
    ]
}
