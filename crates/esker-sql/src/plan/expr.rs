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
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A constant as written.
    Literal(Literal),
    /// `$1`, one-based as PostgreSQL writes it.
    Parameter(u32),
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
            Literal::Bool(_) => "boolean",
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
                ColumnType::Double => Datum::from_text(ColumnType::Double, digits),
                // The digits as written, which is what `numeric`'s own text is.
                ColumnType::Text => Ok(Datum::Text(digits.clone())),
                ColumnType::Int8 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the bigint column \"{column}\""
                ))),
                ColumnType::Bool | ColumnType::Bytea | ColumnType::TimestampTz => mismatch(),
            },

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
    /// Evaluates to a value for a column of `ty`.
    ///
    /// A `$1` with nothing bound to it is `42P02`, which is what PostgreSQL answers a simple query
    /// that contains one — the simple query protocol has no way to carry a parameter.
    pub fn evaluate(&self, ty: ColumnType, column: &str) -> Result<Datum> {
        match self {
            Expr::Literal(literal) => literal.assign(ty, column),
            Expr::Parameter(number) => Err(SqlError::UndefinedParameter(*number)),
        }
    }
}
