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
//! NULL DEFAULT 7` is instant there too.
//!
//! A **volatile** default — `gen_random_uuid()`, `random()` — cannot be one constant in the
//! catalog, so it is the case that does rewrite: every row already stored is read, given its own
//! value from the expression, and written back, which is what PostgreSQL does for it and what
//! `ADD COLUMN … GENERATED ALWAYS AS` here already did. What stays refused is bare `NOT NULL` with
//! no default, which has no value to pad *or* to compute, and `serial` on a table with rows.
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

#[allow(
    clippy::too_many_lines,
    reason = "one block per clause of CREATE TABLE; splitting it would hide the vocabulary"
)]
pub(super) fn create_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateTable,
) -> Result<Outcome> {
    // **An unqualified `CREATE` goes to the first schema of the path**, not to `public`: measured,
    // with `sp_a, sp_b` a `CREATE TABLE made_here` lands in `sp_a`. The name is qualified here, so
    // everything below — the record, the derived key and index names, the messages — is about the
    // relation where it actually is.
    //
    // **A temporary table goes in the session's own schema**, whatever the `search_path` says, and
    // the schema is written on demand — a session that makes none writes no record (ADR 0054).
    // This happens **before** the duplicate check, and that order is the whole of why a permanent
    // `things` does not stop a temporary one: they are in different schemas, so the name is only
    // taken when the *temp* schema already has it. Measured — a `CREATE TABLE` of a name a temp
    // table holds succeeds too, in the other direction.
    let temporary = create.persistence == catalog::Persistence::Temporary
        && !create.name.contains(catalog::SCHEMA_SEPARATOR);
    let placed = if temporary {
        let schema = executor.ensure_temp_schema(txn)?;
        let mut moved = create.clone();
        moved.name = catalog::qualify(&schema, &create.name);
        moved
    } else {
        qualified_create(&*txn, executor, create)?
    };
    let written = create.name.clone();
    let create = &placed;

    // **The schema it is going in, and no other.** This was a `search_path` walk, which is wrong
    // for a `CREATE`: a permanent `things` and a temporary `things` coexist on a real server, and
    // a walk finds the temp one and refuses the permanent one it is not making. The message quotes
    // the name **as written** — `relation "things" already exists`, not the placed
    // `pg_temp_3.things` — measured.
    if catalog::pg_catalog::view(&create.name).is_some()
        || executor
            .catalog_view(&*txn)?
            .relation(&create.name)?
            .is_some()
    {
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(written));
            return Ok(Outcome::done("CREATE TABLE"));
        }
        return Err(SqlError::DuplicateTable(written));
    }

    refuse_missing_schema(&*txn, executor, &create.name)?;
    let declared = declared_columns(&*txn, executor, create)?;
    // **The parents' columns come first**, whatever order the child declared its own in, and a
    // child that redeclares an inherited name merges into it rather than adding a second column.
    let (parents, columns) = inherited_columns(executor, txn, create, declared)?;
    // A partition takes the parent's columns and declares none of its own; see
    // [`partition_of_columns`].
    let (parents, columns) = partition_of_columns(executor, txn, create, parents, columns)?;
    refuse_unavailable_defaults(txn, executor, &columns)?;

    let key_position = |name: &String| {
        columns
            .iter()
            .position(|column| &column.name == name)
            .ok_or_else(|| SqlError::UndefinedColumnInKey(name.clone()))
    };

    // **A partition takes the parent's primary key as well as its columns.** Without it the
    // partition declares no key, gets an internal row id the parent has not, and a row routed
    // from the parent is one value short of the partition's width — an internal error at the
    // first `INSERT`, measured. The parent's key is taken by *name*, which is what makes it land
    // on the partition's own columns.
    let inherited_key = partition_primary_key(create, parents.first());
    let declared_key = inherited_key.as_ref().unwrap_or(&create.primary_key);

    // No declared key: the table gets an internal row id at position 0, and the primary key
    // points at it. Position 0 rather than the end so that a later `ALTER TABLE ADD COLUMN`
    // appends after the user's columns and cannot move it.
    let (columns, primary_key, primary_key_name) = if declared_key.is_empty() {
        let mut with_row_id = Vec::with_capacity(columns.len() + 1);
        with_row_id.push(ColumnDef {
            collation: None,
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
            generated_virtual: false,
            comment: None,
            dropped: false,
            user_type: None,
        });
        with_row_id.extend(columns);
        (with_row_id, vec![0], String::new())
    } else {
        let primary_key = declared_key
            .iter()
            .map(key_position)
            .collect::<Result<Vec<_>>>()?;
        // The same question again: a primary key is a unique index, and a `json` column is no more
        // orderable because the word `PRIMARY` was used.
        for &at in &primary_key {
            refuse_unindexable_type(columns[at].ty, catalog::BTREE_ACCESS_METHOD)?;
        }
        // **`<partition>_pkey`, not the parent's name.** A partition's key is its own relation and
        // its own `pg_index` row, and it is that name a duplicate row quotes back — measured,
        // `pk_part_1_pkey` rather than `pk_part_pkey`.
        // **A *derived* name that is taken gets a number; a *given* one is an error.** That is
        // PostgreSQL's rule and it is reachable now that a table can be renamed: after
        // `ALTER TABLE rr RENAME TO rr2` the old key is still `rr_pkey`, so a new `rr` derives
        // the same string — measured, a real server names the second one `rr_pkey1`. Refusing
        // instead would make "rename it out of the way and recreate it" fail, which is an
        // ordinary migration.
        let name = match &create.primary_key_name {
            Some(given) => given.clone(),
            // A primary key is the one derived name with **no column part** — `makeObjectName`
            // takes a null second name for it, so `pkey` and its separator are the whole overhead.
            None => free_derived_name(txn, executor, &create.name, None, "pkey")?,
        };
        (columns, primary_key, name)
    };

    let indexes = unique_indexes(executor, txn, create, &columns)?;
    // Resolved before the table is written, so a key naming no column, an unsupported strategy or
    // an overlapping bound leaves the catalog untouched.
    let partition_by = partition_key(create, &columns)?;
    let partition_bound = partition_bound(executor, txn, create, parents.first())?;
    refuse_uncovered_primary_key(create, partition_by.as_ref(), &primary_key, &columns)?;

    let table_id = catalog::allocate_id(txn, executor.tenant)?;
    let sequences = sequences_for(executor, txn, create, table_id)?;
    let table = TableDef {
        matview: None,
        on_commit: create.on_commit,
        id: table_id,
        persistence: create.persistence,
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
        partition_by,
        partition_bound,
        // Filled by the parents, not here: this table is nobody's parent yet.
        children: Vec::new(),
        triggers: Vec::new(),
        excludes: create.excludes.clone(),
        child_scans: Vec::new(),
        primary_key_name,
        // A table starts at schema version 1; `ALTER TABLE ADD COLUMN` moves it.
        schema_version: 1,
        sequences,
        comment: None,
        primary_key_comment: None,
        enums: std::collections::BTreeMap::new(),
    };

    validate_checks(&table)?;
    let mut table = table;
    normalise_generated(&mut table)?;
    normalise_defaults(&mut table);
    normalise_checks(&mut table);
    normalise_index_predicates(&mut table);
    normalise_exclude_predicates(&mut table);
    let table = table;
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
    // Every partition silently gets its own copy of the parent's indexes; see
    // [`copy_parent_indexes`].
    if table.partition_bound.is_some() {
        copy_parent_indexes(executor, txn, &mut table, &parents)?;
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
///
/// The name is **derived**, so one already taken is numbered rather than refused, and it is
/// derived with the byte budget `makeObjectName` uses rather than by joining and clipping: a
/// 63-byte table still gets a sequence whose name ends in `_seq`. Both are
/// `adapters/postgresql/serial_test.rb`, one test each.
/// The table's columns, as the catalog holds them, refusing a name written twice.
fn declared_columns(
    txn: &dyn Txn,
    executor: &Executor,
    create: &CreateTable,
) -> Result<Vec<ColumnDef>> {
    let mut columns = Vec::with_capacity(create.columns.len());
    for column in &create.columns {
        if columns
            .iter()
            .any(|kept: &ColumnDef| kept.name == column.name)
        {
            return Err(SqlError::DuplicateColumn(column.name.clone()));
        }
        let (ty, user_type) = resolve_user_type(txn, executor, column)?;
        // **A domain carries its base type's modifier**, and the column takes it: `custom_money`
        // is `numeric(8,2)`, so a column of it overflows at precision 8 scale 2 — measured, and
        // the error names those numbers, which is the base type's error and not the domain's.
        let domain = domain_of(txn, executor, user_type)?;
        let typmod = match &domain {
            Some(catalog::TypeKind::Domain { typmod, .. }) => *typmod,
            _ => column.typmod,
        };
        let mut kept = ColumnDef {
            // What the declaration asked for, which the lowerer has already narrowed to an
            // ordering this node has (ADR 0076).
            collation: column.collation.clone(),
            name: column.name.clone(),
            ty,
            typmod,
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
            generated_virtual: column.generated_virtual,
            comment: None,
            dropped: false,
            user_type,
        };
        // **A domain's `DEFAULT` fills a column that declares none**, and a column's own default
        // wins where it has one — measured, `INSERT … DEFAULT VALUES` into a column of
        // `dm_pos DEFAULT 1` stores 1 (ADR 0065). It is set as an *expression* rather than a
        // value, so it goes through the same folding a written `DEFAULT` does and cannot be a
        // second reading of the same text.
        if let Some(catalog::TypeKind::Domain {
            default: Some(text),
            ..
        }) = &domain
            && kept.default_expr.is_none()
            && kept.default.is_none()
        {
            kept.default_expr = Some(text.clone());
        }
        // **The default is folded again once the type is known**, which is the only moment it
        // can be: lowering read it against `crate::plan::Column::ty`, a placeholder for a column
        // declared as a user-defined type.
        if let Some(oid) = user_type
            && let Some(def) = type_by_oid(txn, executor, oid)?
        {
            kept.default = fold_user_default(kept.default.as_ref(), ty, &def, &kept)?;
        }
        refuse_unknown_default_function(txn, executor, &kept)?;
        columns.push(kept);
    }
    Ok(columns)
}

/// Refuses a column `DEFAULT` that calls a function **nobody declared**.
///
/// Lowering carries the name out instead of raising, because a name its vocabulary lacks may be a
/// user's function and only the catalog knows (`crate::parse::lower::column_default`). This is
/// where that is decided: a declared function is accepted, and anything else gets the same
/// `0A000 the function <name>` it always got, at the same moment — when the table is created.
///
/// **`uuid_test.rb` is the file that needs this**: it defines `my_uuid_generator()` and then
/// creates a table defaulting to it, and it never inserts a row — the eight tests read the
/// *schema*, so the name has to survive into `pg_attrdef` and out through the dumper.
fn refuse_unknown_default_function(
    txn: &dyn Txn,
    executor: &Executor,
    column: &ColumnDef,
) -> Result<()> {
    let Some(text) = &column.default_expr else {
        return Ok(());
    };
    let mut parsed = crate::parse::parse_stored_expr(text)?;
    // **The call is looked for, not a parse failure.** Lowering carries an unknown name out as a
    // `UserFunc` rather than raising, so the parse now succeeds for a name nobody declared — and
    // this asked whether it had failed. Second detector of this shape in one change: making a name
    // resolvable breaks everything that recognised it by *failing to resolve* it.
    let mut missing = None;
    let _ = super::subquery::walk_mut(&mut parsed, &mut |expr| {
        if let plan::Expr::CatalogFunc(call) = &*expr
            && call.func == plan::CatalogFunc::UserFunc
            && let Some(plan::Expr::Literal(plan::Literal::String(name))) = call.args.first()
            && missing.is_none()
            && catalog::function(txn, executor.tenant, name)?.is_none()
        {
            missing = Some(name.clone());
        }
        Ok(())
    });
    match missing {
        Some(name) => Err(SqlError::unsupported(format!("the function {name}"))),
        None => Ok(()),
    }
}

/// The `TypeKind` of a column's user type when it is a **domain**, and `None` otherwise.
fn domain_of(
    txn: &dyn Txn,
    executor: &Executor,
    user_type: Option<u64>,
) -> Result<Option<catalog::TypeKind>> {
    let Some(oid) = user_type else {
        return Ok(None);
    };
    Ok(type_by_oid(txn, executor, oid)?
        .map(|def| def.kind)
        .filter(|kind| matches!(kind, catalog::TypeKind::Domain { .. })))
}

/// One user-defined type by oid, for the places that hold an oid rather than a name.
fn type_by_oid(txn: &dyn Txn, executor: &Executor, oid: u64) -> Result<Option<catalog::TypeDef>> {
    Ok(catalog::user_types(txn, executor.tenant)?
        .into_iter()
        .find(|def| def.oid == oid))
}

/// A column's type, once the catalog has been asked about the name lowering could not resolve.
///
/// [ADR 0050](../../../docs/adr/0050-a-user-defined-type-is-a-value.md)'s first unit. Lowering
/// hands over a bare type name it does not recognise (`crate::plan::Column::user_type_name`) and
/// this is where it becomes a type or an error, because this is where the catalog is.
///
/// **An enum's value is the `int2` of its label's position**, which is what makes ordering,
/// grouping, `=` and an index over the column all the ordinal's — `pg_enum.enumsortorder` is
/// PostgreSQL's own sort key for exactly the same reason. The label is rendered back through the
/// catalog on the way out; nothing below this crate ever sees anything but a small integer, which
/// is invariant 7 kept rather than worked around.
///
/// **A range's value is a range**
/// ([ADR 0063](../../../docs/adr/0063-a-user-defined-range-is-a-representation-chosen-by-its-subtype.md)),
/// and the column type is chosen by its *subtype*: a
/// `CREATE TYPE floatrange AS RANGE (subtype = float8)` column holds the same canonical text a
/// `numrange` one does, read and written by the same `crate::value::range`. Which range type it
/// *is* stays the oid's answer, exactly as two enums over the same `int2` stay two types — and
/// two user ranges over one subtype share a representation for the same reason (ADR 0042: a type
/// may share another's representation only if it shares its comparison, and a range's comparison
/// is its canonical text, which is a function of the subtype alone).
///
/// A **composite** is still refused by name: its value is a row, which this vocabulary has no
/// shape for, and answering a `CREATE TABLE` for one would make a column nothing can read — a
/// wrong answer where a refusal is available (ADR 0031).
fn resolve_user_type(
    txn: &dyn Txn,
    executor: &Executor,
    column: &plan::Column,
) -> Result<(ColumnType, Option<u64>)> {
    let Some(name) = &column.user_type_name else {
        return Ok((column.ty, None));
    };
    // Through the `search_path`, so the statement after a `CREATE TYPE` in a schema can name it:
    // `t.column :current_mood, :mood_in_test_schema` is the next line of the very test that made
    // the create take a schema at all.
    let stored = executor.stored_type_name(txn, name)?;
    let Some(def) = catalog::type_by_name(txn, executor.tenant, &stored)? else {
        // The name is not a type anybody declared, which is where lowering's own refusal has been
        // waiting for a catalog to confirm it. Same `0A000` and same wording as before.
        return Err(SqlError::unsupported(format!("the type {name}")));
    };
    match def.kind {
        catalog::TypeKind::Enum { .. } => Ok((ColumnType::Int2, Some(def.oid))),
        catalog::TypeKind::Range { subtype, .. } => match range_representation(subtype) {
            Some(ty) => Ok((ty, Some(def.oid))),
            // A subtype no range here can hold. Named rather than answered: a column typed as
            // some *other* range would read every value back wrong.
            None => Err(SqlError::unsupported(format!(
                "a column of the range type {name}, whose subtype is {}",
                subtype.name()
            ))),
        },
        // **A domain's value is its base type's**, which is the whole of what a domain is
        // ([ADR 0065](../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)):
        // nothing below this line can tell a `custom_money` column from the `numeric(8,2)` it
        // stands for, and the catalog is where the name comes back. Measured — a value too wide
        // for one reports `numeric field overflow` naming precision 8 scale 2, the **base type's**
        // error and not the domain's.
        catalog::TypeKind::Domain { base, .. } => Ok((base, Some(def.oid))),
        // **A composite's value is its canonical record text**, the way `jsonb`'s is its canonical
        // document text — ADR 0042 permits sharing `text`'s representation when the comparison
        // comes with it, and it does: composite equality is field by field, and two field-equal
        // composites render identically (`value::composite`). The oid rides along so `format_type`
        // and `pg_typeof` answer the type's own name, exactly as they do for a domain above.
        catalog::TypeKind::Composite { .. } => Ok((ColumnType::Text, Some(def.oid))),
    }
}

/// A `DEFAULT` on a column declared as a **user-defined type**, read as that type.
///
/// Lowering folds a default against the column's `ty`, and for one of these columns that `ty` is
/// a placeholder — `crate::plan::Column::ty` says so — because only the catalog knows what the
/// name meant. So the fold has to happen again here, where it is known, and the two kinds want
/// opposite things from it:
///
/// * an **enum**'s default is a *label*, which becomes the ordinal the row holds;
/// * a **range**'s default is already its own value and only needs reading as the right range —
///   `DEFAULT '[0.5,0.7]'` arrives as a `Datum::Text` and a column that stored that would hand
///   back text where every other row of it hands back a range.
fn fold_user_default(
    default: Option<&Datum>,
    ty: ColumnType,
    def: &catalog::TypeDef,
    column: &ColumnDef,
) -> Result<Option<Datum>> {
    let Some(value) = default else {
        return Ok(None);
    };
    Ok(Some(match (&def.kind, value) {
        (catalog::TypeKind::Enum { .. }, _) => {
            super::assign::into_enum(value.clone(), column, def)?
        }
        // A composite `DEFAULT` is canonicalised like any other value of one, so the column's
        // default and a row written by hand are the same string.
        (catalog::TypeKind::Composite { fields }, Datum::Text(text)) => {
            Datum::Text(crate::value::composite::canonicalise(text, fields.len())?)
        }
        (catalog::TypeKind::Range { .. }, Datum::Text(text)) => {
            <Datum as crate::value::PgDatum>::from_text(ty, text)?
        }
        // A NULL, or a value that already has the column's shape.
        _ => value.clone(),
    }))
}

/// The range representation whose bounds are `subtype`, or `None` for a subtype no range here
/// holds.
///
/// **The list is the set of range representations this node has**, not a set of blessed subtypes:
/// a user range over `timestamp` is a `tsrange` in the row and a user range over `float8` is the
/// representation added for `range_test.rb`'s own `floatrange`. `int4` and `int8` answer their own
/// range types even though both store an `i64`, so that `pg_range.rngsubtype` reports the subtype
/// that was written.
///
/// `text` is deliberately absent beside `varchar`: the two would share a representation happily —
/// they compare alike — but `lower()` of a bound has to answer `text` for one and
/// `character varying` for the other, and there would be nothing left to tell them apart. A range
/// over `text` is a `0A000` naming the subtype until that is worth a representation of its own.
pub(super) fn range_representation(subtype: ColumnType) -> Option<ColumnType> {
    Some(match subtype {
        ColumnType::Timestamp => ColumnType::TsRange,
        ColumnType::TimestampTz => ColumnType::TstzRange,
        ColumnType::Int4 => ColumnType::Int4Range,
        ColumnType::Int8 => ColumnType::Int8Range,
        ColumnType::Date => ColumnType::DateRange,
        ColumnType::Numeric => ColumnType::NumRange,
        ColumnType::Double => ColumnType::FloatRange,
        ColumnType::Varchar => ColumnType::VarcharRange,
        _ => return None,
    })
}

/// **The built-in half of [`refuse_unindexable`]: types with no default `btree` operator class.**
///
/// Split out because it had **two readers and only one asker**. `CREATE INDEX` asked it and
/// `PRIMARY KEY`/`UNIQUE` did not, so this node built a key over a `json` column that a real
/// server refuses — eight rows of r1's census, and the plain `CREATE INDEX` beside them was
/// already right. A constraint's key is an index; the question is the same question.
///
/// Takes the type rather than the column so the `CREATE TABLE` path, which has no `TableDef` yet,
/// can ask it too. The user-defined-type half stays with its caller, which has the table.
///
/// **`Ok(true)` means approved outright and the caller must stop asking.** A `gin` or `gist` index
/// over a `tsvector` is covered by that method's own operator class, and none of the *btree*
/// questions its caller goes on to ask apply to it — including this node's own refusal of a
/// `tsvector` btree key. Extracting this function turned that `return Ok(())` from "approved, and
/// we are done" into "this check found nothing", and four tests went red on an index a real server
/// builds: an early return means two different things and only one of them survives being moved
/// into a callee. `Ok(false)` is the other one.
fn refuse_unindexable_type(ty: ColumnType, method: &str) -> Result<bool> {
    // **A `tsvector` column is a `gin` or `gist` key.** `tsvector_ops` is the default operator
    // class for both — measured, `pg_opclass` where `opcdefault` — so `USING gin (tsv)` is exactly
    // what a real server accepts and what `schema_test.rb` builds. Neither method reads the index
    // in key order, which is the whole of why they are safe here and `btree` is not: there the
    // order of the key *is* the index, and this node's order for a `tsvector` is its bytes' rather
    // than `tsvector_ops`'
    // ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md), amended).
    if ty == ColumnType::TsVector
        && matches!(
            method,
            catalog::GIN_ACCESS_METHOD | catalog::GIST_ACCESS_METHOD
        )
    {
        return Ok(true);
    }
    if matches!(
        ty,
        ColumnType::Json
            | ColumnType::Point
            // **And the other six geometric shapes**, measured one at a time: `CREATE INDEX` on
            // an `lseg` column is the same `42704` with the same HINT. The list that started as
            // "exactly two" is eight now, and every one of them was probed.
            | ColumnType::Lseg
            | ColumnType::Box
            | ColumnType::Path
            | ColumnType::Polygon
            | ColumnType::Circle
            | ColumnType::Line
            // **And `xml`**, measured: `data type xml has no default operator class for access
            // method "btree"`, the same `42704` with the same HINT.
            | ColumnType::Xml
    ) {
        return Err(SqlError::NoDefaultOperatorClass(ty.name()));
    }
    Ok(false)
}

/// **`json` and `point` cannot be indexed on a real server**, and they are the only two —
/// measured, one type at a time: `jsonb`, every range, `hstore` and every array all have a default
/// btree operator class there and index fine.
///
/// The message is PostgreSQL's own, HINT included, and it names the *type* because that is what a
/// client has to change. Without it this node **built the index**: `CREATE INDEX … (j)` on a
/// `json` column answered `Done` where a real server refuses, and the first write into it then
/// found the row codec's own "an index key column of type json, jsonb, hstore or a range", which
/// is an internal corruption error for a table the user was allowed to create.
///
/// **The other half is this node's own gap and had the same hole.** `jsonb`, `hstore` and every
/// range are index keys there and are not here — `esker_keys::row::is_index_key` is the list —
/// so `CREATE INDEX … (ts_range)` built an index whose first write hit that same internal error.
/// It is `0A000` naming the type: a gap a client can read, rather than a table it cannot write
/// to. The name is the **declared** one, so a `floatrange` column is refused as a `floatrange`
/// and not as the representation holding it.
fn refuse_unindexable(table: &TableDef, column: &ColumnDef, method: &str) -> Result<()> {
    let ty = column.ty;
    // **A `tsvector` column is a `gin` or `gist` key.** `tsvector_ops` is the default operator
    // class for both — measured, `pg_opclass` where `opcdefault` — so `USING gin (tsv)` is exactly
    // what a real server accepts and what `schema_test.rb` builds. Neither method reads the index
    // in key order, which is the whole of why they are safe here and `btree` is not: there the
    // order of the key *is* the index, and this node's order for a `tsvector` is its bytes' rather
    // than `tsvector_ops`'
    // ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md), amended).
    //
    // An *expression* of this type was already accepted, because `index_expression` has no gate of
    // its own; admitting the column is what makes the two paths agree.
    // Approved outright by the method's own operator class: nothing below is about it.
    if refuse_unindexable_type(ty, method)? {
        return Ok(());
    }
    // **A composite is stored as `text` and must not be indexed as one.** `is_index_key` sees the
    // storage type and cannot tell it apart, so the question is asked here, where the column's
    // declared type is known. PostgreSQL orders a record **field by field**; this node's key would
    // be the canonical text, and the two part company as soon as a field needs quoting — `"a b"`
    // sorts by its opening quote. An index that returns rows in an order a real server does not is
    // ADR 0031's worst class, so it is refused by name until a composite has a key encoding of its
    // own (`docs/plans/composite-type.md`).
    if let Some(def) = super::assign::user_type_of(table, column)
        && matches!(def.kind, catalog::TypeKind::Composite { .. })
    {
        return Err(SqlError::unsupported(format!(
            "an index over the composite type {}",
            def.name
        )));
    }
    if !esker_keys::row::is_index_key(ty) {
        let declared = super::assign::user_type_of(table, column)
            .map_or_else(|| ty.name().to_owned(), |def| def.name.clone());
        return Err(SqlError::unsupported(format!(
            "an index on a column of type {declared}"
        )));
    }
    Ok(())
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
    // The fifth reader, at the statement that writes one: `pg_get_constraintdef` prints what is
    // stored, so the deparsed form is what has to be stored. Only the check this statement adds
    // is touched, which is the rule the two `ALTER` sites for a `DEFAULT` already follow.
    let snapshot = updated.clone();
    if let Some(last) = updated.checks.last_mut()
        && let Some(text) = deparse_wrapped_predicate(&snapshot, &last.expr)
    {
        last.expr = text;
    }
    // **The rows already there are checked, unless `NOT VALID` says not to** — the same rule the
    // foreign-key path follows, and it was missing here entirely: a `CHECK` added over a row that
    // violates it was accepted silently, leaving a table whose rows contradict a constraint it
    // advertises as validated. Measured: `23514 check constraint "q_plain" of relation "nv_t" is
    // violated by some row`, a different sentence from the one an `INSERT` gets.
    if check.validated {
        validate_check_rows(txn, executor, updated, check)?;
    }
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, table, updated)
}

/// `ADD CONSTRAINT … PRIMARY KEY (…)` over columns the table already has.
///
/// It **declares** a key: the rows keep whatever identity they were created with, because whether
/// a table has a hidden row-id column is a fact about the stored rows and is read by that column's
/// own name ([`catalog::TableDef::row_id`]). The same property is what lets `DROP CONSTRAINT` take
/// a primary key away.
///
/// The derived name is `<table>_pkey` on the **unqualified** relation, measured:
/// `g1_apk3_pkey` for `g1_apk3` (`tests/captures/pg19_add_column_primary_key.txt`).
fn add_primary_key(updated: &mut TableDef, name: Option<&str>, columns: &[String]) -> Result<()> {
    if !updated.primary_key_name.is_empty() {
        return Err(SqlError::MultiplePrimaryKeys(updated.name.clone()));
    }
    let mut positions = Vec::with_capacity(columns.len());
    for column in columns {
        let at = updated
            .column(column)
            .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                column: column.clone(),
                relation: updated.name.clone(),
            })?;
        positions.push(at);
    }
    // A key column is `NOT NULL` on a real server whether or not the word was written.
    for &at in &positions {
        if let Some(column) = updated.columns.get_mut(at) {
            column.not_null = true;
        }
    }
    updated.primary_key = positions;
    updated.primary_key_name = name.map_or_else(
        || format!("{}_pkey", catalog::split_qualified(&updated.name).1),
        ToOwned::to_owned,
    );
    updated.schema_version += 1;
    Ok(())
}

/// Adds an `EXCLUDE` to a table that already exists, after checking the rows already in it.
///
/// The constraint itself is the one a `CREATE TABLE` builds, from the same clause parser — the
/// only thing an `ALTER` adds is that the rows are there first. A real server validates them and
/// refuses with a sentence of its own, measured:
/// `23P01 could not create exclusion constraint "c"`, whose `DETAIL` says "conflicts with key"
/// where the write path's says "conflicts with existing key".
///
/// **`DEFERRABLE` does not defer this.** Deferral is about the writes that follow, and a
/// constraint added `DEFERRABLE INITIALLY DEFERRED` over conflicting rows is refused at once —
/// measured, and the reason the check is unconditional here.
fn add_exclude(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    updated: &mut TableDef,
    exclude: &catalog::ExcludeDef,
) -> Result<()> {
    if updated
        .excludes
        .iter()
        .any(|seen| seen.name == exclude.name)
        || updated.checks.iter().any(|seen| seen.name == exclude.name)
    {
        return Err(SqlError::DuplicateConstraint {
            constraint: exclude.name.clone(),
            relation: updated.name.clone(),
        });
    }
    validate_exclude_rows(txn, executor, updated, exclude)?;
    // The seventh reader, at the statement that writes one — the same rule the `CHECK` and the
    // partial index above follow, and for the same reason: what is stored is what is printed.
    let mut exclude = exclude.clone();
    if let Some(predicate) = &exclude.predicate
        && let Some(text) = deparse_wrapped_predicate(updated, predicate)
    {
        exclude.predicate = Some(text);
    }
    updated.excludes.push(exclude);
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, table, updated)
}

/// Scans for two stored rows the constraint would refuse, and names it if it finds them.
///
/// Paged for the reason [`validate_check_rows`]'s is: a table that does not fit in memory is a
/// table this must still be able to check. Each row is put to
/// [`super::dml::exclusion_conflict`], which skips the row itself by primary key — so what it
/// finds is a *different* row that conflicts, which is exactly the question.
fn validate_exclude_rows(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    exclude: &catalog::ExcludeDef,
) -> Result<()> {
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut found: Option<SqlError> = None;
    super::for_each_page(txn, &start, &end, |txn, page| {
        for (_, value) in page {
            if found.is_some() {
                return Ok(());
            }
            let row = crate::row::decode_row(&schema, value, None)?;
            if let Some(SqlError::ExclusionViolation {
                constraint,
                key,
                value,
                existing,
            }) = super::dml::exclusion_conflict(txn, executor.tenant, table, exclude, &row)?
            {
                found = Some(SqlError::ExclusionNotCreatable {
                    constraint,
                    key,
                    value,
                    existing,
                });
            }
        }
        Ok(())
    })?;
    found.map_or(Ok(()), Err)
}

/// Scans the table for a row the check refuses, and names the constraint if it finds one.
///
/// The scan is paged for the reason [`backfill`]'s is: a table that does not fit in memory is a
/// table this must still be able to check.
fn validate_check_rows(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    check: &CheckDef,
) -> Result<()> {
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut violated = false;
    super::for_each_page(txn, &start, &end, |_, page| {
        for (_, value) in page {
            if violated {
                return Ok(());
            }
            let row = crate::row::decode_row(&schema, value, None)?;
            let parsed = crate::parse::parse_stored_expr(&check.expr)?;
            let scope = super::query::Scope::single(table);
            let resolved = super::query::resolve(&parsed, &scope)?;
            // Only `false` violates: NULL is unknown and passes, which is the rule every other
            // reader of a `CHECK` in this crate follows.
            violated = matches!(
                super::cursor::evaluate(&resolved, &row)?,
                Datum::Bool(false)
            );
        }
        Ok(())
    })?;
    if violated {
        return Err(SqlError::CheckViolatedByRow {
            constraint: check.name.clone(),
            relation: table.name.clone(),
        });
    }
    Ok(())
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
    // **The rows already there are checked, unless `NOT VALID` says not to.** A real server scans
    // here, and skipping it left a table whose rows contradict a constraint it advertises as
    // validated — reachable from `ADD CONSTRAINT` alone, with no later statement to blame.
    if resolved.validated {
        super::foreign_key::validate(executor, txn, updated, &resolved)?;
    }
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
        // A constraint's key is an index, so it asks the same question `CREATE INDEX` asks.
        for &at in &ordinals {
            refuse_unindexable_type(columns[at].ty, catalog::BTREE_ACCESS_METHOD)?;
        }
        indexes.push(IndexDef {
            id: catalog::allocate_id(txn, executor.tenant)?,
            // A `UNIQUE` constraint's index takes no `USING`, so it is a btree by construction.
            access_method: catalog::BTREE_ACCESS_METHOD.to_owned(),
            name: constraint
                .name
                .clone()
                .unwrap_or_else(|| plan::unique_constraint_name(&create.name, &constraint.columns)),
            unique: true,
            keys: ordinals.into_iter().map(IndexKey::column).collect(),
            nulls_not_distinct: constraint.nulls_not_distinct,
            // It **is** a constraint, which is what separates it from the identical index a
            // `CREATE UNIQUE INDEX` builds: only this one gets a `pg_constraint` row.
            constraint: Some(match (constraint.deferrable, constraint.deferred) {
                (_, true) => catalog::UniqueKind::Deferred,
                (true, false) => catalog::UniqueKind::Deferrable,
                (false, false) => catalog::UniqueKind::Immediate,
            }),
            state: catalog::SchemaState::Public,
            state_since: 1,
            include: Vec::new(),
            predicate: None,
            comment: None,
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
    // **The schema is resolved before anything is written, and a missing one is `3F000`** — not
    // `42704`, and not a silent install into `public`. Measured:
    // `CREATE EXTENSION hstore SCHEMA nosuchschema` is
    // `3F000 schema "nosuchschema" does not exist`.
    //
    // Checked **after** the already-installed arms above, which is the order a real server uses:
    // `CREATE EXTENSION IF NOT EXISTS hstore SCHEMA g1cs` on an installed `hstore` is a notice and
    // **does not move it**, so the schema is never looked at on that path.
    let schema = match &create.schema {
        Some(name) => {
            if !catalog::schema_exists(&*txn, executor.tenant, name)? {
                return Err(SqlError::UndefinedSchema(name.clone()));
            }
            name.clone()
        }
        None => catalog::PUBLIC_SCHEMA.to_owned(),
    };
    catalog::install_extension(txn, executor.tenant, &create.name, version, &schema);
    done
}

/// `DROP EXTENSION [IF EXISTS] <name> [CASCADE]` — what the suite's teardown sends.
///
/// **The verb decides the class**, which is the trap: `CREATE EXTENSION nosuch` is `0A000`
/// (the *server* does not have it) and `DROP EXTENSION nosuch` is `42704` (this *database* has not
/// installed it). Same name, two classes, measured.
///
/// A column of a type the extension provides is `2BP01` without `CASCADE`, with PostgreSQL's own
/// `DETAIL` naming the column and the type. That case is not in the capture and is here because
/// `hstore` and `citext` are column types now: dropping the extension out from under one would
/// leave a column whose type nothing declares.
pub(super) fn drop_extension(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropExtension,
) -> Result<Outcome> {
    let done = Ok(Outcome::done("DROP EXTENSION"));
    if !catalog::pg_catalog::is_installed(txn, executor.tenant, &drop.name)? {
        if drop.if_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "extension",
                name: drop.name.clone(),
            });
            return done;
        }
        return Err(SqlError::UndefinedExtension(drop.name.clone()));
    }
    drop_extension_columns(executor, txn, &drop.name, drop.cascade)?;
    catalog::uninstall_extension(txn, executor.tenant, &drop.name);
    done
}

/// Refuses, or takes with `CASCADE`, every column whose type the extension provides.
///
/// A dropped column is ADR 0051's tombstone, which is what a real server does too: the column goes
/// and the rows are not rewritten. `NOTICE: drop cascades to column c of table ce` is PostgreSQL's
/// own wording for it.
fn drop_extension_columns(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    extension: &str,
    cascade: bool,
) -> Result<()> {
    let types = catalog::pg_catalog::extension_types(extension);
    if types.is_empty() {
        return Ok(());
    }
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    let tables: Vec<TableDef> = relations
        .rows()
        .filter_map(|row| relations.table(row))
        .filter(|table| {
            table
                .live_columns()
                .any(|(_, column)| types.contains(&column.ty))
        })
        .cloned()
        .collect();
    for table in tables {
        let dependent: Vec<String> = table
            .live_columns()
            .filter(|(_, column)| types.contains(&column.ty))
            .map(|(_, column)| column.name.clone())
            .collect();
        if !cascade {
            return Err(SqlError::DependentExtension {
                extension: extension.to_owned(),
                detail: format!(
                    "column {} of table {} depends on type {}",
                    dependent[0],
                    table.name,
                    table
                        .live_column(&dependent[0])
                        .map_or(extension, |(_, column)| {
                            crate::value::PgType::name(column.ty)
                        })
                ),
            });
        }
        let mut updated = table.clone();
        for column in &dependent {
            executor.notice(SqlError::CascadeDropsColumn {
                column: column.clone(),
                relation: table.name.clone(),
            });
            drop_column(
                txn,
                executor,
                &mut updated,
                &table.name,
                column,
                false,
                true,
            )?;
        }
        updated.schema_version += 1;
        catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    }
    Ok(())
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
/// Whether PostgreSQL converts this pair **without** a `USING` — an assignment cast.
///
/// Measured, not derived: `timestamp -> timestamptz` needs no help and `varchar -> timestamp` does,
/// though both are casts that exist. "Needs `USING`" is a property of the *pair*, and the relation
/// is narrower than "a cast is possible".
fn converts_implicitly(from: ColumnType, to: ColumnType) -> bool {
    use ColumnType::{
        Bool, Bpchar, Citext, CitextArray, Date, Double, Int2, Int4, Int8, Interval, Money,
        Numeric, Real, Text, TextArray, Time, Timestamp, TimestampArray, TimestampTz,
        TimestampTzArray, Uuid, Varchar, VarcharArray,
    };
    if from == to {
        return true;
    }
    let string = |ty| matches!(ty, Text | Varchar | Bpchar | Citext);
    // **Every scalar renders into a string, and a string is never a source.** Measured in both
    // directions and for six different families: a number, a timestamp, a date, a boolean, money
    // and a uuid all take `TYPE text` with no `USING`, while `text -> integer`, `text -> date`,
    // `text -> boolean`, `text -> uuid` and `character(5) -> integer` each need one. That
    // asymmetry is the whole shape of PostgreSQL's assignment casts here: rendering a value is
    // always defined, parsing one is not.
    if string(to) {
        return matches!(
            from,
            Int2 | Int4
                | Int8
                | Numeric
                | Double
                | Real
                | Money
                | Date
                | Timestamp
                | TimestampTz
                | Time
                | Interval
                | Bool
                | Uuid
        ) || string(from);
    }
    if string(from) {
        return false;
    }
    let number = |ty| matches!(ty, Int2 | Int4 | Int8 | Numeric | Double | Real);
    // Every number converts to every other number, and to `money`.
    if number(from) && (number(to) || to == Money) {
        return true;
    }
    // **Two edges a "same family" rule gets wrong**, and both were measured rather than reasoned
    // about — the capture is `captures/pg19_alter_type_cast.txt`:
    //
    // * `money -> numeric` converts and **`money -> double precision` does not**, so money is not
    //   simply a member of the numeric family. It converts *from* any number and *to* `numeric`
    //   alone.
    // * `date -> timestamp` converts and **`date -> time` does not**, though `timestamp -> time`
    //   does. A date has no time of day to keep, and PostgreSQL refuses rather than inventing
    //   midnight.
    matches!(
        (from, to),
        (Money, Numeric)
            | (Date | Timestamp | TimestampTz, Date | Timestamp | TimestampTz)
            | (Timestamp | TimestampTz, Time)
            | (Time | Interval, Time | Interval)
            // The array pairs whose element pair is itself implicit. Written out rather than
            // derived: `ColumnType` has one variant per array type and no element accessor, so a
            // rule over elements would have to invent the mapping this list *is*.
            | (TextArray, VarcharArray)
            | (VarcharArray, TextArray)
            | (TextArray | VarcharArray, CitextArray)
            | (CitextArray, TextArray | VarcharArray)
            | (TimestampArray, TimestampTzArray)
            | (TimestampTzArray, TimestampArray)
    )
}

/// Whether a conversion is possible **at all**, with a `USING` to license it.
///
/// Everything an assignment cast covers, plus the pairs that go through the type's own text
/// representation — which is what `CAST(c AS t)` does for every type this node stores.
fn converts_with_using(from: ColumnType, to: ColumnType) -> bool {
    use ColumnType::{Bpchar, Citext, Text, Varchar};
    if converts_implicitly(from, to) {
        return true;
    }
    // **Text is the hub, in both directions.** Every type renders itself into a string and every
    // type reads itself back out of one, so with a `USING` to license it a conversion goes to a
    // string type or comes from one — and the parse is per row, so a value that does not parse is
    // that row's error rather than the statement's.
    //
    // This is the whole of what `USING` buys here, and it is why `USING string_to_array(c, ',')`
    // stays refused: that asks for a computation, not a conversion.
    matches!(from, Text | Varchar | Bpchar | Citext)
        || matches!(to, Text | Varchar | Bpchar | Citext)
}

/// One value, moved from `from` to `to`.
///
/// **Through the type's own text representation**, which is what `CAST` does for these pairs and
/// what keeps this narrow enough to be honest: there is no per-row expression evaluator here, and
/// this is not one — it is the same `to_text`/`from_text` pair the wire protocol uses, so a value
/// converts exactly as it would if the client had sent it to a column of the new type.
///
/// **The string family is relabelled rather than round-tripped.** `text`, `varchar` and `bpchar`
/// share `Datum::Text`, and going through text would be the identity anyway; `citext` and the two
/// timestamps have *distinct* `Datum` variants for the same bytes, so those are re-tagged here —
/// which is a fact about this crate's representation, not about SQL.
fn convert_datum(from: ColumnType, to: ColumnType, value: &Datum) -> Result<Datum> {
    if matches!(value, Datum::Null) || from == to {
        return Ok(value.clone());
    }
    // Same bytes, different tag. Written as a match on the *value* so a type pair that shares no
    // representation still falls through to the text path below.
    let retagged = match (value, to) {
        (Datum::Text(text) | Datum::Citext(text), ColumnType::Citext) => {
            Some(Datum::Citext(text.clone()))
        }
        (
            Datum::Text(text) | Datum::Citext(text),
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar,
        ) => Some(Datum::Text(text.clone())),
        // No zone conversion in either direction, which is what this node's `timestamptz` means
        // (`ColumnType::TimestampTz`): the micros are the value and only the label moves.
        (Datum::Timestamp(micros) | Datum::TimestampTz(micros), ColumnType::TimestampTz) => {
            Some(Datum::TimestampTz(*micros))
        }
        (Datum::Timestamp(micros) | Datum::TimestampTz(micros), ColumnType::Timestamp) => {
            Some(Datum::Timestamp(*micros))
        }
        // **A date becomes midnight**, measured: `2020-01-02` -> `2020-01-02 00:00:00`. Through
        // the text path a date renders without a time and parses back the same way, so this is a
        // retag with arithmetic rather than a re-read.
        (Datum::Date(days), ColumnType::Timestamp) => {
            Some(Datum::Timestamp(i64::from(*days) * MICROS_PER_DAY))
        }
        (Datum::Date(days), ColumnType::TimestampTz) => {
            Some(Datum::TimestampTz(i64::from(*days) * MICROS_PER_DAY))
        }
        // **The time of day, and nothing above it.** `interval '1 day'` becomes `00:00:00` — the
        // months and days are dropped rather than folded in, measured — and a time becomes an
        // interval that is only a time of day.
        (Datum::Interval { micros, .. }, ColumnType::Time) => Some(Datum::Time(*micros)),
        (Datum::Time(micros), ColumnType::Interval) => Some(Datum::Interval {
            months: 0,
            days: 0,
            micros: *micros,
        }),
        // **Money is a scaled integer and renders with a currency symbol**, so the text path
        // cannot read it back as a number: `$16.00` is not numeric input. Measured both ways —
        // `money -> numeric` is `16.00` and `integer 7 -> money` is `$7.00`.
        (Datum::Money(cents), ColumnType::Numeric) => {
            Some(Datum::Numeric(money_as_numeric(*cents)))
        }
        _ => None,
    };
    if let Some(datum) = retagged {
        return Ok(datum);
    }
    // **The rest is the coercion an expression already goes through**, not a second copy of it:
    // `crate::exec::dml::assign_default` is what turns a `now()` into the `date` a column was
    // declared as, and a column changing type asks the identical question of every stored row.
    // Two converters would be two chances to disagree about the same pair — and they did: the
    // rounding rule that function had measured was written down in its comment and missing here,
    // so `double precision -> integer` refused a row PostgreSQL rounds.
    super::dml::assign_default(value.clone(), to)
}

/// Microseconds in a day, which is what a `date` is worth as a `timestamp`.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A `money` value's cents as the `numeric` PostgreSQL converts it to: the same number, scale two.
///
/// **Not a text round trip**, because money renders with a currency symbol and `$16.00` is not
/// numeric input — the text path answered `22P02` for a conversion a real server performs.
fn money_as_numeric(cents: i64) -> esker_keys::numeric::Numeric {
    let sign = if cents < 0 { "-" } else { "" };
    let text = format!("{sign}{}.{:02}", (cents / 100).abs(), (cents % 100).abs());
    crate::value::numeric::from_text(&text)
        .unwrap_or_else(|_| crate::value::numeric::of_i64(cents / 100))
}

/// `ALTER TABLE … ALTER COLUMN … TYPE <type> [USING …]` — and it rewrites every row.
///
/// A row is stored positionally and decoded against the table's *current* schema (ADR 0030), so a
/// column that changes type and leaves its rows alone makes every row already written decode as
/// the wrong value. The whole table is read and written back in the statement's own transaction,
/// which is the same trade [`backfill`] makes and for the same reason.
///
/// The order of the checks is PostgreSQL's, and it is observable: the **pair** is rejected before
/// any row is read (`42804` with the `USING` to write), the **default** before that (`42804`
/// naming the default), and only then can a row fail on its own value.
/// The four things `ALTER COLUMN … TYPE` carries, together because they are one clause.
struct TypeChange<'a> {
    /// The target type.
    ty: ColumnType,
    /// Its `atttypmod`, or `-1`.
    typmod: i32,
    /// The type a `USING` casts the column to, when the statement wrote one.
    using: Option<ColumnType>,
    /// A `USING` that is not a cast: the expression, evaluated once per row.
    using_expr: Option<&'a plan::Expr>,
    /// The collation the statement named, or `None` to take the new type's own.
    collation: Option<&'a str>,
}

fn set_column_type(
    txn: &mut dyn Txn,
    executor: &Executor,
    updated: &mut TableDef,
    column: &str,
    change: &TypeChange<'_>,
) -> Result<()> {
    let &TypeChange {
        ty,
        typmod,
        using,
        using_expr,
        collation,
    } = change;
    let at = updated
        .column(column)
        .ok_or_else(|| SqlError::UndefinedColumnInRelation {
            column: column.to_owned(),
            relation: updated.name.clone(),
        })?;
    let from = updated.columns[at].ty;
    let target = crate::value::format_type(ty, typmod);
    // **Two hops when a `USING` names its own type**: the column has to reach the `USING`'s type,
    // and that type has to reach the column's new one on its own — which is why
    // `TYPE character varying USING s::text` works and is not the same statement as
    // `TYPE character varying` alone.
    // **An arbitrary `USING` is checked per row and not here**, because its type is not knowable
    // before it runs: `string_to_array(snippets, ',')` answers `text[]` whatever `snippets` is.
    // PostgreSQL is the same — a `USING` whose result does not fit raises on the row it does not
    // fit on, which is why the pre-flight below is skipped rather than approximated.
    let allowed = match (using, using_expr) {
        (_, Some(_)) => true,
        (None, None) => converts_implicitly(from, ty),
        (Some(cast_to), None) => {
            converts_with_using(from, cast_to) && converts_implicitly(cast_to, ty)
        }
    };
    if !allowed {
        return Err(SqlError::CannotCastColumnAutomatically {
            column: column.to_owned(),
            target: target.clone(),
            using: format!("{column}::{target}"),
        });
    }
    // **The default converts or the statement stops**, before any row is touched — and it is held
    // to the *assignment* cast, not to whether its particular value happens to parse. A `varchar`
    // column defaulted to `'0'` going to `integer` is refused even though `'0'` is a fine integer,
    // because `USING` governs the rows and says nothing about the default. Measured on two
    // independent pairs, and the `SET DEFAULT` later in the same statement does not rescue it.
    if updated.columns[at].default.is_some() && !converts_implicitly(from, ty) {
        return Err(SqlError::CannotCastDefaultAutomatically {
            column: column.to_owned(),
            target,
        });
    }

    // Every row, a page at a time, decoded against the old schema and written back under the new
    // one. Collected before anything is written: the walk holds the transaction.
    let (start, end) = crate::row::table_row_range(executor.tenant, updated.id);
    let schema = updated.row_schema();
    let mut rows: Vec<(Vec<u8>, Vec<Datum>)> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            rows.push((key.to_vec(), crate::row::decode_row(&schema, value, None)?));
        }
        Ok(())
    })?;

    // The column's own values move with it: a default and a missing value are stored as data and
    // would otherwise be read back under the new type without ever having been converted.
    let default = match &updated.columns[at].default {
        Some(value) => Some(convert_datum(from, ty, value)?),
        None => None,
    };
    let missing = match &updated.columns[at].missing {
        Some(value) => Some(convert_datum(from, ty, value)?),
        None => None,
    };
    // Kept before the column moves, for the `USING` expression's scope below.
    let before = updated.clone();
    updated.columns[at].ty = ty;
    updated.columns[at].typmod = typmod;
    // **Assigned and not merged.** A statement with no `COLLATE` gives the column its new type's
    // collation, so one that had `COLLATE "C"` and is retyped without a clause goes back to the
    // default — which is what PostgreSQL does, and what keeping the old name here would not.
    updated.columns[at].collation = collation.map(ToOwned::to_owned);
    updated.columns[at].default = default;
    updated.columns[at].missing = missing;
    let types = updated.column_types();
    // **Resolved against the table as it was**, so the expression's column references have the
    // types the rows still carry: `snippets` is a `varchar` inside
    // `string_to_array(snippets, ',')`, and resolving it against the new `text[]` column would
    // type the argument as the answer.
    let resolved = match using_expr {
        Some(expr) => Some(
            // Its own error, not an internal one: `USING nosuch` is the user's `42703`, measured.
            super::query::resolve(expr, &super::query::Scope::single(&before))?,
        ),
        None => None,
    };
    for (key, mut row) in rows {
        // The typmod is applied per row and not compared once: `varchar(5)` over a nineteen
        // character value is `22001`, and which row raises it depends on the data. `fit_to_typmod`
        // is the same function an `INSERT` uses, so a rounded `timestamp(6)` rounds identically.
        let value = match &resolved {
            // **The expression sees the whole row**, not only the column being retyped — a `USING`
            // may name any column of the table, which is the half a per-value conversion cannot do.
            Some(resolved) => using_result_into(
                super::cursor::evaluate_in_txn(resolved, &row, &*txn)?,
                column,
                ty,
                &target,
                executor.rendering(),
            )?,
            None => convert_datum(from, ty, &row[at])?,
        };
        row[at] = crate::value::fit_to_typmod(value, ty, typmod)?;
        txn.put(&key, &crate::row::encode_row(&types, &row)?);
    }
    Ok(())
}

/// The evaluated `USING` value as the column's own — the assignment cast a real server applies to
/// the result — or the `42804` it raises for a result it cannot cast on its own.
///
/// `ALTER COLUMN c TYPE bigint USING length(c)` is an `int4` result into a `bigint` column, which
/// used to reach `encode_row` and be refused as a codec mismatch; `… TYPE integer USING c || 'x'`
/// is a `text` result with no cast to `integer`, which is `42804 result of USING clause for column
/// "c" cannot be cast automatically to type integer` on a real server, HINT included. Both measured.
fn using_result_into(
    value: Datum,
    column: &str,
    ty: ColumnType,
    target: &str,
    rendering: crate::value::Rendering,
) -> Result<Datum> {
    if value.fits(ty) {
        return Ok(value);
    }
    if crate::value::has_assignment_cast(value.column_type(), ty) {
        return crate::value::assignment_cast(value, ty, rendering);
    }
    Err(SqlError::UsingResultCannotBeCast {
        column: column.to_owned(),
        target: target.to_owned(),
    })
}

/// `ALTER TABLE … VALIDATE CONSTRAINT <name>` — the second half of `NOT VALID`.
///
/// It runs the scan the `ADD` skipped and, when every row satisfies the constraint, records it as
/// validated. **Validating a constraint that is already valid is a success**, measured, and so is
/// validating one that was never `NOT VALID`; only a name the table does not have is an error, and
/// it names the relation.
fn validate_constraint(
    txn: &mut dyn Txn,
    executor: &Executor,
    updated: &mut TableDef,
    name: &str,
) -> Result<()> {
    // **A check is validated by name too**, and it is looked for first because the two namespaces
    // are one: `ALTER TABLE … VALIDATE CONSTRAINT` takes any constraint's name and a table cannot
    // hold two of one name.
    if let Some(at) = updated.checks.iter().position(|check| check.name == name) {
        if updated.checks[at].validated {
            return Ok(());
        }
        let check = updated.checks[at].clone();
        validate_check_rows(txn, executor, updated, &check)?;
        updated.checks[at].validated = true;
        return Ok(());
    }
    let at = updated
        .foreign_keys
        .iter()
        .position(|key| key.name == name)
        .ok_or_else(|| SqlError::UndefinedConstraint {
            constraint: name.to_owned(),
            relation: updated.name.clone(),
        })?;
    if updated.foreign_keys[at].validated {
        return Ok(());
    }
    let key = updated.foreign_keys[at].clone();
    super::foreign_key::validate(executor, txn, updated, &key)?;
    updated.foreign_keys[at].validated = true;
    Ok(())
}

/// `ALTER COLUMN … SET NOT NULL` / `DROP NOT NULL` — what `change_column_null` sends.
///
/// **`SET NOT NULL` reads the table.** PostgreSQL scans for a NULL before it writes the flag and
/// refuses `23502` if it finds one; a node that set the flag regardless would leave rows that
/// contradict their own catalog and answer the next `INSERT … VALUES (NULL)` differently from the
/// rows already stored. The scan is paged for the reason [`backfill`]'s is.
///
/// Both directions are **idempotent** — setting a flag that is set, or dropping one that is not,
/// is a success — which is what makes `change_column_null` safe to re-run.
fn set_column_not_null(
    txn: &mut dyn Txn,
    executor: &Executor,
    updated: &mut TableDef,
    column: &str,
    not_null: bool,
) -> Result<()> {
    let at = updated
        .column(column)
        .ok_or_else(|| SqlError::UndefinedColumnInRelation {
            column: column.to_owned(),
            relation: updated.name.clone(),
        })?;
    // The primary key's own `NOT NULL` is not the column's to drop.
    if !not_null && updated.primary_key.contains(&at) {
        return Err(SqlError::ColumnIsInPrimaryKey(column.to_owned()));
    }
    if not_null && !updated.columns[at].not_null {
        let (start, end) = crate::row::table_row_range(executor.tenant, updated.id);
        let schema = updated.row_schema();
        super::for_each_page(txn, &start, &end, |_, page| {
            for (_, value) in page {
                let row = crate::row::decode_row(&schema, value, None)?;
                if matches!(row.get(at), Some(Datum::Null)) {
                    return Err(SqlError::ColumnContainsNulls {
                        column: column.to_owned(),
                        relation: updated.name.clone(),
                    });
                }
            }
            Ok(())
        })?;
    }
    updated.columns[at].not_null = not_null;
    Ok(())
}

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
            let typmod = updated.columns[at].typmod;
            let written = expr.as_deref().unwrap_or("NULL");
            let (folded, unfolded) = crate::parse::fold_column_default(written, ty, typmod)?;
            updated.columns[at].default = folded;
            // The third of `deparse_default`'s three callers. `SET DEFAULT (1 + 2 * 3)` prints
            // `(1 + (2 * 3))` for the same reason the `CREATE TABLE` spelling does: one row, one
            // printer, and the statement that wrote it is not recorded anywhere.
            updated.columns[at].default_expr = unfolded
                .as_ref()
                .and_then(|expr| deparse_default(updated, expr))
                .or(unfolded);
        }
    }
    Ok(())
}

/// **A `PRIMARY KEY` on a partitioned table is the unique-index rule again**, in PostgreSQL's own
/// two sentences with the word substituted.
///
/// Checked here rather than in [`create_index`] because this is the *inline* spelling, the one a
/// `CREATE TABLE` carries — and checked before the table is written, so a refusal leaves nothing
/// behind. A key this node **derived** (the internal row id) is not a declared one and is not
/// checked: nothing was written for it to miss.
fn refuse_uncovered_primary_key(
    create: &CreateTable,
    partition_by: Option<&catalog::PartitionKey>,
    primary_key: &[usize],
    columns: &[ColumnDef],
) -> Result<()> {
    let Some(key) = partition_by else {
        return Ok(());
    };
    if create.primary_key.is_empty() {
        return Ok(());
    }
    if let Some(&missing) = key.columns.iter().find(|at| !primary_key.contains(at)) {
        return Err(SqlError::PartitionKeyNotCovered {
            kind: "PRIMARY KEY",
            relation: create.name.clone(),
            missing: columns[missing].name.clone(),
        });
    }
    Ok(())
}

/// The parent's primary key column names, for a partition that declares none of its own.
///
/// `None` for a table that is not a partition, and for a partition whose parent has no declared
/// key either — that parent carries an **internal row id** instead, and the partition gets one of
/// its own at the same position, which keeps the two the same width.
///
/// By **name**, because the ordinals are the parent's: a partition takes the parent's columns and
/// may not lay them out identically.
fn partition_primary_key(
    create: &CreateTable,
    parent: Option<&std::sync::Arc<TableDef>>,
) -> Option<Vec<String>> {
    let parent = parent.filter(|_| create.partition_of.is_some())?;
    if parent.row_id().is_some() {
        return None;
    }
    Some(
        parent
            .primary_key
            .iter()
            .filter_map(|&at| parent.columns.get(at).map(|column| column.name.clone()))
            .collect(),
    )
}

/// **A partition takes the parent's columns and declares none of its own.**
///
/// The suite writes `CREATE TABLE "measurements_toronto" PARTITION OF measurements FOR VALUES IN
/// (1)` with no column list at all. It is the same borrowing an `INHERITS` child does, which is
/// why the two share the edge and `pg_inherits` reports a partition too — and why a table that is
/// not a partition passes straight through with what it already had.
fn partition_of_columns(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateTable,
    parents: Vec<std::sync::Arc<TableDef>>,
    columns: Vec<ColumnDef>,
) -> Result<(Vec<std::sync::Arc<TableDef>>, Vec<ColumnDef>)> {
    let Some((parent, _)) = &create.partition_of else {
        return Ok((parents, columns));
    };
    let parent = executor.require_table(txn, parent)?;
    let mut inherited: Vec<ColumnDef> = parent
        .user_columns()
        .map(|(_, column)| column.clone())
        .collect();
    for column in columns {
        if !inherited.iter().any(|held| held.name == column.name) {
            inherited.push(column);
        }
    }
    Ok((vec![parent], inherited))
}

/// `INCLUDE (…)` resolved against the table's columns.
///
/// **The plain column-not-found wording**, not the `column "…" named in key does not exist`
/// phrasing a key column gets: an included column is not in the key, and PostgreSQL says so by
/// falling back to its ordinary message. Measured.
///
/// **Not deduplicated against the key**, either: `("firm_id") INCLUDE ("firm_id")` is accepted and
/// reports the same attnum twice in `indkey`. Nothing here removes it.
fn included_columns(table: &TableDef, names: &[String]) -> Result<Vec<usize>> {
    names
        .iter()
        .map(|name| {
            table
                .column(name)
                .ok_or_else(|| SqlError::UndefinedColumn(name.clone()))
        })
        .collect()
}

/// **A unique index on a partitioned table must contain every partition column**, or `0A000`.
///
/// PostgreSQL's own words and its own reason: with the key columns in it, two rows that could
/// collide must route to the *same* partition, so a per-partition index enforces the constraint
/// exactly. Without one they could land in different partitions and neither index would see the
/// pair. `kind` is the word the message and its `DETAIL` are built from — `UNIQUE` or
/// `PRIMARY KEY`, which a real server substitutes into the same two sentences.
fn refuse_uncovered_partition_key(
    table: &TableDef,
    keys: &[IndexKey],
    kind: &'static str,
) -> Result<()> {
    let Some(key) = &table.partition_by else {
        return Ok(());
    };
    // The **first** column it misses is the one PostgreSQL names in the `DETAIL`.
    let missing = key
        .columns
        .iter()
        .find(|&&at| !keys.iter().any(|part| part.position() == Some(at)));
    if let Some(&at) = missing {
        return Err(SqlError::PartitionKeyNotCovered {
            kind,
            relation: table.name.clone(),
            missing: table.columns[at].name.clone(),
        });
    }
    Ok(())
}

/// **Every partition silently gets its own copy of the parent's indexes.**
///
/// Statement 782 creates the index before a single partition exists, and each partition made
/// afterwards acquires one named `<partition>_<cols>_idx` — two partitions produce two indexes the
/// suite never named, and it is the *child* index a duplicate-key error names. Measured.
///
/// A per-partition index is not an approximation of a partitioned one: a unique index on a
/// partitioned table must contain every partition column, so two rows that could collide route to
/// the same partition and one index sees both.
fn copy_parent_indexes(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &mut TableDef,
    parents: &[std::sync::Arc<TableDef>],
) -> Result<()> {
    for parent in parents {
        for index in &parent.indexes {
            let Some(columns) = index.key_columns() else {
                continue;
            };
            let keys: Vec<plan::IndexKeyPart> = columns
                .iter()
                .filter_map(|&at| parent.columns.get(at))
                .map(|column| plan::IndexKeyPart::column(column.name.clone()))
                .collect();
            let mine = columns
                .iter()
                .filter_map(|&at| parent.columns.get(at))
                .filter_map(|column| table.column(&column.name))
                .map(IndexKey::column)
                .collect::<Vec<_>>();
            if mine.len() != columns.len() {
                continue;
            }
            table.indexes.push(IndexDef {
                id: catalog::allocate_id(txn, executor.tenant)?,
                name: plan::index_name(&table.name, &keys),
                unique: index.unique,
                access_method: catalog::BTREE_ACCESS_METHOD.to_owned(),
                keys: mine,
                include: Vec::new(),
                predicate: index.predicate.clone(),
                nulls_not_distinct: index.nulls_not_distinct,
                // The child of a constraint's index is not itself a constraint: only the
                // partitioned table's own row is in `pg_constraint`.
                constraint: None,
                state: catalog::SchemaState::Public,
                state_since: 1,
                comment: None,
            });
        }
    }
    Ok(())
}

/// `PARTITION BY LIST (col, …)` — the key columns resolved against the table's own.
fn partition_key(
    create: &CreateTable,
    columns: &[ColumnDef],
) -> Result<Option<catalog::PartitionKey>> {
    let Some((strategy, names)) = &create.partition_by else {
        return Ok(None);
    };
    let key = names
        .iter()
        .map(|name| {
            columns
                .iter()
                .position(|column| &column.name == name)
                .ok_or_else(|| SqlError::UndefinedColumnInKey(name.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(catalog::PartitionKey {
        strategy: *strategy,
        columns: key,
    }))
}

/// `FOR VALUES IN (…)` or `DEFAULT` — coerced to the parent's key types, and checked for overlap.
///
/// **The coercion is observable.** The suite writes `FOR VALUES IN (1)` against a
/// `character varying` key and a real server prints the bound back as `FOR VALUES IN ('1')`. A
/// bound stored as the literal was written would diverge the moment `ActiveRecord` dumps the
/// schema — and would compare wrongly against a routed row, which reaches the key as text.
fn partition_bound(
    executor: &Executor,
    txn: &dyn Txn,
    create: &CreateTable,
    parent: Option<&std::sync::Arc<TableDef>>,
) -> Result<Option<catalog::PartitionBound>> {
    let Some((parent_name, spec)) = &create.partition_of else {
        return Ok(None);
    };
    let Some(parent) = parent else {
        return Err(SqlError::UndefinedTable(parent_name.clone()));
    };
    // A table that is not partitioned has no bounds to take: PostgreSQL's own message names the
    // relation rather than the clause.
    let Some(key) = &parent.partition_by else {
        return Err(SqlError::NotPartitioned(parent.name.clone()));
    };
    let coerce = |value: &Datum, at: usize| -> Result<Datum> {
        let ty = parent.columns[at].ty;
        let text = crate::value::PgDatum::to_text(value).unwrap_or_default();
        Datum::from_text(ty, &text)
    };
    let bound = match spec {
        plan::PartitionSpec::Default => catalog::PartitionBound::Default,
        plan::PartitionSpec::Values(values) => {
            if values.len() != key.columns.len() {
                return Err(SqlError::unsupported(
                    "FOR VALUES IN over a different number of columns than the key",
                ));
            }
            catalog::PartitionBound::Values(
                values
                    .iter()
                    .zip(&key.columns)
                    .map(|(value, &at)| coerce(value, at))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        plan::PartitionSpec::Range { from, to } => {
            if from.len() != key.columns.len() || to.len() != key.columns.len() {
                return Err(SqlError::unsupported(
                    "FOR VALUES FROM ... TO ... over a different number of columns than the key",
                ));
            }
            let ends = |side: &[plan::RangeEnd]| -> Result<Vec<catalog::RangeBound>> {
                side.iter()
                    .zip(&key.columns)
                    .map(|(end, &at)| {
                        Ok(match end {
                            plan::RangeEnd::MinValue => catalog::RangeBound::MinValue,
                            plan::RangeEnd::MaxValue => catalog::RangeBound::MaxValue,
                            plan::RangeEnd::Value(value) => {
                                catalog::RangeBound::Value(coerce(value, at)?)
                            }
                        })
                    })
                    .collect()
            };
            catalog::PartitionBound::Range {
                from: ends(from)?,
                to: ends(to)?,
            }
        }
    };
    // **An overlapping bound is `42P17`, and the message names the partition it would overlap.**
    // Checked against the parent's existing partitions, which is where the answer is: two `DEFAULT`
    // partitions overlap as surely as two identical value lists.
    for &sibling_id in &parent.children {
        let sibling = executor.table_by_id(txn, sibling_id)?;
        let Some(theirs) = &sibling.partition_bound else {
            continue;
        };
        if bounds_overlap(&bound, theirs) {
            return Err(SqlError::PartitionOverlap {
                partition: create.name.clone(),
                existing: sibling.name.clone(),
            });
        }
    }
    Ok(Some(bound))
}

/// Whether two bounds admit any row in common.
///
/// Two `DEFAULT`s do — a table may have only one — and two value lists do when they share a value.
/// A `DEFAULT` and anything else never do: `DEFAULT` takes what the others do not, by definition.
///
/// Two ranges overlap when each starts before the other ends, which is the **half-open** test:
/// `FROM (MINVALUE) TO (10)` and `FROM (10) TO (MAXVALUE)` share the number `10` and do not
/// overlap, because the first excludes its upper end. Measured — the capture creates both.
///
/// A list and a range cannot meet, because one strategy is a table's and a partition of it cannot
/// be declared with the other's grammar.
fn bounds_overlap(one: &catalog::PartitionBound, other: &catalog::PartitionBound) -> bool {
    use catalog::PartitionBound::{Default, Range, Values};
    match (one, other) {
        (Default, Default) => true,
        (Values(ours), Values(theirs)) => ours.iter().any(|value| theirs.contains(value)),
        (
            Range { from, to },
            Range {
                from: their_from,
                to: their_to,
            },
        ) => {
            catalog::RangeBound::cmp_bound(&from[0], &their_to[0]).is_lt()
                && catalog::RangeBound::cmp_bound(&their_from[0], &to[0]).is_lt()
        }
        // A `DEFAULT` and anything else; and a list against a range, which cannot happen because
        // the strategy is the parent's and both partitions are declared against the same one.
        _ => false,
    }
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

/// The first trigger that names this function, and the table it is on.
fn trigger_naming(
    executor: &Executor,
    txn: &dyn Txn,
    function: &str,
) -> Result<Option<(String, String)>> {
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    for table in relations.tables() {
        if let Some(trigger) = table
            .triggers
            .iter()
            .find(|trigger| trigger.function == function)
        {
            return Ok(Some((trigger.name.clone(), table.name.clone())));
        }
    }
    Ok(None)
}

/// `CREATE [OR REPLACE] FUNCTION f() RETURNS TRIGGER AS $$…$$ LANGUAGE plpgsql`.
///
/// **Defined, never executed.** The body is stored verbatim and nothing parses it: the schema load
/// reaches this twice — statement 762's `INHERITS` block and statement 790 — and inserts nothing
/// through either, so what the suite needs is a catalog that can hold a function. Exactly one test
/// in the whole suite fires a trigger, and a plpgsql runtime stays out of scope.
///
/// **Run twice it is a plain success**, not `42710`: `OR REPLACE` exists for a function and the
/// second `CREATE` simply overwrites. A second `CREATE TRIGGER` of one name *is* a duplicate.
pub(super) fn create_function(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateFunction,
) -> Result<Outcome> {
    // **The two trusted languages a real server has**, measured: `pg_language` holds `c`,
    // `internal`, `plpgsql` and `sql`, and only the last two are `lanpltrusted`. The other two
    // name a shared object or a built-in symbol, neither of which exists here, so a function in
    // one has nothing to be — `42704` for those is a truer answer than storing a body that names
    // a file this node will never open.
    //
    // It knows nothing else about either, and that is the point: a body it cannot run is still a
    // body it can keep, and `uuid_test.rb` needs exactly that — a `LANGUAGE SQL` function whose
    // name has to survive into a column default the schema dumper prints back.
    if !matches!(
        create.language.to_ascii_lowercase().as_str(),
        "plpgsql" | "sql"
    ) {
        return Err(SqlError::UndefinedLanguage(create.language.clone()));
    }
    let id = match catalog::function(txn, executor.tenant, &create.name)? {
        // Replacing keeps the id, so a trigger that named it still names the same relation.
        Some(existing) => existing.id,
        None => catalog::allocate_id(txn, executor.tenant)?,
    };
    catalog::write_function(
        txn,
        executor.tenant,
        &catalog::FunctionDef {
            id,
            name: create.name.clone(),
            body: create.body.clone(),
            language: create.language.clone(),
        },
    )?;
    Ok(Outcome::done("CREATE FUNCTION"))
}

/// `CREATE TRIGGER t BEFORE|AFTER … ON tbl FOR EACH ROW EXECUTE FUNCTION|PROCEDURE f()`.
///
/// Registered on the table and **never fired**. The function must already exist — a real server
/// resolves it here, and a name that is nothing is `42883` from the `CREATE TRIGGER` rather than
/// from a later insert.
pub(super) fn create_trigger(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateTrigger,
) -> Result<Outcome> {
    catalog::pg_catalog::refuse_write(&create.table)?;
    let table = executor.require_table(txn, &create.table)?;
    if catalog::function(txn, executor.tenant, &create.function)?.is_none() {
        return Err(SqlError::UndefinedFunction(format!(
            "{}()",
            create.function
        )));
    }
    // **A trigger's name is unique per table**, not per database, which is what `pg_trigger` is
    // keyed by — and a second one of the same name on the same table is `42710`.
    if table
        .triggers
        .iter()
        .any(|trigger| trigger.name == create.name)
    {
        return Err(SqlError::DuplicateTrigger {
            trigger: create.name.clone(),
            relation: table.name.clone(),
        });
    }
    let mut updated = (*table).clone();
    updated.triggers.push(catalog::TriggerDef {
        name: create.name.clone(),
        before: create.before,
        events: create.events,
        for_each_row: create.for_each_row,
        function: create.function.clone(),
        enabled: true,
    });
    // The schema version moves because a cached `TableDef` would not list it — `pg_trigger` reads
    // the table, and a node holding the old one would report the trigger missing.
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    Ok(Outcome::done("CREATE TRIGGER"))
}

/// `DROP TRIGGER [IF EXISTS] t ON tbl`.
pub(super) fn drop_trigger(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropTrigger,
) -> Result<Outcome> {
    let table = executor.require_table(txn, &drop.table)?;
    if !table
        .triggers
        .iter()
        .any(|trigger| trigger.name == drop.name)
    {
        if drop.if_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "trigger",
                name: drop.name.clone(),
            });
            return Ok(Outcome::done("DROP TRIGGER"));
        }
        return Err(SqlError::UndefinedTrigger {
            trigger: drop.name.clone(),
            table: table.name.clone(),
        });
    }
    let mut updated = (*table).clone();
    updated.triggers.retain(|trigger| trigger.name != drop.name);
    updated.schema_version += 1;
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    Ok(Outcome::done("DROP TRIGGER"))
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

/// The first column whose `DEFAULT` names this function, with the table it is on.
///
/// **Read out of the stored text**, the same way `dependent_views` reads a view's body: a default
/// is kept as the expression `pg_get_expr` prints, and the name is in it. A function call is the
/// only shape that can name one, so the parse is what finds it rather than a substring match — a
/// column defaulting to `'my_uuid_generator()'::text` names nothing.
fn default_naming(
    executor: &Executor,
    txn: &dyn Txn,
    function: &str,
) -> Result<Option<(String, String)>> {
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    for table in relations.tables() {
        for column in table.user_columns().map(|(_, column)| column) {
            let Some(text) = &column.default_expr else {
                continue;
            };
            let Ok(mut parsed) = crate::parse::parse_stored_expr(text) else {
                continue;
            };
            // **The call is looked for, not a parse failure.** This asked whether the expression
            // *failed to parse* naming the function, which was true only while a stored function
            // could not be resolved at all — the moment one could be, every default stopped
            // looking like a dependency and `DROP FUNCTION` took the function out from under a
            // table whose every insert then failed. The dependency is a call in the tree, and
            // that is what this now reads.
            let mut names_it = false;
            let _ = super::subquery::walk_mut(&mut parsed, &mut |expr| {
                if let plan::Expr::CatalogFunc(call) = &*expr
                    && call.func == plan::CatalogFunc::UserFunc
                    && let Some(plan::Expr::Literal(plan::Literal::String(named))) =
                        call.args.first()
                    && named == function
                {
                    names_it = true;
                }
                Ok(())
            });
            if names_it {
                return Ok(Some((column.name.clone(), table.name.clone())));
            }
        }
    }
    Ok(None)
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
pub(super) fn drop_function(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropFunction,
) -> Result<Outcome> {
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
        // A **stored** function: this node has those now, and dropping one is ordinary — unless a
        // trigger still names it, which is `2BP01` with the trigger and its table in the DETAIL.
        if let Some(stored) = catalog::function(txn, executor.tenant, name)? {
            // **A column default is a dependency too**, and it is the one this node could
            // acquire without noticing: a default that names a function is stored as *text*, so
            // dropping the function leaves a table whose every insert then fails. Measured —
            // `2BP01`, with the column and its table in the `DETAIL`, exactly as the trigger edge
            // below reports the trigger and its table.
            if let Some((column, table)) = default_naming(executor, txn, name)? {
                return Err(SqlError::DependentFunction {
                    function: format!("{name}()"),
                    detail: format!(
                        "default value for column {column} of table {table} depends on function \
                         {name}()"
                    ),
                });
            }
            if let Some((trigger, table)) = trigger_naming(executor, txn, name)? {
                return Err(SqlError::DependentFunction {
                    function: format!("{name}()"),
                    detail: format!(
                        "trigger {trigger} on table {table} depends on function {name}()"
                    ),
                });
            }
            catalog::drop_function(txn, executor.tenant, &stored.name)?;
            continue;
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
        // Nothing has been handed out yet, which is what a fresh sequence reports: `last_value` is
        // the start value and `is_called` is false, so the first `nextval` answers the start.
        false,
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
            Some(catalog::Relation::View { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a sequence",
                    found: "DROP VIEW",
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
    // **A permanent table may not reference an unlogged one**, and the rule is one-directional:
    // the reverse is accepted, because losing the child's rows on a crash breaks nothing about the
    // parent while the other way round leaves a constraint pointing at rows that are gone.
    // Measured both ways; a symmetric check would refuse half the statements a real server takes.
    if child.persistence == catalog::Persistence::Permanent
        && parent_def.persistence == catalog::Persistence::Unlogged
    {
        return Err(SqlError::PermanentReferencesUnlogged);
    }
    Ok(catalog::ForeignKeyDef {
        name: key.name.clone(),
        columns,
        parent: parent_def.id,
        parent_columns,
        on_update: key.on_update,
        on_delete: key.on_delete,
        validated: key.validated,
        deferrable: key.deferrable,
        initially_deferred: key.initially_deferred,
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

/// `ALTER TABLE ... DROP [COLUMN] [IF EXISTS] <name> [CASCADE]` — the tombstone and what goes with
/// it (ADR 0051). Answers whether anything changed, which is `false` only for `IF EXISTS` on a
/// column that is not there.
///
/// **The column's slot, type and missing value are left exactly as they are**, and that is the
/// whole design: a row is decoded by position, so the type has to stay for the bytes of every row
/// written before this statement to be read at the right offsets. What is cleared is only what a
/// user could see — the flag is what hides it.
///
/// Everything that lives *on this table* goes silently and needs no `CASCADE`: an index over the
/// column of any width, the column's `CHECK`, its `NOT NULL`, its default, its comment, a foreign
/// key declared on it, and the sequence it owned. Only a dependent living on another object
/// raises `2BP01`, and here that is another table's foreign key referencing the column. All
/// measured against 19beta1.
/// Drops one column and writes the table back, for a `CASCADE` that reaches it from elsewhere.
///
/// `DROP TYPE … CASCADE` takes the columns declared as the type
/// ([ADR 0065](../../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)),
/// and it arrives with a table rather than an `ALTER TABLE` statement — so this is the same
/// [`drop_column`] the statement uses, with the write around it.
pub(super) fn drop_column_cascading(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    column: &str,
) -> Result<()> {
    let mut updated = (*table).clone();
    let name = table.name.clone();
    if drop_column(txn, executor, &mut updated, &name, column, false, true)? {
        catalog::replace_table(txn, executor.tenant, table, &updated)?;
        executor.catalog_written = true;
    }
    Ok(())
}

fn drop_column(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    updated: &mut TableDef,
    relation: &str,
    name: &str,
    if_exists: bool,
    cascade: bool,
) -> Result<bool> {
    let Some(at) = updated.column(name) else {
        if if_exists {
            executor.notice(SqlError::UndefinedColumnInRelation {
                column: name.to_owned(),
                relation: relation.to_owned(),
            });
            return Ok(false);
        }
        return Err(SqlError::UndefinedColumnInRelation {
            column: name.to_owned(),
            relation: relation.to_owned(),
        });
    };

    // **Before anything is changed**, so a refusal leaves the definition as it was.
    refuse_referencing_keys(txn, executor, updated, name, at, cascade)?;

    // **A generated column that reads this one is a dependent**, and it is inside the same table —
    // which makes it the cheapest edge of the three to find and the easiest to forget. The message
    // names both columns.
    if let Some(dependent) = generated_columns_reading(updated, name)
        && !cascade
    {
        return Err(SqlError::ViewDependsOnRelation {
            kind: "column",
            name: format!(
                "{} of table {}",
                catalog::display_name(name),
                catalog::display_name(relation)
            ),
            detail: format!(
                "column {} of table {} depends on column {} of table {}",
                catalog::display_name(&dependent),
                catalog::display_name(relation),
                catalog::display_name(name),
                catalog::display_name(relation)
            ),
        });
    }

    // A view that reads this column is a dependent too, and the message names the column rather
    // than the table. This was the recorded debt on the other side of `tests/drop_column.rs`'s two
    // divergence entries: the column went and the view was left reading one that is gone.
    let dependents = views_depending_on_column(executor, txn, relation, name)?;
    if let Some(view) = dependents.first()
        && !cascade
    {
        return Err(SqlError::ViewDependsOnRelation {
            kind: "column",
            name: format!(
                "{} of table {}",
                catalog::display_name(name),
                catalog::display_name(relation)
            ),
            detail: format!(
                "view {} depends on column {} of table {}",
                catalog::display_name(view),
                catalog::display_name(name),
                catalog::display_name(relation)
            ),
        });
    }
    drop_views_cascading(executor, txn, dependents)?;

    // The tombstone. `ty`, `typmod` and `missing` survive because the row codec reads them;
    // everything else was a promise to a user who can no longer see the column.
    let column = &mut updated.columns[at];
    column.dropped = true;
    column.not_null = false;
    column.default = None;
    column.default_expr = None;
    column.generated = None;
    column.comment = None;

    // An index over the column cannot be maintained once nothing writes the column again, and a
    // real server drops it whether the column is one of its keys or all of them.
    updated
        .indexes
        .retain(|index| !index.keys.iter().any(|key| key.position() == Some(at)));

    // The primary key goes the same way, and the table stays — measured: after
    // `ALTER TABLE dc DROP COLUMN id` the table has no `p` constraint and still answers
    // `SELECT count(*)`.
    if updated.primary_key.contains(&at) {
        updated.primary_key.clear();
        updated.primary_key_name.clear();
        updated.primary_key_comment = None;
    }

    // A foreign key **declared on** the column: on this table, so it goes silently. This is the
    // half that is not `refuse_referencing_keys`'s, and the two are easy to confuse.
    //
    // **And its back-reference goes with it**, which is the half run 50 found missing: the
    // constraint lives in this record and the `(parent, child)` key lives under the *parent*, so
    // dropping one and not the other leaves a parent that outlives the constraint still being told
    // it is referenced. `DROP TABLE` says the same thing about its own keys and was the only place
    // that had to, until a column could take a constraint with it.
    let parents: Vec<u64> = updated
        .foreign_keys
        .iter()
        .filter(|key| key.columns.contains(&at))
        .map(|key| key.parent)
        .collect();
    updated
        .foreign_keys
        .retain(|key| !key.columns.contains(&at));
    for parent in parents {
        forget_backref_if_last(txn, executor.tenant, updated, parent);
    }

    // The sequence the column owned. `serial` makes one and the column owns it, so
    // `DROP COLUMN "id"` takes `dc_id_seq` with it — which does not follow from the statement's
    // wording and is measured.
    updated
        .sequences
        .retain(|sequence| sequence.column != Some(at));

    // A `CHECK` or an `EXCLUDE` is stored as **text**, so what decides whether it depended on the
    // column is whether it still resolves against the table now that the name is gone. That reuses
    // `validate_checks`'s own machinery rather than adding a second walker over the same
    // expressions, which is what would drift.
    let scope_table = updated.clone();
    updated
        .checks
        .retain(|check| resolves(&scope_table, &check.expr));
    updated.excludes.retain(|exclude| {
        resolves(&scope_table, &exclude.key)
            && exclude
                .predicate
                .as_deref()
                .is_none_or(|predicate| resolves(&scope_table, predicate))
    });

    Ok(true)
}

/// `ALTER TABLE … ADD CONSTRAINT <name> UNIQUE (…)`.
///
/// Builds the same index `CREATE UNIQUE INDEX` would and marks it as a **constraint**, which is the
/// only difference between them and the one that decides which statement can remove it. The rows
/// already stored are not checked here for the same reason `CREATE UNIQUE INDEX` does not: an index
/// that is `Public` from the start is this node's declared trade
/// (`crate::catalog::SchemaState`), and a duplicate surfaces at the next write.
/// `ADD CONSTRAINT [name] UNIQUE USING INDEX <index>` — promote an index that already exists.
///
/// **Nothing is built and nothing is backfilled.** A unique constraint here *is* an [`IndexDef`]
/// with its `constraint` field set, so this sets that field on the index the statement names. The
/// index is already there and already filled, which is the whole reason a client writes this
/// spelling: `add_index … unique: true` then `add_unique_constraint … using_index:` is
/// `ActiveRecord`'s way of promoting an index it built earlier without paying for a second one.
///
/// Measured on 19beta1, and the second line is the one an implementation would not guess:
///
/// ```text
/// the constraint takes the columns of the named index
/// the INDEX IS RENAMED to the constraint's name, and the server says so:
///     NOTICE:  ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index "unique_index"
///              to "unique_constraint"
/// and when the two names are already equal there is no rename and no notice
/// ```
fn promote_index_to_constraint(
    executor: &Executor,
    updated: &mut TableDef,
    promote: &plan::UniqueUsingIndex,
) -> Result<()> {
    let name = promote
        .name
        .clone()
        .unwrap_or_else(|| promote.index.clone());
    // The name has to be free unless it is the index's own — promoting `ui` to a constraint called
    // `ui` is a rename to where it already is.
    if name != promote.index
        && (updated.indexes.iter().any(|index| index.name == name)
            || updated.checks.iter().any(|check| check.name == name)
            || updated.foreign_keys.iter().any(|key| key.name == name))
    {
        return Err(SqlError::DuplicateConstraint {
            constraint: name,
            relation: updated.name.clone(),
        });
    }
    let kind = match (promote.deferrable, promote.deferred) {
        (_, true) => catalog::UniqueKind::Deferred,
        (true, false) => catalog::UniqueKind::Deferrable,
        (false, false) => catalog::UniqueKind::Immediate,
    };
    let Some(index) = updated
        .indexes
        .iter_mut()
        .find(|index| index.name == promote.index)
    else {
        return Err(SqlError::UndefinedIndex(promote.index.clone()));
    };
    if !index.unique {
        return Err(SqlError::IndexNotUnique(promote.index.clone()));
    }
    if index.name != name {
        executor.notice(SqlError::Raised {
            message: format!(
                "ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index \"{}\" to \"{name}\"",
                index.name
            ),
            severity: crate::error::Severity::Notice,
        });
        index.name = name;
    }
    index.constraint = Some(kind);
    Ok(())
}

fn add_unique_constraint(
    txn: &mut dyn Txn,
    executor: &Executor,
    updated: &mut TableDef,
    constraint: &plan::UniqueConstraint,
) -> Result<()> {
    let name = constraint
        .name
        .clone()
        .unwrap_or_else(|| plan::unique_constraint_name(&updated.name, &constraint.columns));
    if updated.indexes.iter().any(|index| index.name == name)
        || updated.checks.iter().any(|check| check.name == name)
        || updated.foreign_keys.iter().any(|key| key.name == name)
    {
        return Err(SqlError::DuplicateConstraint {
            constraint: name,
            relation: updated.name.clone(),
        });
    }
    let ordinals = constraint
        .columns
        .iter()
        .map(|column| {
            updated
                .column(column)
                .ok_or_else(|| SqlError::UndefinedColumnInKey(column.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    updated.indexes.push(IndexDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name,
        unique: true,
        access_method: catalog::BTREE_ACCESS_METHOD.to_owned(),
        keys: ordinals.into_iter().map(IndexKey::column).collect(),
        nulls_not_distinct: constraint.nulls_not_distinct,
        constraint: Some(match (constraint.deferrable, constraint.deferred) {
            (_, true) => catalog::UniqueKind::Deferred,
            (true, false) => catalog::UniqueKind::Deferrable,
            (false, false) => catalog::UniqueKind::Immediate,
        }),
        state: catalog::SchemaState::Public,
        state_since: updated.schema_version + 1,
        include: Vec::new(),
        predicate: None,
        comment: None,
    });
    // **The index has to be filled, not merely declared.** Without this the constraint was a
    // catalog entry over an empty index: the rows already in the table were not in it, so a later
    // duplicate of one of them was accepted *and* a `SELECT … WHERE a = <that value>` answered
    // with only the new row. A wrong answer to an ordinary query, reached through a statement that
    // succeeded — and the scan that fixes it is the same one that refuses a constraint the
    // existing rows already break (ADR 0020's recorded gap).
    let index = updated
        .indexes
        .last()
        .ok_or_else(|| SqlError::Internal("the index just pushed is not there".to_owned()))?
        .clone();
    backfill(executor, txn, updated, &index)
}

/// The derived name, or the first one with a numbered label that nothing answers to.
///
/// **Only for names this node derives.** PostgreSQL uniquifies a name it made up and refuses one
/// the user gave, and the difference matters: a migration that renames a table and recreates the
/// old name expects the second key to be numbered, while a `CONSTRAINT c` that collides is a
/// mistake worth reporting. Measured: after a rename, a new table's key is `<table>_pkey1`.
///
/// The parts are passed **unjoined** because the counter goes on the label and the name is then
/// rebuilt from them — see [`plan::choose_relation_name`], which is where that is measured.
fn free_derived_name(
    txn: &dyn Txn,
    executor: &Executor,
    name1: &str,
    name2: Option<&str>,
    label: &str,
) -> Result<String> {
    plan::choose_relation_name(name1, name2, label, |candidate| {
        catalog::name_exists(txn, executor.tenant, candidate)
    })
}

/// `ALTER INDEX <name> RENAME TO <name>`.
///
/// **Renaming an index renames its constraint**, and here that is not two writes but one: a
/// `UNIQUE` constraint and the index it owns are one `IndexDef::name`, and a primary key's index is
/// `TableDef::primary_key_name`. A real server keeps two catalog rows that share a name and moves
/// both; this node keeps one field, so the `pg_constraint` row follows by construction.
///
/// A name already taken is **`42P07`** — `relation "…" already exists`, because an index shares a
/// namespace with tables and sequences — and a name nothing answers to is `42P01`, the *relation*
/// message rather than an index-specific one. Both measured.
pub(super) fn alter_index_rename(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    rename: &plan::AlterIndexRename,
) -> Result<Outcome> {
    let done = Ok(Outcome::done("ALTER INDEX"));
    // **The collision is looked for in the index's own schema**, which is the schema the name it
    // is being renamed *from* resolves in. A bare check asked `public`, so renaming an index
    // inside a schema to a name `public` already held was `42P07` — `SchemaWithDotsTest`'s
    // `ALTER INDEX "posts_pkey" RENAME TO "articles_pkey"`, where `articles_pkey` is a suite
    // fixture's key that is always there.
    //
    // The source is resolved first and only to learn its schema; a name that resolves to nothing
    // comes back unchanged, so the check falls back to exactly what it asked before and the
    // `42P01` below still wins for a source that is not there.
    let from = executor.resolve_unqualified(&*txn, &rename.name)?;
    let target = catalog::qualify(catalog::split_qualified(&from).0, &rename.to);
    if catalog::name_exists(&*txn, executor.tenant, &target)? {
        return Err(SqlError::DuplicateTable(rename.to.clone()));
    }
    let Some(relation) = existing_relation(executor, txn, &rename.name)? else {
        if rename.if_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "relation",
                name: rename.name.clone(),
            });
            return done;
        }
        return Err(SqlError::UndefinedTable(rename.name.clone()));
    };
    // **A real server renames a *table* through this statement** — `ALTER INDEX` is the generic
    // rename wearing another keyword, measured. Nothing the suite sends does it, and accepting it
    // here would mean this arm quietly doing `ALTER TABLE`'s job; it is named instead, and the
    // corpus records the divergence.
    let (catalog::Relation::Index { table_id, .. } | catalog::Relation::PrimaryKey { table_id }) =
        relation
    else {
        return Err(SqlError::unsupported(format!(
            "ALTER INDEX naming {}, which is not an index",
            rename.name
        )));
    };
    let table = executor.table_by_id(txn, table_id)?;
    let mut updated = (*table).clone();
    // **Compared bare to bare.** A *derived* name is stored qualified — `plan::make_object_name`
    // spends its byte budget on the identifier and re-qualifies, so `s.t`'s key is `s\0t_pkey` —
    // while a name the user gave is stored as they wrote it. The statement names one index either
    // way, so the schema is taken off both sides and the comparison is between identifiers.
    let wanted = catalog::split_qualified(&from).1.to_owned();
    let matches = |name: &str| catalog::split_qualified(name).1 == wanted;
    if matches(&updated.primary_key_name) && !updated.primary_key_name.is_empty() {
        updated.primary_key_name.clear();
        updated.primary_key_name.push_str(&rename.to);
    } else if let Some(index) = updated
        .indexes
        .iter_mut()
        .find(|index| matches(&index.name))
    {
        index.name.clear();
        index.name.push_str(&rename.to);
    } else {
        return Err(SqlError::UndefinedTable(rename.name.clone()));
    }
    updated.schema_version += 1;
    // The old name record goes and the new one is written here, because `replace_table` reconciles
    // an index's name and the primary key's — the rule three separate leaks taught it.
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    done
}

/// `ALTER TABLE … RENAME COLUMN <from> TO <to>`.
///
/// **Only `attname` changes.** The column keeps its ordinal, so every index, constraint, default
/// and primary key goes on pointing at the same attribute — they all reference a column by
/// *position* here — and everything that renders a definition renders the new name for free. That
/// is what makes this a one-field write rather than a rewrite, and it is what a real server does
/// too.
///
/// Two failure modes and they are different codes: a column that is not there is `42703` with the
/// short sentence (no relation named), and a name already taken is `42701` — **including renaming
/// a column to the name it already has**, measured.
fn rename_column(updated: &mut TableDef, relation: &str, from: &str, to: &str) -> Result<()> {
    let Some(at) = updated.column(from) else {
        return Err(SqlError::UndefinedColumn(from.to_owned()));
    };
    if updated.live_column(to).is_some() {
        return Err(SqlError::DuplicateColumnInRelation {
            column: to.to_owned(),
            relation: relation.to_owned(),
        });
    }
    // **A stored expression names the column as text and would not follow it.** A `CHECK`, an
    // `EXCLUDE` key or an index predicate is kept as the text the user wrote and re-lowered per
    // write, so renaming a column one of them mentions would leave a constraint that cannot
    // resolve — a table that stops accepting rows, which is worse than a refusal. A real server
    // re-renders these from the attnum and does not care; this node names the constraint and
    // refuses until the same is true here. `DROP COLUMN` finds its dependents the same way.
    let mut renamed = updated.clone();
    renamed.columns[at].name.clear();
    renamed.columns[at].name.push_str(to);
    if let Some(named) = unresolvable_after(&renamed) {
        return Err(SqlError::unsupported(format!(
            "renaming {from}, which {named} is written in terms of"
        )));
    }
    updated.columns[at].name.clear();
    updated.columns[at].name.push_str(to);
    Ok(())
}

/// The first stored expression that no longer resolves against the table, by the name of what
/// carries it.
fn unresolvable_after(table: &TableDef) -> Option<String> {
    for check in &table.checks {
        if !resolves(table, &check.expr) {
            return Some(format!("the constraint {}", check.name));
        }
    }
    for exclude in &table.excludes {
        if !resolves(table, &exclude.key)
            || exclude
                .predicate
                .as_deref()
                .is_some_and(|predicate| !resolves(table, predicate))
        {
            return Some(format!("the constraint {}", exclude.name));
        }
    }
    for index in &table.indexes {
        if index
            .predicate
            .as_deref()
            .is_some_and(|predicate| !resolves(table, predicate))
        {
            return Some(format!("the index {}", index.name));
        }
    }
    None
}

/// `ALTER TABLE … RENAME TO <name>` — the table itself.
///
/// **The indexes and the sequence keep their own names**, measured: after renaming `rc` to `rc2`
/// the indexes are still `index_rc_on_name` and the sequence still `rc_id_seq`, which is why
/// `ActiveRecord` follows this with an explicit `ALTER TABLE <seq> RENAME TO` (`:474`). A node that
/// renamed them helpfully would answer that follow-up with `42P01`.
///
/// The old name record is removed by `catalog::replace_table`, which reconciles a table's name the
/// way it reconciles an index's — the rule the primary key's leak taught it.
fn rename_table(
    txn: &mut dyn Txn,
    executor: &Executor,
    updated: &mut TableDef,
    to: &str,
) -> Result<()> {
    // **The new name is bare and the stored one carries the schema**, so it is rebuilt in the
    // table's *own* schema before it is either checked or written. Using `to` as the stored name
    // was two bugs on two lines and they looked like different faults:
    //
    // * the collision check asked **`public`**, because a bare stored name *is* `public`
    //   (`catalog::qualify`), so a table in another schema could not be renamed to any name the
    //   fixtures happened to hold — which is `SchemaWithDotsTest`'s head error, tapped off the
    //   wire: `ALTER TABLE "posts" RENAME TO "articles"` answering
    //   `42P07 relation "articles" already exists` against a fixture in `public`;
    // * and when `public` held no such name the rename *succeeded* and **moved the table into
    //   `public`**, silently, because the bare name it stored is what a `public` relation's name
    //   looks like. That is why the harness lane saw no `my.schema.articles` afterwards **and no
    //   error** — one cause, two symptoms that read like a contradiction.
    //
    // Measured on 19beta1 with `public.g1v54_art` deliberately present: the rename is accepted,
    // `g1.v54.g1v54_art` exists afterwards and `public.g1v54_art` is untouched. The check itself is
    // right and stays — a second rename onto a name **this** schema holds is `42P07` — it was only
    // ever asking the wrong schema.
    let target = catalog::qualify(catalog::split_qualified(&updated.name).0, to);
    if catalog::name_exists(&*txn, executor.tenant, &target)? {
        // The message names the relation **bare**, which is what a real server prints even for a
        // collision inside a named schema: `relation "g1v54_art" already exists`.
        return Err(SqlError::DuplicateTable(to.to_owned()));
    }
    updated.name.clear();
    updated.name.push_str(&target);
    Ok(())
}

/// `ALTER TABLE … DROP CONSTRAINT [IF EXISTS] <name> [CASCADE]`. Answers whether anything changed,
/// which is `false` only for `IF EXISTS` on a name that is nothing.
///
/// **Six kinds, and the search order is not arbitrary.** A `UNIQUE` *constraint* and a
/// `CREATE UNIQUE INDEX` build the same index and `IndexDef::constraint` is the only thing that
/// tells them apart — an index without one is not a constraint and must reach the `42704`, which
/// is the distinction the capture spends four lines on. `NOT NULL` is last because its name is
/// derived rather than stored, so a real constraint of the same spelling wins.
fn drop_constraint(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    updated: &mut TableDef,
    relation: &str,
    name: &str,
    if_exists: bool,
    cascade: bool,
) -> Result<bool> {
    // A `CHECK`, the plain case: nothing owns it and nothing depends on it.
    if let Some(at) = updated.checks.iter().position(|check| check.name == name) {
        updated.checks.remove(at);
        return Ok(true);
    }
    // An `EXCLUDE`, which owns a synthesised relation and no stored index (ADR 0031's EXCLUDE note).
    if let Some(at) = updated
        .excludes
        .iter()
        .position(|exclude| exclude.name == name)
    {
        updated.excludes.remove(at);
        return Ok(true);
    }
    // A `FOREIGN KEY` — what `remove_foreign_key` sends. **Its back-reference goes with it**, and
    // only once nothing else of this table's points at that parent: the key is per
    // `(parent, child)` pair, which is the rule run 50's regression established.
    if let Some(at) = updated.foreign_keys.iter().position(|key| key.name == name) {
        let parent = updated.foreign_keys[at].parent;
        updated.foreign_keys.remove(at);
        forget_backref_if_last(txn, executor.tenant, updated, parent);
        return Ok(true);
    }
    // A `UNIQUE` constraint. **Its index goes with it, silently** — measured — and an index that
    // is not a constraint is not found here at all.
    if let Some(at) = updated
        .indexes
        .iter()
        .position(|index| index.name == name && index.constraint.is_some())
    {
        updated.indexes.remove(at);
        return Ok(true);
    }
    // The `PRIMARY KEY`, which another table's foreign key can depend on — the one kind here with
    // a dependent outside its own table, and so the only one `CASCADE` means anything for.
    if !updated.primary_key_name.is_empty() && updated.primary_key_name == name {
        refuse_keys_on_the_primary(txn, executor, updated, name, cascade)?;
        updated.primary_key.clear();
        updated.primary_key_name.clear();
        updated.primary_key_comment = None;
        // The index behind it goes too, the way a `UNIQUE` constraint's does.
        updated.indexes.retain(|index| index.name != name);
        return Ok(true);
    }
    // `NOT NULL`, which **is** a droppable constraint in PostgreSQL 19: every such column has its
    // own `pg_constraint` row named `<table>_<column>_not_null`, and dropping it clears
    // `attnotnull` exactly as `ALTER COLUMN … DROP NOT NULL` does.
    let not_null_at = updated
        .live_columns()
        .find(|(_, column)| column.not_null && not_null_constraint_name(updated, column) == name)
        .map(|(at, _)| at);
    if let Some(at) = not_null_at {
        updated.columns[at].not_null = false;
        return Ok(true);
    }

    if if_exists {
        executor.notice(SqlError::UndefinedConstraintSkipping {
            constraint: name.to_owned(),
            relation: relation.to_owned(),
        });
        return Ok(false);
    }
    Err(SqlError::UndefinedConstraint {
        constraint: name.to_owned(),
        relation: relation.to_owned(),
    })
}

/// The name a `NOT NULL` column's constraint has: `<table>_<column>_not_null`, PostgreSQL's own
/// derivation and the one `crate::catalog::pg_constraint` reports.
fn not_null_constraint_name(table: &TableDef, column: &ColumnDef) -> String {
    format!("{}_{}_not_null", table.name, column.name)
}

/// `2BP01` when another table's foreign key depends on this table's primary key, unless `CASCADE`.
///
/// PostgreSQL's sentence names the **index** the foreign key needs rather than the constraint,
/// which is the shape of the dependency: a referencing key is validated through the unique index
/// the primary key owns.
fn refuse_keys_on_the_primary(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    table: &TableDef,
    name: &str,
    cascade: bool,
) -> Result<()> {
    let (start, end) = catalog::foreign_key_backref_range(executor.tenant, table.id);
    for (key, _) in txn.scan(&start, &end, 0)? {
        let child_id = catalog::foreign_key_backref_child(executor.tenant, table.id, &key)?;
        if child_id == table.id {
            continue;
        }
        let child = executor.table_by_id(txn, child_id)?;
        let depends: Vec<String> = child
            .foreign_keys
            .iter()
            .filter(|constraint| constraint.parent == table.id)
            .map(|constraint| constraint.name.clone())
            .collect();
        if depends.is_empty() {
            continue;
        }
        if !cascade {
            return Err(SqlError::DependentConstraint {
                constraint: name.to_owned(),
                relation: table.name.clone(),
                detail: format!(
                    "constraint {} on table {} depends on index {name}",
                    depends[0], child.name
                ),
            });
        }
        // **The key goes and the column stays** — measured: the referencing table keeps `dcp_id`.
        let mut without = (*child).clone();
        without
            .foreign_keys
            .retain(|constraint| !depends.contains(&constraint.name));
        without.schema_version += 1;
        catalog::replace_table(txn, executor.tenant, &child, &without)?;
        forget_backref_if_last(txn, executor.tenant, &without, table.id);
    }
    Ok(())
}

/// Deletes a child's back-reference to one parent, **only once nothing points there any more**.
///
/// The key is `(parent, child)` and not `(parent, child, constraint)`, so a child holding two
/// foreign keys to one parent has **one** back-reference between them. Deleting it while the second
/// still points there would tell the parent it is unreferenced, and the `DROP TABLE` that should
/// have been `2BP01` would go through instead — a wrong answer rather than a stale key. Call this
/// after the constraints have been removed from `child`, which is what makes the test right.
fn forget_backref_if_last(txn: &mut dyn Txn, tenant: u64, child: &TableDef, parent: u64) {
    if !child.foreign_keys.iter().any(|key| key.parent == parent) {
        txn.delete(&catalog::foreign_key_backref_key(tenant, parent, child.id));
    }
}

/// Whether a stored expression still names only columns this table has.
///
/// A parse failure counts as *not* resolving, which is the safe direction here: a constraint whose
/// text cannot be read is one nothing can evaluate, and leaving it behind would fail the next
/// `INSERT` rather than this `ALTER`.
fn resolves(table: &TableDef, expr: &str) -> bool {
    let Ok(parsed) = crate::parse::parse_stored_expr(expr) else {
        return false;
    };
    let scope = crate::exec::query::Scope::single(table);
    crate::exec::query::resolve(&parsed, &scope).is_ok()
}

/// `2BP01` when another table's foreign key references the column, unless `CASCADE`.
///
/// The backref index is the same one `DROP TABLE` walks, so this costs a range scan over the
/// children that reference this table rather than a scan of every table.
fn refuse_referencing_keys(
    txn: &mut dyn Txn,
    executor: &mut Executor,
    table: &TableDef,
    name: &str,
    at: usize,
    cascade: bool,
) -> Result<()> {
    let (start, end) = catalog::foreign_key_backref_range(executor.tenant, table.id);
    for (key, _) in txn.scan(&start, &end, 0)? {
        let child_id = catalog::foreign_key_backref_child(executor.tenant, table.id, &key)?;
        if child_id == table.id {
            continue;
        }
        let child = executor.table_by_id(txn, child_id)?;
        let depends: Vec<String> = child
            .foreign_keys
            .iter()
            .filter(|constraint| {
                constraint.parent == table.id && constraint.parent_columns.contains(&at)
            })
            .map(|constraint| constraint.name.clone())
            .collect();
        if depends.is_empty() {
            continue;
        }
        if !cascade {
            return Err(SqlError::DependentColumn {
                column: name.to_owned(),
                relation: table.name.clone(),
                detail: format!(
                    "constraint {} on table {} depends on column {name} of table {}",
                    depends[0], child.name, table.name
                ),
            });
        }
        // **All of them in one rewrite**, the way `DROP TABLE`'s cascade does it: a child may hold
        // two keys into this column, and replacing the record once per constraint would write the
        // second rewrite over a `child` read before the first.
        let mut without = (*child).clone();
        without
            .foreign_keys
            .retain(|key| !depends.contains(&key.name));
        without.schema_version += 1;
        catalog::replace_table(txn, executor.tenant, &child, &without)?;
        forget_backref_if_last(txn, executor.tenant, &without, table.id);
    }
    Ok(())
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
    let mut sequences: Vec<catalog::SequenceDef> = Vec::new();
    for (ordinal, column) in create.columns.iter().enumerate() {
        if let Some(identity) = column.sequence {
            // **A derived sequence name that is taken is numbered, not refused** — this is
            // `CollidedSequenceNameTest`, where `foo_bar (baz_id serial)` and
            // `foo (bar_baz_id bigserial)` both derive `foo_bar_baz_id_seq` and the second gets
            // `foo_bar_baz_id_seq1`. The first column keeps the plain name, which the test
            // asserts directly.
            //
            // The names chosen earlier in *this* statement count as taken: `foo` derives
            // `foo_bar_id_seq` for one column and `foo_bar_baz_id_seq` for another, and neither
            // is in the catalog yet. A real server creates the sequences one at a time before the
            // table, which is the order this loop runs in.
            let name =
                plan::choose_relation_name(&create.name, Some(&column.name), "seq", |candidate| {
                    if sequences.iter().any(|chosen| chosen.name == candidate) {
                        return Ok(true);
                    }
                    catalog::name_exists(&*txn, executor.tenant, candidate)
                })?;
            sequences.push(catalog::SequenceDef {
                id: catalog::allocate_id(txn, executor.tenant)?,
                name,
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

/// A `CREATE TABLE` whose name has been put in the schema it will live in.
///
/// A qualified name is left alone; an unqualified one takes the first schema of the `search_path`
/// that resolves, which is `public` when none does — so every existing statement lands exactly
/// where it did.
fn qualified_create(
    txn: &dyn Txn,
    executor: &Executor,
    create: &CreateTable,
) -> Result<CreateTable> {
    if create.name.contains(catalog::SCHEMA_SEPARATOR) {
        return Ok(create.clone());
    }
    let schema = executor.creation_schema(txn)?;
    if schema == catalog::PUBLIC_SCHEMA {
        return Ok(create.clone());
    }
    let mut qualified = create.clone();
    qualified.name = catalog::qualify(&schema, &create.name);
    Ok(qualified)
}

/// The schema a relation names must exist: `3F000`, **before** anything else is looked at.
///
/// **Its own class, not `42P01`.** `CREATE TABLE nosuchschema.t` fails on the *schema* — measured
/// — where a relation missing from a schema that is there is `42P01`. A relation in `public`
/// passes straight through, which is what keeps every existing answer unchanged.
fn refuse_missing_schema(txn: &dyn Txn, executor: &Executor, stored: &str) -> Result<()> {
    let (schema, name) = catalog::split_qualified(stored);
    // **Nothing may be created in the two schemas the catalog owns**, and the sentence is its own:
    // `42501 permission denied to create "pg_catalog.mine"`, the whole qualified name inside the
    // quotes. It is the other half of `pg_catalog::refuse_write` — that one guards a write to a
    // relation that *is* a catalog, this one a write to a schema that is — and it is what makes
    // the qualifier a lookup key that no record can ever carry.
    if catalog::is_reserved_schema(schema) {
        return Err(SqlError::CreateInSystemSchema(format!("{schema}.{name}")));
    }
    if catalog::schema_exists(txn, executor.tenant, schema)? {
        return Ok(());
    }
    Err(SqlError::UndefinedSchema(schema.to_owned()))
}

/// `CREATE SCHEMA [IF NOT EXISTS] name`.
///
/// **A second namespace, and nothing is in it yet.** A relation still lives in `public`, so a
/// schema-qualified relation name is refused by name where this node cannot resolve it — a gap
/// rather than a wrong answer, which is what keeps `CREATE SCHEMA` honest before the relations
/// follow it.
pub(super) fn create_schema(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateSchema,
) -> Result<Outcome> {
    // **The `pg_` prefix is refused before existence is even asked about**, which is why this
    // comes first and why `information_schema` — same reservation, no prefix — falls through to
    // the ordinary `42P06`. Measured, both, and `IF NOT EXISTS` does not cover this one: the name
    // is unacceptable rather than taken.
    if create.name.starts_with("pg_") {
        return Err(SqlError::ReservedSchemaName(create.name.clone()));
    }
    // **`AUTHORIZATION u` requires `u` to be a role**, which is the half of ownership a client can
    // tell apart: the statement fails on a name that is not there. What is *not* recorded is who
    // owns the schema afterwards — `pg_namespace` has no owner column and nothing here enforces
    // one (`docs/plans/roles-and-user-schemas.md`).
    if let Some(owner) = &create.owner
        && catalog::role_by_name(&*txn, owner)?.is_none()
    {
        return Err(SqlError::UndefinedRole(owner.clone()));
    }
    if catalog::schema_exists(&*txn, executor.tenant, &create.name)? {
        // **`IF NOT EXISTS` is a notice and a success**, which is what a real server answers; the
        // notice itself is on stderr in `psql` and is not a row.
        if create.if_not_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "schema",
                name: create.name.clone(),
            });
            return Ok(Outcome::done("CREATE SCHEMA"));
        }
        return Err(SqlError::DuplicateSchema(create.name.clone()));
    }
    let id = catalog::allocate_id(txn, executor.tenant)?;
    catalog::create_schema(txn, executor.tenant, &create.name, id)?;
    Ok(Outcome::done("CREATE SCHEMA"))
}

/// `CREATE DATABASE [IF NOT EXISTS] name` — a second tenant, and a row in the cluster's directory.
///
/// **Outside a transaction block**, which the executor checked before it got here: a database is
/// state outside every transaction, so a block that could roll this back would be a block that
/// could roll back half a schema change. PostgreSQL's `25001`, measured.
pub(super) fn create_database(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateDatabase,
) -> Result<Outcome> {
    if catalog::database_id(&*txn, &create.name)?.is_some() {
        // `IF NOT EXISTS` is a notice and a success, the way `CREATE SCHEMA`'s is.
        if create.if_not_exists {
            executor.notice(SqlError::DoesNotExistSkipping {
                kind: "database",
                name: create.name.clone(),
            });
            return Ok(Outcome::done("CREATE DATABASE"));
        }
        return Err(SqlError::DuplicateDatabase(create.name.clone()));
    }
    // **`TEMPLATE` is the one option whose value names something in the catalog**, so it is
    // decided here and not where the rest of the list is read. A database this node creates is
    // empty, and an empty copy of an empty template is exact — so what it cannot do is copy a
    // template that holds anything, and that is refused by name rather than answered with an empty
    // database somebody asked to be a copy.
    if let Some(template) = &create.template {
        let Some(id) = catalog::database_id(&*txn, template)? else {
            return Err(SqlError::UndefinedTemplateDatabase(template.clone()));
        };
        if catalog::has_relations(&*txn, id)? {
            return Err(SqlError::unsupported(format!(
                "CREATE DATABASE ... TEMPLATE {template}, which is not empty"
            )));
        }
    }
    let id = catalog::allocate_database_id(txn)?;
    catalog::create_database(txn, &create.name, id)?;
    Ok(Outcome::done("CREATE DATABASE"))
}

/// `DROP DATABASE [IF EXISTS] name`, and everything the tenant behind it held.
///
/// **`IF EXISTS` covers absence and nothing else.** The database this session is connected to is
/// still `55006` with the clause written, because it is there rather than missing — the same
/// distinction `DROP SCHEMA` draws between a schema that is absent and one that is depended on.
pub(super) fn drop_database(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropDatabase,
) -> Result<Outcome> {
    for name in &drop.names {
        let Some(id) = catalog::database_id(&*txn, name)? else {
            if drop.if_exists {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "database",
                    name: name.clone(),
                });
                continue;
            }
            return Err(SqlError::UndefinedDatabase(name.clone()));
        };
        // **By id, not by name.** The session knows which tenant it is serving and nothing else;
        // comparing names would answer wrongly the moment two spellings reach one database.
        if id == executor.tenant {
            return Err(SqlError::DatabaseInUse(name.clone()));
        }
        // A template is there rather than missing and is not a dependency violation either, so it
        // is its own class — `42809`, measured. `IF EXISTS` does not cover it, for the same reason
        // it does not cover the open database.
        if catalog::is_template_database(name) {
            return Err(SqlError::CannotDropTemplateDatabase);
        }
        catalog::drop_database(txn, name, id)?;
    }
    Ok(Outcome::done("DROP DATABASE"))
}

/// `CREATE [OR REPLACE] VIEW name [(cols)] AS SELECT …`.
///
/// **The definition is lowered once, here, and then stored as text.** Lowering it proves the
/// `SELECT` is one this node can run and that every relation it names exists — PostgreSQL resolves
/// a view's body at creation too, which is why `CREATE VIEW v AS SELECT * FROM nosuch` is `42P01`
/// rather than a view that fails later. What is kept is the text, for the reason
/// [`plan::CreateView`] gives.
pub(super) fn create_view(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateView,
) -> Result<Outcome> {
    catalog::pg_catalog::refuse_write(&create.name)?;
    // The schema a bare name lands in, the same rule `CREATE TABLE` follows.
    let name = if create.name.contains(catalog::SCHEMA_SEPARATOR) {
        create.name.clone()
    } else {
        let schema = executor.creation_schema(txn)?;
        catalog::qualify(&schema, &create.name)
    };
    let existing = existing_relation(executor, txn, &name)?;
    match existing {
        // `OR REPLACE` over a view replaces it; without it the name is taken and that is `42P07`,
        // the same answer a second `CREATE TABLE` of the name gets.
        Some(catalog::Relation::View { .. }) if create.or_replace => {}
        Some(catalog::Relation::View { .. }) => {
            return Err(SqlError::DuplicateTable(create.name.clone()));
        }
        // **`OR REPLACE` does not replace a table with a view.** Measured: `CREATE TABLE v` over a
        // view is `42P07 relation "v" already exists`, and the mirror of it is this.
        Some(_) => return Err(SqlError::DuplicateTable(create.name.clone())),
        None => {}
    }
    // Proves the body before it is stored, and gives the shape its columns are checked against.
    let shape = view_shape(executor, txn, &create.definition, &create.columns)?;
    // **A replacement may append columns and may not rename or drop one.** Measured on 19beta1;
    // this node accepted both, which is a wrong answer rather than a missing feature — a silent
    // rename breaks every query written against the old name. Surfaced by the view corpus once
    // writing through a view stopped aborting the block, so the statements after it were compared
    // for the first time.
    //
    // A view stored before record version 30 publishes no columns at all, and there is nothing to
    // compare it against; such a replacement is allowed rather than refused on missing evidence.
    if create.or_replace
        && let Some(previous) = catalog::view(txn, executor.tenant, &name)?
        && !previous.columns.is_empty()
    {
        if shape.len() < previous.columns.len() {
            return Err(SqlError::CannotDropViewColumns);
        }
        for (before, after) in previous.columns.iter().zip(&shape) {
            if before.name != after.name {
                return Err(SqlError::CannotRenameViewColumn {
                    from: before.name.clone(),
                    to: after.name.clone(),
                });
            }
        }
    }
    let id = catalog::allocate_id(txn, executor.tenant)?;
    catalog::create_view(
        txn,
        executor.tenant,
        &catalog::ViewDef {
            id,
            name,
            definition: create.definition.clone(),
            columns: shape,
        },
    )?;
    executor.catalog_written = true;
    Ok(Outcome::done("CREATE VIEW"))
}

/// The names a view's columns will have: the ones it was declared with, or the ones its own
/// `SELECT` produces.
///
/// **A column list that does not match the query's width is `42P10`** on a real server, and it has
/// to be caught here rather than where the view is read: a stored view whose list is the wrong
/// length would be a relation whose shape is a lie.
fn view_shape(
    executor: &Executor,
    txn: &dyn Txn,
    definition: &str,
    declared: &[String],
) -> Result<Vec<catalog::ViewColumn>> {
    let parsed = crate::parse::parse_statements(definition)?;
    let [statement] = parsed.as_slice() else {
        return Err(SqlError::unsupported(
            "a view definition that is more than one statement",
        ));
    };
    let plan::Statement::Select(select) = statement.lower()? else {
        return Err(SqlError::unsupported(
            "a view definition that is not a SELECT",
        ));
    };
    let planned = executor.plan_select(txn, &select)?;
    // **The types come from the same plan the names do.** A view's column is whatever its
    // expression evaluates to, and the planner has already decided that for the row description it
    // would send — so nothing here re-derives it and the two can never disagree.
    let produced: Vec<catalog::ViewColumn> = planned
        .columns
        .iter()
        .map(|column| catalog::ViewColumn {
            name: column.name.clone(),
            ty: column.ty,
            typmod: column.typmod,
        })
        .collect();
    if declared.is_empty() {
        return Ok(produced);
    }
    if declared.len() != produced.len() {
        return Err(SqlError::ViewColumnCount {
            declared: declared.len(),
            produced: produced.len(),
        });
    }
    // A declared list renames the columns and does not retype them.
    Ok(declared
        .iter()
        .zip(produced)
        .map(|(name, column)| catalog::ViewColumn {
            name: name.clone(),
            ..column
        })
        .collect())
}

/// `CREATE MATERIALIZED VIEW name [(cols)] AS SELECT … [WITH [NO] DATA]`.
///
/// **The relation it creates is a table** that carries its definition
/// ([ADR 0064](../../../../docs/adr/0064-a-materialized-view-is-a-table-whose-rows-are-recomputed.md)).
/// The `SELECT` is planned once here — which proves the body and names every column's type, the
/// same step `create_view` takes — and then run, and what it produced is stored as rows.
pub(super) fn create_materialized_view(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateMaterializedView,
) -> Result<Outcome> {
    catalog::pg_catalog::refuse_write(&create.name)?;
    let name = if create.name.contains(catalog::SCHEMA_SEPARATOR) {
        create.name.clone()
    } else {
        let schema = executor.creation_schema(txn)?;
        catalog::qualify(&schema, &create.name)
    };
    if existing_relation(executor, txn, &name)?.is_some() {
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(create.name.clone()));
            return Ok(Outcome::done("CREATE MATERIALIZED VIEW"));
        }
        // Measured: `42P07 relation "mv_ebooks" already exists` — the same answer a second
        // `CREATE TABLE` of the name gets, because it competes in the same namespace.
        return Err(SqlError::DuplicateTable(create.name.clone()));
    }

    let select = matview_body(&create.definition)?;
    // Planned **and run** in one step, because both are needed and the plan is what types the
    // columns: a relation whose declared types came from anywhere but the query that fills it
    // would be a relation whose shape can disagree with its rows.
    let (planned, rows) = executor.planned_rows(txn, &select)?;
    let columns = matview_columns(&planned, &create.columns)?;

    let table_id = catalog::allocate_id(txn, executor.tenant)?;
    let table = table_from_query(
        table_id,
        name,
        columns,
        Some(catalog::MatviewDef {
            definition: create.definition.clone(),
            populated: create.with_data,
        }),
    );
    catalog::create_table(txn, executor.tenant, &table)?;
    executor.catalog_written = true;
    if create.with_data {
        fill_from_query(executor, txn, &table, rows)?;
    }
    Ok(Outcome::done("CREATE MATERIALIZED VIEW"))
}

/// `CREATE TABLE name [ (col, …) ] AS <query>`.
///
/// The same four steps `create_materialized_view` takes — plan and run the query, type the
/// relation **from the plan**, create it, fill it — with the definition thrown away instead of
/// kept. A table from a query is an ordinary table: measured on 19beta1, the new relation has no
/// `NOT NULL`, no default, no primary key and no index, although `SELECT id FROM people` reads a
/// `bigserial primary key`. Nothing here has to drop those, because nothing here copies them —
/// the columns come from the query's output and carry only its types.
///
/// **The tag is `SELECT <n>`, not `CREATE TABLE AS`.** Measured: PostgreSQL reports the row count
/// it wrote, the way `INSERT … SELECT` does, and a client that counts rows off the tag would be
/// told nothing by a `CREATE` tag.
///
/// `WITH NO DATA` is not handled: the corpus never sends it — the string appears nowhere in
/// `activerecord` — and inventing a rule for it is what
/// `docs/adr/0031-rails-compatibility-is-measured.md` exists to prevent.
pub(super) fn create_table_as(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateTableAs,
) -> Result<Outcome> {
    catalog::pg_catalog::refuse_write(&create.name)?;
    let name = if create.name.contains(catalog::SCHEMA_SEPARATOR) {
        create.name.clone()
    } else {
        let schema = executor.creation_schema(txn)?;
        catalog::qualify(&schema, &create.name)
    };
    if existing_relation(executor, txn, &name)?.is_some() {
        if create.if_not_exists {
            executor.notice(SqlError::AlreadyExistsSkipping(create.name.clone()));
            return Ok(Outcome::done("CREATE TABLE AS"));
        }
        return Err(SqlError::DuplicateTable(create.name.clone()));
    }

    let select = matview_body(&create.definition)?;
    let (planned, rows) = executor.planned_rows(txn, &select)?;
    let columns = matview_columns(&planned, &create.columns)?;
    let written = rows.len();

    let table_id = catalog::allocate_id(txn, executor.tenant)?;
    let table = table_from_query(table_id, name, columns, None);
    catalog::create_table(txn, executor.tenant, &table)?;
    executor.catalog_written = true;
    fill_from_query(executor, txn, &table, rows)?;
    Ok(Outcome::done(format!("SELECT {written}")))
}

/// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`.
///
/// **The rows are replaced inside the caller's transaction**, which is the whole of what makes a
/// refresh transactional — measured, a `REFRESH` inside `BEGIN … ROLLBACK` leaves the old rows.
/// Nothing here arranges that; the storage layer does, because these are ordinary table rows.
pub(super) fn refresh_materialized_view(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    refresh: &plan::RefreshMaterializedView,
) -> Result<Outcome> {
    let stored = executor.resolve_unqualified(txn, &refresh.name)?;
    let table = match existing_relation(executor, txn, &stored)? {
        Some(catalog::Relation::Table { table_id }) => executor.table_by_id(txn, table_id)?,
        // Measured: a `REFRESH` of a name that is not there is `42P01 relation "…" does not
        // exist` — the plain relation message, not one that says "materialized view".
        _ => return Err(SqlError::UndefinedTable(refresh.name.clone())),
    };
    let Some(matview) = table.matview.clone() else {
        return Err(SqlError::WrongObjectType {
            name: refresh.name.clone(),
            expected: "a materialized view",
            found: relation_drop_verb(&table),
        });
    };
    // **`CONCURRENTLY` needs a unique index**, and the name in the message is schema-qualified
    // where almost nothing else is. Copied from the capture rather than composed.
    if refresh.concurrently && !table.indexes.iter().any(|index| index.unique) {
        let (schema, bare) = catalog::split_qualified(&table.name);
        return Err(SqlError::CannotRefreshConcurrently(format!(
            "{schema}.{bare}"
        )));
    }

    // The old rows first, through the ordinary removal path, so every index entry goes with each
    // one rather than being reasoned about separately here.
    truncate_one_table(executor, txn, &table, false)?;
    let rows = if refresh.with_data {
        let select = matview_body(&matview.definition)?;
        let (_, rows) = executor.planned_rows(txn, &select)?;
        rows
    } else {
        Vec::new()
    };
    fill_from_query(executor, txn, &table, rows)?;

    // **`WITH NO DATA` on a refresh un-populates it**, so a read afterwards is `55000` again
    // rather than an honest-looking empty answer.
    let mut updated = (*table).clone();
    updated.matview = Some(catalog::MatviewDef {
        definition: matview.definition,
        populated: refresh.with_data,
    });
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    executor.catalog_written = true;
    Ok(Outcome::done("REFRESH MATERIALIZED VIEW"))
}

/// `DROP MATERIALIZED VIEW [IF EXISTS] name [, …] [CASCADE]`.
pub(super) fn drop_materialized_view(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropMaterializedView,
) -> Result<Outcome> {
    for name in &drop.names {
        catalog::pg_catalog::refuse_write(name)?;
        let stored = executor.resolve_unqualified(txn, name)?;
        let table = match existing_relation(executor, txn, &stored)? {
            Some(catalog::Relation::Table { table_id }) => executor.table_by_id(txn, table_id)?,
            // A plain view under this name is the wrong kind, and PostgreSQL hints the verb that
            // would have worked: `Use DROP VIEW to remove a view.`
            Some(catalog::Relation::View { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a materialized view",
                    found: "DROP VIEW",
                });
            }
            None => {
                if drop.if_exists {
                    executor.notice(SqlError::DoesNotExistSkipping {
                        kind: "materialized view",
                        name: name.clone(),
                    });
                    continue;
                }
                // Measured, and it is **not** the plain relation message a `REFRESH` gets:
                // `42P01 materialized view "mv_nosuch" does not exist`.
                return Err(SqlError::UndefinedMatviewForDrop(name.clone()));
            }
            _ => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a materialized view",
                    found: "DROP TABLE",
                });
            }
        };
        if table.matview.is_none() {
            return Err(SqlError::WrongObjectType {
                name: name.clone(),
                expected: "a materialized view",
                found: relation_drop_verb(&table),
            });
        }
        refuse_or_drop_dependent_views(
            executor,
            txn,
            &table.name,
            "materialized view",
            drop.cascade,
        )?;
        truncate_one_table(executor, txn, &table, false)?;
        catalog::drop_table(txn, executor.tenant, &table)?;
        executor.catalog_written = true;
    }
    Ok(Outcome::done("DROP MATERIALIZED VIEW"))
}

/// The `DROP` verb that would have worked for a relation, for a `42809`'s `HINT`.
fn relation_drop_verb(table: &TableDef) -> &'static str {
    if table.matview.is_some() {
        "DROP MATERIALIZED VIEW"
    } else {
        "DROP TABLE"
    }
}

/// A materialized view's stored `SELECT`, parsed and lowered.
fn matview_body(definition: &str) -> Result<plan::Select> {
    let parsed = crate::parse::parse_statements(definition)?;
    let [statement] = parsed.as_slice() else {
        return Err(SqlError::unsupported(
            "a materialized view definition that is more than one statement",
        ));
    };
    match statement.lower()? {
        plan::Statement::Select(select) => Ok(*select),
        _ => Err(SqlError::unsupported(
            "a materialized view definition that is not a SELECT",
        )),
    }
}

/// The columns a materialized view stores: the query's, renamed by a declared list if there was one.
///
/// **Every one is nullable and none carries a default**, measured: a materialized view over a
/// `NOT NULL` base column reports `attnotnull` `f`, for the same reason a view does — the value is
/// an expression's, and the constraint belongs to the table it read.
fn matview_columns(planned: &super::query::Planned, declared: &[String]) -> Result<Vec<ColumnDef>> {
    if !declared.is_empty() && declared.len() != planned.columns.len() {
        return Err(SqlError::ViewColumnCount {
            declared: declared.len(),
            produced: planned.columns.len(),
        });
    }
    Ok(planned
        .columns
        .iter()
        .enumerate()
        .map(|(at, column)| ColumnDef {
            collation: None,
            name: declared
                .get(at)
                .cloned()
                .unwrap_or_else(|| column.name.clone()),
            ty: column.ty,
            typmod: column.typmod,
            default_expr: None,
            not_null: false,
            default: None,
            missing: None,
            generated: None,
            generated_virtual: false,
            comment: None,
            dropped: false,
            user_type: column.user_type.as_ref().map(|ty| ty.oid),
        })
        .collect())
}

/// The `TableDef` a materialized view is.
///
/// **No primary key**, measured: `pg_constraint` and `pg_index` are both empty for a fresh one, and
/// `view_test.rb`'s `test_does_not_assume_id_column_as_primary_key` asserts exactly that. So it
/// takes the internal row id every keyless table takes, at position 0.
/// A relation built **from a query** rather than from declarations: the columns are the plan's,
/// and column 0 is the internal row id, because such a relation has no key of its own.
///
/// Shared by `CREATE MATERIALIZED VIEW` and `CREATE TABLE … AS`, which differ only in whether the
/// definition is kept — `matview` is `None` for the second, and that is the whole difference.
fn table_from_query(
    id: u64,
    name: String,
    columns: Vec<ColumnDef>,
    matview: Option<catalog::MatviewDef>,
) -> TableDef {
    let mut with_row_id = Vec::with_capacity(columns.len() + 1);
    with_row_id.push(ColumnDef {
        collation: None,
        name: catalog::INTERNAL_ROW_ID_NAME.to_owned(),
        ty: ColumnType::Int8,
        typmod: crate::value::NO_TYPMOD,
        default_expr: None,
        not_null: true,
        default: None,
        missing: None,
        generated: None,
        generated_virtual: false,
        comment: None,
        dropped: false,
        user_type: None,
    });
    with_row_id.extend(columns);
    TableDef {
        matview,
        on_commit: catalog::OnCommit::PreserveRows,
        id,
        persistence: catalog::Persistence::Permanent,
        name,
        columns: with_row_id,
        primary_key: vec![0],
        indexes: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        triggers_disabled: false,
        parents: Vec::new(),
        partition_by: None,
        partition_bound: None,
        children: Vec::new(),
        triggers: Vec::new(),
        excludes: Vec::new(),
        child_scans: Vec::new(),
        primary_key_name: String::new(),
        schema_version: 1,
        sequences: Vec::new(),
        comment: None,
        primary_key_comment: None,
        enums: std::collections::BTreeMap::new(),
    }
}

/// Writes what the definition produced, through the ordinary row-writing path.
fn fill_from_query(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    rows: Vec<Vec<Datum>>,
) -> Result<()> {
    for row in rows {
        let mut stored = Vec::with_capacity(row.len() + 1);
        // Column 0 is the internal row id the relation has instead of a key.
        stored.push(Datum::Int8(executor.next_row_id(table.id)?));
        stored.extend(row);
        let mut written = super::Written::default();
        super::dml::write_row(executor, txn, table, &stored, &mut written)?;
    }
    Ok(())
}

/// `DROP VIEW [IF EXISTS] name [, …]`.
pub(super) fn drop_view(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropView,
) -> Result<Outcome> {
    for name in &drop.names {
        let stored = executor.resolve_unqualified(txn, name)?;
        match existing_relation(executor, txn, &stored)? {
            Some(catalog::Relation::View { .. }) => {
                // A view built on this one is a dependency exactly as a view on a table is —
                // same `2BP01`, with `view` on both sides of the `DETAIL`.
                refuse_or_drop_dependent_views(executor, txn, &stored, "view", drop.cascade)?;
                catalog::drop_view(txn, executor.tenant, &stored)?;
                executor.catalog_written = true;
            }
            // **`42809`, with a `HINT` naming the verb that would have worked** — the same shape
            // `DROP TABLE` over a view gets, measured in both directions. A **materialized** view
            // is a table record and reaches this arm too, and its hint is a third one: measured,
            // `Use DROP MATERIALIZED VIEW to remove a materialized view.`
            Some(catalog::Relation::Table { table_id }) => {
                let table = executor.table_by_id(txn, table_id)?;
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a view",
                    found: relation_drop_verb(&table),
                });
            }
            Some(_) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a view",
                    found: "DROP TABLE",
                });
            }
            None if drop.if_exists => {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "view",
                    name: name.clone(),
                });
            }
            // **`view "x" does not exist`, not `relation`** — the noun is the statement's, which
            // is the rule `DROP TABLE` follows too.
            None => return Err(SqlError::UndefinedViewForDrop(name.clone())),
        }
    }
    Ok(Outcome::done("DROP VIEW"))
}

/// `CREATE ROLE name` / `CREATE USER name`.
///
/// Cluster-wide: a role made here is visible from every database, which is what a real server does
/// and why `catalog::create_role` takes no tenant.
///
/// **No privileges are attached and none are enforced** — a declared divergence
/// (`docs/plans/roles-and-user-schemas.md`). The six tests this unit exists for need a role to
/// *exist*, own a schema and be assumable; a catalog that recorded grants nobody honoured would be
/// a security-shaped feature that is not one.
pub(super) fn create_role(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &plan::CreateRole,
) -> Result<Outcome> {
    if create.if_not_exists && catalog::role_by_name(&*txn, &create.name)?.is_some() {
        executor.notice(SqlError::DoesNotExistSkipping {
            kind: "role",
            name: create.name.clone(),
        });
        return Ok(Outcome::done("CREATE ROLE"));
    }
    catalog::create_role(txn, &create.name, create.flags)?;
    Ok(Outcome::done("CREATE ROLE"))
}

/// `DROP ROLE [IF EXISTS] name` / `DROP USER …`.
pub(super) fn drop_role(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropRole,
) -> Result<Outcome> {
    for name in &drop.names {
        if catalog::role_by_name(&*txn, name)?.is_none() {
            if drop.if_exists {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "role",
                    name: name.clone(),
                });
                continue;
            }
            return Err(SqlError::UndefinedRole(name.clone()));
        }
        catalog::drop_role(txn, name)?;
    }
    Ok(Outcome::done("DROP ROLE"))
}

/// `DROP SCHEMA [IF EXISTS] name [CASCADE]`.
///
/// **`IF EXISTS` covers absence and not dependence**: a schema with something in it is `2BP01`
/// with the clause written, naming one dependent and pointing at `CASCADE`. Measured.
pub(super) fn drop_schema(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &plan::DropSchema,
) -> Result<Outcome> {
    // The sequences every dropped table takes with it — see `drop_one_table`, which cannot forget
    // their reserved blocks itself.
    let mut forgotten: Vec<u64> = Vec::new();
    for name in &drop.names {
        // **The catalog's own two are `2BP01` before anything else is looked at**, `IF EXISTS`
        // included: they exist, so the clause does not apply, and what is wrong is that the
        // database system depends on them rather than that some table does.
        if catalog::is_reserved_schema(name) {
            return Err(SqlError::RequiredSchema(name.clone()));
        }
        if !catalog::schema_exists(&*txn, executor.tenant, name)? {
            if drop.if_exists {
                executor.notice(SqlError::DoesNotExistSkipping {
                    kind: "schema",
                    name: name.clone(),
                });
                continue;
            }
            return Err(SqlError::UndefinedSchema(name.clone()));
        }
        // **A schema with something in it is `2BP01`, naming one dependent** — the way a table
        // with an inheriting child is — and `CASCADE` takes the relations with it instead.
        // `IF EXISTS` does not excuse this: the clause covers absence, not dependence. Measured.
        let held = catalog::relations_in_schema(&*txn, executor.tenant, name)?;
        // **A type in the schema is a dependent too**, and it is the one nothing here could see: a
        // type is not a name record, so `relations_in_schema` never returned one. A schema holding
        // only a domain dropped **silently**, and the domain survived with a record key naming a
        // schema that was gone — visible in `pg_type` under `public`, not resolvable by name, and
        // not droppable. Measured on PostgreSQL: `2BP01 … DETAIL: type ds_s.ds depends on schema
        // ds_s`, with the type named the way a table is.
        let types: Vec<String> = catalog::user_types(&*txn, executor.tenant)?
            .into_iter()
            .map(|def| def.name)
            .filter(|stored| catalog::split_qualified(stored).0 == name)
            .collect();
        if !drop.cascade
            // **A type is named before a table**, measured: a schema holding both reports the
            // type. PostgreSQL names one dependent of many and this is the one it picks.
            && let Some(detail) = types
                .first()
                .map(|first| {
                    let bare = catalog::split_qualified(first).1;
                    format!("type {name}.{bare} depends on schema {name}")
                })
                .or_else(|| {
                    held.first().map(|first| {
                        let bare = catalog::split_qualified(first).1;
                        format!("table {name}.{bare} depends on schema {name}")
                    })
                })
        {
            return Err(SqlError::DependentSchema {
                schema: name.clone(),
                detail,
            });
        }
        // The **tables** first, each taking its own indexes, sequences and primary key with it —
        // the same path `DROP TABLE ... CASCADE` walks, so nothing is dropped twice.
        for stored in &held {
            if let Some(catalog::Relation::Table { table_id }) =
                executor.catalog_view(&*txn)?.relation(stored)?
            {
                let table = executor.table_by_id(txn, table_id)?;
                forgotten.extend(drop_one_table(executor, txn, &table)?);
            }
        }
        // **Then whatever is left, which used to be nothing and is not.** This loop dropped only
        // `Relation::Table` and skipped every other kind in silence, so a sequence created on its
        // own — `CREATE SEQUENCE test_schema.s`, owned by no column — kept its name record while
        // the schema record went. The next `CREATE SEQUENCE` of that name then answered
        // `42P07 relation "test_schema.s" already exists` naming a schema that no longer existed:
        // 51 tests of `schema_test.rb`, whose `setup` ends with exactly that statement and whose
        // `teardown` is `DROP SCHEMA … CASCADE`.
        //
        // A name resolved here is one the table loop did not take, so re-reading is what tells
        // "owned by a table, already gone" from "standalone, still here" without tracking either.
        for stored in &held {
            let Some(catalog::Relation::Sequence {
                table_id,
                sequence_id,
            }) = executor.catalog_view(&*txn)?.relation(stored)?
            else {
                continue;
            };
            if let Some(sequence) =
                catalog::sequence_by_id(txn, executor.tenant, table_id, sequence_id)?
            {
                catalog::drop_sequence(txn, executor.tenant, table_id, &sequence);
                forgotten.push(sequence_id);
            }
        }
        // The types go too, and after the tables: a column declared as one of them has already
        // gone with its table, so nothing is left pointing at a type this removes.
        for stored in &types {
            catalog::drop_type(txn, executor.tenant, stored);
        }
        catalog::drop_schema(txn, executor.tenant, name)?;
        for sequence_id in std::mem::take(&mut forgotten) {
            executor.forget_sequence_block(sequence_id);
        }
    }
    Ok(Outcome::done("DROP SCHEMA"))
}

/// `ALTER SCHEMA name RENAME TO other`.
pub(super) fn alter_schema_rename(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    rename: &plan::AlterSchemaRename,
) -> Result<Outcome> {
    if !catalog::schema_exists(&*txn, executor.tenant, &rename.name)? {
        return Err(SqlError::UndefinedSchema(rename.name.clone()));
    }
    if catalog::schema_exists(&*txn, executor.tenant, &rename.to)? {
        return Err(SqlError::DuplicateSchema(rename.to.clone()));
    }
    // `public` is a property of the build rather than a record, so there is nothing to rename and
    // a real server refuses it for its own reason (ownership). Named rather than half-done.
    if rename.name == catalog::PUBLIC_SCHEMA {
        return Err(SqlError::unsupported("ALTER SCHEMA public RENAME TO"));
    }
    let id = catalog::schemas(&*txn, executor.tenant)?
        .into_iter()
        .find(|(name, _)| *name == rename.name)
        .map_or(0, |(_, id)| id);
    catalog::drop_schema(txn, executor.tenant, &rename.name)?;
    catalog::create_schema(txn, executor.tenant, &rename.to, id)?;
    Ok(Outcome::done("ALTER SCHEMA"))
}

/// The first live generated column whose expression names `column`, if there is one.
///
/// Read out of the stored expression the way a view's dependency is read out of its definition: a
/// generated expression is text and is re-lowered per write anyway, so parsing it here costs
/// nothing new. A column whose own expression no longer parses is skipped — it cannot be a
/// dependency this statement understands.
fn generated_columns_reading(table: &TableDef, column: &str) -> Option<String> {
    table.columns.iter().find_map(|candidate| {
        if candidate.dropped || candidate.name == column {
            return None;
        }
        let expr = candidate.generated.as_ref()?;
        let parsed = crate::parse::parse_stored_expr(expr).ok()?;
        let mut names = false;
        super::bind::descend(&parsed, &mut |expr| {
            if matches!(expr, plan::Expr::Column { name, .. } if name == column) {
                names = true;
            }
        });
        names.then(|| candidate.name.clone())
    })
}

/// Every table whose foreign key points at this one, by record.
///
/// The same back-reference range a `DELETE` on a parent walks (`crate::exec::foreign_key`), so a
/// truncate asks the question the row-level check already asks and gets the same answer. A child
/// that references itself is not one: emptying the table takes its own rows with it.
fn referencing_children(
    executor: &Executor,
    txn: &dyn Txn,
    table: &TableDef,
) -> Result<Vec<TableDef>> {
    let mut children = Vec::new();
    for (child, _) in super::foreign_key::children_of(executor, txn, table)? {
        if child.id != table.id && !children.iter().any(|seen: &TableDef| seen.id == child.id) {
            children.push((*child).clone());
        }
    }
    Ok(children)
}

/// `TRUNCATE [TABLE] t [, …] [RESTART IDENTITY] [CASCADE]`.
///
/// **Not a `DELETE` without a `WHERE`**, and the difference is what it leaves alone: a sequence an
/// identity or `serial` column owns keeps its value, so the next `nextval` carries on from where it
/// was. `RESTART IDENTITY` is the clause that moves it, measured — and it is the one
/// `ActiveRecord` sends when it wants a table to look new.
///
/// Every table named is emptied in the statement's own transaction, and every index entry with it:
/// an index whose rows were dropped and whose entries were not is a `SELECT` answering from a row
/// that is gone.
pub(super) fn truncate(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    truncate: &plan::Truncate,
) -> Result<Outcome> {
    let mut tables = Vec::with_capacity(truncate.names.len());
    for name in &truncate.names {
        catalog::pg_catalog::refuse_write(name)?;
        let Some(catalog::Relation::Table { table_id }) = existing_relation(executor, txn, name)?
        else {
            return Err(SqlError::UndefinedTableForDrop(name.clone()));
        };
        let table = executor.table_by_id(txn, table_id)?;
        // **A materialized view is not a table to `TRUNCATE`**, and the message says exactly that
        // with **no** `HINT` — measured, where the `DROP TABLE` of the same relation carries one.
        // Emptying it would also be a lie: the next `REFRESH` puts every row back.
        if table.matview.is_some() {
            return Err(SqlError::WrongObjectType {
                name: name.clone(),
                expected: "a table",
                found: "TRUNCATE",
            });
        }
        tables.push((*table).clone());
    }
    // **A table something references is refused before anything is emptied**, so a statement that
    // names three tables and cannot empty the second empties none of them. `CASCADE` truncates the
    // children instead — and a table named in the same statement is not a reason to refuse, which
    // is why the check is against the whole list.
    if !truncate.cascade {
        for table in &tables {
            for child in referencing_children(executor, txn, table)? {
                // A child named in the same statement is emptied too, so it is no reason to
                // refuse — which is exactly what PostgreSQL's own hint advises doing.
                if tables.iter().any(|named| named.id == child.id) {
                    continue;
                }
                return Err(SqlError::CannotTruncateReferenced {
                    relation: catalog::display_name(&table.name),
                    child: catalog::display_name(&child.name),
                });
            }
        }
    }
    let mut queue = tables.clone();
    let mut done: Vec<u64> = Vec::new();
    while let Some(table) = queue.pop() {
        if done.contains(&table.id) {
            continue;
        }
        done.push(table.id);
        if truncate.cascade {
            for child in referencing_children(executor, txn, &table)? {
                queue.push(child);
            }
        }
        truncate_one_table(executor, txn, &table, truncate.restart_identity)?;
    }
    Ok(Outcome::done("TRUNCATE TABLE"))
}

/// Every row of one table, and every index entry over it — and its sequences if asked.
fn truncate_one_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    restart_identity: bool,
) -> Result<()> {
    // The rows first, through the ordinary removal path, so every index entry and every
    // back-reference goes with each one rather than being reasoned about separately here.
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut rows: Vec<Vec<Datum>> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (_, value) in page {
            rows.push(crate::row::decode_row(&schema, value, None)?);
        }
        Ok(())
    })?;
    for row in rows {
        super::dml::remove_row(executor, txn, table, &row)?;
    }
    // **Only if asked.** Without `RESTART IDENTITY` the sequence keeps its value and the next id
    // carries on past the rows that are gone — measured, and the opposite of what "empty" suggests.
    if restart_identity {
        // **Two things, and neither alone is enough**: the stored counter, reset in a transaction
        // of its own because that is the one `nextval` reads in, and this session's reserved
        // block, which would otherwise keep handing out values from inside it. Measured: the
        // counter alone gave 33 and the block alone gave 5, where PostgreSQL gives 1.
        for sequence in &table.sequences {
            executor.restart_sequence(sequence.id)?;
        }
    }
    Ok(())
}

/// Whether the table holds a single row — asked with a bound of one, not a count.
///
/// The question every "is there a row that predates this?" check asks, and the whole table is the
/// wrong thing to read for it: one key is enough to know, and a table with a million rows answers
/// as fast as one with none.
fn has_any_row(txn: &dyn Txn, executor: &Executor, table: &TableDef) -> Result<bool> {
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    Ok(!txn.scan(&start, &end, 1)?.is_empty())
}

/// Computes every generated column for the rows already stored, after one was added.
///
/// The whole table is read and written in the statement's own transaction — the trade `backfill`
/// and `set_column_type` already make, and for the same reason: the value is a function of each
/// row, so there is no constant to pad with.
/// **Every row already stored gets its own value from an expression `DEFAULT`.**
///
/// The twin of [`fill_generated_for_existing_rows`], and for the same reason: the value is a
/// function of nothing the catalog can hold once, so it has to be written per row.
/// [`super::dml::column_default_value`] parses and evaluates the stored text each time it is
/// called, which is what makes three rows three different uuids rather than one repeated.
///
/// The values are computed before any of them is written, because computing one reads the
/// transaction and writing borrows it.
fn fill_default_for_existing_rows(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
    ordinal: usize,
) -> Result<()> {
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut rows: Vec<(Vec<u8>, Vec<Datum>)> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            rows.push((key.to_vec(), crate::row::decode_row(&schema, value, None)?));
        }
        Ok(())
    })?;
    let types = table.column_types();
    let Some(column) = table.columns.get(ordinal).cloned() else {
        return Err(SqlError::Internal(format!(
            "the column just added to {} is not at {ordinal}",
            table.name
        )));
    };
    let mut filled: Vec<(Vec<u8>, Vec<Datum>)> = Vec::with_capacity(rows.len());
    for (key, mut row) in rows {
        // A row written before the column exists is narrower than the schema; `decode_row` has
        // already padded it, so the slot is there to fill.
        row.resize(types.len(), Datum::Null);
        row[ordinal] = super::dml::column_default_value(table, &column, &*txn, executor.tenant)?;
        filled.push((key, row));
    }
    for (key, row) in filled {
        txn.put(&key, &crate::row::encode_row(&types, &row)?);
    }
    Ok(())
}

fn fill_generated_for_existing_rows(
    txn: &mut dyn Txn,
    executor: &Executor,
    table: &TableDef,
) -> Result<()> {
    let (start, end) = crate::row::table_row_range(executor.tenant, table.id);
    let schema = table.row_schema();
    let mut rows: Vec<(Vec<u8>, Vec<Datum>)> = Vec::new();
    super::for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            rows.push((key.to_vec(), crate::row::decode_row(&schema, value, None)?));
        }
        Ok(())
    })?;
    let types = table.column_types();
    for (key, mut row) in rows {
        // A row written before the column exists is narrower than the schema; `decode_row` has
        // already padded it, so the slot is there to fill.
        row.resize(types.len(), Datum::Null);
        super::dml::fill_generated(table, &mut row)?;
        txn.put(&key, &crate::row::encode_row(&types, &row)?);
    }
    Ok(())
}

/// The view half of a `DROP`: refuse if anything is built on `relation`, or take it with `CASCADE`.
///
/// Checked **before** the foreign keys, because PostgreSQL reports the first dependent it finds and
/// views come first in its own order. Without this the base could go and the view be left naming a
/// relation that is gone — a name outliving its object, reached from an ordinary `DROP TABLE`.
fn refuse_or_drop_dependent_views(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    relation: &str,
    kind: &'static str,
    cascade: bool,
) -> Result<()> {
    let dependents = dependent_relations(executor, txn, relation)?;
    if let Some((dependent, dependent_kind)) = dependents.first()
        && !cascade
    {
        return Err(SqlError::ViewDependsOnRelation {
            kind,
            name: catalog::display_name(relation),
            // **The dependent's noun, not a constant `view`** — a materialized view says so.
            detail: format!(
                "{dependent_kind} {} depends on {kind} {}",
                catalog::display_name(dependent),
                catalog::display_name(relation)
            ),
        });
    }
    drop_views_cascading(
        executor,
        txn,
        dependents.into_iter().map(|(name, _)| name).collect(),
    )
}

/// Drops each view and everything built on it, depth first.
///
/// **A view over a view is a dependency too**, so a cascade that dropped only the direct
/// dependents would leave the outer one naming a relation that is gone — the very thing the edge
/// exists to prevent, reintroduced by the `CASCADE` that was meant to clean up. Depth first so a
/// view is dropped after everything that reads it.
fn drop_views_cascading(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    views: Vec<String>,
) -> Result<()> {
    for view in views {
        let outer = dependent_views(executor, txn, &view)?;
        drop_views_cascading(executor, txn, outer)?;
        // Already gone with an earlier branch: two views over one table can share a dependent.
        if catalog::view(txn, executor.tenant, &view)?.is_none() {
            continue;
        }
        executor.notice(SqlError::CascadeDropsView(catalog::display_name(&view)));
        catalog::drop_view(txn, executor.tenant, &view)?;
    }
    Ok(())
}

/// Every view whose query names `column` of `relation`, by stored name.
///
/// **Column-precise, because PostgreSQL's message is.** `DROP COLUMN` says `view v depends on
/// column c of table t`, so a view that reads the table but not the column does not stop the
/// statement. A bare column reference counts wherever it appears — target list, `WHERE`, `ORDER
/// BY` — and a qualified one counts only when the qualifier is the table or its alias.
///
/// `SELECT *` is the case worth naming: it lowers to the columns the table had **when the view was
/// created**, so it names the column explicitly and is caught like any other.
fn views_depending_on_column(
    executor: &Executor,
    txn: &dyn Txn,
    relation: &str,
    column: &str,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for view in dependent_views(executor, txn, relation)? {
        let Some(def) = catalog::view(txn, executor.tenant, &view)? else {
            continue;
        };
        let Ok(parsed) = crate::parse::parse_statements(&def.definition) else {
            continue;
        };
        let Some(Ok(lowered)) = parsed.first().map(crate::parse::Parsed::lower) else {
            continue;
        };
        if super::bind::any(&lowered, |expr| {
            matches!(expr, plan::Expr::Column { name, table }
                if name == column
                    && table.as_deref().is_none_or(|qualifier| qualifier == relation))
        }) {
            found.push(view);
        }
    }
    Ok(found)
}

/// Every view whose query names `relation`, by stored name.
///
/// **Read out of the definitions rather than out of an edge.** A `ViewDef` stores the `SELECT`
/// text and nothing else, so the dependency is recovered by parsing each view — which is
/// affordable because `DROP` is rare and a tenant has few views, and which needs no format change.
/// A definition that no longer parses is skipped rather than fatal: it cannot be a dependency this
/// statement understands, and refusing every `DROP TABLE` in the database because one view is
/// unreadable would be the worse answer.
fn dependent_views(executor: &Executor, txn: &dyn Txn, relation: &str) -> Result<Vec<String>> {
    Ok(dependent_relations(executor, txn, relation)?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

/// Every view **and materialized view** whose definition names `relation`, with the noun
/// PostgreSQL uses for each in the `DETAIL`.
///
/// **A materialized view is a dependency exactly as a view is** — measured, `DROP TABLE` of its
/// base is `2BP01 … DETAIL: materialized view mv_ebooks depends on table mv_books`. The noun
/// differs and the edge does not, which is why both come out of one walk rather than two.
fn dependent_relations(
    executor: &Executor,
    txn: &dyn Txn,
    relation: &str,
) -> Result<Vec<(String, &'static str)>> {
    let mut found = Vec::new();
    let names_it = |definition: &str| -> bool {
        let Ok(parsed) = crate::parse::parse_statements(definition) else {
            return false;
        };
        let Some(Ok(lowered)) = parsed.first().map(crate::parse::Parsed::lower) else {
            return false;
        };
        super::bind::table_names(&lowered).contains(&relation)
    };
    for view in catalog::views(txn, executor.tenant)? {
        if names_it(&view.definition) {
            found.push((view.id, view.name.clone(), "view"));
        }
    }
    let relations = catalog::pg_relations::Relations::read(txn, executor.tenant)?;
    for row in relations.of_kind(catalog::pg_relations::RelKind::MaterializedView) {
        let Some(table) = relations.table(row) else {
            continue;
        };
        let Some(matview) = table.matview.as_ref() else {
            continue;
        };
        if names_it(&matview.definition) {
            found.push((table.id, table.name.clone(), "materialized view"));
        }
    }
    // **In creation order, because the `DETAIL` names the first dependent found and a real server
    // finds them by oid.** With ten views over `vb` it says `view v_plain depends on table vb` —
    // the oldest — where a walk in name order said `v_distinct` (`pg19_view_debts.txt:56`).
    found.sort_by_key(|(id, _, _)| *id);
    Ok(found
        .into_iter()
        .map(|(_, name, kind)| (name, kind))
        .collect())
}

pub(super) fn drop_table(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    drop: &DropTable,
) -> Result<Outcome> {
    // See `drop_one_table`: it cannot forget a dropped sequence's reserved block itself.
    let mut forgotten: Vec<u64> = Vec::new();
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
            // Measured: `"v" is not a table`, with `HINT: Use DROP VIEW to remove a view.`
            Some(catalog::Relation::View { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "a table",
                    found: "DROP VIEW",
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
        // **A materialized view is a table record**, so `DROP TABLE` finds one and has to refuse:
        // `"m" is not a table`, hinting the verb that works. Measured.
        if table.matview.is_some() {
            return Err(SqlError::WrongObjectType {
                name: name.clone(),
                expected: "a table",
                found: "DROP MATERIALIZED VIEW",
            });
        }
        refuse_or_drop_dependent_views(executor, txn, &table.name, "table", drop.cascade)?;
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
        //
        // **A partition is the opposite, and the two share this edge.** `DROP TABLE measurements`
        // with two partitions holding rows succeeds and takes both — no `CASCADE` — leaving zero
        // relations under that name. Measured, in the same session that measured the `INHERITS`
        // refusal above, and the difference is not cosmetic: it is the statement
        // `create_table(:measurements, force: true)` sends on every schema reload, so a node that
        // refused it could not load the suite's schema twice.
        for &child_id in &table.children {
            let child = executor.table_by_id(txn, child_id)?;
            if !drop.cascade && child.partition_bound.is_none() {
                return Err(SqlError::DependentTable {
                    relation: table.name.clone(),
                    detail: format!("table {} depends on table {}", child.name, table.name),
                });
            }
            forgotten.extend(drop_one_table(executor, txn, &child)?);
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

        forgotten.extend(drop_one_table(executor, txn, &table)?);
    }
    // **On commit, not now.** The block is a node-level reservation and dropping it is not
    // undone by a rollback, so forgetting it inside the statement made a rolled-back `DROP TABLE`
    // cost a whole batch — see `Executor::forget_sequence_block_on_commit`.
    for sequence_id in forgotten {
        executor.forget_sequence_block_on_commit(sequence_id);
    }
    Ok(Outcome::done("DROP TABLE"))
}

/// One table's rows, index entries and catalog record — everything but the dependency checks.
///
/// Factored out so `CASCADE` can reach an inheriting child with it: a child dropped that way needs
/// exactly the same removal the named table gets, and doing it by hand at the second call site is
/// how one of the three steps gets forgotten.
/// Every relation in one session's temporary schema, and then the schema
/// ([ADR 0054](../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md)).
///
/// The same path `DROP SCHEMA … CASCADE` walks, so each table takes its own indexes, sequences and
/// primary key with it and nothing is left half-dropped.
pub(super) fn drop_temp_schema(
    executor: &Executor,
    txn: &mut dyn Txn,
    schema: &str,
) -> Result<Vec<u64>> {
    let mut forgotten = Vec::new();
    for stored in catalog::relations_in_schema(&*txn, executor.tenant, schema)? {
        if let Some(catalog::Relation::Table { table_id }) =
            executor.catalog_view(&*txn)?.relation(&stored)?
        {
            let table = executor.table_by_id(txn, table_id)?;
            forgotten.extend(drop_one_table(executor, txn, &table)?);
        }
    }
    catalog::drop_schema(txn, executor.tenant, schema)?;
    Ok(forgotten)
}

/// `ON COMMIT` for every temporary table this session has, run at the end of **every** transaction
/// ([ADR 0054](../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md)).
///
/// **"Every transaction" includes the implicit one**, which is the fact this exists to get right:
/// a plain `INSERT` outside a transaction block into an `ON COMMIT DELETE ROWS` table leaves zero
/// rows behind, because that statement's own commit fires the rule. Measured, and an
/// implementation hooked only to an explicit `COMMIT` looks right inside a block and answers one
/// where a real server answers none.
///
/// It runs inside the committing transaction, so the emptying and the statement's own writes are
/// one atomic step — and a transaction that rolls back undoes both, which is what makes a rollback
/// need no rule of its own.
pub(super) fn run_on_commit(executor: &Executor, txn: &mut dyn Txn) -> Result<Vec<u64>> {
    let mut forgotten = Vec::new();
    let Some(schema) = executor.temp_schema() else {
        return Ok(forgotten);
    };
    let held = catalog::relations_in_schema(&*txn, executor.tenant, schema)?;
    for stored in held {
        let Some(catalog::Relation::Table { table_id }) =
            executor.catalog_view(&*txn)?.relation(&stored)?
        else {
            continue;
        };
        let table = executor.table_by_id(txn, table_id)?;
        match table.on_commit {
            catalog::OnCommit::PreserveRows => {}
            catalog::OnCommit::DeleteRows => empty_table(executor, txn, &table)?,
            catalog::OnCommit::Drop => forgotten.extend(drop_one_table(executor, txn, &table)?),
        }
    }
    Ok(forgotten)
}

/// Every row and every index entry of one table, deleted — the table itself stays.
///
/// The row half of [`drop_one_table`], and paged for the same reason.
fn empty_table(executor: &Executor, txn: &mut dyn Txn, table: &TableDef) -> Result<()> {
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
    Ok(())
}

/// Answers with the **ids of the sequences it dropped**, which the caller has to forget.
///
/// A session reserves [`catalog::SEQUENCE_BATCH`] values of a sequence and serves them from an
/// in-memory block; the block is keyed by the sequence's id and nothing here can reach it, because
/// this takes `&Executor`. Leaving it behind cannot hand out a wrong value — `catalog::allocate_id`
/// never reuses an id, so a re-created sequence is a different sequence and gets its own block —
/// but the entry stays for the life of the connection, and a long-lived one that creates and drops
/// tables collects one per drop. So the ids come back out and `Executor::forget_sequence_block`
/// takes them where there is a `&mut` to do it with.
fn drop_one_table(executor: &Executor, txn: &mut dyn Txn, table: &TableDef) -> Result<Vec<u64>> {
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
    catalog::drop_table(txn, executor.tenant, table)?;
    Ok(table.sequences.iter().map(|sequence| sequence.id).collect())
}

#[expect(
    clippy::too_many_lines,
    reason = "one `CREATE INDEX`, and the operator-class checks (ADR 0070) belong beside the key \
              they are about rather than in a pass that would read the same list again"
)]
pub(super) fn create_index(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    create: &CreateIndex,
) -> Result<Outcome> {
    catalog::pg_catalog::refuse_write(&create.table)?;
    let table = executor.require_table(txn, &create.table)?;
    // **An index lives in its table's schema**, whatever the statement wrote — `CREATE INDEX i ON
    // test_schema.things (…)` names the index bare and PostgreSQL puts it beside the table. Two
    // schemas may each hold an index of one name, which is exactly what `schema_test.rb` creates.
    let schema = catalog::split_qualified(&table.name).0.to_owned();
    // A name the user chose is theirs, and a collision with it is a `42P07`.
    let name = if let Some(given) = &create.name {
        let given = catalog::qualify(&schema, catalog::split_qualified(given).1);
        if existing_relation(executor, txn, &given)?.is_some() {
            if create.if_not_exists {
                executor.notice(SqlError::AlreadyExistsSkipping(given.clone()));
                return Ok(Outcome::done("CREATE INDEX"));
            }
            return Err(SqlError::DuplicateTable(given));
        }
        given
    } else {
        // A name *we* derived is disambiguated instead: `CREATE INDEX ON t (a)` twice gives
        // `t_a_idx` and `t_a_idx1`, not an error. Measured against a real server, which produced
        // `..._idx`, `..._idx1` and `..._idx2` for three. The user named nothing, so there is
        // nothing of theirs to collide with.
        //
        // The counter joins the **label**, so a long name is rebuilt rather than lengthened —
        // `plan::choose_relation_name` carries the measurement.
        let addition = plan::index_name_addition(&create.keys);
        plan::choose_relation_name(&create.table, Some(&addition), "idx", |candidate| {
            Ok::<_, SqlError>(existing_relation(executor, txn, candidate)?.is_some())
        })?
    };

    let keys = create
        .keys
        .iter()
        .map(|key| {
            let part = match &key.part {
                plan::KeyPartName::Column(column) => {
                    let at = table
                        .column(column)
                        .ok_or_else(|| SqlError::UndefinedColumn(column.clone()))?;
                    refuse_unindexable(&table, &table.columns[at], &create.access_method)?;
                    KeyPart::Column(at)
                }
                // **A key that resolves to a bare column is a column key**, however it was
                // written. `((t)::text)` over a `text` column is `USING btree (t)` on a real
                // server, with `indkey` naming the column and `indexprs` null — the no-op cast is
                // not a node (`exec::query::resolve`), so what is left is the column and the
                // catalog records it as one. The parser already unwraps `((t))`; this is the same
                // answer one resolution later, and it is the half of group E that no printer
                // could have reached.
                plan::KeyPartName::Expression { expr, shape } => {
                    if let Some(at) = index_key_column(&table, expr) {
                        refuse_unindexable(&table, &table.columns[at], &create.access_method)?;
                        KeyPart::Column(at)
                    } else {
                        let (expr, ty) = index_expression(&table, expr)?;
                        KeyPart::Expression {
                            expr,
                            shape: *shape,
                            ty,
                        }
                    }
                }
            };
            Ok(IndexKey {
                part,
                order: key.order,
                opclass: key.opclass.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // **The operator class is checked against the method and the column's type** (ADR 0070): a
    // class this node does not have, or one that belongs to another method, or one the column's
    // type does not take, is refused rather than written down. Recording a class nothing verified
    // would put a `CREATE INDEX` a real server rejects into this catalog, which is the half of
    // the decision that keeps the recording honest.
    for (key, part) in keys.iter().zip(&create.keys) {
        catalog::check_operator_class(
            &create.access_method,
            part.opclass.as_deref(),
            key.key_type(&table),
        )?;
    }

    // A predicate naming a column the table does not have is `42703` here, not an internal error
    // at the first write — the same rule, and the same reason, as a `CHECK`'s.
    if let Some(predicate) = &create.predicate {
        let parsed = crate::parse::parse_stored_expr(predicate)?;
        let scope = crate::exec::query::Scope::single(&table);
        crate::exec::query::resolve(&parsed, &scope)?;
    }
    if create.unique {
        refuse_uncovered_partition_key(&table, &keys, "UNIQUE")?;
    }
    let include = included_columns(&table, &create.include)?;
    let index = IndexDef {
        id: catalog::allocate_id(txn, executor.tenant)?,
        name,
        unique: create.unique,
        access_method: create.access_method.clone(),
        keys,
        include,
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
        comment: None,
    };

    // **The sixth reader, at the statement that writes one.** `pg_get_expr(indpred)` prints what
    // is stored, so the predicate is deparsed here — before the build and before either of the two
    // pushes below, so the concurrent path stores the same text as the ordinary one.
    let mut index = index;
    if let Some(predicate) = &index.predicate
        && let Some(text) = deparse_wrapped_predicate(&table, predicate)
    {
        index.predicate = Some(text);
    }
    let index = index;

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
        // **And the statement waits for it, unless this session asked not to.** PostgreSQL
        // answers a concurrent build when the build is done — with the build's own `23505` when a
        // duplicate fails it — so the id is left for the far side of the commit
        // (`finish_concurrent_build`), which is the earliest a job step can see the declaration.
        // Under `esker.concurrent_index_build = 'stage'` the statement returns as soon as the job
        // record exists, and `esker_schema_jobs()` is where a human watches the states advance.
        if drives_the_build(executor) {
            executor.concurrent_build = Some(index_id);
        }
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

/// Whether this session's `CREATE INDEX CONCURRENTLY` waits for the build it starts.
///
/// `esker.concurrent_index_build`, whose boot value is `wait` because that is PostgreSQL's
/// contract. The parameter is in the table, so `lookup` cannot fail.
fn drives_the_build(executor: &Executor) -> bool {
    crate::parameter::lookup("esker.concurrent_index_build")
        .is_ok_and(|parameter| executor.parameter(parameter) == "wait")
}

/// Drives the `CREATE INDEX CONCURRENTLY` this statement declared, and answers when it is done.
///
/// Called on the far side of the statement's commit and nowhere else: a job step is a transaction
/// of its own, so it cannot see a declaration this statement has not committed yet. That the
/// statement *has* one of its own is what `25001` guarantees — a concurrent build inside a
/// transaction block is refused, so there is never a block whose rollback could take the
/// declaration back after the build has begun.
///
/// # What the client is told
///
/// * the change reaches `public` — the statement answers `CREATE INDEX`, and the index is readable
///   the moment the client hears back, which is what makes it usable from the next statement;
/// * the backfill meets a duplicate — the states unwind, the **invalid index stays in the
///   catalog** (`indisvalid = f`, its name still taken) and the client is told the build's own
///   `23505 could not create unique index "…"`. All three halves measured on 19beta1 and pinned in
///   `tests/invalid_index.rs`; `postgresql_adapter_test#test_invalid_index` asserts all three;
/// * this node loses its schema lease part-way — `55000`, and the job is left where it is for
///   another node's re-driver, which is the same rule the re-driver applies to itself.
///
/// # The wait between transitions is kept
///
/// A state transition may not be followed by another until PD's step interval has passed, or a
/// node one state behind could end up two (ADR 0020, [`crate::exec::verbs::Stepped`]). Driving
/// from the statement does not make that cheaper, so the wait is taken here — three of them for a
/// build, and on a real cluster that is `3 × (lease + lock TTL)`. A node the interval was never
/// published to takes none: it has no placement driver, and so no second node that could be a
/// state behind it (`bin/esker-sql.rs`).
///
/// Interruptible throughout, in short sleeps: `statement_timeout` and a cancel both reach a
/// statement that is waiting, the way they reach `pg_sleep`.
pub(super) fn finish_concurrent_build(executor: &mut Executor) -> Result<()> {
    let Some(index_id) = executor.concurrent_build.take() else {
        return Ok(());
    };
    let step_wait = executor
        .backend
        .schema_step_interval()
        .map(|interval| std::time::Duration::from_millis(interval.step_ms));
    // Only an `Overtaken` step leaves the change where it was, and only another driver can cause
    // one — a re-driver adopts a job that has been idle for a whole pass, which this one is not.
    // So this bounds something that should not happen at all, rather than the loop itself: every
    // other step either moves a state or moves the backfill cursor, and the change is finished
    // when its job record is gone.
    let mut overtaken = 0;
    loop {
        super::cancel::check()?;
        // **The lease is checked before every step**, not once at the start: a build is many
        // transactions over minutes, and a node past its lease may be acting on a schema the
        // cluster has moved two states beyond (ADR 0028). Stopping leaves the job for another
        // node, which is what the re-driver is for.
        if executor.backend.schema_lease_remaining().is_none() {
            return Err(SqlError::SchemaLeaseExpired {
                command: "CREATE INDEX CONCURRENTLY",
            });
        }
        let mut txn = executor.plain_read()?;
        let running = catalog::job(&*txn, executor.tenant, index_id)?.is_some();
        if !running {
            // Finished — its own last step forgets the record — or finished by somebody else.
            let _ = txn.rollback();
            return Ok(());
        }
        let stepped = super::verbs::step_job(executor, &mut *txn, index_id, None);
        // Read-only: every write a step makes is in a transaction `crate::exec::job` opens and
        // commits itself, which is what makes a step a step.
        let _ = txn.rollback();
        // A duplicate has already unwound the change by the time this is an error, so the invalid
        // index is in the catalog and this is the client's answer.
        let stepped = stepped?;
        if matches!(stepped, super::verbs::Stepped::Overtaken) {
            overtaken += 1;
            if overtaken > OVERTAKEN_LIMIT {
                return Err(SqlError::Internal(format!(
                    "the concurrent build of index {index_id} was overtaken \
                     {OVERTAKEN_LIMIT} times without finishing"
                )));
            }
        } else {
            overtaken = 0;
        }
        if stepped.starts_the_wait()
            && let Some(wait) = step_wait
        {
            let until = std::time::Instant::now() + wait;
            loop {
                super::cancel::check()?;
                let left = until.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                // Short steps, so a cancel is acted on promptly rather than an interval later.
                std::thread::sleep(left.min(std::time::Duration::from_millis(10)));
            }
        }
    }
}

/// How many times in a row a driven build may be overtaken before it is called stuck.
const OVERTAKEN_LIMIT: u32 = 64;

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
    let (text, ty) = deparsed_expression(table, expr)?;
    // **An index key is stored without its own outermost pair, because the reader puts it back.**
    // A key part carries its [`catalog::ExprShape`] beside its text and every reader of the key
    // list — `pg_get_indexdef`, its `pretty` form, `pg_get_expr(indexprs)`, the per-column
    // rendering — asks the shape how many pairs to print. So the text the catalog holds is the
    // *inside* of the expression, and [`deparse`] hands back the outside: `(b IS NULL)` stored
    // that way read back `((b IS NULL))`, and four rows of `tests/expression_index.rs` said so
    // within the hour. A generated column is the other convention — nothing adds a pair on the
    // way out, so it stores the printed form as it is — which is why these are two functions.
    Ok((catalog::unparenthesised(&text).to_owned(), ty))
}

/// The column an index key names, if what it resolves to is a bare one.
///
/// `CREATE INDEX ON t (((c)::text))` over a `text` column is a **column** index on a real server,
/// not an expression index over a cast: the cast is not a node, so the key is the column and
/// `indkey`, `indexprs` and `pg_get_indexdef` all say so. Measured beside `((c))`, which the
/// parser already unwraps, and beside `(((v)::text))` over a `varchar`, which stays an expression
/// because that cast is real.
///
/// `None` for anything else, including an expression that does not resolve — the caller's
/// `index_expression` raises the refusal, and raising it twice from two places is how two
/// sentences for one cause start.
fn index_key_column(table: &TableDef, expr: &str) -> Option<usize> {
    let parsed = crate::parse::parse_stored_expr(expr).ok()?;
    let scope = crate::exec::query::Scope::single(table);
    match crate::exec::query::resolve(&parsed, &scope).ok()? {
        plan::Expr::Ordinal { at, .. } => Some(at),
        _ => None,
    }
}

/// One expression, checked the way an index key is checked and printed the way `pg_get_expr`
/// prints it — **including its own outermost pair**.
///
/// The refusals are [`index_expression`]'s, listed in its doc comment, and they are the right ones
/// for a generated column too: it is the same requirement (a function of the row alone) for the
/// same reason (the value is written once and read forever), and a real server gives the same
/// `42P17` for a generated column over `nextval`.
/// [`deparsed_expression`] for a **generated column**, which is the same check with PostgreSQL's
/// other sentence for it.
///
/// One requirement — a function of the row alone — and two wordings: an index key is
/// `42P17 functions in index expression must be marked IMMUTABLE` and a generated column is
/// `42P17 generation expression is not immutable`. This node gave the index sentence in both
/// places, which named a construct the statement does not have. Only that one refusal is
/// translated; every other one [`refuse_unless_immutable`] raises already names its own cause
/// (`0A000 the function md5`, `42P02 there is no parameter $1`).
fn generated_column_expression(table: &TableDef, expr: &str) -> Result<(String, ColumnType)> {
    deparsed_expression(table, expr).map_err(|error| match error {
        SqlError::NotImmutableInIndex => SqlError::NotImmutableInGeneratedColumn,
        other => other,
    })
}

fn deparsed_expression(table: &TableDef, expr: &str) -> Result<(String, ColumnType)> {
    let parsed = crate::parse::parse_stored_expr(expr)?;
    let scope = crate::exec::query::Scope::single(table);
    let resolved = crate::exec::query::resolve(&parsed, &scope)?;
    refuse_unless_immutable(&resolved, &scope)?;
    let ty = crate::exec::query::expr_type(&resolved, &scope)?;
    let printed = deparse(&resolved, table, ty);
    if reads_back(table, &printed, ty) {
        return Ok((printed, ty));
    }
    // **The written text, because the printed one could not be read back.** Not a fallback for
    // tidiness: the stored string is *evaluated*, and by three different readers that each parse
    // it again — `exec::index` for every index write, `exec::dml` for a default and for a
    // generation expression — so a text that does not re-parse is a table nobody can insert into.
    // `exec::index` says it in its own words: "the stored expression of index ... no longer
    // resolves". This node's deparser is not total over the expression language: a `CatalogFunc`
    // that is not an operator prints `name(...)`, a literal placeholder, and an index over
    // `to_tsvector('english', name)` stored that way raised `XX000` on the next `INSERT` — which
    // is what `tests/tsvector.rs` and `tests/gin_default_opclass.rs` caught within the hour.
    //
    // Keeping the written text is what this function did for every shape but two before the
    // deparser reached the rest of them, so the fallback is the old behaviour and the guard is
    // what makes reaching for the new one safe.
    Ok((expr.to_owned(), ty))
}

/// Whether a printed expression **parses and resolves back to itself** — the invariant that makes
/// storing a deparse instead of the user's text safe.
///
/// Three callers re-parse the stored string on a path where failure is an `XX000` rather than a
/// refusal, so the question is not "is this the right text" but "can this text be read at all".
/// The check is a **fixpoint** and not just a parse, because a string that parses to a *different*
/// expression is the worse failure: the index key or the generated value would then be computed
/// from something other than what the catalog shows, and nothing would say so.
///
/// A wrapping pair is transparent to the parser, so checking the printed form also clears the
/// index convention's [`catalog::unparenthesised`] form of it.
fn reads_back(table: &TableDef, printed: &str, ty: ColumnType) -> bool {
    let Ok(parsed) = crate::parse::parse_stored_expr(printed) else {
        return false;
    };
    let scope = crate::exec::query::Scope::single(table);
    let Ok(resolved) = crate::exec::query::resolve(&parsed, &scope) else {
        return false;
    };
    deparse(&resolved, table, ty) == printed
}

/// The type a comparison's two sides are printed under.
///
/// **A column decides it, then a literal that has a type of its own, and two unknowns are `text`**
/// — which is PostgreSQL's own resolution order for the pair and is why `('a' < 'b')` prints
/// `('a'::text < 'b'::text)` rather than carrying the `boolean` the comparison answers.
///
/// `None` means neither side says anything, and the caller's fallback is `text`: the only way to
/// reach that is two unknown literals, which is exactly the case PostgreSQL resolves as `text`.
fn comparison_operand_type(
    left: &plan::Expr,
    right: &plan::Expr,
    table: &TableDef,
) -> Option<ColumnType> {
    column_type_of(left, table)
        .or_else(|| column_type_of(right, table))
        .or_else(|| decided_literal_type(left))
        .or_else(|| decided_literal_type(right))
}

/// One argument of a **grammar production**, printed the way PostgreSQL prints it after the
/// production coerced it to the call's common type.
///
/// `GREATEST(b, 1)` on a `bigint` column is `GREATEST(b, (1)::bigint)`: the production coerces
/// both arguments, so the `integer` constant sits under a cast in the tree and the cast prints.
/// `GREATEST(n, 1)` on an `integer` column is `GREATEST(n, 1)` — nothing was coerced, so there is
/// no cast node to print. [`numeric_constant`] already knows which of the three forms a constant
/// of a given type takes; what this adds is *which* type to ask it about.
///
/// **Deliberately not folded into [`deparse_literal`]**, whose integer arm types a literal by its
/// own value on purpose (ADR 0087) and is threaded a type by every caller in the crate — a
/// comparison among them, where PostgreSQL picks a cross-type operator and coerces *nothing*
/// (debt #23). One rule for one caller is the honest shape here: the productions are the callers
/// that coerce.
/// Whether this expression is an **unknown constant** — the shape PostgreSQL's parser coerces
/// straight to a cast's target type instead of building a `text` constant under a cast.
///
/// Two of them, and they are the two [`crate::exec::query::is_unknown_literal`] names. A quoted
/// string: `Literal::String` is one the parser has not typed yet and `Literal::Typed` over a
/// `Datum::Text` is the same constant after `parse::fold_column_default` gave it a type — both
/// were written `'...'` and both print as one node. And a bare `NULL`, for the same reason.
fn folded_into_its_cast(expr: &plan::Expr) -> bool {
    match expr {
        // The two unknown constants. **A bare `NULL` is one of them**, and the cast over it is the
        // same single node a quoted string's is: `(NULL)::text` is `NULL::text` on a real server,
        // not `(NULL::text)::text`. This node printed the type twice — once from the literal,
        // which has to name a type because nothing else would, and once from the cast — and group
        // E is what made that visible: the no-op elision collapses the re-parse of the doubled
        // form, so `reads_back` refused the whole expression and the written text was kept
        // instead. A `TypedNull` is *not* here: the user named a type, and `(NULL::integer)::text`
        // is a cast a real server keeps.
        plan::Expr::Literal(plan::Literal::String(_) | plan::Literal::Null) => true,
        plan::Expr::Literal(plan::Literal::Typed(value)) => {
            matches!(value.as_ref(), Datum::Text(_))
        }
        _ => false,
    }
}

/// That constant, printed with the type the cast named rather than the type it is held as.
fn deparse_literal_of(expr: &plan::Expr, to: ColumnType) -> String {
    match expr {
        plan::Expr::Literal(plan::Literal::Typed(value)) => match value.as_ref() {
            Datum::Text(text) => format!("'{}'::{}", text.replace('\'', "''"), to.name()),
            _ => deparse_literal(&plan::Literal::Null, to),
        },
        plan::Expr::Literal(literal) => deparse_literal(literal, to),
        _ => String::new(),
    }
}

/// The type a **grammar production** coerces its arguments to: the common type of the arguments
/// themselves, and not the type the parent threaded down.
///
/// The parent's type is wrong wherever the production is not the whole expression. Inside a
/// `CHECK`, `greatest(b, 1) > 0` deparses the comparison first and threads its *operand* type
/// down, which for a call on the left is whatever the literal on the right decided — `integer` —
/// so the `bigint` column's own width never reached the constant and `GREATEST(b, 1)` printed
/// where a real server prints `GREATEST(b, (1)::bigint)`. Measured through
/// `pg_get_constraintdef`, which is the reader that found it.
///
/// The reduction is `exec::query::greatest_type`'s, over what a `plan::Expr` can say about itself:
/// a column's declared type, or a literal's own. `None` from both — two untyped string literals,
/// `LEAST('a', 'b')` — falls back to the parent's, which is where the type came from there.
fn production_common_type(args: &[plan::Expr], table: &TableDef, ty: ColumnType) -> ColumnType {
    args.iter()
        .filter_map(|arg| column_type_of(arg, table).or_else(|| decided_literal_type(arg)))
        .reduce(|sofar, one| {
            if sofar == one {
                sofar
            } else {
                crate::value::arith::result_type(plan::ArithOp::Add, sofar, one).unwrap_or(sofar)
            }
        })
        .unwrap_or(ty)
}

fn deparse_production_argument(arg: &plan::Expr, table: &TableDef, ty: ColumnType) -> String {
    if let plan::Expr::Literal(plan::Literal::Integer(value)) = arg {
        let own = if i32::try_from(*value).is_ok() {
            ColumnType::Int4
        } else {
            ColumnType::Int8
        };
        if own != ty {
            return numeric_constant(&value.to_string(), ty);
        }
    }
    deparse(arg, table, ty)
}

/// The type each argument of a catalog function is **printed under**, or `None` for a function
/// whose parameters have not been measured.
///
/// `pg_get_expr` shows the coercion a literal argument took, and the coercion is to the
/// *parameter's* type rather than to the call's: `btrim(t, 'x'::text)`, and
/// `split_part(t, ','::text, 1)` where the third argument is an `integer` and prints bare. So a
/// call is deparsed by threading this list down, one type per argument, which is the same
/// mechanism [`deparse`]'s operator arms use for an operand.
///
/// **`None` keeps the placeholder, and that is the safe answer rather than a gap.** A function
/// whose parameters are not in this table deparses to `name(...)`, which [`reads_back`] refuses,
/// so the expression keeps the text it was written with — right in the sense that it re-parses to
/// the same value, and wrong in the characters. Printing an unmeasured coercion would be worse:
/// it might read back, and then a stored expression would carry a cast this node invented.
/// Three functions are out for exactly that reason: `setweight(tv, 'A'::"char")` and
/// `to_tsvector('english'::regconfig, t)` name types this node does not have (`"char"` is
/// PostgreSQL's one-byte type, not `char(n)`), and there is no `ColumnType` to thread for either.
fn catalog_parameter_types(func: plan::CatalogFunc, argc: usize) -> Option<&'static [ColumnType]> {
    use crate::plan::CatalogFunc as Func;
    const TEXT: ColumnType = ColumnType::Text;
    const INT: ColumnType = ColumnType::Int4;
    Some(match (func, argc) {
        (Func::Btrim | Func::Ltrim | Func::Rtrim, 1) => &[TEXT],
        (Func::Btrim | Func::Ltrim | Func::Rtrim | Func::StrPos | Func::StringToArray, 2) => {
            &[TEXT, TEXT]
        }
        (Func::Replace, 3) => &[TEXT, TEXT, TEXT],
        (Func::SplitPart, 3) => &[TEXT, TEXT, INT],
        (Func::Substr, 2) => &[TEXT, INT],
        (Func::Substr, 3) => &[TEXT, INT, INT],
        // No literal argument is possible, so the type threaded is only the one a column ignores
        // — listed anyway, because being in this table is what makes the call print at all.
        (Func::TsStrip, 1) => &[ColumnType::TsVector],
        _ => return None,
    })
}

/// The `(left, right)` types of a catalog function **spelled as an operator**, which prints in its
/// own pair rather than as a call — or `None` for one that prints as a call.
///
/// `pg_get_expr` gives `(t || 'x'::text)` and not `||(t, 'x')`, and the coercion on the right is
/// the operator's own argument type: `(j || '{}'::jsonb)` for `jsonb`, `(j -> 'k'::text)` for a
/// fetch whose key is always `text` whatever the document is. `parse::lower` makes every `||` a
/// `CatalogFunc` — hstore, jsonb and text alike — so the pair has to come from the *variant* and
/// not from the symbol, which is what this table is and what the earlier `name() == "||"` guard
/// could not be: it printed a `jsonb` literal as `'{}'::text`.
fn operator_operand_types(func: plan::CatalogFunc) -> Option<(ColumnType, ColumnType)> {
    use crate::plan::CatalogFunc as Func;
    match func {
        // `textcat(text, text)`, which is what every `||` that is not a document merge lowers to.
        Func::HstoreConcat => Some((ColumnType::Text, ColumnType::Text)),
        Func::JsonbConcat => Some((ColumnType::Jsonb, ColumnType::Jsonb)),
        // `->` and `->>`: the document on the left keeps its own type (a column prints its name
        // and ignores what is threaded, and [`deparse`] prefers the column's type anyway), and the
        // key on the right is `text` for all three spellings — measured on `jsonb`, `json` and
        // `hstore`.
        Func::JsonFetch | Func::JsonbFetch | Func::JsonFetchText | Func::HstoreFetch => {
            Some((ColumnType::Jsonb, ColumnType::Text))
        }
        _ => None,
    }
}

/// The name a catalog function's call prints with, quoted where PostgreSQL quotes it.
///
/// Four of these are **grammar productions** and print upper-cased — `COALESCE`, `GREATEST`,
/// `LEAST`, `NULLIF` — and are handled by their own arms in [`deparse`] because their arguments
/// follow a different rule. What is left is the ordinary lower-case name, with one exception:
/// `substring` is a **reserved word**, so a call to it prints `"substring"(t, 1, 2)` where
/// `substr(t, 1, 2)` beside it prints bare. Measured, both.
fn printed_call_name(func: plan::CatalogFunc) -> String {
    catalog::quote_identifier(func.name())
}

/// The type a literal carries on its own, or `None` for the two PostgreSQL calls `unknown`.
///
/// The same question [`crate::exec::query::literal_type`] answers, asked where only a `plan::Expr`
/// is in hand; a quoted string and a bare `NULL` take their type from the other side, and
/// everything else brings one.
fn decided_literal_type(expr: &plan::Expr) -> Option<ColumnType> {
    match expr {
        plan::Expr::Literal(literal) => crate::exec::query::literal_type(literal),
        _ => None,
    }
}

/// Whether a scalar function takes `text`, and so shows a cast its argument needed.
///
/// Six of the seven do; `abs` is the numeric one. Measured rather than read off the names:
/// `length((v)::text)`, `upper((c)::text)`, `octet_length((v)::text)`, `abs(n)`.
fn takes_text(func: plan::ScalarFunc) -> bool {
    use crate::plan::ScalarFunc;
    match func {
        ScalarFunc::Lower
        | ScalarFunc::Upper
        | ScalarFunc::Reverse
        | ScalarFunc::Ascii
        | ScalarFunc::Length
        | ScalarFunc::OctetLength => true,
        ScalarFunc::Abs => false,
    }
}

/// The declared type of an expression that is exactly one column of this table, or `None`.
///
/// Only a column takes a visible cast: a literal carries its own type annotation already, and a
/// nested call's result is whatever that call returns.
fn column_type_of(expr: &plan::Expr, table: &TableDef) -> Option<ColumnType> {
    match expr {
        plan::Expr::Ordinal { at, .. } => table.columns.get(*at).map(|column| column.ty),
        _ => None,
    }
}

/// Rewrites every generation expression into the form `pg_get_expr` prints, once, where the table
/// is finally known.
///
/// **PostgreSQL stores a tree and prints a deparse**, so the text that comes back is never quite
/// the text that went in: `GENERATED ALWAYS AS (UPPER(name))` over a `varchar` reads back
/// `upper((name)::text)` — lower-cased, and with the cast the argument took — which
/// `virtual_column_test#test_schema_dumping` asserts to the character. This crate stores text, so
/// the normalisation happens here rather than at every reader: the catalog is a layer below the
/// executor and cannot call a deparser, and doing it per read would deparse the same string on
/// every `pg_attribute` scan.
///
/// Only the shapes [`index_expression`] deparses are rewritten, and for the same reason.
/// The same pass for a **`DEFAULT`**, which is the same `pg_attrdef` row.
///
/// Three statements store one, and each stores the text it was written with, so each needs this:
/// `CREATE TABLE` here, `ALTER TABLE ... ADD COLUMN` and `ALTER TABLE ... ALTER COLUMN SET
/// DEFAULT` at their own sites. That is the shape of every bug in this family — a rule measured
/// once and asked in one of the places that stores the thing — and the count is written here so
/// the next reader can check it: `grep -n 'deparse_default' src/exec/ddl.rs` must find three
/// callers.
///
/// **Table-wide is safe here and nowhere else**, because every text in a table this function is
/// handed is the text a user just wrote. Run it a second time over its own output and
/// `upper('a'::text)` would deparse again; the two `ALTER` sites therefore normalise the *one*
/// column the statement writes and leave the others alone.
fn normalise_defaults(table: &mut TableDef) {
    let snapshot = table.clone();
    for column in &mut table.columns {
        let Some(expr) = &column.default_expr else {
            continue;
        };
        if let Some(text) = deparse_default(&snapshot, expr) {
            column.default_expr = Some(text);
        }
    }
}

/// A printed block, moved four spaces to the right.
///
/// What a nested `CASE` needs: PostgreSQL indents the inner one by four relative to the outer, on
/// every line including its `CASE` and `END`. The inner call has already produced the block, so
/// the outer one only has to move it — which is why the `Case` arm needs no depth parameter.
fn indented(block: &str) -> String {
    block
        .split('\n')
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A body in parentheses, with the line break PostgreSQL puts after the `(` when a `CASE` follows.
///
/// `CHECK (\nCASE …)`, `btree ((\nCASE …))`, `(\nCASE …)::text` — the break belongs to the pair
/// and not to the keyword, which is why [`deparse`]'s `Case` arm emits none of its own.
fn parenthesise(body: &str) -> String {
    if body.starts_with("CASE") {
        format!("(\n{body})")
    } else {
        format!("({body})")
    }
}

/// An operand with the separator that goes in front of it — a space, or **a line break when the
/// operand is a `CASE`**.
///
/// A real server starts a `CASE` on its own line wherever it sits, not only at the head of a
/// parenthesised body: `(a +\nCASE …)`, `(flag AND\nCASE …)`, `(NOT\nCASE …)`, measured through a
/// `CHECK` in all three positions. The break *replaces* the space rather than following it, so
/// there is no trailing blank at the end of the line — which is a difference a byte comparison
/// sees. [`parenthesise`] is the same rule one position earlier, where the `(` is what precedes.
///
/// **An operand that already begins with the break keeps it and takes no space.** Flattening a
/// chain strips the pair off a nested one, and what is left can start with the newline the inner
/// call put there: `a AND (CASE … AND b)` gives `\nCASE … END AND (b)` for the right-hand side. A
/// space in front of that is a line ending in one, and nothing else about the text would say so.
fn spaced(operand: &str) -> String {
    if operand.starts_with("CASE") {
        format!("\n{operand}")
    } else if operand.starts_with('\n') {
        operand.to_owned()
    } else {
        format!(" {operand}")
    }
}

/// One boolean expression, printed exactly the way [`deparse`] prints it.
///
/// **The readers add nothing, and that is the whole of group A.** A `CHECK` and a partial index's
/// predicate used to be stored flat and re-parenthesised at read time by
/// `catalog::parenthesised_operands`, which split on the top-level keyword and wrapped each piece.
/// A splitter cannot say which operands bind first: `(a > 0 OR b > 0) AND flag` is
/// `(((a > 0) OR (b > 0)) AND flag)` on a real server and came back
/// `(((a > 0)) OR ((b > 0)) AND flag)` here — the grouping lost and a pair doubled. The tree has
/// the grouping and `deparse` already writes it, so the writer writes all of it and the reader
/// prints what it was given.
fn deparse_wrapped_predicate(table: &TableDef, expr: &str) -> Option<String> {
    deparse_default(table, expr)
}

/// [`deparse_wrapped_predicate`] for a caller outside this module: the form a predicate is
/// **stored** in, so that a comparison against a stored one compares like with like.
///
/// `ON CONFLICT` infers a partial index by comparing its predicate as *text*
/// (`exec::dml::same_predicate`), and once a stored predicate is deparsed, the text a statement
/// writes is no longer the text the catalog holds — `WHERE "b" IS NOT NULL` against
/// `b IS NOT NULL`, which cost a working statement a `42P10` the first time this was tried. So the
/// arbiter puts what the statement wrote through the same printer before comparing. `None` means
/// the printer has nothing to say about this predicate, and the caller compares the text as
/// written, which is what it did before.
pub(super) fn stored_predicate_form(table: &TableDef, expr: &str) -> Option<String> {
    deparse_wrapped_predicate(table, expr)
}

/// Every partial index's predicate on this table, printed the way `pg_get_expr(indpred)` prints
/// one — the sixth reader, and the one `ActiveRecord` reads to dump a `WHERE`.
fn normalise_index_predicates(table: &mut TableDef) {
    let snapshot = table.clone();
    for index in &mut table.indexes {
        let Some(predicate) = &index.predicate else {
            continue;
        };
        if let Some(text) = deparse_wrapped_predicate(&snapshot, predicate) {
            index.predicate = Some(text);
        }
    }
}

/// Every `EXCLUDE`'s predicate on this table, printed the way its index's `pg_get_expr(indpred)`
/// prints one.
///
/// **The seventh reader, and it was reading a text no writer had shaped.** An exclusion
/// constraint carries its own predicate rather than an [`catalog::IndexDef`]'s, and
/// `catalog::pg_index` builds the index row straight from it — so the `WHERE` came back as the
/// user wrote it, `WHERE start_date IS NOT NULL AND end_date IS NOT NULL`, where a real server
/// gives `WHERE ((start_date IS NOT NULL) AND (end_date IS NOT NULL))`. Measured through both of
/// its readers at once: `pg_get_constraintdef` wraps this form once more and
/// `pg_get_indexdef` prints it as it stands.
fn normalise_exclude_predicates(table: &mut TableDef) {
    let snapshot = table.clone();
    for exclude in &mut table.excludes {
        let Some(predicate) = &exclude.predicate else {
            continue;
        };
        if let Some(text) = deparse_wrapped_predicate(&snapshot, predicate) {
            exclude.predicate = Some(text);
        }
    }
}

/// Every `CHECK` on this table, printed the way `pg_get_constraintdef` prints one.
///
/// **The fifth reader of [`deparse`]**, after a generated column, a `DEFAULT`, an index key and a
/// partial index's predicate. `CHECK (greatest(b, 1) > 0)` on a `bigint` column is
/// `CHECK ((GREATEST(b, (1)::bigint) > 0))` on a real server, and the written text is neither
/// upper-cased nor coerced — measured. A check whose expression does not deparse keeps its text,
/// which is [`deparse_default`]'s own contract.
fn normalise_checks(table: &mut TableDef) {
    let snapshot = table.clone();
    for check in &mut table.checks {
        if let Some(text) = deparse_wrapped_predicate(&snapshot, &check.expr) {
            check.expr = text;
        }
    }
}

fn normalise_generated(table: &mut TableDef) -> Result<()> {
    let snapshot = table.clone();
    for column in &mut table.columns {
        let Some(expr) = &column.generated else {
            continue;
        };
        let (text, _) = generated_column_expression(&snapshot, expr)?;
        column.generated = Some(text);
    }
    Ok(())
}

/// One numeric constant, in the form `pg_get_expr` prints it.
///
/// **Three forms, and the choice is derivable from the value and the type together** — measured on
/// 19beta1, `tests/corpus/pg19_numeric_literal_deparse.txt`:
///
/// ```text
/// 1                    1                               an integer literal's own type
/// 1.5                  1.5                             a decimal literal's own type
/// 1::smallint          (1)::smallint
/// 1::bigint            (1)::bigint
/// 1::numeric           (1)::numeric
/// 1.5::float8          (1.5)::double precision
/// -1                   '-1'::integer
/// 9223372036854775807  '9223372036854775807'::bigint
/// 99999999999999999999999999   '99999999999999999999999999'::numeric
/// ```
///
/// PostgreSQL prints the **node**: `(N)::type` is a cast over a constant of the literal form's own
/// type, and `'N'::type` is a bare constant whose type is not that form's. This node folds a cast
/// into the constant, so it has no cast node to read — but it does not need one, because *which
/// node PostgreSQL had is decidable from the constant alone*:
///
/// * an `int8` whose value **fits** `int4` can only have come from a cast, since a literal that
///   small is an `int4`; one that does **not** fit is what the scanner itself produces, so it is a
///   bare constant. The same test one type up decides a `numeric`: integral and inside `int8` is a
///   cast, integral and wider is a literal;
/// * `smallint`, `real` and `double precision` have **no literal syntax at all**, so a constant of
///   one always came from a cast;
/// * a **negative** constant is constant folding having eaten a unary minus, which is a bare
///   constant by construction.
///
/// So the three forms are reachable without the cast node, and the rule is a property of the pair
/// rather than a guess. The one shape this cannot reproduce is a *negative* constant of a
/// non-default type — `(-1)::bigint` is `('-1'::integer)::bigint` on a real server, two nodes deep
/// — and it is declared rather than approximated.
fn numeric_constant(printed: &str, ty: ColumnType) -> String {
    // **One rule, in two steps**: print the constant under the type the *digits* would have had,
    // then wrap that in a cast if the type wanted is not that one. The three forms fall out of it
    // rather than being enumerated, which is what [`natural_constant_type`] and
    // [`constant_in_its_own_type`] are between them; `tests/corpus/pg19_negative_constant.txt`
    // is the measurement, over both signs and every target type.
    let natural = natural_constant_type(printed);
    let inner = constant_in_its_own_type(printed, natural);
    if ty == natural {
        return inner;
    }
    format!("({inner})::{}", ty.name())
}

/// The type PostgreSQL's scanner gives a numeric constant written with these characters.
///
/// `integer` if the digits fit one, `bigint` if they fit that, `numeric` for anything wider and
/// for anything with a decimal point (ADR 0087's ladder, read from the printing end). **The sign
/// is part of the value here**: `-1` is an `integer` and `-9223372036854775807` a `bigint`, which
/// is what makes the wrap below decidable — measured, `(-9223372036854775807)::numeric` is
/// `('-9223372036854775807'::bigint)::numeric` and not `('-9223372036854775807'::integer)::…`.
fn natural_constant_type(printed: &str) -> ColumnType {
    if printed.contains(['.', 'e', 'E']) {
        return ColumnType::Numeric;
    }
    if printed.parse::<i32>().is_ok() {
        return ColumnType::Int4;
    }
    if printed.parse::<i64>().is_ok() {
        return ColumnType::Int8;
    }
    ColumnType::Numeric
}

/// That constant printed under its own type: **bare** where the type has a literal syntax for the
/// value, and quoted with a type annotation where it does not.
///
/// Two spellings have a literal syntax and no others: an unsigned run of digits is an `integer`
/// and an unsigned decimal is a `numeric`. Everything else is a `Const` PostgreSQL prints as
/// `'…'::type` — a value too wide for the literal form's type, and **every negative value**,
/// because a negative constant is a unary minus folded into the constant by the scanner and so was
/// never a literal at all. That is the whole reason `(-1)::bigint` is `('-1'::integer)::bigint`
/// where `(1)::bigint` is `(1)::bigint`.
fn constant_in_its_own_type(printed: &str, natural: ColumnType) -> String {
    let integral = !printed.contains(['.', 'e', 'E']);
    let literal_form = !printed.starts_with('-')
        && match natural {
            ColumnType::Int4 => integral,
            ColumnType::Numeric => !integral,
            _ => false,
        };
    if literal_form {
        return printed.to_owned();
    }
    format!("'{printed}'::{}", natural.name())
}

/// One `DEFAULT`, as `pg_get_expr` prints it — for the shapes where that is not what was written.
///
/// **A default and a generated column are the same `pg_attrdef` row**, printed by the same
/// `pg_get_expr(adbin, adrelid)`, so the deparse rule is one rule; a generated column has been
/// through [`deparse`] since the schema-dump unit and a default never was. Measured on 19beta1,
/// `tests/corpus/pg19_generated_parens.txt`:
///
/// ```text
/// DEFAULT (1 + 2 * 3)   ->  (1 + (2 * 3))     every operator node takes its own pair
/// DEFAULT upper('a')    ->  upper('a'::text)  an unknown literal shows the coercion it took
/// DEFAULT (1 + 1)       ->  (1 + 1)           already agreed, and still does
/// DEFAULT 1::bigint     ->  (1)::bigint       already agreed, from `cast_default_text`
/// ```
///
/// **`None` means "keep what was written", and that is the answer for most defaults**, because for
/// most of them the written text *is* what a real server prints: `now()`, `CURRENT_TIMESTAMP`,
/// `nextval('s'::regclass)`, `gen_random_uuid()`, `7`. Two of those are the reason this is an
/// allow-list of shapes rather than "deparse everything": PostgreSQL keeps `CURRENT_TIMESTAMP` and
/// `now()` apart in its own tree and prints each back as written — measured, and
/// `catalog::pg_attribute::default_expression` records it — while this crate lowers both to one
/// node and could only print one of them. Deparsing them would lose a spelling that agrees today.
///
/// The refusals are swallowed on purpose. This runs over text a real server has already accepted,
/// and a shape [`crate::exec::query::resolve`] does not know is one whose written text is kept —
/// the same answer as a shape that is deliberately out of the list. A default is refused where it
/// is *written* (`parse::lower::refuse_default_shapes`), not here.
fn deparse_default(table: &TableDef, expr: &str) -> Option<String> {
    let parsed = crate::parse::parse_stored_expr(expr).ok()?;
    let scope = crate::exec::query::Scope::single(table);
    let resolved = crate::exec::query::resolve(&parsed, &scope).ok()?;
    if !reprinted_by_pg_get_expr(&resolved) {
        return None;
    }
    let ty = crate::exec::query::expr_type(&resolved, &scope).ok()?;
    let printed = deparse(&resolved, table, ty);
    // The same guard [`deparsed_expression`] carries, for the same reason and a third reader:
    // `exec::dml::column_default_value` parses this string for every row that takes the default.
    reads_back(table, &printed, ty).then_some(printed)
}

/// Whether PostgreSQL's deparser prints this shape differently from the way it is written.
///
/// Total over the expression type, like [`deparse`] itself and for the same reason: a new node has
/// to decide before it compiles. The `false` arms are not a backlog — each is a shape whose
/// written text already agrees, and several of them agree *because* they are not deparsed.
///
/// It has to be **exhaustive and not a list beside a wildcard**, and clippy is what says so:
/// `match_same_arms` rejects an explicit group and a `_` arm that return the same value, so the
/// choice is between listing every variant and listing none. Listing every variant is the one that
/// keeps the promise of the sentence above, and the grouping below is where the reasons live.
fn reprinted_by_pg_get_expr(expr: &plan::Expr) -> bool {
    use crate::plan::Expr;
    match expr {
        // Operators, which take a pair per node at every depth, and calls whose arguments show the
        // coercion they took. These are the shapes the corpus measured a difference on.
        Expr::Arithmetic { .. }
        | Expr::Binary { .. }
        // **A `Negate` that survives lowering was written**, so it belongs with the operators.
        // It used to be on the "nothing to reprint" list below, with the reason that a folded
        // negative literal is stored as a *value* — `DEFAULT - 1` is the `Datum` `-1` and no
        // `Negate` node reaches here at all. True, and not the whole list: `::` binds tighter than
        // unary minus, so `DEFAULT -1::bigint` is an operator **over a cast** and does keep its
        // node. PostgreSQL prints it `(- (1)::bigint)`, and the operand — an `int8` holding a
        // value that fits an `int4`, so it can only have come from a cast — already prints
        // `(1)::bigint` through [`numeric_constant`]. Exactly the correction `Literal` got one
        // revision earlier, for the same reason: true of the value, false of the text
        // (`debts-v1.1.md` #30).
        | Expr::Negate(_)
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::Like { .. }
        | Expr::RegexMatch { .. }
        | Expr::InList { .. }
        | Expr::QuantifiedArray { .. }
        | Expr::Cast { .. }
        | Expr::ToText { .. }
        | Expr::Scalar { .. }
        | Expr::Coalesce(_)
        // **A `COLLATE` gains a pair of its own**: `upper(t COLLATE "C")` prints back as
        // `upper((t COLLATE "C"))`, measured — which is why it is on this list and why the
        // clause is kept in the plan at all (ADR 0096).
        | Expr::Collate { .. }
        | Expr::Case { .. } => true,
        // **A catalog function whose name is an operator**, which today is `||` and which
        // [`deparse`] prints as one. Gated on the same condition `deparse`'s own arm uses, because
        // every other `CatalogFunc` deparses to a `name(...)` placeholder that must never be
        // stored: `DEFAULT ('a' || 'b')` prints `('a'::text || 'b'::text)`, measured.
        // **A catalog function whose arguments [`deparse`] can print**, which is three sets: one
        // spelled as an operator ([`operator_operand_types`]), one of the four grammar productions
        // that print upper-cased, and one whose parameter types are measured
        // ([`catalog_parameter_types`]). Every other one deparses to a `name(...)` placeholder that
        // must never be stored: `DEFAULT ('a' || 'b')` prints `('a'::text || 'b'::text)`,
        // `btrim(t, 'x')` prints `btrim(t, 'x'::text)`, and `setweight(tv, 'A')` prints nothing
        // this node can produce, so its written text is kept.
        // **A numeric constant on its own**, which is a shape `pg_get_expr` really does reprint:
        // `DEFAULT (-1)::bigint` is `('-1'::integer)::bigint` there and was kept as written here,
        // because a folded cast leaves a bare `Literal` and this list had none. `numeric_constant`
        // is PostgreSQL's own `get_const_expr` for these, so deparsing them loses nothing — and
        // for the constants that already agreed it prints the same characters (`7` is `7`).
        //
        // Numeric only. A string, a boolean and a `NULL` are left where they were: `DEFAULT 'x'`
        // and `DEFAULT true` agree as written, and adding them would be a change with no
        // measurement behind it.
        Expr::Literal(literal) => matches!(
            literal,
            plan::Literal::Integer(_) | plan::Literal::Decimal(_)
        ) || matches!(literal, plan::Literal::Typed(value)
                if matches!(**value, Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)
                    | Datum::Numeric(_) | Datum::Real(_) | Datum::Double(_))),
        Expr::CatalogFunc(call) => {
            (operator_operand_types(call.func).is_some() && call.args.len() == 2)
                || matches!(
                    call.func,
                    plan::CatalogFunc::Greatest
                        | plan::CatalogFunc::Least
                        | plan::CatalogFunc::NullIf
                        | plan::CatalogFunc::Substring
                        | plan::CatalogFunc::Mod
                )
                || catalog_parameter_types(call.func, call.args.len()).is_some()
        }
        // **Everything else keeps the text it was written with, and is listed rather than
        // wildcarded** so that a new expression node has to decide before it compiles — the same
        // reason [`deparse`] is total. One arm, because `match_same_arms` will not have two, and
        // three reasons, which is what the comments inside it are for.
        //
        // *The spelling is the thing being preserved.* PostgreSQL keeps `CURRENT_TIMESTAMP` and
        // `now()` apart in its own tree and prints each back as written; this node lowers both to
        // one node and could only print one of them, so deparsing here would lose an agreement.
        // `Sequence` is the same case with a stronger reason: `nextval('s'::regclass)` is
        // assembled by `parse::lower` and `deparse` prints `nextval()`.
        Expr::Sequence(_)
        | Expr::Uuid(_)
        | Expr::CurrentUser
        | Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentSetting { .. }
        | Expr::Advisory { .. }
        // *There is nothing to reprint.*
        | Expr::Array { .. }
        | Expr::Subscript { .. }
        // *It cannot be written in a `DEFAULT` at all.* A column reference, a subquery and a
        // set-returning function are the three `parse::lower::refuse_default_shapes` refuses, by
        // PostgreSQL's own rule that a default is evaluated with no row in scope and one value
        // out; an aggregate, a parameter and the plan-internal nodes have nowhere to come from.
        | Expr::Column { .. }
        | Expr::Ordinal { .. }
        | Expr::Outer { .. }
        | Expr::Subquery(_)
        | Expr::Aggregate(_)
        | Expr::SetFunc(_)
        | Expr::Parameter(_)
        | Expr::Default => false,
    }
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
#[allow(
    clippy::too_many_lines,
    reason = "one arm per expression shape, which is what a deparser is"
)]
fn deparse(expr: &plan::Expr, table: &TableDef, ty: ColumnType) -> String {
    use crate::plan::Expr;
    let sub = |expr: &Expr| deparse(expr, table, ty);
    match expr {
        // **A name is quoted the way `quote_ident` quotes it**, which is one call and not a rule
        // written here: [`catalog::quote_identifier`] carries PostgreSQL 19's own
        // `pg_get_keywords()` answer. Printing it bare made `CHECK (("primary" > 0))` come back
        // `CHECK ((primary > 0))`, which is not only a different string — it does not re-parse,
        // and the index readers only *looked* right because `reads_back` refused the bare form
        // and the written text survived. `tests/corpus/pg19_quoted_identifier.txt` measures all
        // five readers, and `value` and `name` are the pair that says this is a measured list
        // rather than a guess about keywords: both are keywords and both print bare.
        Expr::Ordinal { at, .. } => table.columns.get(*at).map_or_else(
            || format!("<column {at}>"),
            |column| catalog::quote_identifier(&column.name),
        ),
        Expr::Literal(literal) => deparse_literal(literal, ty),
        // **Its own pair, and the name quoted.** `upper(t COLLATE "C")` is stored and
        // printed by a real server as `upper((t COLLATE "C"))`; this node dropped the
        // clause at lowering and printed `upper(t)`, so the catalog disagreed with the
        // statement that wrote it (ADR 0096, `tests/corpus/pg19_collation_family.txt`).
        Expr::Collate { operand, collation } => {
            format!("({} COLLATE \"{collation}\")", sub(operand))
        }
        Expr::Array { elements, .. } => {
            format!(
                "ARRAY[{}]",
                elements.iter().map(sub).collect::<Vec<_>>().join(", ")
            )
        }
        // **`LIKE` is printed as the operator it desugars to**, and there are four of them.
        // Measured on 19beta1 (`tests/corpus/pg19_deparse_parens.txt`, the `c_*like` rows):
        //
        // ```text
        // t LIKE 'a%'        (t ~~ 'a%'::text)
        // t NOT LIKE 'a%'    (t !~~ 'a%'::text)
        // t ILIKE 'a%'       (t ~~* 'a%'::text)
        // t NOT ILIKE 'a%'   (t !~~* 'a%'::text)
        // ```
        //
        // The keyword form is what a user writes and is not what `pg_get_expr` prints, which is the
        // whole reason a printer over the tree exists: no rule about parentheses over the written
        // text can turn `LIKE` into `~~`.
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            ..
        } => {
            // Both sides under `text`, which is the operator's argument type and not the
            // expression's: threading the `boolean` a generated column declares printed
            // `(t LIKE 'a%'::boolean)`.
            let side = |expr: &Expr| deparse(expr, table, ColumnType::Text);
            format!(
                "({} {}{} {})",
                side(operand),
                if *negated { "!" } else { "" },
                if *case_insensitive { "~~*" } else { "~~" },
                side(pattern)
            )
        }
        Expr::RegexMatch {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "({} {} {})",
            sub(operand),
            plan::regex_operator(*negated, *case_insensitive),
            sub(pattern)
        ),
        // **A comparison's sides are deparsed under the *operand's* type, not the expression's.**
        // The same correction the `||`, `LIKE` and `IN` arms carry: threading the `boolean` a
        // comparison answers made `('a' < 'b')` print `('a'::boolean < 'b'::boolean)`, which
        // `reads_back` then refused, so the written text was kept and the coercion PostgreSQL
        // prints — `('a'::text < 'b'::text)` — was lost. Measured,
        // `tests/corpus/pg19_collation_family.txt`'s `DEFAULT ('a' < 'b')` row.
        //
        // `AND` and `OR` are in this variant too and take the expression's own type: their sides
        // *are* booleans, and giving them an operand type would be the same mistake mirrored.
        Expr::Binary { op, left, right } => {
            let operand = if matches!(op, plan::BinaryOp::And | plan::BinaryOp::Or) {
                ty
            } else {
                comparison_operand_type(left, right, table).unwrap_or(ColumnType::Text)
            };
            // **`IS NOT DISTINCT FROM` is a negation on a real server, not an operator.**
            // PostgreSQL holds `NOT (a IS DISTINCT FROM b)` and prints exactly that, where this
            // node held one operator and printed its own spelling. Measured across the census's
            // readers; the *values* were never in question, only which node the tree has.
            if *op == plan::BinaryOp::NotDistinct {
                return format!(
                    "(NOT ({} {} {}))",
                    deparse(left, table, operand),
                    plan::BinaryOp::Distinct.symbol(),
                    deparse(right, table, operand)
                );
            }
            // **`AND` and `OR` are n-ary on a real server, and binary here.** PostgreSQL's
            // `BoolExpr` holds a list, so `a AND b AND c AND d` written without parentheses is one
            // node and prints `((a) AND (b) AND (c) AND (d))` — flat. This node holds a tree, and
            // `parse::lower::balance` folds a chain *pairwise* to keep it `log2(n)` deep, so the
            // same four terms are `(a AND b) AND (c AND d)` here: neither side is the spine.
            // Flattening only the left printed `((a) AND (b) AND ((c) AND (d)))`, which is a real
            // server's answer — to the question `CHECK ((a AND b) AND (c AND d))`, measured — and
            // not the one that was asked.
            //
            // **So both sides are flattened, and the written grouping is not recoverable.** A
            // chain the user *did* parenthesise in the middle lowers to the same balanced tree as
            // one they did not, so there is nothing left to tell the two apart; the flat form is
            // what the common shape asks for, and PostgreSQL's pretty spelling flattens both in
            // any case (`a AND b AND c AND d` for either). The census records the one direction
            // this leaves: a mid-chain pair the user wrote is not printed back.
            if matches!(op, plan::BinaryOp::And | plan::BinaryOp::Or) {
                let chained =
                    |side: &Expr| matches!(side, Expr::Binary { op: inner, .. } if inner == op);
                if chained(left) || chained(right) {
                    let side = |side: &Expr| {
                        let text = deparse(side, table, operand);
                        if chained(side) {
                            catalog::unparenthesised(&text).to_owned()
                        } else {
                            text
                        }
                    };
                    return parenthesise(&format!(
                        "{} {}{}",
                        side(left),
                        op.symbol(),
                        spaced(&side(right))
                    ));
                }
            }
            parenthesise(&format!(
                "{} {}{}",
                deparse(left, table, operand),
                op.symbol(),
                spaced(&deparse(right, table, operand))
            ))
        }
        Expr::Negate(operand) => format!("(- {})", sub(operand)),
        Expr::Arithmetic {
            op, left, right, ..
        } => parenthesise(&format!(
            "{} {}{}",
            sub(left),
            op.symbol(),
            spaced(&sub(right))
        )),
        Expr::Not(operand) => parenthesise(&format!("NOT{}", spaced(&sub(operand)))),
        Expr::IsNull { operand, negated } => format!(
            "({} IS {}NULL)",
            sub(operand),
            if *negated { "NOT " } else { "" }
        ),
        // **`IN` is printed as a quantified comparison over an array**, which is what the parser
        // makes of it and what `pg_get_expr` prints. Measured, same corpus:
        //
        // ```text
        // c1 IN (1, 2)        (c1 = ANY (ARRAY[1, 2]))
        // c1 NOT IN (1, 2)    (c1 <> ALL (ARRAY[1, 2]))
        // t IN ('a', 'b')     (t = ANY (ARRAY['a'::text, 'b'::text]))
        // ```
        //
        // The third row is why the elements are deparsed under the **operand's** type: they are
        // the literals being compared, and each shows the coercion it took, while the expression's
        // own type is `boolean`.
        Expr::InList {
            operand,
            list,
            negated,
            any: _,
        } => {
            let element = column_type_of(operand, table).unwrap_or(ty);
            format!(
                "({} {} (ARRAY[{}]))",
                deparse(operand, table, element),
                if *negated { "<> ALL" } else { "= ANY" },
                list.iter()
                    .map(|item| deparse(item, table, element))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        // **A text function shows the cast its argument took.** `lower(v)` over a `varchar` prints
        // `lower((v)::text)` and over a `text` prints `lower(t)`; `abs(n)` prints bare, being the
        // one scalar function here that does not take text. Measured on 19beta1 across all four
        // shapes, and it is what `schema_dumper_test#test_schema_dump_expression_indices` asserts
        // — the regex names `lower((name)::text)` exactly.
        Expr::Scalar { func, operand } => {
            // **The argument is deparsed under the function's own parameter type**, not under the
            // expression's. Same correction as the `||` arm below and the same measurement behind
            // it: `upper('a')` prints `upper('a'::text)` on a real server, and threading the
            // *result* type down printed `upper('a'::integer)` for an `integer` default whose
            // value happened to be a call. A literal takes its cast from the parameter it fills.
            let argument = deparse(
                operand,
                table,
                if takes_text(*func) {
                    ColumnType::Text
                } else {
                    ty
                },
            );
            let cast = takes_text(*func)
                && matches!(column_type_of(operand, table), Some(from)
                if matches!(from, ColumnType::Varchar | ColumnType::Bpchar));
            if cast {
                format!("{}(({argument})::text)", func.name())
            } else {
                format!("{}({argument})", func.name())
            }
        }
        // **A cast to `text` lands here, so the unknown constant folds here too.** `(NULL)::text`
        // is `NULL::text` on a real server and this printed `(NULL::text)::text`: the literal
        // names a type because a bare `NULL` has none to print, and then the cast named it again.
        // The `Cast` arm below has had this rule for a quoted string all along —
        // [`folded_into_its_cast`] is now the one predicate for both arms, because `::text` is the
        // one target that does not reach that arm.
        Expr::ToText { operand, .. } if folded_into_its_cast(operand) => {
            deparse_literal_of(operand, ColumnType::Text)
        }
        Expr::ToText { operand, .. } => format!("{}::text", parenthesise(&sub(operand))),
        // **A cast over an unadorned string literal is *one* node on a real server**, so it prints
        // as one: `'{}'::jsonb` and not `('{}'::text)::jsonb`. The parser coerces an `unknown`
        // constant straight to the target type rather than building a cast over a `text` one, so
        // there is no inner node to parenthesise — measured through `(j || '{}'::jsonb)`, where
        // this node printed the double form and the pair with it.
        //
        // Only a **bare** string literal: `('a' || 'b')::text` really is a cast over an
        // expression, and a numeric constant under a cast is [`numeric_constant`]'s three forms
        // and debt #24's remaining case.
        Expr::Cast { operand, to, .. } if folded_into_its_cast(operand) => {
            deparse_literal_of(operand, *to)
        }
        // **A cast carries its modifier into its name.** `(v)::character varying(5)` and
        // `(n)::numeric(10,2)` are what a real server prints, and this arm dropped the parameter
        // and printed the bare type — so a cast that PostgreSQL *keeps* was printed wrong even
        // before group E stopped printing the ones it elides. `value::format_type` is the same
        // renderer `format_type()` and an error message use, so there is one spelling of a
        // parameterised type in this crate and not two.
        Expr::Cast {
            operand,
            to,
            typmod,
        } => {
            format!(
                "{}::{}",
                parenthesise(&sub(operand)),
                crate::value::format_type(*to, *typmod)
            )
        }
        // **Five lines, indented four spaces, with the implicit `ELSE` materialised.** This layout
        // is what `pg_get_indexdef` answers on a real server — `pg_get_indexdef` deparses with
        // `PRETTYFLAG_INDENT`, which puts every keyword of a `CASE` on its own line — and it is
        // why `ActiveRecord`'s schema dumper sees a multi-line definition for statement 198.
        // Captured with the newlines escaped, because a corpus line cannot hold one
        // (`tests/corpus/pg19_case_expression.txt`).
        Expr::SetFunc(call) => format!(
            "{}({})",
            call.name,
            call.args
                .iter()
                .map(|arg| deparse(arg, table, ty))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // **A production, so its arguments carry the coercion to the common type**:
        // `COALESCE(b, (1)::bigint)` where the column is `bigint` and the literal is not.
        Expr::Coalesce(args) => format!(
            "COALESCE({})",
            args.iter()
                .map(|arg| {
                    deparse_production_argument(arg, table, production_common_type(args, table, ty))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            // **Every part takes its own pair, which is PostgreSQL's *plain* form** —
            // `WHEN (price IS NOT NULL) THEN (price + 1) ELSE (price * 2)`, measured, and what
            // `pg_get_expr(indexprs)` gives an index key. The *pretty* form takes none of them,
            // and that is `catalog::pretty_case`'s job at the one reader that asks for it: the
            // text is stored once, in this shape, and stripped on the way out.
            // **No leading newline: that belongs to whatever parenthesises this.** PostgreSQL's
            // `pg_get_expr(indexprs)` gives `CASE\n    WHEN …` and its `CHECK` printer gives
            // `CHECK (\nCASE` — the break comes after an opening parenthesis, not before the
            // keyword. This node put it in the expression, so every reader inherited it and the
            // two that add no pair had one too many.
            //
            // **A nested `CASE` starts on the line after its `THEN`, indented four further
            // spaces.** Measured; this node inlined it at the same depth. [`indented`] is what
            // makes the recursion work without a depth parameter: the inner call has already
            // produced a block, and a block moves as a whole.
            // **The simple form's operand goes on the `CASE` line**, in its own pair when it is
            // an operator: `CASE a`, `CASE (a + 1)`, `CASE t` — measured through
            // `pg_get_constraintdef`, where the pretty form drops that pair and the plain one
            // keeps it, which is `catalog::pretty_case`'s half of the same rule.
            let mut text = match operand {
                Some(operand) => format!("CASE {}", sub(operand)),
                None => "CASE".to_owned(),
            };
            // **A simple form's `WHEN` is deparsed under the *operand's* type, not the `CASE`'s.**
            // The same correction the comparison arm carries, for the same reason: the value is
            // compared against the operand, so `CASE t WHEN 'x'` prints `'x'::text` — and printing
            // it under the type the branches resolved to gave `'x'::integer`, which `reads_back`
            // then refused, so the whole `CHECK` fell back to the text the user wrote. Measured,
            // `tests/corpus/pg19_deparse_census.txt`'s `kf5`. A searched form's `WHEN` is a
            // condition and keeps the expression's own type, which is what it had before.
            let when_type = operand
                .as_deref()
                .and_then(|operand| column_type_of(operand, table))
                .unwrap_or(ty);
            for branch in branches {
                let when = deparse(&branch.when, table, when_type);
                let then = sub(&branch.then);
                if then.starts_with("CASE") {
                    let _ = write!(text, "\n    WHEN {when} THEN\n{}", indented(&then));
                } else {
                    let _ = write!(text, "\n    WHEN {when} THEN {then}");
                }
            }
            // An `ELSE` that was not written is **not** absent from the printed form: it is
            // `NULL` of the type the branches resolved to, which is the one part of this text that
            // could not have been produced before the expression met the table.
            let otherwise = otherwise
                .as_deref()
                .map_or_else(|| deparse_literal(&plan::Literal::Null, ty), &sub);
            if otherwise.starts_with("CASE") {
                let _ = write!(text, "\n    ELSE\n{}\nEND", indented(&otherwise));
            } else {
                let _ = write!(text, "\n    ELSE {otherwise}\nEND");
            }
            text
        }
        // Refused before this is reached: `refuse_unless_immutable` rejects every one of them as
        // an index key, and a column reference has been resolved to an `Ordinal` by then. Printed
        // rather than panicked on, because this is a catalog write and not a place to abort.
        Expr::Column { name, .. } => catalog::quote_identifier(name),
        Expr::Parameter(number) => format!("${number}"),
        Expr::CurrentSchema { all: None } => "current_schema()".to_owned(),
        Expr::CurrentDatabase => "current_database()".to_owned(),
        Expr::CurrentUser => "CURRENT_USER".to_owned(),
        Expr::CurrentSchema {
            all: Some(implicit),
        } => format!("current_schemas({implicit})"),
        Expr::CurrentSetting { name, missing_ok } => plan::current_setting_text(name, *missing_ok),
        // Folded away in `Executor::bound` before anything prints one, so this is the shape a
        // stored expression could never hold — printed rather than `unreachable!` because a
        // `DEFAULT` is re-read from text and a panic there would be a crash on catalog data.
        Expr::Advisory { call, .. } => format!("{}()", call.name()),
        Expr::Outer { at, .. } => format!("<outer {at}>"),
        Expr::Default => "DEFAULT".to_owned(),
        Expr::Sequence(call) => format!("{}()", call.func.name()),
        // **A catalog function whose name is an operator prints as one**, which is the shape
        // `pg_get_expr` gives it: `(t || 'x'::text)` and not `||(t, 'x')`. `parse::lower` makes
        // every `||` a `CatalogFunc` — hstore, jsonb and text alike — so the spelling has to come
        // back from the name rather than from the variant, and `name()` is where `||` already is.
        //
        // Measured: `t || 'x'` is `(t || 'x'::text)` and `length(t || 'x')` is
        // `length((t || 'x'::text))` — the argument keeps its own pair inside the call's
        // parentheses, which is what makes this an operator arm and not a call one.
        // **A catalog function spelled as an operator prints as one**, which is the shape
        // `pg_get_expr` gives it: `(t || 'x'::text)` and not `||(t, 'x')`.
        //
        // **Each operand is deparsed under the *operator's* argument type, not the expression's.**
        // `deparse` threads one type down to every literal, which is right for a comparison — where
        // `reconcile` has already retyped the literal against the column — and wrong here:
        // `length(t || 'x')` printed `length((t || 'x'::integer))`, taking `length`'s *result*
        // type, where a real server says `length((t || 'x'::text))`. The pair comes from
        // [`operator_operand_types`], per variant, because the symbol is not enough: `||` over
        // `jsonb` wants `'{}'::jsonb` where `||` over text wants `'x'::text`.
        //
        // The left operand prefers its own column's type when it has one, so a document keeps its
        // declared type where the table can only name one for the family.
        Expr::CatalogFunc(call)
            if operator_operand_types(call.func).is_some() && call.args.len() == 2 =>
        {
            let (left, right) =
                operator_operand_types(call.func).unwrap_or((ColumnType::Text, ColumnType::Text));
            let left = column_type_of(&call.args[0], table).unwrap_or(left);
            format!(
                "({} {} {})",
                deparse(&call.args[0], table, left),
                call.func.name(),
                deparse(&call.args[1], table, right)
            )
        }
        // **`GREATEST` and `LEAST` are grammar productions**: upper-cased, and their arguments
        // coerced to the call's **common type**, which is the type threaded in. Measured:
        // `GREATEST(b, (1)::bigint)` on a `bigint` column widens the literal, and
        // `GREATEST(n, 1)` on an `integer` one shows no cast because the two already agree.
        Expr::CatalogFunc(call)
            if matches!(
                call.func,
                plan::CatalogFunc::Greatest | plan::CatalogFunc::Least
            ) =>
        {
            format!(
                "{}({})",
                call.func.name().to_uppercase(),
                call.args
                    .iter()
                    .map(|arg| {
                        deparse_production_argument(
                            arg,
                            table,
                            production_common_type(&call.args, table, ty),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        // **`NULLIF` is the fourth production and the one whose arguments are *not* coerced.**
        // It resolves `=` between the two and PostgreSQL leaves both sides as they are, so
        // `NULLIF(b, 1)` prints the literal bare where `GREATEST(b, 1)` one arm up prints
        // `(1)::bigint` — the same fact as its result type being the comparison's left input
        // (`exec::query::nullif_type`). So the left is threaded the call's type, which is that
        // input, and the right is threaded **its own**, falling back to the call's for an
        // untyped literal that takes it: `NULLIF(t, 'x'::text)`. Both measured on 19beta1.
        Expr::CatalogFunc(call)
            if call.func == plan::CatalogFunc::NullIf && call.args.len() == 2 =>
        {
            let right = decided_literal_type(&call.args[1]).unwrap_or(ty);
            format!(
                "NULLIF({}, {})",
                deparse(&call.args[0], table, ty),
                deparse(&call.args[1], table, right)
            )
        }
        // **`SUBSTRING` is the fifth production, and the one that is a production *and* a
        // function.** Which it prints as depends on how it was written, and PostgreSQL keeps the
        // two apart: `substring(t from 1 for 2)` is `SUBSTRING(t FROM 1 FOR 2)`, keywords and all,
        // while `substring(t, 1, 2)` is `"substring"(t, 1, 2)` — the call, quoted, because
        // `substring` is a reserved word. `substr(t, 1, 2)` beside them is an ordinary name and
        // prints bare, which is [`catalog_parameter_types`]'s row.
        //
        // **This node keeps only one of the two spellings**, so the keyword form is what prints.
        // `parse::lower` reads `substr(...)` into `CatalogFunc::Substr` and *both* spellings of
        // `substring` into `CatalogFunc::Substring` — the flag it has separates `substr` from
        // `substring` and not the call form from the keyword form — so the call spelling of
        // `substring` is a declared divergence in
        // `tests/corpus/pg19_catalog_func_deparse.txt` and the keyword one agrees. Printing the
        // production is the half that also reads back: this node's parser has no quoted function
        // names.
        Expr::CatalogFunc(call)
            if call.func == plan::CatalogFunc::Substring && (2..=3).contains(&call.args.len()) =>
        {
            let operand = deparse(&call.args[0], table, ColumnType::Text);
            let from = deparse(&call.args[1], table, ColumnType::Int4);
            match call.args.get(2) {
                Some(count) => format!(
                    "SUBSTRING({operand} FROM {from} FOR {})",
                    deparse(count, table, ColumnType::Int4)
                ),
                None => format!("SUBSTRING({operand} FROM {from})"),
            }
        }
        // **`mod` prints as a call and coerces like the operator it is.** `mod(id, 10)` over an
        // `integer` column is `mod(id, 10)` and over a `bigint` one is `mod(id, (10)::bigint)` —
        // measured — which is the common-type rule the four productions follow, spelled with a
        // lower-case name instead of an upper-case one.
        Expr::CatalogFunc(call) if call.func == plan::CatalogFunc::Mod && call.args.len() == 2 => {
            let common = production_common_type(&call.args, table, ty);
            format!(
                "mod({}, {})",
                deparse_production_argument(&call.args[0], table, common),
                deparse_production_argument(&call.args[1], table, common)
            )
        }
        // **An ordinary call, with its arguments and the coercion each parameter took.** The name
        // is its own, lower-case, quoted where a real server quotes it — `substring` is a reserved
        // word and prints `"substring"(t, 1, 2)` where `substr(t, 1, 2)` beside it prints bare.
        Expr::CatalogFunc(call)
            if catalog_parameter_types(call.func, call.args.len()).is_some() =>
        {
            let params = catalog_parameter_types(call.func, call.args.len()).unwrap_or(&[]);
            format!(
                "{}({})",
                printed_call_name(call.func),
                call.args
                    .iter()
                    .zip(params)
                    .map(|(arg, param)| deparse(arg, table, *param))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        // Everything else a catalog function can be keeps the placeholder, which [`reads_back`]
        // refuses — so the expression keeps the text it was written with.
        // [`catalog_parameter_types`] is where that decision lives and why.
        Expr::CatalogFunc(call) => format!("{}(...)", call.func.name()),
        Expr::Aggregate(call) => format!("{}(...)", call.func.name()),
        Expr::Subquery(sub) => sub.kind.describe().to_owned(),
        // **The quantifier prints as the operator and the word**, which is what `pg_get_expr`
        // gives for every one of the twelve spellings: `(c1 <> ALL (ARRAY[1, 2]))`,
        // `(c1 = ALL (ARRAY[1, 2]))`, `(c1 > ALL (ARRAY[1, 2]))` — measured on 19beta1
        // (`tests/corpus/pg19_all_quantifier.txt`). `NOT IN` and `<> ALL` print *identically*,
        // which is why `parse::lower` is free to prefer the `IN` form and why this arm and the
        // `InList` arm below print the same characters for them.
        Expr::QuantifiedArray {
            operand,
            op,
            all,
            array,
        } => {
            // The elements take the operand's type, the same correction the `InList` arm carries:
            // a text array's elements print `'a'::text` and the expression's own type is boolean.
            let element = column_type_of(operand, table).unwrap_or(ty);
            format!(
                "({} {} {} ({}))",
                deparse(operand, table, element),
                op.symbol(),
                if *all { "ALL" } else { "ANY" },
                deparse(array, table, element)
            )
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
        // A typed NULL deparses under **its own** type, not the column's: it is what the user
        // wrote, and `pg_get_expr` prints back what was written.
        Literal::TypedNull(null) => format!("NULL::{}", null.name()),
        Literal::Null => format!("NULL::{}", ty.name()),
        Literal::Bool(value) => value.to_string(),
        // **The `int4` rung read from the printing end**: an unadorned integer is an `integer` when
        // it fits one and a `bigint` when it does not (ADR 0087), and that is the type
        // `numeric_constant` needs to decide which of the three forms PostgreSQL used.
        Literal::Integer(value) => numeric_constant(
            &value.to_string(),
            if i32::try_from(*value).is_ok() {
                ColumnType::Int4
            } else {
                ColumnType::Int8
            },
        ),
        Literal::Decimal(digits) => numeric_constant(digits, ColumnType::Numeric),
        Literal::String(text) => format!("'{}'::{}", text.replace('\'', "''"), ty.name()),
        // **A number prints bare or quoted according to its own type, and everything else prints
        // with its type**, which is PostgreSQL's `get_const_expr`: `(rating > 0)` for an integer
        // column, `(t > 'a'::text)` for a text one and `(d > '2020-01-01'::date)` for a date. The
        // label is what tells the reader — and the re-parse — which type a quoted constant is.
        // Which numbers are bare is [`numeric_constant`]'s subject.
        Literal::Typed(value) => match value.as_ref() {
            Datum::Bool(flag) => flag.to_string(),
            number @ (Datum::Int8(_)
            | Datum::Int4(_)
            | Datum::Int2(_)
            | Datum::Double(_)
            | Datum::Real(_)
            | Datum::Numeric(_)) => numeric_constant(
                &number.to_text().unwrap_or_default(),
                number.column_type().unwrap_or(ty),
            ),
            other => format!(
                "'{}'::{}",
                other.to_text().unwrap_or_default().replace('\'', "''"),
                other.column_type().map_or(ty, |own| own).name()
            ),
        },
    }
}
/// Walks one resolved expression, refusing every node that may not be an index key.
fn refuse_unless_immutable(expr: &plan::Expr, scope: &crate::exec::query::Scope<'_>) -> Result<()> {
    use crate::plan::Expr;
    let mut refusal = None;
    super::subquery::walk(expr, &mut |node| {
        if refusal.is_some() {
            return;
        }
        refusal = match node {
            // **A cast between `text` and a type whose text form is a *setting* is not
            // immutable**, and this node accepted every one of them. Measured on 19beta1 by
            // asking, in both directions and through both readers:
            //
            // ```text
            // refused   date  timestamp  timestamptz  interval  money  and every array type
            // accepted  integer  numeric  double precision  inet  uuid  boolean  json  jsonb  bytea
            // ```
            //
            // `DateStyle` is what makes a `date` stable, `IntervalStyle` an `interval`,
            // `lc_monetary` a `money`, and an array's output function is its element's plus a
            // delimiter. `double precision` is the one a guess gets wrong — `extra_float_digits`
            // moves its output and PostgreSQL marks `float8out` immutable anyway — which is why
            // this list is measured and not derived.
            //
            // **This is the direction no corpus had asked about**: the node was *more permissive*
            // than the server it copies, so nothing here could go red. `docs/plans/debts-v1.1.md`
            // carries the methodology note.
            Expr::ToText { operand, .. } if unstable_text_form(operand, scope) => {
                Some(SqlError::NotImmutableInIndex)
            }
            Expr::Cast { operand, to, .. }
                if *to == ColumnType::Text && unstable_text_form(operand, scope) =>
            {
                Some(SqlError::NotImmutableInIndex)
            }
            // And the same cast read the other way: `(t)::date` is `DateStyle` again.
            Expr::Cast { to, .. } if has_unstable_text_form(*to) => {
                Some(SqlError::NotImmutableInIndex)
            }
            Expr::Aggregate(_) => Some(SqlError::AggregateNotAllowed(
                "aggregate functions are not allowed in index expressions",
            )),
            // A UUID function is the most volatile thing here: two calls give two values, so an
            // index built from one would be read back at a key nothing ever wrote.
            Expr::Sequence(_) | Expr::Uuid(_) => Some(SqlError::NotImmutableInIndex),
            // **A catalog function is asked rather than assumed.** It used to be refused as a
            // group, on the reasoning that every one of them either wrote or read the catalog —
            // true when it was written and not once the text-search family arrived.
            // `to_tsvector('english', …)` is `IMMUTABLE` on a real server and is exactly what
            // `schema_test.rb` builds a GIN index over.
            // **A name this crate has never heard of is *named*, not called not-immutable.**
            // `parse::lower` carries an unknown call out as `CatalogFunc::UserFunc` on purpose —
            // lowering cannot see the catalog, so the name may be a user function — and the query
            // path then refuses it `0A000 the function <name>`
            // (`Executor::inline_user_function`). This guard used to reach it as "a catalog
            // function that is not immutable" and answer
            // `42P17 functions in index expression must be marked IMMUTABLE`, which is a true
            // sentence about a false premise: `md5('a')` is refused here and `md5` appears nowhere
            // in this crate, so the reason a reader was given named the wrong thing.
            //
            // Measured on 19beta1: `md5('a')` is answered (`provolatile = 'i'`) and
            // `nosuchfn('a')` is `42883 function nosuchfn(unknown) does not exist` with
            // `DETAIL: There is no function of that name.` — this node's `0A000` for a name it
            // lacks is the standing C2 convention and `error.rs` states it: "the `0A000` a
            // function this node has never heard of gets is a different answer for a different
            // condition". What was wrong was answering neither of the two.
            //
            // A **declared** user function lands here too and gets the same refusal, which is
            // also right: this node cannot evaluate one at all
            // (`docs/plans/phase-9-rails.md`'s `my_uuid_generator` row), so "not supported" is
            // the honest answer whatever the function's declared volatility says.
            Expr::CatalogFunc(call) if call.func == plan::CatalogFunc::UserFunc => {
                Some(SqlError::unsupported(format!(
                    "the function {}",
                    match call.args.first() {
                        Some(Expr::Literal(plan::Literal::String(name))) => name.as_str(),
                        _ => "?",
                    }
                )))
            }
            Expr::CatalogFunc(call) if !call.is_immutable() => Some(SqlError::NotImmutableInIndex),
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
/// Whether this expression's text form depends on a setting, so a cast to or from `text` over it
/// is **stable rather than immutable**.
///
/// The measured list is in [`refuse_unless_immutable`]'s `ToText` arm. A type is named here rather
/// than reasoned about: `double precision` is accepted by a real server even though
/// `extra_float_digits` moves its output, and `bytea` is accepted even though `bytea_output` does
/// — the answer is `provolatile` on the output function, and only the oracle knows it.
fn has_unstable_text_form(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::TimestampTz
            | ColumnType::Interval
            | ColumnType::Money
    ) || esker_keys::array::ArrayValue::element_of(ty).is_some()
}

/// [`has_unstable_text_form`] for the type an operand turns out to have.
///
/// An operand whose type cannot be worked out is **not** refused: this guard exists to reproduce a
/// refusal a real server makes, and inventing one for a shape it cannot type would be the more
/// permissive direction's mirror image.
fn unstable_text_form(operand: &plan::Expr, scope: &crate::exec::query::Scope<'_>) -> bool {
    crate::exec::query::expr_type(operand, scope).is_ok_and(has_unstable_text_form)
}

/// An index's key as column **names**, which is how a partition's copy is matched to its parent's.
///
/// Positions cannot do it: the copy's ordinals are the partition's and the original's are the
/// parent's, and the two differ the moment a partition carries an internal row id the parent has
/// not. An expression part has no column and is named by its printed text, so two copies of one
///
/// **The second half of the distinction this whole family turns on**: `CREATE UNIQUE INDEX` and
/// `ADD CONSTRAINT … UNIQUE` build the same index, and only the second has a `pg_constraint` row —
/// which is exactly what makes `DROP INDEX` refuse it and `DROP CONSTRAINT` remove it. The primary
/// key's case is the same rule one relation kind over, and it is checked where the relation is
/// resolved because a primary key has no `IndexDef` to find here.
fn refuse_a_constraints_index(table: &TableDef, index_id: u64, name: &str) -> Result<()> {
    if table
        .indexes
        .iter()
        .any(|index| index.id == index_id && index.constraint.is_some())
    {
        return Err(SqlError::DependentObjectsStillExist {
            index: name.to_owned(),
            table: table.name.clone(),
        });
    }
    Ok(())
}

/// expression index still match.
fn index_key_names(table: &TableDef, index: &IndexDef) -> Vec<String> {
    index
        .keys
        .iter()
        .map(|key| {
            key.position()
                .and_then(|at| table.columns.get(at))
                .map_or_else(
                    || key.attname(table).to_owned(),
                    |column| column.name.clone(),
                )
        })
        .collect()
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
            Some(catalog::Relation::View { .. }) => {
                return Err(SqlError::WrongObjectType {
                    name: name.clone(),
                    expected: "an index",
                    found: "DROP VIEW",
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

        refuse_a_constraints_index(&table, index_id, name)?;

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

        // **Dropping the partitioned index takes every partition's own copy with it.** Measured:
        // after `DROP INDEX index_measurements_on_logdate_and_city_id`, *zero* relations match
        // `%logdate_city_id%` — the children the suite never named go with the parent it did. So
        // the copies are found the way they were made, by the key columns' names, and dropped
        // first; the parent's own entries and record follow below.
        if table.partition_by.is_some()
            && let Some(index) = table.indexes.iter().find(|index| index.id == index_id)
        {
            let key = index_key_names(&table, index);
            for &child_id in &table.children {
                let child = executor.table_by_id(txn, child_id)?;
                let mine: Vec<u64> = child
                    .indexes
                    .iter()
                    .filter(|copy| index_key_names(&child, copy) == key)
                    .map(|copy| copy.id)
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                let mut without = (*child).clone();
                for copy_id in &mine {
                    let (start, end) = crate::row::index_range(executor.tenant, child.id, *copy_id);
                    super::for_each_page(txn, &start, &end, |txn, page| {
                        for (key, _) in page {
                            txn.delete(key);
                        }
                        Ok(())
                    })?;
                }
                without.indexes.retain(|copy| !mine.contains(&copy.id));
                without.schema_version += 1;
                catalog::replace_table(txn, executor.tenant, &child, &without)?;
            }
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
/// The action's own name, for the `42809` a relation that cannot take it answers with.
///
/// It used to be the constant `"ADD COLUMN"`, which named the wrong statement for every action but
/// one — a message that sends a reader looking for a clause they did not write.
fn alter_action_name(action: Option<&AlterTableAction>) -> &'static str {
    match action {
        Some(AlterTableAction::AddColumn { .. }) | None => "ADD COLUMN",
        Some(AlterTableAction::DropColumn { .. }) => "DROP COLUMN",
        Some(AlterTableAction::RenameColumn { .. }) => "RENAME COLUMN",
        Some(AlterTableAction::RenameTo(_)) => "RENAME",
        Some(
            AlterTableAction::AddCheck(_)
            | AlterTableAction::AddForeignKey(_)
            | AlterTableAction::AddUnique(_),
        ) => "ADD CONSTRAINT",
        Some(AlterTableAction::DropConstraint { .. }) => "DROP CONSTRAINT",
        Some(
            AlterTableAction::SetDefault { .. }
            | AlterTableAction::SetNotNull { .. }
            | AlterTableAction::SetColumnType { .. },
        ) => "ALTER COLUMN",
        Some(AlterTableAction::ValidateConstraint(_)) => "VALIDATE CONSTRAINT",
        Some(_) => "ALTER",
    }
}

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
        Some(catalog::Relation::Table { table_id }) => {
            let table = executor.table_by_id(txn, table_id)?;
            // **A materialized view is a table record, and `ALTER TABLE` must not act on one.**
            // PostgreSQL names the *action* rather than saying "is not a table" — the same shape
            // an index gets, with its own `DETAIL`. Measured.
            if table.matview.is_some() {
                return Err(SqlError::AlterActionOnWrongObject {
                    action: "ADD COLUMN",
                    name: alter.name.clone(),
                    kind: "materialized views",
                });
            }
            table
        }
        // An index is a relation, so PostgreSQL does not say "is not a table" here -- it says the
        // action cannot be performed on it, and adds that indexes do not take one. Captured.
        Some(catalog::Relation::Index { .. } | catalog::Relation::PrimaryKey { .. }) => {
            return Err(SqlError::AlterActionOnWrongObject {
                action: "ADD COLUMN",
                name: alter.name.clone(),
                kind: "indexes",
            });
        }
        // Measured: `ALTER action ADD COLUMN cannot be performed on relation "v"` with
        // `DETAIL: This operation is not supported for views.`
        Some(catalog::Relation::View { .. }) => {
            return Err(SqlError::AlterActionOnWrongObject {
                action: "ADD COLUMN",
                name: alter.name.clone(),
                kind: "views",
            });
        }
        // **A sequence takes `RENAME TO` and nothing else**, and getting that wrong cost run 55
        // a hundred files. `rename_table` renames a table and then renames the sequence its
        // `serial` column owns with `ALTER TABLE <seq> RENAME TO …` — a sequence named where the
        // grammar says table, which a real server runs. Refusing it left the table moved and the
        // sequence under its old name, so the suite's `force: true` cycle could not clean up: the
        // `DROP TABLE IF EXISTS` found nothing under the old table name, and the `CREATE TABLE`
        // after it collided with a sequence that was still there. Every later schema load hit the
        // same wall while the node answered every health check perfectly.
        Some(catalog::Relation::Sequence {
            table_id,
            sequence_id,
        }) => {
            let [AlterTableAction::RenameTo(to)] = alter.actions.as_slice() else {
                return Err(SqlError::AlterActionOnWrongObject {
                    action: alter_action_name(alter.actions.first()),
                    name: alter.name.clone(),
                    kind: "sequences",
                });
            };
            let owner = executor.table_by_id(txn, table_id)?;
            let sequence = owner
                .sequences
                .iter()
                .find(|sequence| sequence.id == sequence_id)
                .ok_or_else(|| SqlError::UndefinedTable(alter.name.clone()))?;
            catalog::rename_sequence(txn, executor.tenant, sequence, to)?;
            return done;
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
        if let AlterTableAction::AddExclude(exclude) = action {
            add_exclude(txn, executor, &table, &mut updated, exclude)?;
            continue;
        }
        if let AlterTableAction::AddPrimaryKey { name, columns } = action {
            add_primary_key(&mut updated, name.as_deref(), columns)?;
            changed = true;
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
        // Accepted and applied to nothing, which is the whole of it: no catalog write, and **no
        // `columnar_changed`** — telling the placement driver to re-read for a parameter that
        // touched nothing is work it does not need.
        if matches!(action, AlterTableAction::AcceptStorageParameter) {
            continue;
        }
        // **One field on the table and nothing else.** Every relation reads its persistence from
        // the table it belongs to, so the indexes and the owned sequence move with it in this same
        // statement and there is no second place to keep in step.
        if let AlterTableAction::SetPersistence(persistence) = action {
            updated.persistence = *persistence;
            catalog::replace_table(txn, executor.tenant, &table, &updated)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::SetTriggersDisabled { disabled } = action {
            set_triggers_disabled(txn, executor, &table, &mut updated, *disabled)?;
            continue;
        }
        if let AlterTableAction::SetColumnType {
            column,
            ty,
            typmod,
            using,
            using_expr,
            collation,
        } = action
        {
            set_column_type(
                txn,
                executor,
                &mut updated,
                column,
                &TypeChange {
                    ty: *ty,
                    typmod: *typmod,
                    using: *using,
                    using_expr: using_expr.as_ref(),
                    collation: collation.as_deref(),
                },
            )?;
            changed = true;
            continue;
        }
        if let AlterTableAction::ValidateConstraint(name) = action {
            validate_constraint(txn, executor, &mut updated, name)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::SetNotNull { column, not_null } = action {
            set_column_not_null(txn, executor, &mut updated, column, *not_null)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::SetDefault { column, default } = action {
            set_column_default(txn, executor, &mut updated, column, default.as_ref())?;
            changed = true;
            continue;
        }
        if let AlterTableAction::AddUnique(constraint) = action {
            add_unique_constraint(txn, executor, &mut updated, constraint)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::AddUniqueUsingIndex(promote) = action {
            promote_index_to_constraint(executor, &mut updated, promote)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::RenameColumn { from, to } = action {
            rename_column(&mut updated, &alter.name, from, to)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::RenameTo(name) = action {
            rename_table(txn, executor, &mut updated, name)?;
            changed = true;
            continue;
        }
        if let AlterTableAction::DropConstraint {
            name,
            if_exists,
            cascade,
        } = action
        {
            changed |= drop_constraint(
                txn,
                executor,
                &mut updated,
                &alter.name,
                name,
                *if_exists,
                *cascade,
            )?;
            continue;
        }
        if let AlterTableAction::DropColumn {
            column,
            if_exists,
            cascade,
        } = action
        {
            changed |= drop_column(
                txn,
                executor,
                &mut updated,
                &alter.name,
                column,
                *if_exists,
                *cascade,
            )?;
            continue;
        }
        let AlterTableAction::AddColumn {
            column,
            if_not_exists,
            primary_key,
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
        // **`NOT NULL` with no `DEFAULT` is about the rows, not about the clause.** There is no
        // value to give a row that predates the column, so a table holding one is `23502` — with
        // the sentence `SET NOT NULL` uses, because no constraint exists yet to name. A table with
        // no rows has nothing to refuse, and that is the case every test in the suite sends.
        if column.not_null
            && column.default.is_none()
            // **An expression default counts as a value.** It is not one constant, but every row
            // gets one from the backfill below, which is exactly what `NOT NULL` needs and what
            // `ADD COLUMN thingy uuid NOT NULL DEFAULT gen_random_uuid()` asks for.
            && column.default_expr.is_none()
            && has_any_row(&*txn, executor, &updated)?
        {
            return Err(SqlError::ColumnContainsNulls {
                column: column.name.clone(),
                relation: updated.name.clone(),
            });
        }
        // **A `serial` on a table that already holds rows is the one half still refused.** A real
        // server fills the column from the sequence — measured, two rows get `1` and `2` — and
        // that is the table rewrite this `ALTER` does not do
        // (`tests/captures/pg19_add_column_primary_key.txt`). On an empty table there is nothing
        // to fill, which is the case `compatibility_test.rb` sends and the same shape the
        // `NOT NULL` rule above already had.
        if column.sequence.is_some() && has_any_row(&*txn, executor, &updated)? {
            return Err(SqlError::unsupported(
                "ALTER TABLE ... ADD COLUMN ... serial on a table with rows, which PostgreSQL                  fills from the sequence",
            ));
        }
        // The catalog decides what the declared name is, exactly as it does at `CREATE TABLE`,
        // and the default is folded against the answer rather than against the placeholder.
        let (ty, user_type) = resolve_user_type(&*txn, executor, column)?;
        let default = match match user_type {
            Some(oid) => type_by_oid(&*txn, executor, oid)?,
            None => None,
        } {
            // The same fold `CREATE TABLE` does, against the type the catalog just named: a
            // label becomes an ordinal and a range literal becomes a range.
            Some(def) => fold_user_default(
                column.default.as_ref(),
                ty,
                &def,
                &ColumnDef {
                    collation: None,
                    name: column.name.clone(),
                    ty,
                    typmod: column.typmod,
                    default_expr: None,
                    not_null: false,
                    default: None,
                    missing: None,
                    generated: None,
                    generated_virtual: false,
                    comment: None,
                    dropped: false,
                    user_type,
                },
            )?,
            None => column.default.clone(),
        };
        // **A generated column added by `ALTER` is deparsed too**, which `CREATE TABLE` has done
        // since the schema-dump unit (`normalise_generated`) and this path never did. One
        // `pg_attrdef` row, two ways to write it, and `pg_get_expr` cannot tell which statement
        // put it there: `ADD COLUMN g integer GENERATED ALWAYS AS (c1 * 2 + 3) STORED` prints
        // `((c1 * 2) + 3)` on a real server exactly as the `CREATE TABLE` spelling does, measured
        // on 19beta1 over all 32 shapes in `tests/corpus/pg19_generated_parens.txt`.
        //
        // Resolved against `updated` **before** the push, which is every column the expression is
        // allowed to name: a generated column cannot reference itself, and one that names a column
        // the table does not have is `42703` from here rather than a stored expression nothing can
        // evaluate.
        let generated = match &column.generated {
            Some(expr) => Some(generated_column_expression(&updated, expr)?.0),
            None => None,
        };
        // The second of `deparse_default`'s three callers: this column only, over the table as it
        // stands, because the others are already deparsed and would deparse again.
        let default_expr = column
            .default_expr
            .as_ref()
            .map(|expr| deparse_default(&updated, expr).unwrap_or_else(|| expr.clone()));
        updated.columns.push(ColumnDef {
            collation: column.collation.clone(),
            name: column.name.clone(),
            ty,
            typmod: column.typmod,
            default_expr,
            // With a constant default, the missing value below is what every row already stored
            // holds and the decoder pads with it. Without one, the check above has already refused
            // any table that has a row — so reaching here means there are none to pad.
            not_null: column.not_null,
            default: default.clone(),
            // **The missing value is frozen here**, at `ADD COLUMN` time, and a later
            // `ALTER COLUMN SET DEFAULT` must not touch it. Measured on PostgreSQL 19beta1: after
            // `SET DEFAULT 'new'`, rows that predate the column still read `old`
            // (`docs/plans/phase-6e.md` §5 unit 1). One field for both would rewrite history the
            // first time somebody changed a default.
            missing: default,
            generated,
            generated_virtual: column.generated_virtual,
            comment: None,
            dropped: false,
            user_type,
        });
        // **A generated column added by `ALTER` has a value for the rows already there**, and it
        // is not the missing value: it is the expression, over each row as it stands. Every other
        // `ADD COLUMN` here pads from `missing` and touches no row, which is what makes it cheap;
        // this one cannot, because the value is a function of the row rather than a constant.
        if column.generated.is_some() {
            fill_generated_for_existing_rows(txn, executor, &updated)?;
        }
        // **And a volatile default is the same problem with the same answer.** `DEFAULT 7` is one
        // constant and lives on the column as its missing value, touching no row;
        // `DEFAULT gen_random_uuid()` is a different value per row and cannot. PostgreSQL rewrites
        // the table for it — measured, three rows get three distinct uuids and the column's
        // recorded default stays the expression — and so does this.
        if column.default_expr.is_some() {
            let ordinal = updated.columns.len() - 1;
            fill_default_for_existing_rows(txn, executor, &updated, ordinal)?;
        }
        // **The sequence, named the way `CREATE TABLE` names one and taking a relation name of its
        // own.** `column: Some(ordinal)` is the whole of the default wiring — a `serial` column's
        // `nextval` is read from the sequence that fills it rather than stored as column text —
        // and `owner_column` is what makes it go when the column does, which is the bookkeeping
        // `DROP TABLE` and now `DROP SCHEMA` walk.
        let ordinal = updated.columns.len() - 1;
        if let Some(identity) = column.sequence {
            let name = plan::choose_relation_name(
                &updated.name,
                Some(&column.name),
                "seq",
                |candidate| catalog::name_exists(&*txn, executor.tenant, candidate),
            )?;
            let sequence = catalog::SequenceDef {
                id: catalog::allocate_id(txn, executor.tenant)?,
                name,
                table_id: updated.id,
                column: Some(ordinal),
                owner_column: Some(ordinal),
                identity,
                start: 1,
                increment: 1,
            };
            catalog::create_sequence(txn, executor.tenant, &sequence)?;
            updated.sequences.push(sequence);
            // A `serial` is `NOT NULL` on a real server whether or not the word was written.
            if let Some(added) = updated.columns.last_mut() {
                added.not_null = true;
            }
        }
        // **`PRIMARY KEY` declares a key; it does not re-key the rows.** A table created without
        // one has a hidden row-id column and keeps using it — that is a fact about the stored rows
        // and is read by the column's own name, not derived from whether a key is declared
        // (`catalog::TableDef::row_id`). The same property is what lets `DROP CONSTRAINT` take a
        // primary key away.
        if *primary_key {
            // **`primary_key_name` is the test, not `primary_key`.** A keyless table's
            // `primary_key` points at its hidden row-id column, so it is never empty; the *name*
            // is what a real server would quote back and what is empty when the user declared no
            // key (`catalog::TableDef::primary_key_name`).
            if !updated.primary_key_name.is_empty() {
                return Err(SqlError::MultiplePrimaryKeys(updated.name.clone()));
            }
            updated.primary_key = vec![ordinal];
            updated.primary_key_name =
                format!("{}_pkey", catalog::split_qualified(&updated.name).1);
            if let Some(added) = updated.columns.last_mut() {
                added.not_null = true;
            }
        }
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
    let name = &executor.resolve_unqualified(txn, name)?;
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
            let row = crate::row::decode_row(&schema, value, None)?;
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
                //
                // **The build's sentence, not the insert's.** Nothing was inserted here, and
                // PostgreSQL says so — `could not create unique index "…"`, with the `DETAIL`
                // naming the first duplicate found.
                return Err(SqlError::CouldNotCreateUniqueIndex {
                    index: index.name.clone(),
                    // `render_key` already writes the leading `Key `.
                    detail: format!(
                        "{} is duplicated.",
                        super::index::render_key(table, &index.keys, &entry.values)
                    ),
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
