//! `SELECT`, lowered, and the physical plan it becomes.
//!
//! Two shapes live here and the difference between them is the planner. [`Select`] is the
//! statement as written — names, not positions; a `WHERE` clause, not an access path. [`Node`] is
//! what the executor pulls rows through, and every column reference in it has been resolved to a
//! position and every access path chosen.
//!
//! # What the planner is allowed to decide
//!
//! Rule-based, and the rules are few enough to name:
//!
//! 1. A `WHERE` that pins the **whole primary key** to constants is a point read, not a scan.
//! 2. A `WHERE` that bounds the **first primary key column** narrows the scanned range instead of
//!    filtering every row out of it.
//! 3. A `WHERE` that pins the whole key of a **unique index** is a lookup in that index followed by
//!    a point read of the row it names.
//!
//! Anything else is a sequential scan with a filter over it, which is always correct and sometimes
//! slow. `EXPLAIN` prints which one was chosen, because a plan a user cannot see is a plan they
//! cannot fix.

use crate::plan::{Expr, Literal};
use crate::value::{ColumnType, Datum};

/// `SELECT`, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// The table, or `None` for `SELECT 1` — a single row of no table at all, which drivers use to
    /// check a connection.
    pub from: Option<String>,
    /// What to return.
    pub projection: Vec<SelectItem>,
    /// `WHERE`.
    pub filter: Option<Expr>,
    /// `ORDER BY`, in significance order.
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`.
    pub limit: Option<Expr>,
    /// `OFFSET`.
    pub offset: Option<Expr>,
}

/// One item in a target list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// An expression, with the name it will be reported under. `None` means PostgreSQL's own
    /// default: a bare column keeps its name and anything else is `?column?`.
    Expr {
        /// What to evaluate.
        expr: Expr,
        /// `AS name`.
        alias: Option<String>,
    },
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// What to sort on.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST` / `NULLS LAST` when the user said so. `None` means PostgreSQL's default,
    /// which is last for ascending and first for descending — the two are not the same rule
    /// written twice, they are one rule about where NULL sits in the *value* order.
    pub nulls_first: Option<bool>,
}

/// A physical plan node: rows come out of it one at a time.
#[derive(Debug, Clone)]
pub enum Node {
    /// One row of no table, for `SELECT 1`.
    OneRow,
    /// Every row of a table, in primary key order, over a key range.
    SeqScan {
        /// The table.
        table_id: u64,
        /// Column types, for decoding.
        columns: Vec<ColumnType>,
        /// Inclusive start of the scanned range.
        start: Vec<u8>,
        /// Exclusive end.
        end: Vec<u8>,
        /// What the range came from, for `EXPLAIN` — a narrowed range is the interesting case.
        narrowed: bool,
    },
    /// One row, by its primary key.
    PointGet {
        /// The table.
        table_id: u64,
        /// Column types, for decoding.
        columns: Vec<ColumnType>,
        /// The key values, in key order.
        key: Vec<Datum>,
    },
    /// One row, found through a unique index.
    IndexLookup {
        /// The table.
        table_id: u64,
        /// Column types, for decoding the row.
        columns: Vec<ColumnType>,
        /// The index.
        index_id: u64,
        /// Its name, for `EXPLAIN`.
        index_name: String,
        /// The indexed values, in index order.
        key: Vec<Datum>,
        /// Types of the primary key columns, for decoding what the entry points at.
        primary_key_types: Vec<ColumnType>,
    },
    /// Keeps the rows its predicate is true for. NULL is not true.
    Filter {
        /// Where the rows come from.
        input: Box<Node>,
        /// The predicate, with columns already resolved to positions.
        predicate: Expr,
    },
    /// Rebuilds each row as the target list.
    Project {
        /// Where the rows come from.
        input: Box<Node>,
        /// One expression per output column.
        exprs: Vec<Expr>,
    },
    /// Buffers its input and orders it. The one node that cannot stream: the last row of the input
    /// can be the first row of the output.
    Sort {
        /// Where the rows come from.
        input: Box<Node>,
        /// Keys, in significance order.
        keys: Vec<SortKey>,
    },
    /// `LIMIT` and `OFFSET`.
    Limit {
        /// Where the rows come from.
        input: Box<Node>,
        /// How many to skip.
        offset: usize,
        /// How many to return, or `None` for all of them.
        limit: Option<usize>,
    },
}

/// One resolved sort key.
#[derive(Debug, Clone)]
pub struct SortKey {
    /// What to sort on.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// Whether NULLs come first, already resolved from the default.
    pub nulls_first: bool,
}

impl Node {
    /// The `EXPLAIN` tree, one line per node, indented by depth.
    ///
    /// PostgreSQL's own output carries cost estimates; there is no cost model here, so there are no
    /// costs. What it does carry is the access path and the condition, which is the part a user
    /// changes their schema over — and it is written for a *user*, so `columns` is needed to turn
    /// the positions the executor works in back into the names they typed.
    #[must_use]
    pub fn explain(&self, table: &str, columns: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        self.explain_into(table, columns, 0, &mut lines);
        lines
    }

    fn explain_into(&self, table: &str, columns: &[String], depth: usize, lines: &mut Vec<String>) {
        let indent = "  ".repeat(depth);
        let (line, child, extra) = match self {
            Node::OneRow => ("Result".to_owned(), None, None),
            Node::SeqScan { narrowed, .. } => (
                format!("Seq Scan on {table}"),
                None,
                narrowed.then(|| "Range: narrowed by the primary key".to_owned()),
            ),
            Node::PointGet { .. } => (format!("Point Get on {table}"), None, None),
            Node::IndexLookup { index_name, .. } => (
                format!("Index Lookup on {table}"),
                None,
                Some(format!("Index: {index_name}")),
            ),
            Node::Filter { input, predicate } => (
                "Filter".to_owned(),
                Some(input),
                Some(format!("Condition: {}", render(predicate, columns))),
            ),
            Node::Project { input, exprs } => (
                format!("Project ({} columns)", exprs.len()),
                Some(input),
                None,
            ),
            Node::Sort { input, keys } => (
                "Sort".to_owned(),
                Some(input),
                Some(format!(
                    "Keys: {}",
                    keys.iter()
                        .map(|key| format!(
                            "{}{}",
                            render(&key.expr, columns),
                            if key.descending { " DESC" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            ),
            Node::Limit {
                input,
                offset,
                limit,
            } => (
                match limit {
                    Some(limit) => format!("Limit {limit} Offset {offset}"),
                    None => format!("Offset {offset}"),
                },
                Some(input),
                None,
            ),
        };
        lines.push(format!("{indent}{line}"));
        if let Some(extra) = extra {
            lines.push(format!("{indent}  {extra}"));
        }
        if let Some(child) = child {
            child.explain_into(table, columns, depth + 1, lines);
        }
    }
}

/// An expression as `EXPLAIN` shows it.
///
/// Written for a person: a column is its **name**, not the position the executor works in, and a
/// literal is the value, not a `Debug` rendering of the node holding it. Not SQL that would
/// re-parse — PostgreSQL's own `EXPLAIN` output is not either — but every token in it is one the
/// user wrote or could have written.
fn render(expr: &Expr, columns: &[String]) -> String {
    match expr {
        Expr::Literal(literal) => render_literal(literal),
        Expr::Parameter(number) => format!("${number}"),
        Expr::Column(name) => name.clone(),
        // Resolved to a position by the planner; put the name back for the reader. A position with
        // no name behind it can only be a bug, and saying so beats printing a number.
        Expr::Ordinal { at, .. } => columns
            .get(*at)
            .cloned()
            .unwrap_or_else(|| format!("<column {at}>")),
        Expr::Binary { op, left, right } => format!(
            "({} {} {})",
            render(left, columns),
            op.symbol(),
            render(right, columns)
        ),
        Expr::Not(operand) => format!("NOT {}", render(operand, columns)),
        Expr::IsNull { operand, negated } => format!(
            "{} IS {}NULL",
            render(operand, columns),
            if *negated { "NOT " } else { "" }
        ),
    }
}

/// A literal as it would have been written. A string is quoted, because `WHERE e = c` and
/// `WHERE e = 'c'` mean different things and a plan that cannot tell them apart is a plan that
/// cannot be checked against the query.
fn render_literal(literal: &Literal) -> String {
    match literal {
        Literal::Null => "NULL".to_owned(),
        Literal::Integer(value) => value.to_string(),
        Literal::Decimal(digits) => digits.clone(),
        Literal::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_owned(),
        Literal::String(text) => quoted(text),
        // Already resolved against a column's type, so it prints the way that type prints — the
        // same text a client would see the value as in a result.
        Literal::Typed(value) => match value.as_ref() {
            Datum::Null => "NULL".to_owned(),
            Datum::Text(text) => quoted(text),
            other => other.to_text().unwrap_or_else(|| "NULL".to_owned()),
        },
    }
}

/// SQL's own quoting: a single quote is doubled.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
