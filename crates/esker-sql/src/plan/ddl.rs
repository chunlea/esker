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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub ty: ColumnType,
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
    /// `DEFERRABLE INITIALLY IMMEDIATE`, which is **not deferred**.
    ///
    /// It checks at the statement like any other unique constraint; what differs is `condeferrable`
    /// and what `pg_get_constraintdef` prints — which keeps `DEFERRABLE` and drops the
    /// `INITIALLY IMMEDIATE` half, so the text out is not the text in. `INITIALLY DEFERRED` really
    /// waits for `COMMIT` and is refused by name where this is lowered.
    pub deferrable: bool,
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
/// Only `ADD COLUMN` is here. Every other action parses and comes back `0A000` naming itself
/// (contract C2) — `DROP COLUMN` and a type change because the row format carries a column
/// *count* and not column identity ([ADR 0019](../../../docs/adr/0019-a-row-says-how-many-columns-it-has.md)),
/// and the rest because nothing below this crate implements them yet.
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
