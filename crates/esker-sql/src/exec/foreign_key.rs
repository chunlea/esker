//! `FOREIGN KEY`, enforced — both directions, inside the statement's own transaction.
//!
//! Two questions, asked from opposite ends of the same constraint:
//!
//! * **A child row is written.** Does the row it points at exist? Asked of every constraint this
//!   table is the child of, which its own record holds ([`crate::catalog::TableDef::foreign_keys`]).
//! * **A parent row is removed, or its key changed.** Does anything still point at it? A table
//!   cannot answer that about itself, so the question is put to a key space instead
//!   ([`crate::catalog::foreign_key_backref_range`]): one short prefix scan gives the children, and each child's own
//!   record says which of its constraints apply.
//!
//! Both are **immediate**, which is what `DEFERRABLE INITIALLY IMMEDIATE` means on a real server
//! too — and it is why `INITIALLY DEFERRED` is refused by name rather than accepted: a
//! transaction that violates the constraint in the middle and repairs it before `COMMIT` succeeds
//! there and would be refused here.
//!
//! # A NULL points at nothing, and that is not a violation
//!
//! `MATCH SIMPLE`, the default and the only kind here: a row whose referencing columns are **all**
//! NULL is admitted, and so is one where *any* of them is. Measured — `INSERT INTO fxc VALUES (12,
//! NULL, NULL)` succeeds under a constraint the value `99` fails. `MATCH FULL` is the kind that
//! refuses a partly-NULL key, and it is refused by name for exactly that reason.

use std::collections::HashSet;

use crate::backend::Txn;
use crate::catalog::{self, ForeignKeyDef, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::Executor;
use crate::exec::trigger::{AfterRows, Event};
use crate::row;
use crate::value::{Datum, PgDatum};

/// How deep a `CASCADE` may follow itself before this crate calls it a cycle.
///
/// A cycle between two `ON DELETE CASCADE` constraints is a real schema a real server accepts, and
/// it terminates there because a row already deleted is not deleted again. It terminates here for
/// the same reason — the rows run out — and this bound is the guard against the case where it does
/// not: a bug in the walk, rather than a schema a user wrote. Deep enough that no schema reaches
/// it, small enough that a runaway stops instead of exhausting the stack.
const MAX_CASCADE_DEPTH: usize = 32;

/// Whether this table's referential checks are running.
///
/// **A foreign key is two triggers, on two different tables**, and `DISABLE TRIGGER ALL` disables
/// the ones on the table it names. So the child's check — "the row I point at must exist" — is
/// suspended by disabling the **child**, and the parent's — "nothing may point at the row I am
/// removing" — by disabling the **parent**. Measured on PostgreSQL 19, including the case that
/// looks like it should be symmetric and is not: with the parent disabled, an `INSERT` into the
/// child naming a missing parent is still `23503`.
///
/// `DISABLE TRIGGER USER` leaves this alone: it covers only the triggers a user created
/// ([`crate::catalog::TriggerDef::enabled`]), and this is PostgreSQL's internal ones. Measured the
/// other way round too — `ENABLE TRIGGER USER` after `DISABLE TRIGGER ALL` still lets a row with no
/// parent in.
fn enforcing(table: &TableDef) -> bool {
    !table.triggers_disabled
}

/// Every constraint this row must satisfy as a **child**: the row it points at has to exist.
///
/// Called for every row an `INSERT` or an `UPDATE` writes, after the row is built and before it is
/// stored, which is the same place `CHECK` is enforced and for the same reason: a violation must
/// leave nothing behind.
pub(super) fn check_references(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
) -> Result<()> {
    if !enforcing(table) {
        return Ok(());
    }
    for (at, key) in table.foreign_keys.iter().enumerate() {
        let Some(values) = referencing_values(key, row) else {
            // A NULL in the key: `MATCH SIMPLE` admits it, and this is the whole of that rule.
            continue;
        };
        let parent = executor.table_by_id(txn, key.parent)?;
        // **Deferred: the question is asked at `COMMIT` instead**, against the transaction as it
        // stands then — which is what lets a child be written before its parent and both commit.
        if key.deferrable && executor.constraint_is_deferred(&key.name, key.initially_deferred) {
            executor.defer_check(super::deferred::Check::ForeignKey {
                table: Executor::table_arc(table),
                at,
                parent,
                values,
            });
            continue;
        }
        if parent_row(&parent, executor.tenant, txn, key, &values)?.is_none() {
            return Err(SqlError::ForeignKeyViolation {
                relation: message_name(&table.name).to_owned(),
                constraint: key.name.clone(),
                detail: format!(
                    "Key ({})=({}) is not present in table \"{}\".",
                    column_names(table, &key.columns),
                    super::index::render_values(&values),
                    message_name(&parent.name)
                ),
            });
        }
    }
    Ok(())
}

/// Check the rows **already there** against one constraint — the scan `NOT VALID` skips.
///
/// A plain `ADD CONSTRAINT … FOREIGN KEY` runs it and refuses `23503` naming the first row that
/// fails; `NOT VALID` does not, and `VALIDATE CONSTRAINT` runs it later. The rows are collected a
/// page at a time before any parent is looked up, because the lookup needs the transaction the
/// walk is holding.
pub(super) fn validate(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    key: &ForeignKeyDef,
) -> Result<()> {
    let (start, end) = row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut pending: Vec<Vec<Datum>> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (_, value) in page {
            let decoded = row::decode_row(&schema, value, None)?;
            // A NULL key points at nothing and is not a violation, here as at an `INSERT`.
            if let Some(values) = referencing_values(key, &decoded) {
                pending.push(values);
            }
        }
        Ok(())
    })?;
    let parent = executor.table_by_id(txn, key.parent)?;
    for values in pending {
        if parent_row(&parent, executor.tenant, txn, key, &values)?.is_none() {
            return Err(SqlError::ForeignKeyViolation {
                relation: message_name(&table.name).to_owned(),
                constraint: key.name.clone(),
                detail: format!(
                    "Key ({})=({}) is not present in table \"{}\".",
                    column_names(table, &key.columns),
                    super::index::render_values(&values),
                    message_name(&parent.name)
                ),
            });
        }
    }
    Ok(())
}

/// Every constraint that points **at** this row, before it is removed.
///
/// `NO ACTION` and `RESTRICT` refuse; `CASCADE` deletes the referencing rows, which may in turn
/// have children of their own — the walk is what makes a chain work and is bounded by
/// [`MAX_CASCADE_DEPTH`].
pub(super) fn on_parent_removed(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
    written: &mut super::Written,
) -> Result<()> {
    if !enforcing(table) {
        return Ok(());
    }
    cascade_delete(executor, txn, table, row, 0, written)
}

/// The refusing half of an `UPDATE` on a parent row, run **before** the row moves.
///
/// Split from the cascading half so that a `RESTRICT` leaves the table exactly as it was and a
/// `CASCADE` runs against a parent that has already moved. See [`cascade_update`].
pub(super) fn refuse_if_referenced(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    old: &[Datum],
    new: &[Datum],
) -> Result<()> {
    if !enforcing(table) {
        return Ok(());
    }
    for (child, key) in children_of(executor, txn, table)? {
        if !key.on_update.refuses() {
            continue;
        }
        let Some(before) = moved(&key, old, new) else {
            continue;
        };
        if !referencing_rows(executor, txn, &child, &key, &before)?.is_empty() {
            return Err(still_referenced(table, &child, &key, &before));
        }
    }
    Ok(())
}

/// `ON UPDATE CASCADE`, applied **after** the parent row has moved.
///
/// The order is the whole of it: a child's key is rewritten by [`super::dml::rewrite_row`], which
/// re-checks the child's own foreign keys — including this one — so the parent row must already
/// be at its new key or the child would be refused for pointing at a row that is about to exist.
/// A real server does the same thing in the same order, and an implementation that cascades first
/// fails on its own constraint. Measured: `UPDATE fxq SET id = 5` moves both children to `5`.
pub(super) fn cascade_update(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    old: &[Datum],
    new: &[Datum],
    written: &mut super::Written,
) -> Result<()> {
    if !enforcing(table) {
        return Ok(());
    }
    for (child, key) in children_of(executor, txn, table)? {
        if key.on_update.refuses() {
            continue;
        }
        let Some(before) = moved(&key, old, new) else {
            continue;
        };
        // `CASCADE` follows the parent's new key; `SET NULL` and `SET DEFAULT` write their own
        // value instead and do not.
        let after = match written_by(txn, executor.tenant, &child, &key, key.on_update)? {
            Some(values) => values,
            None => referenced_values(&key, new).unwrap_or_default(),
        };
        let mut queued = AfterRows::new(Event::Update);
        for old_child in referencing_rows(executor, txn, &child, &key, &before)? {
            let mut new_child = old_child.clone();
            // **Into the child's type, not the parent's.** `CASCADE` carries the parent's new
            // key across, and a `bigserial` parent with an `integer` child — Rails' default
            // pairing — hands an `Int8` to an `Int4` column. This is the assignment cast every
            // written value gets, applied here because the row is assembled here; without it the
            // write was refused outright as `column N is Int4 and was given Int8`.
            for (at, value) in key.columns.iter().zip(after.iter()) {
                new_child[*at] = super::dml::assign_default(value.clone(), child.columns[*at].ty)?;
            }
            // **The child's row triggers fire**: PostgreSQL's cascade is an `UPDATE` of the child —
            // measured, and a `BEFORE` trigger that points the key elsewhere is the `23503` the
            // rewrite below checks for.
            let Some(new_child) = executor.before_row(
                txn,
                written,
                &child,
                Event::Update,
                Some(&old_child),
                new_child,
            )?
            else {
                continue;
            };
            super::dml::rewrite_row(executor, txn, &child, &old_child, &new_child, written)?;
            queued.remember(&child, Some(old_child), Some(new_child));
        }
        executor.after_rows(txn, written, queued)?;
    }
    Ok(())
}

/// The key this constraint watched **before** an update, or `None` when the update did not move
/// it: an `UPDATE` that rewrites a column no constraint references is not a referential event.
fn moved(key: &ForeignKeyDef, old: &[Datum], new: &[Datum]) -> Option<Vec<Datum>> {
    let before = referenced_values(key, old)?;
    if referenced_values(key, new).as_deref() == Some(before.as_slice()) {
        return None;
    }
    Some(before)
}

/// One level of the delete walk.
fn cascade_delete(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
    depth: usize,
    written: &mut super::Written,
) -> Result<()> {
    if depth > MAX_CASCADE_DEPTH {
        return Err(SqlError::Internal(format!(
            "a foreign-key CASCADE from \"{}\" is more than {MAX_CASCADE_DEPTH} levels deep",
            table.name
        )));
    }
    for (child, key) in children_of(executor, txn, table)? {
        let Some(values) = referenced_values(&key, row) else {
            continue;
        };
        let referencing = referencing_rows(executor, txn, &child, &key, &values)?;
        if referencing.is_empty() {
            continue;
        }
        if key.on_delete.refuses() {
            return Err(still_referenced(table, &child, &key, &values));
        }
        // `SET NULL` and `SET DEFAULT` keep the child row and clear what pointed at the parent.
        // Nothing recurses: the child's **own** key is untouched, so its children still point at
        // a row that is exactly where it was.
        if let Some(values) = written_by(txn, executor.tenant, &child, &key, key.on_delete)? {
            let mut queued = AfterRows::new(Event::Update);
            for old_child in referencing {
                let mut new_child = old_child.clone();
                for (at, value) in key.columns.iter().zip(values.iter()) {
                    new_child[*at] = value.clone();
                }
                // **An `UPDATE` of the child, so its row triggers fire** — measured for `SET NULL`.
                // Into the statement's own `Written`, which is what `rewrite_row` asks for.
                let Some(new_child) = executor.before_row(
                    txn,
                    written,
                    &child,
                    Event::Update,
                    Some(&old_child),
                    new_child,
                )?
                else {
                    continue;
                };
                super::dml::rewrite_row(executor, txn, &child, &old_child, &new_child, written)?;
                queued.remember(&child, Some(old_child), Some(new_child));
            }
            executor.after_rows(txn, written, queued)?;
            continue;
        }
        let mut queued = AfterRows::new(Event::Delete);
        for child_row in referencing {
            // **A `DELETE` of the child, so its row triggers fire**: a `BEFORE DELETE` answering
            // `RETURN NULL` keeps the child row, and PostgreSQL raises nothing for it — measured.
            let Some(child_row) =
                executor.before_row(txn, written, &child, Event::Delete, None, child_row)?
            else {
                continue;
            };
            // Depth-first: the grandchildren go before the child, so no row is ever removed while
            // something still points at it.
            cascade_delete(executor, txn, &child, &child_row, depth + 1, written)?;
            super::dml::remove_row(executor, txn, &child, &child_row)?;
            queued.remember(&child, Some(child_row), None);
        }
        executor.after_rows(txn, written, queued)?;
    }
    Ok(())
}

/// What `SET NULL` and `SET DEFAULT` put into the child's referencing columns.
///
/// **`SET DEFAULT` re-reads the column's default**, expression and all, rather than reusing a
/// value folded at `CREATE TABLE`: the two differ for anything non-constant, and the rewritten row
/// is checked against the constraint afterwards like any other — a default naming a parent that is
/// not there is `23503` from the `DELETE`, which is what a real server answers.
fn written_by(
    txn: &dyn Txn,
    tenant: u64,
    child: &TableDef,
    key: &ForeignKeyDef,
    action: catalog::ReferentialAction,
) -> Result<Option<Vec<Datum>>> {
    match action {
        catalog::ReferentialAction::SetNull => Some(
            key.columns
                .iter()
                .map(|_| Ok(Datum::Null))
                .collect::<Result<Vec<_>>>(),
        ),
        catalog::ReferentialAction::SetDefault => Some(
            key.columns
                .iter()
                .map(|at| {
                    let column = child.columns.get(*at).ok_or_else(|| {
                        SqlError::Internal(format!(
                            "a foreign key on \"{}\" names column {at}, which is not there",
                            child.name
                        ))
                    })?;
                    super::dml::column_default_value(child, column, txn, tenant)
                })
                .collect::<Result<Vec<_>>>(),
        ),
        _ => None,
    }
    .transpose()
}

/// The name a `23503` prints for a relation: **bare**, whatever schema it lives in.
///
/// PostgreSQL builds both of these messages from the relation's own name and never the qualified
/// one — measured on 19beta1 with the child in one schema and the parent in another:
///
/// ```text
/// insert or update on table "wagons" violates foreign key constraint "fk_x"
///   DETAIL:  Key (train_id)=(99) is not present in table "trains".
/// ```
///
/// where those are `sb.wagons` and `sa.trains`. The same holds for the parent-side message, for a
/// deferred check at `COMMIT`, and for the scan `ADD CONSTRAINT` runs.
///
/// **This is not the rule for every message**: `42P01` quotes the qualifier back inside the
/// quotes, which is what [`crate::catalog::display_name`] is for. Here the stored name would put
/// its NUL separator inside them instead.
pub(super) fn message_name(stored: &str) -> &str {
    catalog::split_qualified(stored).1
}

/// `23503` from the parent's side, which names **both** tables — the one difference between the
/// two messages a real server sends for this SQLSTATE.
fn still_referenced(
    parent: &TableDef,
    child: &TableDef,
    key: &ForeignKeyDef,
    values: &[Datum],
) -> SqlError {
    SqlError::ForeignKeyStillReferenced {
        relation: message_name(&parent.name).to_owned(),
        constraint: key.name.clone(),
        child: message_name(&child.name).to_owned(),
        detail: format!(
            "Key ({})=({}) is still referenced from table \"{}\".",
            column_names(parent, &key.parent_columns),
            super::index::render_values(values),
            message_name(&child.name)
        ),
    }
}

/// Every (child table, constraint) pair that references this table.
///
/// The back-reference range gives the child ids in one scan; each child's record says which of its
/// constraints point here. A child that has been dropped leaves no record, and its back-reference
/// goes with it (`crate::exec::ddl::drop_table`) — so a missing table here is skipped rather than
/// an error, which is the same tolerance a `DELETE` of an absent index entry has.
pub(super) fn children_of(
    executor: &Executor,
    txn: &dyn Txn,
    table: &TableDef,
) -> Result<Vec<(std::sync::Arc<TableDef>, ForeignKeyDef)>> {
    let (start, end) = catalog::foreign_key_backref_range(executor.tenant, table.id);
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for (key, _) in txn.scan(&start, &end, 0)? {
        let child_id = catalog::foreign_key_backref_child(executor.tenant, table.id, &key)?;
        if !seen.insert(child_id) {
            continue;
        }
        let child = executor.table_by_id(txn, child_id)?;
        for constraint in &child.foreign_keys {
            if constraint.parent == table.id {
                found.push((std::sync::Arc::clone(&child), constraint.clone()));
            }
        }
    }
    Ok(found)
}

/// The rows of `child` whose referencing columns equal `values`.
///
/// **A scan of the child table.** PostgreSQL requires a unique index on the *parent* side and
/// leaves the child side to the user, which is why `ActiveRecord` creates one and why a real
/// server is fast here; this node has no way to use one for a range read yet
/// (`crate::exec::query::access_path` narrows only on a whole pinned unique key), so the scan is
/// what makes it correct. `TODO(post-v1)`: read the child's index when there is one, which is a
/// planner change and not a change to this rule.
/// Whether a child's key **is** a parent's, by PostgreSQL's equality rather than the enum's.
///
/// `Datum::Int4(1)` and `Datum::Int8(1)` are different values and the same key. That is not an
/// edge case here: `t.integer :train_id` referencing a `bigserial` primary key is what
/// `ActiveRecord` writes by default, so it is the ordinary shape of a Rails foreign key.
///
/// The child's side of the constraint has always agreed, because it asks by building the parent's
/// row key and the memcomparable codec widens both to the same bytes — which is why an `INSERT`
/// into such a child is checked correctly. Only this direction compared `Vec<Datum>` with `==`,
/// so `DELETE` from the parent found no referencing row, **returned success, and left the child
/// pointing at nothing**. `ON DELETE CASCADE`, `SET NULL` and the `ON UPDATE` pair went through
/// the same function and did nothing at all.
///
/// [`crate::value::PgDatum::pg_cmp`] is the rule `=` and `GROUP BY` use, and the one the two
/// directions now share. A NULL never arrives: `referencing_values` returns `None` for a key with
/// one, which is `MATCH SIMPLE`.
fn references(child: &[Datum], parent: &[Datum]) -> bool {
    child.len() == parent.len() && std::iter::zip(child, parent).all(|(a, b)| a.pg_cmp(b).is_eq())
}

fn referencing_rows(
    executor: &Executor,
    txn: &dyn Txn,
    child: &TableDef,
    key: &ForeignKeyDef,
    values: &[Datum],
) -> Result<Vec<Vec<Datum>>> {
    let (start, end) = row::table_row_range(executor.tenant, child.id);
    let schema = child.row_schema();
    let mut rows = Vec::new();
    for (_, encoded) in txn.scan(&start, &end, 0)? {
        let row = row::decode_row(&schema, &encoded, None)?;
        let Some(referencing) = referencing_values(key, &row) else {
            continue;
        };
        if references(&referencing, values) {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// The parent row this child row points at, or `None`.
///
/// The parent's referenced columns are its primary key or a unique index — checked when the
/// constraint was made (`42830` otherwise) — so this is a point read and not a scan.
pub(super) fn parent_row(
    parent: &TableDef,
    tenant: u64,
    txn: &dyn Txn,
    key: &ForeignKeyDef,
    values: &[Datum],
) -> Result<Option<Vec<Datum>>> {
    if parent.primary_key == key.parent_columns {
        let row_key = row::row_key(tenant, parent.id, values)?;
        return match txn.get(&row_key)? {
            Some(encoded) => Ok(Some(row::decode_row(&parent.row_schema(), &encoded, None)?)),
            None => Ok(None),
        };
    }
    // A unique index: its entry holds the parent's primary key, which the row is then read by.
    let Some(index) = parent.indexes.iter().find(|index| {
        index.unique
            && index.state.readable()
            && index.predicate.is_none()
            && index.key_columns().as_deref() == Some(key.parent_columns.as_slice())
    }) else {
        return Err(SqlError::Internal(format!(
            "foreign key {} references columns with no unique index behind them",
            key.name
        )));
    };
    let index_key = row::index_key(tenant, parent.id, index.id, values, None)?;
    let Some(encoded) = txn.get(&index_key)? else {
        return Ok(None);
    };
    let primary_key = row::decode_row(
        &row::RowSchema::nullable(parent.primary_key_types()),
        &encoded,
        None,
    )?;
    let row_key = row::row_key(tenant, parent.id, &primary_key)?;
    match txn.get(&row_key)? {
        Some(encoded) => Ok(Some(row::decode_row(&parent.row_schema(), &encoded, None)?)),
        None => Ok(None),
    }
}

/// The referencing columns' values, or `None` when any of them is NULL.
fn referencing_values(key: &ForeignKeyDef, row: &[Datum]) -> Option<Vec<Datum>> {
    key.columns
        .iter()
        .map(|at| match row.get(*at) {
            Some(Datum::Null) | None => None,
            Some(value) => Some(value.clone()),
        })
        .collect()
}

/// The referenced columns' values of a parent row, or `None` when any of them is NULL.
///
/// A NULL cannot be pointed at — no child row's key ever equals it, because a child with a NULL is
/// admitted without looking — so there is nothing to check and nothing to cascade.
fn referenced_values(key: &ForeignKeyDef, row: &[Datum]) -> Option<Vec<Datum>> {
    key.parent_columns
        .iter()
        .map(|at| match row.get(*at) {
            Some(Datum::Null) | None => None,
            Some(value) => Some(value.clone()),
        })
        .collect()
}

/// `a, b` for a message's `Key (…)`.
pub(super) fn column_names(table: &TableDef, ordinals: &[usize]) -> String {
    ordinals
        .iter()
        .filter_map(|at| table.columns.get(*at).map(|column| column.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}
