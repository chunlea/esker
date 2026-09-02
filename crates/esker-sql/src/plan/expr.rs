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
use crate::value::{PgDatum, PgType};

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
        /// The column's typmod, so a comparison against a `character(n)` can normalise one.
        ///
        /// A `bpchar`'s values are stored padded to `n`, so a literal has to be padded the same
        /// way before a byte comparison means what PostgreSQL means. Carrying the number here is
        /// what lets that happen once, where the literal is typed, rather than in the evaluator —
        /// which sees two `Datum::Text`s and cannot tell a `character(3)` from a `text`.
        typmod: i32,
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
    /// `x IN (a, b, …)`, or `NOT IN` when negated.
    ///
    /// Carried as itself rather than lowered to `x = a OR x = b`, for one reason that is not
    /// taste: the left-hand side would be **evaluated once per item**. `k IN (id + 6, id + 7)` is
    /// cheap written this way and quadratic written the other way for a wide left side, and a
    /// rewrite that duplicated a `nextval` would be worse than slow.
    ///
    /// The three-valued rule is the trap the corpus exists for and it is **not** "NULL means
    /// false": a match wins over a NULL, and a NULL wins over no match. `1 IN (1, NULL)` is true,
    /// `1 IN (2, NULL)` is NULL, and `1 NOT IN (2, NULL)` is NULL — so a `NOT IN` over a list
    /// containing NULL matches nothing at all. Measured, `tests/corpus/pg19_in.txt`.
    InList {
        /// The left-hand side, evaluated once.
        operand: Box<Expr>,
        /// The list, in the order written. PostgreSQL's grammar has no empty one.
        list: Vec<Expr>,
        /// `NOT IN`, which is `NOT (x IN …)` and not "none of them are equal" — the difference is
        /// entirely in what NULL does.
        negated: bool,
    },
    /// `x IS NULL`, or `IS NOT NULL` when negated. Never NULL itself — that is the whole point of
    /// the operator, and the reason `x = NULL` is not a way to write it.
    IsNull {
        /// What is being tested.
        operand: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// `DEFAULT`, written where a value goes: `INSERT INTO t VALUES (DEFAULT, 1)` and
    /// `UPDATE t SET a = DEFAULT`.
    ///
    /// Not a value and not a literal — it is a *reference to the column's own default*, which is
    /// a constant for most columns and a `nextval` for a `bigserial` one. It therefore cannot be
    /// evaluated without knowing which column it is being written into, and the two statements
    /// that can say resolve it; anywhere else it is `0A000` naming itself, which is what a real
    /// server does too (`DEFAULT` in a `WHERE` is a syntax error there).
    Default,
    /// A sequence function — `nextval('s')`, `currval('s')`, `setval('s', 10)`, `lastval()`.
    ///
    /// **Never evaluated by the row evaluator**, and for a stronger reason than an aggregate: it
    /// has *side effects*. `nextval` is not a function of the row, it is a write, and it happens
    /// once per statement in the order the statement names it. The executor evaluates these before
    /// it plans and substitutes the values it got; one reaching a row evaluator is a planner bug.
    Sequence(Box<SequenceCall>),
    /// A column of a row **outside** the plan this expression is in: a correlated reference.
    ///
    /// `WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)` resolves `b.a_id` to an
    /// [`Expr::Ordinal`] in the sub-plan's own row and `a.id` to one of these. It is never
    /// evaluated: before a correlated sub-plan is run for one outer row, every `Outer` in it whose
    /// `level` matches that row is replaced by the value it names, so the plan a cursor is opened
    /// on has none left (`docs/plans/phase-12-subquery.md` §1).
    Outer {
        /// How many scopes out, one-based: `1` is the row immediately outside this plan.
        ///
        /// Needed rather than implied, because a sub-plan inside a sub-plan has **two** rows
        /// outside it and both can be named — measured, a three-level `EXISTS` chain where the
        /// innermost query references the middle table and the outermost one. Substitution matches
        /// this against the depth it has descended to, which is why nothing has to be renumbered.
        level: usize,
        /// Position in that row.
        at: usize,
        /// The column's type, so a comparison against it resolves a literal the same way a
        /// comparison against a column of this row does.
        ty: ColumnType,
        /// The column's typmod, for the same reason [`Expr::Ordinal`] carries one.
        typmod: i32,
    },
    /// A subquery written where a value goes — `(SELECT …)`, `EXISTS (…)`, `x IN (SELECT …)`,
    /// `x = ANY (SELECT …)`.
    ///
    /// **Never evaluated with its `run` field empty**, for the same reason a
    /// [`crate::plan::routing::Columnar`] node is never opened unresolved: the value it carries
    /// comes from *running* a plan, which the row evaluator has no transaction to do until
    /// `crate::exec::subquery` has given it one. An unresolved one says so rather than answering
    /// "no rows", because no rows from a scalar subquery is a **NULL** and a NULL looks like an
    /// answer (`docs/plans/phase-12-subquery.md` §1).
    Subquery(Box<crate::plan::SubqueryExpr>),
    /// An aggregate call — `count(*)`, `sum(a)`, `min(DISTINCT b)`.
    ///
    /// **Never evaluated.** It is a value *of a group*, not of a row, so the executor's
    /// [`crate::exec`] row evaluator has no case for it: the planner replaces every one of these
    /// with an [`Expr::Ordinal`] into the aggregated row before the tree is built. One reaching a
    /// row evaluator is a planner bug and says so rather than returning a number.
    Aggregate(Box<AggregateCall>),
}

/// The four functions a sequence answers to.
///
/// PostgreSQL has one more, `nextval`'s sibling `setval` in its two-argument and three-argument
/// forms, which are one function here because they differ only in a boolean. Everything else in
/// `pg_sequence`'s surface — `ALTER SEQUENCE`, `CREATE SEQUENCE`, reading a sequence as a relation
/// — is `0A000` naming itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceFunc {
    /// `nextval(regclass)`: the next value, and a write.
    NextVal,
    /// `currval(regclass)`: the last value **this session** got from that sequence.
    CurrVal,
    /// `setval(regclass, bigint [, boolean])`: where the sequence resumes from.
    SetVal,
    /// `lastval()`: the last value this session got from *any* sequence.
    LastVal,
}

impl SequenceFunc {
    /// The four names, matched case-insensitively as PostgreSQL matches them.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "nextval" => Some(SequenceFunc::NextVal),
            "currval" => Some(SequenceFunc::CurrVal),
            "setval" => Some(SequenceFunc::SetVal),
            "lastval" => Some(SequenceFunc::LastVal),
            _ => None,
        }
    }

    /// What it is called — and, because PostgreSQL names an output column after the function that
    /// filled it, what `SELECT nextval('s')` calls its one column.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SequenceFunc::NextVal => "nextval",
            SequenceFunc::CurrVal => "currval",
            SequenceFunc::SetVal => "setval",
            SequenceFunc::LastVal => "lastval",
        }
    }
}

/// One sequence-function call, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceCall {
    /// Which one.
    pub func: SequenceFunc,
    /// The sequence's name, **folded the way an identifier is folded**, or `None` for `lastval()`.
    ///
    /// The argument is a string and PostgreSQL reads it as a *name*: measured,
    /// `nextval('Q1_ID_SEQ')` finds `q1_id_seq` and `nextval('"q1_id_seq"')` finds it too. So the
    /// quoting rules that apply to an identifier apply inside the quotes, which is not a thing a
    /// reader would guess about a `text` argument.
    pub name: Option<String>,
    /// `setval`'s value.
    pub value: Option<i64>,
    /// `setval`'s third argument, `is_called`, which defaults to true.
    ///
    /// True means the value has been handed out and the next `nextval` answers one *past* it;
    /// false means it has not and the next `nextval` answers it. Measured both ways.
    pub is_called: bool,
}

/// The five aggregates this node computes.
///
/// PostgreSQL has dozens; these are the five `ActiveRecord`'s own calculations use — `count`, `sum`,
/// `minimum`, `maximum`, `average` — and the four `esker-columnar`'s fragment evaluator already
/// defines (`docs/plans/phase-7-columnar.md` M2), which is why the semantics below are a match
/// rather than a second opinion. Everything else is `0A000` naming itself, contract C2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    /// `count(*)` and `count(expr)`, which are different aggregates wearing one name.
    Count,
    /// `sum`, over `int8` and `float8`.
    Sum,
    /// `min`, in [`crate::value::PgDatum::pg_cmp`] order.
    Min,
    /// `max`, likewise.
    Max,
    /// `avg`, over `float8` only — `avg(int8)` is `numeric` on a real server and this node has no
    /// `numeric` to be right with (`docs/adr/0031-rails-compatibility-is-measured.md`).
    Avg,
}

impl AggregateFunc {
    /// The five names, matched the way PostgreSQL matches them: case-insensitively, so `COUNT(*)`
    /// and `Count(*)` are the same call. Measured — both forms execute on a real server.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(AggregateFunc::Count),
            "sum" => Some(AggregateFunc::Sum),
            "min" => Some(AggregateFunc::Min),
            "max" => Some(AggregateFunc::Max),
            "avg" => Some(AggregateFunc::Avg),
            _ => None,
        }
    }

    /// What it is called, in the lower case PostgreSQL's own messages use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
            AggregateFunc::Avg => "avg",
        }
    }
}

/// One aggregate call, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    /// Which one.
    pub func: AggregateFunc,
    /// The arguments as written. Every one of the five takes exactly one; the rest are carried so
    /// that the refusal can name their **types** the way a real server does — measured,
    /// `count(n, g)` is `function count(bigint, text) does not exist`, and the types are not known
    /// until the planner has resolved them.
    pub args: Vec<Expr>,
    /// `count(*)`: the argument list was a single `*`, so the call reads no value at all — which
    /// is why it counts a row whose every column is NULL.
    pub star: bool,
    /// `DISTINCT` *inside* the parentheses: `count(DISTINCT a)`. Not the same clause as
    /// `SELECT DISTINCT`, which is on [`crate::plan::Select`].
    pub distinct: bool,
}

impl AggregateCall {
    /// The single argument this call folds over, or `None` for `count(*)`.
    ///
    /// `None` for a call of the wrong arity too, which is why the planner checks the arity before
    /// it asks.
    #[must_use]
    pub fn arg(&self) -> Option<&Expr> {
        if self.star { None } else { self.args.first() }
    }
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
            Literal::Integer(_) => matches!(
                ty,
                ColumnType::Int8
                    | ColumnType::Int4
                    | ColumnType::Int2
                    | ColumnType::Double
                    | ColumnType::Real
            ),
            Literal::Decimal(_) => matches!(
                ty,
                ColumnType::Int8
                    | ColumnType::Int4
                    | ColumnType::Int2
                    | ColumnType::Double
                    | ColumnType::Real
            ),
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
                // The range check is the type: a constant a real server refuses with `22003`
                // must not be quietly accepted here, which is the whole argument ADR 0033 made
                // for a distinct `int4` rather than an alias.
                ColumnType::Int4 => i32::try_from(*value)
                    .map(Datum::Int4)
                    .map_err(|_| SqlError::IntegerLiteralOutOfRange(ColumnType::Int4.name())),
                ColumnType::Int2 => i16::try_from(*value)
                    .map(Datum::Int2)
                    .map_err(|_| SqlError::IntegerLiteralOutOfRange(ColumnType::Int2.name())),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the widening is PostgreSQL's own assignment cast, and lossy the same way"
                )]
                ColumnType::Double => Ok(Datum::Double(*value as f64)),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the widening is PostgreSQL's own assignment cast, and lossy the same way"
                )]
                ColumnType::Real => Ok(Datum::Real(*value as f32)),
                // PostgreSQL's assignment cast to text is the value's own text.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                    Ok(Datum::Text(value.to_string()))
                }
                ColumnType::Bool
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Timestamp
                // Neither takes a number or a boolean: `INSERT INTO t (j) VALUES (1)` is a type
                // mismatch on a real server, not a one-element document.
                | ColumnType::Json
                | ColumnType::Jsonb => mismatch(),
            },

            Literal::Decimal(digits) => match ty {
                // The same refusal `int8` gets, naming the column's own type.
                ColumnType::Int4 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the integer column \"{column}\""
                ))),
                ColumnType::Int2 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the smallint column \"{column}\""
                ))),
                // `numeric` has no signed zero, so `-0.0` in a `double precision` column is `0`
                // and not `-0`. Measured: the literal goes through `numeric` on its way, and that
                // is where the sign is lost.
                // The same `numeric` road one width down, and the same signed-zero loss.
                ColumnType::Real => {
                    numeric_text(Datum::from_text(ColumnType::Real, digits), digits).map(|value| {
                        match value {
                            Datum::Real(0.0) => Datum::Real(0.0),
                            other => other,
                        }
                    })
                }
                ColumnType::Double => {
                    numeric_text(Datum::from_text(ColumnType::Double, digits), digits).map(
                        |value| match value {
                            // `0.0` as a pattern already matches `-0.0`, which is
                            // exactly the case being normalised away.
                            Datum::Double(0.0) => Datum::Double(0.0),
                            other => other,
                        },
                    )
                }
                // The digits as written, which is what `numeric`'s own text is.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                    Ok(Datum::Text(digits.clone()))
                }
                ColumnType::Int8 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the bigint column \"{column}\""
                ))),
                ColumnType::Bool
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                // A number is not a document, whichever way it is written.
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Timestamp => mismatch(),
            },

            // Already resolved. It fits the column it was resolved against and nothing else.
            Literal::Typed(value) if value.fits(ty) => Ok((**value).clone()),
            Literal::Typed(_) => mismatch(),

            Literal::Bool(value) => match ty {
                ColumnType::Bool => Ok(Datum::Bool(*value)),
                // `true`, not `t`: the cast, not the output function.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => Ok(Datum::Text(
                    if *value { "true" } else { "false" }.to_owned(),
                )),
                ColumnType::Int8
                | ColumnType::Int4
                | ColumnType::Int2
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Timestamp
                | ColumnType::Double
                // `true` *is* a JSON document, and `INSERT INTO t (j) VALUES (true)` is still a
                // type mismatch on a real server: the literal is a `boolean`, and there is no
                // assignment cast from one to `json`.
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Real => mismatch(),
            },
        }
    }
}

/// Rewrites a float's out-of-range message to quote the literal as **`numeric`** would print it.
///
/// A bare `1e400` is a `numeric` before anything casts it, so the value PostgreSQL quotes is
/// `numeric`'s own text — plain decimal, no exponent — where a *string* `'1e400'` is quoted
/// exactly as written. Measured on 19beta1 for both float widths
/// ([`crate::value::float::plain_decimal`] carries the two statements).
fn numeric_text(outcome: Result<Datum>, digits: &str) -> Result<Datum> {
    outcome.map_err(|error| match error {
        SqlError::FloatOutOfRange { ty, .. } => SqlError::FloatOutOfRange {
            ty,
            value: crate::value::float::plain_decimal(digits),
        },
        other => other,
    })
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
        Expr::Outer { .. } => "a correlated column reference",
        Expr::Binary { .. } => "an operator",
        Expr::Not(_) => "NOT",
        Expr::IsNull { .. } => "IS NULL",
        Expr::InList { negated: false, .. } => "IN",
        Expr::InList { negated: true, .. } => "NOT IN",
        Expr::Aggregate(_) => "an aggregate function",
        Expr::Default => "DEFAULT",
        Expr::Sequence(_) => "a sequence function",
        Expr::Subquery(sub) => sub.kind.describe(),
    }
}
