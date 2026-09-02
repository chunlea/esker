//! A subquery written where a value goes, and the one decision it makes.
//!
//! `docs/plans/phase-12-subquery.md` §1. Five spellings arrive here — a scalar `(SELECT …)`,
//! `EXISTS`, `IN (SELECT …)`, and `ANY`/`ALL` with a subquery on the right — and they are one
//! type with one field that decides everything: **how many times the sub-plan runs**. Nothing in
//! it refers to the row outside it, so it runs once, before the cursor opens
//! ([`crate::exec::subquery::resolve`]); something does, and it runs per outer row.
//!
//! # `IN` is `= ANY`, and `NOT IN` is `<> ALL`
//!
//! Not a simplification — it is what PostgreSQL's own three-valued answers are, measured side by
//! side in `tests/corpus/pg19_subquery_expr.txt`:
//!
//! ```text
//! 1   NOT IN (100, 200, NULL)   \N        1   <> ALL (100, 200, NULL)   \N
//! 100 NOT IN (100, 200, NULL)   f         100 <> ALL (100, 200, NULL)   f
//! ```
//!
//! So [`SubqueryKind::In`] is carried as itself for the *message* it produces and for `EXPLAIN`,
//! and evaluated by the quantified rule. One implementation of the asymmetric NULL logic rather
//! than two that can drift.
//!
//! # The empty subquery is decided before the NULL
//!
//! The trap the capture exists for. `NULL IN (SELECT … no rows)` is **false**, where `NULL IN (1)`
//! is NULL — so emptiness wins over an unknown left-hand side, and an implementation that
//! short-circuited on a NULL operand (which is exactly what [`crate::plan::Expr::InList`]
//! correctly does, because PostgreSQL's grammar has no empty list) answers NULL and drops a row.

use crate::plan::{BinaryOp, Expr, Node, Select};
use crate::value::{ColumnType, Datum};

/// Which of the five spellings a subquery expression is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    /// `(SELECT …)` where a value goes. No rows is NULL; two rows is `21000`, which is an error
    /// rather than the first row.
    Scalar,
    /// `EXISTS (SELECT …)`, or `NOT EXISTS`.
    ///
    /// The one kind that reads the rows rather than the values in them, so a subquery with any
    /// number of columns is legal and a row of NULLs still counts. It is the only kind that can
    /// stop at the **first** row, which is what makes it affordable per outer row.
    Exists {
        /// `NOT EXISTS`.
        negated: bool,
    },
    /// `x IN (SELECT …)`, or `NOT IN`.
    In {
        /// `NOT IN`, which is `<> ALL` and **not** "none of them are equal" — see the module note.
        negated: bool,
    },
    /// `x <op> ANY (SELECT …)`, `x <op> SOME (…)` and `x <op> ALL (…)`.
    Quantified {
        /// The comparison, which is any of the six.
        op: BinaryOp,
        /// `ALL` rather than `ANY`/`SOME`.
        all: bool,
    },
}

impl SubqueryKind {
    /// Whether the subquery's rows are read for their **value** rather than only counted.
    ///
    /// `EXISTS` is the one that is not, and it is why a two-column subquery is legal under it and
    /// `42601` under every other kind.
    #[must_use]
    pub fn reads_a_value(self) -> bool {
        !matches!(self, SubqueryKind::Exists { .. })
    }

    /// The most rows this kind ever needs, or `None` for the kinds that need all of them.
    ///
    /// `EXISTS` needs one — the only question asked of the result is whether it is empty.
    /// A scalar needs **two**: the second row is the `21000`, so reading a third would be reading
    /// rows nobody will look at. Both are observationally identical to draining the sub-plan, and
    /// both are what make a correlated subquery affordable.
    #[must_use]
    pub fn rows_needed(self) -> Option<usize> {
        match self {
            SubqueryKind::Exists { .. } => Some(1),
            SubqueryKind::Scalar => Some(2),
            SubqueryKind::In { .. } | SubqueryKind::Quantified { .. } => None,
        }
    }

    /// What a refusal calls this, in the words a user wrote.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            SubqueryKind::Scalar => "a scalar subquery",
            SubqueryKind::Exists { negated: false } => "EXISTS",
            SubqueryKind::Exists { negated: true } => "NOT EXISTS",
            SubqueryKind::In { negated: false } => "IN (subquery)",
            SubqueryKind::In { negated: true } => "NOT IN (subquery)",
            SubqueryKind::Quantified { all: false, .. } => "ANY (subquery)",
            SubqueryKind::Quantified { all: true, .. } => "ALL (subquery)",
        }
    }
}

/// One subquery in an expression: what it is, what it compares against, and what it answered.
///
/// The last three fields are filled in by two later passes and are `None` as written, which is the
/// same shape [`crate::plan::routing::Columnar`] uses for a fragment's answer and for the same
/// reason: a value that arrives from *running* something has no business being reconstructible
/// from the statement.
#[derive(Debug, Clone)]
pub struct SubqueryExpr {
    /// Which spelling.
    pub kind: SubqueryKind,
    /// The left-hand side of `IN`/`ANY`/`ALL`. `None` for a scalar subquery and for `EXISTS`,
    /// neither of which compares against anything.
    pub operand: Option<Box<Expr>>,
    /// The sub-select as written, which is what a nested subquery inside it is planned from.
    pub select: Box<Select>,
    /// The plan built from [`SubqueryExpr::select`], filled by
    /// [`crate::exec::subquery::plan_subqueries`] before the outer plan is built — because the
    /// outer statement cannot be *typed* until this one has been.
    pub plan: Option<Box<Node>>,
    /// The name and type of the subquery's single output column, filled at the same time.
    ///
    /// `None` for an `EXISTS`, which reads no value, and only ever `None` there: every other kind
    /// is `42601` when the subquery does not have exactly one column.
    pub column: Option<(String, ColumnType)>,
    /// The subquery's answer: its single column, one entry per row — or, for an `EXISTS`, one
    /// entry per row of any value at all, because only the length is read.
    ///
    /// Filled by [`crate::exec::subquery::resolve`] before the cursor opens when the subquery is
    /// uncorrelated, and per outer row when it is not. `None` at the row evaluator is a bug in
    /// this crate and says so rather than answering "no rows" — which for a scalar subquery is a
    /// **NULL**, and a NULL looks like an answer.
    pub run: Option<Vec<Datum>>,
}

/// Two subquery expressions are equal when they were **written** the same.
///
/// Hand-written rather than derived for one reason that is not style: [`Node`] is not `PartialEq`,
/// and it is not going to be — a plan holds `RowSchema`s and key ranges whose equality is not a
/// question anybody asks. The three fields left out are all *derived from* `select` by a pass that
/// runs later, so comparing them would either say nothing new or say that one of two identical
/// statements had been planned and the other had not.
impl PartialEq for SubqueryExpr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.operand == other.operand && self.select == other.select
    }
}

impl SubqueryExpr {
    /// A subquery with nothing on its left: a scalar, or an `EXISTS`.
    #[must_use]
    pub fn bare(kind: SubqueryKind, select: Box<Select>) -> Self {
        SubqueryExpr {
            kind,
            operand: None,
            select,
            plan: None,
            column: None,
            run: None,
        }
    }

    /// A subquery with a left-hand side: `IN`, `ANY`, `ALL`.
    #[must_use]
    pub fn compared(kind: SubqueryKind, operand: Expr, select: Box<Select>) -> Self {
        SubqueryExpr {
            operand: Some(Box::new(operand)),
            ..SubqueryExpr::bare(kind, select)
        }
    }

    /// The type this expression has: the subquery's own for a scalar, `boolean` for the four that
    /// answer a question about it.
    ///
    /// `text` is the fallback for a scalar whose column has not been resolved yet, which is the
    /// same thing [`crate::exec::query::expr_type`] does for an untyped literal — the caller that
    /// cares has already failed with a better message.
    #[must_use]
    pub fn value_type(&self) -> ColumnType {
        match self.kind {
            SubqueryKind::Scalar => self.column.as_ref().map_or(ColumnType::Text, |(_, ty)| *ty),
            _ => ColumnType::Bool,
        }
    }

    /// The name PostgreSQL gives this expression in a target list, measured with `psql`:
    ///
    /// * a scalar subquery takes **the subquery's own column name** — `max`, `id`, `count`, or the
    ///   inner `AS` — which is why the name is carried beside the type rather than thrown away;
    /// * `EXISTS (…)` is `exists`;
    /// * `NOT EXISTS`, `IN (…)` and `= ANY (…)` are all `?column?`, because each of them is an
    ///   operator rather than a column.
    #[must_use]
    pub fn output_name(&self) -> Option<&str> {
        match self.kind {
            SubqueryKind::Scalar => self.column.as_ref().map(|(name, _)| name.as_str()),
            SubqueryKind::Exists { negated: false } => Some("exists"),
            _ => None,
        }
    }
}
