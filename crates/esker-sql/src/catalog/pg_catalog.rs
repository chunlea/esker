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
use esker_keys::array::ArrayValue;

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
    /// The extensions installed here, which is **none** — where a real server always has at least
    /// `plpgsql`.
    ///
    /// A declared divergence rather than a gap, and the same argument that keeps
    /// [`CatalogView::PgCollation`] empty: a row here would tell a client
    /// `CREATE FUNCTION … LANGUAGE plpgsql` will work, and there is no procedural language and no
    /// `CREATE EXTENSION` on this node. `ActiveRecord` reads it to write `enable_extension` lines
    /// into a schema dump, and none is the truth.
    PgExtension,
    /// Which relation inherits which, which is **nothing**: there is no `INHERITS` and no
    /// `PARTITION BY` here, so the emptiness is complete rather than provisional. Empty on a real
    /// server too until something partitions.
    PgInherits,
    /// The index access methods, which is **two**: `btree` and `gist`.
    ///
    /// A real server has six. These two are the ones this node's own `pg_class.relam` can point
    /// at — everything a `CREATE INDEX` builds is a btree, and an `EXCLUDE` constraint records
    /// `gist` — and a row for a method nothing can be built with would be a claim rather than a
    /// report. Declared: `hash`, `gin`, `spgist` and `brin` are on a real server and not here.
    PgAm,
    /// Every stored function: what `CREATE FUNCTION` wrote and nothing else — this node has no
    /// built-in functions in `pg_proc`, which is a declared divergence.
    PgProc,
    /// Every trigger registered on a table, and never fired.
    PgTrigger,
    /// The procedural languages, which is **one**: `plpgsql`.
    ///
    /// A name rather than a runtime — a `CREATE FUNCTION … LANGUAGE plpgsql` body is stored and
    /// never executed. Reporting the language is what makes that definition storable, and it is
    /// the first thing a client asks before writing one.
    PgLanguage,
    /// One row per partitioned table: its strategy, its key's width, and the key columns.
    PgPartitionedTable,
    /// One row per index, **as a view over `pg_class` and `pg_index`** rather than a catalog
    /// table: the table it is on and the `CREATE INDEX` that would rebuild it.
    ///
    /// A real server defines it in exactly those terms, and `indexdef` is `pg_get_indexdef` — the
    /// same string, which is why the two agree about `INCLUDE (…)` without being written twice.
    PgIndexes,
    /// The databases this server has, which is **one**.
    ///
    /// `ActiveRecord`'s adapter reads it three times while connecting — the encoding, the collation
    /// and the ctype — each joined to `current_database()`, which answers the same name this row
    /// carries so the join matches by construction.
    PgDatabase,
    /// **Which sequence belongs to which column**, and nothing else this node has a dependency
    /// for. `ActiveRecord`'s `pk_and_sequence_for` joins it to `pg_class`, `pg_attribute`,
    /// `pg_constraint` and `pg_namespace` to find the sequence behind a primary key, and
    /// `reset_pk_sequence!` cannot fix a sequence it cannot name — run 46's largest row.
    PgDepend,
    /// A sequence's parameters: start, increment, bounds, cache and cycle.
    ///
    /// **Not `last_value`**, which is state and lives in the sequence relation itself. Two
    /// different reads, and `reset_pk_sequence!` uses both.
    PgSequence,
    /// The values of every enum type, which is **none**: `CREATE TYPE … AS ENUM` is `0A000`, so
    /// nothing can put a row here. Empty on a real server too until somebody makes an enum.
    PgEnum,
    /// What *could* be installed, which is not what is — and **not** [`CatalogView::PgExtension`].
    ///
    /// `pg_extension` lists what is installed; this lists the catalogue a server could install
    /// from, which is a superset and is where the `installed_version IS NULL` case lives.
    /// `ActiveRecord`'s `test/cases/helper.rb` reaches it around the schema load through two
    /// one-line methods, and every suite file stopped on the `42P01` until it existed:
    ///
    /// * `extension_available?` — `SELECT true FROM pg_available_extensions WHERE name = $1`
    /// * `extension_enabled?` — `SELECT installed_version IS NOT NULL FROM … WHERE name = $1`
    ///
    /// **One query shape, three answers**, and `ActiveRecord` tells all three apart because
    /// `query_value` maps "no rows" to `nil`: an installed extension is `t`, an available but
    /// uninstalled one is `f`, and an unknown name is **no row at all** — not `f`, not an error.
    /// A view that returned a row per name, or none for an uninstalled one, would look right in
    /// one case and answer the other two wrongly.
    PgAvailableExtensions,
}

impl CatalogView {
    /// Every view, for the tests that must not silently skip one.
    pub const ALL: [CatalogView; 27] = [
        CatalogView::PgType,
        CatalogView::PgRange,
        CatalogView::PgClass,
        CatalogView::PgNamespace,
        CatalogView::PgAttribute,
        CatalogView::PgAttrdef,
        CatalogView::PgIndex,
        CatalogView::PgConstraint,
        CatalogView::PgCollation,
        CatalogView::PgExtension,
        CatalogView::PgInherits,
        CatalogView::PgAm,
        CatalogView::PgProc,
        CatalogView::PgTrigger,
        CatalogView::PgLanguage,
        CatalogView::PgPartitionedTable,
        CatalogView::PgIndexes,
        CatalogView::PgDatabase,
        CatalogView::PgDepend,
        CatalogView::PgSequence,
        CatalogView::PgEnum,
        CatalogView::PgAvailableExtensions,
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
            CatalogView::PgExtension => "pg_extension",
            CatalogView::PgAvailableExtensions => "pg_available_extensions",
            CatalogView::PgInherits => "pg_inherits",
            CatalogView::PgAm => "pg_am",
            CatalogView::PgProc => "pg_proc",
            CatalogView::PgTrigger => "pg_trigger",
            CatalogView::PgLanguage => "pg_language",
            CatalogView::PgPartitionedTable => "pg_partitioned_table",
            CatalogView::PgIndexes => "pg_indexes",
            CatalogView::PgDatabase => "pg_database",
            CatalogView::PgDepend => "pg_depend",
            CatalogView::PgSequence => "pg_sequence",
            CatalogView::PgEnum => "pg_enum",
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
                CatalogView::PgExtension => 14,
                CatalogView::PgInherits => 15,
                CatalogView::PgAm => 23,
                CatalogView::PgProc => 18,
                CatalogView::PgTrigger => 19,
                CatalogView::PgLanguage => 20,
                CatalogView::PgPartitionedTable => 21,
                CatalogView::PgIndexes => 22,
                CatalogView::PgDatabase => 26,
                CatalogView::PgDepend => 24,
                CatalogView::PgSequence => 25,
                CatalogView::PgEnum => 16,
                CatalogView::PgAvailableExtensions => 17,
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
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per view, each the column list a client reads; splitting it would put \
                  half the catalog's vocabulary somewhere else"
    )]
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
                // **Last for the same reason**, and added for boot statement 26, which joins
                // `pg_type` to `pg_namespace` on it to find a schema's enum types. Every type here
                // reports the one namespace this node has, exactly as every relation's
                // `relnamespace` does; on a real server they are in `pg_catalog`, which is a
                // difference in the schema model and not in this column
                // (`PUBLIC_NAMESPACE_OID`).
                ("typnamespace", ColumnType::Int8),
                // **Last again**, and for the third time the reason is `SELECT *`'s order. Two
                // columns three captures wanted and none of them could get: `typlen` is the
                // width in bytes, `-1` for a varlena, and `typcategory` is PostgreSQL's coarse
                // grouping. Both are derived — the width from `PgType::type_len`, the category
                // from a match a new type has to answer — so neither can drift from the type it
                // describes.
                ("typlen", ColumnType::Int2),
                ("typcategory", ColumnType::Text),
                // **Last for the fourth time**, and the reason has not changed. `typarray` is the
                // oid of the array type paired with this one — a user-defined type gets one made
                // for it by `CREATE TYPE`, with no statement asking — and `typrelid` is the
                // `pg_class` row a **composite** owns and every other type reports as `0`.
                ("typarray", ColumnType::Int8),
                ("typrelid", ColumnType::Int8),
            ],
            // No `oid`: see the module note. It is what keeps `ON oid = rngtypid` unambiguous.
            CatalogView::PgRange => &[
                ("rngtypid", ColumnType::Int8),
                ("rngsubtype", ColumnType::Int8),
            ],
            // Exactly the five `ActiveRecord` reads. `relname` and `nspname` are `name` on a real
            // server — the 64-byte identifier type — and `text` here, which compares identically.
            CatalogView::PgClass => &[
                ("oid", ColumnType::Int8),
                ("relname", ColumnType::Text),
                ("relnamespace", ColumnType::Int8),
                ("relkind", ColumnType::Text),
                ("relhastriggers", ColumnType::Bool),
                // Added for declarative partitioning. `relispartition` says the relation *is* a
                // partition, `relhassubclass` that something inherits from or partitions it, and
                // `relpartbound` holds the bound `pg_get_expr` prints.
                ("relispartition", ColumnType::Bool),
                ("relhassubclass", ColumnType::Bool),
                ("relpartbound", ColumnType::Text),
                // **Last**, because `SELECT *` expands in this order. The access method of an
                // index and **zero** for everything else, which is what a real server reports for
                // a table — the join `pg_am am ON am.oid = i.relam` then finds nothing for one,
                // which is how a client filters indexes by method.
                ("relam", ColumnType::Int8),
                // **Last again.** A `"char"` on a real server and `text` here, with the same
                // single character in it. The one column that tells an `UNLOGGED` table from an
                // ordinary one — `information_schema.tables` calls both `BASE TABLE`, so a client
                // that reads the standard view cannot see persistence at all.
                ("relpersistence", ColumnType::Text),
            ],
            // Exactly the three a client reads. `amname` is a `name` on a real server and `amtype`
            // a `"char"`; both are `text` here, the trade every `pg_catalog` column makes.
            CatalogView::PgAm => &[
                ("oid", ColumnType::Int8),
                ("amname", ColumnType::Text),
                ("amtype", ColumnType::Text),
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
            // Exactly the two `ActiveRecord` reads of each, which is this module's standing rule:
            // a column it does not read is `42703`, the answer a real server gives for a name that
            // is not a column at all, rather than a value nobody measured. `extname` is a `name`
            // and the three oids are `oid` on a real server; all four are this node's own types.
            CatalogView::PgExtension => &[
                ("extname", ColumnType::Text),
                ("extnamespace", ColumnType::Int8),
                // Added for `CREATE EXTENSION`, which is where a version comes from: the one the
                // build offers as `default_version`, not one the statement chooses.
                ("extversion", ColumnType::Text),
            ],
            // `inhseqno` numbers a child's parents from **1**, in the order the `INHERITS` clause
            // wrote them — measured, and it is what tells multiple inheritance apart.
            CatalogView::PgInherits => &[
                ("inhrelid", ColumnType::Int8),
                ("inhparent", ColumnType::Int8),
                ("inhseqno", ColumnType::Int4),
            ],
            // What the capture reads, and `prosrc` because its **length** is how a client checks
            // the body survived. `prokind` is `f` and `provolatile` `v`, both `"char"` on a real
            // server and `text` here.
            CatalogView::PgLanguage => &[
                ("oid", ColumnType::Int8),
                ("lanname", ColumnType::Text),
                ("lanpltrusted", ColumnType::Bool),
            ],
            CatalogView::PgProc => &[
                ("oid", ColumnType::Int8),
                ("proname", ColumnType::Text),
                ("pronamespace", ColumnType::Int8),
                ("prokind", ColumnType::Text),
                ("pronargs", ColumnType::Int2),
                ("provolatile", ColumnType::Text),
                ("prosrc", ColumnType::Text),
                ("prolang", ColumnType::Text),
            ],
            // `tgenabled` is a **letter** and `tgtype` a bitmask, neither of which is the word the
            // DDL used.
            CatalogView::PgTrigger => &[
                ("oid", ColumnType::Int8),
                ("tgrelid", ColumnType::Int8),
                ("tgname", ColumnType::Text),
                ("tgenabled", ColumnType::Text),
                ("tgtype", ColumnType::Int2),
                ("tgnargs", ColumnType::Int2),
                ("tgisinternal", ColumnType::Bool),
                ("tgfoid", ColumnType::Int8),
            ],
            // `schemaname` and `tablespace` are what a real server's view has and this node has
            // neither concept: one schema, and no tablespaces — `public` and NULL, which is what
            // a real server answers for an index in the default tablespace too.
            CatalogView::PgIndexes => &[
                ("schemaname", ColumnType::Text),
                ("tablename", ColumnType::Text),
                ("indexname", ColumnType::Text),
                ("tablespace", ColumnType::Text),
                ("indexdef", ColumnType::Text),
            ],
            // `partstrat` is a **one-letter code** and `partattrs` an `int2vector` — neither is
            // the word the DDL used, which `pg_get_partkeydef` gives instead.
            CatalogView::PgPartitionedTable => &[
                ("partrelid", ColumnType::Int8),
                ("partstrat", ColumnType::Text),
                ("partnatts", ColumnType::Int2),
                ("partattrs", ColumnType::Text),
            ],
            // The five a real server has, in its order. `name` is of type `name` there and the
            // four others are `text`; this node has one string type and answers `text` for all
            // five, which is the same trade every `pg_catalog` column makes.
            CatalogView::PgAvailableExtensions => &[
                ("name", ColumnType::Text),
                ("default_version", ColumnType::Text),
                ("installed_version", ColumnType::Text),
                ("location", ColumnType::Text),
                ("comment", ColumnType::Text),
            ],
            // `enumsortorder` is a `real` on a real server, which is the one place this view's
            // types are worth reading: the order is a float so a value can be inserted *between*
            // two others without renumbering.
            // The four the adapter reads, plus the `oid` every catalog relation carries.
            CatalogView::PgDatabase => &[
                ("oid", ColumnType::Int8),
                ("datname", ColumnType::Text),
                ("encoding", ColumnType::Int4),
                ("datcollate", ColumnType::Text),
                ("datctype", ColumnType::Text),
            ],
            // PostgreSQL's own seven columns, in its own order. `classid`/`objid`/`objsubid`
            // name the **dependent** object and `refclassid`/`refobjid`/`refobjsubid` the one it
            // depends on — a sequence depending on the column it fills, which is the only
            // dependency this node records.
            CatalogView::PgDepend => &[
                ("classid", ColumnType::Int8),
                ("objid", ColumnType::Int8),
                ("objsubid", ColumnType::Int4),
                ("refclassid", ColumnType::Int8),
                ("refobjid", ColumnType::Int8),
                ("refobjsubid", ColumnType::Int4),
                ("deptype", ColumnType::Text),
            ],
            CatalogView::PgSequence => &[
                ("seqrelid", ColumnType::Int8),
                ("seqtypid", ColumnType::Int8),
                ("seqstart", ColumnType::Int8),
                ("seqincrement", ColumnType::Int8),
                ("seqmax", ColumnType::Int8),
                ("seqmin", ColumnType::Int8),
                ("seqcache", ColumnType::Int8),
                ("seqcycle", ColumnType::Bool),
            ],
            CatalogView::PgEnum => &[
                ("enumtypid", ColumnType::Int8),
                ("enumlabel", ColumnType::Text),
                ("enumsortorder", ColumnType::Real),
            ],
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
            CatalogView::PgType => pg_type_rows(txn, tenant),
            CatalogView::PgDepend => pg_depend_rows(txn, tenant),
            CatalogView::PgSequence => pg_sequence_rows(txn, tenant),
            CatalogView::PgClass => pg_class_rows(txn, tenant),
            CatalogView::PgAttribute => super::pg_attribute::rows(txn, tenant),
            CatalogView::PgAttrdef => super::pg_attribute::default_rows(txn, tenant),
            CatalogView::PgIndex => super::pg_index::rows(txn, tenant),
            CatalogView::PgInherits => inherits_rows(txn, tenant),
            CatalogView::PgProc => proc_rows(txn, tenant),
            CatalogView::PgTrigger => trigger_rows(txn, tenant),
            CatalogView::PgPartitionedTable => partitioned_table_rows(txn, tenant),
            CatalogView::PgIndexes => indexes_rows(txn, tenant),
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
            // **Trusted**, which is what `lanpltrusted` says of `plpgsql` on a real server: a
            // non-superuser may write a function in it. Nothing here acts on the flag; it is
            // reported because a client reads it before defining one.
            CatalogView::PgLanguage => Ok(vec![vec![
                Datum::Int8(PLPGSQL_LANGUAGE_OID),
                Datum::Text("plpgsql".to_owned()),
                Datum::Bool(true),
            ]]),
            // `amtype` `i` — an index method, which is what both of these are. A real server's
            // `pg_am` also holds table methods (`amtype` `t`, `heap`); this node has one storage
            // engine and no `USING` on a table, so there is nothing to name.
            CatalogView::PgAm => Ok(vec![
                vec![
                    Datum::Int8(BTREE_AM_OID),
                    Datum::Text("btree".to_owned()),
                    Datum::Text("i".to_owned()),
                ],
                vec![
                    Datum::Int8(GIST_AM_OID),
                    Datum::Text("gist".to_owned()),
                    Datum::Text("i".to_owned()),
                ],
            ]),
            // **One row per schema**, `public` included — and `public` is not a record: it is a
            // property of the build, the way the available extensions are, so a tenant that has
            // created nothing still reports it.
            // **One database, and its name is `current_database()`'s.** The two are one constant so
            // that `WHERE datname = current_database()` matches without either half knowing about
            // the other.
            CatalogView::PgDatabase => Ok(vec![vec![
                Datum::Int8(DATABASE_OID),
                Datum::Text(crate::parse::DATABASE_NAME.to_owned()),
                // 6 is `UTF8` in PostgreSQL's own encoding table, and it is the only encoding this
                // node speaks — the startup packet says so too (`client_encoding`).
                Datum::Int4(6),
                // **`C`, not the oracle's `en_US.utf8`.** A collation is a feature this node does
                // not have (`CatalogView::PgCollation` is empty for the same reason), so the honest
                // locale is the one that sorts by byte value.
                Datum::Text("C".to_owned()),
                Datum::Text("C".to_owned()),
            ]]),
            CatalogView::PgNamespace => Ok(super::schema_names(txn, tenant)?
                .into_iter()
                .map(|(name, id)| {
                    vec![
                        Datum::Int8(super::pg_relations::as_oid(id)),
                        Datum::Text(name),
                    ]
                })
                .collect()),
            // **One fact read two ways.** Which extensions are installed is stored, and both of
            // these views report it — `pg_extension` as a row per installed extension and
            // `pg_available_extensions` as a non-NULL `installed_version` beside every available
            // one. They disagreed before `CREATE EXTENSION` existed, because one was a constant
            // that said `plpgsql` was installed and the other held nothing at all.
            CatalogView::PgExtension => Ok(installed(txn, tenant)?
                .into_iter()
                .map(|(name, version)| {
                    vec![
                        Datum::Text(name),
                        Datum::Int8(PUBLIC_NAMESPACE_OID),
                        Datum::Text(version),
                    ]
                })
                .collect()),
            CatalogView::PgAvailableExtensions => {
                let installed = installed(txn, tenant)?;
                Ok(AVAILABLE_EXTENSIONS
                    .iter()
                    .map(|(name, default_version)| {
                        let version = installed
                            .iter()
                            .find(|(installed, _)| installed == name)
                            .map(|(_, version)| Datum::Text(version.clone()));
                        vec![
                            Datum::Text((*name).to_owned()),
                            Datum::Text((*default_version).to_owned()),
                            version.unwrap_or(Datum::Null),
                            Datum::Null,
                            Datum::Null,
                        ]
                    })
                    .collect())
            }
            constant => Ok(constant.rows()),
        }
    }

    #[must_use]
    fn rows(self) -> Vec<Vec<Datum>> {
        match self {
            // **The available set is a property of the build**, and which of them is installed is
            // state — so the rows are constants and their `installed_version` is not. `plpgsql`
            // is installed from the start, as it is on every PostgreSQL database; `hstore` is
            // available and not installed until a `CREATE EXTENSION` says otherwise. A name that
            // is neither matches no row at all, which is `ActiveRecord`'s third case
            // (`extension_available?` is `nil`) and the one a `WHERE` gives for free.
            //
            // `location` and `comment` are NULL: a real server fills them from the control file it
            // found on disk, and there is none here. Neither is read by the two methods this view
            // exists for.
            // `PgRange` and `PgCollation` have none, and the catalog-backed views never reach
            // here — `rows_of` answers for those before it delegates, the two extension views
            // included.
            CatalogView::PgAvailableExtensions
            | CatalogView::PgRange
            | CatalogView::PgCollation
            | CatalogView::PgExtension
            | CatalogView::PgInherits
            | CatalogView::PgAm
            | CatalogView::PgProc
            | CatalogView::PgTrigger
            | CatalogView::PgLanguage
            | CatalogView::PgPartitionedTable
            | CatalogView::PgIndexes
            | CatalogView::PgEnum
            | CatalogView::PgClass
            | CatalogView::PgNamespace
            | CatalogView::PgAttribute
            | CatalogView::PgType
            | CatalogView::PgDatabase
            | CatalogView::PgDepend
            | CatalogView::PgSequence
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
                        // Synthetic and never stored, so its persistence is the default.
                        persistence: crate::catalog::Persistence::Permanent,
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
                                default_expr: None,
                                not_null: false,
                                default: None,
                                missing: None,
                                generated: None,
                                comment: None,
                            })
                            .collect(),
                        primary_key: Vec::new(),
                        indexes: Vec::new(),
                        primary_key_name: String::new(),
                        schema_version: 1,
                        sequences: Vec::new(),
                        checks: Vec::new(),
                        foreign_keys: Vec::new(),
                        triggers_disabled: false,
                        parents: Vec::new(),
                        children: Vec::new(),
                        triggers: Vec::new(),
                        excludes: Vec::new(),
                        child_scans: Vec::new(),
                        partition_by: None,
                        partition_bound: None,
                        comment: None,
                        primary_key_comment: None,
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
/// The oid `pg_language` reports for `plpgsql`, and what a `pg_proc.prolang` would point at.
const PLPGSQL_LANGUAGE_OID: i64 = 14_024;

const PUBLIC_NAMESPACE_OID: i64 = 11;

/// The one database's oid. PostgreSQL's `postgres` database is 5 on a fresh cluster; this is a
/// number of ours, because the database here is not that one and pretending otherwise would be a
/// value nobody measured.
const DATABASE_OID: i64 = 16_384;

/// The oid of a schema by name, for `relnamespace` and its kin.
///
/// `public`'s is fixed the way a real server fixes it; every other schema's is the id its record
/// was allocated. A name with no schema — nothing can produce one — falls back to `public`, which
/// is the answer that cannot mislead.
fn namespace_oid(schemas: &[(String, u64)], schema: &str) -> i64 {
    schemas
        .iter()
        .find(|(name, _)| name == schema)
        .map_or(PUBLIC_NAMESPACE_OID, |(_, id)| {
            super::pg_relations::as_oid(*id)
        })
}

/// The extensions this build offers, with the version each installs at.
///
/// A **property of the build, not of the tenant**: it says what a `CREATE EXTENSION` can succeed
/// at, and every other name is `0A000 … is not available` with PostgreSQL's own HINT.
///
/// **Exactly the ones `postgresql_specific_schema.rb` needs in order to load**, and no more. The
/// versions are the oracle's own (`pgcrypto` 1.4, `uuid-ossp` 1.1), and `plpgsql` is
/// [`PRE_INSTALLED`] rather than merely available, which is what every PostgreSQL database
/// reports for it.
///
/// **Installing one does not bring the functions it carries.** `uuid_generate_v4()` and
/// `gen_random_uuid()` are still `42883` naming themselves — the statement's job is to let the
/// schema load, and a function this node does not have is a gap of its own with its own capture.
/// The list is deliberately short for the reason `pg_type` is: an entry here tells a client this
/// server has something, so a type-bearing extension does not go on it until the type does.
const AVAILABLE_EXTENSIONS: [(&str, &str); 3] = [
    ("pgcrypto", "1.4"),
    ("plpgsql", "1.0"),
    ("uuid-ossp", "1.1"),
];

/// The extension every database has installed before anything runs.
const PRE_INSTALLED: (&str, &str) = ("plpgsql", "1.0");

/// Every installed extension, in name order: the one that is always there, then whatever a
/// `CREATE EXTENSION` recorded.
fn installed(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<(String, String)>> {
    let mut rows = vec![(PRE_INSTALLED.0.to_owned(), PRE_INSTALLED.1.to_owned())];
    for (name, version) in super::installed_extensions(txn, tenant)? {
        if name != PRE_INSTALLED.0 {
            rows.push((name, version));
        }
    }
    rows.sort();
    Ok(rows)
}

/// Whether this tenant has already installed an extension.
pub fn is_installed(txn: &dyn crate::backend::Txn, tenant: u64, name: &str) -> Result<bool> {
    Ok(installed(txn, tenant)?
        .iter()
        .any(|(installed, _)| installed == name))
}

/// Whether this build has an extension, and the version it would install at.
#[must_use]
pub fn available_extension(name: &str) -> Option<&'static str> {
    AVAILABLE_EXTENSIONS
        .iter()
        .find(|(available, _)| *available == name)
        .map(|(_, version)| *version)
}

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
/// One row per stored function.
///
/// **Only what `CREATE FUNCTION` wrote.** A real server's `pg_proc` also holds every built-in, and
/// this one does not — a declared divergence, and the reason `DROP FUNCTION`'s protected list is a
/// table in the executor rather than a query over this view.
fn proc_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    Ok(super::functions(txn, tenant)?
        .into_iter()
        .map(|function| {
            vec![
                Datum::Int8(super::pg_relations::as_oid(function.id)),
                Datum::Text(function.name),
                Datum::Int8(PUBLIC_NAMESPACE_OID),
                // `f` for an ordinary function; `p` is a procedure and `a` an aggregate.
                Datum::Text("f".to_owned()),
                Datum::Int2(0),
                // `v` for volatile, which is the default and what a trigger function is.
                Datum::Text("v".to_owned()),
                Datum::Text(function.body),
                Datum::Text(function.language),
            ]
        })
        .collect())
}

/// One row per registered trigger.
fn trigger_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let functions = super::functions(txn, tenant)?;
    let mut rows = Vec::new();
    for table in relations.tables() {
        for (at, trigger) in table.triggers.iter().enumerate() {
            let foid = functions
                .iter()
                .find(|function| function.name == trigger.function)
                .map_or(0, |function| super::pg_relations::as_oid(function.id));
            rows.push(vec![
                Datum::Int8(trigger_oid(table.id, at)),
                Datum::Int8(super::pg_relations::as_oid(table.id)),
                Datum::Text(trigger.name.clone()),
                Datum::Text(trigger.tgenabled().to_owned()),
                Datum::Int2(trigger.tgtype()),
                Datum::Int2(0),
                // **Never internal.** A foreign key's own triggers are, and this node has none of
                // those as rows — `NOT tgisinternal` is how a client filters them out and it must
                // not hide a user's trigger.
                Datum::Bool(false),
                Datum::Int8(foid),
            ]);
        }
    }
    Ok(rows)
}

/// A trigger's oid: its table and its position, the way a `CHECK`'s is built.
pub(super) fn trigger_oid(table_id: u64, at: usize) -> i64 {
    // A table cannot hold more triggers than a `Vec` can index, and the multiply keeps each
    // table's block of a thousand to itself.
    let at = i64::try_from(at).unwrap_or(i64::MAX);
    super::pg_relations::as_oid(table_id)
        .wrapping_mul(1_000)
        .wrapping_add(at.wrapping_add(1))
}

/// One row per index, the way `pg_indexes` presents them.
///
/// **A view, not a table**: a real server defines it over `pg_class` and `pg_index`, and its
/// `indexdef` *is* `pg_get_indexdef(indexrelid)` — so the string here is the one that function
/// builds rather than a second rendering that could drift from it. A primary key is in it, which
/// is why `companies_pkey` is one of the capture's two rows.
fn indexes_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for relation in relations.rows() {
        if !matches!(
            relation.kind,
            super::pg_relations::RelKind::Index | super::pg_relations::RelKind::PrimaryKey
        ) {
            continue;
        }
        let Some(table) = relations.table(relation) else {
            continue;
        };
        let Datum::Text(definition) =
            super::pg_index::index_definition(&relations, Some(relation.oid), None)
        else {
            continue;
        };
        rows.push(vec![
            Datum::Text(PUBLIC_SCHEMA.to_owned()),
            Datum::Text(table.name.clone()),
            Datum::Text(relation.name.clone()),
            // No tablespaces here, and NULL is what a real server answers for an index in the
            // default one — so the column is the same answer rather than a stub.
            Datum::Null,
            Datum::Text(definition),
        ]);
    }
    Ok(rows)
}

/// One row per partitioned table, the way `pg_partitioned_table` holds them.
fn partitioned_table_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for table in relations.tables() {
        let Some(key) = &table.partition_by else {
            continue;
        };
        let attnums: Vec<usize> = key.columns.clone();
        rows.push(vec![
            Datum::Int8(super::pg_relations::as_oid(table.id)),
            Datum::Text(key.strategy.code().to_owned()),
            Datum::Int2(i16::try_from(key.columns.len()).unwrap_or(i16::MAX)),
            // **The bare `int2vector` form**, `1`, not the brace form `{1}` — the same rendering
            // `pg_index.indkey` uses, and what a real server prints here. Measured.
            Datum::Text(
                attnums
                    .iter()
                    .map(|&at| super::pg_relations::attnum_of(table, at).to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        ]);
    }
    Ok(rows)
}

/// One row per inheritance edge, the way `pg_inherits` holds them.
///
/// Read from the **child** side, because that is where the order lives: `inhseqno` numbers a
/// child's parents from 1 in the order its `INHERITS` clause wrote them, and the parent's own list
/// has no such order to offer.
fn inherits_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for table in relations.tables() {
        for (at, &parent) in table.parents.iter().enumerate() {
            rows.push(vec![
                Datum::Int8(super::pg_relations::as_oid(table.id)),
                Datum::Int8(super::pg_relations::as_oid(parent)),
                Datum::Int4(i32::try_from(at + 1).unwrap_or(i32::MAX)),
            ]);
        }
    }
    Ok(rows)
}
/// One row per sequence: the sequence depends on the column it fills.
///
/// **`deptype` is `a`**, "automatic": the sequence goes away with the column, which is what
/// `bigserial` makes and what `ActiveRecord` looks for. A sequence that fills nothing — every
/// `CREATE SEQUENCE` makes one — has no dependency and no row here, exactly as on a real server.
///
/// `attnum` is the column's **one-based** position, which is what `pg_attribute` reports and what
/// `cons.conkey[1]` is compared against.
fn pg_depend_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let class_oid = i64::try_from(CatalogView::PgClass.table_def().id).unwrap_or(i64::MAX);
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for table in relations.tables() {
        for sequence in &table.sequences {
            let Some(column) = sequence.column else {
                continue;
            };
            let Ok(attnum) = i32::try_from(column + 1) else {
                continue;
            };
            rows.push(vec![
                Datum::Int8(class_oid),
                Datum::Int8(super::pg_relations::as_oid(sequence.id)),
                Datum::Int4(0),
                Datum::Int8(class_oid),
                Datum::Int8(super::pg_relations::as_oid(table.id)),
                Datum::Int4(attnum),
                Datum::Text("a".to_owned()),
            ]);
        }
    }
    rows.sort_by_key(|row| match row.get(1) {
        Some(Datum::Int8(oid)) => *oid,
        _ => 0,
    });
    Ok(rows)
}

/// One row per sequence: its parameters, which are the same for every sequence this node makes.
///
/// **`last_value` is not here.** It is state, it lives in the sequence relation, and the two reads
/// are what `reset_pk_sequence!` uses in its two branches: `seqmin` when the table is empty and
/// `MAX(pk)` when it is not.
fn pg_sequence_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for table in relations.tables() {
        for sequence in &table.sequences {
            rows.push(vec![
                Datum::Int8(super::pg_relations::as_oid(sequence.id)),
                // `bigint`, which is what every sequence this node makes counts in.
                Datum::Int8(i64::from(ColumnType::Int8.oid())),
                Datum::Int8(1),
                Datum::Int8(1),
                Datum::Int8(i64::MAX),
                Datum::Int8(1),
                Datum::Int8(1),
                Datum::Bool(false),
            ]);
        }
    }
    rows.sort_by_key(|row| match row.first() {
        Some(Datum::Int8(oid)) => *oid,
        _ => 0,
    });
    Ok(rows)
}

/// Every `pg_type` row: the built-in types, then this tenant's own.
///
/// The built-ins are derived from `ColumnType::ALL` rather than written out, so a type cannot be
/// added to this node and left out of its own `pg_type`. The user types are read from the catalog
/// for the same reason `pg_class` is a view over the name records: there is no second copy to keep
/// in step.
fn pg_type_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let mut rows: Vec<Vec<Datum>> = ColumnType::ALL
        .iter()
        .map(|ty| {
            vec![
                Datum::Int8(i64::from(ty.oid())),
                Datum::Text(typname(*ty).to_owned()),
                // **`typelem` is the element type's OID**, and zero for everything
                // that is not an array — which is how a client reads what an array is
                // over. `typtype` stays `b` for an array too: `b` is "base", as
                // against `r`ange, `e`num, `d`omain and `c`omposite, and an array is
                // none of those.
                Datum::Int8(
                    ArrayValue::element_of(*ty).map_or(0, |element| i64::from(element.oid())),
                ),
                Datum::Text(",".to_owned()),
                Datum::Text(typinput(*ty).to_owned()),
                Datum::Text("b".to_owned()),
                Datum::Int8(0),
                // No collation on any type here, which is what makes
                // `a.attcollation <> t.typcollation` false for every column — the
                // same answer a real server gives, by the same comparison.
                Datum::Int8(0),
                // The one namespace this node has, the same one every relation
                // reports.
                Datum::Int8(PUBLIC_NAMESPACE_OID),
                Datum::Int2(ty.type_len()),
                Datum::Text(typcategory(*ty).to_owned()),
                // Derived from the same table `'x[]'::regtype` reads, so the two can
                // never disagree about which array a type is paired with.
                Datum::Int8(i64::from(crate::value::array_oid(*ty))),
                // No built-in type owns a `pg_class` row: only a composite does.
                Datum::Int8(0),
            ]
        })
        .collect();
    rows.extend(user_type_rows(txn, tenant)?);
    rows.sort_by_key(|row| match row.first() {
        Some(Datum::Int8(oid)) => *oid,
        _ => 0,
    });
    Ok(rows)
}

/// One row per user-defined type, and **one more for the array type `CREATE TYPE` made with it**.
///
/// The array is not asked for by any statement and exists all the same — `typarray` of `floatrange`
/// is `floatrange[]`, measured — so it is derived here rather than stored: its oid is the type's
/// plus one, taken from the same sequence at creation.
fn user_type_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    for def in super::user_types(txn, tenant)? {
        let oid = super::pg_relations::as_oid(def.oid);
        rows.push(vec![
            Datum::Int8(oid),
            Datum::Text(def.name.clone()),
            // Not an array, so no element type — the array row below is the one with one.
            Datum::Int8(0),
            Datum::Text(",".to_owned()),
            Datum::Text(format!("{}_in", def.name)),
            Datum::Text(def.kind.typtype().to_owned()),
            Datum::Int8(0),
            Datum::Int8(0),
            Datum::Int8(PUBLIC_NAMESPACE_OID),
            Datum::Int2(-1),
            Datum::Text(def.kind.typcategory().to_owned()),
            Datum::Int8(oid + 1),
            // **A composite owns a `pg_class` row and the other two do not.** It is how its
            // fields are stored on a real server, and the one column that tells the three kinds
            // apart beyond their letters.
            Datum::Int8(match def.kind {
                super::TypeKind::Composite { .. } => oid + 2,
                _ => 0,
            }),
        ]);
        rows.push(vec![
            Datum::Int8(oid + 1),
            Datum::Text(format!("_{}", def.name)),
            Datum::Int8(oid),
            Datum::Text(",".to_owned()),
            Datum::Text("array_in".to_owned()),
            Datum::Text("b".to_owned()),
            Datum::Int8(0),
            Datum::Int8(0),
            Datum::Int8(PUBLIC_NAMESPACE_OID),
            Datum::Int2(-1),
            Datum::Text("A".to_owned()),
            Datum::Int8(0),
            Datum::Int8(0),
        ]);
    }
    Ok(rows)
}

fn pg_class_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    // **`relhastriggers` is `t` for either side of a foreign key**, because a foreign key *is* two
    // internal triggers — one on the child and one on the parent. Measured: a table with no
    // constraint at all is `f`, the child is `t` and the parent is `t`. So the referenced side has
    // to be collected, and it is collected here in one pass over every table rather than by a
    // back-reference scan per row, since `Relations` already holds them all.
    let referenced: std::collections::BTreeSet<u64> = relations
        .tables()
        .flat_map(|table| table.foreign_keys.iter().map(|key| key.parent))
        .collect();
    // By name, which is the order the scan already returns them in and the order a reader can
    // predict. PostgreSQL promises no order without an `ORDER BY`; a deterministic one is a
    // superset of that promise, as `pg_type`'s rows are.
    // **The catalog describes itself.** A real server's `pg_class` has a row for every catalog
    // relation, and `ActiveRecord` asks for one by name — `pg_available_extensions` — before it
    // loads a single fixture. Derived from `CatalogView::ALL` rather than written out, so a view
    // added to this node cannot be missing from the catalog that is supposed to list it.
    //
    // `relkind` is `v`: they are views on a real server, and the table lists a client asks for
    // filter `relkind IN ('r', 'p')`, so these rows are invisible to them exactly as they should
    // be. The name is the unqualified one, which is what `relname` holds — the schema is a
    // separate column there and `information_schema.tables` is `tables` in `pg_class`.
    let views = CatalogView::ALL.into_iter().map(|view| {
        vec![
            Datum::Int8(i64::try_from(view.id()).unwrap_or(i64::MAX)),
            Datum::Text(
                view.name()
                    .rsplit_once('.')
                    .map_or_else(|| view.name().to_owned(), |(_, name)| name.to_owned()),
            ),
            Datum::Int8(PUBLIC_NAMESPACE_OID),
            Datum::Text("v".to_owned()),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Null,
            Datum::Int8(0),
            // A catalog view is not stored at all, and a real server reports `p` for one.
            Datum::Text(super::Persistence::Permanent.relpersistence().to_owned()),
        ]
    });
    let schemas = super::schema_names(txn, tenant)?;
    Ok(relations
        .rows()
        .map(|relation| {
            vec![
                Datum::Int8(relation.oid),
                Datum::Text(relation.name.clone()),
                // **The schema the relation is in**, not a constant: `relnamespace` is what joins
                // `pg_class` to `pg_namespace`, and two tables of one name in two schemas are told
                // apart by exactly this column.
                Datum::Int8(namespace_oid(&schemas, &relation.schema)),
                // **Four `relkind`s in one feature, and one is a capital letter.** A partitioned
                // table is `p` where an ordinary one is `r`, and an index *on* a partitioned table
                // is `I` where an ordinary one is `i` — so the letter is not a function of the
                // relation's kind alone, which is why it is decided here with the table in hand.
                // A `relkind IN ('r','p')` filter that forgets `I` still passes every test that
                // never makes a partitioned index.
                Datum::Text(partitioned_relkind(&relations, relation)),
                // Only a **table** has them: an index over a table with a foreign key is `f` on a
                // real server, and the index's row here names that table.
                Datum::Bool(
                    relation.kind == super::pg_relations::RelKind::Table
                        && (referenced.contains(&relation.table_id)
                            || relations
                                .table(relation)
                                .is_some_and(|table| !table.foreign_keys.is_empty())),
                ),
                // `relispartition`: the relation is a partition. An index on one is a partition
                // too on a real server; here the flag follows the table it is on.
                Datum::Bool(
                    relations
                        .table(relation)
                        .is_some_and(|table| table.partition_bound.is_some()),
                ),
                // `relhassubclass`: something inherits from or partitions it. **`f` for a freshly
                // created partitioned table with no partitions yet** — measured, the capture's
                // first `pg_class` row has it false.
                Datum::Bool(
                    relation.kind == super::pg_relations::RelKind::Table
                        && relations
                            .table(relation)
                            .is_some_and(|table| !table.children.is_empty()),
                ),
                // `relpartbound`. NULL for everything that is not a partition, which is what a
                // real server holds there too.
                relations
                    .table(relation)
                    .filter(|_| relation.kind == super::pg_relations::RelKind::Table)
                    .and_then(|table| table.partition_bound.as_ref())
                    .map_or(Datum::Null, |bound| {
                        Datum::Text(super::partition_bound_definition(bound))
                    }),
                Datum::Int8(access_method_oid(relation.kind)),
                // **The owning table's, for every relation it owns.** A `bigserial`'s sequence
                // and every index — the primary key's included — report the table's persistence
                // on a real server, and `ALTER TABLE … SET LOGGED` moves all of them in one
                // statement. Reading it from the table rather than storing a copy per relation is
                // what makes both true at once and leaves nothing that can drift.
                Datum::Text(
                    relations
                        .table(relation)
                        .map_or(super::Persistence::Permanent, |table| table.persistence)
                        .relpersistence()
                        .to_owned(),
                ),
            ]
        })
        .chain(views)
        .collect())
}

/// PostgreSQL's own oid for the `btree` access method, which is a fixed catalog id there.
const BTREE_AM_OID: i64 = 403;

/// PostgreSQL's own oid for `gist`, likewise fixed.
const GIST_AM_OID: i64 = 783;

/// `pg_class.relam`: the access method an index is built with, and **zero** for anything that is
/// not an index — which is what a real server reports for a table, so a join to `pg_am` drops it.
fn access_method_oid(kind: super::pg_relations::RelKind) -> i64 {
    use super::pg_relations::RelKind;
    match kind {
        RelKind::Index | RelKind::PrimaryKey => BTREE_AM_OID,
        RelKind::Exclusion => GIST_AM_OID,
        RelKind::Table | RelKind::Sequence => 0,
    }
}

/// The `relkind` letter, which needs the **table** and not only the relation's kind.
///
/// A partitioned table is `p`, an index on one is `I`, and everything else is what
/// `RelKind::relkind` says.
fn partitioned_relkind(
    relations: &super::pg_relations::Relations,
    relation: &super::pg_relations::RelationRow,
) -> String {
    use super::pg_relations::RelKind;
    let partitioned = relations
        .table(relation)
        .is_some_and(|table| table.partition_by.is_some());
    match relation.kind {
        RelKind::Table if partitioned => "p".to_owned(),
        RelKind::Index if partitioned => "I".to_owned(),
        kind => kind.relkind().to_owned(),
    }
}

/// `pg_type.typname`: the internal name, which is not the one this node complains with — a column
/// is declared `int8` and named `bigint` in an error. Measured against 19beta1, all six.
pub(crate) fn typname(ty: ColumnType) -> &'static str {
    match ty {
        // **An array type's internal name is the element's with a leading underscore** — `_int4`,
        // not `int4[]`. That spelling is what `pg_type.typname` holds on a real server and what a
        // client matching on it expects.
        ColumnType::Int8Array => "_int8",
        ColumnType::Int4Array => "_int4",
        ColumnType::NumericArray => "_numeric",
        ColumnType::TextArray => "_text",
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
        ColumnType::Numeric => "numeric",
        ColumnType::Time => "time",
        ColumnType::Uuid => "uuid",
        ColumnType::Interval => "interval",
        ColumnType::Oid => "oid",
    }
}

/// `pg_type.typcategory`: PostgreSQL's coarse grouping of types, one character each.
///
/// Measured on 19beta1 for all eighteen rather than reasoned about, because the groupings are not
/// what a reader would guess: `bytea` is `U` (user-defined) and not `S` (string), a `uuid` is `U`
/// too, and all three datetime types are `D` while an `interval` is `T`. An exhaustive match, so
/// a type added here has to answer instead of inheriting somebody else's letter.
fn typcategory(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Int8
        | ColumnType::Int4
        | ColumnType::Int2
        | ColumnType::Double
        | ColumnType::Real
        | ColumnType::Numeric
        // A number, and PostgreSQL groups it with them despite being an identifier.
        | ColumnType::Oid => "N",
        ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => "S",
        ColumnType::Bool => "B",
        ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Date | ColumnType::Time => {
            "D"
        }
        ColumnType::Bytea | ColumnType::Json | ColumnType::Jsonb | ColumnType::Uuid => "U",
        // `T` for timespan, which is its own category and not the datetimes' `D`.
        ColumnType::Interval => "T",
        // `A` for array, whatever the elements are — the category is the constructor's, not the
        // element type's.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::NumericArray
        | ColumnType::TextArray => "A",
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
        // **`array_in` for every array type**, and this one value is load-bearing beyond the
        // catalog: `ActiveRecord` decides that a column is an array by comparing this string, and
        // a column it does not know to be an array is what makes it hand a Ruby `Array` to
        // `quote` and fail client-side with `can't quote Array`.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::NumericArray
        | ColumnType::TextArray => "array_in",
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
        ColumnType::Numeric => "numeric_in",
        ColumnType::Time => "time_in",
        ColumnType::Uuid => "uuid_in",
        ColumnType::Interval => "interval_in",
        ColumnType::Oid => "oidin",
    }
}
