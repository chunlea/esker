//! `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`.
//!
//! Each one is a read of the catalog, a check, and a write — all inside the caller's transaction,
//! so DDL is atomic with whatever else that transaction did and invisible until it commits.
//!
//! # `ALTER TABLE ADD COLUMN` rewrites nothing
//!
//! Appending a nullable column is a catalog write and nothing else. The rows already stored say
//! how many columns they hold ([ADR 0019](../../../docs/adr/0019-a-row-says-how-many-columns-it-has.md)),
//! so a reader pads the new column to NULL, which is what PostgreSQL shows for it anyway. That is
//! the whole feature. A **constant** `DEFAULT` is admitted on the same argument, one step further
//! on: the value is stored on the column as its *missing value* and the decoder pads with that
//! instead of with NULL, which is PostgreSQL 11's `attmissingval` and is why `ADD COLUMN ... NOT
//! NULL DEFAULT 7` is instant there too. What stays refused is what would need a row rewritten —
//! a **volatile** default like `random()`, whose value differs per row and so cannot be one
//! constant in the catalog, and bare `NOT NULL` with no default, which has no value to pad with.
//!
//! # A table with no primary key gets a hidden one
//!
//! The row key *is* the primary key (`crate::row`), so a table without one would have no key space
//! to live in. PostgreSQL allows such a table and gives its rows an identity of its own, so this
//! does too: a column the user cannot name, holding an `int8` from a per-table sequence, at
//! position 0 with the primary key pointing at it. Every layer below is then unchanged — the row
//! key, the index suffix, `UPDATE` and `DELETE` all work on a primary key like any other.
//!
//! **The divergence from PostgreSQL is honest and worth stating.** PostgreSQL's `ctid` is a
//! *physical* address, `(block, offset)`, and it moves whenever the row is rewritten — an `UPDATE`
//! changes it, and `VACUUM FULL` changes every one of them. Ours is a *logical* identity that
//! never changes for the life of the row, because the row key is what an index entry points at and
//! a moving key would mean rewriting every index on every update. So: `ctid` is not implemented and
//! is `0A000` (there is no block and no offset to report), the hidden column cannot be selected
//! under any spelling, and the only user-visible consequence of its existence is that a table
//! without a key can now be created, written and read.

use crate::backend::Txn;
use crate::catalog::{self, CheckDef, ColumnDef, IndexDef, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::Executor;
use crate::pgwire::session::Outcome;
use crate::plan::{
    self, AlterTable, AlterTableAction, CreateIndex, CreateTable, DropIndex, DropTable,
};
use crate::value::Datum;

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

    let columns = declared_columns(create)?;

    let key_position = |name: &String| {
        columns
            .iter()
            .position(|column| &column.name == name)
            .ok_or_else(|| SqlError::UndefinedColumnInKey(name.clone()))
    };

    // No declared key: the table gets an internal row id at position 0, and the primary key
    // points at it. Position 0 rather than the end so that a later `ALTER TABLE ADD COLUMN`
    // appends after the user's columns and cannot move it.
    let (columns, primary_key, primary_key_name) = if create.primary_key.is_empty() {
        let mut with_row_id = Vec::with_capacity(columns.len() + 1);
        with_row_id.push(ColumnDef {
            name: catalog::INTERNAL_ROW_ID_NAME.to_owned(),
            ty: crate::value::ColumnType::Int8,
            typmod: crate::value::NO_TYPMOD,
            // The executor fills it, so it has no default of either kind.
            default_now: false,
            not_null: true,
            // The executor fills it on every insert, so it has neither.
            default: None,
            missing: None,
        });
        with_row_id.extend(columns);
        (with_row_id, vec![0], String::new())
    } else {
        let primary_key = create
            .primary_key
            .iter()
            .map(key_position)
            .collect::<Result<Vec<_>>>()?;
        let name = create
            .primary_key_name
            .clone()
            .unwrap_or_else(|| plan::primary_key_name(&create.name));
        (columns, primary_key, name)
    };

    let key_position = |name: &String| {
        columns
            .iter()
            .position(|column: &ColumnDef| &column.name == name)
            .ok_or_else(|| SqlError::UndefinedColumnInKey(name.clone()))
    };
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
            // Born public. Nothing predates a `UNIQUE` declared with the table, so there is no
            // interleaving for the states to protect: the ADR's whole argument is about rows and
            // writers that already exist (`docs/plans/phase-6e.md` §2).
            state: catalog::SchemaState::Public,
            state_since: 1,
            predicate: None,
        });
    }

    let table_id = catalog::allocate_id(txn, executor.tenant)?;
    let sequences = sequences_for(executor, txn, create, table_id)?;
    let table = TableDef {
        id: table_id,
        name: create.name.clone(),
        columns,
        primary_key,
        indexes,
        checks: create.checks.clone(),
        primary_key_name,
        // A table starts at schema version 1; `ALTER TABLE ADD COLUMN` moves it.
        schema_version: 1,
        sequences,
    };

    validate_checks(&table)?;
    catalog::create_table(txn, executor.tenant, &table)?;
    for sequence in &table.sequences {
        catalog::create_sequence(txn, executor.tenant, sequence)?;
    }
    Ok(Outcome::done("CREATE TABLE"))
}

/// One sequence per `bigserial` or identity column, named the way a real server names it and
/// taking that name in the same namespace tables and indexes share — `CREATE TABLE t_id_seq` after
/// a `bigserial` is `42P07` on both servers.
/// The table's columns, as the catalog holds them, refusing a name written twice.
fn declared_columns(create: &CreateTable) -> Result<Vec<ColumnDef>> {
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
            typmod: column.typmod,
            default_now: column.default_now,
            // A primary key column is NOT NULL whether or not it said so, which is PostgreSQL's
            // rule and also ours by necessity: a NULL cannot be part of a row key.
            not_null: column.not_null || create.primary_key.contains(&column.name),
            default: column.default.clone(),
            // **No missing value at `CREATE TABLE`**, whatever the default is. Nothing predates a
            // column the table was created with, so there is no narrower row for a pad to answer
            // for — and writing one would be a claim about rows that cannot exist.
            missing: None,
        });
    }
    Ok(columns)
}

/// A `CHECK` added after the fact.
///
/// It bumps the schema version like any other change to the table definition, because every
/// node's cached `TableDef` has to learn it: a node still holding the old one would accept rows
/// the constraint forbids.
///
/// **It is not validated against the rows already there.** PostgreSQL does validate. A backfill
/// scan is the schema-change machinery of ADR 0020 and an `ADD CONSTRAINT` does not go through it
/// yet, so this is a known gap rather than a decision — `tests/check_constraint.rs` pins what the
/// node actually does and says why.
fn add_check(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    updated: &mut TableDef,
    check: &CheckDef,
) -> Result<()> {
    if updated.checks.iter().any(|seen| seen.name == check.name) {
        return Err(SqlError::DuplicateConstraint {
            constraint: check.name.clone(),
            relation: updated.name.clone(),
        });
    }
    updated.checks.push(check.clone());
    validate_checks(updated)?;
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, table, updated)
}

/// Every `CHECK` on `table`, resolved against its own columns — **now**, not on the first row.
///
/// A predicate is stored as text and lowered when a row is written, so nothing else would notice
/// that it names a column the table does not have until the first `INSERT`, and then as an
/// internal error rather than the `42703` a real server gives at `CREATE TABLE`. Accepting one
/// would create a table carrying a constraint that can never be evaluated. This is where the
/// stored text stops being trusted.
fn validate_checks(table: &TableDef) -> Result<()> {
    for check in &table.checks {
        let parsed = crate::parse::parse_predicate(&check.expr)?;
        let scope = crate::exec::query::Scope::single(table);
        crate::exec::query::resolve(&parsed, &scope)?;
    }
    Ok(())
}

fn sequences_for(
    executor: &Executor,
    txn: &mut dyn Txn,
    create: &CreateTable,
    table_id: u64,
) -> Result<Vec<catalog::SequenceDef>> {
    let mut sequences = Vec::new();
    for (ordinal, column) in create.columns.iter().enumerate() {
        if let Some(identity) = column.sequence {
            sequences.push(catalog::SequenceDef {
                id: catalog::allocate_id(txn, executor.tenant)?,
                name: plan::sequence_name(&create.name, &column.name),
                table_id,
                column: ordinal,
                identity,
            });
        }
    }
    Ok(sequences)
}

pub(super) fn drop_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &DropTable,
) -> Result<Outcome> {
    for name in &drop.names {
        // `42501`, and `IF EXISTS` does not excuse it — measured, both spellings.
        catalog::pg_catalog::refuse_write(name)?;
        let table = match existing_relation(executor, txn, name)? {
            Some(catalog::Relation::Table { table_id }) => executor.table_by_id(txn, table_id)?,
            // A name that is there but is an index is *not* "does not exist": PostgreSQL says
            // `"t9_pkey" is not a table` with `42809`, and `IF EXISTS` does not excuse it either.
            // Captured, because collapsing the two would send a user looking for a missing index.
            Some(catalog::Relation::Index { .. } | catalog::Relation::PrimaryKey { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a table",
                    found: "DROP INDEX",
                });
            }
            Some(catalog::Relation::Sequence { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a table",
                    found: "DROP SEQUENCE",
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
        //
        // Paged, because a whole range cannot be asked for in one call ([`super::for_each_page`]).
        let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
        super::for_each_page(txn, &start, &end, |txn, page| {
            for (key, _) in page {
                txn.delete(key);
            }
            Ok(())
        })?;
        for index in &table.indexes {
            let (start, end) = crate::row::index_range(executor.tenant, table.id, index.id);
            super::for_each_page(txn, &start, &end, |txn, page| {
                for (key, _) in page {
                    txn.delete(key);
                }
                Ok(())
            })?;
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
    catalog::pg_catalog::refuse_write(&create.table)?;
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

    // A predicate naming a column the table does not have is `42703` here, not an internal error
    // at the first write — the same rule, and the same reason, as a `CHECK`'s.
    if let Some(predicate) = &create.predicate {
        let parsed = crate::parse::parse_predicate(predicate)?;
        let scope = crate::exec::query::Scope::single(&table);
        crate::exec::query::resolve(&parsed, &scope)?;
    }
    let index = IndexDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name,
        unique: create.unique,
        columns,
        predicate: create.predicate.clone(),
        // Public the moment it is declared, because it is built inside this statement's own
        // transaction: no other node ever sees it half-made. That is what makes the plain form
        // correct and also what makes it `TODO(post-v1)` for a table large enough to matter — the
        // whole backfill is one transaction. `CONCURRENTLY` is the staged one, below.
        state: if create.concurrently {
            catalog::SchemaState::Absent
        } else {
            catalog::SchemaState::Public
        },
        state_since: table.schema_version,
    };

    if create.concurrently {
        // Declared at `absent` and built by the job: no backfill here, and nothing reads it until
        // the job has taken it all the way to `public`.
        let mut updated = (*table).clone();
        updated.schema_version += 1;
        let index_id = index.id;
        updated.indexes.push(index);
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
        catalog::put_job(
            txn,
            executor.tenant,
            &catalog::JobRecord {
                index_id,
                table_id: updated.id,
                cursor: Vec::new(),
                done: false,
                removing: false,
            },
        );
        // The statement returns as soon as the job exists, which is what `CONCURRENTLY` means:
        // `esker_schema_jobs()` is where a human watches the states advance.
        return Ok(Outcome::done("CREATE INDEX"));
    }
    // An index over a table that already has rows has to be *built*, not just declared. An index
    // that exists and is empty is worse than no index: the planner will use it, and it will answer
    // every lookup with no rows. The tests caught exactly that.
    backfill(executor, txn, &table, &index)?;

    let mut updated = (*table).clone();
    updated.indexes.push(index);
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
                    found: "DROP TABLE",
                });
            }
            Some(catalog::Relation::Sequence { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "an index",
                    found: "DROP SEQUENCE",
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

        if drop.concurrently {
            // The **removal direction**: the index stays where it is and a job walks it backwards,
            // `public → write-only → delete-only → absent`, before anything is removed. Dropping it
            // here instead would take a node from `public` to gone in one step, and a node still at
            // `public` reads an index a node at `absent` has stopped maintaining — ADR 0020's
            // anomalies, read backwards.
            catalog::put_job(
                txn,
                executor.tenant,
                &catalog::JobRecord {
                    index_id,
                    table_id,
                    cursor: Vec::new(),
                    // A removal has no backfill: there is nothing to build, only entries to stop
                    // writing and then to take away.
                    done: true,
                    removing: true,
                },
            );
            continue;
        }

        let (start, end) = crate::row::index_range(executor.tenant, table_id, index_id);
        super::for_each_page(txn, &start, &end, |txn, page| {
            for (key, _) in page {
                txn.delete(key);
            }
            Ok(())
        })?;
        let mut updated = (*table).clone();
        updated.indexes.retain(|index| index.id != index_id);
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    }
    Ok(Outcome::done("DROP INDEX"))
}

/// `ALTER TABLE ... ADD COLUMN`, the only action this crate executes.
///
/// Every action in one statement is applied to one copy of the definition and written once, which
/// is what makes `ALTER TABLE t ADD COLUMN a text, ADD COLUMN b text` atomic the way PostgreSQL's
/// is. A statement whose actions all skip writes nothing at all: it has changed no shape, and
/// bumping the catalog version for it would make every node discard its cache and every concurrent
/// DDL conflict, for nothing.
pub(super) fn alter_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    alter: &AlterTable,
) -> Result<Outcome> {
    let done = Ok(Outcome::done("ALTER TABLE"));
    catalog::pg_catalog::refuse_write(&alter.name)?;
    let table = match existing_relation(executor, txn, &alter.name)? {
        Some(catalog::Relation::Table { table_id }) => executor.table_by_id(txn, table_id)?,
        // An index is a relation, so PostgreSQL does not say "is not a table" here -- it says the
        // action cannot be performed on it, and adds that indexes do not take one. Captured.
        Some(catalog::Relation::Index { .. } | catalog::Relation::PrimaryKey { .. }) => {
            return Err(SqlError::AlterActionOnWrongObject {
                action: "ADD COLUMN",
                name: alter.name.clone(),
                kind: "indexes",
            });
        }
        Some(catalog::Relation::Sequence { .. }) => {
            return Err(SqlError::AlterActionOnWrongObject {
                action: "ADD COLUMN",
                name: alter.name.clone(),
                kind: "sequences",
            });
        }
        None => {
            if alter.if_exists {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "relation",
                    name: alter.name.clone(),
                });
                return done;
            }
            return Err(SqlError::UndefinedTable(alter.name.clone()));
        }
    };

    let mut updated = (*table).clone();
    let mut changed = false;
    for action in &alter.actions {
        if let AlterTableAction::AddCheck(check) = action {
            add_check(txn, executor, &table, &mut updated, check)?;
            continue;
        }
        let AlterTableAction::AddColumn {
            column,
            if_not_exists,
        } = action
        else {
            // Retention is not part of the table definition and deliberately does **not** bump the
            // schema version (`docs/adr/0021-time-machine.md` Decision 4). It changes nothing
            // about how a row is written or read, and bumping would make every node in the cluster
            // discard its table cache to learn a number none of them uses. The collector picks it
            // up on its next pass, which is the only place it means anything.
            // The columnar setting is not part of the table definition either, and for the same
            // reason: it says how many *copies* the cluster keeps, not how a row is written or
            // read. The placement driver acts on it; no reader of rows does, so no node's cached
            // `TableDef` is stale because of it.
            if let AlterTableAction::SetColumnarReplicas { replicas } = action {
                match replicas {
                    // The row schema is published in the same write, because a store that holds
                    // a learner needs it to decode a row and cannot ask this crate for it.
                    Some(replicas) => catalog::set_table_columnar_replicas(
                        txn,
                        executor.tenant,
                        &updated,
                        *replicas,
                    )?,
                    None => {
                        catalog::clear_table_columnar_replicas(txn, executor.tenant, table.id);
                    }
                }
                // PD acts on this and **cannot read it**: the setting is in the cluster's own key
                // space and PD links neither this crate nor a client (ADR 0022 Decision 5). So the
                // node that ran the `ALTER` tells it — after the commit, from the executor, as a
                // full assertion of every wish rather than this one's delta.
                executor.columnar_changed();
                continue;
            }
            let AlterTableAction::SetRetention { retention_ms } = action else {
                unreachable!("every action is one of the three")
            };
            match retention_ms {
                Some(retention_ms) => {
                    catalog::set_table_retention(txn, executor.tenant, table.id, *retention_ms);
                }
                // `DEFAULT` deletes the override rather than storing a zero: a table with no
                // override is not "retention zero", it is the cluster default, and the difference
                // is an absent key against a key holding zero.
                None => catalog::clear_table_retention(txn, executor.tenant, table.id),
            }
            continue;
        };
        if updated.column(&column.name).is_some() {
            if *if_not_exists {
                executor.notice(SqlError::DuplicateColumnSkipping {
                    column: column.name.clone(),
                    relation: alter.name.clone(),
                });
                continue;
            }
            return Err(SqlError::DuplicateColumnInRelation {
                column: column.name.clone(),
                relation: alter.name.clone(),
            });
        }
        updated.columns.push(ColumnDef {
            name: column.name.clone(),
            ty: column.ty,
            typmod: column.typmod,
            default_now: column.default_now,
            // `NOT NULL` is admissible **only with a constant default**, which is what makes every
            // row already stored hold a value: the missing value below is that value, and the
            // decoder pads with it. Without one the lowering refuses `NOT NULL`, because the
            // alternative is a rewrite and this `ALTER` touches no row.
            not_null: column.not_null,
            default: column.default.clone(),
            // **The missing value is frozen here**, at `ADD COLUMN` time, and a later
            // `ALTER COLUMN SET DEFAULT` must not touch it. Measured on PostgreSQL 19beta1: after
            // `SET DEFAULT 'new'`, rows that predate the column still read `old`
            // (`docs/plans/phase-6e.md` §5 unit 1). One field for both would rewrite history the
            // first time somebody changed a default.
            missing: column.default.clone(),
        });
        changed = true;
    }
    if !changed {
        return done;
    }

    // One statement, one shape change, whatever the number of columns it added: they become
    // visible together.
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    // And the published schema, if this table has one, in the SAME transaction. That is the
    // ordering guarantee a columnar learner gets: a row written after this `ALTER` cannot reach a
    // store before the schema that decodes it, because the two commit together. A learner given
    // a stale schema does not read the row wrongly — `decode_row` refuses a row wider than its
    // schema — but it stops applying, and by ADR 0022's constraint it may not fetch, so it would
    // sit behind until somebody noticed. A no-op for a table nobody wants a columnar copy of.
    catalog::refresh_published_schema(txn, executor.tenant, &updated)?;
    done
}

fn existing_relation(
    executor: &Executor,
    txn: &dyn Txn,
    name: &str,
) -> Result<Option<catalog::Relation>> {
    // A `pg_catalog` relation is a relation, and every verb that asks this question should see one
    // — so a `DROP INDEX pg_type` is `42809 "pg_type" is not an index` and a `CREATE TABLE
    // pg_type` is `42P07`, which is what a real server answers for the qualified spelling.
    // Refusing here instead would answer `42501` for both, and only one of them is a write.
    if let Some(view) = catalog::pg_catalog::view(name) {
        return Ok(Some(catalog::Relation::Table {
            table_id: view.table_def().id,
        }));
    }
    executor.catalog_view(txn)?.relation(name)
}

/// Writes an index entry for every row the table already has.
///
/// The whole table is read to do it, in the transaction the statement runs in, so a `CREATE INDEX`
/// on a table someone is writing to conflicts and one of the two retries. `TODO(post-v1)`: a
/// concurrent build, which is what `CREATE INDEX CONCURRENTLY` is for and which needs a background
/// job phase 6a does not have — the clause is refused by name until then.
///
/// A `UNIQUE` index built over rows that already violate it fails here, with the same `23505` an
/// `INSERT` would have raised, which is what PostgreSQL does too.
///
/// The scan is **paged** ([`super::for_each_page`]), which is not a detail: a `limit` of 0 means
/// "everything" to [`crate::backend::Txn`] and a page to the real `TxnClient`, and an index built
/// from one page of a larger table is an index that makes a query return *fewer* rows than the
/// same query without it. That is the wrong-answer shape, reached through a statement that
/// succeeded.
fn backfill(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    index: &IndexDef,
) -> Result<()> {
    let tenant = executor.tenant;
    let (start, end) = crate::row::table_row_range(tenant, table.id);
    let schema = table.row_schema();
    let primary_key_types = table.primary_key_types();

    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (_, value) in page {
            let row = crate::row::decode_row(&schema, value)?;
            let columns: Vec<Datum> = index
                .columns
                .iter()
                .map(|&ordinal| row[ordinal].clone())
                .collect();
            let primary_key: Vec<Datum> = table
                .primary_key
                .iter()
                .map(|&ordinal| row[ordinal].clone())
                .collect();
            let by_value =
                index.unique && crate::row::unique_index_key_is_unique_by_value(&columns);
            let suffix = if by_value {
                None
            } else {
                Some(primary_key.as_slice())
            };
            let key = crate::row::index_key(tenant, table.id, index.id, &columns, suffix)?;
            if by_value && entries.iter().any(|(existing, _)| existing == &key) {
                // Out of the walk as well as out of the page: the index cannot be built and
                // reading the rest of the table would learn nothing.
                return Err(SqlError::UniqueViolation {
                    constraint: index.name.clone(),
                    key: None,
                });
            }
            entries.push((
                key,
                crate::row::encode_row(&primary_key_types, &primary_key)?,
            ));
        }
        Ok(())
    })?;

    for (key, value) in entries {
        txn.put(&key, &value);
    }
    Ok(())
}
