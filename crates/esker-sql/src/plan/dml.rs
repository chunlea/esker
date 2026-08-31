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
