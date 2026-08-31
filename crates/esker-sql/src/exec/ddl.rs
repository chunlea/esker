//! `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`.
//!
//! Each one is a read of the catalog, a check, and a write — all inside the caller's transaction,
//! so DDL is atomic with whatever else that transaction did and invisible until it commits.
//!
//! # A table needs a primary key
//!
//! The row key *is* the primary key (`crate::row`), so a table without one has no key space to
//! live in. PostgreSQL allows such a table and gives its rows a hidden identity; we do not, and
//! contract C2 says what to do about a statement PostgreSQL accepts and we cannot run — `0A000`,
//! naming it. `TODO(post-v1)`: an implicit row id from a per-table sequence, which is how this is
//! usually closed, and which needs a sequence phase 6a does not have.

use crate::backend::Txn;
use crate::catalog::{self, ColumnDef, IndexDef, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::Executor;
use crate::pgwire::session::Outcome;
use crate::plan::{self, CreateIndex, CreateTable, DropIndex, DropTable};

pub(super) fn create_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateTable,
) -> Result<Outcome> {
    if let Some(existing) = existing_relation(executor, txn, &create.name)? {
        let _ = existing;
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(create.name.clone()));
            return Ok(Outcome::done("CREATE TABLE"));
        }
        return Err(SqlError::DuplicateTable(create.name.clone()));
    }

    let mut columns = Vec::with_capacity(create.columns.len());
    for column in &create.columns {
        if columns
            .iter()
            .any(|kept: &ColumnDef| kept.name == column.name)
        {
            return Err(SqlError::DuplicateColumn(column.name.clone()));
        }
        columns.push(ColumnDef {
            name: column.name.clone(),
            ty: column.ty,
            // A primary key column is NOT NULL whether or not it said so, which is PostgreSQL's
            // rule and also ours by necessity: a NULL cannot be part of a row key.
            not_null: column.not_null || create.primary_key.contains(&column.name),
        });
    }

    let key_position = |name: &String| {
        columns
            .iter()
            .position(|column| &column.name == name)
            .ok_or_else(|| SqlError::UndefinedColumnInKey(name.clone()))
    };

    if create.primary_key.is_empty() {
        return Err(SqlError::unsupported(format!(
            "a table without a PRIMARY KEY (\"{}\")",
            create.name
        )));
    }
    let primary_key = create
        .primary_key
        .iter()
        .map(key_position)
        .collect::<Result<Vec<_>>>()?;

    let mut indexes = Vec::with_capacity(create.unique.len());
    for constraint in &create.unique {
        let ordinals = constraint
            .columns
            .iter()
            .map(key_position)
            .collect::<Result<Vec<_>>>()?;
        indexes.push(IndexDef {
            id: catalog::allocate_id(txn, executor.tenant)?,
            name: constraint
                .name
                .clone()
                .unwrap_or_else(|| plan::unique_constraint_name(&create.name, &constraint.columns)),
            unique: true,
            columns: ordinals,
        });
    }

    let table = TableDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name: create.name.clone(),
        columns,
        primary_key,
        indexes,
        primary_key_name: create
            .primary_key_name
            .clone()
            .unwrap_or_else(|| plan::primary_key_name(&create.name)),
    };
    catalog::create_table(txn, executor.tenant, &table)?;
    Ok(Outcome::done("CREATE TABLE"))
}

pub(super) fn drop_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &DropTable,
) -> Result<Outcome> {
    for name in &drop.names {
        let table = match existing_relation(executor, txn, name)? {
            Some(catalog::Relation::Table { table_id }) => executor.table_by_id(txn, table_id)?,
            // A name that is there but is an index is *not* "does not exist": PostgreSQL says
            // `"t9_pkey" is not a table` with `42809`, and `IF EXISTS` does not excuse it either.
            // Captured, because collapsing the two would send a user looking for a missing index.
            Some(catalog::Relation::Index { .. } | catalog::Relation::PrimaryKey { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a table",
                });
            }
            None => {
                if drop.if_exists {
                    executor.notice(SqlError::DoesNotExistSkipping {
                        kind: "table",
                        name: name.clone(),
                    });
                    continue;
                }
                return Err(SqlError::UndefinedTableForDrop(name.clone()));
            }
        };
        // The rows go with the table. A range delete is what this wants and the transaction layer
        // has none, so every key is deleted individually -- correct, and `TODO(post-v1)` for a
        // table large enough that this is a problem.
        let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
        for (key, _) in txn.scan(&start, &end, 0)? {
            txn.delete(&key);
        }
        for index in &table.indexes {
            let (start, end) = crate::row::index_range(executor.tenant, table.id, index.id);
            for (key, _) in txn.scan(&start, &end, 0)? {
                txn.delete(&key);
            }
        }
        catalog::drop_table(txn, executor.tenant, &table)?;
    }
    Ok(Outcome::done("DROP TABLE"))
}

pub(super) fn create_index(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateIndex,
) -> Result<Outcome> {
    let table = executor.require_table(txn, &create.table)?;
    // A name the user chose is theirs, and a collision with it is a `42P07`.
    let name = if let Some(given) = &create.name {
        if existing_relation(executor, txn, given)?.is_some() {
            if create.if_not_exists {
                executor.notice(SqlError::AlreadyExistsSkipping(given.clone()));
                return Ok(Outcome::done("CREATE INDEX"));
            }
            return Err(SqlError::DuplicateTable(given.clone()));
        }
        given.clone()
    } else {
        // A name *we* derived is disambiguated instead: `CREATE INDEX ON t (a)` twice gives
        // `t_a_idx` and `t_a_idx1`, not an error. Measured against a real server, which produced
        // `..._idx`, `..._idx1` and `..._idx2` for three. The user named nothing, so there is
        // nothing of theirs to collide with.
        let derived = plan::index_name(&create.table, &create.columns);
        let mut name = derived.clone();
        let mut suffix = 0u32;
        while existing_relation(executor, txn, &name)?.is_some() {
            suffix += 1;
            name = format!("{derived}{suffix}");
        }
        name
    };

    let columns = create
        .columns
        .iter()
        .map(|column| {
            table
                .column(column)
                .ok_or_else(|| SqlError::UndefinedColumn(column.clone()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut updated = (*table).clone();
    updated.indexes.push(IndexDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name,
        unique: create.unique,
        columns,
    });
    // Building the index over rows that already exist is the executor's job once there are rows to
    // build it from; until `INSERT` lands there are none.
    // TODO(unit-6b): backfill the index from the table's existing rows.
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    Ok(Outcome::done("CREATE INDEX"))
}

pub(super) fn drop_index(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &DropIndex,
) -> Result<Outcome> {
    for name in &drop.names {
        let (table_id, index_id) = match existing_relation(executor, txn, name)? {
            Some(catalog::Relation::Index { table_id, index_id }) => (table_id, index_id),
            Some(catalog::Relation::Table { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "an index",
                });
            }
            // The primary key has no index to drop, and PostgreSQL refuses to drop the one it does
            // have for the same reason: the constraint needs it.
            Some(catalog::Relation::PrimaryKey { table_id }) => {
                let table = executor.table_by_id(txn, table_id)?;
                return Err(SqlError::DependentObjectsStillExist {
                    index: name.clone(),
                    table: table.name.clone(),
                });
            }
            None => {
                if drop.if_exists {
                    executor.notice(SqlError::DoesNotExistSkipping {
                        kind: "index",
                        name: name.clone(),
                    });
                    continue;
                }
                return Err(SqlError::UndefinedIndex(name.clone()));
            }
        };
        let table = executor.table_by_id(txn, table_id)?;
        let (start, end) = crate::row::index_range(executor.tenant, table_id, index_id);
        for (key, _) in txn.scan(&start, &end, 0)? {
            txn.delete(&key);
        }
        let mut updated = (*table).clone();
        updated.indexes.retain(|index| index.id != index_id);
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    }
    Ok(Outcome::done("DROP INDEX"))
}

fn existing_relation(
    executor: &Executor,
    txn: &dyn Txn,
    name: &str,
) -> Result<Option<catalog::Relation>> {
    executor.catalog_view(txn)?.relation(name)
}
