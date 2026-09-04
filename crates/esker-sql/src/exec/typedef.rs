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
        return Err(SqlError::DuplicateType(create.name.clone()));
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
        refuse_if_a_column_depends(executor, txn, name)?;
        catalog::drop_type(txn, executor.tenant, name);
    }
    Ok(Outcome::done("DROP TYPE"))
}
