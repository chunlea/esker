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
use crate::plan::{AddValuePosition, AlterType, AlterTypeAction, CreateType, DropType};

/// `CREATE TYPE`.
pub(super) fn create(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateType,
) -> Result<Outcome> {
    // **A type takes a schema, the way a relation does.** An unqualified name goes in the first
    // schema of the `search_path` (`Executor::creation_schema`) and not in `public`, which is what
    // `enum_test.rb` asks for: an enum created under `search_path = test_schema` is
    // `test_schema.mood_in_test_schema`, and the dumper prints the schema it reads back.
    let wrote_a_schema = create.name.contains(catalog::SCHEMA_SEPARATOR);
    let (schema, bare) = catalog::split_qualified(&create.name);
    let (schema, bare) = (schema.to_owned(), bare.to_owned());
    let schema = if wrote_a_schema {
        schema
    } else {
        executor.creation_schema(&*txn)?
    };
    // A schema that is not there is `3F000` before anything else is looked at — the same class and
    // the same sentence `CREATE TABLE nosuchschema.t` gives, rather than a type that quietly lands
    // in `public`.
    if !executor.catalog_view(&*txn)?.schema_exists(&schema)? {
        return Err(SqlError::UndefinedSchema(schema));
    }
    let stored = catalog::qualify(&schema, &bare);
    // **One namespace for types and relations**, which is what makes the shared oid space honest:
    // a name that is already a table is `42710` here exactly as a duplicate type is.
    if catalog::type_by_name(txn, executor.tenant, &stored)?.is_some() {
        // **The `DO` block's guard, and the whole of what it does.** `create_enum` asks `pg_type`
        // first and skips the `CREATE` when the type is there, so the second run is a success that
        // changes nothing — the labels of the *first* run survive even when the second names
        // different ones, which is measured (`pg19_do_create_enum.txt`).
        if create.if_not_exists {
            return Ok(Outcome::done("DO"));
        }
        // **The bare name, though the statement may have written a schema.** Measured:
        // `CREATE TYPE g1e_a.g1e_mood` over an existing one is `42710 type "g1e_mood" already
        // exists`, with the schema outside the quotes.
        return Err(SqlError::DuplicateType(bare));
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
            name: stored,
            oid,
            kind: create.kind.clone(),
        },
    )?;
    Ok(Outcome::done("CREATE TYPE"))
}

/// `ALTER TYPE <name> RENAME TO … | ADD VALUE … | RENAME VALUE … TO …`.
///
/// The three shapes `ActiveRecord`'s `rename_enum`, `add_enum_value` and `rename_enum_value` send.
///
/// **`ADD VALUE` in the middle rewrites rows, where PostgreSQL does not**, and that difference is
/// the whole of what this costs. A real server's `pg_enum.enumsortorder` is a **`real`**: inserting
/// `angry` before `ok` gives it sort order **1.5** and moves nothing. This node stores an enum
/// value as the *ordinal of its label's position* (ADR 0050), which is what gives ordering,
/// grouping and indexing for free — and it means a label inserted before the end shifts every
/// later label's ordinal, so every stored row holding one has to move with it. A node that skipped
/// that would answer the *wrong label* for rows written before the `ALTER`, which is worse than
/// slow: the representation ADR 0050 chose is exactly the one that cannot leave those rows alone.
pub(super) fn alter(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    alter: &AlterType,
) -> Result<Outcome> {
    let stored = executor.stored_type_name(&*txn, &alter.name)?;
    let Some(def) = catalog::type_by_name(&*txn, executor.tenant, &stored)? else {
        return Err(SqlError::UndefinedType(alter.name.clone()));
    };
    match &alter.action {
        AlterTypeAction::RenameTo(to) => rename_type(executor, txn, &def, to)?,
        AlterTypeAction::AddValue {
            label,
            if_not_exists,
            position,
        } => add_enum_value(
            executor,
            txn,
            &def,
            label,
            *if_not_exists,
            position.as_ref(),
        )?,
        AlterTypeAction::RenameValue { from, to } => {
            rename_enum_value(executor, txn, &def, from, to)?;
        }
    }
    executor.catalog_written = true;
    Ok(Outcome::done("ALTER TYPE"))
}

/// `RENAME TO`: the record moves and **the oid does not**, so every column of it keeps working.
fn rename_type(executor: &Executor, txn: &mut dyn Txn, def: &TypeDef, to: &str) -> Result<()> {
    // The new name is qualified into the same schema the old one is in, so `ALTER TYPE s.t RENAME
    // TO u` leaves it in `s` — which is what PostgreSQL does; `RENAME TO` does not move a type
    // between schemas and has `SET SCHEMA` for that.
    let (schema, _) = catalog::split_qualified(&def.name);
    let stored = if schema == catalog::PUBLIC_SCHEMA {
        to.to_owned()
    } else {
        catalog::qualify(schema, to)
    };
    if catalog::type_by_name(&*txn, executor.tenant, &stored)?.is_some() {
        return Err(SqlError::DuplicateType(to.to_owned()));
    }
    catalog::drop_type(txn, executor.tenant, &def.name)?;
    catalog::put_type(
        txn,
        executor.tenant,
        &TypeDef {
            name: stored,
            oid: def.oid,
            kind: def.kind.clone(),
        },
    )?;
    Ok(())
}

/// `RENAME VALUE 'from' TO 'to'`: the label changes and its position does not, so no row moves.
fn rename_enum_value(
    executor: &Executor,
    txn: &mut dyn Txn,
    def: &TypeDef,
    from: &str,
    to: &str,
) -> Result<()> {
    let catalog::TypeKind::Enum { labels } = &def.kind else {
        return Err(SqlError::unsupported(format!(
            "ALTER TYPE ... RENAME VALUE on the type {}",
            catalog::display_name(&def.name)
        )));
    };
    let mut labels = labels.clone();
    // **Two different codes, measured**: a label that is not there is `22023` and one that is
    // already taken is `42710`. The order matters — renaming `sad` to `happy` when both exist is
    // the duplicate, not the missing one.
    let Some(at) = labels.iter().position(|label| label == from) else {
        return Err(SqlError::NotAnEnumLabel(from.to_owned()));
    };
    if labels.iter().any(|label| label == to) {
        return Err(SqlError::DuplicateEnumLabel(to.to_owned()));
    }
    to.clone_into(&mut labels[at]);
    put_labels(executor, txn, def, labels)
}

/// `ADD VALUE [IF NOT EXISTS] 'label' [BEFORE | AFTER 'other']`.
fn add_enum_value(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    def: &TypeDef,
    label: &str,
    if_not_exists: bool,
    position: Option<&AddValuePosition>,
) -> Result<()> {
    let catalog::TypeKind::Enum { labels } = &def.kind else {
        return Err(SqlError::unsupported(format!(
            "ALTER TYPE ... ADD VALUE on the type {}",
            catalog::display_name(&def.name)
        )));
    };
    let mut labels = labels.clone();
    if labels.iter().any(|seen| seen == label) {
        // **A no-op, not a success that changes the order.** `IF NOT EXISTS` over a label that is
        // there leaves the list exactly as it was, measured — including its position.
        if if_not_exists {
            return Ok(());
        }
        return Err(SqlError::DuplicateEnumLabel(label.to_owned()));
    }
    let at = match position {
        None => labels.len(),
        Some(AddValuePosition::Before(other) | AddValuePosition::After(other)) => {
            let Some(found) = labels.iter().position(|seen| seen == other) else {
                return Err(SqlError::NotAnEnumLabel(other.clone()));
            };
            match position {
                Some(AddValuePosition::After(_)) => found + 1,
                _ => found,
            }
        }
    };
    // **Every stored row at or after the insert moves up by one**, and it has to happen before the
    // labels are written: the rows are read against the ordinals they were written with.
    if at < labels.len() {
        shift_enum_ordinals(executor, txn, def, at)?;
    }
    labels.insert(at, label.to_owned());
    put_labels(executor, txn, def, labels)
}

/// Writes a type's labels back, keeping its oid.
fn put_labels(
    executor: &Executor,
    txn: &mut dyn Txn,
    def: &TypeDef,
    labels: Vec<String>,
) -> Result<()> {
    catalog::put_type(
        txn,
        executor.tenant,
        &TypeDef {
            name: def.name.clone(),
            oid: def.oid,
            kind: catalog::TypeKind::Enum { labels },
        },
    )
}

/// Adds one to every stored ordinal at or after `at`, in every column declared as this enum.
///
/// The scan is paged the way every other whole-table rewrite in this crate is.
fn shift_enum_ordinals(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    def: &TypeDef,
    at: usize,
) -> Result<()> {
    let relations = executor.catalog_view(&*txn)?.relations()?;
    let mut wanted: Vec<(u64, Vec<usize>)> = Vec::new();
    for table in relations.tables() {
        let columns: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.user_type == Some(def.oid))
            .map(|(ordinal, _)| ordinal)
            .collect();
        if !columns.is_empty() {
            wanted.push((table.id, columns));
        }
    }
    // **The stored ordinal is the label's index plus one** (`catalog::enum_ordinal`), so a label
    // inserted at index `at` moves every ordinal from `at + 1` up. Using the index here shifted
    // every row including the ones before the insert, which turned `sad` into `angry`.
    let floor = i16::try_from(at + 1).unwrap_or(i16::MAX);
    for (table_id, columns) in wanted {
        let table = executor.table_by_id(txn, table_id)?;
        let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
        let schema = table.row_schema();
        let mut rows: Vec<Vec<crate::value::Datum>> = Vec::new();
        super::for_each_page(txn, &start, &end, |_, page| {
            for (_, value) in page {
                rows.push(crate::row::decode_row(&schema, value, None)?);
            }
            Ok(())
        })?;
        for row in rows {
            let mut moved = row.clone();
            let mut changed = false;
            for &ordinal in &columns {
                if let crate::value::Datum::Int2(held) = moved[ordinal]
                    && held >= floor
                {
                    moved[ordinal] = crate::value::Datum::Int2(held.saturating_add(1));
                    changed = true;
                }
            }
            if changed {
                let mut written = super::Written::default();
                super::dml::rewrite_row(executor, txn, &table, &row, &moved, &mut written)?;
            }
        }
    }
    Ok(())
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
    let relations = executor.catalog_view(txn)?.relations()?;
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
    let relations = executor.catalog_view(txn)?.relations()?;
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
        // Resolved once, and every step below takes the stored name: with `g1ts_a, public` and the
        // name in both, `DROP TYPE g1ts_shadow` drops `g1ts_a`'s and leaves `public`'s. Measured.
        let stored = executor.stored_type_name(&*txn, name)?;
        if catalog::type_by_name(&*txn, executor.tenant, &stored)?.is_none() {
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
            drop_columns_of_type(executor, txn, &stored)?;
        } else {
            refuse_if_a_column_depends(executor, txn, &stored)?;
        }
        catalog::drop_type(txn, executor.tenant, &stored)?;
    }
    Ok(Outcome::done("DROP TYPE"))
}
