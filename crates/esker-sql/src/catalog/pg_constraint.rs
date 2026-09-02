//! `pg_constraint`, computed from the primary key and the `NOT NULL` columns.
//!
//! What `ActiveRecord` reads it for, four methods and four `contype`s: `foreign_keys()` (`f`),
//! `check_constraints()` (`c`), `unique_constraints()` (`u`) and `exclusion_constraints()` (`x`).
//! This node has none of those four, and the honest answer to each is no rows — which is a
//! *correct* answer about this catalog rather than an omission, and every one of them is named in
//! `docs/plans/phase-13-catalog.md` §0.
//!
//! What it does have is the two `contype`s nothing asked about:
//!
//! # `n` — and PostgreSQL 19 is the first major that has it
//!
//! **A `NOT NULL` column has a `pg_constraint` row**, `contype` `n`, named
//! `<table>_<column>_not_null`, whose `pg_get_constraintdef` is `NOT NULL id`. That row did not
//! exist in older majors, so a catalog emulation written from an older memory would be missing it
//! — measured, not remembered, and it is why `information_schema.table_constraints` reports a
//! `NOT NULL` as a `CHECK` (unit 4).
//!
//! A primary key's columns get one each **without saying `NOT NULL`**, because a key column is
//! `NOT NULL` whether it says so or not; a column that says so *and* is in the key gets one row,
//! not two. Measured over `kc (k1 int8, k2 text NOT NULL, …, PRIMARY KEY (k1, k2))`: two rows.
//!
//! # `u` is missing, and that is a gap rather than an answer
//!
//! A `UNIQUE` constraint and a `CREATE UNIQUE INDEX` produce **the same record here** — an
//! [`crate::catalog::IndexDef`] with `unique` set — and PostgreSQL tells them apart: the first has
//! a `pg_constraint` row and the second does not. Nothing in the catalog record says which one
//! wrote the index, so this reports the shape it can prove (a unique index, in `pg_index`) and
//! claims no constraint. The alternative — a `u` row for every unique index — would tell
//! `unique_constraints()` about a constraint the user never declared, which is a wrong answer
//! rather than a missing one.
//!
//! Closing it needs one bool on `IndexDef`, which is a catalog **record format** change and
//! therefore a question for a human rather than a decision for this lane (`CLAUDE.md`, "Ask before
//! doing"). Until then `schema_dumper` writes `t.index …, unique: true` where a real server writes
//! `t.unique_constraint …`, and the schema that round-trips is the same schema.
//!
//! # An oid for a thing with no record
//!
//! A primary key's constraint **is** its relation, so it uses that relation's oid
//! ([`crate::catalog::pg_relations`]). A `NOT NULL` constraint has no relation and no record at
//! all: its oid is derived from the table and the column, in a band of its own, and
//! [`constraint_definition`] reverses it. Both are stable for as long as the column is, which is
//! what a client that reads `c.oid` and then calls `pg_get_constraintdef(c.oid)` needs.

use crate::backend::Txn;
use crate::catalog::TableDef;
use crate::catalog::pg_relations::{self, RelKind, Relations};
use crate::error::Result;
use crate::value::{ColumnType, Datum};

/// Where a `NOT NULL` constraint's oid comes from: `1 << 61`, then the table and the column.
///
/// Below [`pg_relations::PRIMARY_KEY_OID_BASE`] (`1 << 62`) and above anything a per-tenant
/// relation-id sequence starting at 1 can reach. The table id is shifted by 16 and the attnum
/// occupies the low bits, which is exact for every catalog a `Datum::Int2` attnum can describe —
/// a column number is at most `i16::MAX`, so it cannot carry into the table's half.
const NOT_NULL_OID_BASE: u64 = 0x2000_0000_0000_0000;

/// How many bits the attnum occupies at the bottom of a `NOT NULL` constraint's oid.
const NOT_NULL_COLUMN_BITS: u32 = 16;

/// Where a `CHECK` constraint's synthetic oid starts.
///
/// Its own region, between the `NOT NULL` one and `PRIMARY_KEY_OID_BASE`, for the same reason
/// those two have theirs: `pg_get_constraintdef(oid)` is given nothing but the number, so the
/// number has to say which constraint it is. A `CHECK` is not a relation here — nothing can name
/// it in a query — so it has no relation oid to borrow, and the table plus its position in the
/// table's `checks` is what identifies it.
const CHECK_OID_BASE: u64 = 0x3000_0000_0000_0000;

/// Bits reserved for a check's position within its table, mirroring [`NOT_NULL_COLUMN_BITS`].
const CHECK_INDEX_BITS: u32 = 16;

/// The oid of the `at`-th `CHECK` on `table_id`.
fn check_oid(table_id: u64, at: usize) -> i64 {
    let at = u64::try_from(at).unwrap_or(0);
    i64::try_from(CHECK_OID_BASE + (table_id << CHECK_INDEX_BITS) + at).unwrap_or(i64::MAX)
}

/// The table and position a `CHECK` oid names, or `None` for an oid outside that region.
fn check_of(oid: i64) -> Option<(u64, usize)> {
    let oid = u64::try_from(oid).ok()?;
    let below = oid.checked_sub(CHECK_OID_BASE)?;
    if below >= pg_relations::PRIMARY_KEY_OID_BASE - CHECK_OID_BASE {
        return None;
    }
    let at = usize::try_from(below & ((1 << CHECK_INDEX_BITS) - 1)).ok()?;
    Some((below >> CHECK_INDEX_BITS, at))
}

/// `confupdtype` and `confdeltype` for a constraint that is not a foreign key: a **space**.
///
/// Measured — not the empty string, which is what `attidentity` uses for "none", and not NULL.
/// Two neighbouring catalogs spell "no value" two different ways.
const NO_FOREIGN_ACTION: &str = " ";

/// The one schema, and the id `pg_constraint.connamespace` points at — `pg_class.relnamespace`'s.
const PUBLIC_NAMESPACE_OID: i64 = 11;

/// Every `pg_constraint` row this tenant has.
pub fn rows(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    Ok(rows_from(&Relations::read(txn, tenant)?))
}

/// The same, over a snapshot somebody else has already read — which is how
/// `information_schema.table_constraints` gets these rows without a second scan of the catalog.
#[must_use]
pub fn rows_from(relations: &Relations) -> Vec<Vec<Datum>> {
    let mut rows = Vec::new();
    for relation in relations.of_kind(RelKind::Table) {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        for constraint in constraints_of(relations, table, relation.oid) {
            rows.push(vec![
                Datum::Int8(constraint.oid),
                Datum::Text(constraint.name),
                Datum::Int8(PUBLIC_NAMESPACE_OID),
                Datum::Text(constraint.contype.to_owned()),
                // Nothing here is deferrable: `DEFERRABLE` is refused by name in the DDL, and a
                // constraint that is not deferrable is `f`/`f` — which is also what a real server
                // says for a `DEFERRABLE INITIALLY IMMEDIATE` one's `condeferred`.
                Datum::Bool(false),
                Datum::Bool(false),
                // Every constraint here is validated: there is no `NOT VALID` to leave one behind.
                Datum::Bool(true),
                Datum::Int8(relation.oid),
                Datum::Int8(constraint.conindid),
                // No foreign keys, so no referenced relation and no referential actions.
                Datum::Int8(0),
                Datum::Text(NO_FOREIGN_ACTION.to_owned()),
                Datum::Text(NO_FOREIGN_ACTION.to_owned()),
            ]);
        }
    }
    rows
}

/// `pg_get_constraintdef(oid [, pretty])`.
///
/// NULL for an oid that names no constraint, measured — the same rule `pg_get_indexdef` follows,
/// and for the same reason: a client calls it over a column of oids it has not filtered.
///
/// The `pretty` flag changes nothing this node prints. On a real server it re-wraps a long `CHECK`
/// expression, and there are no `CHECK` constraints here.
#[must_use]
pub fn constraint_definition(relations: &Relations, oid: Option<i64>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    // A primary key: its constraint is its relation, so the snapshot already knows it.
    if let Some(relation) = relations.by_oid(oid)
        && relation.kind == RelKind::PrimaryKey
        && let Some(table) = relations.table(relation)
    {
        return Datum::Text(primary_key_definition(table));
    }
    // A `CHECK`: the oid is the table and the check's position, read back the same way.
    if let Some((table_id, at)) = check_of(oid)
        && let Some(table) = relations
            .rows()
            .find(|row| row.kind == RelKind::Table && row.table_id == table_id)
            .and_then(|row| relations.table(row))
        && let Some(check) = table.checks.get(at)
    {
        // `CHECK ((p > 0))` — the doubled parentheses are PostgreSQL's, which wraps the whole
        // predicate and then prints it parenthesised. Measured.
        return Datum::Text(format!("CHECK (({}))", check.expr));
    }
    // A `NOT NULL`: the oid is the table and the column, and this is where it is read back.
    let Some((table_id, attnum)) = not_null_of(oid) else {
        return Datum::Null;
    };
    let Some(table) = relations
        .rows()
        .find(|row| row.kind == RelKind::Table && row.table_id == table_id)
        .and_then(|row| relations.table(row))
    else {
        return Datum::Null;
    };
    match not_null_columns(table)
        .into_iter()
        .find(|(_, at)| pg_relations::attnum_of(table, *at) == attnum)
    {
        Some((name, _)) => Datum::Text(format!("NOT NULL {name}")),
        None => Datum::Null,
    }
}

/// One computed constraint row, before it is turned into `Datum`s.
struct Constraint {
    oid: i64,
    name: String,
    contype: &'static str,
    conindid: i64,
}

/// Every constraint one table has, in name order — which is the order `pg_constraint` is read in.
fn constraints_of(relations: &Relations, table: &TableDef, table_oid: i64) -> Vec<Constraint> {
    let mut out: Vec<Constraint> = not_null_columns(table)
        .into_iter()
        .map(|(name, at)| Constraint {
            oid: not_null_oid(table.id, pg_relations::attnum_of(table, at)),
            // PostgreSQL's own spelling, measured: `ka_id_not_null`.
            name: format!("{}_{name}_not_null", table.name),
            contype: "n",
            // Zero, measured: a `NOT NULL` is enforced by the column and has no index behind it.
            conindid: 0,
        })
        .collect();
    if !table.primary_key_name.is_empty() {
        // The primary key's constraint **is** its relation, so its oid is that relation's — and
        // `conindid` is the same value, because on a real server the constraint points at the
        // index that enforces it and here the two are one thing.
        let oid = relations
            .by_name(&table.primary_key_name)
            .map_or(table_oid, |relation| relation.oid);
        out.push(Constraint {
            oid,
            name: table.primary_key_name.clone(),
            contype: "p",
            conindid: oid,
        });
    }
    // `CHECK`, contype `c`. It has no index behind it, so `conindid` is zero for the same reason
    // a `NOT NULL`'s is: the column enforces it, not a relation.
    for (at, check) in table.checks.iter().enumerate() {
        out.push(Constraint {
            oid: check_oid(table.id, at),
            name: check.name.clone(),
            contype: "c",
            conindid: 0,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `PRIMARY KEY (a, b)`, as `pg_get_constraintdef` writes it.
fn primary_key_definition(table: &TableDef) -> String {
    let columns: Vec<&str> = table
        .primary_key
        .iter()
        .filter_map(|at| table.columns.get(*at).map(|column| column.name.as_str()))
        .collect();
    format!("PRIMARY KEY ({})", columns.join(", "))
}

/// Every column that is `NOT NULL`, with its position — a key column included, which is what makes
/// a two-column key produce two of these.
fn not_null_columns(table: &TableDef) -> Vec<(&str, usize)> {
    table
        .user_columns()
        .filter(|(_, column)| column.not_null)
        .map(|(at, column)| (column.name.as_str(), at))
        .collect()
}

/// A `NOT NULL` constraint's oid, from the table and the column it is on.
fn not_null_oid(table_id: u64, attnum: i16) -> i64 {
    let attnum = u64::from(attnum.unsigned_abs());
    i64::try_from(NOT_NULL_OID_BASE + (table_id << NOT_NULL_COLUMN_BITS) + attnum)
        .unwrap_or(i64::MAX)
}

/// The table and column a `NOT NULL` constraint's oid names, or `None` for an oid that is not one.
fn not_null_of(oid: i64) -> Option<(u64, i16)> {
    let oid = u64::try_from(oid).ok()?;
    let below = oid.checked_sub(NOT_NULL_OID_BASE)?;
    if below >= pg_relations::PRIMARY_KEY_OID_BASE - NOT_NULL_OID_BASE {
        return None;
    }
    let attnum = i16::try_from(below & ((1 << NOT_NULL_COLUMN_BITS) - 1)).ok()?;
    Some((below >> NOT_NULL_COLUMN_BITS, attnum))
}

/// The columns of `pg_constraint`, in PostgreSQL's own order.
pub const CONSTRAINT_COLUMNS: &[(&str, ColumnType)] = &[
    ("oid", ColumnType::Int8),
    ("conname", ColumnType::Text),
    ("connamespace", ColumnType::Int8),
    ("contype", ColumnType::Text),
    ("condeferrable", ColumnType::Bool),
    ("condeferred", ColumnType::Bool),
    ("convalidated", ColumnType::Bool),
    ("conrelid", ColumnType::Int8),
    ("conindid", ColumnType::Int8),
    ("confrelid", ColumnType::Int8),
    ("confupdtype", ColumnType::Text),
    ("confdeltype", ColumnType::Text),
];
