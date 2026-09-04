//! `COMMENT ON TABLE | COLUMN | INDEX`, and where a comment is kept.
//!
//! A comment is a **field of the object it is about** — the table record carries its own, its
//! primary key's, one per column and one per index (`crate::catalog::record`, version 21). That is
//! not how PostgreSQL stores them: there a comment is a row in `pg_description` keyed by
//! `(classoid, objoid, objsubid)`, and the rows are cleaned up by the dependency machinery when
//! the object goes away. Here the field *is* the dependency: a `RENAME` keeps it, a `DROP COLUMN`
//! takes it away, and dropping a table takes all of them, none of it written anywhere (ADR 0049).
//!
//! # Two spellings of "no comment", and PostgreSQL has only one
//!
//! `COMMENT ON … IS NULL` and `COMMENT ON … IS ''` both leave nothing behind: a real server
//! deletes the `pg_description` row rather than storing an empty comment, so a comment that *is*
//! the empty string cannot be observed there. Measured, and it is why an empty string needs no
//! representation of its own here.

use crate::backend::Txn;
use crate::catalog::{self, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::{Executor, Outcome};
use crate::plan::{Comment, CommentObject};

/// Sets or removes one comment.
pub(super) fn comment(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    statement: &Comment,
) -> Result<Outcome> {
    // A `pg_catalog` relation is a relation, so the name resolves — and writing one is `42501`,
    // the same answer every other write to a system catalog gets here.
    catalog::pg_catalog::refuse_write(&statement.name)?;
    // `IS ''` is `IS NULL`: see the module doc.
    let text = statement
        .comment
        .clone()
        .filter(|comment| !comment.is_empty());
    let relation = existing(executor, txn, statement)?;
    let table_id = match relation {
        catalog::Relation::Table { table_id }
        | catalog::Relation::Index { table_id, .. }
        | catalog::Relation::PrimaryKey { table_id } => table_id,
        // A sequence is a relation whose name resolves; its kind is wrong for `TABLE`, `COLUMN`
        // and `INDEX`, and right only for the `SEQUENCE` this node cannot store.
        catalog::Relation::Sequence { .. } if statement.object == CommentObject::Sequence => {
            return Err(SqlError::unsupported("COMMENT ON SEQUENCE"));
        }
        // A view joins it: **`COMMENT ON TABLE` over one is `42809 "v" is not a table` with no
        // `HINT`** — measured, and unlike the `DROP` verbs, which do hint. So both take the path
        // every other wrong kind takes rather than a sentence of their own.
        catalog::Relation::Sequence { .. } | catalog::Relation::View { .. } => {
            return Err(wrong_kind(statement));
        }
    };
    // The two kinds that exist here only to name themselves in a `42809`. A sequence's own record
    // has no field for a comment, so even the right kind is a refusal — said out loud rather than
    // accepted and then unreadable.
    if statement.object == CommentObject::Sequence
        && matches!(relation, catalog::Relation::Sequence { .. })
    {
        return Err(SqlError::unsupported("COMMENT ON SEQUENCE"));
    }
    if matches!(
        statement.object,
        CommentObject::Sequence | CommentObject::View
    ) {
        return Err(wrong_kind(statement));
    }
    let table = executor.table_by_id(txn, table_id)?;
    let mut updated = (*table).clone();
    match (statement.object, &relation) {
        (CommentObject::Table, catalog::Relation::Table { .. }) => updated.comment = text,
        (CommentObject::Column, catalog::Relation::Table { .. }) => {
            set_column_comment(&mut updated, statement, text)?;
        }
        (CommentObject::Index, catalog::Relation::Index { index_id, .. }) => {
            let index = updated
                .indexes
                .iter_mut()
                .find(|index| index.id == *index_id)
                .ok_or_else(|| SqlError::UndefinedIndex(statement.name.clone()))?;
            index.comment = text;
        }
        // `t_pkey` is a relation a client can name and there is no index behind it, so its comment
        // lives on the table (`TableDef::primary_key_comment`).
        (CommentObject::Index, catalog::Relation::PrimaryKey { .. }) => {
            updated.primary_key_comment = text;
        }
        _ => return Err(wrong_kind(statement)),
    }
    catalog::replace_table(txn, executor.tenant, &table, &updated)?;
    Ok(Outcome::done("COMMENT"))
}

/// `COMMENT ON COLUMN t.c`, which is the only one of the three that names two things.
fn set_column_comment(
    table: &mut TableDef,
    statement: &Comment,
    text: Option<String>,
) -> Result<()> {
    let name = statement.column.as_deref().unwrap_or_default();
    let column = table
        .columns
        .iter_mut()
        // A tombstoned column is not one a user can comment on: `DROP COLUMN` keeps the slot and
        // the stored name, and `COMMENT ON COLUMN t.gone` must be the `42703` every other clause
        // gives (ADR 0051).
        .find(|column| column.name == name && !column.dropped)
        .ok_or_else(|| SqlError::UndefinedColumnInRelation {
            column: name.to_owned(),
            relation: statement.name.clone(),
        })?;
    column.comment = text;
    Ok(())
}

/// The relation the statement names, or `42P01`.
///
/// **An `EXCLUDE` constraint's index is refused by name.** It is a relation a client can name
/// (`RelKind::Exclusion`) but it is synthesised from the constraint rather than stored, so there is
/// no record to put a comment in — a gap, said out loud, rather than a comment that is accepted
/// and then cannot be read back.
fn existing(executor: &Executor, txn: &dyn Txn, statement: &Comment) -> Result<catalog::Relation> {
    match executor.catalog_view(txn)?.relation(&statement.name)? {
        Some(relation) => Ok(relation),
        None => Err(SqlError::UndefinedTable(statement.name.clone())),
    }
}

/// `42809 "x" is not an index`, with **no hint**.
///
/// PostgreSQL hints at `DROP` because there is another `DROP` to suggest; for a comment there is
/// nothing to suggest, and it sends the message alone. Measured, both ways.
fn wrong_kind(statement: &Comment) -> SqlError {
    SqlError::WrongObjectType {
        name: statement.name.clone(),
        expected: statement.object.article_and_name(),
        found: "COMMENT",
    }
}
