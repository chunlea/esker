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
//! **`pg_range` has a row per range type and no `oid` column.** It was empty while this node had
//! no range types; it cannot be now, because `ActiveRecord`'s boot query is
//! `pg_type LEFT JOIN pg_range ON oid = rngtypid` and a range type whose `rngsubtype` comes back
//! NULL is one it does not register — a `floatrange` column would then hand a client the raw text.
//! The missing `oid` column is not an omission but the capture: `SELECT oid FROM pg_range` is
//! `42703` on a real server, and that is what makes that unqualified `oid` legal. A `pg_range`
//! given an `oid` would make the same statement `42702`.
//!
//! # Read-only, and the refusal is `42501`
//!
//! Measured rather than assumed: `DROP TABLE pg_type`, `ALTER TABLE pg_type ADD COLUMN` and
//! `CREATE INDEX … ON pg_type` all answer `42501 permission denied: "pg_type" is a system
//! catalog`. So does every write here — including the DML a *superuser* is allowed to attempt on a
//! real server, which is the divergence `tests/pg_catalog.rs` declares: there are no roles here,
//! and a computed relation has nothing to write to.

use super::NO_LENGTH;
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
    /// `information_schema.views`: one row per view, with the standard's `is_updatable`.
    ///
    /// **`YES`/`NO` text, not a boolean** — the standard spells these `character varying(3)`, and
    /// a client comparing against the string would read a boolean as neither.
    InformationSchemaViews,
    /// `information_schema.domains`: one row per **domain**, describing its base type the way the
    /// standard describes a column's
    /// ([ADR 0065](../../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)).
    InformationSchemaDomains,
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
    /// The index access methods, which is **three**: `btree`, `gin` and `gist`.
    ///
    /// A real server has six. These three are the ones this node's own `pg_class.relam` can point
    /// at — a `CREATE INDEX` may be declared `USING gin` or `USING gist` and is *recorded* as
    /// such ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)),
    /// and an `EXCLUDE` constraint records `gist` — and a row for a method nothing can be built
    /// with would be a claim rather than a report. Declared: `hash`, `spgist` and `brin` are on a
    /// real server and not here, and `USING` one of them is still refused.
    PgAm,
    /// The operator classes a `CREATE INDEX` may name, which is **four**.
    ///
    /// The same rule `PgAm` states: a class nothing can be declared with would be a claim rather
    /// than a report. These four are the ones `crate::catalog::OPERATOR_CLASSES` accepts —
    /// `gin_trgm_ops`, `gist_trgm_ops`, `text_pattern_ops` and `varchar_pattern_ops` — and each
    /// is *recorded* on the index that names it while the index underneath stays the ordered one
    /// ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)).
    /// A real server has hundreds, and the `hash` halves of the two `_pattern_ops` are among the
    /// ones missing here because `USING hash` is refused.
    PgOpclass,
    /// `pg_cast`, which a client reads to find out whether one type reaches another.
    ///
    /// 100 rows: every cast PostgreSQL 19 has whose **source and target are both types this node
    /// has**. `ActiveRecord` joins it to `pg_proc` to decide whether `LOWER()` applies to a
    /// column's type, and that join is the only reason `character varying` answers `true` — there
    /// is no `lower(varchar)`, only a cast to the `text` the one overload takes.
    PgCast,
    /// The text-search configurations this node has, which is **not** the thirty-two a real
    /// server's `initdb` creates.
    ///
    /// The same rule `PgAm` above states: a row for a configuration nothing can be tokenised with
    /// would be a claim rather than a report. `simple` and `english` are the two this node
    /// implements and the two `captures/pg19_tsvector.txt` exercises; the other thirty are a
    /// declared divergence in `tests/tsvector.rs` rather than thirty rows that answer `0A000` the
    /// moment anyone uses them.
    PgTsConfig,
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
    /// `pg_views`: one row per view, with the `SELECT` it stands for.
    PgViews,
    /// `pg_matviews`: one row per **materialized** view.
    ///
    /// A separate view from `pg_views` on a real server, and the split is the point: a
    /// materialized view is in this one and in neither `pg_views` nor
    /// `information_schema.tables` — measured, all three (ADR 0064).
    PgMatviews,
    /// The sessions this server is running, which is **the asking one and no other**.
    ///
    /// `migration_test.rb:1108` reads it to ask whether the connection that held an advisory lock
    /// has gone away, and the whole view was `42P01` here until now. A real server's is
    /// cluster-wide — one row per backend, in every database — and this node has no registry of
    /// live sessions to build that from (the debt [ADR
    /// 0052](../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md)
    /// names for `DROP DATABASE` is the same missing thing), so it reports the backend that is
    /// asking. That row is **true**: it is `active`, because it is running the query that reads
    /// the view, and its `datname` is the database it is serving.
    PgStatActivity,
    /// `pg_roles`, the readable view of the cluster's roles.
    PgRoles,
    /// `pg_authid`, the same rows with the columns `pg_roles` hides behind NULL.
    PgAuthid,
    /// **Who holds a row lock on this node, and who is waiting for one** — the view a stuck
    /// session's counterpart can be found in.
    ///
    /// PostgreSQL's `pg_locks` has sixteen columns and this one has the same sixteen, because the
    /// tools that read it — `ActiveRecord`'s lock tests, and the Rails harness's own forensics —
    /// select named columns rather than `*`. What differs is which of them can be true here, and
    /// each is answered `NULL` rather than invented: this node has no pages, no tuple numbers and
    /// no virtual transactions, so `page`, `tuple` and `virtualxid` are `NULL` where a real server
    /// has numbers.
    ///
    /// Measured against PostgreSQL 19 with one session holding `SELECT … FOR UPDATE` and a second
    /// blocked behind it: the holder shows a `tuple` row with `granted = true`, and **the waiter
    /// shows a `transactionid` row with `granted = false`** naming the transaction it waits for.
    /// That pair is the whole diagnostic, and it is the shape reproduced here.
    PgLocks,
    /// **What this session has prepared** — SQL `PREPARE`s and the protocol's named statements,
    /// which are one store on a real server and are one store here.
    ///
    /// Eight columns, measured from `\d pg_prepared_statements` on PostgreSQL 19. Two of them are
    /// answered the way every other view here answers a thing this node does not keep:
    /// `prepare_time` is NULL, because no wall clock is read for anything a client can order by
    /// (the same reason `pg_stat_activity.backend_start` and `pg_locks.waitstart` are NULL), and
    /// `generic_plans`/`custom_plans` are `0`, which is not a placeholder — there is no plan cache
    /// here, so no generic plan has ever been used, and `0` is what a real server reports for a
    /// statement that has not been run either.
    ///
    /// **The rows come from the session, not the store**, which is why this is the first view
    /// whose input is neither the transaction nor the tenant: the protocol hands its statements
    /// down before each one runs (`crate::pgwire::session::Execute::remember_prepared`).
    PgPreparedStatements,
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
    /// One row per label of every enum type this tenant has declared.
    ///
    /// A **view over the type records**, the way `pg_class` is a view over the name records: the
    /// labels live on the `TypeDef` that `CREATE TYPE` wrote, in declaration order, and this
    /// projects them. There is no second copy and no way for the two to disagree — which matters
    /// more here than elsewhere, because that order *is* the sort order of the type
    /// (`enumsortorder`, and ADR 0050's never-reuse rule).
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
    pub const ALL: [CatalogView; 39] = [
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
        CatalogView::PgOpclass,
        CatalogView::PgCast,
        CatalogView::PgTsConfig,
        CatalogView::PgProc,
        CatalogView::PgTrigger,
        CatalogView::PgLanguage,
        CatalogView::PgPartitionedTable,
        CatalogView::PgIndexes,
        CatalogView::PgViews,
        CatalogView::PgMatviews,
        CatalogView::PgStatActivity,
        CatalogView::PgRoles,
        CatalogView::PgAuthid,
        CatalogView::PgLocks,
        CatalogView::PgPreparedStatements,
        CatalogView::PgDatabase,
        CatalogView::PgDepend,
        CatalogView::PgSequence,
        CatalogView::PgEnum,
        CatalogView::PgAvailableExtensions,
        CatalogView::InformationSchemaTables,
        CatalogView::InformationSchemaViews,
        CatalogView::InformationSchemaDomains,
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
            CatalogView::PgOpclass => "pg_opclass",
            CatalogView::PgCast => "pg_cast",
            CatalogView::PgTsConfig => "pg_ts_config",
            CatalogView::PgProc => "pg_proc",
            CatalogView::PgTrigger => "pg_trigger",
            CatalogView::PgLanguage => "pg_language",
            CatalogView::PgPartitionedTable => "pg_partitioned_table",
            CatalogView::PgIndexes => "pg_indexes",
            CatalogView::PgViews => "pg_views",
            CatalogView::PgMatviews => "pg_matviews",
            CatalogView::PgStatActivity => "pg_stat_activity",
            CatalogView::PgRoles => "pg_roles",
            CatalogView::PgAuthid => "pg_authid",
            CatalogView::PgLocks => "pg_locks",
            CatalogView::PgPreparedStatements => "pg_prepared_statements",
            CatalogView::PgDatabase => "pg_database",
            CatalogView::PgDepend => "pg_depend",
            CatalogView::PgSequence => "pg_sequence",
            CatalogView::PgEnum => "pg_enum",
            CatalogView::InformationSchemaTables => "information_schema.tables",
            CatalogView::InformationSchemaViews => "information_schema.views",
            CatalogView::InformationSchemaDomains => "information_schema.domains",
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

    /// The schema it lives in: `pg_catalog`, or `information_schema` for the SQL-standard views.
    ///
    /// **This is what keeps the catalog out of a client's table list**, and it is the right thing
    /// to keep it out with. `ActiveRecord`'s `tables()` filters `n.nspname = ANY
    /// (current_schemas(false))`, which is `{public}`; `pg_catalog` is in the *implicit* path and
    /// so resolves without a qualifier while never appearing in the list. Before this, every
    /// catalog relation was reported in `public` and `relkind` `v` was doing the schema's job —
    /// which worked, and was two wrong answers at once.
    #[must_use]
    pub fn schema(self) -> &'static str {
        match self {
            CatalogView::InformationSchemaTables
            | CatalogView::InformationSchemaViews
            | CatalogView::InformationSchemaDomains
            | CatalogView::InformationSchemaColumns
            | CatalogView::InformationSchemaTableConstraints
            | CatalogView::InformationSchemaKeyColumnUsage
            | CatalogView::InformationSchemaReferentialConstraints => super::INFORMATION_SCHEMA,
            _ => super::PG_CATALOG_SCHEMA,
        }
    }

    /// The bare name, without the schema its stored one may carry.
    #[must_use]
    pub fn relname(self) -> &'static str {
        match self.name().split_once('.') {
            Some((_, name)) => name,
            None => self.name(),
        }
    }

    /// Its `pg_class.relkind`, as PostgreSQL 19 reports it — **measured, one relation at a time**.
    ///
    /// Most of `pg_catalog` is ordinary tables (`r`); the five that are views are the ones a real
    /// server defines *over* those tables. Guessing this from the name would get
    /// `pg_partitioned_table` and `pg_available_extensions` the wrong way round, so the corpus
    /// asks for all twenty-nine at once.
    #[must_use]
    pub fn relkind(self) -> &'static str {
        match self {
            CatalogView::PgIndexes
            | CatalogView::PgViews
            | CatalogView::PgStatActivity
            | CatalogView::PgLocks
            | CatalogView::PgPreparedStatements
            | CatalogView::PgAvailableExtensions
            | CatalogView::InformationSchemaTables
            | CatalogView::InformationSchemaViews
            | CatalogView::InformationSchemaDomains
            | CatalogView::InformationSchemaColumns
            | CatalogView::InformationSchemaTableConstraints
            | CatalogView::InformationSchemaKeyColumnUsage
            | CatalogView::InformationSchemaReferentialConstraints => "v",
            _ => "r",
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
                // **36, not 34.** 34 and 35 are `PgRoles`'s and `PgAuthid`'s, which
                // landed on main while this was on a branch — a reserved relation id is
                // claimed at HEAD or it is claimed twice, and two views sharing one id is
                // the failure `Relations` exists to prevent.
                CatalogView::PgOpclass => 36,
                CatalogView::PgCast => 37,
                CatalogView::PgTsConfig => 32,
                CatalogView::PgProc => 18,
                CatalogView::PgTrigger => 19,
                CatalogView::PgLanguage => 20,
                CatalogView::PgPartitionedTable => 21,
                CatalogView::PgIndexes => 22,
                // **27, because 25 is `PgSequence`'s.** These are reserved *relation ids* and two
                // views sharing one resolve to each other: giving `pg_views` 25 made
                // `'…'::regclass` over a sequence find this view instead, and eight tests that had
                // nothing to do with views went red at once.
                CatalogView::PgViews => 27,
                // 29: the next free reserved relation id — 33 is `InformationSchemaViews`'s and
                // two views sharing one resolve to each other, which is the mistake the note
                // above records.
                CatalogView::PgMatviews => 29,
                CatalogView::PgStatActivity => 28,
                // **34 and 35, claimed out loud.** The last id this lane took collided with g1's,
                // because we both read the array at our own HEAD and both saw the same next
                // number free (`no_two_views_share_a_relation_id_or_a_name` is the guard).
                CatalogView::PgRoles => 34,
                CatalogView::PgAuthid => 35,
                // **31, because 30 is `InformationSchemaDomains`'s.** This view and g1's
                // domains view were written in parallel and both took 30, which git merged
                // without a word: the arrays combined cleanly and the id collided. Two views
                // sharing one resolve to each other, and what it broke was *their* test —
                // `information_schema.domains` answered no rows — which is the third time this
                // file has recorded that failure.
                CatalogView::PgLocks => 31,
                // **38, claimed out loud at `c7a1ff57`**, which is the next free id and was free
                // on every branch when this was written (`ddl-trio`, `mpp`, `values`, `main`).
                // Whoever commits second renumbers: two views sharing an id resolve to each
                // other, which is what the notes above are scars from.
                CatalogView::PgPreparedStatements => 38,
                CatalogView::PgDatabase => 26,
                CatalogView::PgDepend => 24,
                CatalogView::PgSequence => 25,
                CatalogView::PgEnum => 16,
                CatalogView::PgAvailableExtensions => 17,
                CatalogView::InformationSchemaTables => 9,
                CatalogView::InformationSchemaViews => 33,
                // 30: free, and its own the way every reserved id here is.
                CatalogView::InformationSchemaDomains => 30,
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
    /// **The third element is the length the column declares**, PostgreSQL's `atttypmod`, and it
    /// is `catalog::NO_LENGTH` — a spelling of `value::NO_TYPMOD` — for all but one family: `information_schema`'s `yes_or_no` columns
    /// are a domain over `character varying(3)`. Measured, and the measurement is why the slot
    /// exists rather than a constant beside it — **no `pg_catalog` column has a typmod at all** on
    /// a real server (`atttypmod` is `-1` for every one of them), and of `information_schema`'s
    /// five domains only `yes_or_no` and `time_stamp`, which no view here serves, carry one. So
    /// the slot is worth six columns and the honesty of the other 269: a column list that could
    /// not say "no length" was not saying anything.
    ///
    /// The types are this node's own where it has no answer for the real one. `typname` is a
    /// `name` and `typtype`, `typcategory` and `typdelim` are `"char"` on both sides now; what is
    /// left is an `oid` answered as a `bigint` and a `regproc` as `text`, each its own unit on the
    /// type-surface queue. The *values* were always identical and only `RowDescription`'s OID
    /// differs — declared in `tests/pg_catalog.rs`.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per view, each the column list a client reads; splitting it would put \
                  half the catalog's vocabulary somewhere else"
    )]
    pub fn columns(self) -> &'static [(&'static str, ColumnType, i32)] {
        match self {
            CatalogView::PgType => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("typname", ColumnType::Name, NO_LENGTH),
                ("typelem", ColumnType::Oid, NO_LENGTH),
                ("typdelim", ColumnType::Char, NO_LENGTH),
                ("typinput", ColumnType::RegProc, NO_LENGTH),
                ("typtype", ColumnType::Char, NO_LENGTH),
                ("typbasetype", ColumnType::Oid, NO_LENGTH),
                // **Last**, because `SELECT *` expands in this order (`7be39ca`) and a column
                // added anywhere else would move every one after it. Read only by
                // `ActiveRecord`'s `columns()`, and only as `a.attcollation <> t.typcollation`
                // — see `CatalogView::PgCollation`.
                ("typcollation", ColumnType::Oid, NO_LENGTH),
                // **Last for the same reason**, and added for boot statement 26, which joins
                // `pg_type` to `pg_namespace` on it to find a schema's enum types. Every type here
                // reports the one namespace this node has, exactly as every relation's
                // `relnamespace` does; on a real server they are in `pg_catalog`, which is a
                // difference in the schema model and not in this column
                // (`PUBLIC_NAMESPACE_OID`).
                ("typnamespace", ColumnType::Oid, NO_LENGTH),
                // **Last again**, and for the third time the reason is `SELECT *`'s order. Two
                // columns three captures wanted and none of them could get: `typlen` is the
                // width in bytes, `-1` for a varlena, and `typcategory` is PostgreSQL's coarse
                // grouping. Both are derived — the width from `PgType::type_len`, the category
                // from a match a new type has to answer — so neither can drift from the type it
                // describes.
                ("typlen", ColumnType::Int2, NO_LENGTH),
                ("typcategory", ColumnType::Char, NO_LENGTH),
                // **Last for the fourth time**, and the reason has not changed. `typarray` is the
                // oid of the array type paired with this one — a user-defined type gets one made
                // for it by `CREATE TYPE`, with no statement asking — and `typrelid` is the
                // `pg_class` row a **composite** owns and every other type reports as `0`.
                ("typarray", ColumnType::Oid, NO_LENGTH),
                ("typrelid", ColumnType::Int8, NO_LENGTH),
                // **Last for the fifth time.** A **domain**'s two: whether it refuses a NULL and
                // the `DEFAULT` expression it prints back (ADR 0065). Every other type answers
                // `f` and NULL, which is what a real server reports for one — these describe a
                // constraint a domain carries and nothing else can.
                ("typnotnull", ColumnType::Bool, NO_LENGTH),
                ("typdefault", ColumnType::Text, NO_LENGTH),
            ],
            // No `oid`: see the module note. It is what keeps `ON oid = rngtypid` unambiguous.
            CatalogView::PgRange => &[
                ("rngtypid", ColumnType::Oid, NO_LENGTH),
                ("rngsubtype", ColumnType::Oid, NO_LENGTH),
            ],
            // Exactly the five `ActiveRecord` reads. `relname` and `nspname` are `name` on a real
            // server — the 64-byte identifier type — and `text` here, which compares identically.
            CatalogView::PgClass => &[
                // **Not an `oid`, and the reason is arithmetic.** A primary key's index has no
                // record of its own, so its `pg_class.oid` is
                // `pg_relations::PRIMARY_KEY_OID_BASE + table_id` — bit 62 — and four bytes cannot
                // carry it. Every other id in this crate comes from a counter that starts at
                // `catalog::FIRST_USER_ID` and fits with room to spare; these derived ones are the
                // whole of what keeps this column a `bigint`, and moving them into 32 bits is a
                // change to how the oid is *allocated* rather than to how it is declared
                // (ADR 0097). `tests/oid_type.rs::no_declared_oid_column_saturates` is the guard.
                ("oid", ColumnType::Int8, NO_LENGTH),
                ("relname", ColumnType::Name, NO_LENGTH),
                ("relnamespace", ColumnType::Oid, NO_LENGTH),
                ("relkind", ColumnType::Char, NO_LENGTH),
                ("relhastriggers", ColumnType::Bool, NO_LENGTH),
                // Added for declarative partitioning. `relispartition` says the relation *is* a
                // partition, `relhassubclass` that something inherits from or partitions it, and
                // `relpartbound` holds the bound `pg_get_expr` prints.
                ("relispartition", ColumnType::Bool, NO_LENGTH),
                ("relhassubclass", ColumnType::Bool, NO_LENGTH),
                ("relpartbound", ColumnType::Text, NO_LENGTH),
                // **Last**, because `SELECT *` expands in this order. The access method of an
                // index and **zero** for everything else, which is what a real server reports for
                // a table — the join `pg_am am ON am.oid = i.relam` then finds nothing for one,
                // which is how a client filters indexes by method.
                ("relam", ColumnType::Oid, NO_LENGTH),
                // **Last again.** A `"char"`, which it is on a real server too (ADR 0095). The
                // one column that tells an `UNLOGGED` table from an ordinary one — `information_schema.tables` calls both `BASE TABLE`, so a client
                // that reads the standard view cannot see persistence at all.
                ("relpersistence", ColumnType::Char, NO_LENGTH),
                // **Last again**, for the same reason. `t` for every relation that is not a
                // materialized view — measured, a real server reports `relispopulated = t` for an
                // ordinary table — and `f` only for one created `WITH NO DATA` and not yet
                // refreshed (ADR 0064).
                ("relispopulated", ColumnType::Bool, NO_LENGTH),
            ],
            // Exactly the three a client reads, and all three types are a real server's:
            // `amname` a `name`, `amtype` a `"char"` (ADR 0095), the `oid` still a `bigint`.
            CatalogView::PgAm => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("amname", ColumnType::Name, NO_LENGTH),
                ("amtype", ColumnType::Char, NO_LENGTH),
            ],
            // Exactly the five a client reads of it. `opcname` is a `name` on a real server and
            // `opcdefault` a boolean; the three oids are `oid` there and this node's own types
            // here, the trade every `pg_catalog` column makes.
            // **`castsource` and `casttarget` are `oid`, here as on a real server** — this pair
            // steps out of the standing `bigint` trade because they are what a `regtype` is
            // compared against: `castsource = 'character varying'::regtype` is the join
            // `ActiveRecord`'s case-insensitivity probe makes, and an `oid` beside a `regtype` is
            // one representation where a `bigint` beside one is not (ADR 0077). `castcontext` and
            // `castmethod` are `"char"` on both sides (ADR 0095).
            CatalogView::PgCast => &[
                ("castsource", ColumnType::Oid, NO_LENGTH),
                ("casttarget", ColumnType::Oid, NO_LENGTH),
                ("castcontext", ColumnType::Char, NO_LENGTH),
                ("castmethod", ColumnType::Char, NO_LENGTH),
            ],
            CatalogView::PgOpclass => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("opcname", ColumnType::Name, NO_LENGTH),
                ("opcmethod", ColumnType::Oid, NO_LENGTH),
                ("opcintype", ColumnType::Oid, NO_LENGTH),
                ("opcdefault", ColumnType::Bool, NO_LENGTH),
            ],
            // `cfgname` is a `name` on a real server, `text` here — the trade every `pg_catalog`
            // column makes. `cfgnamespace` is the oid a client joins to `pg_namespace`, which is
            // exactly what the capture's query does.
            CatalogView::PgTsConfig => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("cfgname", ColumnType::Name, NO_LENGTH),
                ("cfgnamespace", ColumnType::Oid, NO_LENGTH),
            ],
            CatalogView::PgNamespace => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("nspname", ColumnType::Name, NO_LENGTH),
            ],
            // In PostgreSQL's own order, restricted to what this node has — `SELECT *` expands in
            // that order and a client reading by position would otherwise read the wrong column.
            // `attnum` is an `int2` and `atttypmod` an `int4` on both servers, which is two fewer
            // declared-type divergences than `pg_class` has.
            CatalogView::PgAttribute => super::pg_attribute::ATTRIBUTE_COLUMNS,
            CatalogView::PgAttrdef => super::pg_attribute::ATTRDEF_COLUMNS,
            CatalogView::PgIndex => super::pg_index::INDEX_COLUMNS,
            CatalogView::PgConstraint => super::pg_constraint::CONSTRAINT_COLUMNS,
            CatalogView::PgCollation => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("collname", ColumnType::Name, NO_LENGTH),
            ],
            // Exactly the two `ActiveRecord` reads of each, which is this module's standing rule:
            // a column it does not read is `42703`, the answer a real server gives for a name that
            // is not a column at all, rather than a value nobody measured. `extname` is a `name`
            // and the three oids are `oid` on a real server; all four are this node's own types.
            CatalogView::PgExtension => &[
                ("extname", ColumnType::Name, NO_LENGTH),
                ("extnamespace", ColumnType::Oid, NO_LENGTH),
                // Added for `CREATE EXTENSION`, which is where a version comes from: the one the
                // build offers as `default_version`, not one the statement chooses.
                ("extversion", ColumnType::Text, NO_LENGTH),
            ],
            // `inhseqno` numbers a child's parents from **1**, in the order the `INHERITS` clause
            // wrote them — measured, and it is what tells multiple inheritance apart.
            CatalogView::PgInherits => &[
                ("inhrelid", ColumnType::Int8, NO_LENGTH),
                ("inhparent", ColumnType::Int8, NO_LENGTH),
                ("inhseqno", ColumnType::Int4, NO_LENGTH),
            ],
            // What the capture reads, and `prosrc` because its **length** is how a client checks
            // the body survived. `prokind` is `f` and `provolatile` `v`, and both are `"char"`
            // here as they are on a real server (ADR 0095).
            CatalogView::PgLanguage => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("lanname", ColumnType::Name, NO_LENGTH),
                ("lanpltrusted", ColumnType::Bool, NO_LENGTH),
            ],
            CatalogView::PgProc => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("proname", ColumnType::Name, NO_LENGTH),
                ("pronamespace", ColumnType::Oid, NO_LENGTH),
                ("prokind", ColumnType::Char, NO_LENGTH),
                ("pronargs", ColumnType::Int2, NO_LENGTH),
                ("provolatile", ColumnType::Char, NO_LENGTH),
                ("prosrc", ColumnType::Text, NO_LENGTH),
                // **An oid, not the name.** `pg_language.oid = pg_proc.prolang` is the join
                // every client writes to learn a function's language, and a `text` here made it
                // `42883 operator does not exist: bigint = text` — a join that reads as a type
                // error about a catalog rather than as a missing column.
                ("prolang", ColumnType::Oid, NO_LENGTH),
                // `oidvector` on a real server: the argument types, space separated. Rendered as
                // text here, which is order-sensitive and so compares exactly as an `oidvector`
                // does — the same trade `regtype` and `name` make in every other view.
                //
                // **Last, because the row builder appends it there.** Declaring it beside `prosrc`
                // where it reads best put it in `prolang`'s slot, and the misalignment surfaced as
                // `pg_proc JOIN pg_language` returning nothing for a user's function — a column
                // order is a contract with the row builder, not a matter of taste.
                // **`oidvector`** — the type the `::oidvector` cast beside it already names,
                // and the one `ActiveRecord` compares against in
                // `can_perform_case_insensitive_comparison_for?` (ADR 0077).
                ("proargtypes", ColumnType::OidVector, NO_LENGTH),
            ],
            // `tgenabled` is a **letter** and `tgtype` a bitmask, neither of which is the word the
            // DDL used.
            CatalogView::PgTrigger => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("tgrelid", ColumnType::Int8, NO_LENGTH),
                ("tgname", ColumnType::Name, NO_LENGTH),
                ("tgenabled", ColumnType::Char, NO_LENGTH),
                ("tgtype", ColumnType::Int2, NO_LENGTH),
                ("tgnargs", ColumnType::Int2, NO_LENGTH),
                ("tgisinternal", ColumnType::Bool, NO_LENGTH),
                ("tgfoid", ColumnType::Oid, NO_LENGTH),
            ],
            // `schemaname` and `tablespace` are what a real server's view has and this node has
            // neither concept: one schema, and no tablespaces — `public` and NULL, which is what
            // a real server answers for an index in the default tablespace too.
            CatalogView::PgIndexes => &[
                ("schemaname", ColumnType::Name, NO_LENGTH),
                ("tablename", ColumnType::Name, NO_LENGTH),
                ("indexname", ColumnType::Name, NO_LENGTH),
                ("tablespace", ColumnType::Name, NO_LENGTH),
                ("indexdef", ColumnType::Text, NO_LENGTH),
            ],
            // **`viewowner` is here and is empty**, because this node has no roles: the column has
            // to exist for `SELECT * FROM pg_views` to have PostgreSQL's shape, and a name
            // invented for it would be a user nobody created.
            CatalogView::PgViews => &[
                ("schemaname", ColumnType::Name, NO_LENGTH),
                ("viewname", ColumnType::Name, NO_LENGTH),
                ("viewowner", ColumnType::Name, NO_LENGTH),
                ("definition", ColumnType::Text, NO_LENGTH),
            ],
            // A real server's order, so `SELECT *` expands the way a client expects.
            // `matviewowner` and `tablespace` are empty here for the same reason `viewowner` is:
            // this node has neither roles nor tablespaces, and a name invented for one would be an
            // object nobody created.
            CatalogView::PgMatviews => &[
                ("schemaname", ColumnType::Name, NO_LENGTH),
                ("matviewname", ColumnType::Name, NO_LENGTH),
                ("matviewowner", ColumnType::Name, NO_LENGTH),
                ("tablespace", ColumnType::Name, NO_LENGTH),
                ("hasindexes", ColumnType::Bool, NO_LENGTH),
                ("ispopulated", ColumnType::Bool, NO_LENGTH),
                ("definition", ColumnType::Text, NO_LENGTH),
            ],
            // **All twenty-two, in a real server's order**, because `SELECT *` on this view is
            // what a client writes and the column *after* the one it wanted has to be where it
            // expects. Three of the types are ones this node does not have — `inet` for
            // `client_addr`, `xid` for the two transaction columns, `name` for the two identifier
            // ones — and each answers as the nearest thing it does have, the standing trade every
            // `pg_catalog` column makes.
            // Sixteen columns, in PostgreSQL 19's order and under its names and types — measured
            // from `\d pg_locks` rather than copied from documentation. `xid` is the one type this
            // node does not have, and `transactionid` is an `int8` here carrying the holder's
            // start timestamp, which is the identity a waiter is waiting for.
            CatalogView::PgLocks => &[
                ("locktype", ColumnType::Text, NO_LENGTH),
                ("database", ColumnType::Oid, NO_LENGTH),
                ("relation", ColumnType::Int8, NO_LENGTH),
                ("page", ColumnType::Int4, NO_LENGTH),
                ("tuple", ColumnType::Int2, NO_LENGTH),
                ("virtualxid", ColumnType::Text, NO_LENGTH),
                ("transactionid", ColumnType::Int8, NO_LENGTH),
                // **Both `oid`s, and they fit because the key is already split**: `classid` is
                // an advisory key's high 32 bits and `objid` its low 32, which is what makes
                // `(classid::bigint << 32) | objid::bigint` the number the client passed. Each
                // half is a `u32` by construction — unlike `pg_depend`'s pair of the same names,
                // which name catalogs and stay `bigint`.
                ("classid", ColumnType::Oid, NO_LENGTH),
                ("objid", ColumnType::Oid, NO_LENGTH),
                ("objsubid", ColumnType::Int2, NO_LENGTH),
                ("virtualtransaction", ColumnType::Text, NO_LENGTH),
                ("pid", ColumnType::Int4, NO_LENGTH),
                ("mode", ColumnType::Text, NO_LENGTH),
                ("granted", ColumnType::Bool, NO_LENGTH),
                ("fastpath", ColumnType::Bool, NO_LENGTH),
                ("waitstart", ColumnType::TimestampTz, NO_LENGTH),
            ],
            // **`pg_roles` and `pg_authid` are the same rows.** On a real server `pg_roles` is a
            // view over `pg_authid` that replaces the password with `********` and is readable by
            // everyone, where `pg_authid` is superuser-only. This node stores no password and
            // enforces no privilege, so the two differ in exactly one column and that is honest
            // rather than lazy: `rolpassword` is NULL here either way.
            CatalogView::PgRoles | CatalogView::PgAuthid => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("rolname", ColumnType::Name, NO_LENGTH),
                ("rolsuper", ColumnType::Bool, NO_LENGTH),
                ("rolinherit", ColumnType::Bool, NO_LENGTH),
                ("rolcreaterole", ColumnType::Bool, NO_LENGTH),
                ("rolcreatedb", ColumnType::Bool, NO_LENGTH),
                ("rolcanlogin", ColumnType::Bool, NO_LENGTH),
                ("rolreplication", ColumnType::Bool, NO_LENGTH),
                ("rolbypassrls", ColumnType::Bool, NO_LENGTH),
                ("rolconnlimit", ColumnType::Int4, NO_LENGTH),
                ("rolpassword", ColumnType::Text, NO_LENGTH),
                ("rolvaliduntil", ColumnType::TimestampTz, NO_LENGTH),
            ],
            // Eight columns in PostgreSQL 19's order, from `\d pg_prepared_statements`.
            // `parameter_types` and `result_types` are `regtype[]` there and here, and they spell
            // "nothing" differently on purpose: a statement that takes no parameters has `{}` and
            // one that returns no rows has **NULL** — measured side by side on an `INSERT` with no
            // `RETURNING`, whose `parameter_types` is `{integer}` and whose `result_types` is NULL.
            CatalogView::PgPreparedStatements => &[
                // **`text`, not `name`** — the one column in the catalog called `name` that is not
                // one, measured off `pg_type` rather than read off the column's spelling. A
                // prepared statement's name is a client's string and not an identifier: it can be
                // longer than 63 characters and is not truncated.
                ("name", ColumnType::Text, NO_LENGTH),
                ("statement", ColumnType::Text, NO_LENGTH),
                ("prepare_time", ColumnType::TimestampTz, NO_LENGTH),
                ("parameter_types", ColumnType::RegTypeArray, NO_LENGTH),
                ("result_types", ColumnType::RegTypeArray, NO_LENGTH),
                ("from_sql", ColumnType::Bool, NO_LENGTH),
                ("generic_plans", ColumnType::Int8, NO_LENGTH),
                ("custom_plans", ColumnType::Int8, NO_LENGTH),
            ],
            CatalogView::PgStatActivity => &[
                ("datid", ColumnType::Oid, NO_LENGTH),
                ("datname", ColumnType::Name, NO_LENGTH),
                ("pid", ColumnType::Int4, NO_LENGTH),
                ("leader_pid", ColumnType::Int4, NO_LENGTH),
                ("usesysid", ColumnType::Oid, NO_LENGTH),
                ("usename", ColumnType::Name, NO_LENGTH),
                ("application_name", ColumnType::Text, NO_LENGTH),
                ("client_addr", ColumnType::Text, NO_LENGTH),
                ("client_hostname", ColumnType::Text, NO_LENGTH),
                ("client_port", ColumnType::Int4, NO_LENGTH),
                ("backend_start", ColumnType::TimestampTz, NO_LENGTH),
                ("xact_start", ColumnType::TimestampTz, NO_LENGTH),
                ("query_start", ColumnType::TimestampTz, NO_LENGTH),
                ("state_change", ColumnType::TimestampTz, NO_LENGTH),
                ("wait_event_type", ColumnType::Text, NO_LENGTH),
                ("wait_event", ColumnType::Text, NO_LENGTH),
                ("state", ColumnType::Text, NO_LENGTH),
                ("backend_xid", ColumnType::Int8, NO_LENGTH),
                ("backend_xmin", ColumnType::Int8, NO_LENGTH),
                ("query_id", ColumnType::Int8, NO_LENGTH),
                ("query", ColumnType::Text, NO_LENGTH),
                ("backend_type", ColumnType::Text, NO_LENGTH),
            ],
            // `partstrat` is a **one-letter code** and `partattrs` an `int2vector` — neither is
            // the word the DDL used, which `pg_get_partkeydef` gives instead.
            CatalogView::PgPartitionedTable => &[
                ("partrelid", ColumnType::Int8, NO_LENGTH),
                ("partstrat", ColumnType::Char, NO_LENGTH),
                ("partnatts", ColumnType::Int2, NO_LENGTH),
                ("partattrs", ColumnType::Text, NO_LENGTH),
            ],
            // The five a real server has, in its order. `name` is of type `name` there and the
            // four others are `text`; this node has one string type and answers `text` for all
            // five, which is the same trade every `pg_catalog` column makes.
            CatalogView::PgAvailableExtensions => &[
                ("name", ColumnType::Name, NO_LENGTH),
                ("default_version", ColumnType::Text, NO_LENGTH),
                ("installed_version", ColumnType::Text, NO_LENGTH),
                ("location", ColumnType::Text, NO_LENGTH),
                ("comment", ColumnType::Text, NO_LENGTH),
            ],
            // `enumsortorder` is a `real` on a real server, which is the one place this view's
            // types are worth reading: the order is a float so a value can be inserted *between*
            // two others without renumbering.
            // The four the adapter reads, plus the `oid` every catalog relation carries.
            CatalogView::PgDatabase => &[
                ("oid", ColumnType::Oid, NO_LENGTH),
                ("datname", ColumnType::Name, NO_LENGTH),
                ("encoding", ColumnType::Int4, NO_LENGTH),
                ("datcollate", ColumnType::Text, NO_LENGTH),
                ("datctype", ColumnType::Text, NO_LENGTH),
            ],
            // PostgreSQL's own seven columns, in its own order. `classid`/`objid`/`objsubid`
            // name the **dependent** object and `refclassid`/`refobjid`/`refobjsubid` the one it
            // depends on — a sequence depending on the column it fills, which is the only
            // dependency this node records.
            CatalogView::PgDepend => &[
                // **`classid` and `refclassid` are `bigint`, and the other two are `oid`s.** They
                // name a *catalog*, and a catalog view's own id here comes from `VIEW_ID_BASE`
                // near `i64::MAX` — a view has no relation record to take an id from — so
                // `dep.classid = 'pg_class'::regclass`, the join `ActiveRecord` writes to find a
                // serial's sequence, cannot be an `oid` comparison (ADR 0097). `objid` and
                // `refobjid` name a relation, whose id is a counter, and are `oid`s.
                ("classid", ColumnType::Int8, NO_LENGTH),
                ("objid", ColumnType::Int8, NO_LENGTH),
                ("objsubid", ColumnType::Int4, NO_LENGTH),
                ("refclassid", ColumnType::Int8, NO_LENGTH),
                ("refobjid", ColumnType::Int8, NO_LENGTH),
                ("refobjsubid", ColumnType::Int4, NO_LENGTH),
                ("deptype", ColumnType::Char, NO_LENGTH),
            ],
            CatalogView::PgSequence => &[
                ("seqrelid", ColumnType::Int8, NO_LENGTH),
                ("seqtypid", ColumnType::Oid, NO_LENGTH),
                ("seqstart", ColumnType::Int8, NO_LENGTH),
                ("seqincrement", ColumnType::Int8, NO_LENGTH),
                ("seqmax", ColumnType::Int8, NO_LENGTH),
                ("seqmin", ColumnType::Int8, NO_LENGTH),
                ("seqcache", ColumnType::Int8, NO_LENGTH),
                ("seqcycle", ColumnType::Bool, NO_LENGTH),
            ],
            // `enumtypid` is an `oid`, here as on a real server, and for the reason
            // `pg_cast.castsource` is: it is what a `regtype` is compared against.
            // `WHERE enumtypid = 'mood'::regtype` is what `ActiveRecord` sends to read an enum's
            // labels, and an `oid` beside a `regtype` is one representation where a `bigint`
            // beside one is not (ADR 0077).
            CatalogView::PgEnum => &[
                ("enumtypid", ColumnType::Oid, NO_LENGTH),
                ("enumlabel", ColumnType::Name, NO_LENGTH),
                ("enumsortorder", ColumnType::Real, NO_LENGTH),
            ],
            CatalogView::InformationSchemaTables => super::information_schema::TABLES_COLUMNS,
            CatalogView::InformationSchemaViews => super::information_schema::VIEWS_COLUMNS,
            CatalogView::InformationSchemaDomains => super::information_schema::DOMAINS_COLUMNS,
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
    /// The rows of one `information_schema` view, split out of [`CatalogView::rows_of`].
    fn information_schema_rows(
        view: CatalogView,
        txn: &dyn crate::backend::Txn,
        tenant: u64,
        rendering: crate::value::Rendering,
    ) -> Result<Vec<Vec<Datum>>> {
        match view {
            CatalogView::InformationSchemaTables => super::information_schema::tables(txn, tenant),
            CatalogView::InformationSchemaViews => super::information_schema::views(txn, tenant),
            CatalogView::InformationSchemaDomains => {
                super::information_schema::domains(txn, tenant)
            }
            CatalogView::InformationSchemaColumns => {
                super::information_schema::columns(txn, tenant, rendering)
            }
            CatalogView::InformationSchemaTableConstraints => {
                super::information_schema::table_constraints(txn, tenant)
            }
            CatalogView::InformationSchemaKeyColumnUsage => {
                super::information_schema::key_column_usage(txn, tenant)
            }
            // `referential_constraints` has no rows and no function; it falls through with every
            // `pg_catalog` view, which never reaches here.
            other => Ok(other.rows()),
        }
    }

    /// Every row, in OID order — reading the catalog for the views whose rows are not constants.
    ///
    /// `txn` and `tenant` are what make `pg_class` a **view over the records** rather than a
    /// second copy of them: its rows are one scan of the same name keys `CREATE TABLE` writes, so
    /// there is no state to keep in step and no way for the two to disagree. The constant views
    /// ignore both.
    ///
    /// `rendering` reaches exactly three of the arms below — the two that print a column's stored
    /// `DEFAULT` and the `information_schema` view over the same thing — because
    /// `pg_get_expr` renders a constant through the type's **output function**, so an `interval`
    /// default reads `'3 years'::interval` under the boot style and `'P3Y'::interval` under the one
    /// `ActiveRecord` sets. Measured; it is the whole of `test_schema_dump_with_default_value`.
    pub fn rows_of(
        self,
        txn: &dyn crate::backend::Txn,
        tenant: u64,
        rendering: crate::value::Rendering,
        prepared: &[crate::session::PreparedStatement],
        advisory: Option<&crate::advisory::Locks>,
    ) -> Result<Vec<Vec<Datum>>> {
        let mut rows = self.built_rows(txn, tenant, rendering, prepared, advisory)?;
        self.as_declared(&mut rows);
        Ok(rows)
    }

    /// **A view's rows carry the types its column list declares.**
    ///
    /// The builders below write every id as a `Datum::Int8`, because an id is a `u64` in this
    /// crate and a `bigint` is what fits one. Which of those columns a *client* is told is an
    /// `oid` is decided in one place — [`Self::columns`] — and a client told `oid` is sent four
    /// bytes, so the datum has to follow the declaration or the wire disagrees with the
    /// `RowDescription`. That is the defect shape this crate keeps closing (run 98's `->`, run
    /// 105's enum, run 106's `array_agg`): right bytes, wrong declared type, invisible to `psql`.
    ///
    /// One coercion here rather than a cast in forty builders, so that a column which changes its
    /// mind cannot leave a row behind — and so a builder added later inherits the rule instead of
    /// having to know it.
    ///
    /// **Saturating, and the saturation is a test.** An id above `u32::MAX` cannot be an `oid` at
    /// all; the columns whose ids come from the five derived bases at bits 56–62
    /// (`pg_relations::PRIMARY_KEY_OID_BASE` and `pg_constraint`'s four) are **not** declared
    /// `oid` for exactly that reason, and `tests/oid_type.rs` asserts no declared one saturates.
    fn as_declared(self, rows: &mut [Vec<Datum>]) {
        let columns = self.columns();
        for row in rows {
            for (at, datum) in row.iter_mut().enumerate() {
                match columns.get(at) {
                    Some((_, ColumnType::Oid, _)) => {
                        if let Datum::Int8(id) = *datum {
                            *datum = Datum::Oid(u32::try_from(id).unwrap_or(u32::MAX));
                        }
                    }
                    // **A `regproc` is written as its name and read as an oid.** The builders name
                    // the input function — that is what the column is *for* — and the oid is
                    // looked up from the measured table rather than derived, because a function's
                    // oid is a fact about a real server and not about this one (ADR 0098).
                    Some((_, ColumnType::RegProc, _)) => {
                        if let Datum::Text(name) = datum {
                            let oid = crate::value::reg_proc::oid_of(name).unwrap_or(0);
                            *datum = Datum::RegProc {
                                oid,
                                name: std::mem::take(name).into_boxed_str(),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn built_rows(
        self,
        txn: &dyn crate::backend::Txn,
        tenant: u64,
        rendering: crate::value::Rendering,
        prepared: &[crate::session::PreparedStatement],
        advisory: Option<&crate::advisory::Locks>,
    ) -> Result<Vec<Vec<Datum>>> {
        match self {
            CatalogView::PgType => pg_type_rows(txn, tenant),
            CatalogView::PgRange => pg_range_rows(txn, tenant),
            CatalogView::PgEnum => pg_enum_rows(txn, tenant),
            CatalogView::PgDepend => pg_depend_rows(txn, tenant),
            CatalogView::PgSequence => pg_sequence_rows(txn, tenant),
            CatalogView::PgClass => pg_class_rows(txn, tenant),
            CatalogView::PgAttribute => super::pg_attribute::rows(txn, tenant),
            CatalogView::PgAttrdef => super::pg_attribute::default_rows(txn, tenant, rendering),
            CatalogView::PgIndex => super::pg_index::rows(txn, tenant),
            CatalogView::PgInherits => inherits_rows(txn, tenant),
            CatalogView::PgProc => proc_rows(txn, tenant),
            CatalogView::PgTrigger => trigger_rows(txn, tenant),
            CatalogView::PgPartitionedTable => partitioned_table_rows(txn, tenant),
            CatalogView::PgIndexes => indexes_rows(txn, tenant),
            CatalogView::PgViews => views_rows(txn, tenant),
            CatalogView::PgMatviews => matviews_rows(txn, tenant),
            CatalogView::PgStatActivity => stat_activity_rows(txn, tenant),
            CatalogView::PgRoles => role_rows(txn, false),
            CatalogView::PgAuthid => role_rows(txn, true),
            CatalogView::PgLocks => Ok(locks_rows(txn, tenant, advisory)),
            CatalogView::PgPreparedStatements => Ok(prepared_statement_rows(prepared)),
            CatalogView::PgConstraint => super::pg_constraint::rows(txn, tenant),
            // **The standard's views delegate as a group**, in their own function: they are six
            // arms that all call one module, and keeping them here is what pushed `rows_of` past
            // the size lint when the sixth arrived (ADR 0065's `information_schema.domains`).
            view if view.schema() == super::INFORMATION_SCHEMA => {
                Self::information_schema_rows(view, txn, tenant, rendering)
            }
            // **The four a real server has**, measured: `c`, `internal`, `plpgsql` and `sql`,
            // with only the last two `lanpltrusted` — a non-superuser may write a function in
            // those and not in the other two. Nothing here acts on the flag; it is reported
            // because a client reads it before defining one.
            //
            // All four are listed even though only two can hold a function here, because this is
            // the catalog a client *reads*: a language missing from it is a language that does not
            // exist, and `c` and `internal` do exist on the server this node answers as. What
            // happens when a function is actually written in one is `crate::exec::ddl`'s answer.
            CatalogView::PgLanguage => Ok(LANGUAGES
                .iter()
                .map(|(oid, name, trusted)| {
                    vec![
                        Datum::Int8(*oid),
                        Datum::Text((*name).to_owned()),
                        Datum::Bool(*trusted),
                    ]
                })
                .collect()),
            // `amtype` `i` — an index method, which is what both of these are. A real server's
            // `pg_am` also holds table methods (`amtype` `t`, `heap`); this node has one storage
            // engine and no `USING` on a table, so there is nothing to name.
            CatalogView::PgOpclass => Ok(pg_opclass_rows()),
            CatalogView::PgCollation => Ok(pg_collation_rows()),
            CatalogView::PgCast => Ok(pg_cast_rows()),
            CatalogView::PgAm => Ok(pg_am_rows()),
            CatalogView::PgTsConfig => Ok(pg_ts_config_rows()),
            // **One row per schema**, `public` included — and `public` is not a record: it is a
            // property of the build, the way the available extensions are, so a tenant that has
            // created nothing still reports it.
            // **One row per database in the cluster's directory**, and this is the one catalog
            // view here that is *not* about the tenant asking: `pg_database` answers the same
            // list from every database, which is why the directory is the one piece of catalog
            // state that carries no tenant (ADR 0052).
            //
            // **The oid is the id is the tenant**, so `WHERE datname = current_database()` matches
            // by construction rather than by two constants being kept equal by hand — and a
            // cluster nobody has written a directory for answers with the one database it is
            // serving, which is what it answered before it could name a second.
            CatalogView::PgDatabase => Ok(super::databases(txn)?
                .into_iter()
                .map(|(name, id)| {
                    vec![
                        Datum::Int8(i64::try_from(id).unwrap_or(i64::MAX)),
                        Datum::Text(name),
                        // 6 is `UTF8` in PostgreSQL's own encoding table, and it is the only
                        // encoding this node speaks — the startup packet says so too
                        // (`client_encoding`).
                        Datum::Int4(6),
                        // **`C`, not the oracle's `en_US.utf8`.** A collation is a feature this
                        // node does not have (`CatalogView::PgCollation` is empty for the same
                        // reason), so the honest locale is the one that sorts by byte value.
                        Datum::Text("C".to_owned()),
                        Datum::Text("C".to_owned()),
                    ]
                })
                .collect()),
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
            // **`extnamespace` is read back, not assumed.** It was `PUBLIC_NAMESPACE_OID` for
            // every row, which made `CREATE EXTENSION … SCHEMA <name>` unreportable even once it
            // was accepted, and put `plpgsql` in `public` where a real server has it in
            // `pg_catalog`. `ActiveRecord#extensions` joins this to `pg_namespace` and then drops
            // the schema when it equals `current_schema`, so the constant made every extension
            // list bare.
            CatalogView::PgExtension => {
                let schemas = super::schema_names(txn, tenant)?;
                Ok(installed(txn, tenant)?
                    .into_iter()
                    .map(|(name, version, schema)| {
                        vec![
                            Datum::Text(name),
                            Datum::Int8(namespace_oid(&schemas, &schema)),
                            Datum::Text(version),
                        ]
                    })
                    .collect())
            }
            CatalogView::PgAvailableExtensions => {
                let installed = installed(txn, tenant)?;
                Ok(AVAILABLE_EXTENSIONS
                    .iter()
                    .map(|(name, default_version)| {
                        let version = installed
                            .iter()
                            .find(|(installed, ..)| installed == name)
                            .map(|(_, version, _)| Datum::Text(version.clone()));
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
            | CatalogView::PgMatviews
            | CatalogView::InformationSchemaDomains
            | CatalogView::PgRange

            | CatalogView::PgOpclass
            | CatalogView::PgCollation
            | CatalogView::PgCast
            | CatalogView::PgExtension
            | CatalogView::PgInherits
            | CatalogView::PgAm
            | CatalogView::PgTsConfig
            | CatalogView::PgProc
            | CatalogView::PgRoles
            | CatalogView::PgAuthid
            | CatalogView::PgTrigger
            | CatalogView::PgLanguage
            | CatalogView::PgPartitionedTable
            | CatalogView::PgIndexes
            // Catalog-backed like `pg_indexes`: `rows_of` answers for it before it delegates here.
            | CatalogView::PgViews
            | CatalogView::PgStatActivity
            | CatalogView::PgLocks
            | CatalogView::PgPreparedStatements
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
            | CatalogView::InformationSchemaViews
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
                        matview: None,
                        // Synthetic and never stored, so its persistence is the default.
                        persistence: crate::catalog::Persistence::Permanent,
                        on_commit: super::OnCommit::default(),
                        id: view.id(),
                        name: view.name().to_owned(),
                        columns: view
                            .columns()
                            .iter()
                            .map(|(name, ty, typmod)| ColumnDef {
                                collation: None,
                                name: (*name).to_owned(),
                                ty: *ty,
                                // **The length its own column list declares.** "A computed
                                // relation declares no lengths" stood here and was a property of
                                // the list rather than of the relation: `information_schema`'s
                                // `yes_or_no` columns are `character varying(3)` on a real
                                // server, which is a domain's typmod and reaches a client through
                                // this field.
                                typmod: *typmod,
                                default_expr: None,
                                not_null: false,
                                default: None,
                                missing: None,
                                generated: None,
                                generated_virtual: false,
                                comment: None,
                                dropped: false,
                                user_type: None,
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
                        enums: std::collections::BTreeMap::new(),
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

/// The view a **stored** name is, if it is one.
///
/// Two spellings reach here and both are the parser's output. A bare `pg_class` is the search
/// path's answer — `pg_catalog` is in the implicit path, so a name with no qualifier finds the
/// catalog before it finds anything else. A `pg_catalog`-qualified one carries the schema in the
/// stored form (`super::SCHEMA_SEPARATOR`), and then only a relation *in* `pg_catalog` may answer:
/// `pg_catalog.books` is `42P01` on a real server however many `books` there are in `public`.
///
/// `information_schema.tables` is its own third case and always has been: the qualifier is part
/// of the name, because that schema is **not** in the search path and a client must write it.
#[must_use]
pub fn view(name: &str) -> Option<CatalogView> {
    if let Some(bare) = name.strip_prefix(super::PG_CATALOG_SCHEMA)
        && let Some(bare) = bare.strip_prefix(super::SCHEMA_SEPARATOR)
    {
        return CatalogView::ALL
            .into_iter()
            .find(|view| view.schema() == super::PG_CATALOG_SCHEMA && view.relname() == bare);
    }
    CatalogView::ALL
        .into_iter()
        .find(|view| view.name() == name)
}

/// The view an **oid** is, if it is one — the inverse of the reserved ids `CatalogView::id`
/// hands out, and what makes `<oid>::regclass` print a catalog relation's name.
///
/// The name is written plainly rather than linked: `id` is private, and a public item's doc may
/// not link to one under `RUSTDOCFLAGS="-D warnings"`.
#[must_use]
pub fn view_by_oid(oid: i64) -> Option<CatalogView> {
    CatalogView::ALL
        .into_iter()
        .find(|view| i64::try_from(view.id()).unwrap_or(i64::MAX) == oid)
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
/// The languages `pg_language` reports, and the oids `pg_proc.prolang` points at.
///
/// **Ours rather than a real server's**, the same choice every reserved id in this module makes:
/// what has to be true is that the two catalogs agree with each other, which is the join a client
/// writes.
const LANGUAGES: &[(i64, &str, bool)] = &[
    (13, "c", false),
    (12, "internal", false),
    (PLPGSQL_LANGUAGE_OID, "plpgsql", true),
    (14, "sql", true),
];

/// The oid `pg_language` reports for `plpgsql`, and what a `pg_proc.prolang` would point at.
const PLPGSQL_LANGUAGE_OID: i64 = 14_024;

/// One language's oid by name, for `pg_proc.prolang`. Zero for a name that is not a language,
/// which no stored function has: `crate::exec::ddl::create_function` refuses one first.
fn language_oid(name: &str) -> i64 {
    LANGUAGES
        .iter()
        .find(|(_, known, _)| known.eq_ignore_ascii_case(name))
        .map_or(0, |(oid, _, _)| *oid)
}

const PUBLIC_NAMESPACE_OID: i64 = 11;

/// The oid of a schema by name, for `relnamespace` and its kin.
///
/// `public`'s is fixed the way a real server fixes it; every other schema's is the id its record
/// was allocated. A name with no schema — nothing can produce one — falls back to `public`, which
/// is the answer that cannot mislead.
pub(super) fn namespace_oid(schemas: &[(String, u64)], schema: &str) -> i64 {
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
/// **Seven, by the user's decision of 2026-09-05**: `uuid-ossp`, `pgcrypto`, `plpgsql`, `hstore`,
/// `citext`, `ltree` and `pg_trgm`. Anything else — `postgres_fdw` above all, which is ten of the
/// suite's `CREATE EXTENSION` failures on its own and every one of them in `foreign_table_test.rb`
/// — is refused by name until the user says otherwise.
///
/// The comment here used to read *"exactly the ones `postgresql_specific_schema.rb` needs in order
/// to load, and no more"*, which was true when it was written and then quietly stopped being: the
/// list grew past that file twice. It is a **ruling** rather than a derivation, so it is written as
/// one, and the next entry needs the same ruling rather than an argument from what some schema
/// happens to load.
///
/// The versions are the oracle's own (`pgcrypto` 1.4, `uuid-ossp` 1.1, `ltree` 1.3 where the two
/// string extensions are at 1.8), and `plpgsql` is [`PRE_INSTALLED`] rather than merely available,
/// which is what every PostgreSQL database reports for it.
///
/// **Installing one does not bring the functions it carries.** `uuid_generate_v4()` and
/// `gen_random_uuid()` are still `42883` naming themselves — the statement's job is to let the
/// schema load, and a function this node does not have is a gap of its own with its own capture.
/// The list is deliberately short for the reason `pg_type` is: an entry here tells a client this
/// server has something, so a type-bearing extension does not go on it until the type does.
/// **`hstore` is the first entry that carries a type**, and it went on in the commit that made
/// `ColumnType::Hstore` — not before.
///
/// **The type is here whether or not the extension is installed**, which is one place this differs
/// from a real server: there, `CREATE TABLE t (c hstore)` without the extension is
/// `42704 type "hstore" does not exist`, and here it is accepted. The type name resolves in the
/// lowering, which has no catalog to ask (`crate::parse::lower`), and every statement the suite
/// sends installs the extension first — so it is a gap nothing measured reaches, recorded here
/// rather than in a divergence nothing would exercise.
const AVAILABLE_EXTENSIONS: [(&str, &str); 7] = [
    ("citext", "1.8"),
    ("hstore", "1.8"),
    // Measured on the oracle, like the rest: `ltree` is at **1.3** where the two string
    // extensions are at 1.8.
    ("ltree", "1.3"),
    ("pg_trgm", "1.6"),
    ("pgcrypto", "1.4"),
    ("plpgsql", "1.0"),
    ("uuid-ossp", "1.1"),
];

/// The column types an extension provides, which is what a `DROP EXTENSION` has to account for.
///
/// **Only the two that reach a column.** `pgcrypto` and `uuid-ossp` bring functions, and `plpgsql`
/// a language; none of them can be the type of a stored column, so dropping one takes nothing with
/// it. If a third type-bearing extension arrives, this is the list that has to learn it — and the
/// symptom of forgetting would be a column whose type nothing declares.
#[must_use]
pub fn extension_types(extension: &str) -> &'static [ColumnType] {
    match extension {
        "citext" => &[ColumnType::Citext],
        "hstore" => &[ColumnType::Hstore, ColumnType::HstoreArray],
        "ltree" => &[ColumnType::Ltree, ColumnType::LtreeArray],
        _ => &[],
    }
}

/// The extension every database has installed before anything runs, and **the schema it lives
/// in**, which is not `public`.
///
/// Measured on 19beta1: `plpgsql`'s `extnamespace` is `pg_catalog` and its `extrelocatable` is
/// `f` - alone among the extensions this build offers, and the reason `ActiveRecord#extensions`
/// lists it as `pg_catalog.plpgsql` rather than bare. Reporting it in `public` put the wrong
/// string in that list for the one extension every database has.
const PRE_INSTALLED: (&str, &str, &str) = ("plpgsql", "1.0", super::PG_CATALOG_SCHEMA);

/// Every installed extension, in name order, as `(name, version, schema)`: the one that is always
/// there, then whatever a `CREATE EXTENSION` recorded.
fn installed(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<(String, String, String)>> {
    let mut rows = vec![(
        PRE_INSTALLED.0.to_owned(),
        PRE_INSTALLED.1.to_owned(),
        PRE_INSTALLED.2.to_owned(),
    )];
    for row in super::installed_extensions(txn, tenant)? {
        if row.0 != PRE_INSTALLED.0 {
            rows.push(row);
        }
    }
    rows.sort();
    Ok(rows)
}

/// Whether this tenant has already installed an extension.
pub fn is_installed(txn: &dyn crate::backend::Txn, tenant: u64, name: &str) -> Result<bool> {
    Ok(installed(txn, tenant)?
        .iter()
        .any(|(installed, ..)| installed == name))
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
                Datum::Int8(language_oid(&function.language)),
                // A user's function takes whatever it was declared with, and this node does not
                // record argument *types* — only their count — so the list is empty rather than
                // guessed. Empty is what an `oidvector` of no arguments prints as.
                Datum::Text(String::new()),
            ]
        })
        // **The built-ins too**, which this view never listed: it mapped `catalog::functions`,
        // which is what `CREATE FUNCTION` made. So `lower` was not in `pg_proc` at all and
        // `ActiveRecord`'s case-insensitivity probe could not have answered `true` for any type —
        // it would have answered `false`, which is a wrong answer where a refusal was honest.
        .chain(
            BUILTIN_FUNCTIONS
                .iter()
                .map(|(oid, name, args, _ret, volatile)| {
                    vec![
                        Datum::Int8(*oid),
                        Datum::Text((*name).to_owned()),
                        Datum::Int8(PUBLIC_NAMESPACE_OID),
                        Datum::Text("f".to_owned()),
                        Datum::Int2(1),
                        Datum::Text((*volatile).to_owned()),
                        // A built-in has no SQL body to show; `prosrc` on a real server names the C
                        // symbol, and naming one this node does not have would be a claim.
                        Datum::Text(String::new()),
                        // `internal` is the language a real server reports for these.
                        Datum::Int8(language_oid("internal")),
                        Datum::Text((*args).to_owned()),
                    ]
                }),
        )
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
/// `pg_views`: one row per view, with the `SELECT` it stands for.
///
/// **Not materialized views** — those are `pg_matviews` on a real server, and `pg_views` answers
/// nothing for one (measured). This node has neither, so the distinction costs nothing to keep and
/// would cost a wrong row to drop.
/// One row: the session doing the asking.
///
/// **`datname` is the tenant's own name**, read from the directory rather than kept beside it —
/// the oid *is* the id *is* the tenant (ADR 0052), so `WHERE datname = current_database()`
/// matches this row by construction and cannot drift out of step with what `pg_database` says.
///
/// **`state` is `active` and not `idle`**, and that is the answer `migration_test.rb:1108` turns
/// on: a backend that is running the query which reads the view is by definition not idle, so a
/// view built out of the asking session alone reports no idle sessions, which is what a real
/// server reports for a cluster where nobody else is connected. The test asks whether the
/// connection that held an advisory lock has *gone*; both sides answer that it has.
///
/// The columns this node cannot know are NULL rather than invented. A NULL there is what a real
/// server sends for a backend whose detail it will not show, so a client that reads one is on a
/// path it already has; a `client_addr` of `127.0.0.1` or a `backend_start` of "now" would be a
/// fact nobody measured.
/// `pg_locks`: what this node holds, and who is waiting for it.
///
/// **Two row shapes, because that is what a real server answers with.** Measured against
/// PostgreSQL 19 with a held row and a blocked writer: the holder appears as a `tuple` row with
/// `granted = true`, and the waiter appears as a `transactionid` row with `granted = false` naming
/// the transaction it waits for. Reading those two together is how a stuck session's counterpart is
/// found, which is the whole reason this view exists.
///
/// **What is `NULL` here and is a number on a real server**: `page` and `tuple`, because this node
/// locks a *key* rather than a tuple at a page offset; and `virtualxid`, because there are no
/// virtual transactions. A row lock's identity travels in `virtualtransaction` instead, as
/// `0/<transaction id>` — the shape PostgreSQL uses, and the only column that tells two sessions of
/// this node apart, since every one of them reports the node's own `pid`.
///
/// **What is not modelled**: PostgreSQL also shows a waiter holding a `tuple` lock while it waits,
/// and shows relation-level locks (`RowShareLock`, `RowExclusiveLock`) that this node does not take
/// at all. Their absence is the truth about this node rather than a gap in the view.
///
/// Locks are node-local (`crate::backend::locks`), so this answers for **this process**. Rows whose
/// key belongs to another database are left out, which is what `database` filtering would do
/// anyway.
fn locks_rows(
    txn: &dyn crate::backend::Txn,
    tenant: u64,
    advisory: Option<&crate::advisory::Locks>,
) -> Vec<Vec<Datum>> {
    let view = txn.locks();
    // **A pid per row, from the lock table.** This was `std::process::id()` — the same number on
    // every row, so the column could not tell two holders apart and a join to
    // `pg_stat_activity.pid` matched nothing. On a real server `pg_locks.pid` *is* the backend pid:
    // the number `pg_backend_pid()` returns and the one `pg_cancel_backend` takes.
    let backend = |pid: u32| Datum::Int4(i32::try_from(pid).unwrap_or(i32::MAX));
    let database = Datum::Int8(i64::try_from(tenant).unwrap_or(i64::MAX));
    let mut rows = Vec::with_capacity(view.held.len() + view.waiting.len());
    // `holder id -> start_ts`, so a waiter's row can name the transaction it waits for the way a
    // real server does: by the transaction, not by the key.
    let mut start_of: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
    for (key, holder, start_ts, session) in &view.held {
        start_of.insert(*holder, *start_ts);
        let Some((owner, table_id)) = esker_keys::prefix::row_key_table(key) else {
            // Not a row key: an index entry or a metadata key, which this node does not row-lock.
            continue;
        };
        if owner != tenant {
            continue;
        }
        rows.push(vec![
            Datum::Text("tuple".to_owned()),
            database.clone(),
            Datum::Int8(i64::try_from(table_id).unwrap_or(i64::MAX)),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Int8(i64::try_from(*start_ts).unwrap_or(i64::MAX)),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Text(format!("0/{holder}")),
            backend(*session),
            // `FOR SHARE` is served as `FOR UPDATE` (ADR 0057 §5), so every row lock here is
            // exclusive and reporting anything else would describe a mode this node cannot take.
            Datum::Text("ExclusiveLock".to_owned()),
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Null,
        ]);
    }
    for (waiter, holder, session) in &view.waiting {
        rows.push(vec![
            Datum::Text("transactionid".to_owned()),
            database.clone(),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            // **`NULL` when the holder is not holding anything**, never a sentinel. This was
            // `i64::MAX`, and run 78's capture read it as a transaction waiting on an id that can
            // never commit or abort — a diagnostic inventing the thing it was asked to report.
            // With the wait-for graph no longer leaking there should be no such row at all; if one
            // appears, `NULL` says "this view does not know" rather than naming a transaction.
            start_of
                .get(holder)
                .and_then(|ts| i64::try_from(*ts).ok())
                .map_or(Datum::Null, Datum::Int8),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Text(format!("0/{waiter}")),
            backend(*session),
            // What a waiter asks for on the holder's transaction id, measured.
            Datum::Text("ShareLock".to_owned()),
            Datum::Bool(false),
            Datum::Bool(false),
            // The table records that a session waits, not since when.
            Datum::Null,
        ]);
    }
    // **The advisory locks, which are a third row shape and a different table.** Measured on
    // PostgreSQL 19 for `pg_advisory_lock(5295901941258979200)`:
    //
    // ```text
    // locktype | classid    | objid      | objsubid | mode          | granted | fastpath
    // advisory | 1233048257 | 3054176128 |        1 | ExclusiveLock | t       | f
    // ```
    //
    // with `relation`, `page`, `tuple`, `virtualxid` and `transactionid` all NULL. The key is
    // **split**: `classid` is its high 32 bits and `objid` its low 32, which is what makes
    // `(classid::bigint << 32) | objid::bigint` the number the client passed — the query
    // `connection_test.rb` writes, and the reason the halves cannot be swapped or widened.
    // `objsubid` is `1` for the one-argument form and `2` for `(int4, int4)`, which is how a real
    // server tells the two key spaces apart, and this node keeps that distinction in
    // `advisory::Space`.
    //
    // `database` is the **reading** session's, not the holder's: this table is one per node and
    // records no tenant, and every session that can read this view is in the same database as the
    // reader anyway. Declared rather than invented — the alternative is a NULL, and a real server
    // never shows one here.
    for row in advisory
        .map(crate::advisory::Locks::rows)
        .unwrap_or_default()
    {
        rows.push(vec![
            Datum::Text("advisory".to_owned()),
            database.clone(),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Int8(i64::from(row.key.classid())),
            Datum::Int8(i64::from(row.key.objid())),
            Datum::Int2(row.key.space.objsubid()),
            Datum::Text(format!("0/{}", row.session.pid())),
            Datum::Int4(row.session.pid()),
            Datum::Text(row.mode.name().to_owned()),
            // An advisory lock in this table is held: a session that cannot take one is told
            // `false` and queues nowhere, so there is no ungranted advisory row to report.
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Null,
        ]);
    }
    rows
}

/// Every role, for `pg_roles` and `pg_authid`.
///
/// One function for both, because the rows are the same: see the column list. `_authid` is there
/// so the difference has a name if this node ever stores a password.
fn role_rows(txn: &dyn crate::backend::Txn, _authid: bool) -> Result<Vec<Vec<Datum>>> {
    Ok(super::roles(txn)?
        .into_iter()
        .map(|role| {
            vec![
                Datum::Int8(i64::try_from(role.oid).unwrap_or(i64::MAX)),
                Datum::Text(role.name),
                // **Recorded, not enforced** — every one of these is what the statement asked
                // for and what `pg_roles` reports, and nothing in this node consults any of them.
                // Reporting `f` for a role created `SUPERUSER` was worse than either: the client
                // was told yes and then shown no.
                Datum::Bool(role.flags.superuser),
                // `rolinherit` is `true` for every role here, which is `CREATE ROLE`'s own default
                // on a real server (measured) — so it is a constant rather than a stored bit, and
                // a stored bit would have read `false` for every role written before it existed.
                Datum::Bool(true),
                Datum::Bool(role.flags.create_role),
                Datum::Bool(role.flags.create_db),
                // **The one attribute that changes behaviour**: `CREATE USER` implies it, `CREATE
                // ROLE` does not, and nothing else about the two statements differs.
                Datum::Bool(role.flags.login),
                Datum::Bool(false),
                Datum::Bool(false),
                // No connection limit, which is `-1` rather than NULL.
                Datum::Int4(-1),
                // No password is stored, so there is none to hide.
                Datum::Null,
                Datum::Null,
            ]
        })
        .collect())
}

/// Every `pg_prepared_statements` row: what **this** session has named, in name order.
///
/// **This session's and no other's**, which is where it parts company with `pg_stat_activity`
/// beside it: that view is cluster-wide on a real server and this one is per-backend, so the list
/// arrives from the session that is asking rather than from the process-wide registry. A second
/// connection's prepared statements are invisible here, exactly as they are on PostgreSQL.
///
/// Name order rather than PostgreSQL's hash order. Neither server promises one, and a corpus that
/// replays two `PREPARE`s needs the answer not to depend on a hash seed.
fn prepared_statement_rows(prepared: &[crate::session::PreparedStatement]) -> Vec<Vec<Datum>> {
    prepared
        .iter()
        .map(|statement| {
            vec![
                Datum::Text(statement.name.clone()),
                Datum::Text(statement.statement.clone()),
                // No wall clock is read here (`pg_locks.waitstart` and every timestamp in
                // `pg_stat_activity` are NULL for the same reason).
                Datum::Null,
                regtype_array(&statement.parameters),
                statement
                    .results
                    .as_deref()
                    .map_or(Datum::Null, regtype_array),
                Datum::Bool(statement.from_sql),
                // No plan cache, so no plan of either kind has been reused. `0` is what a real
                // server reports for a statement nobody has executed yet.
                Datum::Int8(0),
                Datum::Int8(0),
            ]
        })
        .collect()
}

/// A `regtype[]` of these OIDs, which is how both type columns of `pg_prepared_statements` print.
fn regtype_array(oids: &[u32]) -> Datum {
    Datum::Array(ArrayValue::one_dimensional(
        ColumnType::RegType,
        1,
        oids.iter()
            .map(|oid| Some(crate::value::regtype_of_oid(*oid)))
            .collect(),
    ))
}

fn stat_activity_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let datname = super::databases(txn)?
        .into_iter()
        .find(|(_, id)| *id == tenant)
        .map(|(name, _)| name);
    // **One row per live session, from the one registry every session is in** — a connection, a
    // `Pair` and a `Cluster` alike (`crate::session`). It used to be a single row whose pid was
    // `std::process::id()`, the same number for every session and therefore useless to the one
    // thing a client wants it for: `SELECT pid FROM pg_stat_activity WHERE query LIKE …`, which is
    // how `pg_cancel_backend` is aimed.
    Ok(crate::session::snapshot()
        .into_iter()
        .map(|(pid, activity)| stat_activity_row(tenant, datname.clone(), pid, &activity))
        .collect())
}

/// One `pg_stat_activity` row.
fn stat_activity_row(
    tenant: u64,
    datname: Option<String>,
    pid: u32,
    activity: &crate::session::Activity,
) -> Vec<Datum> {
    vec![
        Datum::Int8(i64::try_from(tenant).unwrap_or(i64::MAX)),
        datname.map_or(Datum::Null, Datum::Text),
        Datum::Int4(i32::try_from(pid).unwrap_or(i32::MAX)),
        // Not a parallel worker: there are none, so no backend here has a leader.
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Text(String::new()),
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        // Not waiting: a snapshot-isolated transaction does not block on another one, which is
        // ADR 0031's permanent caveat and is why both wait columns are NULL rather than empty.
        Datum::Null,
        Datum::Null,
        // **Three states, measured side by side with PG19.** `active` while a statement runs;
        // `idle in transaction` between statements inside an open block — the state that tells an
        // operator a session is holding locks and doing nothing — and `idle` otherwise. This node
        // answered `active` always, and then `idle`/`active` with no third case.
        Datum::Text(
            if activity.running {
                "active"
            } else if activity.in_transaction {
                "idle in transaction"
            } else {
                "idle"
            }
            .to_owned(),
        ),
        Datum::Null,
        Datum::Null,
        Datum::Null,
        // The statement this session is running, which is what `WHERE query LIKE …` needs. NULL
        // while it runs nothing, as a real server sends for a session with no current query.
        activity.query.clone().map_or(Datum::Null, Datum::Text),
        Datum::Text("client backend".to_owned()),
    ]
}

/// Every `pg_matviews` row: one per materialized view, and no ordinary table.
///
/// **Read off the relations rather than out of a view record**, because a materialized view *is* a
/// table record — which is also why `pg_views`, whose source is the view records, excludes them
/// without being told to (ADR 0064).
fn matviews_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for relation in relations.of_kind(super::pg_relations::RelKind::MaterializedView) {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        let Some(matview) = table.matview.as_ref() else {
            continue;
        };
        rows.push(vec![
            Datum::Text(relation.schema.clone()),
            Datum::Text(relation.name.clone()),
            Datum::Text(String::new()),
            Datum::Text(String::new()),
            // **The indexes a user made on it**, which a materialized view can have and a view
            // cannot — measured, `CREATE UNIQUE INDEX` over one succeeds and flips this to `t`.
            Datum::Bool(!table.indexes.is_empty()),
            Datum::Bool(matview.populated),
            Datum::Text(matview.definition.clone()),
        ]);
    }
    Ok(rows)
}

fn views_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    Ok(super::views(txn, tenant)?
        .into_iter()
        .map(|view| {
            let (schema, name) = super::split_qualified(&view.name);
            vec![
                Datum::Text(schema.to_owned()),
                Datum::Text(name.to_owned()),
                Datum::Text(String::new()),
                Datum::Text(view.definition),
            ]
        })
        .collect())
}

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
/// One `pg_opclass` row per operator class this node accepts.
/// **One row per class this node accepts**, with `opcdefault` `f` for every one —
/// measured, and it is why none of them is what a column gets when no class is
/// written. The oid is this node's own, in the extension range for the same reason
/// hstore's is: a real server allocates an extension's classes at `CREATE EXTENSION`
/// time and a client reads them by `opcname`.
/// Every cast PostgreSQL 19 has whose **source and target are both types this node has** —
/// 100 of its 249, captured to `tests/captures/pg19_pg_cast.txt`. The pairs that are missing
/// are the ones whose ends this node does not have, so a row for one would name a type
/// `pg_type` does not list.
///
/// `(castsource, casttarget, castcontext, castmethod)`. `castcontext` is `e` explicit, `a`
/// assignment, `i` implicit; `castmethod` is `f` a function, `b` binary-coercible, `i` I/O.
pub const CASTS: [(i64, i64, &str, &str); 115] = [
    // **`regclass`'s nine rows, measured** rather than reasoned: `SELECT castsource, casttarget,
    // castcontext, castmethod FROM pg_cast WHERE castsource = 2205 OR casttarget = 2205`. Six
    // types reach a `regclass` implicitly and three leave it — `regclass -> bigint` and
    // `-> integer` are *assignment* casts where `-> oid` is implicit, which is the asymmetry a
    // hand-written list would have flattened.
    (20, 2205, "i", "f"),
    (21, 2205, "i", "f"),
    (23, 2205, "i", "b"),
    (25, 2205, "i", "f"),
    (26, 2205, "i", "b"),
    (1043, 2205, "i", "f"),
    (2205, 20, "a", "f"),
    (2205, 23, "a", "b"),
    (2205, 26, "i", "b"),
    // **`regtype` and `oid` are one representation too**, implicit both ways and by
    // reinterpretation — measured, the same pair `regproc` has (ADR 0098). It is what makes
    // `t::regtype::oid` the 23 a real server answers; the *literal* spelling
    // `'int4'::regtype::oid` was folded at parse time and never asked `pg_cast`.
    (2206, 26, "i", "b"),
    (26, 2206, "i", "b"),
    (16, 23, "e", "f"),
    (16, 25, "a", "f"),
    (16, 1042, "a", "f"),
    (16, 1043, "a", "f"),
    (17, 20, "e", "f"),
    (17, 21, "e", "f"),
    (17, 23, "e", "f"),
    (17, 2950, "e", "f"),
    // **`"char"`'s rows were already here**, taken when the oids were measured rather than when
    // the type was built — the asymmetry is the point and none of it is guessable: `"char"` ->
    // `text` is **implicit** while `text` -> `"char"` is only an assignment, and the `int4` pair is
    // **explicit both ways**, which is what makes `'r'::"char"::int4` a cast a client has to write
    // rather than one that happens.
    (18, 23, "e", "f"),
    (18, 25, "i", "f"),
    (18, 1042, "a", "f"),
    (18, 1043, "a", "f"),
    (19, 25, "i", "f"),
    (19, 1042, "a", "f"),
    (19, 1043, "a", "f"),
    (20, 17, "e", "f"),
    (20, 21, "a", "f"),
    (20, 23, "a", "f"),
    (20, 26, "i", "f"),
    (20, 700, "i", "f"),
    (20, 701, "i", "f"),
    (20, 1700, "i", "f"),
    (21, 17, "e", "f"),
    (21, 20, "i", "f"),
    (21, 23, "i", "f"),
    (21, 26, "i", "f"),
    (21, 700, "i", "f"),
    (21, 701, "i", "f"),
    (21, 1700, "i", "f"),
    (23, 16, "e", "f"),
    (23, 17, "e", "f"),
    (23, 18, "e", "f"),
    (23, 20, "i", "f"),
    (23, 21, "a", "f"),
    (23, 26, "i", "b"),
    (23, 700, "i", "f"),
    (23, 701, "i", "f"),
    (23, 1700, "i", "f"),
    (25, 18, "a", "f"),
    (25, 19, "i", "f"),
    (25, 142, "e", "f"),
    (25, 1042, "i", "b"),
    (25, 1043, "i", "b"),
    // **`regproc` and `oid` are one representation**, implicit both ways and by reinterpretation
    // (`b`) rather than through a function — measured. It is what makes `typinput::oid` the 42 a
    // real server answers, and what puts an integer literal beside a `regproc` in a comparison
    // (ADR 0098).
    (24, 26, "i", "b"),
    (26, 24, "i", "b"),
    (26, 20, "a", "f"),
    (26, 23, "a", "b"),
    (114, 3802, "a", "i"),
    (142, 25, "a", "b"),
    (142, 1042, "a", "b"),
    (142, 1043, "a", "b"),
    (700, 20, "a", "f"),
    (700, 21, "a", "f"),
    (700, 23, "a", "f"),
    (700, 701, "i", "f"),
    (700, 1700, "a", "f"),
    (701, 20, "a", "f"),
    (701, 21, "a", "f"),
    (701, 23, "a", "f"),
    (701, 700, "a", "f"),
    (701, 1700, "a", "f"),
    (1042, 18, "a", "f"),
    (1042, 19, "i", "f"),
    (1042, 25, "i", "f"),
    (1042, 142, "e", "f"),
    (1042, 1042, "i", "f"),
    (1042, 1043, "i", "f"),
    (1043, 18, "a", "f"),
    (1043, 19, "i", "f"),
    (1043, 25, "i", "b"),
    (1043, 142, "e", "f"),
    (1043, 1042, "i", "b"),
    (1043, 1043, "i", "f"),
    (1082, 1114, "i", "f"),
    // **The network pair, measured both ways round and asymmetric**:
    // `SELECT castsource, casttarget, castcontext, castmethod FROM pg_cast WHERE castsource IN
    // (650,869) OR casttarget IN (650,869)`. `cidr -> inet` is **implicit** and by *binary
    // coercion*, `inet -> cidr` is only an **assignment** — which is what makes `inet` the type a
    // `COALESCE` of the two settles on, and the reason `min(cidr)` is an `inet` rather than the
    // other way round. Their four casts to the string types are already licensed by
    // `casts_to`'s I/O rule and are not rows here.
    (650, 869, "i", "b"),
    (869, 650, "a", "f"),
    (1082, 1184, "i", "f"),
    (1083, 1083, "i", "f"),
    (1083, 1186, "i", "f"),
    (1114, 1082, "a", "f"),
    (1114, 1083, "a", "f"),
    (1114, 1114, "i", "f"),
    (1114, 1184, "i", "f"),
    (1184, 1082, "a", "f"),
    (1184, 1083, "a", "f"),
    (1184, 1114, "a", "f"),
    (1184, 1184, "i", "f"),
    (1186, 1083, "a", "f"),
    (1186, 1186, "i", "f"),
    (1700, 20, "a", "f"),
    (1700, 21, "a", "f"),
    (1700, 23, "a", "f"),
    (1700, 700, "i", "f"),
    (1700, 701, "i", "f"),
    (1700, 1700, "i", "f"),
    (2950, 17, "e", "f"),
    (3802, 16, "e", "f"),
    (3802, 20, "e", "f"),
    (3802, 21, "e", "f"),
    (3802, 23, "e", "f"),
    (3802, 114, "a", "i"),
    (3802, 700, "e", "f"),
    (3802, 701, "e", "f"),
    (3802, 1700, "e", "f"),
];

/// The built-in functions this node has, as PostgreSQL numbers them.
///
/// `pg_proc` listed only what `CREATE FUNCTION` made, so `lower` was not in it and
/// `ActiveRecord`'s case-insensitivity probe could not have answered `true` for any type.
/// The oids are PostgreSQL's own and so are the argument lists, which is what lets
/// `proargtypes = ARRAY['text'::regtype]::oidvector` compare true for the right reason —
/// this node already uses PostgreSQL's type oids, so `text` really is 25 on both sides.
///
/// `(oid, name, proargtypes, prorettype, provolatile)`.
pub const BUILTIN_FUNCTIONS: [(i64, &str, &str, i64, &str); 15] = [
    (1705, "abs", "1700", 1700, "i"),
    (1396, "abs", "20", 20, "i"),
    (1398, "abs", "21", 21, "i"),
    (1397, "abs", "23", 23, "i"),
    (1394, "abs", "700", 700, "i"),
    (1395, "abs", "701", 701, "i"),
    (1620, "ascii", "25", 23, "i"),
    (2010, "length", "17", 23, "i"),
    (1317, "length", "25", 23, "i"),
    (870, "lower", "25", 25, "i"),
    (720, "octet_length", "17", 23, "i"),
    (1374, "octet_length", "25", 23, "i"),
    (6382, "reverse", "17", 17, "i"),
    (3062, "reverse", "25", 25, "i"),
    (871, "upper", "25", 25, "i"),
];

/// Whether an **explicit** cast exists between two types.
///
/// Two rules, and the second is the one reasoning misses. Measured on 19beta1:
///
/// ```text
/// pg_cast rows for text -> integer   0      '42'::text::integer      42
/// pg_cast rows for date -> integer   0      '2020-01-01'::date::int  42846 cannot cast type date to integer
/// ```
///
/// **A `pg_cast` row licenses a cast, and so does either side being a string type** — that second
/// one is PostgreSQL's *I/O conversion*, which exists for every pair where one end has a text
/// form to go through, and it is why `text -> integer` works with no row at all. Without it this
/// refused `txt::integer`, which is the commonest cast in any suite.
///
/// The categories come from `typcategory` rather than a list written here, so "which types are
/// strings" is answered once. `castcontext` is not read: explicit is the widest of the three, so
/// any row licenses one.
#[must_use]
pub fn casts_to(from: ColumnType, to: ColumnType) -> bool {
    // A type always casts to itself, which `pg_cast` does not list.
    if from == to || typcategory(from) == "S" || typcategory(to) == "S" {
        return true;
    }
    // **An array casts to an array exactly when its element does**, which no `pg_cast` row says:
    // a real server has no `_int4 -> _int8` row and derives the coercion from the element pair.
    // Measured, four each way — `int4[]::int8[]`, `::text[]`, `::numeric[]` and `::bool[]` all
    // answer, and `int4[]::date[]` and `date[]::int4[]` are `42846`, which is the element rule
    // exactly. It became reachable when `ARRAY(SELECT 1)` started folding to an `int4[]`
    // (ADR 0087) and `ARRAY(SELECT 1)::int8[]` refused itself.
    if let (Some(held), Some(wanted)) = (ArrayValue::element_of(from), ArrayValue::element_of(to)) {
        return casts_to(held, wanted);
    }
    let (from, to) = (i64::from(from.oid()), i64::from(to.oid()));
    CASTS
        .iter()
        .any(|(source, target, _, _)| *source == from && *target == to)
}

/// One `pg_cast` row per cast this node's types have between them.
fn pg_cast_rows() -> Vec<Vec<Datum>> {
    CASTS
        .iter()
        .map(|(source, target, context, method)| {
            vec![
                // The declared type, so the rows and the description agree: an `oid` renders the
                // same digits a `bigint` did, and now compares with a `regtype` as well.
                Datum::Oid(u32::try_from(*source).unwrap_or(0)),
                Datum::Oid(u32::try_from(*target).unwrap_or(0)),
                Datum::Text((*context).to_owned()),
                Datum::Text((*method).to_owned()),
            ]
        })
        .collect()
}

/// The collations this node has: `C`, `POSIX`, and the database's own `default`.
///
/// All three are the **same ordering** — byte order, which is what a memcomparable key gives — and
/// that is why they can be reported honestly
/// ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)). A real
/// server numbers them 950, 951 and 100, and `ActiveRecord` reads the name off this view by
/// joining `a.attcollation = c.oid`, so the oids have to be PostgreSQL's or the join finds nothing.
///
/// `default` is here and is **not** the same ordering it would be on the oracle, where the
/// database's default is a locale: `ORDER BY t` gives `a, A, b, B` there and `A, B, a, b` here.
/// That difference is the pre-existing one this ADR names rather than a new one, and it is
/// declared with its capture line in `tests/corpus/pg19_collation.txt`.
fn pg_collation_rows() -> Vec<Vec<Datum>> {
    [(950_i64, "C"), (951, "POSIX"), (100, "default")]
        .into_iter()
        .map(|(oid, name)| vec![Datum::Int8(oid), Datum::Text(name.to_owned())])
        .collect()
}

fn pg_opclass_rows() -> Vec<Vec<Datum>> {
    super::OPERATOR_CLASSES
        .iter()
        .enumerate()
        .map(|(at, (name, method, ty))| {
            use crate::value::PgType as _;
            vec![
                Datum::Int8(OPCLASS_OID_BASE + i64::try_from(at).unwrap_or(0)),
                Datum::Text((*name).to_owned()),
                Datum::Int8(access_method_by_name(method)),
                Datum::Int8(i64::from(ty.oid())),
                Datum::Bool(false),
            ]
        })
        .collect()
}

/// One row per index access method a `CREATE INDEX` here may name: `btree`, `gin` and `gist`.
///
/// All three are `amtype` `i`, which is what an *index* method is; a real server also has `t` for
/// a table method and six methods in total. The three missing ones — `hash`, `spgist` and `brin` —
/// are refused by `USING`, so a row for one would be a claim rather than a report (ADR 0070).
fn pg_am_rows() -> Vec<Vec<Datum>> {
    [
        (BTREE_AM_OID, "btree"),
        (GIN_AM_OID, "gin"),
        (GIST_AM_OID, "gist"),
    ]
    .into_iter()
    .map(|(oid, name)| {
        vec![
            Datum::Int8(oid),
            Datum::Text(name.to_owned()),
            Datum::Text("i".to_owned()),
        ]
    })
    .collect()
}

fn pg_sequence_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = super::pg_relations::Relations::read(txn, tenant)?;
    // **A sequence no column owns is filed under `STANDALONE_SEQUENCE_OWNER`, which has no
    // `TableDef`**, so walking the tables finds every `bigserial`'s sequence and none of the ones
    // `CREATE SEQUENCE` made. `pg_class` lists them — it reads the name records — and this view
    // did not, which is why `SELECT … FROM pg_sequence WHERE seqrelid = 's1'::regclass` came back
    // empty for a sequence that plainly exists. Read straight from the record, the way
    // `catalog::sequence_by_id` exists to allow.
    let mut standalone = Vec::new();
    for row in relations.of_kind(super::pg_relations::RelKind::Sequence) {
        if row.table_id != super::STANDALONE_SEQUENCE_OWNER {
            continue;
        }
        let id = u64::try_from(row.oid).unwrap_or(0);
        if let Some(sequence) = super::sequence_by_id(txn, tenant, row.table_id, id)? {
            standalone.push(sequence);
        }
    }
    let mut rows = Vec::new();
    for (table, sequence) in relations
        .tables()
        .flat_map(|table| table.sequences.iter().map(move |s| (Some(table), s)))
        .chain(standalone.iter().map(|s| (None, s)))
    {
        // **A serial's sequence counts in the column's type, not always in `bigint`** — a
        // `serial` gets an `integer` sequence whose ceiling is `2147483647`, and a
        // `smallserial` a `smallint` one. Measured on 19beta1, `pg_sequence` for
        // `foo_bar_baz_id_seq` beside the `bigserial` next to it. Nothing new is stored for
        // it: the column the sequence fills is already recorded, and its type is the answer.
        // A standalone `CREATE SEQUENCE` owns no column and is `bigint`, which is what a real
        // server defaults one to.
        let ty = table
            .and_then(|table| sequence.column.and_then(|at| table.columns.get(at)))
            .map_or(ColumnType::Int8, |column| column.ty);
        let (floor, ceiling) = match ty {
            ColumnType::Int2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
            ColumnType::Int4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
            _ => (i64::MIN, i64::MAX),
        };
        // **A descending sequence runs from the type's floor to `-1`**, not from `1` to the
        // ceiling — measured for all three widths, `INCREMENT BY -1` giving
        // `seqmax` `-1` and `seqmin` the type's own minimum. An ascending one bottoms out at
        // `1` whatever the type is, which is the half a `serial` uses.
        let (min, max) = if sequence.increment < 0 {
            (floor, -1)
        } else {
            (1, ceiling)
        };
        rows.push(vec![
            Datum::Int8(super::pg_relations::as_oid(sequence.id)),
            Datum::Int8(i64::from(ty.oid())),
            Datum::Int8(sequence.start),
            Datum::Int8(sequence.increment),
            Datum::Int8(max),
            Datum::Int8(min),
            // `CACHE 1`: a block is reserved by the node and not by the sequence
            // ([ADR 0072](../../../docs/adr/0072-a-sequence-block-belongs-to-the-node-not-to-the-connection.md)),
            // so nothing here caches and a client reading this is told so.
            Datum::Int8(1),
            Datum::Bool(false),
        ]);
    }
    rows.sort_by_key(|row| match row.first() {
        Some(Datum::Int8(oid)) => *oid,
        _ => 0,
    });
    Ok(rows)
}

/// One `pg_enum` row per label of every enum type this tenant has declared.
///
/// **`enumsortorder` is a `real` and it is not the label's index** — it is what PostgreSQL wrote
/// when the label was declared, and `ALTER TYPE … ADD VALUE … BEFORE` puts a new one *between* two
/// existing numbers, which is why the column is a float and not an integer.
///
/// **This node's numbers are always 1, 2, 3 … even after an insert in the middle**, because the
/// value a row stores *is* the label's position: `ALTER TYPE … ADD VALUE … BEFORE` rewrites the
/// rows and renumbers, where a real server writes 1.5 and moves nothing. So the sequence differs
/// and the **order** does not, which is the only thing these numbers encode — the order is the
/// declaration order, never the alphabet (ADR 0050).
///
/// Rows are ordered by type oid and then by that number, which is the order a client reading them
/// without an `ORDER BY` would find least surprising — and `ActiveRecord`'s own `enum_types` query
/// sorts inside `array_agg` anyway, so it does not depend on this.
fn pg_enum_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    for def in super::user_types(txn, tenant)? {
        let super::TypeKind::Enum { labels } = &def.kind else {
            continue;
        };
        let oid = super::pg_relations::as_oid(def.oid);
        for (at, label) in labels.iter().enumerate() {
            rows.push(vec![
                // The declared type, so the rows and the description agree.
                Datum::Oid(u32::try_from(oid).unwrap_or(0)),
                Datum::Text(label.clone()),
                // `at + 1`: a real server's first label is `1`, not `0`.
                Datum::Real(f32::from(u16::try_from(at + 1).unwrap_or(u16::MAX))),
            ]);
        }
    }
    rows.sort_by_key(|row| match (row.first(), row.get(2)) {
        (Some(Datum::Oid(oid)), Some(Datum::Real(order))) => (*oid, order.to_bits()),
        _ => (0, 0),
    });
    Ok(rows)
}

/// Every `pg_range` row: one per range type, built-in or this tenant's own.
///
/// **It was empty until this node had range types**, which the module note above still described.
/// It cannot stay empty now: `ActiveRecord`'s boot query is
/// `pg_type LEFT JOIN pg_range ON oid = rngtypid`, and a range type whose `rngsubtype` comes back
/// NULL is one it does not register — so a `floatrange` column would hand a client the raw text
/// instead of a range, which is the whole of what `range_test.rb` reads back.
///
/// `rngsubtype` is the subtype's **own** oid and not the one this node reads its bounds with:
/// `int4range` reports `integer` on a real server even though every integer here is an `i64`.
/// `rngcanonical` and `rngsubdiff` are not columns of this view — nothing reads them — for the
/// reason `oid` is not one either (see the module note).
fn pg_range_rows(txn: &dyn crate::backend::Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let builtin = [
        (ColumnType::TsRange, ColumnType::Timestamp),
        (ColumnType::TstzRange, ColumnType::TimestampTz),
        (ColumnType::Int4Range, ColumnType::Int4),
        (ColumnType::DateRange, ColumnType::Date),
        (ColumnType::NumRange, ColumnType::Numeric),
        (ColumnType::Int8Range, ColumnType::Int8),
    ];
    let mut rows: Vec<Vec<Datum>> = builtin
        .into_iter()
        .map(|(range, subtype)| {
            vec![
                Datum::Int8(i64::from(range.oid())),
                Datum::Int8(i64::from(subtype.oid())),
            ]
        })
        .collect();
    for def in super::user_types(txn, tenant)? {
        if let super::TypeKind::Range { subtype, .. } = def.kind {
            rows.push(vec![
                Datum::Int8(super::pg_relations::as_oid(def.oid)),
                Datum::Int8(i64::from(subtype.oid())),
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
                // **`typelem` is the element type's OID**, and `typtype` stays `b` for an array
                // too: `b` is "base", as against `r`ange, `e`num, `d`omain and `c`omposite, and
                // an array is none of those.
                Datum::Int8(typelem(*ty)),
                Datum::Text(typdelim(*ty).to_owned()),
                Datum::Text(typinput(*ty).to_owned()),
                Datum::Text(typtype(*ty).to_owned()),
                Datum::Int8(0),
                // **The type's own collation**, which a collatable type has and the rest do not:
                // `100` for `text`, `varchar`, `bpchar` and `citext`, `0` for everything else, as
                // a real server reports. It is the other half of `ActiveRecord`'s read-back — a
                // column that named no collation inherits this number, so
                // `a.attcollation <> t.typcollation` is false for it and the join yields NULL,
                // while a column that named `C` carries 950 and is reported
                // ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)).
                //
                // Leaving this at `0` while `attcollation` answered `100` made every plain `text`
                // column report the collation `default`, which is the wrong half of the same
                // comparison.
                Datum::Int8(super::pg_attribute::typcollation(*ty)),
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
                // Only a domain can constrain its own values.
                Datum::Bool(false),
                Datum::Null,
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
    let schemas = super::schemas(txn, tenant)?;
    let mut rows = Vec::new();
    for def in super::user_types(txn, tenant)? {
        let oid = super::pg_relations::as_oid(def.oid);
        // **The bare name**, the way a relation's `relname` is bare: the schema is `typnamespace`
        // and repeating it here would make `typname = 'text'` fail to find a domain called `text`.
        let (_, bare) = super::split_qualified(&def.name);
        rows.push(vec![
            Datum::Int8(oid),
            Datum::Text(bare.to_owned()),
            // Not an array, so no element type — the array row below is the one with one.
            Datum::Int8(0),
            Datum::Text(",".to_owned()),
            Datum::Text(format!("{bare}_in")),
            Datum::Text(def.kind.typtype().to_owned()),
            // **`typbasetype` is the domain's base type and zero for everything else** — measured,
            // `typbasetype::regtype` over `custom_money` prints `numeric`. It is the column a
            // client reads to learn what a domain is a domain *over*, and the one that told the
            // corpus the answer was `-`: an oid of zero has no `regtype` to print.
            Datum::Int8(match &def.kind {
                super::TypeKind::Domain { base, .. } => i64::from(base.oid()),
                _ => 0,
            }),
            Datum::Int8(0),
            // **The schema the type is actually in**, not a constant. It was `public` while a type
            // could only be there; a domain takes a schema like a relation does (ADR 0065), and
            // `schema_test.rb` creates `schema_1.text` — a domain reported in `public` would be a
            // *wrong* row rather than a missing one.
            Datum::Int8(namespace_oid(&schemas, super::split_qualified(&def.name).0)),
            Datum::Int2(def.kind.typlen()),
            Datum::Text(def.kind.typcategory().to_owned()),
            Datum::Int8(oid + 1),
            // **A composite owns a `pg_class` row and the other two do not.** It is how its
            // fields are stored on a real server, and the one column that tells the three kinds
            // apart beyond their letters.
            Datum::Int8(match def.kind {
                super::TypeKind::Composite { .. } => oid + 2,
                _ => 0,
            }),
            Datum::Bool(matches!(
                def.kind,
                super::TypeKind::Domain { not_null: true, .. }
            )),
            match &def.kind {
                super::TypeKind::Domain {
                    default: Some(text),
                    ..
                } => Datum::Text(text.clone()),
                _ => Datum::Null,
            },
        ]);
        rows.push(vec![
            Datum::Int8(oid + 1),
            Datum::Text(format!("_{bare}")),
            Datum::Int8(oid),
            Datum::Text(",".to_owned()),
            Datum::Text("array_in".to_owned()),
            // An array **of** a user-defined type is a base type: the `r`/`e` belongs to the type
            // it is an array of, not to the array.
            Datum::Text("b".to_owned()),
            Datum::Int8(0),
            Datum::Int8(0),
            Datum::Int8(PUBLIC_NAMESPACE_OID),
            Datum::Int2(-1),
            Datum::Text("A".to_owned()),
            Datum::Int8(0),
            Datum::Int8(0),
            // An array of a domain is not itself a domain: it constrains nothing.
            Datum::Bool(false),
            Datum::Null,
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
    let schemas = super::schema_names(txn, tenant)?;
    let views = CatalogView::ALL.into_iter().map(|view| {
        vec![
            Datum::Int8(i64::try_from(view.id()).unwrap_or(i64::MAX)),
            Datum::Text(view.relname().to_owned()),
            Datum::Int8(namespace_oid(&schemas, view.schema())),
            Datum::Text(view.relkind().to_owned()),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Null,
            Datum::Int8(0),
            // A catalog view is not stored at all, and a real server reports `p` for one.
            Datum::Text(super::Persistence::Permanent.relpersistence().to_owned()),
            // `relispopulated`: `t`. Only a materialized view can be false, and a catalog
            // relation is never one.
            Datum::Bool(true),
        ]
    });
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
                // **The method the index was *declared* with**, which for `USING gin` is `gin`
                // even though what is built underneath is the ordered index every index here is
                // (ADR 0070). `pg_get_indexdef` and `ActiveRecord`'s schema dumper both read it.
                Datum::Int8(
                    relations
                        .table(relation)
                        .zip(relation.index_at)
                        .and_then(|(table, at)| table.indexes.get(at))
                        .map_or_else(
                            || access_method_oid(relation.kind),
                            |index| access_method_by_name(&index.access_method),
                        ),
                ),
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
                // `relispopulated`: **only a materialized view can be false**, and it is false
                // only between `WITH NO DATA` and the first `REFRESH`. Everything else — an
                // ordinary table, an index, a view — is `t` on a real server.
                Datum::Bool(
                    relations
                        .table(relation)
                        .and_then(|table| table.matview.as_ref())
                        .is_none_or(|matview| matview.populated),
                ),
            ]
        })
        .chain(views)
        .collect())
}

/// PostgreSQL's own oid for the `btree` access method, which is a fixed catalog id there.
/// The text-search configurations this node has, in the order `pg_ts_config` reports them.
///
/// Two, not thirty-two: see [`CatalogView::PgTsConfig`]. `simple` needs no stemmer at all —
/// lowercase, split, keep everything — and `english` is the one the suite's index expression
/// names.
const TS_CONFIGS: &[(i64, &str)] = &[(1, "english"), (2, "simple")];

/// `pg_ts_config`'s rows.
///
/// **Both live in `pg_catalog`**, which is where a real server puts them, so the capture's join to
/// `pg_namespace` finds them.
///
/// The oids are this node's own. PostgreSQL fixes `simple`'s in its catalog headers but creates
/// the language configurations at `initdb` time, so `english`'s is not a constant anywhere and
/// inventing one would be a claim. Nothing in the suite or the capture reads either.
fn pg_ts_config_rows() -> Vec<Vec<Datum>> {
    TS_CONFIGS
        .iter()
        .map(|(oid, name)| {
            vec![
                Datum::Int8(*oid),
                Datum::Text((*name).to_owned()),
                Datum::Int8(super::pg_relations::as_oid(super::RESERVED_SCHEMAS[0].1)),
            ]
        })
        .collect()
}

const BTREE_AM_OID: i64 = 403;

/// PostgreSQL's own oid for `gist`, likewise fixed.
const GIST_AM_OID: i64 = 783;

/// PostgreSQL's own oid for `gin`, likewise fixed.
const GIN_AM_OID: i64 = 2742;

/// Where this node's operator-class oids start.
///
/// **Not PostgreSQL's own**, and deliberately: `gin_trgm_ops` is an extension's class and its oid
/// is allocated at `CREATE EXTENSION` time on a real server, so the number differs per database
/// and a client reads the class by `opcname` — which is what `ActiveRecord`'s schema dumper does.
/// The same call `HSTORE_OID` made, one catalog over.
const OPCLASS_OID_BASE: i64 = 16500;

/// The oid of an access method by the name an index was declared with (ADR 0070).
fn access_method_by_name(name: &str) -> i64 {
    match name {
        "gin" => GIN_AM_OID,
        "gist" => GIST_AM_OID,
        _ => BTREE_AM_OID,
    }
}

/// `pg_class.relam`: the access method an index is built with, and **zero** for anything that is
/// not an index — which is what a real server reports for a table, so a join to `pg_am` drops it.
fn access_method_oid(kind: super::pg_relations::RelKind) -> i64 {
    use super::pg_relations::RelKind;
    match kind {
        RelKind::Index | RelKind::PrimaryKey => BTREE_AM_OID,
        RelKind::Exclusion => GIST_AM_OID,
        // A view is not built with an access method either, so a join to `pg_am` drops it.
        RelKind::Table | RelKind::Sequence | RelKind::View | RelKind::MaterializedView => 0,
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
#[expect(
    clippy::too_many_lines,
    reason = "one match over the whole type vocabulary, and it is a list of names \
              rather than of rules; splitting it would put half the vocabulary somewhere \
              else and let a type be added to one half without the other"
)]
pub(crate) fn typname(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::RegType => "regtype",
        ColumnType::RegProc => "regproc",
        ColumnType::RegClass => "regclass",
        ColumnType::Int2Vector => "int2vector",
        ColumnType::OidVector => "oidvector",
        ColumnType::RegTypeArray => "_regtype",
        ColumnType::RegProcArray => "_regproc",
        ColumnType::RegClassArray => "_regclass",
        // **An array type's internal name is the element's with a leading underscore** — `_int4`,
        // not `int4[]`. That spelling is what `pg_type.typname` holds on a real server and what a
        // client matching on it expects.
        ColumnType::Int8Array => "_int8",
        ColumnType::Int4Array => "_int4",
        ColumnType::Int2Array => "_int2",
        ColumnType::NumericArray => "_numeric",
        ColumnType::TextArray => "_text",
        ColumnType::Int8 => "int8",
        ColumnType::Int4 => "int4",
        ColumnType::Int2 => "int2",
        ColumnType::Text => "text",
        ColumnType::Varchar => "varchar",
        ColumnType::Name => "name",
        ColumnType::Bpchar => "bpchar",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Xml => "xml",
        ColumnType::XmlArray => "_xml",
        ColumnType::Ltree => "ltree",
        ColumnType::LtreeArray => "_ltree",
        ColumnType::LQuery => "lquery",
        ColumnType::Hstore => "hstore",
        ColumnType::TsVector => "tsvector",
        ColumnType::TsQuery => "tsquery",
        ColumnType::Citext => "citext",
        ColumnType::TsRange => "tsrange",
        ColumnType::TstzRange => "tstzrange",
        ColumnType::Int4Range => "int4range",
        ColumnType::DateRange => "daterange",
        ColumnType::NumRange => "numrange",
        ColumnType::Int8Range => "int8range",
        // **An array type's internal name is the element's with a leading underscore**, and this
        // one is `_money` on a real server — measured beside the type it is over.
        ColumnType::Money => "money",
        ColumnType::MoneyArray => "_money",
        ColumnType::Inet => "inet",
        ColumnType::Cidr => "cidr",
        ColumnType::MacAddr => "macaddr",
        ColumnType::InetArray => "_inet",
        ColumnType::CidrArray => "_cidr",
        ColumnType::MacAddrArray => "_macaddr",
        // **`varbit`, not `bit varying`** — `typname` is the internal name and the two differ for
        // this type as they do for `int8`/`bigint`.
        ColumnType::Lseg => "lseg",
        ColumnType::Box => "box",
        ColumnType::Path => "path",
        ColumnType::Polygon => "polygon",
        ColumnType::Circle => "circle",
        ColumnType::Line => "line",
        ColumnType::Bit => "bit",
        ColumnType::VarBit => "varbit",
        ColumnType::BitArray => "_bit",
        ColumnType::VarBitArray => "_varbit",
        // **No row of their own.** These two are the representation a user-defined range type
        // gets, and its `pg_type` row is written by `user_type_rows` under the name the
        // `CREATE TYPE` gave it — `ColumnType::ALL`, which is what this view iterates, leaves
        // them out for exactly that reason. What is here is the fallback a debugger sees.
        ColumnType::FloatRange => "float8range",
        ColumnType::VarcharRange => "varcharrange",
        ColumnType::Point => "point",
        ColumnType::PointArray => "_point",
        ColumnType::BoxArray => "_box",
        ColumnType::LsegArray => "_lseg",
        ColumnType::PathArray => "_path",
        ColumnType::PolygonArray => "_polygon",
        ColumnType::CircleArray => "_circle",
        ColumnType::LineArray => "_line",
        ColumnType::TstzRangeArray => "_tstzrange",
        ColumnType::Int4RangeArray => "_int4range",
        ColumnType::DateRangeArray => "_daterange",
        ColumnType::NumRangeArray => "_numrange",
        ColumnType::Int8RangeArray => "_int8range",
        ColumnType::TsRangeArray => "_tsrange",
        ColumnType::BoolArray => "_bool",
        ColumnType::ByteaArray => "_bytea",
        ColumnType::BpcharArray => "_bpchar",
        ColumnType::VarcharArray => "_varchar",
        ColumnType::NameArray => "_name",
        // **`typname` is `char`, unquoted** — the quotes belong to how it is *written* and to what
        // `format_type` prints, not to the catalog's own spelling. Measured.
        ColumnType::Char => "char",
        ColumnType::CharArray => "_char",
        ColumnType::Void => "void",
        ColumnType::DateArray => "_date",
        ColumnType::TimeArray => "_time",
        ColumnType::TimestampArray => "_timestamp",
        ColumnType::TimestampTzArray => "_timestamptz",
        ColumnType::IntervalArray => "_interval",
        ColumnType::RealArray => "_float4",
        ColumnType::DoubleArray => "_float8",
        ColumnType::UuidArray => "_uuid",
        ColumnType::JsonArray => "_json",
        ColumnType::JsonbArray => "_jsonb",
        ColumnType::OidArray => "_oid",
        ColumnType::CitextArray => "_citext",
        ColumnType::HstoreArray => "_hstore",
        ColumnType::TsVectorArray => "_tsvector",
        ColumnType::TsQueryArray => "_tsquery",
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

/// `pg_type.typtype`: **`r` for a range** and `b` for a base type.
///
/// Measured: `tsrange`, `tstzrange` and `int4range` are `r` while `_tsrange` — the array — is `b`,
/// which is the pair a reader would get wrong. An enum's is `e` and lives on its `TypeDef`
/// (ADR 0050); this is only the types the column vocabulary has.
fn typtype(ty: ColumnType) -> &'static str {
    match ty {
        // The one pseudo-type, measured: `void` is `p` where every storage type here is `b`.
        ColumnType::Void => "p",
        ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange => "r",
        // An array **of** a money is a base type, as every array here is: the `N` belongs to the
        // element and the `A` below to the array.
        _ => "b",
    }
}

/// `pg_type.typcategory`: PostgreSQL's coarse grouping of types, one character each.
///
/// Measured on 19beta1 for all eighteen rather than reasoned about, because the groupings are not
/// what a reader would guess: `bytea` is `U` (user-defined) and not `S` (string), a `uuid` is `U`
/// too, and all three datetime types are `D` while an `interval` is `T`. An exhaustive match, so
/// a type added here has to answer instead of inheriting somebody else's letter.
pub(crate) fn typcategory(ty: ColumnType) -> &'static str {
    match ty {
        // **`Z` is `"char"`'s own group**, and it is not `S` with the strings. That one letter is
        // why `'r'::"char" = 'r'::text` is `t` and a `CASE` over the two is `42804`: `unify`
        // refuses across categories, so they compare and have no common type — measured, both
        // halves.
        ColumnType::Char => "Z",
        // **`P` is the pseudo-type group and `void` is this vocabulary's only member.** Measured
        // beside `typtype = 'p'`: the pair is what says a type is not storage, and it is why no
        // column may be declared one and why its `typarray` is 0.
        ColumnType::Void => "P",
        // A `regtype` sits in the `N` group below beside the `oid` it is, and its array in `A`
        // with every other array — which is the point of the model (ADR 0077) rather than an
        // exception to it.
        ColumnType::Int8
        | ColumnType::Int4
        | ColumnType::Int2
        | ColumnType::Double
        | ColumnType::Real
        | ColumnType::Numeric
        // A number, and PostgreSQL groups it with them despite being an identifier.
        | ColumnType::Oid
        | ColumnType::RegType
        | ColumnType::RegProc
        | ColumnType::RegClass
        // **And a money**, which a real server puts here too — not in `U` with the extension
        // types and not in a category of its own. Measured.
        | ColumnType::Money => "N",
        // **`I` for the two addresses**, which is not what a reader would guess: PostgreSQL has a
        // network-address category and puts only `inet` and `cidr` in it — a `macaddr` joins the
        // `U` group below. Measured off `pg_type.typcategory`.
        ColumnType::Inet | ColumnType::Cidr => "I",
        // **`V`, a category of its own** — not `S` with the strings, measured.
        ColumnType::Bit | ColumnType::VarBit => "V",
        // **`S` for citext too**, measured: it is a string type to the adapter, which is how it
        // is told apart from hstore's `U` in the boot type-map query.
        ColumnType::Text
        | ColumnType::Varchar
        | ColumnType::Name
        | ColumnType::Bpchar
        | ColumnType::Citext => "S",
        ColumnType::Bool => "B",
        ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Date | ColumnType::Time => {
            "D"
        }
        // **`U` for hstore**, measured: the adapter's boot type-map query reads this column
        // and an extension type is "user" rather than "string".
        ColumnType::Bytea
        | ColumnType::Json
        | ColumnType::Jsonb
        // **And an `xml`**, which is `U` and not `S`: it is a string to a reader and a
        // user-defined type to `pg_type`. Measured.
        | ColumnType::Xml
        // **`U` for `ltree` too**, measured — an extension type is "user" rather than "string",
        // the same answer `hstore` gets and the one the adapter's boot query reads.
        | ColumnType::Ltree
        // `U` too, measured beside `ltree`'s.
        | ColumnType::LQuery
        | ColumnType::Hstore
        | ColumnType::TsVector
        | ColumnType::TsQuery
        | ColumnType::Uuid
        // **And a `macaddr`**, which a real server groups with them rather than with the two
        // addresses it looks like. Measured.
        | ColumnType::MacAddr => "U",
        // `T` for timespan, which is its own category and not the datetimes' `D`.
        ColumnType::Interval => "T",
        // `A` for array, whatever the elements are — the category is the constructor's, not the
        // element type's.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::HstoreArray
        | ColumnType::TsVectorArray
        | ColumnType::TsQueryArray
        | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::RegClassArray | ColumnType::CitextArray | ColumnType::MoneyArray | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray | ColumnType::BitArray | ColumnType::VarBitArray | ColumnType::XmlArray | ColumnType::LtreeArray
        // **`A` for the two vectors too**, measured: `int2vector` and `oidvector` are in
        // PostgreSQL's array category despite not being array types.
        | ColumnType::Int2Vector
        | ColumnType::OidVector => "A",
        // **`R` for a range**, its own category — measured, and not `U` the way hstore is.
        // **`G` for geometric**, which is neither the `U` an extension type gets nor the
        // `S` a string does. Measured off `pg_type.typcategory`, all seven.
        ColumnType::Point
        | ColumnType::Lseg
        | ColumnType::Box
        | ColumnType::Path
        | ColumnType::Polygon
        | ColumnType::Circle
        | ColumnType::Line => "G",
        ColumnType::TsRange | ColumnType::TstzRange | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
        | ColumnType::FloatRange | ColumnType::VarcharRange => "R",
        // **`N`, with the numbers**, which is where a real server puts it — not `U`, where the
        // extension types are, and not a category of its own. Measured.

    }
}

/// `pg_type.typinput`: the name of the type's input function. A `regproc` on a real server and
/// `text` here, with the same characters in it.
///
/// `ActiveRecord` reads it, and reads it by comparison — `row["typinput"] == "array_in"` is how it
/// tells an array type from everything else — so the value is load-bearing and the spelling is the
/// capture's, underscore and all: `timestamptz_in` has one and `int8in` does not.
/// `pg_type.typelem`: what one element of this type is, and **not only for an array**.
///
/// An array's element is the obvious half, and it was the only half here. The other is measured:
/// a real server sets `typelem` on the fixed-length types that are *internally* an array of
/// something — `point` and `line` are two `float8`s, `lseg` and `box` are two `point`s,
/// `int2vector` is `int2`s and `oidvector` is `oid`s. `path`, `polygon` and `circle` are
/// variable-length and report `0`, which is why this is a list and not a rule about shapes.
///
/// **`typelem` does not say a row is an array — `typinput` does.** That is
/// `array_delimiter.rs::every_base_type_has_an_array_or_is_listed`'s own finding: written as
/// `typelem = 0` its guard passed only because this node reported `box`'s as zero, so it was
/// resting on this divergence rather than on the rule. The guard was corrected there and the
/// value is corrected here.
///
/// Harmless to the client that reads it, and worth being right about anyway: `ActiveRecord` sorts
/// a `pg_type` row by name and by `typinput` long before it looks at `typelem`, and all six of
/// these are registered by name — which is why copying the oracle's value cannot change what the
/// adapter does with it. What it does change is that the six now answer the adapter's *array*
/// query, `WHERE typelem IN (…)`, exactly as they do on a real server
/// (`tests/pg_catalog.rs`'s query 9).
///
/// **`name` points at `char` (18)**, because a real server's `name` *is* an array of them — a
/// 64-byte one — and 18 is now a row that is there. It was a zero for exactly as long as the type
/// was missing: a `typelem` naming an absent row is what `array_delimiter.rs::no_typarray_dangles`
/// forbids one column over, so the gap was the honest answer and stopped being one the day
/// `"char"` arrived (ADR 0095). `name`'s own array (`_name`, 1003) was the same gap and closed
/// first ([ADR 0086](../../../docs/adr/0086-a-folded-cast-keeps-the-type-it-named.md)).
fn typelem(ty: ColumnType) -> i64 {
    if let Some(element) = ArrayValue::element_of(ty) {
        return i64::from(element.oid());
    }
    let element = match ty {
        ColumnType::Point | ColumnType::Line => ColumnType::Double,
        ColumnType::Lseg | ColumnType::Box => ColumnType::Point,
        ColumnType::Int2Vector => ColumnType::Int2,
        ColumnType::OidVector => ColumnType::Oid,
        // **A `name` is 64 `"char"`s**, which is the one non-array type whose element is a
        // scalar the row codec also stores. Measured: `pg_type.typelem` for `name` is 18.
        ColumnType::Name => ColumnType::Char,
        _ => return 0,
    };
    i64::from(element.oid())
}

/// `pg_type.typdelim`: the character that separates two elements inside an array literal.
///
/// **A comma for every type but one.** `box` uses a **semicolon**, because a box's own text
/// already contains commas — `(1,1),(0,0)` — so `{(1,1),(0,0);(3,3),(2,2)}` is a two-element
/// `box[]` and the same string with commas would be four points. Measured the only way that
/// settles it, by asking a real server which types disagree with the default:
/// `SELECT typname, typdelim FROM pg_type WHERE typdelim <> ','` answers `box` and `_box`, and
/// nothing else.
///
/// The `,` was a constant here before, which is a right answer for 77 types and a wrong one for
/// the seventy-eighth.
fn typdelim(ty: ColumnType) -> &'static str {
    // **An array's delimiter is its element's**, which is a rule and not a special case: `_box`
    // is the only array type in all of `pg_type` whose delimiter is not a comma, and it is one
    // because `box` is. Measured by asking a real server for every array type whose
    // `typdelim <> ','` and getting exactly that one row.
    if let Some(element) = ArrayValue::element_of(ty) {
        return typdelim(element);
    }
    match ty {
        // A `box` is written `(x1,y1),(x2,y2)`, so a comma cannot separate two of them.
        ColumnType::Box => ";",
        _ => ",",
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one match over the whole type vocabulary, and it is a list of names \
              rather than of rules; splitting it would put half the vocabulary somewhere \
              else and let a type be added to one half without the other"
)]
fn typinput(ty: ColumnType) -> &'static str {
    match ty {
        // Measured, and it exists even though nothing calls it: a pseudo-type still carries an
        // input function on a real server.
        ColumnType::Void => "void_in",
        ColumnType::Char => "charin",
        // PostgreSQL's own name; the array's `array_in` is in the group below with every other.
        ColumnType::RegType => "regtypein",
        ColumnType::RegProc => "regprocin",
        ColumnType::RegClass => "regclassin",
        ColumnType::Int2Vector => "int2vectorin",
        ColumnType::OidVector => "oidvectorin",
        // **`array_in` for every array type**, and this one value is load-bearing beyond the
        // catalog: `ActiveRecord` decides that a column is an array by comparing this string, and
        // a column it does not know to be an array is what makes it hand a Ruby `Array` to
        // `quote` and fail client-side with `can't quote Array`.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::HstoreArray
        | ColumnType::TsVectorArray
        | ColumnType::TsQueryArray
        | ColumnType::TsRangeArray
        | ColumnType::TstzRangeArray
        | ColumnType::Int4RangeArray
        | ColumnType::DateRangeArray
        | ColumnType::NumRangeArray
        | ColumnType::Int8RangeArray
        | ColumnType::PointArray
        | ColumnType::BoxArray
        | ColumnType::LsegArray
        | ColumnType::PathArray
        | ColumnType::PolygonArray
        | ColumnType::CircleArray
        | ColumnType::LineArray
        | ColumnType::BoolArray
        | ColumnType::ByteaArray
        | ColumnType::BpcharArray
        | ColumnType::VarcharArray
        | ColumnType::NameArray
        | ColumnType::CharArray
        | ColumnType::DateArray
        | ColumnType::TimeArray
        | ColumnType::TimestampArray
        | ColumnType::TimestampTzArray
        | ColumnType::IntervalArray
        | ColumnType::RealArray
        | ColumnType::DoubleArray
        | ColumnType::UuidArray
        | ColumnType::JsonArray
        | ColumnType::JsonbArray
        | ColumnType::OidArray
        | ColumnType::RegTypeArray
        | ColumnType::RegProcArray
        | ColumnType::RegClassArray
        | ColumnType::CitextArray
        | ColumnType::MoneyArray
        | ColumnType::InetArray
        | ColumnType::CidrArray
        | ColumnType::MacAddrArray
        | ColumnType::BitArray
        | ColumnType::VarBitArray
        | ColumnType::XmlArray
        | ColumnType::LtreeArray => "array_in",
        ColumnType::Int8 => "int8in",
        ColumnType::Int4 => "int4in",
        ColumnType::Int2 => "int2in",
        ColumnType::Text => "textin",
        ColumnType::Varchar => "varcharin",
        // Its own input function, and the one that truncates at 63 (`value::truncate_to_name`).
        ColumnType::Name => "namein",
        ColumnType::Bpchar => "bpcharin",
        ColumnType::Json => "json_in",
        ColumnType::Jsonb => "jsonb_in",
        ColumnType::Xml => "xml_in",
        // `ltree_in`, the name the adapter reads to decide the type is an ltree.
        ColumnType::Ltree => "ltree_in",
        ColumnType::LQuery => "lquery_in",
        // `hstore_in`, which is the name the adapter reads to decide the type is hstore.
        ColumnType::Hstore => "hstore_in",
        ColumnType::TsVector => "tsvectorin",
        ColumnType::TsQuery => "tsqueryin",
        ColumnType::Point => "point_in",
        // Not `money_in`: the input function is named for the C type behind it.
        ColumnType::Money => "cash_in",
        ColumnType::Inet => "inet_in",
        ColumnType::Cidr => "cidr_in",
        ColumnType::MacAddr => "macaddr_in",
        ColumnType::Lseg => "lseg_in",
        ColumnType::Box => "box_in",
        ColumnType::Path => "path_in",
        // **`poly_in`, not `polygon_in`** — the input function is named for the C type, the way
        // `money`'s is `cash_in`. Measured.
        ColumnType::Polygon => "poly_in",
        ColumnType::Circle => "circle_in",
        ColumnType::Line => "line_in",
        ColumnType::Bit => "bit_in",
        ColumnType::VarBit => "varbit_in",
        // **`range_in` for every range**, measured: a real server's `int4range` has
        // `typinput = range_in`, not `int4range_in` — the input function belongs to the range
        // *machinery* and reads the subtype out of `pg_range`, which is why one function serves
        // all of them.
        //
        // The rule was here for the two user-defined ranges and not for the six built-in ones,
        // which had a name each. Found by diffing this whole table against the oracle's, column
        // by column (`scratchpad/pgtype-audit.py`), rather than by anybody reading it. The user
        // ranges have no row of their own here (see `typname`); `user_type_rows` is where a
        // `floatrange` gets one.
        ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange => "range_in",
        ColumnType::Citext => "citextin",
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

#[cfg(test)]
mod tests {
    use super::CatalogView;

    /// **Every view's reserved relation id is its own**, and this test exists because that has now
    /// failed three times.
    ///
    /// The ids are a hand-maintained table, and two lanes adding a view in parallel cannot see each
    /// other: the arrays merge cleanly and the numbers collide in silence. Two views sharing an id
    /// **resolve to each other**, so what breaks is not the new view but some *other* one — which is
    /// how it was found each time. `pg_views` taking `pg_sequence`'s id turned eight unrelated tests
    /// red; `pg_locks` taking `information_schema.domains`'s id emptied that view in another lane's
    /// corpus, and neither lane could have seen it coming.
    ///
    /// A duplicated name resolves the same way and costs the same to check, so both are here.
    #[test]
    fn no_two_views_share_a_relation_id_or_a_name() {
        let mut by_id: std::collections::BTreeMap<u64, &str> = std::collections::BTreeMap::new();
        let mut by_name: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
        for view in CatalogView::ALL {
            let (id, name) = (view.id(), view.name());
            // `insert` hands back what was there **before**, which is the other view's name. Read
            // the map after inserting and the message names the new view twice, which is a
            // failure that tells you nothing about who it collided with.
            if let Some(other) = by_id.insert(id, name) {
                panic!(
                    "{name} and {other} both reserve relation id {id}; they resolve to each other"
                );
            }
            if let Some(other) = by_name.insert(name, id) {
                panic!("two views are both named {name}, with ids {id} and {other}");
            }
        }
        assert_eq!(by_id.len(), CatalogView::ALL.len());
    }
}
