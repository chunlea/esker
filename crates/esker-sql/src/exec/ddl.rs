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

    let declared = declared_columns(create)?;
    // **The parents' columns come first**, whatever order the child declared its own in, and a
    // child that redeclares an inherited name merges into it rather than adding a second column.
    let (parents, columns) = inherited_columns(executor, txn, create, declared)?;
    refuse_unavailable_defaults(txn, executor, &columns)?;

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
            default_expr: None,
            not_null: true,
            // The executor fills it on every insert, so it has neither.
            default: None,
            missing: None,
            generated: None,
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
        parents: parents.iter().map(|parent| parent.id).collect(),
        // Filled by the parents, not here: this table is nobody's parent yet.
        children: Vec::new(),
        child_scans: Vec::new(),
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
    // **The other half of the edge.** A parent's `children` is what a scan of it reads to reach
    // these rows, and what `DROP TABLE` reads to refuse; the child's `parents` alone would answer
    // neither without a scan of every table record. The two are written in one transaction, so a
    // reader never sees one without the other.
    for parent in &parents {
        let mut updated = (**parent).clone();
        updated.children.push(table.id);
        // The schema version moves because every node's cached `TableDef` has to learn it: a node
        // still holding the old one would scan the parent and miss the child's rows entirely.
        updated.schema_version += 1;
        catalog::replace_table(txn, executor.tenant, parent, &updated)?;
    }
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
            default_expr: column.default_expr.clone(),
            // A primary key column is NOT NULL whether or not it said so, which is PostgreSQL's
            // rule and also ours by necessity: a NULL cannot be part of a row key.
            not_null: column.not_null || create.primary_key.contains(&column.name),
            default: column.default.clone(),
            // **No missing value at `CREATE TABLE`**, whatever the default is. Nothing predates a
            // column the table was created with, so there is no narrower row for a pad to answer
            // for — and writing one would be a claim about rows that cannot exist.
            missing: None,
            generated: column.generated.clone(),
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

/// Refuses a volatile default whose function needs an extension that is not installed.
///
/// **At `CREATE TABLE`, not at the first insert.** A real server resolves the default expression
/// when the table is created, so a table defaulting to `uuid_generate_v4()` cannot exist before
/// `uuid-ossp` does — and a node that deferred the check would accept the table and then fail
/// every insert into it, which is a worse answer than refusing the statement that was wrong.
fn refuse_unavailable_defaults(
    txn: &dyn Txn,
    executor: &Executor,
    columns: &[ColumnDef],
) -> Result<()> {
    for column in columns {
        // Matched on the **text**, because that is what the catalog holds now that a default is
        // any expression. `uuid_generate_v4` is the only name `uuid-ossp` promises that this node
        // has, so a default naming it anywhere in its expression needs the extension.
        let Some(expr) = &column.default_expr else {
            continue;
        };
        if !expr.contains("uuid_generate_v4") {
            continue;
        }
        if !catalog::pg_catalog::is_installed(txn, executor.tenant, "uuid-ossp")? {
            return Err(SqlError::UndefinedFunctionName(
                "uuid_generate_v4()".to_owned(),
            ));
        }
    }
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
            nulls_not_distinct: constraint.nulls_not_distinct,
            // It **is** a constraint, which is what separates it from the identical index a
            // `CREATE UNIQUE INDEX` builds: only this one gets a `pg_constraint` row.
            constraint: Some(if constraint.deferrable {
                catalog::UniqueKind::Deferrable
            } else {
                catalog::UniqueKind::Immediate
            }),
            state: catalog::SchemaState::Public,
            state_since: 1,
            predicate: None,
        });
    }
    Ok(indexes)
}

/// `CREATE EXTENSION [IF NOT EXISTS] name`.
///
/// Three outcomes and the clause only changes one of them:
///
/// * the build **does not have** it — `0A000 extension "x" is not available`, with PostgreSQL's
///   own HINT, **whether or not** `IF NOT EXISTS` was written. Measured, both spellings. The
///   clause is about existence and this is about availability;
/// * it is **already installed** — `42710` without the clause, a notice and a plain success with
///   it, and either way the version does not change;
/// * otherwise it is recorded at the version the build offers, which is the `default_version`
///   `pg_available_extensions` was already reporting. The two views are one fact and this is where
///   it is written.
pub(super) fn create_extension(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateExtension,
) -> Result<Outcome> {
    let done = Ok(Outcome::done("CREATE EXTENSION"));
    let Some(version) = catalog::pg_catalog::available_extension(&create.name) else {
        return Err(SqlError::ExtensionNotAvailable(create.name.clone()));
    };
    if catalog::pg_catalog::is_installed(txn, executor.tenant, &create.name)? {
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(create.name.clone()));
            return done;
        }
        return Err(SqlError::DuplicateExtension(create.name.clone()));
    }
    catalog::install_extension(txn, executor.tenant, &create.name, version);
    done
}

/// A sequence's first value as the counter stores it.
///
/// The counter is a `u64` and `START` is signed, so a negative start is refused by name rather
/// than wrapped into an enormous positive one — this node counts up from the value it is given and
/// has nowhere to put a number below zero.
fn sequence_start(start: i64) -> Result<u64> {
    u64::try_from(start)
        .map_err(|_| SqlError::unsupported(format!("CREATE SEQUENCE ... START {start}")))
}

/// `ALTER TABLE … ALTER COLUMN c SET DEFAULT <expr>` and `… DROP DEFAULT`.
///
/// **A sequence default replaces whatever the column had, including another sequence**, and that
/// replacement is the point of the statement: `pg_attrdef` holds one row for the column before and
/// after, and the sequence the column *used* to draw from stops filling it — which is what makes
/// that one droppable without `CASCADE` on the very next line. A node that added the new sequence
/// beside the old one would leave the column drawing from two counters and would answer `2BP01`
/// to the drop, which is PostgreSQL's own correct answer to a state it should not be in.
fn set_column_default(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    updated: &mut TableDef,
    column: &str,
    default: Option<&plan::ColumnDefault>,
) -> Result<()> {
    let at = updated
        .column(column)
        .ok_or_else(|| SqlError::UndefinedColumnInRelation {
            column: column.to_owned(),
            relation: updated.name.clone(),
        })?;
    // Resolved before anything is written, so a `nextval` naming nothing leaves the column alone.
    let sequence = match default {
        Some(plan::ColumnDefault::Sequence(name)) => Some(executor.require_sequence(txn, name)?),
        _ => None,
    };

    // Whatever filled this column stops filling it — a sequence, a folded value, an expression.
    // All three are cleared together because a column has **one** default, not one of each.
    for owned in &mut updated.sequences {
        if owned.column == Some(at) {
            owned.column = None;
            catalog::replace_sequence(txn, executor.tenant, owned);
        }
    }
    updated.columns[at].default = None;
    updated.columns[at].default_expr = None;

    match default {
        None => {}
        // The `Some` was resolved above for exactly this arm; a `let else` says so without a
        // panic that would be unreachable and unprovable at the same time.
        Some(plan::ColumnDefault::Sequence(_)) => {
            let Some(mut sequence) = sequence else {
                return Err(SqlError::Internal(
                    "a sequence default reached the writer unresolved".to_owned(),
                ));
            };
            sequence.column = Some(at);
            catalog::replace_sequence(txn, executor.tenant, &sequence);
            // The table's own copy, so the cached definition agrees with the records.
            if let Some(held) = updated
                .sequences
                .iter_mut()
                .find(|held| held.id == sequence.id)
            {
                held.column = Some(at);
            } else {
                updated.sequences.push(sequence);
            }
        }
        Some(plan::ColumnDefault::Value { expr, .. }) => {
            // **Folded here and not where it was lowered**, because folding needs the column's
            // type and a plan is built without the catalog — which is also where `22P02` for a
            // literal the type will not take comes from, exactly as a `CREATE TABLE` default does.
            let ty = updated.columns[at].ty;
            let written = expr.as_deref().unwrap_or("NULL");
            let (folded, unfolded) = crate::parse::fold_column_default(written, ty)?;
            updated.columns[at].default = folded;
            updated.columns[at].default_expr = unfolded;
        }
    }
    Ok(())
}

/// The columns a `CREATE TABLE … INHERITS (…)` ends up with, and the parents it resolved.
///
/// **The inherited columns come first**, in the parents' order, and the table's own follow —
/// whatever order they were written in. Measured: `CREATE TABLE ic2 (extra text) INHERITS (ip)`
/// has `id, number, tag, extra`.
///
/// A child redeclaring an inherited name **merges** into that column rather than adding a second
/// one, and a redeclaration whose type differs is `42804` with two `DETAIL` lines — PostgreSQL
/// spells the position first and the types second.
///
/// What is inherited is the column: its type, its `NOT NULL` and its **default**, the parent's
/// sequence included. What is not is the parent's indexes or its primary key — `pg_index` for the
/// child is empty on a real server, so the uniqueness the parent promises does not hold across the
/// pair, and this node stores the same.
fn inherited_columns(
    executor: &Executor,
    txn: &dyn Txn,
    create: &CreateTable,
    declared: Vec<ColumnDef>,
) -> Result<(Vec<std::sync::Arc<TableDef>>, Vec<ColumnDef>)> {
    if create.inherits.is_empty() {
        return Ok((Vec::new(), declared));
    }
    let mut parents = Vec::with_capacity(create.inherits.len());
    let mut columns: Vec<ColumnDef> = Vec::new();
    for name in &create.inherits {
        let parent = executor.require_table(txn, name)?;
        // The parent's **user** columns: its internal row id is its own identity and means nothing
        // in another table, which gets one of its own if it needs one.
        for (_, column) in parent.user_columns() {
            match columns.iter().position(|held| held.name == column.name) {
                Some(_) => {}
                None => columns.push(column.clone()),
            }
        }
        parents.push(parent);
    }
    for column in declared {
        match columns.iter_mut().find(|held| held.name == column.name) {
            // A redeclared column merges, and the types have to agree.
            Some(held) => {
                if held.ty != column.ty {
                    return Err(SqlError::ColumnTypeConflict {
                        column: column.name.clone(),
                        inherited: held.ty.name(),
                        declared: column.ty.name(),
                    });
                }
                held.not_null |= column.not_null;
            }
            None => columns.push(column),
        }
    }
    Ok((parents, columns))
}

/// **Every function this node has, with the signature `DROP FUNCTION` names it by.**
///
/// This node has no `CREATE FUNCTION`, so the whole function namespace is this list — which is
/// exactly what makes `DROP FUNCTION` implementable rather than a no-op: a name in it is
/// *protected*, and a name outside it does not exist. A test below checks the list against the
/// function enums, so a function added to the language and forgotten here becomes a failure rather
/// than a `DROP FUNCTION` that reports success for something still callable.
///
/// The types are PostgreSQL's spellings, and they are what the `2BP01` prints back — **not** what
/// the user wrote: `concat(VARIADIC "any")` is answered as `concat("any")`, and
/// `convert_to(text, name)` as `convert_to(text,name)` with no space. Measured, both.
const BUILT_IN_FUNCTIONS: &[(&str, &[&str])] = &[
    ("lower", &["text"]),
    ("upper", &["text"]),
    ("random", &[]),
    ("concat", &["\"any\""]),
    ("convert_to", &["text", "name"]),
    ("now", &[]),
    ("gen_random_uuid", &[]),
    ("uuid_generate_v4", &[]),
    ("nextval", &["regclass"]),
    ("currval", &["regclass"]),
    ("lastval", &[]),
    ("setval", &["regclass", "bigint"]),
    ("format_type", &["oid", "integer"]),
    ("pg_get_expr", &["pg_node_tree", "oid"]),
    ("pg_get_indexdef", &["oid"]),
    ("pg_get_constraintdef", &["oid"]),
    ("pg_get_partkeydef", &["oid"]),
    ("obj_description", &["oid", "name"]),
    ("col_description", &["oid", "integer"]),
    ("current_schema", &[]),
    ("current_schemas", &["boolean"]),
    ("array_length", &["anyarray", "integer"]),
    ("array_lower", &["anyarray", "integer"]),
    ("array_upper", &["anyarray", "integer"]),
    ("array_position", &["anyarray", "anyelement"]),
    ("generate_subscripts", &["anyarray", "integer"]),
];

/// One function's canonical signature, as the `2BP01` prints it.
fn built_in_signature(name: &str, args: &[&str]) -> String {
    format!("{name}({})", args.join(","))
}

/// `DROP FUNCTION [IF EXISTS] f [(<types>)] [, …]`.
///
/// **Not a no-op, even though nothing here can create a function.** Three outcomes, and `IF EXISTS`
/// changes only one of them:
///
/// * a **built-in** is `2BP01 … because it is required by the database system`, with or without
///   the clause — the clause covers absence, not protection, and a node answering success would
///   let a schema drop `lower` and report that it had;
/// * a name and signature that match nothing are `42883` without the clause and a success with it;
/// * a statement with **no argument list** is a different statement, not a shorthand: it selects
///   *the* function of that name, and its "not found" sentence is its own, because with no
///   argument list there is no signature to name.
pub(super) fn drop_function(executor: &mut Executor, drop: &plan::DropFunction) -> Result<Outcome> {
    for (name, args) in &drop.functions {
        let matched = BUILT_IN_FUNCTIONS.iter().find(|(built_in, signature)| {
            *built_in == name
                && args
                    .as_ref()
                    .is_none_or(|written| written.as_slice() == *signature)
        });
        if let Some((built_in, signature)) = matched {
            return Err(SqlError::FunctionRequiredBySystem(built_in_signature(
                built_in, signature,
            )));
        }
        if drop.if_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "function",
                name: name.clone(),
            });
            continue;
        }
        // **Two sentences, and which one depends on whether a signature was written.** With an
        // argument list there is one to name; without one there is not, and a real server says so
        // in different words.
        return Err(match args {
            Some(written) => {
                SqlError::FunctionToDropNotFound(format!("{name}({})", written.join(", ")))
            }
            None => SqlError::UnnamedFunctionNotFound(name.clone()),
        });
    }
    Ok(Outcome::done("DROP FUNCTION"))
}

/// `CREATE SEQUENCE [IF NOT EXISTS] s [START n] [INCREMENT BY n] [OWNED BY t.c]`.
///
/// **`OWNED BY` does not give the column a default.** It records that the sequence goes when the
/// column does — the opposite direction from a default — and `pg_attrdef` shows it: creating one
/// against a `bigserial`'s column leaves that column's own sequence as its default and the table
/// with one default row, measured. Pointing the column at the new sequence takes an
/// `ALTER COLUMN … SET DEFAULT`, which is the suite's next statement.
///
/// A column may own more than one, which is why the record is keyed by the sequence's id
/// (`catalog::SequenceDef`).
pub(super) fn create_sequence(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateSequence,
) -> Result<Outcome> {
    // The whole relation namespace, not just other sequences: `CREATE SEQUENCE` over a table's
    // name is the same `42P07` a table over a table's is.
    if existing_relation(executor, txn, &create.name)?.is_some() {
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(create.name.clone()));
            return Ok(Outcome::done("CREATE SEQUENCE"));
        }
        return Err(SqlError::DuplicateTable(create.name.clone()));
    }

    // `OWNED BY t.c` names a column, and **both halves can be wrong with different codes**: a
    // missing table is `42P01` and a missing column is `42703`, which spells the relation it
    // looked in.
    let owner = match &create.owned_by {
        Some((table, column)) => {
            let table = executor.require_table(txn, table)?;
            let at = table
                .column(column)
                .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                    column: column.clone(),
                    relation: table.name.clone(),
                })?;
            Some((table, at))
        }
        None => None,
    };
    let sequence = catalog::SequenceDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name: create.name.clone(),
        table_id: owner
            .as_ref()
            .map_or(catalog::STANDALONE_SEQUENCE_OWNER, |(table, _)| table.id),
        // **Nothing yet.** It fills no column until an `ALTER COLUMN … SET DEFAULT` says so.
        column: None,
        owner_column: owner.as_ref().map(|(_, at)| *at),
        // A value written into a column this does not fill is nobody's business to refuse, and
        // `Identity::Default` is the kind that refuses nothing.
        identity: catalog::Identity::Default,
        start: create.start,
        increment: create.increment,
    };
    catalog::create_sequence(txn, executor.tenant, &sequence)?;
    // The counter starts **at** the start value, because `START n` hands out `n` first — measured,
    // `START 101` answers `101` and then `102`. Storing `n - 1` and stepping would be one short
    // for every sequence anyone gave a `START`.
    catalog::set_sequence_value(
        txn,
        executor.tenant,
        sequence.id,
        sequence_start(create.start)?,
    );
    if let Some((table, _)) = owner {
        // The owning table's cached definition now has one more sequence in it.
        let mut updated = (*table).clone();
        updated.schema_version += 1;
        updated.sequences.push(sequence);
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    }
    Ok(Outcome::done("CREATE SEQUENCE"))
}

/// `DROP SEQUENCE [IF EXISTS] s [, …] [CASCADE | RESTRICT]`.
///
/// **`RESTRICT` and no clause at all are the same statement**, and both refuse: a sequence a
/// column's default depends on is `2BP01`, with a `DETAIL` naming the **column** and the table and
/// the same `Use DROP ... CASCADE` hint a dependent table gets. Measured, both spellings.
///
/// `CASCADE` takes the default with the sequence and **leaves the column**: the default *is* the
/// sequence here, so deleting the record removes both facts and nothing else — the rows stay and
/// the column still takes a value.
pub(super) fn drop_sequence(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropSequence,
) -> Result<Outcome> {
    // **Every name is resolved before any of them is dropped**, because the statement is
    // all-or-nothing: `DROP SEQUENCE a, b` with `b` absent drops neither, measured. Dropping as
    // the loop walked the list would take `a` and then fail, leaving a statement that reported an
    // error and changed the database anyway.
    let mut targets = Vec::with_capacity(drop.names.len());
    for name in &drop.names {
        let (table_id, sequence_id) = match existing_relation(executor, txn, name)? {
            Some(catalog::Relation::Sequence {
                table_id,
                sequence_id,
            }) => (table_id, sequence_id),
            // The name resolves and is the wrong kind, which is `42809` and not `42P01`: the
            // `HINT` says which verb would have worked. A `DROP TABLE` over a sequence gets the
            // mirror of this.
            Some(catalog::Relation::Table { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a sequence",
                    found: "DROP TABLE",
                });
            }
            Some(catalog::Relation::Index { .. } | catalog::Relation::PrimaryKey { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a sequence",
                    found: "DROP INDEX",
                });
            }
            None => {
                if drop.if_exists {
                    executor.notice(SqlError::DoesNotExistSkipping {
                        kind: "sequence",
                        name: name.clone(),
                    });
                    continue;
                }
                return Err(SqlError::UndefinedSequenceForDrop(name.clone()));
            }
        };
        let Some(sequence) = catalog::sequence_by_id(txn, executor.tenant, table_id, sequence_id)?
        else {
            continue;
        };
        targets.push((name, table_id, sequence));
    }

    // **Existence for every name first, dependencies after** — which is the order a real server
    // reports in and not an implementation detail: `DROP SEQUENCE a, b` where `a` has a dependent
    // default and `b` does not exist answers `42P01` about `b`, not `2BP01` about `a`. Checking
    // each name's dependency as the first loop reached it named the wrong one.
    for (name, table_id, sequence) in &targets {
        // **Only a sequence that *fills* a column has a dependent.** One a column merely owns
        // does not: `DROP SEQUENCE s` succeeds while `s` is `OWNED BY t.c`, measured, because
        // ownership points the other way — it says the sequence goes when the *column* does. The
        // `DETAIL` names the column, which is what tells a reader which default is in the way.
        let Some(column) = sequence.column else {
            continue;
        };
        if !drop.cascade {
            let table = executor.table_by_id(txn, *table_id)?;
            return Err(SqlError::DependentSequence {
                sequence: (*name).clone(),
                column: table.columns[column].name.clone(),
                table: table.name.clone(),
            });
        }
    }

    for (_, table_id, sequence) in targets {
        catalog::drop_sequence(txn, executor.tenant, table_id, &sequence);
        // A sequence no column owns has no table record to rewrite, and nothing caches it.
        if table_id == catalog::STANDALONE_SEQUENCE_OWNER {
            continue;
        }
        let table = executor.table_by_id(txn, table_id)?;
        // **The table record is rewritten and its schema version bumped**, and without that the
        // drop is invisible: a `TableDef` is cached per node and keyed by this version, the
        // sequence records live beside the table rather than in it, and deleting them left every
        // reader still holding a definition that says the column has a sequence. The next `INSERT`
        // then filled the column from a counter that had been dropped — reported success, and the
        // corpus caught it on the `23502` that should have followed.
        let mut updated = (*table).clone();
        updated.schema_version += 1;
        updated.sequences.retain(|kept| kept.id != sequence.id);
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    }
    Ok(Outcome::done("DROP SEQUENCE"))
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
                // A `bigserial` both fills and owns the column: it *is* the default, and it goes
                // when the column does. `CREATE SEQUENCE … OWNED BY` sets only the second.
                column: Some(ordinal),
                owner_column: Some(ordinal),
                identity,
                start: 1,
                increment: 1,
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
        // **An inheriting child is a dependent too**, and it stops the drop the same way a
        // foreign key does — `2BP01`, with a `DETAIL` naming the child. `CASCADE` takes the child
        // *table*, unlike the foreign-key case above where it takes only the constraint: a child
        // has no meaning without the parent whose columns it borrowed.
        for &child_id in &table.children {
            let child = executor.table_by_id(txn, child_id)?;
            if !drop.cascade {
                return Err(SqlError::DependentTable {
                    relation: table.name.clone(),
                    detail: format!("table {} depends on table {}", child.name, table.name),
                });
            }
            drop_one_table(executor, txn, &child)?;
        }
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

        drop_one_table(executor, txn, &table)?;
    }
    Ok(Outcome::done("DROP TABLE"))
}

/// One table's rows, index entries and catalog record — everything but the dependency checks.
///
/// Factored out so `CASCADE` can reach an inheriting child with it: a child dropped that way needs
/// exactly the same removal the named table gets, and doing it by hand at the second call site is
/// how one of the three steps gets forgotten.
fn drop_one_table(executor: &Executor, txn: &mut dyn Txn, table: &TableDef) -> Result<()> {
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
    // **Its parents stop listing it**, or a later scan of one would look for rows in a table that
    // is gone. The child's own `parents` goes with its record.
    for &parent_id in &table.parents {
        let parent = executor.table_by_id(txn, parent_id)?;
        let mut updated = (*parent).clone();
        updated.children.retain(|&held| held != table.id);
        updated.schema_version += 1;
        catalog::replace_table(txn, executor.tenant, &parent, &updated)?;
    }
    catalog::drop_table(txn, executor.tenant, table)
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
        // **`CREATE UNIQUE INDEX` is not a constraint.** It builds the same index a `UNIQUE (c)`
        // does and PostgreSQL tells them apart by exactly this: only the constraint has a
        // `pg_constraint` row, and a node that gave one to every unique index would report a
        // constraint the schema never declared.
        constraint: None,
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
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            ..
        } => format!(
            "({} {}{} {})",
            sub(operand),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            sub(pattern)
        ),
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", sub(left), op.symbol(), sub(right))
        }
        Expr::Negate(operand) => format!("(- {})", sub(operand)),
        Expr::Arithmetic {
            op, left, right, ..
        } => {
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
        Expr::Uuid(func) => format!("{}()", func.name()),
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
            // A UUID function is the most volatile thing here: two calls give two values, so an
            // index built from one would be read back at a key nothing ever wrote.
            Expr::Sequence(_) | Expr::CatalogFunc(_) | Expr::Uuid(_) => {
                Some(SqlError::NotImmutableInIndex)
            }
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
#[allow(
    clippy::too_many_lines,
    reason = "one block per ALTER action; splitting it would hide the vocabulary rather than clarify it"
)]
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
        if let AlterTableAction::SetDefault { column, default } = action {
            set_column_default(txn, executor, &mut updated, column, default.as_ref())?;
            changed = true;
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
            default_expr: column.default_expr.clone(),
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
            generated: None,
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
