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

use crate::catalog::fold_identifier;
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
    /// Every `UNIQUE` constraint, from a column option or a table constraint.
    pub unique: Vec<UniqueConstraint>,
}

/// One declared column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Folded.
    pub name: String,
    /// One of the six.
    pub ty: ColumnType,
    /// Whether `NOT NULL` was declared. A primary key column becomes `NOT NULL` whether or not it
    /// said so, which the executor applies.
    pub not_null: bool,
    /// `DEFAULT <constant>`, already read as a value of the column's own type.
    ///
    /// A constant, and the lowering is where that is enforced: a **volatile** default such as
    /// `random()` differs per row and so cannot be one value in the catalog, and an unfolded
    /// expression such as `(1+1)` would need a folder this crate does not have. Both are `0A000`
    /// naming what they are, rather than a value that is wrong for every row but the first.
    pub default: Option<Datum>,
}

/// A `UNIQUE` constraint, which becomes a unique index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// The name it was given, or `None` for PostgreSQL's derived one.
    pub name: Option<String>,
    /// Column names, in key order.
    pub columns: Vec<String>,
}

/// `DROP TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTable {
    /// One or more, folded. PostgreSQL takes a list.
    pub names: Vec<String>,
    /// `IF EXISTS`: a missing table is a notice rather than a `42P01`.
    pub if_exists: bool,
}

/// `CREATE INDEX`, including `CREATE UNIQUE INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndex {
    /// The name it was given, or `None` for PostgreSQL's derived one.
    pub name: Option<String>,
    /// The table it is on, folded.
    pub table: String,
    /// Column names, in key order.
    pub columns: Vec<String>,
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
}

/// `DROP INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndex {
    /// One or more, folded.
    pub names: Vec<String>,
    /// `IF EXISTS`.
    pub if_exists: bool,
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

/// `<table>_<column>…_idx`, PostgreSQL's name for an unnamed index.
#[must_use]
pub fn index_name(table: &str, columns: &[String]) -> String {
    let mut parts = vec![table];
    parts.extend(columns.iter().map(String::as_str));
    parts.push("idx");
    derived(&parts)
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
    use super::{index_name, primary_key_name, unique_constraint_name};
    use crate::catalog::MAX_IDENTIFIER_BYTES;

    /// The names a real PostgreSQL 19 gave a table with a primary key, a `UNIQUE` column and two
    /// indexes. A client matching on a constraint name would not recognise anything else.
    #[test]
    fn derived_names_are_the_ones_postgresql_derives() {
        assert_eq!(primary_key_name("mixed"), "mixed_pkey");
        assert_eq!(
            unique_constraint_name("mixed", &["val".into()]),
            "mixed_val_key"
        );
        assert_eq!(index_name("mixed", &["a".into()]), "mixed_a_idx");
        assert_eq!(
            index_name("mixed", &["a".into(), "b".into()]),
            "mixed_a_b_idx"
        );
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    /// `ADD [COLUMN] [IF NOT EXISTS] <column> <type>`, nullable and with no default — the only
    /// shape that needs no row rewritten.
    AddColumn {
        /// The column to append. Always nullable: `NOT NULL` and `DEFAULT` are refused by name.
        column: Column,
        /// `IF NOT EXISTS`: a column that is already there is a notice rather than a `42701`.
        if_not_exists: bool,
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
}
