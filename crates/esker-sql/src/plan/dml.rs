//! `INSERT`, `UPDATE` and `DELETE`, lowered — and the `RETURNING` all three share.

use crate::plan::{Expr, SelectItem};

/// `RETURNING`, which is a target list over the rows a statement wrote.
///
/// The same [`SelectItem`] a `SELECT` projects, deliberately: `RETURNING *`, `RETURNING id`,
/// `RETURNING id AS new_id` and `RETURNING t.*` are the same four shapes with the same three
/// answers, and giving them a second type would be a second place for them to drift.
///
/// **Which row it sees is the whole of the semantics.** An `INSERT` returns the row as *stored*,
/// so a column filled from its `DEFAULT` comes back with that value; an `UPDATE` returns the row
/// as it is *after* the assignments; a `DELETE` returns the row as it was before it went. All
/// three were measured.
pub type Returning = Vec<SelectItem>;

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
    /// `RETURNING`, over the rows as stored.
    pub returning: Option<Returning>,
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
    /// `RETURNING`, over the rows **after** the assignments.
    pub returning: Option<Returning>,
}

/// `DELETE FROM t WHERE ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The table, folded.
    pub table: String,
    /// `WHERE`. Absent means every row.
    pub filter: Option<Expr>,
    /// `RETURNING`, over the rows as they were before they went.
    pub returning: Option<Returning>,
}
