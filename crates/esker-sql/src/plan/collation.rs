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
}

impl Derived<'_> {
    /// Nothing said: what a literal carries.
    const NONE: Self = Derived {
        collation: None,
        strength: Derivation::None,
    };

    /// A column said it, and the caller did not say which.
    const IMPLICIT: Self = Derived {
        collation: None,
        strength: Derivation::Implicit,
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

/// The collation an expression carries, and how firmly — raising `42P21` on the way.
///
/// **An outer `COLLATE` overrides what is inside it**, measured:
/// `(('a' COLLATE "C") COLLATE "POSIX") < 'b'` is answered, not refused. So this returns the outer
/// clause and still walks the operand, because a mismatch *inside* the operand is still a mismatch.
fn derive(expr: &Expr) -> Result<Derived<'_>> {
    if let Expr::Collate { operand, collation } = expr {
        derive(operand)?;
        return Ok(Derived {
            collation: Some(collation),
            strength: Derivation::Explicit,
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
    Ok(if right.strength > left.strength {
        right
    } else {
        left
    })
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
