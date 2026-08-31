//! `INSERT`: a row's worth of values into a row's worth of keys.
//!
//! One row becomes one key/value pair plus one entry per index, all written into the caller's
//! transaction. Nothing here can fail at the write — `Txn::put` buffers (`crate::backend`) — so the
//! checks that *can* fail all happen before it: the column list resolves, every value assigns to
//! its column's type, no `NOT NULL` column is left NULL, and every unique index key is read and
//! required absent.
//!
//! # The two ways a duplicate arrives, and where each is caught
//!
//! `docs/plans/phase-6a.md` §5 rules that uniqueness needs no new storage primitive. Here is the
//! half of that ruling this file implements:
//!
//! 1. **Already committed.** The read below is at the transaction's snapshot, so an entry another
//!    transaction committed is visible, and `23505` is raised before anything is written.
//! 2. **Concurrent.** Two transactions both read the key as absent and both write it. Neither can
//!    see the other, and no read here could have caught it. Percolator's write-write conflict
//!    detection lets exactly one commit, and the loser is told `40001` — which `crate::exec` turns
//!    back into `23505`, because it recorded that the key was a unique index entry.
//!
//! The primary key is the same rule with no index behind it: two rows with one key *are* one key,
//! so the row key itself is read and required absent, and the constraint named is the table's
//! `_pkey`.

use crate::backend::Txn;
use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::exec::{Executor, Unique, Written};
use crate::exec::{cursor, query};
use crate::pgwire::session::Outcome;
use crate::plan::{Delete, Insert, Update};
use crate::row;
use crate::value::Datum;

pub(super) fn insert(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    insert: &Insert,
    written: &mut Written,
) -> Result<Outcome> {
    let table = executor.require_table(txn, &insert.table)?;
    let targets = target_columns(&table, insert)?;

    for values in &insert.rows {
        if values.len() > targets.len() {
            // PostgreSQL calls this a syntax error, oddly enough, and says so before it looks at
            // any of the values.
            return Err(SqlError::InsertTooManyExpressions);
        }

        // Every column starts NULL: `INSERT INTO t (a) VALUES (1)` leaves the rest NULL, and so
        // does a `VALUES` tuple shorter than the column list.
        let mut row = vec![Datum::Null; table.columns.len()];
        for (target, expr) in targets.iter().zip(values) {
            let column = &table.columns[*target];
            row[*target] = expr.evaluate(column.ty, &column.name)?;
        }

        check_not_null(&table, &row)?;
        write_row(executor, txn, &table, &row, written)?;
    }

    // The leading zero is the OID of the inserted row, which PostgreSQL stopped assigning in 8.1
    // and still reports as 0. A client that parses the tag expects three fields.
    Ok(Outcome::done(format!("INSERT 0 {}", insert.rows.len())))
}

/// Which column each value in a `VALUES` tuple is for.
fn target_columns(table: &TableDef, insert: &Insert) -> Result<Vec<usize>> {
    let Some(names) = &insert.columns else {
        return Ok((0..table.columns.len()).collect());
    };
    names
        .iter()
        .map(|name| {
            table
                .column(name)
                .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                    column: name.clone(),
                    relation: table.name.clone(),
                })
        })
        .collect()
}

/// Writes one row and its index entries, checking every uniqueness constraint on the way.
fn write_row(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
    written: &mut Written,
) -> Result<()> {
    let tenant = executor.tenant;
    let primary_key: Vec<Datum> = table
        .primary_key
        .iter()
        .map(|&ordinal| row[ordinal].clone())
        .collect();
    let key = row::row_key(tenant, table.id, &primary_key)?;

    // The primary key is a unique index whose entry is the row itself.
    let detail = render_key(table, &table.primary_key, &primary_key);
    if txn.get(&key)?.is_some() {
        return Err(SqlError::UniqueViolation {
            constraint: table.primary_key_name.clone(),
            key: Some(detail.clone()),
        });
    }
    written.unique_keys.push(Unique {
        key: key.clone(),
        constraint: table.primary_key_name.clone(),
        detail,
    });

    for index in &table.indexes {
        let columns: Vec<Datum> = index
            .columns
            .iter()
            .map(|&ordinal| row[ordinal].clone())
            .collect();
        // A unique index leaves the primary key off, which is what makes a duplicate a collision
        // on one key -- unless a column is NULL, because PostgreSQL admits any number of NULLs in
        // a `UNIQUE` column and those entries need the suffix to stay apart (`crate::row`).
        let by_value = index.unique && row::unique_index_key_is_unique_by_value(&columns);
        let suffix = if by_value {
            None
        } else {
            Some(primary_key.as_slice())
        };
        let index_key = row::index_key(tenant, table.id, index.id, &columns, suffix)?;

        if by_value {
            let detail = render_key(table, &index.columns, &columns);
            if txn.get(&index_key)?.is_some() {
                return Err(SqlError::UniqueViolation {
                    constraint: index.name.clone(),
                    key: Some(detail),
                });
            }
            written.unique_keys.push(Unique {
                key: index_key.clone(),
                constraint: index.name.clone(),
                detail,
            });
        }
        // The value is the primary key, which is what an index lookup follows back to the row.
        txn.put(
            &index_key,
            &row::encode_row(&table.primary_key_types(), &primary_key)?,
        );
    }

    txn.put(&key, &row::encode_row(&table.column_types(), row)?);
    Ok(())
}

/// `Key (a, b)=(1, x)`, PostgreSQL's `DETAIL` for a uniqueness failure.
///
/// Nothing is quoted or escaped, which is PostgreSQL's own behaviour and not a shortcut: a text
/// value containing `, y)` really does come back as `Key (a, b)=(1, x, y))`. Copied exactly.
fn render_key(table: &TableDef, ordinals: &[usize], values: &[Datum]) -> String {
    let names: Vec<&str> = ordinals
        .iter()
        .map(|&ordinal| table.columns[ordinal].name.as_str())
        .collect();
    format!("Key ({})=({})", names.join(", "), render_values(values))
}

/// Values joined with `, `, a NULL written `null`, nothing quoted.
fn render_values(values: &[Datum]) -> String {
    values
        .iter()
        .map(|value| value.to_text().unwrap_or_else(|| "null".to_owned()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `UPDATE`: read the rows that match, rewrite them, and keep every index in step.
///
/// # Why the rows are read before any of them is written
///
/// The scan and the writes go through the same transaction, and the buffer is merged into a scan
/// (`crate::backend`), so a row whose primary key this statement *moves* could be met again
/// further along the scan and updated twice. That is the Halloween problem, and materialising the
/// matching rows first is the cheap way out of it: what the statement writes can no longer change
/// what it is about to read.
pub(super) fn update(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    update: &Update,
    written: &mut Written,
) -> Result<Outcome> {
    let table = executor.require_table(txn, &update.table)?;

    // Resolve the target of every assignment first, so `SET nope = 1` fails before anything is
    // read rather than after some rows have been rewritten.
    let assignments = update
        .assignments
        .iter()
        .map(|(name, value)| {
            let ordinal =
                table
                    .column(name)
                    .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                        column: name.clone(),
                        relation: table.name.clone(),
                    })?;
            Ok((ordinal, value.clone()))
        })
        .collect::<Result<Vec<_>>>()?;

    let rows = collect(executor, txn, update.filter.as_ref(), &table)?;
    let mut count = 0;
    for old in rows {
        let mut new = old.clone();
        for (ordinal, value) in &assignments {
            let column = &table.columns[*ordinal];
            // Evaluated against the row as it was, so `SET a = b, b = a` swaps them.
            let evaluated = match value {
                crate::plan::Expr::Literal(literal) => literal.assign(column.ty, &column.name)?,
                other => {
                    let resolved = query::resolve_against(other, &table)?;
                    cursor::evaluate(&resolved, &old)?
                }
            };
            if !evaluated.fits(column.ty) {
                return Err(SqlError::DatatypeMismatchInColumn {
                    column: column.name.clone(),
                    column_type: column.ty.name(),
                    expression_type: "the expression's",
                });
            }
            new[*ordinal] = evaluated;
        }
        check_not_null(&table, &new)?;
        remove_row(executor, txn, &table, &old)?;
        write_row(executor, txn, &table, &new, written)?;
        count += 1;
    }
    Ok(Outcome::done(format!("UPDATE {count}")))
}

/// `DELETE`: the row and every index entry that points at it.
pub(super) fn delete(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    delete: &Delete,
) -> Result<Outcome> {
    let table = executor.require_table(txn, &delete.table)?;
    let rows = collect(executor, txn, delete.filter.as_ref(), &table)?;
    let count = rows.len();
    for row in rows {
        remove_row(executor, txn, &table, &row)?;
    }
    Ok(Outcome::done(format!("DELETE {count}")))
}

/// Every row a predicate matches, read before anything is written. See [`update`] for why.
fn collect(
    executor: &Executor,
    txn: &dyn Txn,
    filter: Option<&crate::plan::Expr>,
    table: &TableDef,
) -> Result<Vec<Vec<Datum>>> {
    let node = query::matching_rows(filter, executor.tenant, table)?;
    let mut cursor = cursor::Cursor::open(txn, executor.tenant, &node)?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next()? {
        rows.push(row);
    }
    Ok(rows)
}

fn check_not_null(table: &TableDef, row: &[Datum]) -> Result<()> {
    for (ordinal, column) in table.columns.iter().enumerate() {
        if column.not_null && matches!(row[ordinal], Datum::Null) {
            return Err(SqlError::NotNullViolationInRelation {
                column: column.name.clone(),
                relation: table.name.clone(),
                row: Some(render_values(row)),
            });
        }
    }
    Ok(())
}

/// Deletes a row and every index entry built from it.
fn remove_row(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
) -> Result<()> {
    let tenant = executor.tenant;
    let primary_key: Vec<Datum> = table
        .primary_key
        .iter()
        .map(|&ordinal| row[ordinal].clone())
        .collect();

    for index in &table.indexes {
        let columns: Vec<Datum> = index
            .columns
            .iter()
            .map(|&ordinal| row[ordinal].clone())
            .collect();
        let by_value = index.unique && row::unique_index_key_is_unique_by_value(&columns);
        let suffix = if by_value {
            None
        } else {
            Some(primary_key.as_slice())
        };
        txn.delete(&row::index_key(
            tenant, table.id, index.id, &columns, suffix,
        )?);
    }
    txn.delete(&row::row_key(tenant, table.id, &primary_key)?);
    Ok(())
}
