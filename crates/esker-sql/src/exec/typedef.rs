//! `CREATE TYPE` and `DROP TYPE`: the three shapes, and where a user-defined type lives.
//!
//! A type is a record of its own, keyed by name under `'y'` (`crate::catalog::record`, version 22),
//! and its **oid comes from the tenant's relation-id sequence** — the counter tables and indexes
//! draw from. That is PostgreSQL's arrangement, where `pg_class` and `pg_type` are two catalogs
//! over one oid space, and it is what lets `'floatrange'::regtype` and `'people'::regclass` be
//! numbers a client can compare without either catalog knowing about the other.
//!
//! # What this module does and does not do
//!
//! It makes the type **exist**: `CREATE TYPE` succeeds, `pg_type`, `pg_range` and `pg_enum` report
//! it, `DROP TYPE` removes it, and the four refusals a real server raises are the ones raised here.
//! It does **not** make the type usable as a column type or as a value — a range constructor, a
//! composite's field access, an enum's declaration order as a sort order. Those need a value
//! representation in a crate whose `ColumnType` is a closed, `Copy` enum, which is a decision worth
//! writing down rather than improvising (`docs/plans/phase-9-rails.md`).

use crate::backend::Txn;
use crate::catalog::{self, TypeDef};
use crate::error::{Result, SqlError};
use crate::exec::{Executor, Outcome};
use crate::plan::{CreateType, DropType};

/// `CREATE TYPE`.
pub(super) fn create(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateType,
) -> Result<Outcome> {
    // **One namespace for types and relations**, which is what makes the shared oid space honest:
    // a name that is already a table is `42710` here exactly as a duplicate type is.
    if catalog::type_by_name(txn, executor.tenant, &create.name)?.is_some() {
        // **The `DO` block's guard, and the whole of what it does.** `create_enum` asks `pg_type`
        // first and skips the `CREATE` when the type is there, so the second run is a success that
        // changes nothing — the labels of the *first* run survive even when the second names
        // different ones, which is measured (`pg19_do_create_enum.txt`).
        if create.if_not_exists {
            return Ok(Outcome::done("DO"));
        }
        return Err(SqlError::DuplicateType(create.name.clone()));
    }
    // **A range's subtype has to be ordered**, because ordering the bounds is what a range is:
    // `CREATE TYPE r AS RANGE (subtype = point)` is `42704 data type point has no default
    // operator class for access method "btree"` on a real server, with a HINT of its own.
    // Measured — and the two types it refuses are the same two `CREATE INDEX` refuses, which is
    // not a coincidence: both questions are "can a btree put these in order".
    if let catalog::TypeKind::Range { subtype, .. } = create.kind
        && matches!(
            subtype,
            crate::value::ColumnType::Json
                | crate::value::ColumnType::Point
                | crate::value::ColumnType::Xml
        )
    {
        return Err(SqlError::RangeSubtypeNotOrdered(
            <crate::value::ColumnType as crate::value::PgType>::name(subtype),
        ));
    }
    let oid = catalog::allocate_id(txn, executor.tenant)?;
    // **Two ids, and the second is the array type's.** A real server makes a type's array type
    // when it makes the type, and `pg_type` here reports `typarray` as `oid + 1` and emits a row
    // for it — so the id has to be *taken* and not merely named. With one id per type the second
    // `CREATE TYPE` in a database was handed the first one's array oid, and `pg_type` had two
    // rows claiming it: `ActiveRecord`'s `enum_types()` answered a phantom `_mood` carrying
    // `tense`'s labels. Found by asking that query about two enums, which one enum cannot show.
    let array = catalog::allocate_id(txn, executor.tenant)?;
    debug_assert_eq!(
        array,
        oid + 1,
        "the array type's id is the type's plus one, which is what `pg_type` reports"
    );
    catalog::put_type(
        txn,
        executor.tenant,
        &TypeDef {
            name: create.name.clone(),
            oid,
            kind: create.kind.clone(),
        },
    );
    Ok(Outcome::done("CREATE TYPE"))
}

/// `2BP01` when a column is still declared as this type.
///
/// **The first dependent, not all of them**, which is what a real server reports: it names one
/// column and its table in the DETAIL and stops. Without this a `DROP TYPE` would leave every row
/// of that column holding an ordinal with no labels to read it by — the one way ADR 0050's stored
/// ordinal can become a *wrong* value rather than a missing one, which is why the rule and the
/// refusal arrived together.
fn refuse_if_a_column_depends(executor: &Executor, txn: &dyn Txn, name: &str) -> Result<()> {
    let Some(def) = catalog::type_by_name(txn, executor.tenant, name)? else {
        return Ok(());
    };
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    for table in relations.tables() {
        for column in &table.columns {
            if column.user_type == Some(def.oid) {
                return Err(SqlError::DependentType {
                    ty: name.to_owned(),
                    detail: format!(
                        "column {} of table {} depends on type {name}",
                        column.name, table.name
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Drops every column declared as this type, which is what `CASCADE` means here.
///
/// Measured: after `DROP DOMAIN ds_dep CASCADE` the type is gone **and so is the column** —
/// `information_schema.columns` has no row for it. A cascade that dropped the type and left the
/// column would leave a column whose declared type is not in the catalog, which is the shape that
/// makes a later statement fail with a message about neither.
fn drop_columns_of_type(executor: &mut Executor, txn: &mut dyn Txn, name: &str) -> Result<()> {
    let Some(def) = catalog::type_by_name(txn, executor.tenant, name)? else {
        return Ok(());
    };
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    let mut wanted: Vec<(u64, Vec<String>)> = Vec::new();
    for table in relations.tables() {
        let columns: Vec<String> = table
            .user_columns()
            .filter(|(_, column)| column.user_type == Some(def.oid))
            .map(|(_, column)| column.name.clone())
            .collect();
        if !columns.is_empty() {
            wanted.push((table.id, columns));
        }
    }
    for (table_id, columns) in wanted {
        for column in columns {
            let table = executor.table_by_id(txn, table_id)?;
            super::ddl::drop_column_cascading(executor, txn, &table, &column)?;
        }
    }
    Ok(())
}

/// `DROP TYPE [IF EXISTS] <name> [, …]`.
pub(super) fn drop(executor: &mut Executor, txn: &mut dyn Txn, drop: &DropType) -> Result<Outcome> {
    for name in &drop.names {
        if catalog::type_by_name(txn, executor.tenant, name)?.is_none() {
            if drop.if_exists {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "type",
                    name: name.clone(),
                });
                continue;
            }
            return Err(SqlError::UndefinedType(name.clone()));
        }
        // **`CASCADE` takes the dependent columns with it**, and without it the refusal names
        // one. It used to refuse either way, which meant `DROP DOMAIN d CASCADE` answered with
        // the hint telling you to write the clause you had just written.
        if drop.cascade {
            drop_columns_of_type(executor, txn, name)?;
        } else {
            refuse_if_a_column_depends(executor, txn, name)?;
        }
        catalog::drop_type(txn, executor.tenant, name);
    }
    Ok(Outcome::done("DROP TYPE"))
}
