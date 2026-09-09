//! The staged schema change, driven: four states and a backfill, one step per transaction.
//!
//! [ADR 0020](../../../docs/adr/0020-online-schema-change.md), `docs/plans/phase-6e.md` units 4 and
//! 5. `CREATE INDEX CONCURRENTLY` is what starts one; the states and what each is for live in
//! [`crate::catalog::SchemaState`].
//!
//! # The backfill is many transactions, and the cursor is why
//!
//! One transaction over a large table holds locks for its whole duration, conflicts with
//! everything, and outlives the lock TTL that the step arithmetic depends on — so the ADR requires
//! many small ones. Many small ones need somewhere durable to say where they got to, or a node
//! that dies mid-job restarts rather than resumes, and on a table large enough to need a job at all
//! restarting is how a job never finishes. That is [`crate::catalog::JobRecord::cursor`].
//!
//! Two properties make it safe to run beside live traffic, and both are already true:
//!
//! * it runs while every node is at **write-only**, so a row written or deleted during the
//!   backfill is maintained by its own writer. The backfill and the writer may both write the same
//!   entry — identical bytes, so the second is a no-op, and a lost race is an ordinary
//!   conflict-and-retry;
//! * a `UNIQUE` index whose backfill meets a duplicate fails the **whole change** with `23505`
//!   naming the row, which is the one place a schema change fails on *data* rather than on a
//!   conflict. The states then unwind, so a failed change leaves no half-built index behind.
//!
//! # Who drives it, and how that differs from ADR 0020
//!
//! The ADR puts the step clock in PD. PD publishes the **interval** and the **lease** (ADR 0028)
//! and that is where the arithmetic belongs, but PD does not drive the steps: a step is a *catalog
//! transaction*, the catalog is `esker-sql`'s, and PD is byte-opaque by `CLAUDE.md` invariant 7 —
//! it cannot read a table definition, let alone write one. So the job record lives in the catalog
//! where every node can see it, and a node drives it using PD's published interval as the wait.
//!
//! That is a real deviation and it is the honest one: what PD owns is the *number*, because a
//! cluster-wide bound needs one writer, and what a SQL node owns is the *transaction*, because a
//! schema change is one. Recorded in `docs/plans/phase-6e.md` §10 rather than left as a surprise.

use crate::catalog::{self, JobRecord, SchemaState, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::Executor;
use crate::exec::index::Entry;
use crate::value::Datum;

/// Rows a single backfill transaction reads and indexes.
///
/// Small on purpose. The ceiling that matters is the **lock TTL**: a batch that takes longer than
/// one holds locks past the point a resolver will roll it back, which both loses the work and
/// breaks the step arithmetic that assumes a writer cannot outlive its TTL (ADR 0020). A few
/// hundred rows is far inside three seconds for a point write, and the cost of being wrong in this
/// direction is one extra round trip per batch.
pub const BATCH_ROWS: usize = 256;

/// Runs one batch of a job's backfill, moving the cursor.
///
/// Returns whether the backfill is now finished. Each call is **one transaction of its own**, which
/// is what makes it a batch rather than a chunk of a long one.
pub(super) fn backfill_batch(executor: &Executor, index_id: u64) -> Result<bool> {
    let tenant = executor.tenant;
    let mut txn = executor.plain_read()?;

    let Some(job) = catalog::job(&*txn, tenant, index_id)? else {
        // A job that is gone has no backfill left to run. Another node finished the change while
        // this batch was being decided on, which is ordinary — and it matters that it is not an
        // error, because the arm above this one treats anything but `40001` as a change that
        // failed on **data** and unwinds it. An internal error here is one transaction away from
        // tearing down an index that is already `public`
        // (`redrive.rs::a_driver_whose_job_is_finished_inside_its_step_does_not_unwind_the_change`).
        return Ok(true);
    };
    if job.done {
        return Ok(true);
    }
    let table = executor.table_by_id(&*txn, job.table_id)?;
    let index = table
        .indexes
        .iter()
        .find(|index| index.id == index_id)
        .ok_or_else(|| {
            SqlError::Internal(format!("index {index_id} left table \"{}\"", table.name))
        })?
        .clone();

    let (start, end) = crate::row::table_row_range(tenant, table.id);
    let from = if job.cursor.is_empty() {
        start
    } else {
        job.cursor.clone()
    };

    let read = txn.scan(&from, &end, u32::try_from(BATCH_ROWS).unwrap_or(u32::MAX))?;
    let Some((last, _)) = read.last() else {
        // An **empty** read is the end of the range, and only an empty one: a short read is not
        // evidence of anything, because the store may cap a scan below what was asked
        // (`crate::exec::for_each_page`).
        let done = JobRecord { done: true, ..job };
        catalog::put_job(&mut *txn, tenant, &done);
        txn.commit()?;
        return Ok(true);
    };
    let next = crate::exec::query::successor(last);

    let schema = table.row_schema();
    let mut entries: Vec<(Entry, Vec<u8>)> = Vec::with_capacity(read.len());
    for (_, value) in &read {
        let row = crate::row::decode_row(&schema, value)?;
        if let Some(entry) = index_entry(tenant, &table, &index, &row)? {
            entries.push(entry);
        }
    }

    for (entry, value) in entries {
        // **A duplicate is the user's, and it fails the whole change.** Checked against what is
        // already there rather than against this batch alone, because the row it collides with may
        // have been indexed by an earlier batch or written by live traffic at write-only.
        if index.unique
            && let Some(existing) = txn.get(&entry.key)?
            && existing != value
        {
            // **The build's sentence, not the insert's**, and the same one the blocking backfill
            // gives (`crate::exec::ddl::backfill`): nothing was inserted here. Measured on
            // 19beta1 — `23505 could not create unique index "invalid_index"`, `DETAIL: Key
            // (number)=(1) is duplicated.` — and it reaches the client because the statement
            // waits for the change (`crate::exec::ddl::finish_concurrent_build`), which is what
            // `postgresql_adapter_test#test_invalid_index` asserts. `duplicate key value violates
            // unique constraint` is what a *writer* meeting the finished index is told.
            return Err(SqlError::CouldNotCreateUniqueIndex {
                index: index.name.clone(),
                detail: format!(
                    "{} is duplicated.",
                    crate::exec::index::render_key(&table, &index.keys, &entry.values)
                ),
            });
        }
        txn.put(&entry.key, &value);
    }

    let moved = JobRecord {
        cursor: next,
        ..job
    };
    catalog::put_job(&mut *txn, tenant, &moved);
    txn.commit()?;
    Ok(false)
}

/// The index entry for one row and the value it stores, or `None` for a row a **partial** index
/// excludes.
///
/// The whole [`Entry`] rather than its key alone, because a duplicate's `DETAIL` prints the key's
/// *values* and they are gone once the key is encoded.
///
/// Both come from `crate::exec::index`, which is the same code `crate::exec::dml` writes through:
/// an entry a backfill wrote and one a writer wrote have to be the same bytes or the two would
/// conflict forever, and a row one of them indexes and the other does not is an entry nothing
/// ever removes.
fn index_entry(
    tenant: u64,
    table: &TableDef,
    index: &catalog::IndexDef,
    row: &[Datum],
) -> Result<Option<(Entry, Vec<u8>)>> {
    let primary_key: Vec<Datum> = table
        .primary_key
        .iter()
        .map(|&ordinal| row[ordinal].clone())
        .collect();
    let Some(entry) = crate::exec::index::entry(tenant, table, index, row, &primary_key)? else {
        return Ok(None);
    };
    let value = crate::row::encode_row(&table.primary_key_types(), &primary_key)?;
    Ok(Some((entry, value)))
}

/// Moves an index one state on, in a transaction of its own, **if it is still where the caller
/// found it**.
///
/// One step, and [`catalog::advance_index_state`] is what refuses two — the two-version invariant
/// is a property of the primitive rather than of anybody's discipline.
///
/// `from` is that discipline's other half, and it exists because a step is *two* transactions: a
/// caller reads the state, decides which move it implies, and then makes it. Between those, a
/// second driver can take the same move — and the danger is not that the state ends up two on,
/// which `advance_index_state` refuses, but that it ends up one on **twice as fast**. The second
/// driver would be stepping a transition that happened moments ago rather than an interval ago,
/// and the interval is the whole bound on how stale a writer may be.
///
/// Two drivers that overlap conflict on the table record and one is rolled back; two that merely
/// *follow* each other do not overlap, and this is what stops the second. Returns whether it
/// moved: `false` means somebody else did it first — the state has already moved, or the whole
/// change has finished and its job is gone — which is an ordinary outcome and not an error.
pub(super) fn advance(
    executor: &Executor,
    index_id: u64,
    from: SchemaState,
    to: SchemaState,
) -> Result<bool> {
    let mut txn = executor.plain_read()?;
    // A job that is gone is the strongest form of "somebody else did it first" there is: the
    // change is finished and forgotten. Same answer as a state that has already moved, for the
    // same reason.
    let Some(job) = catalog::job(&*txn, executor.tenant, index_id)? else {
        return Ok(false);
    };
    let table = executor.table_by_id(&*txn, job.table_id)?;
    // Read inside the transaction that writes, so that what is checked is what is committed
    // against: a reader outside it would be checking a snapshot the write does not share.
    if table
        .indexes
        .iter()
        .find(|index| index.id == index_id)
        .is_none_or(|index| index.state != from)
    {
        return Ok(false);
    }
    catalog::advance_index_state(&mut *txn, executor.tenant, &table, index_id, to)?;
    txn.commit()?;
    Ok(true)
}

/// Takes an index away for good: its entries, its definition, its name and its job.
///
/// Only ever called at [`SchemaState::Absent`], where nothing reads the index and nothing writes
/// it — so what is removed is something no live transaction can still want. The wait that makes
/// that true is the caller's (`crate::exec::verbs::removing_step`).
pub(super) fn remove(executor: &Executor, index_id: u64, table: &TableDef) -> Result<bool> {
    let tenant = executor.tenant;
    let mut txn = executor.plain_read()?;
    // The same check `advance` makes and for the same reason, with one more thing to be sure of:
    // an index another driver has already removed is not there to remove again.
    let current = executor.table_by_id(&*txn, table.id)?;
    if !current
        .indexes
        .iter()
        .any(|index| index.id == index_id && index.state == SchemaState::Absent)
    {
        return Ok(false);
    }
    let (start, end) = crate::row::index_range(tenant, table.id, index_id);
    crate::exec::for_each_page(&mut *txn, &start, &end, |txn, page| {
        for (key, _) in page {
            txn.delete(key);
        }
        Ok(())
    })?;
    let mut updated = table.clone();
    updated.schema_version += 1;
    updated.indexes.retain(|index| index.id != index_id);
    catalog::replace_table(&mut *txn, tenant, table, &updated)?;
    catalog::drop_job(&mut *txn, tenant, index_id);
    txn.commit()?;
    Ok(true)
}

/// Unwinds a change that failed, back to `absent`, and forgets the job.
///
/// Backwards through every state rather than straight to `absent`, for the same reason the forward
/// direction goes one at a time: a node one step behind must never be two. The entries the backfill
/// wrote are left for the delete-only pass to clear as rows change — an index at `absent` is not
/// read, so what it holds cannot be wrong, only wasteful.
pub(super) fn unwind(executor: &Executor, index_id: u64) -> Result<()> {
    for to in [
        SchemaState::WriteOnly,
        SchemaState::DeleteOnly,
        SchemaState::Absent,
    ] {
        let mut txn = executor.plain_read()?;
        let Some(job) = catalog::job(&*txn, executor.tenant, index_id)? else {
            return Ok(());
        };
        let table = executor.table_by_id(&*txn, job.table_id)?;
        let current = table
            .indexes
            .iter()
            .find(|index| index.id == index_id)
            .map(|index| index.state);
        // Only the states it actually passed through, so unwinding from write-only does not try to
        // step down from a state it never reached.
        if current.is_some_and(|state| state > to) {
            catalog::advance_index_state(&mut *txn, executor.tenant, &table, index_id, to)?;
            txn.commit()?;
        }
    }

    let mut txn = executor.plain_read()?;
    catalog::drop_job(&mut *txn, executor.tenant, index_id);
    txn.commit()?;
    Ok(())
}
