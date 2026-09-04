//! `VALUES` as a **relation**: rows written into the statement rather than read from anywhere.
//!
//! Two spellings of one thing — `FROM (VALUES (1),(2)) AS t(a)` where a table goes, and `VALUES …`
//! as a whole statement — and this module is what both become. `ActiveRecord` writes the first in
//! its inserts and its `array_agg` probes; `ARRAY(VALUES (1),(2))` is the second inside a
//! constructor.
//!
//! # The relation is a synthetic [`TableDef`], which is what makes everything above it work
//!
//! One column per output column, under [`crate::catalog::DERIVED_TABLE_ID`] — the same trick
//! `plan_derived` and `table_function_def` turn on. With it `Scope` resolves `t.a`, `SELECT *`
//! expands, `EXPLAIN` prints the names and the join machinery materialises the rows, none of them
//! learning that nothing stores them.
//!
//! # The type is the first row's, and the rest are read as it
//!
//! `VALUES (1),('a')` is **not** a text column and not a union of two types: it is
//! `22P02 invalid input syntax for type integer: "a"`, the second row's literal failing to parse
//! as the first row's type. Measured. A column of nothing but NULL is `text`, which is what an
//! untyped NULL is everywhere else in this crate.
//!
//! Deciding it here rather than at lowering is deliberate: an expression's type needs a scope, and
//! a half-answer carried up from the parser would mean two places deciding one thing.

use std::sync::Arc;

use esker_keys::value::Datum;

use crate::catalog::{ColumnDef, DERIVED_TABLE_ID, TableDef};
use crate::error::{Result, SqlError};
use crate::plan::{Expr, Node, TableRef, ValuesList};
use crate::value::ColumnType;

/// The relation a `VALUES` list stands for.
pub(super) fn def(entry: &TableRef) -> Result<Arc<TableDef>> {
    let list = list_of(entry)?;
    let name = entry.referred_as().to_owned();
    let columns = list
        .columns
        .iter()
        .zip(column_types(list)?)
        .map(|(name, ty)| ColumnDef {
            name: name.clone(),
            ty,
            typmod: crate::value::NO_TYPMOD,
            default_expr: None,
            not_null: false,
            default: None,
            missing: None,
            generated: None,
            comment: None,
            dropped: false,
            user_type: None,
        })
        .collect();
    Ok(Arc::new(TableDef {
        // Synthetic and never stored, so its persistence is the default.
        persistence: crate::catalog::Persistence::Permanent,
        id: DERIVED_TABLE_ID,
        name,
        columns,
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
        child_scans: Vec::new(),
        excludes: Vec::new(),
        partition_by: None,
        partition_bound: None,
        comment: None,
        primary_key_comment: None,
        enums: std::collections::BTreeMap::new(),
    }))
}

/// The node, with every row read as its column's own type.
///
/// The coercion is here rather than in the cursor because this is where the types are known and
/// because PostgreSQL raises the `22P02` before it runs anything: a statement that cannot read its
/// own constants never reaches execution on a real server either.
pub(super) fn node(entry: &TableRef, def: &TableDef) -> Result<Node> {
    let mut list = list_of(entry)?.clone();
    for row in &mut list.rows {
        for (expr, column) in row.iter_mut().zip(&def.columns) {
            super::query::give_branch_type(expr, column.ty)?;
        }
    }
    Ok(Node::Values {
        list: Box::new(list),
        columns: def.row_schema(),
    })
}

/// The rows, evaluated once when the cursor opens.
///
/// Against **no row**, exactly as a table function's arguments are: a constant list cannot read a
/// column, and an expression that tries is the refusal `resolve` already raises rather than a
/// silent NULL.
///
/// **In the transaction**, because `CURRENT_TIMESTAMP` is the transaction's instant and reads it
/// from there. `FROM (VALUES (CURRENT_TIMESTAMP = transaction_timestamp())) AS t(a)` is `t` on a
/// real server, and without the transaction here it was the internal error of a clock function
/// that reached an evaluator with nothing to read.
pub(super) fn rows(list: &ValuesList, txn: &dyn crate::backend::Txn) -> Result<Vec<Vec<Datum>>> {
    list.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|expr| super::cursor::evaluate_in_txn(expr, &[], txn))
                .collect()
        })
        .collect()
}

/// One type per column, taken from the first row.
fn column_types(list: &ValuesList) -> Result<Vec<ColumnType>> {
    let Some(first) = list.rows.first() else {
        return Ok(Vec::new());
    };
    first
        .iter()
        .map(|expr| match expr {
            // The two spellings that carry no type of their own are `text`, which is what an
            // `unknown` resolves to when nothing else in the statement types it.
            Expr::Literal(literal) => {
                Ok(super::query::literal_type(literal).unwrap_or(ColumnType::Text))
            }
            other => super::query::expr_type(other, &super::query::Scope::empty()),
        })
        .collect()
}

/// The list an entry carries, or the internal error of a values entry that lost it.
fn list_of(entry: &TableRef) -> Result<&ValuesList> {
    entry.values.as_deref().ok_or_else(|| {
        SqlError::Internal("a VALUES relation reached the planner without its rows".to_owned())
    })
}
