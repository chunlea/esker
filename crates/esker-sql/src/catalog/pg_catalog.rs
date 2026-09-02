//! `pg_catalog`, as relations **computed** from what this node has rather than stored.
//!
//! `docs/plans/phase-9-rails.md` §Unit 5 decided the shape and this is it: a catalog relation is a
//! view over the records, materialised per query, with no duplicated state and no way to drift.
//! The alternative — catalog tables as real tables in a reserved key space — would give every
//! `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE` and sequence allocation a second write to keep in
//! step, forever, and a `pg_class` that disagreed with the `'m'` space would be wrong in a way
//! only a client notices.
//!
//! # What is in it, and why it is short
//!
//! **`pg_type` lists the types this server has**, with PostgreSQL's own OIDs for them, because
//! those are the OIDs a `RowDescription` from this node carries. It grows as the type surface does
//! and cannot be forgotten: the rows are derived from `ColumnType::ALL`. The alternative was to
//! list PostgreSQL's standard set — `int4` is 23, `numeric` is 1700 — and that would tell a client
//! this server has types it answers `0A000` for.
//!
//! The predecessor left the choice open and asked for a capture rather than an argument, and the
//! capture is of `ActiveRecord` rather than of PostgreSQL: `add_pg_decoders` builds its decoders
//! with `filter_map`, so a name it gets no row for is a decoder it does not make;
//! `TypeMapInitializer#run` partitions the rows it is handed and registers each partition, so an
//! empty partition registers nothing; and a column whose OID is in no map decodes as a string.
//! **A short `pg_type` costs a client nothing it can see, and this node never sends an OID that is
//! not in it.** `tests/corpus/pg19_pg_catalog.txt` carries the argument beside the rows.
//!
//! **`pg_range` is empty**, because this node has no range types, and it has no `oid` column —
//! which is not an omission but the capture: `SELECT oid FROM pg_range` is `42703` on a real
//! server, and that is what makes `ActiveRecord`'s `LEFT JOIN pg_range AS r ON oid = rngtypid`
//! legal with an unqualified `oid`. A `pg_range` given an `oid` would make that statement `42702`.
//!
//! # Read-only, and the refusal is `42501`
//!
//! Measured rather than assumed: `DROP TABLE pg_type`, `ALTER TABLE pg_type ADD COLUMN` and
//! `CREATE INDEX … ON pg_type` all answer `42501 permission denied: "pg_type" is a system
//! catalog`. So does every write here — including the DML a *superuser* is allowed to attempt on a
//! real server, which is the divergence `tests/pg_catalog.rs` declares: there are no roles here,
//! and a computed relation has nothing to write to.

use std::sync::Arc;
use std::sync::OnceLock;

use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum, PgType};

/// Where the reserved relation ids for catalog views start.
///
/// A tenant's relation ids come from a sequence that starts at 1, so nothing a user creates can
/// reach here. The id exists because a `TableDef` has one and `EXPLAIN` prints it; no key is ever
/// built from it, because a view is never scanned — [`crate::exec`]'s access path returns a
/// [`crate::plan::Node::CatalogView`] before a range is computed.
///
/// **Inside `i64`**, because `'pg_type'::regclass` answers this number and `pg_class.oid` is a
/// `bigint`: above `i64::MAX` every view would clamp to the same value and three of them would
/// share an oid, which is the failure `crate::catalog::pg_relations` exists to prevent.
const VIEW_ID_BASE: u64 = (i64::MAX as u64) - 1023;

/// One relation of `pg_catalog`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogView {
    /// Every type this server has.
    PgType,
    /// The range types this server has, which is none.
    PgRange,
    /// Every relation this tenant has: tables, indexes and sequences.
    ///
    /// The first catalog view whose rows are **not** constants — they come from a scan of the
    /// catalog's own name records, which is what makes it a view over the records rather than a
    /// second copy of them. `ActiveRecord` lists a schema's tables through it on every boot.
    PgClass,
    /// The schemas this tenant has, which is one.
    PgNamespace,
    /// Every column of every relation this tenant has — a table's own, and the ones an index or a
    /// primary key is over ([`crate::catalog::pg_attribute`]).
    PgAttribute,
    /// One row per column that has a **default**, and none for the rest.
    PgAttrdef,
    /// Every index this tenant has, and every primary key — which has no index behind it here
    /// and a row all the same ([`crate::catalog::pg_index`]).
    PgIndex,
    /// Every constraint this tenant has: its primary keys, and PostgreSQL 19's `NOT NULL` rows
    /// ([`crate::catalog::pg_constraint`]).
    PgConstraint,
    /// `information_schema.tables`: one row per table, and nothing else this tenant has.
    ///
    /// The five below are the SQL standard's view of the same records, and their names carry the
    /// schema because a bare `tables` is `42P01` on a real server
    /// ([`crate::catalog::information_schema`]).
    InformationSchemaTables,
    /// `information_schema.columns`.
    InformationSchemaColumns,
    /// `information_schema.table_constraints`, where a `NOT NULL` appears as a `CHECK`.
    InformationSchemaTableConstraints,
    /// `information_schema.key_column_usage`: a primary key's columns, **one row each** — the
    /// answer `pg_index.indkey` cannot give without an array value.
    InformationSchemaKeyColumnUsage,
    /// `information_schema.referential_constraints`, which is empty: no foreign keys.
    InformationSchemaReferentialConstraints,
    /// The collations this server has, which is none.
    ///
    /// Empty for the reason [`CatalogView::PgRange`] is: a collation is a feature this node does
    /// not have at all, so listing PostgreSQL's 880 would tell a client it could ask for one. What
    /// makes the emptiness safe is measured — `ActiveRecord` reads it only through
    /// `LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation`,
    /// and that condition is **false for every column on a real server too**, so the answer is
    /// NULL on both.
    PgCollation,
}

impl CatalogView {
    /// Every view, for the tests that must not silently skip one.
    pub const ALL: [CatalogView; 14] = [
        CatalogView::PgType,
        CatalogView::PgRange,
        CatalogView::PgClass,
        CatalogView::PgNamespace,
        CatalogView::PgAttribute,
        CatalogView::PgAttrdef,
        CatalogView::PgIndex,
        CatalogView::PgConstraint,
        CatalogView::PgCollation,
        CatalogView::InformationSchemaTables,
        CatalogView::InformationSchemaColumns,
        CatalogView::InformationSchemaTableConstraints,
        CatalogView::InformationSchemaKeyColumnUsage,
        CatalogView::InformationSchemaReferentialConstraints,
    ];

    /// The name a query spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            CatalogView::PgType => "pg_type",
            CatalogView::PgRange => "pg_range",
            CatalogView::PgClass => "pg_class",
            CatalogView::PgNamespace => "pg_namespace",
            CatalogView::PgAttribute => "pg_attribute",
            CatalogView::PgAttrdef => "pg_attrdef",
            CatalogView::PgIndex => "pg_index",
            CatalogView::PgConstraint => "pg_constraint",
            CatalogView::PgCollation => "pg_collation",
            CatalogView::InformationSchemaTables => "information_schema.tables",
            CatalogView::InformationSchemaColumns => "information_schema.columns",
            CatalogView::InformationSchemaTableConstraints => {
                "information_schema.table_constraints"
            }
            CatalogView::InformationSchemaKeyColumnUsage => "information_schema.key_column_usage",
            CatalogView::InformationSchemaReferentialConstraints => {
                "information_schema.referential_constraints"
            }
        }
    }

    /// Its reserved relation id.
    #[must_use]
    fn id(self) -> u64 {
        VIEW_ID_BASE
            + match self {
                CatalogView::PgType => 0,
                CatalogView::PgRange => 1,
                CatalogView::PgClass => 2,
                CatalogView::PgNamespace => 3,
                CatalogView::PgAttribute => 4,
                CatalogView::PgAttrdef => 5,
                CatalogView::PgIndex => 6,
                CatalogView::PgConstraint => 7,
                CatalogView::PgCollation => 8,
                CatalogView::InformationSchemaTables => 9,
                CatalogView::InformationSchemaColumns => 10,
                CatalogView::InformationSchemaTableConstraints => 11,
                CatalogView::InformationSchemaKeyColumnUsage => 12,
                CatalogView::InformationSchemaReferentialConstraints => 13,
            }
    }

    /// Its columns, in the order a row carries them.
    ///
    /// **Exactly the columns `ActiveRecord` reads**, and another is `42703 column "x" does not
    /// exist` — the same answer a real server gives for a name that is not a column at all. That
    /// is a divergence in the honest direction: a column answered with a value nobody measured
    /// would be a wrong answer, and one refused by name is a gap a capture closes.
    ///
    /// The types are this node's own. A real server's `pg_type.oid` is `oid`, `typname` is `name`,
    /// `typdelim` and `typtype` are `"char"` and `typinput` is `regproc`; this node has none of
    /// those four, so an `oid` is a `bigint` and the other three are `text`. The *values* are
    /// identical and only `RowDescription`'s OID differs — declared in `tests/pg_catalog.rs`.
    #[must_use]
    pub fn columns(self) -> &'static [(&'static str, ColumnType)] {
        match self {
            CatalogView::PgType => &[
                ("oid", ColumnType::Int8),
                ("typname", ColumnType::Text),
                ("typelem", ColumnType::Int8),
                ("typdelim", ColumnType::Text),
                ("typinput", ColumnType::Text),
                ("typtype", ColumnType::Text),
                ("typbasetype", ColumnType::Int8),
                // **Last**, because `SELECT *` expands in this order (`7be39ca`) and a column
                // added anywhere else would move every one after it. Read only by
                // `ActiveRecord`'s `columns()`, and only as `a.attcollation <> t.typcollation`
                // — see `CatalogView::PgCollation`.
                ("typcollation", ColumnType::Int8),
            ],
            // No `oid`: see the module note. It is what keeps `ON oid = rngtypid` unambiguous.
            CatalogView::PgRange => &[
                ("rngtypid", ColumnType::Int8),
                ("rngsubtype", ColumnType::Int8),
            ],
            // Exactly the four `ActiveRecord` reads. `relname` and `nspname` are `name` on a real
            // server — the 64-byte identifier type — and `text` here, which compares identically.
            CatalogView::PgClass => &[
                ("oid", ColumnType::Int8),
                ("relname", ColumnType::Text),
                ("relnamespace", ColumnType::Int8),
                ("relkind", ColumnType::Text),
            ],
            CatalogView::PgNamespace => &[("oid", ColumnType::Int8), ("nspname", ColumnType::Text)],
            // In PostgreSQL's own order, restricted to what this node has — `SELECT *` expands in
            // that order and a client reading by position would otherwise read the wrong column.
            // `attnum` is an `int2` and `atttypmod` an `int4` on both servers, which is two fewer
            // declared-type divergences than `pg_class` has.
            CatalogView::PgAttribute => super::pg_attribute::ATTRIBUTE_COLUMNS,
            CatalogView::PgAttrdef => super::pg_attribute::ATTRDEF_COLUMNS,
            CatalogView::PgIndex => super::pg_index::INDEX_COLUMNS,
            CatalogView::PgConstraint => super::pg_constraint::CONSTRAINT_COLUMNS,
            CatalogView::PgCollation => {
                &[("oid", ColumnType::Int8), ("collname", ColumnType::Text)]
            }
            CatalogView::InformationSchemaTables => super::information_schema::TABLES_COLUMNS,
            CatalogView::InformationSchemaColumns => super::information_schema::COLUMNS_COLUMNS,
            CatalogView::InformationSchemaTableConstraints => {
                super::information_schema::TABLE_CONSTRAINTS_COLUMNS
            }
            CatalogView::InformationSchemaKeyColumnUsage => {
                super::information_schema::KEY_COLUMN_USAGE_COLUMNS
            }
            CatalogView::InformationSchemaReferentialConstraints => {
                super::information_schema::REFERENTIAL_CONSTRAINTS_COLUMNS
            }
        }
    }

    /// Every row, in OID order.
    ///
    /// PostgreSQL promises no order at all without an `ORDER BY` and returns its own physical
    /// order; this one is deterministic, which is a superset of that promise and is the order
    /// `ORDER BY oid` would have given anyway. The same choice `crate::exec::aggregate` made for
    /// group order, and for the same reason.
    /// Every row, in OID order — reading the catalog for the views whose rows are not constants.
    ///
    /// `txn` and `tenant` are what make `pg_class` a **view over the records** rather than a
    /// second copy of them: its rows are one scan of the same name keys `CREATE TABLE` writes, so
    /// there is no state to keep in step and no way for the two to disagree. The constant views
    /// ignore both.
    pub fn rows_of(self, txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
        match self {
            CatalogView::PgClass => pg_class_rows(txn, tenant),
            CatalogView::PgAttribute => super::pg_attribute::rows(txn, tenant),
            CatalogView::PgAttrdef => super::pg_attribute::default_rows(txn, tenant),
            CatalogView::PgIndex => super::pg_index::rows(txn, tenant),
            CatalogView::PgConstraint => super::pg_constraint::rows(txn, tenant),
            CatalogView::InformationSchemaTables => super::information_schema::tables(txn, tenant),
            CatalogView::InformationSchemaColumns => {
                super::information_schema::columns(txn, tenant)
            }
            CatalogView::InformationSchemaTableConstraints => {
                super::information_schema::table_constraints(txn, tenant)
            }
            CatalogView::InformationSchemaKeyColumnUsage => {
                super::information_schema::key_column_usage(txn, tenant)
            }
            CatalogView::PgNamespace => Ok(vec![vec![
                Datum::Int8(PUBLIC_NAMESPACE_OID),
                Datum::Text(PUBLIC_SCHEMA.to_owned()),
            ]]),
            constant => Ok(constant.rows()),
        }
    }

    #[must_use]
    fn rows(self) -> Vec<Vec<Datum>> {
        match self {
            // Derived from `ColumnType::ALL` rather than written out, so a type cannot be added
            // to this node and left out of its own `pg_type`.
            CatalogView::PgType => {
                let mut rows: Vec<Vec<Datum>> = ColumnType::ALL
                    .iter()
                    .map(|ty| {
                        vec![
                            Datum::Int8(i64::from(ty.oid())),
                            Datum::Text(typname(*ty).to_owned()),
                            // No array types here, so no element type and no base type; `b` is
                            // "base", as against `r`ange, `e`num, `d`omain and `c`omposite.
                            Datum::Int8(0),
                            Datum::Text(",".to_owned()),
                            Datum::Text(typinput(*ty).to_owned()),
                            Datum::Text("b".to_owned()),
                            Datum::Int8(0),
                            // No collation on any type here, which is what makes
                            // `a.attcollation <> t.typcollation` false for every column — the
                            // same answer a real server gives, by the same comparison.
                            Datum::Int8(0),
                        ]
                    })
                    .collect();
                rows.sort_by_key(|row| match row.first() {
                    Some(Datum::Int8(oid)) => *oid,
                    _ => 0,
                });
                rows
            }
            // `PgRange` and `PgCollation` have none, and the catalog-backed views never reach
            // here — `rows_of` answers for those before it delegates.
            CatalogView::PgRange
            | CatalogView::PgCollation
            | CatalogView::PgClass
            | CatalogView::PgNamespace
            | CatalogView::PgAttribute
            | CatalogView::PgAttrdef
            | CatalogView::PgIndex
            | CatalogView::PgConstraint
            | CatalogView::InformationSchemaTables
            | CatalogView::InformationSchemaColumns
            | CatalogView::InformationSchemaTableConstraints
            | CatalogView::InformationSchemaKeyColumnUsage
            // Empty, and a correct answer: this node has no foreign keys, so there is nothing
            // referential to constrain.
            | CatalogView::InformationSchemaReferentialConstraints => Vec::new(),
        }
    }

    /// The relation, as everything above the catalog sees one.
    ///
    /// No primary key and no index, which is what makes the planner pick a scan of the view and a
    /// materialised inner side for a join — a computed relation has no key range to seek in.
    #[must_use]
    pub fn table_def(self) -> Arc<TableDef> {
        static DEFS: OnceLock<Vec<Arc<TableDef>>> = OnceLock::new();
        let defs = DEFS.get_or_init(|| {
            CatalogView::ALL
                .iter()
                .map(|view| {
                    Arc::new(TableDef {
                        id: view.id(),
                        name: view.name().to_owned(),
                        columns: view
                            .columns()
                            .iter()
                            .map(|(name, ty)| ColumnDef {
                                name: (*name).to_owned(),
                                ty: *ty,
                                // A computed relation declares no lengths.
                                typmod: crate::value::NO_TYPMOD,
                                default_now: false,
                                not_null: false,
                                default: None,
                                missing: None,
                            })
                            .collect(),
                        primary_key: Vec::new(),
                        indexes: Vec::new(),
                        primary_key_name: String::new(),
                        schema_version: 1,
                        sequences: Vec::new(),
                        checks: Vec::new(),
                        foreign_keys: Vec::new(),
                    })
                })
                .collect()
        });
        let at = CatalogView::ALL
            .iter()
            .position(|view| *view == self)
            .unwrap_or(0);
        Arc::clone(&defs[at])
    }
}

/// The view a name is, if it is one.
#[must_use]
pub fn view(name: &str) -> Option<CatalogView> {
    CatalogView::ALL
        .into_iter()
        .find(|view| view.name() == name)
}

/// The view a relation is, if it is one. By id, so a `TableDef` that has travelled does not have
/// to be recognised by its name.
#[must_use]
pub fn view_of(table: &TableDef) -> Option<CatalogView> {
    CatalogView::ALL
        .into_iter()
        .find(|view| view.id() == table.id)
}

/// `42501` for any write to a catalog relation, or `Ok` for a name that is not one.
///
/// The one guard every write path calls. Measured: a real server answers exactly this sentence for
/// `DROP TABLE`, `ALTER TABLE` and `CREATE INDEX` on `pg_type`.
pub fn refuse_write(name: &str) -> Result<()> {
    match view(name) {
        Some(view) => Err(SqlError::SystemCatalog(view.name())),
        None => Ok(()),
    }
}

/// The one schema this node has, and the id `pg_class.relnamespace` points at.
///
/// A real server's is whatever `CREATE SCHEMA` allocated and differs per database; this one is
/// reserved beside the view ids for the same reason they are — nothing a user creates can reach
/// it. What has to be true is only that `relnamespace` equals `pg_namespace.oid`, which is the
/// join `ActiveRecord` writes.
const PUBLIC_NAMESPACE_OID: i64 = 11;

/// The schema every relation is in.
const PUBLIC_SCHEMA: &str = "public";

/// Every relation this tenant has, out of the one snapshot every view in this phase reads.
///
/// The value of a name record says which kind of relation it is, and that is exactly what
/// `relkind` reports. Measured on 19beta1: a table is `r`, an index is `i`, and a sequence is `S`
/// — and a **primary key is an index**, `r4a_pkey` with relkind `i`, even here where the row key
/// *is* the primary key and no separate index exists. That is the right answer rather than a
/// convenient one: what `relkind` describes is the relation a client can name, and a client can
/// name `r4a_pkey`.
///
/// The **oid** is [`crate::catalog::pg_relations`]'s, which is what changed here: this function
/// used to read the id out of the name record's *key*, so a primary key and a sequence both
/// reported the table's own id and three rows of `pg_class` shared one oid. Nothing read the
/// column before phase 13; every statement in the schema-dump path joins on it.
fn pg_class_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    // By name, which is the order the scan already returns them in and the order a reader can
    // predict. PostgreSQL promises no order without an `ORDER BY`; a deterministic one is a
    // superset of that promise, as `pg_type`'s rows are.
    Ok(relations
        .rows()
        .map(|relation| {
            vec![
                Datum::Int8(relation.oid),
                Datum::Text(relation.name.clone()),
                Datum::Int8(PUBLIC_NAMESPACE_OID),
                Datum::Text(relation.kind.relkind().to_owned()),
            ]
        })
        .collect())
}

/// `pg_type.typname`: the internal name, which is not the one this node complains with — a column
/// is declared `int8` and named `bigint` in an error. Measured against 19beta1, all six.
pub(super) fn typname(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Int8 => "int8",
        ColumnType::Int4 => "int4",
        ColumnType::Int2 => "int2",
        ColumnType::Text => "text",
        ColumnType::Varchar => "varchar",
        ColumnType::Bpchar => "bpchar",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Bool => "bool",
        ColumnType::Bytea => "bytea",
        ColumnType::TimestampTz => "timestamptz",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Double => "float8",
        ColumnType::Real => "float4",
        ColumnType::Date => "date",
    }
}

/// `pg_type.typinput`: the name of the type's input function. A `regproc` on a real server and
/// `text` here, with the same characters in it.
///
/// `ActiveRecord` reads it, and reads it by comparison — `row["typinput"] == "array_in"` is how it
/// tells an array type from everything else — so the value is load-bearing and the spelling is the
/// capture's, underscore and all: `timestamptz_in` has one and `int8in` does not.
fn typinput(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Int8 => "int8in",
        ColumnType::Int4 => "int4in",
        ColumnType::Int2 => "int2in",
        ColumnType::Text => "textin",
        ColumnType::Varchar => "varcharin",
        ColumnType::Bpchar => "bpcharin",
        ColumnType::Json => "json_in",
        ColumnType::Jsonb => "jsonb_in",
        ColumnType::Bool => "boolin",
        ColumnType::Bytea => "byteain",
        ColumnType::TimestampTz => "timestamptz_in",
        ColumnType::Timestamp => "timestamp_in",
        ColumnType::Double => "float8in",
        ColumnType::Real => "float4in",
        ColumnType::Date => "date_in",
    }
}
