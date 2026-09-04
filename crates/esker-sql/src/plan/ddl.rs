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

use crate::catalog::{ExprShape, Identity, KeyOrder, ReferentialAction, fold_identifier};
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
    /// Every `CHECK`, named the way PostgreSQL names one: as written, or
    /// `<table>_<column>_check` for a column constraint with no name of its own.
    pub checks: Vec<crate::catalog::CheckDef>,
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
    /// The sequence that fills this column — `bigserial` or `GENERATED ... AS IDENTITY` — and
    /// which of the three it is.
    ///
    /// All three serial spellings are among them, since [ADR
    /// 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md) gave this node the integers
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

/// `CREATE EXTENSION [IF NOT EXISTS] name`.
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
    /// `IF NOT EXISTS`, which turns the `42P06` into a notice and a success.
    pub if_not_exists: bool,
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

/// `DROP VIEW [IF EXISTS] name [, …]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropView {
    /// The views named, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`.
    pub if_exists: bool,
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
}

impl IndexKeyPart {
    /// An ascending column part, which is what everything but a `CREATE INDEX` produces.
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        IndexKeyPart {
            part: KeyPartName::Column(name.into()),
            order: KeyOrder::ASCENDING,
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
    /// Its name — given, or derived as `<table>_<column>_fkey`.
    pub name: String,
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
    /// `DEFERRABLE`, which is recorded and changes nothing here.
    pub deferrable: bool,
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
#[must_use]
pub fn primary_key_name(table: &str) -> String {
    derived(&[table, "pkey"])
}

/// `<table>_<column>…_key`, PostgreSQL's name for an unnamed `UNIQUE` constraint.
#[must_use]
pub fn unique_constraint_name(table: &str, columns: &[String]) -> String {
    let mut parts = vec![table];
    parts.extend(columns.iter().map(String::as_str));
    parts.push("key");
    derived(&parts)
}

/// `<table>_<column>_seq`, PostgreSQL's name for the sequence behind a `bigserial` or an identity
/// column. Measured: `pg_get_serial_sequence('s1','id')` answers `public.s1_id_seq`.
#[must_use]
pub fn sequence_name(table: &str, column: &str) -> String {
    derived(&[table, column, "seq"])
}

/// `<table>_<column>_fkey`, PostgreSQL's name for an unnamed foreign key constraint.
///
/// Measured: `CREATE TABLE fxe (id int8 PRIMARY KEY, p int8 REFERENCES fxp)` names it
/// `fxe_p_fkey`. Every referencing column contributes, the way an index's do.
#[must_use]
pub fn foreign_key_name(table: &str, columns: &[String]) -> String {
    let mut parts = vec![table];
    parts.extend(columns.iter().map(String::as_str));
    parts.push("fkey");
    derived(&parts)
}

/// `<table>_<column>…_idx`, PostgreSQL's name for an unnamed index.
#[must_use]
pub fn index_name(table: &str, keys: &[IndexKeyPart]) -> String {
    let mut parts = vec![table.to_owned()];
    parts.extend(keys.iter().map(key_part_name));
    parts.push("idx".to_owned());
    derived(&parts.iter().map(String::as_str).collect::<Vec<_>>())
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

/// Joins the parts and applies the same 63-byte limit every identifier has.
///
/// PostgreSQL also disambiguates a derived name that is already taken by appending a number; that
/// is not done here, and a collision is reported as the `42P07` it is rather than silently renamed
/// — `TODO(post-v1)`, and a deliberate difference rather than an oversight, because a server that
/// quietly renames an index makes `DROP INDEX` guesswork.
fn derived(parts: &[&str]) -> String {
    let joined = parts.join("_");
    fold_identifier(&joined, true).0
}

#[cfg(test)]
mod tests {
    use super::{IndexKeyPart, KeyPartName, index_name, primary_key_name, unique_constraint_name};
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

/// One action of an `ALTER TABLE`.
///
/// `ADD COLUMN` and `DROP COLUMN` are here. Every other action parses and comes back `0A000`
/// naming itself (contract C2), a type change included: the row format carries a column *count*
/// and not column identity ([ADR 0019](../../../docs/adr/0019-a-row-says-how-many-columns-it-has.md)),
/// so a column cannot change width under rows already written. `DROP COLUMN` used to be refused
/// for that same reason and no longer is — it does not need identity, because the slot never moves
/// ([ADR 0051](../../../docs/adr/0051-a-dropped-column-keeps-its-slot.md)).
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    /// `ADD [COLUMN] [IF NOT EXISTS] <column> <type>`, nullable and with no default — the only
    /// shape that needs no row rewritten.
    AddColumn {
        /// The column to append. Always nullable: `NOT NULL` and `DEFAULT` are refused by name.
        column: Column,
        /// `IF NOT EXISTS`: a column that is already there is a notice rather than a `42701`.
        if_not_exists: bool,
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
    /// `ADD CONSTRAINT <name> UNIQUE (…)`, which is what `add_unique_constraint` sends — and the
    /// setup every `remove_unique_constraint` test needs before it can remove one.
    ///
    /// **The index it builds is a constraint's**, not a `CREATE UNIQUE INDEX`'s: the two make the
    /// same index and only `IndexDef::constraint` tells them apart, which is what decides whether
    /// `DROP CONSTRAINT` or `DROP INDEX` can remove it.
    AddUnique(UniqueConstraint),
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
    AddCheck(crate::catalog::CheckDef),
    /// `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (…) REFERENCES … (…)`.
    ///
    /// The parent is named rather than resolved: nothing can turn `author_addresses` into a table
    /// id until the catalog has been read, and the executor is where that happens once.
    AddForeignKey(ForeignKey),
    /// `ENABLE`/`DISABLE TRIGGER ALL` — and it is **not** a no-op on a node with no triggers.
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
    /// So this carries whether it was `ALL`, and `USER` is accepted with nothing to record. A
    /// named trigger is `42704` where it is lowered: this node has none to name.
    SetTriggersDisabled {
        /// Whether the table's checks are suspended from here on. Stored on the table, because
        /// PostgreSQL's is stored too — `pg_trigger.tgenabled` outlives the transaction that set
        /// it and every session sees it.
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
    /// should keep ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5).
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
}
