//! `INSERT`, lowered.

use crate::plan::Expr;

/// `INSERT INTO t (cols) VALUES (...), (...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The table, folded.
    pub table: String,
    /// The column list as written, or `None` for `INSERT INTO t VALUES ...`, which means every
    /// column in declaration order.
    pub columns: Option<Vec<String>>,
    /// One row per `VALUES` tuple. A row may be shorter than the column list — PostgreSQL fills
    /// the rest with NULL rather than refusing — but never longer.
    pub rows: Vec<Vec<Expr>>,
}

/// `UPDATE t SET a = ..., b = ... WHERE ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The table, folded.
    pub table: String,
    /// Column name and the expression to put in it, in the order written. PostgreSQL evaluates
    /// every one against the row as it was *before* the statement, so `SET a = b, b = a` swaps
    /// them rather than assigning `a` twice.
    pub assignments: Vec<(String, Expr)>,
    /// `WHERE`. Absent means every row.
    pub filter: Option<Expr>,
}

/// `DELETE FROM t WHERE ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The table, folded.
    pub table: String,
    /// `WHERE`. Absent means every row.
    pub filter: Option<Expr>,
}
