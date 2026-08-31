//! Expressions, and the one rule that decides what a literal means.
//!
//! A literal in SQL has no type of its own until something gives it one. `'2024-01-01'` is a date
//! in a `timestamptz` column and six characters in a `text` one, and PostgreSQL calls that state
//! `unknown` and resolves it from context. This module keeps literals in that state — [`Literal`]
//! is what was *written* — and [`Literal::assign`] is where a column's type resolves it.
//!
//! That is not a simplification of PostgreSQL's rules, it is the useful half of them, and it is
//! what lets the whole value layer be reused: assigning a string literal to any of the six types is
//! [`crate::value::Datum::from_text`], which is already checked against a real server in both
//! directions.
//!
//! # What a literal will and will not become
//!
//! The conversions here were measured. Some are less obvious than they look:
//!
//! * **`true` in a `text` column stores `true`, not `t`.** The output function writes one
//!   character and the *assignment cast* writes the word, and they are different functions —
//!   the same split that made `bool` the one type where capturing a `::text` cast taught the wrong
//!   answer (`crate::value`).
//! * **`1.5` in a `text` column stores `1.5`** — the digits as written. PostgreSQL types a decimal
//!   literal `numeric`, and `numeric`'s text is its own digits, not a float's rendering of them.
//!   So the literal keeps its source text.
//! * **A decimal literal in an `int8` column is refused**, though PostgreSQL rounds it. Getting
//!   `numeric`'s rounding wrong is a silently wrong number: PostgreSQL rounds half away from zero
//!   (`2.5` becomes `3`) where a `float8` would round half to even (`2.5` becomes `2`), and the
//!   only way to be sure is to have `numeric`, which phase 6a does not. Contract C2 names it.

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum};

/// An expression, as far as phase 6a needs one.
///
/// Arithmetic is deliberately absent. `a + 1` is refused by name rather than implemented, because
/// every operator brings its own overflow, division and type-resolution rules and each of them is
/// a way to return a confidently wrong number. §3's scope is projection, `WHERE`, `ORDER BY`,
/// `LIMIT` and `OFFSET`, and this is exactly what those need.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A constant as written.
    Literal(Literal),
    /// `$1`, one-based as PostgreSQL writes it.
    Parameter(u32),
    /// A column of the row being evaluated, by name. The planner resolves it to a position.
    Column {
        /// The table it was qualified with — `a` in `a.id` — or `None` for a bare name.
        ///
        /// Carried rather than dropped, which it used to be. With one table in a query a
        /// qualifier adds nothing *when it is right*, and the first version of this threw it away
        /// on that argument; the argument is wrong, because it also throws away the case where it
        /// is **not** right. `SELECT wrong.a FROM t` is `42P01` on a real server and was answered
        /// here as though the user had written `a`.
        table: Option<String>,
        /// The column's name.
        name: String,
    },
    /// A column already resolved to its position, which is what the executor evaluates.
    Ordinal {
        /// Position in the row.
        at: usize,
        /// The column's type, so a comparison against it can resolve a literal.
        ty: ColumnType,
    },
    /// A comparison or a logical connective.
    Binary {
        /// Which one.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `NOT x`.
    Not(Box<Expr>),
    /// `x IS NULL`, or `IS NOT NULL` when negated. Never NULL itself — that is the whole point of
    /// the operator, and the reason `x = NULL` is not a way to write it.
    IsNull {
        /// What is being tested.
        operand: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
}

/// The operators phase 6a evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `=`.
    Eq,
    /// `<>`, and `!=` which PostgreSQL treats as the same operator.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
    /// `AND`.
    And,
    /// `OR`.
    Or,
}

impl BinaryOp {
    /// The symbol, for the `operator does not exist: text = integer` message.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
        }
    }

    /// Whether this is a comparison rather than a connective.
    #[must_use]
    pub fn is_comparison(self) -> bool {
        !matches!(self, BinaryOp::And | BinaryOp::Or)
    }
}

/// A constant, still untyped.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `NULL`, which fits every column and no type.
    Null,
    /// An integer.
    Integer(i64),
    /// A decimal, kept as written — see the module note on why the digits matter.
    Decimal(String),
    /// A quoted string: PostgreSQL's `unknown`, and the reason [`Literal::assign`] can lean on
    /// [`Datum::from_text`].
    String(String),
    /// `TRUE` or `FALSE`.
    Bool(bool),
    /// A value the planner already resolved against a column's type — a `timestamptz` read out of
    /// a quoted literal, say. It carries no ambiguity left to resolve, which is the point: the
    /// executor evaluates it and nothing re-reads the text.
    Typed(Box<Datum>),
}

impl Literal {
    /// The type name PostgreSQL uses for this literal when it complains about it.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            // A NULL has no type to name, and never reaches a mismatch anyway; a quoted string
            // is PostgreSQL's `unknown` and takes whatever type the column gives it.
            Literal::Null | Literal::String(_) => "unknown",
            // PostgreSQL types a small integer constant `integer`, not `bigint`.
            Literal::Integer(value) if i32::try_from(*value).is_ok() => "integer",
            Literal::Integer(_) => "bigint",
            Literal::Decimal(_) => "numeric",
            Literal::Typed(value) => value.column_type().map_or("unknown", ColumnType::name),
            Literal::Bool(_) => "boolean",
        }
    }

    /// Whether this literal can be *compared* against a column of `ty`.
    ///
    /// Comparison is stricter than assignment, and the difference is not a detail: PostgreSQL
    /// stores `42` in a `text` column happily — that is an assignment cast — and answers
    /// `WHERE txt = 42` with `operator does not exist: text = integer`, because there is no such
    /// operator to call. Using the assignment rule for both would turn that error into a silent
    /// `false`, which is a wrong answer rather than a missing feature. Measured on both sides.
    #[must_use]
    pub fn comparable_with(&self, ty: ColumnType) -> bool {
        match self {
            // `unknown` takes whatever type the other side has -- if it can be read as one.
            Literal::Null | Literal::String(_) => true,
            Literal::Integer(_) => matches!(ty, ColumnType::Int8 | ColumnType::Double),
            Literal::Decimal(_) => matches!(ty, ColumnType::Int8 | ColumnType::Double),
            Literal::Bool(_) => matches!(ty, ColumnType::Bool),
            Literal::Typed(value) => value.fits(ty),
        }
    }

    /// Resolves this literal against the column it is being assigned to.
    ///
    /// `column` is only for the message; PostgreSQL names the column in a type mismatch and a
    /// client reading "is of type bytea but expression is of type integer" needs to know which one.
    pub fn assign(&self, ty: ColumnType, column: &str) -> Result<Datum> {
        let mismatch = || {
            Err(SqlError::DatatypeMismatchInColumn {
                column: column.to_owned(),
                column_type: ty.name(),
                expression_type: self.type_name(),
            })
        };
        match self {
            Literal::Null => Ok(Datum::Null),

            // The `unknown` literal: whatever the column is, read it as that. This is one
            // function, checked against a real server for all six types, rather than six rules.
            Literal::String(text) => Datum::from_text(ty, text),

            Literal::Integer(value) => match ty {
                ColumnType::Int8 => Ok(Datum::Int8(*value)),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the widening is PostgreSQL's own assignment cast, and lossy the same way"
                )]
                ColumnType::Double => Ok(Datum::Double(*value as f64)),
                // PostgreSQL's assignment cast to text is the value's own text.
                ColumnType::Text => Ok(Datum::Text(value.to_string())),
                ColumnType::Bool | ColumnType::Bytea | ColumnType::TimestampTz => mismatch(),
            },

            Literal::Decimal(digits) => match ty {
                // `numeric` has no signed zero, so `-0.0` in a `double precision` column is `0`
                // and not `-0`. Measured: the literal goes through `numeric` on its way, and that
                // is where the sign is lost.
                ColumnType::Double => {
                    Datum::from_text(ColumnType::Double, digits).map(|value| match value {
                        // `0.0` as a pattern already matches `-0.0`, which is
                        // exactly the case being normalised away.
                        Datum::Double(0.0) => Datum::Double(0.0),
                        other => other,
                    })
                }
                // The digits as written, which is what `numeric`'s own text is.
                ColumnType::Text => Ok(Datum::Text(digits.clone())),
                ColumnType::Int8 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the bigint column \"{column}\""
                ))),
                ColumnType::Bool | ColumnType::Bytea | ColumnType::TimestampTz => mismatch(),
            },

            // Already resolved. It fits the column it was resolved against and nothing else.
            Literal::Typed(value) if value.fits(ty) => Ok((**value).clone()),
            Literal::Typed(_) => mismatch(),

            Literal::Bool(value) => match ty {
                ColumnType::Bool => Ok(Datum::Bool(*value)),
                // `true`, not `t`: the cast, not the output function.
                ColumnType::Text => Ok(Datum::Text(
                    if *value { "true" } else { "false" }.to_owned(),
                )),
                ColumnType::Int8
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Double => mismatch(),
            },
        }
    }
}

impl Expr {
    /// Evaluates to a value for a column of `ty`, which is what `INSERT` needs.
    ///
    /// A `$1` with nothing bound to it is `42P02`, which is what PostgreSQL answers a simple query
    /// that contains one — the simple query protocol has no way to carry a parameter.
    pub fn evaluate(&self, ty: ColumnType, column: &str) -> Result<Datum> {
        match self {
            Expr::Literal(literal) => literal.assign(ty, column),
            Expr::Parameter(number) => Err(SqlError::UndefinedParameter(*number)),
            other => Err(SqlError::unsupported(format!(
                "{} in a VALUES list",
                describe(other)
            ))),
        }
    }
}

fn describe(expr: &Expr) -> &'static str {
    match expr {
        Expr::Literal(_) => "a literal",
        Expr::Parameter(_) => "a parameter",
        Expr::Column { .. } | Expr::Ordinal { .. } => "a column reference",
        Expr::Binary { .. } => "an operator",
        Expr::Not(_) => "NOT",
        Expr::IsNull { .. } => "IS NULL",
    }
}
