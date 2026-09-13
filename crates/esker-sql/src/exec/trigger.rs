//! Row triggers: which of them fire, in what order, and what a `BEFORE` trigger's answer does to the
//! row — [ADR 0113](../../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md),
//! `docs/plans/plpgsql-subset.md` §7.
//!
//! **PostgreSQL's order for one row, and this node's.** Defaults, sequences and the row id; then
//! every enabled `BEFORE ROW` trigger in name order, each handed the `NEW` the one before it
//! returned; then generated columns, `NOT NULL`, `CHECK` and the write. Once the statement has
//! written every row, its `AFTER ROW` triggers fire, and each of them sees all of those rows.
//! `RETURN NULL` from a `BEFORE` trigger takes the row out of the statement: not written, not in
//! `RETURNING`, not counted. All measured (`tests/corpus/pg19_plpgsql_trigger.txt`).
//!
//! **Every write a statement makes to a row fires them**: `INSERT`, `UPDATE` and `DELETE` — through
//! an inheritance parent, the triggers of the child the row is in — and a foreign key's `CASCADE`,
//! `SET NULL` and `SET DEFAULT`, which PostgreSQL runs as statements on the child. Rows moved by
//! something that is not a statement's write — `ALTER TABLE` and `ALTER TYPE` rewriting what is
//! stored, a flashback restore — fire nothing, in PostgreSQL as here.
//!
//! **A body runs in its statement's transaction**, through `exec::plpgsql`, so what it writes is
//! undone with the statement that fired it — measured too: the audit row a trigger wrote for a
//! statement that then failed on a duplicate key is not there.

use std::sync::Arc;

use super::{Executor, Written};
use crate::backend::Txn;
use crate::catalog::{TableDef, TriggerDef};
use crate::error::{Result, SqlError};
use crate::value::Datum;

/// Which statement fires a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Event {
    /// `INSERT`: `NEW` is the row, and `OLD` is `NULL`.
    Insert,
    /// `UPDATE`: `OLD` is the stored row, `NEW` the one about to be written.
    Update,
    /// `DELETE`: `OLD` is the row, and `NEW` is `NULL`.
    Delete,
}

impl Event {
    /// PostgreSQL's `tgtype` bit for the event, which is what a stored trigger carries.
    fn bit(self) -> i16 {
        match self {
            Event::Insert => 4,
            Event::Delete => 8,
            Event::Update => 16,
        }
    }
}

/// One row an `AFTER ROW` trigger will see, once its statement has written every row.
#[derive(Debug)]
struct AfterRow {
    table: Arc<TableDef>,
    old: Option<Vec<Datum>>,
    new: Option<Vec<Datum>>,
}

/// The rows a statement's `AFTER ROW` triggers will see, remembered as the statement writes them.
#[derive(Debug)]
pub(super) struct AfterRows {
    event: Event,
    rows: Vec<AfterRow>,
}

impl AfterRows {
    /// Nothing remembered yet, for the `AFTER` triggers of `event`.
    pub(super) fn new(event: Event) -> AfterRows {
        AfterRows {
            event,
            rows: Vec::new(),
        }
    }

    /// A row the statement wrote into `table`, with `OLD` and `NEW` as the event has them — kept
    /// only when `table` has an enabled `AFTER` trigger for the event, so that a statement on a
    /// table with none, which is nearly every statement, keeps no copy of what it wrote.
    pub(super) fn remember(
        &mut self,
        table: &Arc<TableDef>,
        old: Option<Vec<Datum>>,
        new: Option<Vec<Datum>>,
    ) {
        let bit = self.event.bit();
        if table
            .triggers
            .iter()
            .any(|trigger| trigger.enabled && !trigger.before && trigger.events & bit != 0)
        {
            self.rows.push(AfterRow {
                table: Arc::clone(table),
                old,
                new,
            });
        }
    }
}

/// **`INSERT … ON CONFLICT` into a table with an enabled `INSERT` or `UPDATE` row trigger is refused
/// by name.** PostgreSQL fires `BEFORE INSERT` for the proposed row and then, on the conflict path,
/// the `UPDATE` triggers for the existing one — an ordering of its own, which this node does not
/// build (`docs/plans/plpgsql-subset.md` §7).
pub(super) fn refuse_on_conflict(table: &TableDef) -> Result<()> {
    let events = Event::Insert.bit() | Event::Update.bit();
    if table
        .triggers
        .iter()
        .any(|trigger| trigger.enabled && trigger.events & events != 0)
    {
        return Err(SqlError::unsupported(
            "INSERT ... ON CONFLICT on a table with a row trigger",
        ));
    }
    Ok(())
}

/// The enabled row triggers of `table` for `event` at one timing, in name order.
///
/// **What `CREATE TRIGGER` refuses since ADR 0113 is refused here too**, for a trigger stored before
/// then: a statement-level trigger, and one on a partitioned table or a partition. Skipped, either
/// would be a trigger that silently never fires.
fn chosen(table: &TableDef, before: bool, event: Event) -> Result<Vec<&TriggerDef>> {
    let mut triggers = Vec::new();
    for trigger in &table.triggers {
        if !trigger.enabled || trigger.before != before || trigger.events & event.bit() == 0 {
            continue;
        }
        if !trigger.for_each_row {
            return Err(SqlError::unsupported(
                "CREATE TRIGGER ... FOR EACH STATEMENT",
            ));
        }
        if table.partition_by.is_some() || table.partition_bound.is_some() {
            return Err(SqlError::unsupported(
                "CREATE TRIGGER on a partitioned table or a partition",
            ));
        }
        triggers.push(trigger);
    }
    // PostgreSQL fires the triggers of one event in name order, by bytes — measured: `s2t_a_upper`
    // before `s2t_b_skip`, whatever order they were created in.
    triggers.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    Ok(triggers)
}

impl Executor {
    /// The `BEFORE ROW` triggers of `table` for `event` over one row: the row that goes on, or
    /// `None` when a trigger answered `RETURN NULL`.
    ///
    /// For an `INSERT` the row is `NEW`; for an `UPDATE` it is `NEW` and `old` is `OLD`; for a
    /// `DELETE` it is `OLD`, and it is what goes on unless a trigger says otherwise. Each trigger is
    /// handed the `NEW` the one before it returned, and `OLD` is the stored row every time.
    pub(super) fn before_row(
        &mut self,
        txn: &mut dyn Txn,
        written: &mut Written,
        table: &TableDef,
        event: Event,
        old: Option<&[Datum]>,
        row: Vec<Datum>,
    ) -> Result<Option<Vec<Datum>>> {
        let triggers = chosen(table, true, event)?;
        if triggers.is_empty() {
            return Ok(Some(row));
        }
        let mut row = row;
        // **A generated column reads `NULL` in a `BEFORE` trigger**, on an `UPDATE` as on an
        // `INSERT` — measured — because it is computed from the row the triggers leave.
        if event != Event::Delete {
            for (slot, column) in row.iter_mut().zip(&table.columns) {
                if column.generated.is_some() {
                    *slot = Datum::Null;
                }
            }
        }
        for trigger in triggers {
            let returned = match event {
                Event::Insert => {
                    self.run_trigger(txn, written, trigger, table, None, Some(&row))?
                }
                Event::Update => self.run_trigger(txn, written, trigger, table, old, Some(&row))?,
                Event::Delete => {
                    self.run_trigger(txn, written, trigger, table, Some(&row), None)?
                }
            };
            match (event, returned) {
                (_, None) => return Ok(None),
                // A `DELETE` goes on with the stored row, whatever row a trigger handed back.
                (Event::Delete, Some(_)) => {}
                (_, Some(next)) => row = next,
            }
        }
        if event != Event::Delete {
            // What a trigger handed back is fitted to its columns again and its generated columns
            // are computed from it here, because a foreign key's `SET NULL` writes the row with no
            // pass of its own.
            super::dml::fit_typmods(table, &mut row)?;
            super::dml::fill_generated(table, &mut row)?;
        }
        Ok(Some(row))
    }

    /// The `AFTER ROW` triggers for the rows a statement has finished writing, in the order it wrote
    /// them. An `AFTER` trigger's answer changes nothing; its errors fail the statement.
    pub(super) fn after_rows(
        &mut self,
        txn: &mut dyn Txn,
        written: &mut Written,
        rows: AfterRows,
    ) -> Result<()> {
        let AfterRows { event, rows } = rows;
        for row in rows {
            for trigger in chosen(&row.table, false, event)? {
                self.run_trigger(
                    txn,
                    written,
                    trigger,
                    &row.table,
                    row.old.as_deref(),
                    row.new.as_deref(),
                )?;
            }
        }
        Ok(())
    }

    /// One trigger's function, over one row.
    fn run_trigger(
        &mut self,
        txn: &mut dyn Txn,
        written: &mut Written,
        trigger: &TriggerDef,
        table: &TableDef,
        old: Option<&[Datum]>,
        new: Option<&[Datum]>,
    ) -> Result<Option<Vec<Datum>>> {
        // `DROP FUNCTION` refuses while a trigger names the function, so a function that is not
        // there is a record gone missing rather than a statement to answer in PostgreSQL's words.
        let Some(function) = crate::catalog::function(&*txn, self.tenant, &trigger.function)?
        else {
            return Err(SqlError::Internal(format!(
                "trigger \"{}\" names function {}(), which is not there",
                trigger.name, trigger.function
            )));
        };
        self.run_trigger_function(txn, written, &function.body, table, old, new)
    }
}
