//! The four DDL statements phase 6a executes, lowered.
//!
//! Names here are already folded and truncated ([`crate::catalog::fold_identifier`]), because a
//! lowered statement is what the executor acts on and a name that still needed folding would be
//! one the catalog could be asked about under two spellings.
//!
//! The default constraint and index names are PostgreSQL's own, read off a server rather than
//! remembered: `<table>_pkey` for a primary key, `<table>_<column>_key` for a `UNIQUE` constraint,
//! and `<table>_<column>…_idx` for a `CREATE INDEX` with no name. They matter because they are
//! what a `23505` message quotes back, and a client that matches on the constraint name would not
//! recognise ours if we invented them.

use crate::catalog::{
    ExprShape, Identity, KeyOrder, MAX_IDENTIFIER_BYTES, ReferentialAction, fold_identifier,
};
use crate::value::{ColumnType, Datum};

/// `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    /// The table's name, folded.
    pub name: String,
    /// `IF NOT EXISTS`: an existing table is a notice rather than a `42P07`.
    pub if_not_exists: bool,
    /// In declaration order, which is the order a row's values are stored in.
    pub columns: Vec<Column>,
    /// Primary key column names, in key order. Empty when the table has none.
    pub primary_key: Vec<String>,
    /// The name a `CONSTRAINT ... PRIMARY KEY` gave it, or `None` for PostgreSQL's derived
    /// `<table>_pkey`. Carried rather than dropped: a constraint the user named is the name a
    /// `23505` will quote back at them.
    pub primary_key_name: Option<String>,
    /// The `EXCLUDE` constraints, re-attached after the parser was handed a statement without
    /// them (`crate::parse::strip_exclude_constraints`).
    pub excludes: Vec<crate::catalog::ExcludeDef>,
    /// `UNLOGGED`, re-attached the same way and for the same reason — the parser cannot read the
    /// keyword, so it is cut out of the source and put back here.
    pub persistence: crate::catalog::Persistence,
    /// `ON COMMIT PRESERVE ROWS | DELETE ROWS | DROP` — a temporary table's own clause, and
    /// `42P16` on any other kind of table.
    pub on_commit: crate::catalog::OnCommit,
    /// `PARTITION BY LIST (col, …)` — the strategy and the key columns' names, unresolved.
    pub partition_by: Option<(crate::catalog::PartitionStrategy, Vec<String>)>,
    /// `PARTITION OF parent FOR VALUES IN (…)` / `… DEFAULT` — the parent's name and the bound as
    /// written, both unresolved.
    ///
    /// The bound's values are **expressions here and values in the catalog**: coercing them needs
    /// the parent's key columns, and a plan is lowered without the catalog. That coercion is
    /// visible — the suite writes `IN (1)` against a `character varying` key and a real server
    /// prints `FOR VALUES IN ('1')` back.
    pub partition_of: Option<(String, PartitionSpec)>,
    /// `INHERITS (parent, …)` — the parents' names, in the order written, unresolved.
    ///
    /// Resolved by the executor, which is where the catalog is: a parent's columns are prepended
    /// to this table's own and cannot be known until it has been read.
    pub inherits: Vec<String>,
    /// Every `UNIQUE` constraint, from a column option or a table constraint.
    pub unique: Vec<UniqueConstraint>,
    /// Every `CHECK`, in the order written — which is the order a derived name is numbered in.
    pub checks: Vec<CheckConstraint>,
    /// Every `FOREIGN KEY`, from a column option (`p int8 REFERENCES t`) or a table constraint.
    ///
    /// Resolved by the executor rather than here, for the reason a `CREATE INDEX`'s key parts are:
    /// the parent is a name until the catalog has been read.
    pub foreign_keys: Vec<ForeignKey>,
}

/// One declared column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Folded.
    pub name: String,
    /// `GENERATED ALWAYS AS (expr) STORED`: the expression, normalised the way a `CHECK` is.
    ///
    /// A generated column is **not** a defaulted one: a writer may not supply a value for it at
    /// all, and the catalog reports it in a different place (`catalog::ColumnDef::generated`).
    pub generated: Option<String>,
    /// Whether it was declared `VIRTUAL` rather than `STORED` — storage, not answers.
    pub generated_virtual: bool,
    /// The type it was declared as.
    ///
    /// For a column declared as a **user-defined type** this is what the value physically is —
    /// `int2` for an enum's ordinal — and it is a placeholder until the executor has read the
    /// catalog. [`Column::user_type_name`] is the name that decides it.
    pub ty: ColumnType,
    /// The **name** of a user-defined type this column was declared as, unresolved.
    ///
    /// Unresolved for the same reason `partition_of`'s bound and `inherits`' parents are: reading
    /// it needs the catalog, and a plan is lowered without one. The executor turns it into
    /// `catalog::ColumnDef::user_type` and settles `ty` at the same time, and it is where the
    /// `0A000 the type <name> is not supported` for a name nobody declared comes from — lowering
    /// can no longer tell a user type from a typo, because only the catalog knows.
    pub user_type_name: Option<String>,
    /// PostgreSQL's `atttypmod` for the declaration, or `crate::value::NO_TYPMOD`. See
    /// `crate::catalog::ColumnDef::typmod`, which is where it comes to rest.
    pub typmod: i32,
    /// Whether `NOT NULL` was declared. A primary key column becomes `NOT NULL` whether or not it
    /// said so, which the executor applies.
    pub not_null: bool,
    /// A default that stays an **expression**, evaluated per row, or `None` for a folded or
    /// absent one (`crate::catalog::ColumnDef::default_expr`).
    pub default_expr: Option<String>,
    /// `DEFAULT <constant>`, already read as a value of the column's own type.
    ///
    /// The **folded** half of a default, set for exactly what PostgreSQL's coercion folds to a
    /// constant: a literal, read as this column's type. Everything else is above, as text.
    pub default: Option<Datum>,
    /// The collation the column was declared with, or `None` for the type's own.
    ///
    /// `C` or `POSIX` only
    /// ([ADR 0076](../../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)); every
    /// other name is refused in the lowerer, so what reaches here is always an ordering this node
    /// actually has.
    pub collation: Option<String>,
    /// The sequence that fills this column — `bigserial` or `GENERATED ... AS IDENTITY` — and
    /// which of the three it is.
    ///
    /// All three serial spellings are among them, since [ADR
    /// 0033](../../../../docs/adr/0033-tier-1-of-the-type-surface.md) gave this node the integers
    /// they stand for: `smallserial` is an `int2`, `serial` an `int4` and `bigserial` an `int8`,
    /// each plus a sequence. Before `int4` existed, `serial` was `0A000` rather than an `int8` in
    /// disguise, which would have accepted every value between 2^31 and 2^63 that a real server
    /// refuses with `22003`.
    pub sequence: Option<Identity>,
}

/// `ALTER INDEX [IF EXISTS] <name> RENAME TO <name>`.
///
/// **The one `ALTER INDEX` form `ActiveRecord` sends**, from two places: `rename_index` renames any
/// index, and `rename_table` follows a table rename with the primary key's index because
/// PostgreSQL does not rename it for you (`postgresql/schema_statements.rb:590,467`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterIndexRename {
    /// The index as it is now, schema qualification already resolved away.
    pub name: String,
    /// What it becomes.
    pub to: String,
    /// `IF EXISTS`: a name nothing answers to is a notice rather than `42P01`.
    pub if_exists: bool,
}

/// `ADD CONSTRAINT [name] UNIQUE USING INDEX <index> [DEFERRABLE …]`.
///
/// The index supplies the columns, so there is no column list to lower — which is why this is not
/// a [`UniqueConstraint`] with an extra field: that struct is also `CREATE TABLE`'s, where the
/// spelling does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueUsingIndex {
    /// The constraint's name, or `None` to take the index's.
    pub name: Option<String>,
    /// The existing index being promoted.
    pub index: String,
    /// `DEFERRABLE`.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED`.
    pub deferred: bool,
}

/// A `UNIQUE` constraint, which becomes a unique index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// The name it was given, or `None` for PostgreSQL's derived one.
    pub name: Option<String>,
    /// Column names, in key order.
    pub columns: Vec<String>,
    /// `NULLS NOT DISTINCT`: two NULLs collide, so **one** row may have a NULL there and no more.
    ///
    /// The consequence catches a reader out: an `INSERT` that never mentions the column still
    /// collides with the first, because omitting it writes a NULL.
    pub nulls_not_distinct: bool,
    /// `DEFERRABLE`, in either of its two initial modes.
    ///
    /// `INITIALLY IMMEDIATE` checks at the statement like any other unique constraint; what
    /// differs is `condeferrable` and what `pg_get_constraintdef` prints, which keeps `DEFERRABLE`
    /// and drops the `INITIALLY IMMEDIATE` half, so the text out is not the text in.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED`: the check waits for `COMMIT` (`crate::exec::deferred`).
    ///
    /// Never true without [`UniqueConstraint::deferrable`] — `INITIALLY DEFERRED` implies
    /// `DEFERRABLE` in the grammar, and a constraint that is not deferrable cannot be deferred by
    /// `SET CONSTRAINTS` either.
    pub deferred: bool,
}

/// `DROP EXTENSION [IF EXISTS] <name> [CASCADE|RESTRICT]`.
///
/// **What the suite's teardown sends**, always in the `IF EXISTS` form and with `CASCADE` when
/// `disable_extension(name, force: :cascade)` asks for it (`postgresql_adapter.rb:503`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropExtension {
    /// The extension's name, folded.
    pub name: String,
    /// `IF EXISTS`: a name nothing has installed is a **notice** rather than `42704`.
    pub if_exists: bool,
    /// `CASCADE`: also drop what depends on the extension's types. Without it a column of one is
    /// `2BP01` — measured, and the case the capture does not reach.
    pub cascade: bool,
}

/// `CREATE EXTENSION [IF NOT EXISTS] name [[WITH] SCHEMA schema]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateExtension {
    /// The extension's name, as written — **case-sensitively**, because `ActiveRecord` quotes it
    /// (`CREATE EXTENSION IF NOT EXISTS "uuid-ossp"`) and a hyphenated name has to survive.
    pub name: String,
    /// `IF NOT EXISTS`: an extension already installed is a notice rather than a `42710`.
    ///
    /// It covers **existence only**. An extension this build does not have is `0A000 … is not
    /// available` with or without the clause — measured, both spellings.
    pub if_not_exists: bool,
    /// `SCHEMA <name>`, or `None` for `public`.
    ///
    /// **Where the extension goes, which `pg_extension.extnamespace` reports and
    /// `ActiveRecord#extensions` reads.** A name no schema answers to is `3F000`, checked when the
    /// statement runs rather than here — the catalog is what knows, and the check happens *after*
    /// the already-installed arms, because `IF NOT EXISTS` on an installed extension never looks
    /// at the schema and never moves it.
    pub schema: Option<String>,
}

/// What `SET DEFAULT` was given: a sequence to draw from, or an ordinary default.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnDefault {
    /// `nextval('s')` — the sequence's name, unresolved.
    Sequence(String),
    /// Everything else, in the two halves `catalog::ColumnDef` keeps: the folded value and the
    /// expression text (`crate::parse::lower`'s `column_default`).
    Value {
        /// The folded constant, when it folds.
        folded: Option<Datum>,
        /// The expression text, when it stays one.
        expr: Option<String>,
    },
}

/// `CREATE [OR REPLACE] FUNCTION f() RETURNS TRIGGER AS $$…$$ LANGUAGE plpgsql`.
///
/// **Define-only.** The body is stored verbatim and never parsed, let alone run: the schema load
/// reaches this twice and inserts nothing through it, so what it needs is a catalog that can hold
/// a function. PostgreSQL validates a plpgsql body at `CREATE` time and this node does not, which
/// is a declared divergence and not what the load requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateFunction {
    /// Its name, folded.
    pub name: String,
    /// The body between the dollar quotes, exactly as written.
    pub body: String,
    /// `LANGUAGE …`, folded. Anything but `plpgsql` is `42704` at execution.
    pub language: String,
    /// `OR REPLACE`. Absent, a name already taken is still a success here — PostgreSQL's `42723`
    /// for a duplicate function is unreachable while every function takes no arguments.
    pub or_replace: bool,
}

/// `CREATE TRIGGER t BEFORE|AFTER <events> ON tbl FOR EACH ROW EXECUTE FUNCTION|PROCEDURE f()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTrigger {
    /// The trigger's name, folded.
    pub name: String,
    /// The table it is on.
    pub table: String,
    /// `BEFORE` rather than `AFTER`.
    pub before: bool,
    /// The events as `tgtype` bits: `4` INSERT, `8` DELETE, `16` UPDATE.
    pub events: i16,
    /// `FOR EACH ROW`.
    pub for_each_row: bool,
    /// The function it names, folded — **`EXECUTE PROCEDURE` and `EXECUTE FUNCTION` are one
    /// clause**: statement 762 writes the first and 790 the second, and `pg_get_triggerdef` prints
    /// only the second.
    pub function: String,
}

/// `DROP TRIGGER [IF EXISTS] t ON tbl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTrigger {
    /// The trigger's name.
    pub name: String,
    /// The table it is on — a trigger is named per table, not per database.
    pub table: String,
    /// `IF EXISTS`.
    pub if_exists: bool,
}

/// `DROP FUNCTION [IF EXISTS] f [(<types>)] [, …]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropFunction {
    /// The functions named, each with its argument types **as written** or `None` for a statement
    /// that gave no list at all.
    ///
    /// The two are different statements rather than long and short spellings of one: with a list
    /// PostgreSQL selects a signature, and without one it selects *the* function of that name and
    /// is `42725` when there is more than one.
    pub functions: Vec<(String, Option<Vec<String>>)>,
    /// `IF EXISTS`: a function that is not there is a notice rather than a `42883`.
    ///
    /// **It covers absence only.** A built-in is `2BP01` with the clause and without it.
    pub if_exists: bool,
}

/// `CREATE SCHEMA [IF NOT EXISTS] name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSchema {
    /// The schema's name, folded.
    pub name: String,
    /// `AUTHORIZATION u` — the role named, which with no schema name is also the schema's name.
    ///
    /// **Carried, checked, and not stored.** The role has to exist for the statement to succeed,
    /// which is what a client can tell; `pg_namespace` has no owner column and this node enforces
    /// no ownership, so recording one would be a fact nobody reads. Declared in
    /// `docs/plans/roles-and-user-schemas.md`.
    pub owner: Option<String>,
    /// `IF NOT EXISTS`, which turns the `42P06` into a notice and a success.
    pub if_not_exists: bool,
}

/// `CREATE ROLE name` / `CREATE USER name`.
///
/// The two are one statement: `CREATE USER` is rewritten to `CREATE ROLE … LOGIN` before the parser
/// sees it (`crate::parse::rewrite_user_as_role`), so `login` is the only thing that carries the
/// difference here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRole {
    /// The role's name, folded.
    pub name: String,
    /// What the statement asked for: `LOGIN` (implied by `CREATE USER`), `SUPERUSER`, `CREATEDB`,
    /// `CREATEROLE`.
    ///
    /// **The catalog's own type, not four bools repeated here.** They are one set of attributes
    /// travelling from the parser to the record, and giving each layer its own copy would be three
    /// places for them to disagree — and three `struct_excessive_bools` allowances for one reason.
    ///
    /// Recorded, not honoured: nothing here checks a privilege. Reporting what was asked is
    /// honest; reporting the opposite, which is what dropping them did, is not.
    pub flags: crate::catalog::RoleFlags,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
}

/// `DROP ROLE [IF EXISTS] name` / `DROP USER …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRole {
    /// The roles named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`.
    pub if_exists: bool,
}

/// `DROP SCHEMA [IF EXISTS] name [CASCADE]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropSchema {
    /// The schemas named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`, which **covers absence and not dependence**: a schema with a table in it is
    /// still `2BP01` with the clause written. Measured.
    pub if_exists: bool,
    /// `CASCADE`, which takes everything in the schema with it.
    pub cascade: bool,
}

/// `CREATE [OR REPLACE] VIEW name [(cols)] AS SELECT …`.
///
/// **A view is a stored derived table.** The `SELECT` is kept as text and re-lowered wherever the
/// view is read, so `FROM v` becomes `FROM (<definition>) AS v` before anything plans it — the
/// rewrite `crate::plan::cte` already performs for a `WITH` item, with the text coming from the
/// catalog instead of the statement. That is why a view needs no plan node, no access path and no
/// read of its own.
///
/// Text and not a lowered plan, for the reason `CHECK` constraints and generated columns are also
/// stored as text: a lowered plan would have to be invalidated with the catalog entry that caches
/// it, and re-lowering a short `SELECT` per statement is the cheaper mistake to make. It is also
/// what `pg_get_viewdef` has to print back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateView {
    /// The view's name, folded.
    pub name: String,
    /// `CREATE VIEW v (a, b) AS …` — the names the view gives its columns, or empty when it takes
    /// them from the query.
    pub columns: Vec<String>,
    /// The `SELECT`, as text.
    pub definition: String,
    /// `OR REPLACE`.
    pub or_replace: bool,
}

/// `CREATE MATERIALIZED VIEW name [(cols)] AS SELECT … [WITH [NO] DATA]`.
///
/// [ADR 0064](../../../../docs/adr/0064-a-materialized-view-is-a-table-whose-rows-are-recomputed.md):
/// this creates a **table** that carries its definition, so the `SELECT` is planned once here the
/// `CREATE TABLE name [ (col, …) ] AS <query>`.
///
/// A table whose **shape is the query's**: the columns are typed from the plan, and nothing else
/// of the source comes across — measured on 19beta1, the new relation has no `NOT NULL`, no
/// default, no primary key and no index, although `SELECT id FROM people` reads a
/// `bigserial primary key`.
///
/// Deliberately a sibling of [`CreateMaterializedView`] rather than a field on
/// [`CreateTable`](super::CreateTable): the two share every step — plan the query, type the
/// relation from the plan, create it, fill it — and differ only in whether the definition is kept.
/// `CreateTable`'s executor builds columns from *declarations*, of which this statement has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableAs {
    /// The relation's name, folded.
    pub name: String,
    /// `CREATE TABLE t (a, b) AS …` — the names it gives its columns, or empty when it takes them
    /// from the query.
    pub columns: Vec<String>,
    /// The `SELECT`, as text, rendered back through the parser for the reason
    /// [`CreateMaterializedView::definition`] is.
    pub definition: String,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
}

/// way a view's is and then run to fill the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMaterializedView {
    /// The relation's name, folded.
    pub name: String,
    /// `CREATE MATERIALIZED VIEW m (a, b) AS …` — the names it gives its columns, or empty when it
    /// takes them from the query.
    pub columns: Vec<String>,
    /// The `SELECT`, as text.
    pub definition: String,
    /// `WITH DATA` (the default) computes the rows now; `WITH NO DATA` leaves the relation
    /// unpopulated, and reading it before a `REFRESH` is `55000`.
    pub with_data: bool,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
}

/// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshMaterializedView {
    /// The relation named, folded.
    pub name: String,
    /// `CONCURRENTLY`. PostgreSQL requires a unique index for it and refuses without one; the
    /// difference from the plain form is **locking**, and this node declares that divergence
    /// rather than hiding it (ADR 0064).
    pub concurrently: bool,
    /// `WITH NO DATA` on a refresh **empties** the relation and marks it unpopulated again.
    pub with_data: bool,
}

/// `DROP MATERIALIZED VIEW [IF EXISTS] name [, …] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropMaterializedView {
    /// The relations named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`.
    pub if_exists: bool,
    /// `CASCADE`: also drop what depends on it.
    pub cascade: bool,
}

/// `TRUNCATE [TABLE] t [, …] [RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE | RESTRICT]`.
///
/// **Not a `DELETE` without a `WHERE`**, and the difference that matters here is what it does
/// *not* touch: the sequences an identity or `serial` column owns keep their value, so the next
/// `nextval` carries on. `RESTART IDENTITY` is the clause that moves them, and it is the one
/// `ActiveRecord` sends when it wants a table to look new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncate {
    /// The tables named, folded. PostgreSQL empties them in one statement.
    pub names: Vec<String>,
    /// `RESTART IDENTITY`: also set every sequence these tables own back to its start.
    pub restart_identity: bool,
    /// `CASCADE`: also truncate the tables whose foreign keys point at these. Without it, a table
    /// something references is refused.
    pub cascade: bool,
}

/// `DROP VIEW [IF EXISTS] name [, …]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropView {
    /// The views named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`.
    pub if_exists: bool,
    /// `CASCADE`: also drop the views built on this one. Without it a dependent is `2BP01`.
    pub cascade: bool,
}

/// `CREATE DATABASE [IF NOT EXISTS] name`.
///
/// **A database is a tenant** ([ADR 0052](../../../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md)),
/// so this statement allocates one and writes the cluster's directory. PostgreSQL's option list is
/// not in `sqlparser` 0.62.0's `CREATE DATABASE` grammar at all, so it is cut out of the source
/// before the parse and re-attached in the lowering
/// (`crate::parse::strip_create_database_options`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDatabase {
    /// The database's name, folded.
    pub name: String,
    /// `IF NOT EXISTS`, which turns the `42P04` into a notice and a success.
    pub if_not_exists: bool,
    /// `TEMPLATE = x` — the database whose contents the new one starts with, or `None` for the
    /// default.
    ///
    /// **The only option that reaches the executor**, because it is the only one whose value names
    /// something in the catalog. Every other option is decided where it is read: this node has one
    /// encoding and one collation, so `ENCODING`/`LC_COLLATE`/`LC_CTYPE` are answered against
    /// constants and never recorded — there is nothing to record that is not already true of every
    /// database here.
    pub template: Option<String>,
}

/// `DROP DATABASE [IF EXISTS] name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropDatabase {
    /// The databases named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`, which covers absence and nothing else — the database the session is connected
    /// to is still `55006` with the clause written, because it is there rather than missing.
    pub if_exists: bool,
}

/// `ALTER SCHEMA name RENAME TO other`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterSchemaRename {
    /// The schema as it is now.
    pub name: String,
    /// What it becomes. **Every relation moves with it**, because a relation records the schema it
    /// is in by name and the rename rewrites that name.
    pub to: String,
}

/// A partition's bound as the statement wrote it, before the parent's types are in reach.
#[derive(Debug, Clone, PartialEq)]
pub enum PartitionSpec {
    /// `FOR VALUES IN (…)`.
    Values(Vec<Datum>),
    /// `FOR VALUES FROM (…) TO (…)`.
    Range {
        /// The lower bound, one entry per key column.
        from: Vec<RangeEnd>,
        /// The upper bound, one entry per key column.
        to: Vec<RangeEnd>,
    },
    /// `DEFAULT`.
    Default,
}

/// One end of a `FOR VALUES FROM … TO …`, as written.
///
/// `MINVALUE` and `MAXVALUE` are **keywords and not values**, so they cannot be a `Datum` waiting
/// for a type — which is the whole reason this enum exists beside [`PartitionSpec::Values`].
#[derive(Debug, Clone, PartialEq)]
pub enum RangeEnd {
    /// `MINVALUE`.
    MinValue,
    /// A literal, still untyped: the key column's type is the parent's and the parent is the
    /// executor's.
    Value(Datum),
    /// `MAXVALUE`.
    MaxValue,
}

/// `CREATE SEQUENCE [IF NOT EXISTS] s [START n] [INCREMENT BY n] [OWNED BY t.c | NONE]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSequence {
    /// Its name, folded, in the same namespace as tables and indexes.
    pub name: String,
    /// `IF NOT EXISTS`: an existing name is a notice rather than a `42P07`.
    pub if_not_exists: bool,
    /// `START n`: the **first** value `nextval` answers.
    pub start: i64,
    /// `INCREMENT BY n`.
    pub increment: i64,
    /// `OWNED BY t.c` — the table and column, unresolved. `None` for `OWNED BY NONE` and for a
    /// statement that said nothing, which PostgreSQL treats the same.
    ///
    /// **Ownership is not a default**: it says the sequence goes when the column does, and the
    /// column keeps whatever default it had.
    pub owned_by: Option<(String, String)>,
}

/// `DROP SEQUENCE [IF EXISTS] s [, …] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropSequence {
    /// One or more, folded. PostgreSQL takes a list.
    pub names: Vec<String>,
    /// `IF EXISTS`: a missing sequence is a notice rather than a `42P01`.
    pub if_exists: bool,
    /// `CASCADE`: take the column default that depends on it too, instead of refusing.
    ///
    /// `RESTRICT` is the default and the same statement as writing nothing — measured, both are
    /// `2BP01` with the same `DETAIL` and the same `HINT`.
    pub cascade: bool,
}

/// `DROP TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTable {
    /// One or more, folded. PostgreSQL takes a list.
    pub names: Vec<String>,
    /// `IF EXISTS`: a missing table is a notice rather than a `42P01`.
    pub if_exists: bool,
    /// `CASCADE`: take the dependent objects with it instead of refusing.
    ///
    /// One flag and not two, because `RESTRICT` **is** the default — `DROP TABLE t` and
    /// `DROP TABLE t RESTRICT` are the same statement and both are `2BP01` when something depends
    /// on the table. Measured, both spellings.
    ///
    /// `CASCADE` and `IF EXISTS` are **independent**: `DROP TABLE IF EXISTS x CASCADE` on a table
    /// that never existed is a plain success, and without `IF EXISTS` it is `42P01` whatever
    /// `CASCADE` says.
    pub cascade: bool,
}

/// One part of a `CREATE INDEX` key, before the table is known.
///
/// The catalog's [`crate::catalog::IndexKey`] with a name where the position will be: nothing can
/// resolve `b` to a column until the table has been read, and nothing may resolve it *twice*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexKeyPart {
    /// What the key part is over.
    pub part: KeyPartName,
    /// `DESC` and `NULLS FIRST/LAST`, already resolved against the direction's own default.
    pub order: KeyOrder,
    /// The **operator class** written after the column, folded to lower case, or `None` for the
    /// type's default. Carried and checked against the access method when the table is known
    /// (ADR 0070).
    pub opclass: Option<String>,
}

impl IndexKeyPart {
    /// An ascending column part, which is what everything but a `CREATE INDEX` produces.
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        IndexKeyPart {
            part: KeyPartName::Column(name.into()),
            order: KeyOrder::ASCENDING,
            // Only a `CREATE INDEX` can write one; a primary key and a `UNIQUE` take the default.
            opclass: None,
        }
    }
}

/// What one `CREATE INDEX` key part is over, before the table is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPartName {
    /// A column, by name, folded.
    Column(String),
    /// An expression over the row — `((lower(b)))`.
    Expression {
        /// The expression's own text, with no parentheses of its own.
        expr: String,
        /// Which of PostgreSQL's three deparse shapes it is.
        shape: ExprShape,
    },
}

/// `CREATE INDEX`, including `CREATE UNIQUE INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each one is an independent clause of the statement -- UNIQUE, IF NOT EXISTS, \
              CONCURRENTLY, NULLS NOT DISTINCT -- written or not written on its own, and a \
              struct of flags would be a name for a grouping SQL does not have"
)]
pub struct CreateIndex {
    /// The name it was given, or `None` for PostgreSQL's derived one.
    pub name: Option<String>,
    /// The table it is on, folded.
    pub table: String,
    /// The key, in key order.
    pub keys: Vec<IndexKeyPart>,
    /// The **access method** written after `USING`, folded — `btree` when none was.
    ///
    /// Recorded and not acted on ([ADR 0070](../../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)):
    /// the index built underneath is the ordered one every index here is, and this is what
    /// `pg_class.relam` and `pg_get_indexdef` report.
    pub access_method: String,
    /// Whether a duplicate is refused.
    pub unique: bool,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
    /// `CONCURRENTLY`: build it as a **staged job** rather than inside this statement.
    ///
    /// PostgreSQL's own distinction, and the same one: the concurrent form does not hold the table
    /// against writers, and pays for it by taking longer and by being able to leave an invalid
    /// index behind if it fails. Here it is ADR 0020's four states and a batched backfill; the
    /// plain form is still one transaction, which is correct for a small table and is the
    /// `TODO(post-v1)` for a large one.
    pub concurrently: bool,
    /// `WHERE …`, which makes it a **partial** index. `None` for an ordinary one.
    ///
    /// See `crate::catalog::IndexDef::predicate`: maintained, and never chosen for a read.
    pub predicate: Option<String>,
    /// `NULLS NOT DISTINCT`. See `crate::catalog::IndexDef::nulls_not_distinct` — the one clause
    /// in an index definition that changes which rows are refused.
    pub nulls_not_distinct: bool,
    /// `INCLUDE (…)` — the **non-key payload** columns, by name, unresolved.
    ///
    /// Plain column names and nothing else: an included column takes no `ASC`/`DESC` and no
    /// operator class, both of which a real server refuses with `42P17` rather than a syntax
    /// error. Neither reaches here — `sqlparser` 0.62.0 types this clause as a list of bare
    /// identifiers — so both are a C1 parser gap and are declared as such.
    pub include: Vec<String>,
}

/// A `FOREIGN KEY` as written, before the parent has been looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// Its name if it was given one. `None` is derived where the table is — `<table>_<column>_fkey`,
    /// numbered past every constraint name the schema already holds (`crate::exec::ddl`).
    pub name: Option<String>,
    /// This table's columns, by name, folded.
    pub columns: Vec<String>,
    /// The referenced table, folded.
    pub parent: String,
    /// The referenced columns, by name — **empty for `REFERENCES t` with no list**, which means
    /// the parent's primary key and is the form `t.references :parrot, foreign_key: true` emits.
    pub parent_columns: Vec<String>,
    /// `ON UPDATE …`.
    pub on_update: ReferentialAction,
    /// `ON DELETE …`.
    pub on_delete: ReferentialAction,
    /// `NOT VALID`: skip the scan of the rows already there. New rows are checked either way.
    pub validated: bool,
    /// `DEFERRABLE`: the check may move to `COMMIT`.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED`: it starts there.
    pub initially_deferred: bool,
}

/// One `CHECK`, as written: its expression, and its name **if it was given one**.
///
/// **The name is not derived here**, because PostgreSQL's derivation needs what lowering cannot
/// see: the column the expression reads, which is known once the expression is parsed, and every
/// constraint name the schema already holds, which a derived name is numbered past — measured,
/// `nsq_x_check1` beside another table's `nsq_x_check`. `crate::exec::ddl` names it where the
/// catalog is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConstraint {
    /// Given, or `None` for a name derived where the constraint is made.
    pub name: Option<String>,
    /// The expression, as the text that is stored.
    pub expr: String,
    /// Cleared by `NOT VALID`: the rows already there are not scanned.
    pub validated: bool,
}

/// `CREATE TYPE <name> AS RANGE (…) | AS (…) | AS ENUM (…)`.
///
/// One statement for three shapes, as PostgreSQL parses it. What each shape *means* is
/// [`crate::catalog::TypeKind`]; this is only what was written.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateType {
    /// The type's name, folded.
    pub name: String,
    /// Which shape, already read into the catalog's own form.
    pub kind: crate::catalog::TypeKind,
}

/// `ALTER TYPE <name> …` — the three shapes `ActiveRecord`'s enum helpers send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterType {
    /// The type's name, folded.
    pub name: String,
    /// What to do to it.
    pub action: AlterTypeAction,
}

/// The three `ALTER TYPE` actions, which are `rename_enum`, `add_enum_value` and
/// `rename_enum_value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTypeAction {
    /// `RENAME TO <new>` — the type keeps its oid, so every column of it keeps working.
    RenameTo(String),
    /// `ADD VALUE [IF NOT EXISTS] '<label>' [BEFORE | AFTER '<other>']`.
    AddValue {
        /// The label being added.
        label: String,
        /// `IF NOT EXISTS`: a label that is already there is a **no-op**, not `42710`.
        if_not_exists: bool,
        /// Where it goes, or `None` for the end.
        position: Option<AddValuePosition>,
    },
    /// `RENAME VALUE '<from>' TO '<to>'` — the label changes and the ordering does not.
    RenameValue {
        /// The label as it is now.
        from: String,
        /// What it becomes.
        to: String,
    },
}

/// `BEFORE '<label>'` or `AFTER '<label>'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddValuePosition {
    /// Immediately before the named label.
    Before(String),
    /// Immediately after it.
    After(String),
}

/// `DROP TYPE [IF EXISTS] <name> [, …] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropType {
    /// One or more, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`: a name that is not there is a **notice**, not `42704`.
    pub if_exists: bool,
    /// `CASCADE`, which would drop the columns that depend on the type.
    pub cascade: bool,
}

/// `COMMENT ON TABLE | COLUMN | INDEX <name> IS '…' | NULL`.
///
/// One statement for three objects, exactly as PostgreSQL parses it, because the differences are
/// all in *which* object is found and none in what happens to it: a comment is set or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Which kind of object was named, which decides both the lookup and the `42809` message when
    /// the name is a relation of a different kind.
    pub object: CommentObject,
    /// The name, folded. For a column it is the table's name; [`Comment::column`] holds the rest.
    pub name: String,
    /// The column, for `COMMENT ON COLUMN t.c`. Folded like every other identifier.
    pub column: Option<String>,
    /// The comment, or `None` for `IS NULL`.
    ///
    /// **`IS ''` arrives here as `Some("")` and is stored as `None`**, which is where PostgreSQL
    /// puts the rule too: it deletes the row rather than writing an empty one, so the two spellings
    /// cannot be told apart afterwards.
    pub comment: Option<String>,
}

/// Which kind of object a `COMMENT ON` named.
///
/// Only the three this node has. Every other kind PostgreSQL accepts — a schema, a type, a role —
/// is `0A000` naming itself at lowering, because a comment on an object this node does not have is
/// a comment with nowhere to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentObject {
    /// `COMMENT ON TABLE`.
    Table,
    /// `COMMENT ON COLUMN`.
    Column,
    /// `COMMENT ON INDEX`.
    Index,
    /// `COMMENT ON SEQUENCE`, which is here **to report the kind rather than to succeed**.
    ///
    /// PostgreSQL resolves the name before it looks at the word, so `COMMENT ON SEQUENCE <a
    /// table>` is `42809 "t" is not a sequence` and not a refusal of the statement. Taking the
    /// word is what lets this node give that answer. A comment on a *real* sequence is `0A000`
    /// naming itself: a sequence's record has no field to keep one in.
    Sequence,
    /// `COMMENT ON VIEW`, for the same reason — and this node has no views at all, so every name
    /// that resolves is the `42809` and every name that does not is `42P01`.
    View,
}

impl CommentObject {
    /// The word PostgreSQL uses in `42809 "x" is not an index`.
    #[must_use]
    pub fn article_and_name(self) -> &'static str {
        match self {
            CommentObject::Table | CommentObject::Column => "a table",
            CommentObject::Index => "an index",
            CommentObject::Sequence => "a sequence",
            CommentObject::View => "a view",
        }
    }
}

/// `DROP INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndex {
    /// One or more, folded.
    pub names: Vec<String>,
    /// `CONCURRENTLY`: run the states **backwards** as a staged job rather than dropping it here.
    ///
    /// The removal direction (ADR 0020): `public → write-only → delete-only → absent`, and only
    /// then are the entries removed. Each step waits longer than an adding one, because what a
    /// removal has to outlast is a *reader* rather than a writer.
    pub concurrently: bool,
    /// `IF EXISTS`.
    pub if_exists: bool,
    /// `CASCADE`, accepted and with nothing to do: **nothing depends on an index** here. A real
    /// server takes the word too and drops the same index, which is why it is carried rather than
    /// refused — the answer is identical and refusing would be inventing a difference.
    pub cascade: bool,
}

/// `<table>_pkey`, PostgreSQL's name for an unnamed primary key constraint.
///
/// **No column part at all**, which is the one derived name that has none — `makeObjectName` is
/// called with a null second name, so a 63-byte table keeps 58 of its bytes here and only 56
/// under [`sequence_name`] for a column called `id`.
#[must_use]
pub fn primary_key_name(table: &str) -> String {
    make_object_name(table, None, "pkey")
}

/// `<table>_<column>…_key`, PostgreSQL's name for an unnamed `UNIQUE` constraint.
#[must_use]
pub fn unique_constraint_name(table: &str, columns: &[String]) -> String {
    make_object_name(
        table,
        Some(&name_addition(columns.iter().map(String::as_str))),
        "key",
    )
}

/// `<table>_<column>_seq`, PostgreSQL's name for the sequence behind a `bigserial` or an identity
/// column. Measured: `pg_get_serial_sequence('s1','id')` answers `public.s1_id_seq`.
#[must_use]
pub fn sequence_name(table: &str, column: &str) -> String {
    make_object_name(table, Some(column), "seq")
}

/// `<table>_<column>_fkey`, PostgreSQL's name for an unnamed foreign key constraint.
///
/// Measured: `CREATE TABLE fxe (id int8 PRIMARY KEY, p int8 REFERENCES fxp)` names it
/// `fxe_p_fkey`. Every referencing column contributes, the way an index's do.
#[must_use]
pub fn foreign_key_name(table: &str, columns: &[String]) -> String {
    make_object_name(
        table,
        Some(&name_addition(columns.iter().map(String::as_str))),
        "fkey",
    )
}

/// The column half of a derived constraint name — `a_b` for `(a, b)` — for a caller that numbers
/// the name itself: [`choose_relation_name`] takes the parts unjoined.
#[must_use]
pub fn column_name_addition(columns: &[String]) -> String {
    name_addition(columns.iter().map(String::as_str))
}

/// `<table>_<column>…_idx`, PostgreSQL's name for an unnamed index.
#[must_use]
pub fn index_name(table: &str, keys: &[IndexKeyPart]) -> String {
    make_object_name(table, Some(&index_name_addition(keys)), "idx")
}

/// What the key parts contribute to a derived index name, before the table and the label are
/// joined on: `ChooseIndexNameAddition`.
#[must_use]
pub fn index_name_addition(keys: &[IndexKeyPart]) -> String {
    name_addition(keys.iter().map(key_part_name))
}

/// What one key part contributes to a derived index name.
///
/// A column contributes its name; an expression contributes the **function it calls**, and
/// `expr` when it is not a call. Measured against PostgreSQL 19, which named
/// `CREATE INDEX ON xj (a, (lower(b)))` `xj_a_lower_idx`, `((abs(a)))` `xj_abs_idx` and both
/// `((a + c))` and `((1))` `xj_expr_idx` — the second disambiguated to `xj_expr_idx1`, which is
/// the collision loop `crate::exec::ddl::create_index` already runs for a derived name.
fn key_part_name(key: &IndexKeyPart) -> String {
    match &key.part {
        KeyPartName::Column(name) => name.clone(),
        // The callee's name is everything before the first `(` — a call's text is `name(args)`
        // and nothing else, because that is what `ExprShape::Call` means.
        KeyPartName::Expression {
            expr,
            shape: ExprShape::Call,
        } => expr
            .split_once('(')
            .map_or_else(|| expr.clone(), |(name, _)| name.to_owned()),
        KeyPartName::Expression { .. } => "expr".to_owned(),
    }
}

/// The column half of a derived name: the parts joined with `_`, and **stopped** once it is at
/// least as long as one identifier.
///
/// `ChooseIndexNameAddition`, which builds into a buffer of `2 * NAMEDATALEN` and breaks out of
/// the loop the moment the buffer has reached `NAMEDATALEN` — so a long key list contributes at
/// most one part past 63 bytes and the rest are not joined at all. [`make_object_name`] then
/// truncates what comes out, which is why the cap is not visible for any name a test writes; it is
/// here so that a hundred-column key does not build a kilobyte of string to throw away.
fn name_addition(parts: impl Iterator<Item = impl AsRef<str>>) -> String {
    let mut out = String::new();
    for part in parts {
        if !out.is_empty() {
            out.push('_');
        }
        out.push_str(part.as_ref());
        if out.len() >= MAX_IDENTIFIER_BYTES {
            break;
        }
    }
    out
}

/// PostgreSQL's `makeObjectName`: `<name1>_<name2>_<label>`, made to fit in one identifier by
/// **taking characters off the longer of the two names** until it does.
///
/// The label and the separators are never what gives way — that is the whole point of the
/// function, and it is why a 63-byte table still gets a sequence whose name ends in `_seq`.
/// `name2` is [`None`] for a primary key, which is the one derived name with no column part, and
/// then there is no separator for it either.
///
/// Measured on 19beta1 against a table named with exactly `NAMEDATALEN - 1` characters, which is
/// what `LongerSequenceNameDetectionTest` uses. All three of its sequences come out at exactly 63
/// bytes with a different amount of table left in each:
///
/// | column | sequence | table bytes kept |
/// |---|---|---|
/// | `id` | `long_table_…_for_seri_id_seq` | 56 |
/// | `seq` | `long_table_…_for_ser_seq_seq` | 55 |
/// | `bigseq` | `long_table_…_for__bigseq_seq` | 52 |
///
/// The third keeps the trailing `_` of the table name and then adds the separator, which is the
/// double underscore — a join-then-truncate would have produced none of these three.
#[must_use]
pub fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    // **The budget belongs to the identifier, never to the schema in front of it.** A stored name
    // is bare in `public` and `schema ++ NUL ++ name` anywhere else
    // ([ADR 0071](../../../../docs/adr/0071-a-relation-name-is-keyed-by-its-schema.md)), so handing
    // the qualified form in made the schema and its separator eat into the table's share: a
    // 60-character table in a one-character schema derived a **61**-byte index name where a real
    // server derives 63, and a long enough schema would have truncated the separator itself away
    // and put the index in `public`.
    //
    // Invisible in `public`, where the two forms are the same string — which is why
    // `tests/derived_name_in_a_schema.rs` is in a schema of its own.
    // Guarded on the **separator**, not on the schema `split_qualified` reports: it answers
    // `public` for a bare name, so a check against "no schema" is never true and recurses for ever.
    if let Some((schema, bare)) = name1.split_once(crate::catalog::SCHEMA_SEPARATOR) {
        return crate::catalog::qualify(schema, &make_object_name_bare(bare, name2, label));
    }
    make_object_name_bare(name1, name2, label)
}

/// [`make_object_name`] once the schema is off, which is where the byte budget is spent.
fn make_object_name_bare(name1: &str, name2: Option<&str>, label: &str) -> String {
    // `NAMEDATALEN - 1 - overhead`, where the overhead is the label, its separator, and the
    // separator before `name2` when there is one.
    let overhead = usize::from(name2.is_some()) + label.len() + 1;
    let available = MAX_IDENTIFIER_BYTES.saturating_sub(overhead);
    let mut keep1 = name1.len();
    let mut keep2 = name2.map_or(0, str::len);
    // "This logic could be expressed without a loop, but it's simple and obvious as a loop" —
    // and it is the loop that makes the two names give way alternately once they are equal,
    // which is what decides where a tie lands.
    while keep1 + keep2 > available {
        if keep1 > keep2 {
            keep1 -= 1;
        } else if keep2 > 0 {
            keep2 -= 1;
        } else {
            // Only reachable if the label alone does not fit, which no label here is long
            // enough to do. Truncating the label would produce a name that means something
            // else, so the name is left over-long and `fold_identifier` clips it below.
            break;
        }
    }
    let mut out = String::with_capacity(MAX_IDENTIFIER_BYTES);
    out.push_str(clip(name1, keep1));
    if let Some(name2) = name2 {
        out.push('_');
        out.push_str(clip(name2, keep2));
    }
    out.push('_');
    out.push_str(label);
    // A no-op for every name the budget above produced; the one thing that could still be over is
    // the unreachable branch, and a name that cannot be stored is worse than a short one.
    fold_identifier(&out, true).0
}

/// The first `at` **bytes** of `s`, backed up to a character boundary — `pg_mbcliplen`.
///
/// Cutting a multi-byte character in half would leave a name that is not UTF-8, which the catalog
/// would refuse to read back.
fn clip(s: &str, mut at: usize) -> &str {
    if at >= s.len() {
        return s;
    }
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    &s[..at]
}

/// The name a derived relation actually gets: [`make_object_name`], and then **a counter on the
/// label** for as long as `taken` says the name is in use.
///
/// PostgreSQL's `ChooseRelationName`. The counter joining the *label* rather than the finished
/// name is not a detail — it changes the answer whenever the name is long enough to truncate,
/// because a longer label leaves less room and the name is rebuilt from the full parts. Measured
/// on 19beta1: a 56-character table with an `id serial` whose plain name `<56>_id_seq` was already
/// taken got **`<55>_id_seq1`**, one character shorter in the table half. Appending `1` to the
/// truncated name would have given `<56>_id_seq1`, which is 64 bytes and not what the server
/// chose.
///
/// A *given* name that is taken is an error and never comes here; only a name this node derived is
/// disambiguated, because there is nothing of the user's to collide with.
pub fn choose_relation_name<E>(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    mut taken: impl FnMut(&str) -> Result<bool, E>,
) -> Result<String, E> {
    let mut modlabel = label.to_owned();
    for pass in 1..=u32::MAX {
        let candidate = make_object_name(name1, name2, &modlabel);
        if !taken(&candidate)? {
            return Ok(candidate);
        }
        modlabel = format!("{label}{pass}");
    }
    // Four billion relations of one derived name is not reachable, and answering the plain name
    // lets the caller's own duplicate check give the error rather than inventing one here.
    Ok(make_object_name(name1, name2, label))
}

#[cfg(test)]
mod tests {
    use super::{
        IndexKeyPart, KeyPartName, choose_relation_name, index_name, primary_key_name,
        sequence_name, unique_constraint_name,
    };
    use crate::catalog::MAX_IDENTIFIER_BYTES;
    use crate::catalog::{ExprShape, KeyOrder};

    fn column(name: &str) -> IndexKeyPart {
        IndexKeyPart::column(name)
    }

    /// The names a real PostgreSQL 19 gave a table with a primary key, a `UNIQUE` column and two
    /// indexes. A client matching on a constraint name would not recognise anything else.
    #[test]
    fn derived_names_are_the_ones_postgresql_derives() {
        assert_eq!(primary_key_name("mixed"), "mixed_pkey");
        assert_eq!(
            unique_constraint_name("mixed", &["val".into()]),
            "mixed_val_key"
        );
        assert_eq!(index_name("mixed", &[column("a")]), "mixed_a_idx");
        assert_eq!(
            index_name("mixed", &[column("a"), column("b")]),
            "mixed_a_b_idx"
        );
    }

    /// The names PostgreSQL 19 derived for an index with an **expression** in its key: the
    /// function's name for a call, and `expr` for anything else. Measured on `xj (id, a, b, c)`.
    #[test]
    fn a_derived_name_takes_the_function_from_an_expression_key() {
        let expression = |expr: &str, shape| IndexKeyPart {
            part: KeyPartName::Expression {
                expr: expr.to_owned(),
                shape,
            },
            order: KeyOrder::ASCENDING,
            opclass: None,
        };
        let call = |expr: &str| expression(expr, ExprShape::Call);
        let other = |expr: &str| expression(expr, ExprShape::Operator);
        assert_eq!(index_name("xj", &[call("lower(b)")]), "xj_lower_idx");
        assert_eq!(index_name("xj", &[call("abs(a)")]), "xj_abs_idx");
        assert_eq!(
            index_name("xj", &[column("a"), call("lower(b)")]),
            "xj_a_lower_idx"
        );
        assert_eq!(
            index_name("xj", &[call("lower(b)"), call("upper(b)")]),
            "xj_lower_upper_idx"
        );
        // `((a + c))` and `((1))` are both `xj_expr_idx`; the second is disambiguated by the
        // collision loop in `crate::exec::ddl::create_index`, not here.
        assert_eq!(index_name("xj", &[other("a + c")]), "xj_expr_idx");
        assert_eq!(index_name("xj", &[other("1")]), "xj_expr_idx");
    }

    /// A derived name is an identifier like any other and obeys the same limit.
    #[test]
    fn a_derived_name_is_truncated_like_any_other_identifier() {
        let long = "x".repeat(70);
        assert_eq!(primary_key_name(&long).len(), MAX_IDENTIFIER_BYTES);
    }

    /// `LongerSequenceNameDetectionTest`'s table, which is `NAMEDATALEN - 1` characters exactly,
    /// and the three sequences PostgreSQL 19beta1 named for it.
    ///
    /// **The `_seq` is never what gives way.** All three are 63 bytes and a different amount of
    /// the table survives in each, because the budget is spent on the *column* first. Joining the
    /// parts and clipping the result — which is what this did before — makes all three the same
    /// string, and then the adapter cannot find the sequence it just created.
    #[test]
    fn a_63_byte_table_still_gets_a_name_ending_in_seq() {
        let long = "long_table_name_to_test_sequence_name_detection_for_serial_cols";
        assert_eq!(long.len(), MAX_IDENTIFIER_BYTES);
        for (column, expected) in [
            (
                "id",
                "long_table_name_to_test_sequence_name_detection_for_seri_id_seq",
            ),
            (
                "seq",
                "long_table_name_to_test_sequence_name_detection_for_ser_seq_seq",
            ),
            // The table's own trailing `_` survives and the separator follows it, which is the
            // double underscore. Nothing about it is special-cased; it is what is left of the
            // table after 52 bytes.
            (
                "bigseq",
                "long_table_name_to_test_sequence_name_detection_for__bigseq_seq",
            ),
        ] {
            let name = sequence_name(long, column);
            assert_eq!(name, expected, "sequence for {column}");
            assert_eq!(name.len(), MAX_IDENTIFIER_BYTES, "sequence for {column}");
        }
        // A primary key has no column part at all, so it keeps two more bytes than the shortest
        // column here would leave it.
        assert_eq!(
            primary_key_name(long),
            "long_table_name_to_test_sequence_name_detection_for_serial_pkey"
        );
    }

    /// **The counter joins the label, and the name is then rebuilt** — measured on 19beta1 with a
    /// 56-character table whose plain sequence name was already taken by a `CREATE SEQUENCE`.
    ///
    /// The server answered `<55 a's>_id_seq1`: the table half gave up a character to make room for
    /// the `1`. Appending the counter to the finished name would have given `<56>_id_seq1`, which
    /// is 64 bytes and is not a name a real server ever produced. For a short name the two rules
    /// agree, which is why `foo_bar_baz_id_seq1` alone does not decide it.
    #[test]
    fn a_collision_counter_joins_the_label_and_the_name_is_rebuilt() {
        let taken = "a".repeat(56);
        let plain = sequence_name(&taken, "id");
        assert_eq!(plain.len(), MAX_IDENTIFIER_BYTES);

        let chosen = choose_relation_name(&taken, Some("id"), "seq", |candidate| {
            Ok::<_, ()>(candidate == plain)
        })
        .unwrap();
        assert_eq!(chosen, format!("{}_id_seq1", "a".repeat(55)));
        assert_eq!(chosen.len(), MAX_IDENTIFIER_BYTES);

        // `CollidedSequenceNameTest`'s own pair, where no truncation happens and the counter looks
        // like a plain suffix.
        let short = choose_relation_name("foo", Some("bar_baz_id"), "seq", |candidate| {
            Ok::<_, ()>(candidate == "foo_bar_baz_id_seq")
        })
        .unwrap();
        assert_eq!(short, "foo_bar_baz_id_seq1");
    }
}

/// `ALTER TABLE`.
///
/// PostgreSQL takes a list of actions in one statement and applies them together
/// (`ALTER TABLE t ADD COLUMN a text, ADD COLUMN b text` is one atomic change), so this carries a
/// list rather than a single action.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    /// The table's name, folded.
    pub name: String,
    /// `IF EXISTS`: a missing table is a notice rather than a `42P01`.
    pub if_exists: bool,
    /// In the order they were written, which is the order the columns land in.
    pub actions: Vec<AlterTableAction>,
}

/// The triggers an `ENABLE` or `DISABLE TRIGGER` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSelection {
    /// `ALL`: every trigger a user created, and the internal ones a foreign key is made of — the
    /// table's referential checks ([`crate::catalog::TableDef::triggers_disabled`]).
    All,
    /// `USER`: every trigger a user created, and none of the foreign key's.
    User,
    /// One trigger, by its folded name.
    Named(String),
}

/// One action of an `ALTER TABLE`.
///
/// `ADD COLUMN` and `DROP COLUMN` are here. Every other action parses and comes back `0A000`
/// naming itself (contract C2), a type change included: the row format carries a column *count*
/// and not column identity ([ADR 0019](../../../../docs/adr/0019-a-row-says-how-many-columns-it-has.md)),
/// so a column cannot change width under rows already written. `DROP COLUMN` used to be refused
/// for that same reason and no longer is — it does not need identity, because the slot never moves
/// ([ADR 0051](../../../../docs/adr/0051-a-dropped-column-keeps-its-slot.md)).
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    /// `ADD [COLUMN] [IF NOT EXISTS] <column> <type>`, nullable and with no default — the only
    /// shape that needs no row rewritten.
    AddColumn {
        /// The column to append, with its `NOT NULL`, its `DEFAULT` and — since the legacy
        /// primary-key unit — its sequence, if it was declared `serial`.
        column: Column,
        /// `IF NOT EXISTS`: a column that is already there is a notice rather than a `42701`.
        if_not_exists: bool,
        /// `PRIMARY KEY` written on the column.
        ///
        /// The table keeps whatever identity its rows already have — a table created without a key
        /// has a hidden row-id column and goes on using it, which is a fact about the *stored
        /// rows* and not about whether a key is declared ([`crate::catalog::TableDef::row_id`]).
        /// So this declares a key rather than re-keying anything.
        primary_key: bool,
    },
    /// `DROP [COLUMN] [IF EXISTS] <name> [CASCADE|RESTRICT]`.
    ///
    /// The column is **tombstoned, not removed** (ADR 0051): a row is decoded by position, so
    /// taking the slot out would turn every row written before this statement into a decode
    /// error. What the executor changes is `ColumnDef::dropped` and the objects that depended on
    /// the column; the rows are not read and not written.
    DropColumn {
        /// The column, folded.
        column: String,
        /// `IF EXISTS`: a column that is not there is a notice rather than a `42703`.
        if_exists: bool,
        /// `CASCADE`: also drop the objects **outside this table** that depend on the column.
        ///
        /// It does not govern the ones on the table itself — an index over the column, its
        /// `CHECK`, its `NOT NULL`, its default and a foreign key declared on it go either way,
        /// measured. What `CASCADE` buys is a view over the column, or another table's foreign key
        /// referencing it; without it those are `2BP01`.
        cascade: bool,
    },
    /// `ALTER COLUMN <name> TYPE <type> [USING <expr>]`, which is what `change_column` sends.
    ///
    /// **`USING` is carried as a fact, not as an expression.** This node has no per-row cast to
    /// evaluate one with, and the only `USING` the suite writes is `CAST(<the same column> AS <the
    /// same target>)` — which asks for exactly the conversion the statement already names. So what
    /// travels is whether the statement licensed a conversion PostgreSQL would not do implicitly;
    /// any *other* `USING` expression is refused by name, because running it would need the
    /// evaluator that does not exist and ignoring it would be a wrong answer.
    SetColumnType {
        /// The column, folded.
        column: String,
        /// The target type.
        ty: ColumnType,
        /// Its `atttypmod`, or `-1`.
        typmod: i32,
        /// The type a `USING` casts this column to, when the statement wrote one.
        ///
        /// **Not necessarily the target type.** `ALTER COLUMN s TYPE character varying USING
        /// s::text` casts to `text` and lands in `varchar`, which PostgreSQL takes because the
        /// second hop is an assignment cast — so both hops are checked rather than one.
        using: Option<ColumnType>,
        /// A `USING` that is **not** a cast of the column: the expression itself, evaluated once
        /// per row in place of the implicit conversion.
        ///
        /// `ALTER COLUMN snippets TYPE text[] USING string_to_array(snippets, ',')` is the shape
        /// `array_test.rb` sends, and it is the general case — PostgreSQL takes any expression over
        /// the row and writes what it answers. The cast form above stays its own field because it
        /// is what the *pre-flight* check reads: a cast's target type can be checked before a row
        /// is touched, and an arbitrary expression's cannot.
        ///
        /// At most one of the two is set.
        using_expr: Option<super::Expr>,
        /// The collation the statement named, or `None` to keep the type's own.
        ///
        /// **`None` clears one that was there**, which is PostgreSQL's rule and not a shortcut:
        /// `ALTER COLUMN c TYPE text` gives the column its new type's collation, so a column that
        /// was `COLLATE "C"` and is retyped without a clause goes back to the default.
        collation: Option<String>,
    },
    /// `VALIDATE CONSTRAINT <name>` — the second half of `NOT VALID`.
    ///
    /// It scans the rows the `ADD` skipped and, if they all satisfy the constraint, marks it
    /// validated. **On an already-valid constraint it is a success, not an error**, and so it is
    /// on one that was never `NOT VALID`; a name the table does not have is `42704`.
    ValidateConstraint(String),
    /// `ALTER COLUMN <name> SET NOT NULL` / `DROP NOT NULL`, which is what `change_column_null`
    /// sends (`abstract/schema_statements.rb`).
    ///
    /// **`SET NOT NULL` scans the table.** PostgreSQL refuses it with `23502 column "c" of
    /// relation "t" contains null values` if a row already holds one — a different message from
    /// the `23502` an offending *insert* gets, and it names no constraint because the constraint
    /// does not exist yet. Adding the flag without the scan would leave a table whose rows
    /// contradict its own catalog.
    SetNotNull {
        /// The column, folded.
        column: String,
        /// `SET` (true) or `DROP` (false).
        not_null: bool,
    },
    /// `ADD CONSTRAINT <name> UNIQUE (…)`, which is what `add_unique_constraint` sends — and the
    /// setup every `remove_unique_constraint` test needs before it can remove one.
    ///
    /// **The index it builds is a constraint's**, not a `CREATE UNIQUE INDEX`'s: the two make the
    /// same index and only `IndexDef::constraint` tells them apart, which is what decides whether
    /// `DROP CONSTRAINT` or `DROP INDEX` can remove it.
    AddUnique(UniqueConstraint),
    /// `ADD CONSTRAINT … UNIQUE USING INDEX <index>` — promote an index that already exists.
    ///
    /// A unique constraint here **is** an [`crate::catalog::IndexDef`] with its `constraint` field
    /// set, so promoting one is setting that field and renaming the index; nothing is built and
    /// nothing is backfilled, because the index is already there and already filled.
    AddUniqueUsingIndex(UniqueUsingIndex),
    /// `RENAME COLUMN <from> TO <to>`, the one shape `rename_column` sends
    /// (`abstract/schema_statements.rb:1923`).
    ///
    /// **A rename does not move the column.** Its ordinal is unchanged, so every index, constraint
    /// and default keeps pointing at the same attribute and nothing needs rewriting — which is
    /// exactly why this node can do it: those all reference a column by *position*, and only
    /// `attname` is stored as text.
    RenameColumn {
        /// The column as it is now.
        from: String,
        /// What it becomes.
        to: String,
    },
    /// `RENAME TO <name>` — the table itself, `rename_table`'s statement (`:459`) and this one's
    /// neighbour in the same capture.
    ///
    /// **The sequence a `serial` column owns is not renamed with it**, measured, which is why
    /// `ActiveRecord` follows this with an explicit `ALTER TABLE <seq> RENAME TO` (`:474`).
    RenameTo(String),
    /// `DROP CONSTRAINT [IF EXISTS] <name> [CASCADE|RESTRICT]`.
    ///
    /// **The one statement four `ActiveRecord` methods end in** — `remove_check_constraint`,
    /// `remove_foreign_key`, `remove_unique_constraint` and `remove_exclusion_constraint` all
    /// render through `schema_creation.rb:101`.
    DropConstraint {
        /// The constraint, folded.
        name: String,
        /// `IF EXISTS`: a name that is nothing is a notice rather than a `42704`, which is what
        /// makes `ActiveRecord`'s idempotent migrations work.
        if_exists: bool,
        /// `CASCADE`: also drop what depends on the constraint. Only a primary key or unique
        /// constraint has anything that can — another table's foreign key needs the index behind
        /// it — and the cascade takes that foreign key, leaving the referencing *column* alone.
        cascade: bool,
    },
    /// `ALTER COLUMN c SET DEFAULT <expr>` and `ALTER COLUMN c DROP DEFAULT`.
    SetDefault {
        /// The column, folded.
        column: String,
        /// The default, or `None` for `DROP DEFAULT`.
        ///
        /// **`nextval('s')` is its own arm and not an expression.** A sequence *is* a column's
        /// default in this catalog, so pointing a column at one moves which sequence fills it
        /// rather than storing text to evaluate — and that move is what frees the sequence the
        /// column used to draw from.
        default: Option<ColumnDefault>,
    },
    /// `ALTER TABLE … SET { LOGGED | UNLOGGED }`.
    ///
    /// Built from the statement's *class* rather than from a parsed action, because `sqlparser`
    /// 0.62.0 has no `LOGGED` keyword (`crate::parse::set_persistence`). It reaches the executor
    /// through the ordinary `ALTER TABLE` path all the same, so it gets the same transaction, the
    /// same `42P01` for a missing table and the same schema-version bump.
    SetPersistence(crate::catalog::Persistence),
    /// `ALTER TABLE … ADD CONSTRAINT … CHECK (…)`.
    AddCheck(CheckConstraint),
    /// `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (…) REFERENCES … (…)`.
    ///
    /// The parent is named rather than resolved: nothing can turn `author_addresses` into a table
    /// id until the catalog has been read, and the executor is where that happens once.
    AddForeignKey(ForeignKey),
    /// `ENABLE`/`DISABLE TRIGGER ALL | USER | <name>` — and `ALL` is **not** a no-op on a table
    /// with no triggers of its own.
    ///
    /// `ALL` includes PostgreSQL's *internal* foreign-key triggers, which is why a real server
    /// requires superuser for it and why `ActiveRecord` writes it around every fixture load:
    /// `disable_referential_integrity` is this statement, and the point of it is to insert rows
    /// whose parents are not there yet. Measured on PostgreSQL 19, all four halves:
    ///
    /// * with the **child's** triggers disabled, an `INSERT` naming a parent row that does not
    ///   exist **succeeds**;
    /// * with the **parent's** disabled, a `DELETE` of a referenced row succeeds and leaves the
    ///   child pointing at nothing;
    /// * disabling the *parent's* does **not** suspend the child's insert check, because the two
    ///   triggers live on different tables;
    /// * `USER` suspends none of it — an `INSERT` under `DISABLE TRIGGER USER` is still `23503`.
    ///
    /// So this carries which triggers the statement named: `ALL` moves the referential checks and
    /// every user trigger, `USER` only the user triggers, and a name only that one
    /// ([`crate::catalog::TriggerDef::enabled`]).
    SetTriggersDisabled {
        /// Which triggers.
        which: TriggerSelection,
        /// Whether they are disabled from here on. Stored on the table, because PostgreSQL's is
        /// stored too — `pg_trigger.tgenabled` outlives the transaction that set it and every
        /// session sees it.
        disabled: bool,
    },
    /// `SET (retention = '7d' | 'forever' | DEFAULT)` — how far back this table can be read.
    ///
    /// A storage parameter, which is PostgreSQL's own shape for a per-table knob and one this
    /// node needs no grammar of its own to accept. It is the travel window
    /// (`docs/adr/0021-time-machine.md` Decision 2): how far back you can read is how far back
    /// the collector has not yet swept, and those must be one number or the feature is a promise
    /// the storage layer does not keep.
    SetRetention {
        /// Milliseconds, [`crate::catalog::RETENTION_FOREVER`] for `'forever'`, or `None` for
        /// `DEFAULT`, which deletes the override rather than storing a zero. An absent key and a
        /// key holding zero are different things to the collector.
        retention_ms: Option<u64>,
    },
    /// `SET (columnar_replicas = <n>)` — how many columnar copies of this table the cluster
    /// should keep ([ADR 0022](../../../../docs/adr/0022-columnar-learner-replica.md) Decision 5).
    ///
    /// The same storage-parameter shape as `retention` above and, like it, **not part of the
    /// table definition**: it changes nothing about how a row is written or read, so it does not
    /// bump the schema version. What acts on it is the placement driver.
    ///
    /// PostgreSQL refuses this spelling — measured, not assumed: `unrecognized parameter
    /// "columnar_replicas"`, SQLSTATE 22023, and there is **no** custom spelling it accepts, since
    /// an arbitrary namespace is refused too (`tests/corpus/pg19_storage_parameters.txt`). So this
    /// is a deliberate divergence rather than a gap, and it is in the register with the others.
    SetColumnarReplicas {
        /// How many, or `None` for `RESET`, which forgets the setting. Zero is legal and means
        /// the same as forgetting it to every reader.
        replicas: Option<u8>,
    },
    /// `ADD CONSTRAINT … PRIMARY KEY (…)` over columns the table already has.
    ///
    /// The other half of `ADD COLUMN … PRIMARY KEY`, and the spelling `change_table`'s
    /// `t.primary_key :id` sends when the column is already there. It declares a key and re-keys
    /// nothing — see [`AlterTableAction::AddColumn`]'s `primary_key` for why that is sound.
    AddPrimaryKey {
        /// The constraint's name, or `None` for the one a real server derives (`<table>_pkey`).
        name: Option<String>,
        /// The key's columns, by name, in the order written.
        columns: Vec<String>,
    },
    /// `ADD CONSTRAINT … EXCLUDE (…)` on a table that already exists.
    ///
    /// The same [`crate::catalog::ExcludeDef`] a `CREATE TABLE` builds, from the same clause
    /// parser — what differs is only that the rows are already there, so the executor validates
    /// them before the constraint is recorded.
    AddExclude(crate::catalog::ExcludeDef),
    /// A storage parameter a real server takes and this node has nowhere to put: every name in a
    /// `RESET` but `columnar_replicas`, and anything in the `toast` namespace.
    ///
    /// **It is its own variant because the alternative was wrong.** Saying "accepted, and it
    /// changes nothing" with `SetColumnarReplicas { replicas: None }` reuses the one action that
    /// already meant something — *forget the setting* — so a `SET (toast.autovacuum_enabled = …)`
    /// deleted a table's columnar wish and told the placement driver about it. A no-op needs a
    /// state of its own; it cannot borrow one.
    AcceptStorageParameter,
}
