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

use std::fmt::Write as _;

use crate::backend::Txn;
use crate::catalog::pg_relations::{self, RelKind, Relations};
use crate::catalog::{IndexDef, TableDef, UniqueKind};
use crate::error::Result;
use crate::value::{ColumnType, Datum};

/// Where a `NOT NULL` constraint's oid comes from: `1 << 61`, then the table and the column.
///
/// Below [`pg_relations::PRIMARY_KEY_OID_BASE`] (`1 << 62`) and above anything a per-tenant
/// relation-id sequence starting at 1 can reach. The table id is shifted by 16 and the attnum
/// occupies the low bits, which is exact for every catalog a `Datum::Int2` attnum can describe —
/// a column number is at most `i16::MAX`, so it cannot carry into the table's half.
const NOT_NULL_OID_BASE: u64 = 0x2000_0000_0000_0000;

/// Where a `FOREIGN KEY`'s synthetic oid starts: its own region **below** the `NOT NULL` one.
///
/// The fourth region, for the reason the other three exist — `pg_get_constraintdef(oid)` is given
/// nothing but the number. Below `NOT_NULL_OID_BASE` and far above the ids a tenant's sequence
/// hands out, so the four regions and the relation ids cannot collide.
const FOREIGN_KEY_OID_BASE: u64 = 0x1000_0000_0000_0000;

/// Bits reserved for a foreign key's position within its table, mirroring [`CHECK_INDEX_BITS`].
const FOREIGN_KEY_INDEX_BITS: u32 = 16;

/// The oid of the `at`-th `FOREIGN KEY` on `table_id`.
fn foreign_key_oid(table_id: u64, at: usize) -> i64 {
    let at = u64::try_from(at).unwrap_or(0);
    i64::try_from(FOREIGN_KEY_OID_BASE + (table_id << FOREIGN_KEY_INDEX_BITS) + at)
        .unwrap_or(i64::MAX)
}

/// The table and position a `FOREIGN KEY` oid names, or `None` for an oid outside that region.
fn foreign_key_of(oid: i64) -> Option<(u64, usize)> {
    let oid = u64::try_from(oid).ok()?;
    let below = oid.checked_sub(FOREIGN_KEY_OID_BASE)?;
    if below >= NOT_NULL_OID_BASE - FOREIGN_KEY_OID_BASE {
        return None;
    }
    let at = usize::try_from(below & ((1 << FOREIGN_KEY_INDEX_BITS) - 1)).ok()?;
    Some((below >> FOREIGN_KEY_INDEX_BITS, at))
}

/// `{2}` / `{1,3}` — attribute numbers as an `int2vector` prints them.
fn attnum_vector(table: &TableDef, ordinals: &[usize]) -> String {
    let numbers: Vec<String> = ordinals
        .iter()
        .map(|at| pg_relations::attnum_of(table, *at).to_string())
        .collect();
    format!("{{{}}}", numbers.join(","))
}

/// Where an `EXCLUDE` constraint's synthetic oid starts: the fifth region.
///
/// An exclusion constraint is not a relation on this node — its `USING gist` index is recorded and
/// never built — so like a `CHECK` it has no relation oid to borrow, and the table plus its
/// position in `TableDef::excludes` is what identifies it. Below `FOREIGN_KEY_OID_BASE` and far
/// above a tenant's relation ids, so the five regions cannot collide.
const EXCLUDE_OID_BASE: u64 = 0x0800_0000_0000_0000;

/// Bits reserved for an exclusion's position within its table, mirroring [`CHECK_INDEX_BITS`].
const EXCLUDE_INDEX_BITS: u32 = 16;

/// The oid of the `at`-th `EXCLUDE` on `table_id`.
///
/// **Shared with its index relation** ([`pg_relations::RelKind::Exclusion`]): the constraint *is*
/// its index here, the arrangement a primary key and a `UNIQUE` constraint already have.
pub(super) fn exclude_oid(table_id: u64, at: usize) -> i64 {
    let at = u64::try_from(at).unwrap_or(0);
    i64::try_from(EXCLUDE_OID_BASE + (table_id << EXCLUDE_INDEX_BITS) + at).unwrap_or(i64::MAX)
}

/// The table and position an `EXCLUDE` oid names, or `None` for an oid outside that region.
fn exclude_of(oid: i64) -> Option<(u64, usize)> {
    let oid = u64::try_from(oid).ok()?;
    let below = oid.checked_sub(EXCLUDE_OID_BASE)?;
    if below >= FOREIGN_KEY_OID_BASE - EXCLUDE_OID_BASE {
        return None;
    }
    let at = usize::try_from(below & ((1 << EXCLUDE_INDEX_BITS) - 1)).ok()?;
    Some((below >> EXCLUDE_INDEX_BITS, at))
}

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
                // `condeferrable` is what was written, and `condeferred` too — an `EXCLUDE` is the
                // one constraint here that can carry `INITIALLY DEFERRED`, which it **records and
                // does not yet honour**: the check runs at the statement. Everything else is
                // `INITIALLY IMMEDIATE` because `INITIALLY DEFERRED` is refused by name on it.
                Datum::Bool(constraint.condeferrable),
                Datum::Bool(constraint.condeferred),
                // Every constraint here is validated: there is no `NOT VALID` to leave one behind.
                Datum::Bool(true),
                Datum::Int8(relation.oid),
                Datum::Int8(constraint.conindid),
                Datum::Int8(
                    constraint
                        .foreign
                        .as_ref()
                        .map_or(0, |foreign| foreign.confrelid),
                ),
                Datum::Text(
                    constraint
                        .foreign
                        .as_ref()
                        .map_or(NO_FOREIGN_ACTION, |foreign| foreign.confupdtype)
                        .to_owned(),
                ),
                Datum::Text(
                    constraint
                        .foreign
                        .as_ref()
                        .map_or(NO_FOREIGN_ACTION, |foreign| foreign.confdeltype)
                        .to_owned(),
                ),
                match &constraint.conkey {
                    Some(conkey) => Datum::Text(conkey.clone()),
                    None => Datum::Null,
                },
                match &constraint.foreign {
                    Some(foreign) => Datum::Text(foreign.confkey.clone()),
                    None => Datum::Null,
                },
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
    // A `UNIQUE` constraint: it **is** its index, so the oid is that relation's — and only an
    // index a constraint made answers here, which is what keeps `CREATE UNIQUE INDEX` out.
    if let Some(relation) = relations.by_oid(oid)
        && relation.kind == RelKind::Index
        && let Some(table) = relations.table(relation)
        && let Some(index) = relation.index_at.and_then(|at| table.indexes.get(at))
        && let Some(kind) = index.constraint
    {
        return Datum::Text(unique_definition(table, index, kind));
    }
    // A `CHECK`: the oid is the table and the check's position, read back the same way.
    if let Some((table_id, at)) = check_of(oid)
        && let Some(table) = table_of(relations, table_id)
        && let Some(check) = table.checks.get(at)
    {
        // `CHECK ((p > 0))` — the doubled parentheses are PostgreSQL's, which wraps the whole
        // predicate and then prints it parenthesised. Measured.
        return Datum::Text(format!("CHECK (({}))", check.expr));
    }
    // A `FOREIGN KEY`: the oid is the table and the constraint's position in its list.
    if let Some((table_id, at)) = foreign_key_of(oid)
        && let Some(table) = table_of(relations, table_id)
        && let Some(key) = table.foreign_keys.get(at)
    {
        return Datum::Text(foreign_key_definition(relations, table, key));
    }
    // An `EXCLUDE`: the oid is the table and the constraint's position in its list.
    if let Some((table_id, at)) = exclude_of(oid)
        && let Some(table) = table_of(relations, table_id)
        && let Some(exclude) = table.excludes.get(at)
    {
        return Datum::Text(exclude_definition(exclude));
    }
    // A `NOT NULL`: the oid is the table and the column, and this is where it is read back.
    let Some((table_id, attnum)) = not_null_of(oid) else {
        return Datum::Null;
    };
    let Some(table) = table_of(relations, table_id) else {
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
    /// The constrained columns as an `int2vector` prints — `{2}`, `{1,3}` — or `None` where the
    /// constraint does not name any.
    ///
    /// **Every kind but `CHECK` has one**, measured: a `NOT NULL` names its column, a primary key
    /// and a foreign key name theirs in declaration order, and that order is the answer — the
    /// schema dump reads `conkey` through `generate_subscripts` precisely to recover it. A
    /// `CHECK`'s `conkey` is the set of columns its expression mentions, which needs the
    /// expression analysed rather than the definition read, so it is `None` here and a divergence
    /// where a statement asks for it.
    conkey: Option<String>,
    /// A `FOREIGN KEY`'s columns, or `None` for every other kind.
    foreign: Option<ForeignColumns>,
    /// `condeferrable`. A `UNIQUE` constraint carries it as well as a `FOREIGN KEY`, and it says
    /// nothing about *when* the check runs here — every constraint this node holds is checked at
    /// the statement.
    condeferrable: bool,
    /// `condeferred` — `INITIALLY DEFERRED`, which only an `EXCLUDE` can carry here. Recorded and
    /// not yet honoured; see [`crate::catalog::ExcludeDef::deferred`].
    condeferred: bool,
}

/// What a `FOREIGN KEY` row carries that no other constraint does.
struct ForeignColumns {
    confrelid: i64,
    confupdtype: &'static str,
    confdeltype: &'static str,
    /// The **parent**'s key columns, as an `int2vector` prints. The child's are `Constraint`'s
    /// `conkey`, which every kind of constraint has.
    confkey: String,
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
            conkey: Some(attnum_vector(table, &[at])),
            foreign: None,
            condeferrable: false,
            condeferred: false,
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
            conkey: Some(attnum_vector(table, &table.primary_key)),
            foreign: None,
            condeferrable: false,
            condeferred: false,
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
            conkey: None,
            foreign: None,
            condeferrable: false,
            condeferred: false,
        });
    }
    // `FOREIGN KEY`, contype `f`. `conindid` is the index on the **parent** that the constraint
    // is enforced through — zero here, because the parent's key is read by its primary key or by
    // a unique index this row does not name, and reporting an index oid that is not the one a
    // real server would report is a worse answer than reporting none.
    for (at, key) in table.foreign_keys.iter().enumerate() {
        let parent_row = relations
            .rows()
            .find(|row| row.kind == RelKind::Table && row.table_id == key.parent);
        let parent_oid = parent_row.map_or(0, |row| row.oid);
        let parent = parent_row.and_then(|row| relations.table(row));
        out.push(Constraint {
            oid: foreign_key_oid(table.id, at),
            name: key.name.clone(),
            contype: "f",
            conindid: 0,
            conkey: Some(attnum_vector(table, &key.columns)),
            foreign: Some(ForeignColumns {
                confrelid: parent_oid,
                confupdtype: key.on_update.code(),
                confdeltype: key.on_delete.code(),
                confkey: parent.map_or_else(String::new, |parent| {
                    attnum_vector(parent, &key.parent_columns)
                }),
            }),
            condeferrable: key.deferrable,
            condeferred: false,
        });
    }
    // `UNIQUE`, contype `u`. **Only an index a constraint made**: `CREATE UNIQUE INDEX` builds an
    // identical index and gets no row here, which is the distinction `IndexDef::constraint` exists
    // for. The constraint *is* its index, so the oid and `conindid` are that relation's — the same
    // arrangement a primary key has.
    for index in &table.indexes {
        let Some(kind) = index.constraint else {
            continue;
        };
        let oid = relations
            .by_name(&index.name)
            .map_or(table_oid, |relation| relation.oid);
        out.push(Constraint {
            oid,
            name: index.name.clone(),
            contype: "u",
            conindid: oid,
            // An expression key has no column to name, so `conkey` is absent rather than wrong —
            // the same reading a `CHECK`'s takes. A `UNIQUE` constraint over an expression is not
            // a shape this node can build anyway; the `None` is what makes that visible.
            conkey: index.key_columns().map(|keys| attnum_vector(table, &keys)),
            foreign: None,
            condeferrable: kind == UniqueKind::Deferrable,
            condeferred: false,
        });
    }
    // `EXCLUDE`, contype `x`. The constraint **is** its index, so the oid and `conindid` are one
    // value — the arrangement a primary key and a `UNIQUE` constraint already have here. The
    // **operator lives only in this row**, which is what makes `pg_constraint` the one place an
    // exclusion can be read from: `pg_get_indexdef` prints no `WITH &&`, measured.
    for (at, exclude) in table.excludes.iter().enumerate() {
        let oid = exclude_oid(table.id, at);
        out.push(Constraint {
            oid,
            name: exclude.name.clone(),
            contype: "x",
            conindid: oid,
            // The key is an expression, not a column, so there is nothing to name — the reading a
            // `CHECK`'s `conkey` takes, and for the same reason.
            conkey: None,
            foreign: None,
            condeferrable: exclude.deferrable,
            condeferred: exclude.deferred,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `EXCLUDE USING gist (daterange(a, b) WITH &&) WHERE (((…))) DEFERRABLE INITIALLY DEFERRED`.
///
/// **Triple parentheses around the `WHERE`**, and they are not a typo: PostgreSQL prints the
/// predicate with each operand parenthesised, wraps that, and `pg_get_constraintdef` wraps it
/// again — where `pg_get_indexdef` stops one level earlier. Written
/// `WHERE (a IS NOT NULL AND b IS NOT NULL)`, it comes back
/// `WHERE (((a IS NOT NULL) AND (b IS NOT NULL)))`. Three spellings of one predicate, measured.
///
/// `DEFERRABLE INITIALLY IMMEDIATE` prints as bare `DEFERRABLE` — the same "keep only what differs
/// from the default" rule a deferrable `UNIQUE` follows.
fn exclude_definition(exclude: &crate::catalog::ExcludeDef) -> String {
    let mut out = format!(
        "EXCLUDE USING {} ({} WITH {})",
        exclude.method, exclude.key, exclude.operator
    );
    if let Some(predicate) = &exclude.predicate {
        let _ = write!(
            out,
            " WHERE (({}))",
            super::parenthesised_operands(predicate)
        );
    }
    if exclude.deferrable {
        out.push_str(" DEFERRABLE");
        if exclude.deferred {
            out.push_str(" INITIALLY DEFERRED");
        }
    }
    out
}

/// `UNIQUE (a, b)`, `UNIQUE NULLS NOT DISTINCT (a)`, `UNIQUE (a) DEFERRABLE`.
///
/// **The clause sits on the other side from the index's own rendering**: a constraint writes
/// `UNIQUE NULLS NOT DISTINCT (position_4)` and `pg_get_indexdef` writes
/// `… USING btree (position_4) NULLS NOT DISTINCT`. Measured, both.
///
/// `DEFERRABLE` is kept and `INITIALLY IMMEDIATE` is dropped, so the text that comes back is not
/// the text that went in — the same rule a deferrable foreign key follows.
fn unique_definition(table: &TableDef, index: &IndexDef, kind: UniqueKind) -> String {
    let mut out = "UNIQUE".to_owned();
    if index.nulls_not_distinct {
        out.push_str(" NULLS NOT DISTINCT");
    }
    let columns = index.key_columns().unwrap_or_default();
    let _ = write!(out, " ({})", column_list(table, &columns));
    if kind == UniqueKind::Deferrable {
        out.push_str(" DEFERRABLE");
    }
    out
}

/// `FOREIGN KEY (p) REFERENCES fxp(id) ON UPDATE CASCADE ON DELETE CASCADE DEFERRABLE`.
///
/// Four things the capture settled and none of them guessable:
///
/// * **no space before the parent's column list** — `REFERENCES fxp(id)`, where the child's side
///   has one (`FOREIGN KEY (p)`);
/// * **`ON UPDATE` before `ON DELETE`**, whichever order they were written in;
/// * **the default prints nothing**, so `ON DELETE NO ACTION` written out comes back absent;
/// * **`DEFERRABLE` is kept and `INITIALLY IMMEDIATE` is dropped**, because that one is the
///   default — so the exact clause `ActiveRecord` writes is *not* what comes back out.
fn foreign_key_definition(
    relations: &Relations,
    table: &TableDef,
    key: &crate::catalog::ForeignKeyDef,
) -> String {
    let parent = table_of(relations, key.parent);
    let parent_name = parent.map_or("?", |parent| parent.name.as_str());
    let parent_columns = parent.map_or_else(String::new, |parent| {
        column_list(parent, &key.parent_columns)
    });
    let mut out = format!(
        "FOREIGN KEY ({}) REFERENCES {parent_name}({parent_columns})",
        column_list(table, &key.columns)
    );
    if !key.on_update.clause().is_empty() {
        out.push_str(" ON UPDATE ");
        out.push_str(key.on_update.clause());
    }
    if !key.on_delete.clause().is_empty() {
        out.push_str(" ON DELETE ");
        out.push_str(key.on_delete.clause());
    }
    if key.deferrable {
        out.push_str(" DEFERRABLE");
    }
    out
}

/// `a, b` — a constraint's columns as every definition above prints them.
fn column_list(table: &TableDef, ordinals: &[usize]) -> String {
    ordinals
        .iter()
        .filter_map(|at| table.columns.get(*at).map(|column| column.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One table out of the snapshot, by id.
fn table_of(relations: &Relations, table_id: u64) -> Option<&TableDef> {
    relations
        .rows()
        .find(|row| row.kind == RelKind::Table && row.table_id == table_id)
        .and_then(|row| relations.table(row))
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
    // `int2vector`s on a real server, and text here for the reason `pg_index.indkey` is text: the
    // characters are the same and so is what they mean. **This is not the column that unblocks
    // `ActiveRecord`'s `foreign_keys()`** — that one subscripts them (`c.conkey[idx]`) under
    // `generate_subscripts` and `array_agg`, which needs the array type and its functions. They
    // are here because a `contype = 'f'` row without its key columns is an incomplete record, and
    // `SELECT conkey` is answerable where `conkey[1]` is still `0A000`.
    ("conkey", ColumnType::Text),
    ("confkey", ColumnType::Text),
];
