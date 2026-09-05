//! `information_schema`: the SQL standard's view of the same records `pg_catalog` describes.
//!
//! Five relations, computed from one snapshot like everything else in this phase. Two of them
//! answer a question this node cannot answer through `pg_catalog` at all: `key_column_usage` gives
//! a primary key's columns **one row each**, where `pg_index.indkey` gives them as an
//! `int2vector` that needs `= ANY` over an array value to read — so a client that wants a table's
//! key can have it here today.
//!
//! # Two names, and the schema is part of them
//!
//! A bare `tables` is not a relation on a real server and must stay `42P01` here, so these views
//! are named `information_schema.tables` and the qualifier is part of the name rather than
//! something stripped before lookup. `pg_catalog.pg_class` is the mirror case: there the qualifier
//! *is* stripped, because `pg_class` is a relation on its own. Both are measured, and
//! `parse::lower` learns exactly these two schemas — `public.t` stays refused by name.
//!
//! # What the capture settled
//!
//! * **Only tables are in it.** An index, a sequence and a primary key are all in `pg_class` and
//!   none of them is in `information_schema.tables` or `.columns` — measured, `count(*)` is 0 for
//!   each of the three.
//! * **A `NOT NULL` is reported as a `CHECK`.** `table_constraints` reads `pg_constraint`, and
//!   PostgreSQL 19's `contype` `n` rows come out with `constraint_type` `CHECK`. So a five-column
//!   table with two `NOT NULL`s and a key has **three** rows here, two of them `CHECK`.
//! * **`is_nullable` is `YES`/`NO` and `is_identity` is `YES`/`NO`** — the standard's `yes_or_no`
//!   domain, not a boolean. A client reading them as booleans reads every column as true.
//! * **An integer's `numeric_scale` is 0 and a float's is NULL.** The one asymmetry in the type
//!   table below, and the one a reader would get wrong.
//! * **`datetime_precision` is 6 for a `timestamp` with no declared precision**, not NULL — the
//!   number of digits it actually stores — while `character_maximum_length` for a `varchar` with
//!   no length *is* NULL. Two "no modifier" cases, two different answers.
//! * **`referential_constraints` is empty**, which is a correct answer: this node has no foreign
//!   keys, so there is nothing referential to constrain.

use crate::backend::Txn;
use crate::catalog::pg_relations::{RelKind, Relations};
use crate::catalog::{ColumnDef, Identity};
use crate::error::Result;
use crate::value::{self, ColumnType, Datum};

/// The one schema every relation is in.
const PUBLIC_SCHEMA: &str = "public";

/// The standard's `yes_or_no` domain, which is two strings and not a boolean.
const YES: &str = "YES";
/// The other one.
const NO: &str = "NO";

/// Every `information_schema.tables` row: one per **table**, and nothing else.
pub fn tables(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = Relations::read(txn, tenant)?;
    // **A view is here and a materialized view is not**, measured: `information_schema.tables` has
    // a `VIEW` row for the first and no row at all for the second — the standard has no table type
    // for something PostgreSQL invented, so a real server leaves it out rather than calling it a
    // table (ADR 0064). `of_kind` gives the exclusion for free now that the two have different
    // kinds; the views have to be added.
    let views = relations.of_kind(RelKind::View).map(|relation| {
        vec![
            Datum::Text(relation.schema.clone()),
            Datum::Text(relation.name.clone()),
            Datum::Text("VIEW".to_owned()),
        ]
    });
    Ok(relations
        .of_kind(RelKind::Table)
        .map(|relation| {
            // **The schema the relation is actually in**, not a constant. It was `public` while
            // that was the only schema a relation could be in; a temporary table is in
            // `pg_temp_<n>` and a real server lists it there (ADR 0054), and a table in a user
            // schema was being reported in `public` — a *wrong* row rather than a missing one.
            vec![
                Datum::Text(relation.schema.clone()),
                Datum::Text(relation.name.clone()),
                // `BASE TABLE` for every one of them: a view, a materialised view, a partitioned
                // table and a foreign table are the other four values and this node has none.
                Datum::Text("BASE TABLE".to_owned()),
            ]
        })
        .chain(views)
        .collect())
}

/// The columns of `information_schema.domains`, in the standard's order.
///
/// **No `domain_catalog`**, for the reason [`TABLES_COLUMNS`] gives about `table_catalog`: this
/// node has no database name to report and a constant would be a value nobody measured. What is
/// left is what the capture reads — a domain described the way a column of its base type would be.
pub const DOMAINS_COLUMNS: &[(&str, ColumnType)] = &[
    ("domain_schema", ColumnType::Text),
    ("domain_name", ColumnType::Text),
    ("data_type", ColumnType::Text),
    ("numeric_precision", ColumnType::Int4),
    ("numeric_scale", ColumnType::Int4),
];

/// Every `information_schema.domains` row: one per domain, and nothing else.
///
/// `data_type` is the **base** type's standard name and not the domain's — measured, a
/// `custom_money` over `numeric(8,2)` reports `numeric`, with the precision and scale beside it.
/// The domain's own name is `domain_name`, which is the column that tells them apart (ADR 0065).
pub fn domains(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    for def in super::user_types(txn, tenant)? {
        let super::TypeKind::Domain { base, typmod, .. } = def.kind else {
            continue;
        };
        let (schema, name) = super::split_qualified(&def.name);
        let (precision, scale) = match value::numeric::precision_and_scale(typmod) {
            Some((precision, scale)) => (Datum::Int4(precision), Datum::Int4(scale)),
            None => (Datum::Null, Datum::Null),
        };
        rows.push(vec![
            Datum::Text(schema.to_owned()),
            Datum::Text(name.to_owned()),
            Datum::Text(value::PgType::name(base).to_owned()),
            precision,
            scale,
        ]);
    }
    Ok(rows)
}

/// The columns of `information_schema.views`, in the standard's order.
pub const VIEWS_COLUMNS: &[(&str, ColumnType)] = &[
    ("table_schema", ColumnType::Text),
    ("table_name", ColumnType::Text),
    ("view_definition", ColumnType::Text),
    ("is_updatable", ColumnType::Text),
    ("is_insertable_into", ColumnType::Text),
];

/// Whether PostgreSQL would treat this view's query as **automatically updatable**.
///
/// The rule, measured: exactly one entry in `FROM` (a table, or another view that is itself
/// updatable), and no aggregate or window function, `DISTINCT`, `GROUP BY`, `HAVING`, set
/// operation, `LIMIT` or `OFFSET`.
///
/// **A computed column does not make the view read-only.** `SELECT id, a + 1 AS a1 FROM vb` is
/// `YES` — this function's first draft required every target entry to be a plain column and the
/// oracle said otherwise. What a computation costs is that *column*'s own updatability
/// (`information_schema.columns.is_updatable`), which is a different question from this one.
///
/// A definition this node cannot parse is `NO`: an answer of `YES` is a promise that an `UPDATE`
/// through the view will work, and a query nothing here understands cannot make it.
fn is_automatically_updatable(definition: &str) -> bool {
    let Ok(parsed) = crate::parse::parse_statements(definition) else {
        return false;
    };
    let Some(Ok(crate::plan::Statement::Select(select))) =
        parsed.first().map(crate::parse::Parsed::lower)
    else {
        return false;
    };
    select.from.is_some()
        && select.joins.is_empty()
        && !select.distinct
        && select.group_by.is_empty()
        && select.having.is_none()
        && select.limit.is_none()
        && select.offset.is_none()
        // No window functions exist in this plan at all, so an aggregate is the whole of the
        // "not a plain projection" case.
        && !crate::exec::bind::any(&crate::plan::Statement::Select(select), |expr| {
            matches!(expr, crate::plan::Expr::Aggregate(_))
        })
}

/// Every `information_schema.views` row: one per view, with PostgreSQL's updatability rule.
pub fn views(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    Ok(crate::catalog::views(txn, tenant)?
        .into_iter()
        .map(|view| {
            let (schema, name) = crate::catalog::split_qualified(&view.name);
            // **`YES`/`NO` text, not a boolean** — the standard spells these as
            // `character varying(3)`, and a client comparing against the string would read a
            // boolean as neither.
            let updatable = if is_automatically_updatable(&view.definition) {
                "YES"
            } else {
                "NO"
            };
            vec![
                Datum::Text(schema.to_owned()),
                Datum::Text(name.to_owned()),
                Datum::Text(view.definition.clone()),
                Datum::Text(updatable.to_owned()),
                // The two agree for every automatically updatable view: what makes one insertable
                // is what makes it updatable, and a trigger could separate them only if this node
                // had `INSTEAD OF` triggers.
                Datum::Text(updatable.to_owned()),
            ]
        })
        .collect())
}

/// Every `information_schema.columns` row: one per column of a table, in declaration order.
pub fn columns(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = Relations::read(txn, tenant)?;
    // One read for the whole view rather than a lookup per column, the trade `Relations` already
    // makes for user types: a schema dump asks this of every column of every table.
    let domains: std::collections::BTreeMap<u64, String> = super::user_types(txn, tenant)?
        .into_iter()
        .filter(|def| matches!(def.kind, super::TypeKind::Domain { .. }))
        .map(|def| {
            let (_, bare) = super::split_qualified(&def.name);
            (def.oid, bare.to_owned())
        })
        .collect();
    let mut rows = Vec::new();
    for relation in relations.of_kind(RelKind::Table) {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        for (position, (at, column)) in table.user_columns().enumerate() {
            let sequence = super::pg_relations::sequence_for(table, at);
            let identity = sequence.map(|sequence| sequence.identity);
            rows.push(vec![
                Datum::Text(PUBLIC_SCHEMA.to_owned()),
                Datum::Text(table.name.clone()),
                Datum::Text(column.name.clone()),
                Datum::Int4(i32::try_from(position + 1).unwrap_or(i32::MAX)),
                // **NULL for a generated column**, where `pg_attrdef` holds its expression:
                // a generated column has no *default*, and this is the column that says so. The
                // two views read one `pg_attrdef` row and disagree about what it is; measured, and
                // a reader that looked here for a generation expression would find nothing.
                match super::pg_attribute::default_expression(column, table, at) {
                    _ if column.generated.is_some() => Datum::Null,
                    Some(expression) => Datum::Text(expression),
                    None => Datum::Null,
                },
                Datum::Text(if column.not_null { NO } else { YES }.to_owned()),
                // The **spelling**, which is `format_type` with no modifier — `character` and not
                // `bpchar`, and `timestamp without time zone` in full. A column declared as a
                // user-defined type is the literal string `USER-DEFINED`, whatever the type is and
                // whatever it is stored as: measured, and it is what `timestamp_test.rb:202`
                // asserts by name, beside a `udt_name` of the type itself.
                // **A domain is the exception**: it reports its *base* type here, not
                // `USER-DEFINED` — measured, a `custom_money` column over `numeric(8,2)` says
                // `numeric`, and `domain_name` is the only column that names the domain. That is
                // what makes `ActiveRecord` read the column as a `:decimal` while its `sql_type`
                // stays `custom_money` (ADR 0065).
                Datum::Text(
                    match column.user_type.filter(|oid| !domains.contains_key(oid)) {
                        Some(_) => USER_DEFINED.to_owned(),
                        None => data_type(column.ty),
                    },
                ),
                length_of(column),
                numeric_precision(column.ty),
                numeric_scale(column.ty),
                datetime_precision(column),
                // The type's internal name, which is `pg_type.typname` — and for a user-defined
                // type that is the name it was declared with, not the name of what it is stored
                // as. `ActiveRecord` reads this column to decide a column is an enum.
                Datum::Text(
                    column
                        .user_type
                        // A domain's `udt_name` is its base type's too, for the same reason
                        // `data_type` above is: the domain's own name lives in `domain_name`.
                        .filter(|oid| !domains.contains_key(oid))
                        .and_then(|oid| table.enums.get(&oid))
                        .map_or_else(
                            || super::pg_catalog::typname(column.ty).to_owned(),
                            |def| def.name.clone(),
                        ),
                ),
                Datum::Text(
                    match identity {
                        // `bigserial` is a default, not an identity — measured, `is_identity` is
                        // `NO` for it and its `column_default` is the `nextval`.
                        None | Some(Identity::Default) => NO,
                        Some(_) => YES,
                    }
                    .to_owned(),
                ),
                match identity {
                    Some(Identity::ByDefault) => Datum::Text("BY DEFAULT".to_owned()),
                    Some(Identity::Always) => Datum::Text("ALWAYS".to_owned()),
                    None | Some(Identity::Default) => Datum::Null,
                },
                // `ALWAYS` for a `GENERATED … AS (expr) STORED` column and `NEVER` for every
                // other, **including an identity one**: `is_generated` is about a generation
                // expression and an identity is not one. Measured — an identity column is
                // `is_identity YES` and `is_generated NEVER` at once.
                Datum::Text(
                    if column.generated.is_some() {
                        "ALWAYS"
                    } else {
                        "NEVER"
                    }
                    .to_owned(),
                ),
                // The expression itself, where `column_default` above is **NULL** for the same
                // column: one `pg_attrdef` row, and the two views disagree about what it is.
                match &column.generated {
                    Some(expr) => Datum::Text(expr.clone()),
                    None => Datum::Null,
                },
                // The domain the column was declared as. A column of an **enum** or a **range**
                // also carries a `user_type`, and neither is a domain — so the kind is checked
                // rather than the presence of an oid.
                match column.user_type.and_then(|oid| domains.get(&oid)) {
                    Some(name) => Datum::Text(name.clone()),
                    None => Datum::Null,
                },
                // **`pg_catalog` for a built-in, and for a domain over one too** — measured: a
                // column of `schema_9.text` reports `udt_name` `text` and `udt_schema`
                // `pg_catalog`, because both describe the *base* type. An enum or a range is a
                // user type and lives where it was declared.
                Datum::Text(
                    column
                        .user_type
                        .filter(|oid| !domains.contains_key(oid))
                        .and_then(|oid| table.enums.get(&oid))
                        .map_or_else(
                            || PG_CATALOG_SCHEMA.to_owned(),
                            |def| super::split_qualified(&def.name).0.to_owned(),
                        ),
                ),
            ]);
        }
    }
    Ok(rows)
}

/// Every `information_schema.table_constraints` row, out of `pg_constraint`.
pub fn table_constraints(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    // One snapshot for the constraint rows *and* for the table each names, rather than one read
    // per row: this is a view over a view, and the catalog underneath is read once.
    let relations = Relations::read(txn, tenant)?;
    let schemas = super::schemas(txn, tenant)?;
    let mut rows = Vec::new();
    for row in super::pg_constraint::rows_from(&relations, &schemas) {
        let (Some(Datum::Text(name)), Some(Datum::Text(contype))) = (row.get(1), row.get(3)) else {
            continue;
        };
        // **An `EXCLUDE` constraint is not listed at all.** Measured on 19beta1: a table with
        // three of them returns two rows here, the primary key and the `bigserial`'s not-null.
        // The standard has no exclusion constraint, so PostgreSQL leaves it out rather than
        // reporting it under one of the four names — and anything reading exclusions has to read
        // `pg_constraint`, where `contype = 'x'`.
        if contype == "x" {
            continue;
        }
        let Some(Datum::Int8(conrelid)) = row.get(7) else {
            continue;
        };
        let Some(table) = relations.by_oid(*conrelid) else {
            continue;
        };
        rows.push(vec![
            Datum::Text(PUBLIC_SCHEMA.to_owned()),
            Datum::Text(name.clone()),
            Datum::Text(PUBLIC_SCHEMA.to_owned()),
            Datum::Text(table.name.clone()),
            Datum::Text(constraint_type(contype).to_owned()),
            // **`YES`/`NO` here is `t`/`f` there**, read from the same two `pg_constraint`
            // columns rather than assumed: this view is a view over that one, and a constraint
            // that reports `condeferrable` and does not report `is_deferrable` would be two
            // answers to one question.
            Datum::Text(yes_no(row.get(4)).to_owned()),
            Datum::Text(yes_no(row.get(5)).to_owned()),
        ]);
    }
    Ok(rows)
}

/// `YES`/`NO` for a `pg_constraint` boolean, which is how `information_schema` spells one.
fn yes_no(value: Option<&Datum>) -> &'static str {
    if matches!(value, Some(Datum::Bool(true))) {
        YES
    } else {
        NO
    }
}

/// Every `information_schema.key_column_usage` row: a primary key's columns, one each.
///
/// The answer `pg_index` cannot give without an array. `position_in_unique_constraint` is NULL for
/// a primary key and a unique constraint alike — it is the position in the *referenced* key of a
/// foreign key, and there are none.
pub fn key_column_usage(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for relation in relations.of_kind(RelKind::Table) {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        if table.primary_key_name.is_empty() {
            continue;
        }
        for (position, at) in table.primary_key.iter().enumerate() {
            let Some(column) = table.columns.get(*at) else {
                continue;
            };
            rows.push(vec![
                Datum::Text(PUBLIC_SCHEMA.to_owned()),
                Datum::Text(table.primary_key_name.clone()),
                Datum::Text(PUBLIC_SCHEMA.to_owned()),
                Datum::Text(table.name.clone()),
                Datum::Text(column.name.clone()),
                Datum::Int4(i32::try_from(position + 1).unwrap_or(i32::MAX)),
                Datum::Null,
            ]);
        }
    }
    Ok(rows)
}

/// `pg_constraint.contype` as the standard spells it.
///
/// **A `NOT NULL` is a `CHECK`**, measured on 19beta1 — the `n` rows that major added come out of
/// `table_constraints` under the standard's older name for the same thing.
fn constraint_type(contype: &str) -> &'static str {
    match contype {
        "p" => "PRIMARY KEY",
        "u" => "UNIQUE",
        "f" => "FOREIGN KEY",
        // `x` never reaches here — an exclusion constraint is filtered out above, because the
        // standard has no such thing and PostgreSQL omits it rather than renaming it.
        _ => "CHECK",
    }
}

/// What `data_type` says for a column of a user-defined type — the name is in `udt_name`.
const USER_DEFINED: &str = "USER-DEFINED";

/// Where every built-in type lives, which is what `udt_schema` reports for one.
const PG_CATALOG_SCHEMA: &str = "pg_catalog";

/// `data_type`: the type's name with no modifier on it.
///
/// `character`, not `bpchar` — the one type whose bare spelling differs from what
/// `format_type(oid, -1)` gives, and the distinction `crate::catalog::def_functions` calls
/// `typemod_given`.
fn data_type(ty: ColumnType) -> String {
    // **Every array column is the literal `ARRAY`**, whatever it is an array of — the element is
    // named in `udt_name` (`_text`, `_int4`, `_point`) and nowhere else. Measured across four
    // element types at once, because one would not have shown that the rule is about arrays
    // rather than about the element. This answered `text[]`, `integer[]`, `point[]` since arrays
    // became column types, and the `point` corpus is the first statement to ask.
    if esker_keys::array::ArrayValue::element_of(ty).is_some() {
        return "ARRAY".to_owned();
    }
    match ty {
        ColumnType::Bpchar => "character".to_owned(),
        // **The bare name, where `pg_attribute`'s `format_type` says `bit(1)`.** The two views
        // disagree on purpose: `information_schema.data_type` names the type and
        // `character_maximum_length` beside it carries the 1.
        // **`USER-DEFINED`, and `udt_name` beside it carries the name.** An extension's type is
        // not one of the standard's, so `information_schema` refuses to name it here — measured,
        // and it is what `ActiveRecord`'s schema dumper reads to reach for `udt_name`.
        ColumnType::Ltree => USER_DEFINED.to_owned(),
        ColumnType::Bit => "bit".to_owned(),
        ColumnType::VarBit => "bit varying".to_owned(),
        other => value::format_type(other, value::NO_TYPMOD),
    }
}

/// `character_maximum_length`: the declared length of a string type, or NULL.
fn length_of(column: &ColumnDef) -> Datum {
    // **A bare `bit` is one bit long** and a bare `bit varying` has no limit at all, which is the
    // asymmetry: `t.bit :another_bit` reports 1 and `t.bit_varying :another_bit_varying` NULL.
    if matches!(column.ty, ColumnType::Bit | ColumnType::VarBit) {
        return match (column.ty, column.typmod) {
            (ColumnType::Bit, value::NO_TYPMOD) => Datum::Int4(1),
            (_, value::NO_TYPMOD) => Datum::Null,
            (_, length) => Datum::Int4(length),
        };
    }
    match column.length() {
        Some(length) => Datum::Int4(i32::try_from(length).unwrap_or(i32::MAX)),
        None => Datum::Null,
    }
}

/// `numeric_precision`: how many bits or digits the type holds, for the types that are numbers.
fn numeric_precision(ty: ColumnType) -> Datum {
    match ty {
        ColumnType::Int8 => Datum::Int4(64),
        ColumnType::Int4 => Datum::Int4(32),
        ColumnType::Int2 => Datum::Int4(16),
        ColumnType::Double => Datum::Int4(53),
        ColumnType::Real => Datum::Int4(24),
        _ => Datum::Null,
    }
}

/// `numeric_scale`: **0 for an integer and NULL for a float**, which is the asymmetry.
///
/// An integer has a scale and it is zero; a float has none at all, because a binary float's scale
/// is not a property it has. Measured, and a reader would give both the same answer.
fn numeric_scale(ty: ColumnType) -> Datum {
    match ty {
        ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2 => Datum::Int4(0),
        _ => Datum::Null,
    }
}

/// `datetime_precision`: the declared precision of a timestamp, or **6** for one with none.
///
/// Not NULL, unlike `character_maximum_length` for an unqualified `varchar`: six is the number of
/// fractional digits a `timestamp` stores, so a column with no modifier still has a precision.
fn datetime_precision(column: &ColumnDef) -> Datum {
    match column.ty {
        // **`interval` is here too, and its default is six as well** — measured, a plain
        // `interval` column reports `6` where this answered NULL. The type keeps six fractional
        // digits whether or not anybody wrote a number, which is the same argument as `timestamp`'s
        // above; only the typmod's *encoding* differs, and `ColumnDef::precision` absorbs that.
        ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Interval => {
            Datum::Int4(i32::try_from(column.precision().unwrap_or(6)).unwrap_or(6))
        }
        _ => Datum::Null,
    }
}

/// The columns of `information_schema.tables`, in the standard's order.
///
/// **No `table_catalog`.** A real server reports the database it is connected to, and this node has
/// no database concept at all — no `current_database()`, and the startup parameter never reaches
/// the executor — so there is no name to report and a constant would be a value nobody measured.
/// `42703`, the same answer `pg_range` gives for `oid`.
pub const TABLES_COLUMNS: &[(&str, ColumnType)] = &[
    ("table_schema", ColumnType::Text),
    ("table_name", ColumnType::Text),
    ("table_type", ColumnType::Text),
];

/// The columns of `information_schema.columns`, in the standard's order.
pub const COLUMNS_COLUMNS: &[(&str, ColumnType)] = &[
    ("table_schema", ColumnType::Text),
    ("table_name", ColumnType::Text),
    ("column_name", ColumnType::Text),
    ("ordinal_position", ColumnType::Int4),
    ("column_default", ColumnType::Text),
    ("is_nullable", ColumnType::Text),
    ("data_type", ColumnType::Text),
    ("character_maximum_length", ColumnType::Int4),
    ("numeric_precision", ColumnType::Int4),
    ("numeric_scale", ColumnType::Int4),
    ("datetime_precision", ColumnType::Int4),
    ("udt_name", ColumnType::Text),
    ("is_identity", ColumnType::Text),
    ("identity_generation", ColumnType::Text),
    ("is_generated", ColumnType::Text),
    // **Last**, the rule `pg_type`'s columns follow: `SELECT *` expands in declared order, so a
    // column added anywhere else moves every one after it.
    ("generation_expression", ColumnType::Text),
    // **Last again**, same rule. The **domain** a column was declared as, and NULL for a column
    // declared as an ordinary type (ADR 0065). This is the one column that tells the two apart
    // here: `data_type` and `udt_name` both report the *base* type — measured, a `custom_money`
    // column over `numeric(8,2)` says `numeric` for both and `dm_money` only here.
    ("domain_name", ColumnType::Text),
    // **Last again.** The schema the `udt_name` type lives in — `pg_catalog` for every built-in,
    // which is what a column of a domain over one reports too, because `udt_name` is the *base*
    // type's. A client that qualifies a type name reads it, and asking for a column this view
    // does not have is `42703`.
    ("udt_schema", ColumnType::Text),
];

/// The columns of `information_schema.table_constraints`, in the standard's order.
pub const TABLE_CONSTRAINTS_COLUMNS: &[(&str, ColumnType)] = &[
    ("constraint_schema", ColumnType::Text),
    ("constraint_name", ColumnType::Text),
    ("table_schema", ColumnType::Text),
    ("table_name", ColumnType::Text),
    ("constraint_type", ColumnType::Text),
    ("is_deferrable", ColumnType::Text),
    ("initially_deferred", ColumnType::Text),
];

/// The columns of `information_schema.key_column_usage`, in the standard's order.
pub const KEY_COLUMN_USAGE_COLUMNS: &[(&str, ColumnType)] = &[
    ("constraint_schema", ColumnType::Text),
    ("constraint_name", ColumnType::Text),
    ("table_schema", ColumnType::Text),
    ("table_name", ColumnType::Text),
    ("column_name", ColumnType::Text),
    ("ordinal_position", ColumnType::Int4),
    ("position_in_unique_constraint", ColumnType::Int4),
];

/// The columns of `information_schema.referential_constraints`, which has no rows.
pub const REFERENTIAL_CONSTRAINTS_COLUMNS: &[(&str, ColumnType)] = &[
    ("constraint_schema", ColumnType::Text),
    ("constraint_name", ColumnType::Text),
    ("unique_constraint_schema", ColumnType::Text),
    ("unique_constraint_name", ColumnType::Text),
    ("match_option", ColumnType::Text),
    ("update_rule", ColumnType::Text),
    ("delete_rule", ColumnType::Text),
];
