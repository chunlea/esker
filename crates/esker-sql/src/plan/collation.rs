//! **Where a collation comes from**, which is a column, an explicit clause, or nowhere.
//!
//! [ADR 0096](../../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md) is
//! the decision and `tests/corpus/pg19_collation_family.txt` is the measurement. The short version
//! is the SQL standard's own three-valued *derivation*: an operand carries a collation with a
//! **strength**, and three rules fall out of merging them.
//!
//! `C` and `POSIX` are the only collations this node has (ADR 0076) and both order by byte, so
//! nothing here changes a value. What it changes is **which statements exist**: a real server
//! refuses several this node used to build, and that direction — answering where the right answer
//! is a refusal — is the one ADR 0031 calls worst.

use crate::error::{Result, SqlError};
use crate::plan::Expr;

/// How firmly an expression holds the collation it carries.
///
/// PostgreSQL's own three, and the order is a total one: **explicit beats implicit beats none**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Derivation {
    /// Nothing said. A literal, and anything built only out of literals — which is why
    /// `upper('a')` has nowhere to get an ordering from and `upper('a'::text)` still has nowhere,
    /// measured.
    None,
    /// A **column** said it, by being one. Any column will do, and it need not even be collatable:
    /// `upper(n::text)` over an `integer` column is accepted where `upper(1::text)` is not.
    Implicit,
    /// A `COLLATE` clause said it. **On a column** — an explicit clause on a literal makes a
    /// stored expression no more acceptable than none at all, in any placement, measured one
    /// placement at a time.
    Explicit,
}

/// What an expression carries: a name, and how firmly.
///
/// The name is `None` when nothing named one, and also when a *column* is what carries it and the
/// caller did not say which collation that column has — the strength is what decides a generated
/// column's fate, and the name only matters when two of them meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Derived<'a> {
    /// `C` or `POSIX`, or nothing.
    pub(crate) collation: Option<&'a str>,
    /// How firmly.
    pub(crate) strength: Derivation,
    /// **Whether a column reference is under here at all**, which is a different question from
    /// [`Self::strength`] and the one a generated column asks.
    ///
    /// Measured, and it is the pair that separates them: `upper(t COLLATE "C")` is accepted and
    /// `upper('a' COLLATE "C")` is `42P22`. Both are `Derivation::Explicit` — a clause **names** a
    /// collation — and only the first has a column under it. So an explicit clause names an
    /// ordering and does **not** make one derivable; only a column does, and any column will,
    /// even an `integer` one through a cast (`upper(n::text)` is accepted where `upper(1::text)`
    /// is not).
    pub(crate) from_column: bool,
}

impl Derived<'_> {
    /// Nothing said: what a literal carries.
    const NONE: Self = Derived {
        collation: None,
        strength: Derivation::None,
        from_column: false,
    };

    /// A column said it, and the caller did not say which.
    const IMPLICIT: Self = Derived {
        collation: None,
        strength: Derivation::Implicit,
        from_column: true,
    };
}

/// **The explicit clauses in one expression agree**, or `42P21`.
///
/// This is the half of ADR 0096 that needs no scope: two `COLLATE` clauses that disagree are
/// decidable from the expression's own shape, so it runs at lowering and covers a query, a
/// `DEFAULT`, a generated column and an index key at once — which is what a real server does,
/// measured in a `SELECT`, in `ORDER BY` and in `GENERATED ALWAYS AS`.
///
/// **Wider than a comparison.** `||`, `COALESCE` and `CASE` raise it too, and it propagates up
/// through a function: `upper('a' COLLATE "C") < ('b' COLLATE "POSIX")` is `42P21`. Anywhere two
/// explicit clauses *merge*.
pub(crate) fn refuse_explicit_mismatch(expr: &Expr) -> Result<()> {
    derive(expr).map(|_| ())
}

/// **A collation-using operation whose collation cannot be derived**, refused — `42P22`.
///
/// ADR 0096's second rule, and the context is the whole of it: only a **generated column** asks.
/// A `DEFAULT`, an index expression, an index predicate and a `CHECK` all accept `upper('a')` on
/// 19beta1, measured, and the corpus header that said otherwise is corrected beside its own rows.
///
/// The walk is outermost-first and stops at the first refusal, which is what a real server does:
/// one error names one operation.
pub(crate) fn refuse_underivable(
    expr: &Expr,
    type_of: &dyn Fn(&Expr) -> Option<crate::value::ColumnType>,
) -> Result<()> {
    // **`from_column`, not `strength`.** An explicit clause names a collation and does not make
    // one derivable: `upper('a' COLLATE "C")` is `42P22` on 19beta1 and `upper(t COLLATE "C")`
    // builds, measured one placement at a time. A check on the strength would have accepted the
    // first, which is the direction this whole row exists to close.
    if let Some(operation) = collation_using(expr, type_of)
        && !derive(expr)?.from_column
    {
        return Err(SqlError::IndeterminateCollation(operation));
    }
    for child in children(expr) {
        refuse_underivable(child, type_of)?;
    }
    Ok(())
}

/// **What PostgreSQL calls this operation**, when it is one that uses a collation.
///
/// Six names, measured over 43 shapes at once
/// (`tests/captures/pg19_collation_operations.txt`) rather than read off the three the
/// placement-by-placement corpus happened to reach. `initcap` is a name of its own and this node
/// does not have the function; `LIKE` and `ILIKE` are two names for one node; a regular-expression
/// match is `regular expression` and not `LIKE`.
///
/// **The type decides, not the operator.** `(1 = 2)` is accepted and `('a' = 'b')` is not, so a
/// comparison asks only over a *collatable* type — the same predicate a `COLLATE` clause on a
/// column is checked against.
///
/// **And `string comparison` is much wider than `<`.** A function that *compares* needs a
/// collation and one that only *cuts* does not: `replace`, `split_part`, `strpos`,
/// `string_to_array`, `greatest`, `least`, `nullif` and `array_position` do; `substr`,
/// `substring`, `btrim`, `ltrim`, `rtrim`, `reverse`, `ascii`, `length`, `md5` and `||` do not.
/// `COALESCE` picks rather than compares and does not; a `CASE` asks through the comparison in its
/// `WHEN`, which this walk reaches as a node of its own.
fn collation_using(
    expr: &Expr,
    type_of: &dyn Fn(&Expr) -> Option<crate::value::ColumnType>,
) -> Option<&'static str> {
    use crate::plan::{BinaryOp, CatalogFunc, ScalarFunc};

    let collatable =
        |operand: &Expr| type_of(operand).is_some_and(crate::catalog::pg_attribute::collatable);
    match expr {
        Expr::Scalar {
            func: ScalarFunc::Lower,
            ..
        } => Some("lower() function"),
        Expr::Scalar {
            func: ScalarFunc::Upper,
            ..
        } => Some("upper() function"),
        Expr::Like {
            case_insensitive, ..
        } => Some(if *case_insensitive { "ILIKE" } else { "LIKE" }),
        Expr::RegexMatch { .. } => Some("regular expression"),
        Expr::Binary { op, left, .. }
            if matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            ) && collatable(left) =>
        {
            Some(STRING_COMPARISON)
        }
        // `IN` is an equality per element, and answers the same name — measured.
        Expr::InList { operand, .. } if collatable(operand) => Some(STRING_COMPARISON),
        // **The functions that compare.** Each was measured; none is derivable from its name,
        // which is why they are a list and `substr` beside `replace` is the pair that says so.
        Expr::CatalogFunc(call)
            if matches!(
                call.func,
                CatalogFunc::Replace
                    | CatalogFunc::SplitPart
                    | CatalogFunc::StrPos
                    | CatalogFunc::StringToArray
                    | CatalogFunc::Greatest
                    | CatalogFunc::Least
                    | CatalogFunc::NullIf
                    | CatalogFunc::ArrayPosition
            ) && call.args.first().is_some_and(collatable) =>
        {
            Some(STRING_COMPARISON)
        }
        _ => None,
    }
}

/// The one name fourteen measured shapes answer to.
const STRING_COMPARISON: &str = "string comparison";

/// The collation an expression carries, and how firmly — raising `42P21` on the way.
///
/// **An outer `COLLATE` overrides what is inside it**, measured:
/// `(('a' COLLATE "C") COLLATE "POSIX") < 'b'` is answered, not refused. So this returns the outer
/// clause and still walks the operand, because a mismatch *inside* the operand is still a mismatch.
fn derive(expr: &Expr) -> Result<Derived<'_>> {
    if let Expr::Collate { operand, collation } = expr {
        // **The clause wins the *name* and the operand keeps the *column*.** They are two
        // answers, not one: `upper(t COLLATE "C")` builds and `upper('a' COLLATE "C")` is
        // `42P22`, both being `Explicit`.
        let inner = derive(operand)?;
        return Ok(Derived {
            collation: Some(collation),
            strength: Derivation::Explicit,
            from_column: inner.from_column,
        });
    }
    let mut merged = leaf_derivation(expr);
    for child in children(expr) {
        merged = merge(merged, derive(child)?)?;
    }
    Ok(merged)
}

/// What a node carries **before** its children are asked.
///
/// Only a column reference has anything of its own; everything else starts at nothing and takes
/// whatever its operands give it.
fn leaf_derivation(expr: &Expr) -> Derived<'static> {
    match expr {
        Expr::Column { .. } | Expr::Ordinal { .. } | Expr::Outer { .. } => Derived::IMPLICIT,
        _ => Derived::NONE,
    }
}

/// Two operands meeting.
///
/// **Explicit wins, and two explicits that disagree are the error.** Two *implicit* collations that
/// disagree are not an error here — they are a conflict that survives to evaluation, which is the
/// third of ADR 0096's rules and is measured to fire only when a row reaches the operator.
fn merge<'a>(left: Derived<'a>, right: Derived<'a>) -> Result<Derived<'a>> {
    if left.strength == Derivation::Explicit && right.strength == Derivation::Explicit {
        match (left.collation, right.collation) {
            (Some(first), Some(second)) if first != second => {
                return Err(SqlError::CollationMismatch {
                    left: first.to_owned(),
                    right: second.to_owned(),
                });
            }
            _ => {}
        }
    }
    let mut merged = if right.strength > left.strength {
        right
    } else {
        left
    };
    // **A column under either operand is a column under the node**, whichever side won the name.
    merged.from_column = left.from_column || right.from_column;
    Ok(merged)
}

/// Every **immediate** child of one expression, in the order the statement writes them.
///
/// The order is what makes `42P21`'s message right: measured,
/// `((t COLLATE "POSIX") < (u COLLATE "C"))` names `"POSIX" and "C"` and not the other way round.
///
/// **Total on purpose**, like the four other walks in this crate: a variant added to
/// [`crate::plan::Expr`] is a compile error here rather than a child quietly not asked. That has
/// been the failure twice — `exec::bind`'s two walks say so in their own comment — and this one is
/// the reason a fifth walk exists rather than a fifth `_ => {}`.
fn children(expr: &Expr) -> Vec<&Expr> {
    match expr {
        // Nothing under them.
        Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::Column { .. }
        | Expr::Ordinal { .. }
        | Expr::Outer { .. }
        | Expr::Uuid(_)
        | Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentUser
        | Expr::CurrentSetting { .. }
        | Expr::Default
        | Expr::Sequence(_)
        | Expr::SetFunc(_)
        // **A subquery is a leaf here**, because its expressions were lowered against its own
        // scope: a `COLLATE` inside it was checked when that select was lowered, and a clause in
        // the outer expression cannot merge with one inside a sub-select.
        | Expr::Subquery(_) => Vec::new(),
        Expr::Negate(operand)
        | Expr::Not(operand)
        | Expr::IsNull { operand, .. }
        | Expr::Scalar { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::ToText { operand, .. } => vec![operand],
        Expr::Arithmetic { left, right, .. } | Expr::Binary { left, right, .. } => {
            vec![left, right]
        }
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::RegexMatch {
            operand, pattern, ..
        } => vec![operand, pattern],
        Expr::Subscript { operand, index, .. } => vec![operand, index],
        Expr::QuantifiedArray { operand, array, .. } => vec![operand, array],
        Expr::InList { operand, list, .. } => {
            let mut all = vec![&**operand];
            all.extend(list);
            all
        }
        Expr::Array { elements, .. } | Expr::Coalesce(elements) => elements.iter().collect(),
        Expr::Advisory { args, .. } => args.iter().collect(),
        Expr::CatalogFunc(call) => call.args.iter().collect(),
        Expr::Aggregate(call) => call.args.iter().collect(),
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            let mut all: Vec<&Expr> = operand.as_deref().into_iter().collect();
            for branch in branches {
                all.push(&branch.when);
                all.push(&branch.then);
            }
            all.extend(otherwise.as_deref());
            all
        }
    }
}
