//! `FLASHBACK` — putting a table back, without unwriting anything.
//!
//! [ADR 0021](../../../docs/adr/0021-time-machine.md) Decision 3's fourth verb, deferred from phase
//! 6d for exactly the batching-with-a-durable-cursor machinery
//! [`crate::exec::job`] built (`docs/plans/phase-6e.md` unit 7).
//!
//! # It is compensating writes, and that is the whole argument
//!
//! A flashback reads the table as of `ts`, reads it as of now, and **writes the difference as an
//! ordinary transaction at a fresh `commit_ts`**. It does not touch a single version that already
//! exists. So every state the table was in is still there and still readable `AS OF` an instant
//! before the correction — which means an undo is itself undoable, and the audit trail survives
//! the fix.
//!
//! The alternative — mutating history so the old rows simply are not there — would be the one
//! operation in this system that destroys evidence, and it would destroy it precisely when
//! somebody is trying to work out what happened. That is the reason this design is not the obvious
//! one.
//!
//! # It is `esker_diff` applied backwards
//!
//! The comparison is the same two-cursor merge `crate::exec::verbs::diff` does, run from *now* to
//! the *target*, and each row of that diff is a write:
//!
//! | The diff says | Because | So the flashback |
//! |---|---|---|
//! | `insert` | the row is in the target and not now | writes it back |
//! | `delete` | the row is now and not in the target | removes it |
//! | `update` | both, different bytes | writes the target's version |
//!
//! Reusing the merge rather than writing a second one matters: a flashback that disagreed with
//! `esker_diff` about what changed would be a flashback whose preview was a lie.
//!
//! # Batched, resumable, and through the ordinary write path
//!
//! Both requirements come straight from the ADR. `O(rows changed)` writes in **one** transaction
//! would hold locks for their whole duration and outlive the lock TTL on any table big enough to
//! need this, so it is batches with a durable cursor — the same shape, and the same reason, as the
//! index backfill. And every write goes through [`crate::exec::dml::write_row`] and
//! [`crate::exec::dml::remove_row`], which are the paths an `UPDATE` and a `DELETE` use, so every
//! index is maintained by the code that already knows how rather than by a shortcut that would
//! have to be kept in step with it.

use crate::backend::Txn;
use crate::catalog::{self, FlashbackRecord, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::{Executor, Written};
use crate::row::RowSchema;
use crate::value::Datum;

/// Rows one flashback transaction compares and writes.
///
/// The same ceiling and the same reason as the backfill's ([`crate::exec::job::BATCH_ROWS`]): a
/// batch that outlives the lock TTL is a batch a resolver rolls back, which loses the work. Smaller
/// than the backfill's because each row here may be a *write* rather than an index entry, and a
/// write costs a conflict check.
pub(super) const BATCH_ROWS: usize = 128;

/// Runs one batch, moving the cursor. Returns `None` when the flashback is finished.
///
/// One transaction of its own, which is what makes it a batch. The target and the cursor are read
/// back from the record every time, so a resume cannot pick up a cursor that belonged to a
/// different target.
pub(super) fn batch(executor: &Executor, table_id: u64) -> Result<Option<u64>> {
    let tenant = executor.tenant;
    let mut txn = executor.plain_read()?;

    let Some(record) = catalog::flashback(&*txn, tenant, table_id)? else {
        return Ok(None);
    };
    let table = executor.table_by_id(&*txn, table_id)?;
    let (start, end) = crate::row::table_row_range(tenant, table.id);
    let from = if record.cursor.is_empty() {
        start
    } else {
        record.cursor.clone()
    };

    // The past, as its own read-only transaction — the flashback's whole input.
    let past = executor.read_at(record.target_ts)?;
    // **The past's own schema**, because the rows there were written under it. A row from before an
    // `ADD COLUMN` decodes with the columns it had, and writing it back at today's width is what
    // makes the flashback a *write* rather than a reinterpretation.
    let past_table = table_at(executor, &*past, table_id)?;
    let past_schema = past_table.row_schema();
    let now_schema = table.row_schema();

    let limit = u32::try_from(BATCH_ROWS).unwrap_or(u32::MAX);
    let old = past.scan(&from, &end, limit)?;
    let new = txn.scan(&from, &end, limit)?;

    // The batch ends at the smaller of the two sides' last keys, so neither side runs ahead of the
    // other: a key past the end of one page might exist on the other, and calling it an insert or a
    // delete would be reading half a comparison. Both sides are asked for the same range next time.
    let boundary = match (old.last(), new.last()) {
        (None, None) => {
            let done = record.changed;
            catalog::drop_flashback(&mut *txn, tenant, table_id);
            txn.commit()?;
            return Ok(Some(done));
        }
        (Some((a, _)), Some((b, _))) => a.min(b).to_vec(),
        (Some((a, _)), None) => a.to_vec(),
        (None, Some((b, _))) => b.to_vec(),
    };

    let sides = Sides {
        table: &table,
        past: &past_schema,
        now: &now_schema,
        boundary: &boundary,
    };
    let changed = merge(executor, &mut *txn, &sides, &old, &new)?;

    let moved = FlashbackRecord {
        cursor: crate::exec::query::successor(&boundary),
        changed: record.changed + changed,
        ..record
    };
    catalog::put_flashback(&mut *txn, tenant, &moved);
    txn.commit()?;
    Ok(None)
}

/// What one batch's merge needs to know: the table it is writing, and how each side decodes.
struct Sides<'a> {
    table: &'a TableDef,
    /// The schema the target's rows were written under.
    past: &'a RowSchema,
    /// The schema today's rows are written under.
    now: &'a RowSchema,
    /// The last key both sides were read up to; anything past it belongs to the next batch.
    boundary: &'a [u8],
}

/// Walks the two sides together and writes the difference, returning how many rows moved.
///
/// The same merge `crate::exec::verbs::diff` does, and deliberately: a flashback that disagreed
/// with the diff about what changed would be a flashback whose preview was a lie.
fn merge(
    executor: &Executor,
    txn: &mut dyn Txn,
    sides: &Sides<'_>,
    old: &[(bytes::Bytes, bytes::Bytes)],
    new: &[(bytes::Bytes, bytes::Bytes)],
) -> Result<u64> {
    let mut written = Written::default();
    let mut changed = 0u64;
    let mut left = old.iter().filter(|(key, _)| key.as_ref() <= sides.boundary);
    let mut right = new.iter().filter(|(key, _)| key.as_ref() <= sides.boundary);
    let (mut a, mut b) = (left.next(), right.next());
    loop {
        match (a, b) {
            (None, None) => break,
            // In the past only: it was deleted, so write it back.
            (Some((_, value)), None) => {
                restore(
                    executor,
                    &mut *txn,
                    sides.table,
                    sides.past,
                    value,
                    &mut written,
                )?;
                changed += 1;
                a = left.next();
            }
            // Now only: it was created after the target, so remove it.
            (None, Some((_, value))) => {
                remove(executor, &mut *txn, sides.table, sides.now, value)?;
                changed += 1;
                b = right.next();
            }
            (Some((old_key, old_value)), Some((new_key, new_value))) => {
                match old_key.cmp(new_key) {
                    std::cmp::Ordering::Less => {
                        restore(
                            executor,
                            &mut *txn,
                            sides.table,
                            sides.past,
                            old_value,
                            &mut written,
                        )?;
                        changed += 1;
                        a = left.next();
                    }
                    std::cmp::Ordering::Greater => {
                        remove(executor, &mut *txn, sides.table, sides.now, new_value)?;
                        changed += 1;
                        b = right.next();
                    }
                    std::cmp::Ordering::Equal => {
                        // Identical bytes are not a change, so an unchanged row costs a comparison
                        // and no write at all — which is what makes a flashback `O(rows changed)`
                        // rather than `O(rows)`.
                        if old_value != new_value {
                            remove(executor, &mut *txn, sides.table, sides.now, new_value)?;
                            restore(
                                executor,
                                &mut *txn,
                                sides.table,
                                sides.past,
                                old_value,
                                &mut written,
                            )?;
                            changed += 1;
                        }
                        a = left.next();
                        b = right.next();
                    }
                }
            }
        }
    }

    Ok(changed)
}

/// The table as it was at the flashback's target.
///
/// Read through the historical transaction, so a flashback across a schema change sees the schema
/// each side actually had. A table that did not exist then is `42P01` from that side, which is the
/// honest answer: putting a table back to before it existed is `DROP TABLE`, and ADR 0021 puts
/// restoring a dropped table explicitly out of scope.
fn table_at(
    executor: &Executor,
    past: &dyn Txn,
    table_id: u64,
) -> Result<std::sync::Arc<TableDef>> {
    catalog::Catalog::new()
        .view_uncached(past, executor.tenant)?
        .table_by_id(table_id)?
        .ok_or_else(|| {
            SqlError::UndefinedTable(format!("table {table_id} did not exist at that snapshot"))
        })
}

/// Writes one row back, through the path an `INSERT` uses.
fn restore(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    schema: &RowSchema,
    value: &[u8],
    written: &mut Written,
) -> Result<()> {
    let mut row = crate::row::decode_row(schema, value)?;
    // Widened to today's shape if the table has gained columns since. The pad is the catalog's
    // missing value, which is the same answer a read of that row would have given.
    row.resize(table.columns.len(), Datum::Null);
    crate::exec::dml::write_row(executor, txn, table, &row, written)
}

/// Removes one row, through the path a `DELETE` uses — so every index is maintained by the code
/// that already knows how.
fn remove(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    schema: &RowSchema,
    value: &[u8],
) -> Result<()> {
    let row = crate::row::decode_row(schema, value)?;
    // The keys it removed are of no interest here: a flashback is a rewrite of the whole table
    // rather than a statement whose failed commit has to be explained per key.
    crate::exec::dml::remove_row(executor, txn, table, &row)?;
    Ok(())
}
