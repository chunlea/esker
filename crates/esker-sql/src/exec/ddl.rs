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

use std::fmt::Write as _;

use crate::backend::Txn;
use crate::catalog::{self, CheckDef, ColumnDef, IndexDef, IndexKey, KeyPart, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::Executor;
use crate::pgwire::session::Outcome;
use crate::plan::{
    self, AlterTable, AlterTableAction, CreateIndex, CreateTable, DropIndex, DropTable,
};
use crate::value::{ColumnType, Datum, PgDatum as _, PgType as _};

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
            ty: ColumnType::Int8,
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

    let indexes = unique_indexes(executor, txn, create, &columns)?;

    let table_id = catalog::allocate_id(txn, executor.tenant)?;
    let sequences = sequences_for(executor, txn, create, table_id)?;
    let table = TableDef {
        id: table_id,
        name: create.name.clone(),
        columns,
        primary_key,
        indexes,
        checks: create.checks.clone(),
        // Filled below: resolving one needs the table it is on, which is this value. A new
        // table's checks are on; `DISABLE TRIGGER` is a statement of its own.
        foreign_keys: Vec::new(),
        triggers_disabled: false,
        primary_key_name,
        // A table starts at schema version 1; `ALTER TABLE ADD COLUMN` moves it.
        schema_version: 1,
        sequences,
    };

    validate_checks(&table)?;
    // Resolved against a table that is not in the catalog yet, which is what lets a
    // self-reference — `CREATE TABLE t (id int8 PRIMARY KEY, parent int8 REFERENCES t)` — work
    // in the statement that declares it.
    let mut table = table;
    let mut backrefs = Vec::new();
    for key in &create.foreign_keys {
        let resolved = resolve_foreign_key(txn, executor, &table, key)?;
        backrefs.push((resolved.parent, table.id));
        table.foreign_keys.push(resolved);
    }
    catalog::create_table(txn, executor.tenant, &table)?;
    for (parent, child) in backrefs {
        txn.put(
            &catalog::foreign_key_backref_key(executor.tenant, parent, child),
            &[],
        );
    }
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

/// `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (…) REFERENCES … (…)`.
///
/// Everything is resolved here and nothing is left for the first write to discover: the parent
/// must exist (`42P01`), the referencing columns must (`42703`, with a sentence of its own), and
/// the referenced columns must be **a key of the parent** (`42830`) — a primary key or a whole
/// unique index — because a reference to a column that can repeat has no single row to point at.
/// Each was measured; each would otherwise surface as an internal error at an `INSERT`.
///
/// The constraint is **not** validated against the rows already there. A real server does check
/// them, and this is the one place that difference shows: `ActiveRecord` writes every
/// `ADD CONSTRAINT` before it writes any rows, so nothing it does reaches the gap. Declared, and
/// the `TODO(post-v1)` is a scan of the child table with the parent lookup this module already
/// has.
fn add_foreign_key(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    updated: &mut TableDef,
    key: &plan::ForeignKey,
) -> Result<()> {
    if updated.checks.iter().any(|seen| seen.name == key.name)
        || updated
            .foreign_keys
            .iter()
            .any(|seen| seen.name == key.name)
    {
        return Err(SqlError::DuplicateConstraint {
            constraint: key.name.clone(),
            relation: updated.name.clone(),
        });
    }
    let resolved = resolve_foreign_key(txn, executor, updated, key)?;
    let parent_id = resolved.parent;
    // **Creation order, not name order.** `pg_constraint` sorts by name where it is read
    // (`catalog::pg_constraint::constraints_of`), and the one place the order in this list shows
    // is the `2BP01` a `DROP TABLE` gives: a real server names the *first* constraint that
    // depends on the table, which is the first one made.
    updated.foreign_keys.push(resolved);
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, table, updated)?;
    // The reverse direction, so a `DELETE` on the parent is a prefix scan rather than a scan of
    // every table this tenant has.
    txn.put(
        &catalog::foreign_key_backref_key(executor.tenant, parent_id, updated.id),
        &[],
    );
    Ok(())
}

/// The index behind each `UNIQUE` declared with the table.
///
/// Every one is born **public**. Nothing predates a `UNIQUE` declared with the table, so there is
/// no interleaving for the schema states to protect — the ADR's whole argument is about rows and
/// writers that already exist (`docs/plans/phase-6e.md` §2).
fn unique_indexes(
    executor: &Executor,
    txn: &mut dyn Txn,
    create: &CreateTable,
    columns: &[ColumnDef],
) -> Result<Vec<IndexDef>> {
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
            keys: ordinals.into_iter().map(IndexKey::column).collect(),
            // A `UNIQUE` constraint's grammar takes `NULLS NOT DISTINCT` on a real server; this
            // node refuses the clause there (`parse::lower`), so a constraint's index never has
            // it and the default is the truth rather than a placeholder.
            nulls_not_distinct: false,
            state: catalog::SchemaState::Public,
            state_since: 1,
            predicate: None,
        });
    }
    Ok(indexes)
}

/// `ALTER TABLE … ENABLE`/`DISABLE TRIGGER ALL`.
///
/// One flag on the table, written the way every other constraint change is written — and it
/// **does not bump the schema version**, for the reason retention does not: no row is written or
/// read differently because of it, so no node's cached row schema is stale. What it changes is
/// which checks run, and those are read from the table record each statement asks for.
///
/// `ENABLE` on a table that was never disabled is a write of the value it already has. That is
/// what a real server does too — `ALTER TABLE … ENABLE TRIGGER ALL` on an untouched table is a
/// plain success — and it is the shape `ActiveRecord` emits: `disable_referential_integrity`
/// re-enables every table it named, whether or not it changed any of them.
fn set_triggers_disabled(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    updated: &mut TableDef,
    disabled: bool,
) -> Result<()> {
    updated.triggers_disabled = disabled;
    catalog::replace_table(txn, executor.tenant, table, updated)
}

/// `ALTER TABLE … SET (columnar_replicas = n | DEFAULT)`.
///
/// Not part of the table definition and deliberately does **not** bump the schema version, for the
/// reason retention does not: it says how many *copies* the cluster keeps, not how a row is
/// written or read. The placement driver acts on it; no reader of rows does, so no node's cached
/// `TableDef` is stale because of it.
fn set_columnar_replicas(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    table: &TableDef,
    replicas: Option<u8>,
) -> Result<()> {
    match replicas {
        // The row schema is published in the same write, because a store that holds a learner
        // needs it to decode a row and cannot ask this crate for it.
        Some(replicas) => {
            catalog::set_table_columnar_replicas(txn, executor.tenant, table, replicas)?;
        }
        None => catalog::clear_table_columnar_replicas(txn, executor.tenant, table.id),
    }
    // PD acts on this and **cannot read it**: the setting is in the cluster's own key space and PD
    // links neither this crate nor a client (ADR 0022 Decision 5). So the node that ran the
    // `ALTER` tells it — after the commit, from the executor, as a full assertion of every wish
    // rather than this one's delta.
    executor.columnar_changed();
    Ok(())
}

/// One `FOREIGN KEY`, resolved against the table it is on and the table it points at.
///
/// The **self-reference** is why `child` is passed rather than read from the catalog: a table
/// declaring a constraint into itself is not in the catalog in that shape yet, whether it is being
/// created or altered.
fn resolve_foreign_key(
    txn: &dyn Txn,
    executor: &Executor,
    child: &TableDef,
    key: &plan::ForeignKey,
) -> Result<catalog::ForeignKeyDef> {
    let columns = key
        .columns
        .iter()
        .map(|column| {
            child
                .column(column)
                .ok_or_else(|| SqlError::UndefinedColumnInForeignKey(column.clone()))
        })
        .collect::<Result<Vec<_>>>()?;

    let parent = if key.parent == child.name {
        None
    } else {
        Some(executor.require_table(txn, &key.parent)?)
    };
    let parent_def: &TableDef = parent.as_deref().unwrap_or(child);

    let parent_columns = if key.parent_columns.is_empty() {
        // `REFERENCES t` with no list is the parent's **primary key**, which is what
        // `t.references :parrot, foreign_key: true` writes.
        parent_def.primary_key.clone()
    } else {
        key.parent_columns
            .iter()
            .map(|column| {
                parent_def
                    .column(column)
                    .ok_or_else(|| SqlError::UndefinedColumnInForeignKey(column.clone()))
            })
            .collect::<Result<Vec<_>>>()?
    };
    if !references_a_key(parent_def, &parent_columns) || columns.len() != parent_columns.len() {
        return Err(SqlError::NoUniqueConstraintForReference(
            parent_def.name.clone(),
        ));
    }
    Ok(catalog::ForeignKeyDef {
        name: key.name.clone(),
        columns,
        parent: parent_def.id,
        parent_columns,
        on_update: key.on_update,
        on_delete: key.on_delete,
        deferrable: key.deferrable,
    })
}

/// Whether these columns of `parent` are a key: its primary key, or a whole unique index's.
///
/// **In order and in full.** PostgreSQL matches the referenced columns against a unique
/// constraint as a set, and a prefix of a composite key is not one — a reference to the first
/// column of a two-column key would have several rows to point at, which is the whole condition
/// `42830` names. A **partial** unique index does not count either: it constrains only the rows
/// its predicate admits, so the column can repeat among the others.
fn references_a_key(parent: &TableDef, columns: &[usize]) -> bool {
    if parent.primary_key == columns {
        return true;
    }
    parent.indexes.iter().any(|index| {
        index.unique
            && index.state.readable()
            && index.predicate.is_none()
            && index.key_columns().as_deref() == Some(columns)
    })
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
        let parsed = crate::parse::parse_stored_expr(&check.expr)?;
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
        // **A table something references cannot be dropped**, and `2BP01` names the constraint
        // that stops it — unless `CASCADE`, which takes the constraint with the table instead.
        // `RESTRICT` is the default and the same thing as writing nothing; measured, both.
        //
        // What cascades is the **constraint**, not the child: a real server drops
        // `constraint tc_p_fkey on table tc` and leaves `tc` and every one of its rows exactly
        // where they were. Measured — the rows are still there afterwards and `pg_constraint` has
        // one fewer entry.
        //
        // A self-reference does not count in either direction: dropping the table takes its own
        // constraint with it, which is what a real server does too.
        let (start, end) = catalog::foreign_key_backref_range(executor.tenant, table.id);
        for (key, _) in txn.scan(&start, &end, 0)? {
            let child_id = catalog::foreign_key_backref_child(executor.tenant, table.id, &key)?;
            if child_id == table.id {
                continue;
            }
            let child = executor.table_by_id(txn, child_id)?;
            let Some(constraint) = child
                .foreign_keys
                .iter()
                .find(|constraint| constraint.parent == table.id)
            else {
                continue;
            };
            if !drop.cascade {
                return Err(SqlError::DependentTable {
                    relation: table.name.clone(),
                    detail: format!(
                        "constraint {} on table {} depends on table {}",
                        constraint.name, child.name, table.name
                    ),
                });
            }
            // Every constraint of this child that points here, not only the first: a child may
            // hold two (`CONSTRAINT c1 … REFERENCES p, CONSTRAINT c2 … REFERENCES p`), and
            // leaving the second behind would leave the child referring to a table that is gone.
            let mut without = (*child).clone();
            without
                .foreign_keys
                .retain(|constraint| constraint.parent != table.id);
            without.schema_version += 1;
            catalog::replace_table(txn, executor.tenant, &child, &without)?;
            txn.delete(&key);
        }
        // Its own back-references go with it: this table as a **child** is a key under every
        // parent it points at, and a parent that outlives it must not be told it is still
        // referenced.
        for constraint in &table.foreign_keys {
            txn.delete(&catalog::foreign_key_backref_key(
                executor.tenant,
                constraint.parent,
                table.id,
            ));
        }

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
        let derived = plan::index_name(&create.table, &create.keys);
        let mut name = derived.clone();
        let mut suffix = 0u32;
        while existing_relation(executor, txn, &name)?.is_some() {
            suffix += 1;
            name = format!("{derived}{suffix}");
        }
        name
    };

    let keys = create
        .keys
        .iter()
        .map(|key| {
            let part = match &key.part {
                plan::KeyPartName::Column(column) => table
                    .column(column)
                    .map(KeyPart::Column)
                    .ok_or_else(|| SqlError::UndefinedColumn(column.clone()))?,
                plan::KeyPartName::Expression { expr, shape } => {
                    let (expr, ty) = index_expression(&table, expr)?;
                    KeyPart::Expression {
                        expr,
                        shape: *shape,
                        ty,
                    }
                }
            };
            Ok(IndexKey {
                part,
                order: key.order,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // A predicate naming a column the table does not have is `42703` here, not an internal error
    // at the first write — the same rule, and the same reason, as a `CHECK`'s.
    if let Some(predicate) = &create.predicate {
        let parsed = crate::parse::parse_stored_expr(predicate)?;
        let scope = crate::exec::query::Scope::single(&table);
        crate::exec::query::resolve(&parsed, &scope)?;
    }
    let index = IndexDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name,
        unique: create.unique,
        keys,
        predicate: create.predicate.clone(),
        nulls_not_distinct: create.nulls_not_distinct,
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

/// Checks that an index expression can be an index key, and resolves it against the table.
///
/// Three refusals, each measured against PostgreSQL 19:
///
/// * a column the table does not have is **`42703`** — from `resolve`, the same rule and the same
///   reason as an index predicate's and a `CHECK`'s: at `CREATE INDEX`, not at the first write;
/// * an aggregate is **`42803 aggregate functions are not allowed in index expressions`**;
/// * anything whose value is not a function of the row alone is
///   **`42P17 functions in index expression must be marked IMMUTABLE`** — the code a real server
///   gives for `nextval`, and also for `format_type` and `pg_get_indexdef`, which are *stable*
///   there rather than volatile. Every function this crate has is in one of those two groups:
///   `lower` and `upper` are immutable, a sequence call writes, and a `pg_catalog` function reads
///   the catalog, which the index would then have to be rebuilt whenever anybody changed.
///
/// An index whose key is not a function of the row is not a slow index, it is a **wrong** one: the
/// entry is written from the value the expression had at insert and looked up from the value it
/// has at read, and nothing ever notices they differ.
fn index_expression(table: &TableDef, expr: &str) -> Result<(String, ColumnType)> {
    let parsed = crate::parse::parse_stored_expr(expr)?;
    let scope = crate::exec::query::Scope::single(table);
    let resolved = crate::exec::query::resolve(&parsed, &scope)?;
    refuse_unless_immutable(&resolved)?;
    let ty = crate::exec::query::expr_type(&resolved, &scope)?;
    // **A `CASE` is stored deparsed and every other expression is stored as written**, which is
    // the smallest form of what a real server does: PostgreSQL stores a parse tree everywhere and
    // prints it back, so the text that comes out is never quite the text that went in. For a
    // function call or an operator the two agree once the outer parentheses are normalised, and
    // this crate has stored the written text since the expression-index unit. A `CASE` is the
    // first shape where they cannot agree — the implicit `ELSE` is **filled in with the resolved
    // type**, which is not in the written text at all and is not knowable until here, where the
    // expression has met the table.
    let text = match &resolved {
        plan::Expr::Case { .. } => deparse(&resolved, table, ty),
        _ => expr.to_owned(),
    };
    Ok((text, ty))
}

/// One resolved expression, as `pg_get_expr` prints it.
///
/// Only reached for a `CASE` today — everything else is stored as written — but total over the
/// expression type on purpose: a new variant that can be an index key has to decide how it prints
/// before it compiles, which is the same reason [`catalog::ExprShape`] exists.
///
/// Operator-shaped nodes carry their own parentheses, exactly as PostgreSQL's deparser adds them,
/// so a `WHEN` writes its condition unadorned and gets `WHEN (rating > 0)` for a comparison and
/// `WHEN flag` for a boolean column. Measured, both.
fn deparse(expr: &plan::Expr, table: &TableDef, ty: ColumnType) -> String {
    use crate::plan::Expr;
    let sub = |expr: &Expr| deparse(expr, table, ty);
    match expr {
        Expr::Ordinal { at, .. } => table
            .columns
            .get(*at)
            .map_or_else(|| format!("<column {at}>"), |column| column.name.clone()),
        Expr::Literal(literal) => deparse_literal(literal, ty),
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", sub(left), op.symbol(), sub(right))
        }
        Expr::Not(operand) => format!("(NOT {})", sub(operand)),
        Expr::IsNull { operand, negated } => format!(
            "({} IS {}NULL)",
            sub(operand),
            if *negated { "NOT " } else { "" }
        ),
        Expr::InList {
            operand,
            list,
            negated,
        } => format!(
            "({} {}IN ({}))",
            sub(operand),
            if *negated { "NOT " } else { "" },
            list.iter().map(&sub).collect::<Vec<_>>().join(", ")
        ),
        Expr::Scalar { func, operand } => format!("{}({})", func.name(), sub(operand)),
        Expr::ToText { operand, .. } => format!("({})::text", sub(operand)),
        // **Five lines, indented four spaces, with the implicit `ELSE` materialised.** This layout
        // is what `pg_get_indexdef` answers on a real server — `pg_get_indexdef` deparses with
        // `PRETTYFLAG_INDENT`, which puts every keyword of a `CASE` on its own line — and it is
        // why `ActiveRecord`'s schema dumper sees a multi-line definition for statement 198.
        // Captured with the newlines escaped, because a corpus line cannot hold one
        // (`tests/corpus/pg19_case_expression.txt`).
        Expr::Case {
            branches,
            otherwise,
        } => {
            let mut text = "\nCASE".to_owned();
            for branch in branches {
                let _ = write!(
                    text,
                    "\n    WHEN {} THEN {}",
                    sub(&branch.when),
                    sub(&branch.then)
                );
            }
            // An `ELSE` that was not written is **not** absent from the printed form: it is
            // `NULL` of the type the branches resolved to, which is the one part of this text that
            // could not have been produced before the expression met the table.
            let otherwise = otherwise
                .as_deref()
                .map_or_else(|| deparse_literal(&plan::Literal::Null, ty), &sub);
            let _ = write!(text, "\n    ELSE {otherwise}\nEND");
            text
        }
        // Refused before this is reached: `refuse_unless_immutable` rejects every one of them as
        // an index key, and a column reference has been resolved to an `Ordinal` by then. Printed
        // rather than panicked on, because this is a catalog write and not a place to abort.
        Expr::Column { name, .. } => name.clone(),
        Expr::Parameter(number) => format!("${number}"),
        Expr::Outer { at, .. } => format!("<outer {at}>"),
        Expr::Default => "DEFAULT".to_owned(),
        Expr::Sequence(call) => format!("{}()", call.func.name()),
        Expr::CatalogFunc(call) => format!("{}(...)", call.func.name()),
        Expr::Aggregate(call) => format!("{}(...)", call.func.name()),
        Expr::Subquery(sub) => sub.kind.describe().to_owned(),
        Expr::AnyArray { operand, array } => {
            format!("({} = ANY ({}))", sub(operand), sub(array))
        }
        Expr::Subscript { operand, index, .. } => format!("{}[{}]", sub(operand), sub(index)),
    }
}

/// A constant, with the cast PostgreSQL prints on the ones whose type is not in their spelling.
///
/// `'a'::text` and `NULL::text`, not `'a'` and `NULL`: a constant in a stored tree carries a type,
/// and the deparser writes it out wherever the literal alone would not say what it is. A number
/// and a boolean say it themselves.
fn deparse_literal(literal: &plan::Literal, ty: ColumnType) -> String {
    use crate::plan::Literal;
    match literal {
        Literal::Null => format!("NULL::{}", ty.name()),
        Literal::Bool(value) => value.to_string(),
        Literal::Integer(value) => value.to_string(),
        Literal::Decimal(digits) => digits.clone(),
        Literal::String(text) => format!("'{}'::{}", text.replace('\'', "''"), ty.name()),
        // **A number prints bare and everything else prints with its type**, which is
        // PostgreSQL's `get_const_expr` and is measured: `(rating > 0)` for an integer column,
        // `(t > 'a'::text)` for a text one and `(d > '2020-01-01'::date)` for a date. The label is
        // what tells the reader — and the re-parse — which type a quoted constant is; a numeral
        // says so itself.
        Literal::Typed(value) => match value.as_ref() {
            Datum::Bool(flag) => flag.to_string(),
            number @ (Datum::Int8(_)
            | Datum::Int4(_)
            | Datum::Int2(_)
            | Datum::Double(_)
            | Datum::Real(_)) => number.to_text().unwrap_or_default(),
            other => format!(
                "'{}'::{}",
                other.to_text().unwrap_or_default().replace('\'', "''"),
                other.column_type().map_or(ty, |own| own).name()
            ),
        },
    }
}

/// Walks one resolved expression, refusing every node that may not be an index key.
fn refuse_unless_immutable(expr: &plan::Expr) -> Result<()> {
    use crate::plan::Expr;
    let mut refusal = None;
    super::subquery::walk(expr, &mut |node| {
        if refusal.is_some() {
            return;
        }
        refusal = match node {
            Expr::Aggregate(_) => Some(SqlError::AggregateNotAllowed(
                "aggregate functions are not allowed in index expressions",
            )),
            Expr::Sequence(_) | Expr::CatalogFunc(_) => Some(SqlError::NotImmutableInIndex),
            // A parameter has no value at `CREATE INDEX` time; a real server answers
            // `42P02 there is no parameter $1`, which is what this prints.
            Expr::Parameter(at) => Some(SqlError::UndefinedParameter(*at)),
            // None of the three can be produced by `parse::lower` from an index column — a
            // sub-select there is a syntax error on both servers, an outer reference needs a
            // query around it, and `DEFAULT` is refused where it is written. This arm is what
            // says so rather than a panic if one ever arrives.
            Expr::Subquery(_) | Expr::Outer { .. } | Expr::Default => {
                Some(SqlError::unsupported("that index expression"))
            }
            _ => None,
        };
    });
    refusal.map_or(Ok(()), Err)
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
        if let AlterTableAction::AddForeignKey(key) = action {
            add_foreign_key(txn, executor, &table, &mut updated, key)?;
            continue;
        }
        if let AlterTableAction::SetColumnarReplicas { replicas } = action {
            set_columnar_replicas(txn, executor, &updated, *replicas)?;
            continue;
        }
        if let AlterTableAction::SetTriggersDisabled { disabled } = action {
            set_triggers_disabled(txn, executor, &table, &mut updated, *disabled)?;
            continue;
        }
        let AlterTableAction::AddColumn {
            column,
            if_not_exists,
        } = action
        else {
            let AlterTableAction::SetRetention { retention_ms } = action else {
                unreachable!("every other action was handled above")
            };
            // Retention is not part of the table definition and deliberately does **not** bump the
            // schema version (`docs/adr/0021-time-machine.md` Decision 4). It changes nothing
            // about how a row is written or read, and bumping would make every node in the cluster
            // discard its table cache to learn a number none of them uses. The collector picks it
            // up on its next pass, which is the only place it means anything.
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
            let primary_key: Vec<Datum> = table
                .primary_key
                .iter()
                .map(|&ordinal| row[ordinal].clone())
                .collect();
            // `None` is a row a **partial** index excludes, and it is why this goes through
            // `super::index` rather than building the key here: a backfill that indexed the rows
            // the predicate leaves out would refuse `CREATE UNIQUE INDEX … WHERE …` for a
            // duplicate among them, and leave entries no writer would ever remove.
            let Some(entry) = super::index::entry(tenant, table, index, &row, &primary_key)? else {
                continue;
            };
            if entry.by_value && entries.iter().any(|(existing, _)| existing == &entry.key) {
                // Out of the walk as well as out of the page: the index cannot be built and
                // reading the rest of the table would learn nothing.
                return Err(SqlError::UniqueViolation {
                    constraint: index.name.clone(),
                    key: None,
                });
            }
            entries.push((
                entry.key,
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
