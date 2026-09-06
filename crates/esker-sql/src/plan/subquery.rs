//! A subquery written where a value goes, and the one decision it makes.
//!
//! `docs/plans/phase-12-subquery.md` §1. Five spellings arrive here — a scalar `(SELECT …)`,
//! `EXISTS`, `IN (SELECT …)`, and `ANY`/`ALL` with a subquery on the right — and they are one
//! type with one field that decides everything: **how many times the sub-plan runs**. Nothing in
//! it refers to the row outside it, so it runs once, before the cursor opens
//! (`crate::exec::subquery::resolve`); something does, and it runs per outer row.
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

use std::sync::Arc;

use crate::catalog::TableDef;
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
    /// `ARRAY( SELECT … )`: the subquery's rows, in its own order, as one array.
    ///
    /// **Not `array_agg`, and the difference is visible.** Over zero rows this is `{}` and
    /// `array_agg` is NULL — measured in one statement, `array_agg(x) IS NULL` is `t` and
    /// `ARRAY(SELECT x …) IS NULL` is `f` over the same empty input. It is *why* `ActiveRecord`
    /// writes boot statement 32 this way, and an implementation that rewrote one into the other
    /// would return NULL where Rails expects an empty array.
    ///
    /// A NULL row is a NULL **element**, so `ARRAY(SELECT NULL::int4)` is `{NULL}` — a third
    /// answer distinct from both `{}` and NULL.
    Array,
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
            // Every row, because every row is an element.
            SubqueryKind::In { .. } | SubqueryKind::Quantified { .. } | SubqueryKind::Array => None,
        }
    }

    /// The comparison this kind puts between its operand and each of the subquery's values, or
    /// `None` for the two kinds that have no operand.
    ///
    /// `IN` is `=` and `NOT IN` is `<>`, which is the other half of "`NOT IN` is `<> ALL`".
    #[must_use]
    pub fn comparison(self) -> Option<BinaryOp> {
        match self {
            // Neither compares its rows against anything: one *is* the value, the other counts
            // them, and this one collects them.
            SubqueryKind::Scalar | SubqueryKind::Exists { .. } | SubqueryKind::Array => None,
            SubqueryKind::In { negated: false } => Some(BinaryOp::Eq),
            SubqueryKind::In { negated: true } => Some(BinaryOp::NotEq),
            SubqueryKind::Quantified { op, .. } => Some(op),
        }
    }

    /// What a refusal calls this, in the words a user wrote.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            SubqueryKind::Scalar => "a scalar subquery",
            SubqueryKind::Array => "ARRAY (subquery)",
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
    /// The left-hand side of `IN`/`ANY`/`ALL`. **Empty** for a scalar subquery and for `EXISTS`,
    /// neither of which compares against anything.
    ///
    /// **A list, because the left-hand side may be a row**: `(a, b) IN (SELECT x, y …)` is what
    /// `ActiveRecord` sends for a composite primary key, and it compares column by column. One
    /// operand is the ordinary case and reads as a list of one; there is deliberately no general
    /// row-value expression, which would be a wide surface for this one shape.
    pub operands: Vec<Expr>,
    /// The sub-select as written, which is what a nested subquery inside it is planned from.
    pub select: Box<Select>,
    /// The plan built from [`SubqueryExpr::select`], filled by
    /// `crate::exec::subquery::plan_subqueries` before the outer plan is built — because the
    /// outer statement cannot be *typed* until this one has been.
    pub plan: Option<Box<Node>>,
    /// The name and type of the subquery's single output column, filled at the same time.
    ///
    /// `None` for an `EXISTS`, which reads no value, and only ever `None` there: every other kind
    /// is `42601` when the subquery does not have exactly one column.
    pub column: Option<(String, ColumnType)>,
    /// Whether anything in the sub-plan names a column of the row **outside** it.
    ///
    /// The one field that decides how many times this runs. `false` and it runs once, before the
    /// cursor opens; `true` and it runs per outer row, with the outer values substituted in first.
    /// Filled by `crate::exec::subquery::plan_subqueries` from the plan it built — a fact about
    /// the plan rather than a reading of the statement, so a reference that resolved to the inner
    /// scope after all does not count (`SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE
    /// b.a_id = id)` is **not** correlated: `id` is `b`'s).
    pub correlated: bool,
    /// The subquery's answer: one entry per row, each the whole row.
    ///
    /// **Rows and not values**, because the left-hand side may be one: `(a, b) IN (SELECT x, y …)`
    /// compares column by column. Every other kind reads column 0 and ignores the rest, and an
    /// `EXISTS` reads only the length.
    ///
    /// Filled by `crate::exec::subquery::resolve` before the cursor opens when the subquery is
    /// uncorrelated, and per outer row when it is not. `None` at the row evaluator is a bug in
    /// this crate and says so rather than answering "no rows" — which for a scalar subquery is a
    /// **NULL**, and a NULL looks like an answer.
    pub run: Option<Vec<Vec<Datum>>>,
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
        self.kind == other.kind && self.operands == other.operands && self.select == other.select
    }
}

impl SubqueryExpr {
    /// A subquery with nothing on its left: a scalar, or an `EXISTS`.
    #[must_use]
    pub fn bare(kind: SubqueryKind, select: Box<Select>) -> Self {
        SubqueryExpr {
            kind,
            operands: Vec::new(),
            select,
            plan: None,
            column: None,
            correlated: false,
            run: None,
        }
    }

    /// A subquery with a left-hand side: `IN`, `ANY`, `ALL`.
    #[must_use]
    pub fn compared(kind: SubqueryKind, operand: Expr, select: Box<Select>) -> Self {
        SubqueryExpr::compared_row(kind, vec![operand], select)
    }

    /// The same with a **row** on the left: `(a, b) IN (SELECT x, y …)`.
    #[must_use]
    pub fn compared_row(kind: SubqueryKind, operands: Vec<Expr>, select: Box<Select>) -> Self {
        SubqueryExpr {
            operands,
            ..SubqueryExpr::bare(kind, select)
        }
    }

    /// The type this expression has: the subquery's own for a scalar, `boolean` for the four that
    /// answer a question about it.
    ///
    /// `text` is the fallback for a scalar whose column has not been resolved yet, which is the
    /// same thing `crate::exec::query::expr_type` does for an untyped literal — the caller that
    /// cares has already failed with a better message.
    #[must_use]
    pub fn value_type(&self) -> ColumnType {
        match self.kind {
            SubqueryKind::Scalar => self.column.as_ref().map_or(ColumnType::Text, |(_, ty)| *ty),
            // **The array of whatever the subquery's one column is.** PostgreSQL does not encode
            // dimensionality in the name, so a nested one is still `integer[]` — measured,
            // `ARRAY(SELECT ARRAY(SELECT 1))` is `{{1}}` of type `integer[]`.
            SubqueryKind::Array => self
                .column
                .as_ref()
                .and_then(|(_, ty)| esker_keys::array::ArrayValue::array_of(*ty))
                .unwrap_or(ColumnType::TextArray),
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
            // `array` on a real server, whatever the subquery's own column is called. Measured.
            SubqueryKind::Array => Some("array"),
            _ => None,
        }
    }
}

/// A **derived table**: `FROM (SELECT …) AS t`, and the alias list that renames its columns.
///
/// It is the same subquery the rest of this module is about, standing where a relation goes rather
/// than where a value goes — so what it needs is not a value but a *shape*, and that is exactly
/// what the two filled-in fields are. Once they are there, everything above knows it as a table:
/// name resolution, the `SELECT *` expansion, `EXPLAIN`'s column names and the join machinery all
/// work against `def` and never learn that nothing stores it.
///
/// # What the capture settled
///
/// `tests/corpus/pg19_subquery_from.txt`, and two of the three would have been guessed wrong:
///
/// * **the alias is optional on 19.** It was mandatory before PostgreSQL 16 and the documentation
///   a reader is most likely to find still says so. Without one the relation simply has no name to
///   qualify with, which is why an absent alias is an empty [`crate::plan::TableRef::name`] rather
///   than a generated one — an invented name is a name a user could collide with.
/// * **a column alias list may be SHORTER than the target list.** `AS t(a)` over two columns
///   renames the first and leaves the second alone; only a *longer* one is an error, and it is
///   `42P10 table "t" has 1 columns available but 2 columns specified`. Refusing a short one, which
///   reads like the obvious symmetry, would refuse a statement a real server runs.
/// * and a name the list replaced is **gone**: after `AS t(a, b)`, `t.id` is `42703`.
#[derive(Debug, Clone)]
pub struct Derived {
    /// The sub-select as written.
    pub select: Box<Select>,
    /// `AS t(a, b)` — the column names, in order, or empty for a bare alias.
    pub columns: Vec<String>,
    /// Whether this derived table is an inlined CTE, which changes **one message and nothing
    /// else**: a wrong-length column alias list is `WITH query "t" has 1 columns available but 2
    /// columns specified` for a CTE and `table "t" has …` for a `FROM (SELECT …)`. Same SQLSTATE,
    /// two sentences, measured — and a client that greps the text sees two.
    pub cte: bool,
    /// The plan its rows come from, filled by `crate::exec::subquery::plan_subqueries`.
    pub plan: Option<Box<Node>>,
    /// The relation it looks like from above: one column per output column of the sub-select,
    /// under [`crate::catalog::DERIVED_TABLE_ID`]. Filled at the same time.
    pub def: Option<Arc<TableDef>>,
}

/// Two derived tables are equal when they were **written** the same — the same reason
/// [`SubqueryExpr`]'s equality is hand-written, and the same two fields left out.
impl PartialEq for Derived {
    fn eq(&self, other: &Self) -> bool {
        self.select == other.select && self.columns == other.columns && self.cte == other.cte
    }
}

impl Derived {
    /// A derived table as the parser saw it, with nothing planned yet.
    #[must_use]
    pub fn new(select: Box<Select>, columns: Vec<String>) -> Self {
        Derived {
            select,
            columns,
            cte: false,
            plan: None,
            def: None,
        }
    }

    /// The same thing, arrived at by inlining a `WITH` item.
    #[must_use]
    pub fn from_cte(select: Box<Select>, columns: Vec<String>) -> Self {
        Derived {
            cte: true,
            ..Derived::new(select, columns)
        }
    }
}
