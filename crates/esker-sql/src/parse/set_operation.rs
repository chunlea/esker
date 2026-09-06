//! `UNION`, `INTERSECT` and `EXCEPT`: flattening the parser's tree into a list of arms.
//!
//! **A separate file on purpose.** The one line it replaces in `crate::parse::lower` sits thirty
//! lines from another lane's unlanded work on the same function, and a feature written inside it
//! would have collided hunk for hunk.
//!
//! # The shape
//!
//! `sqlparser` gives a set operation as a **left-leaning binary tree**: `a UNION ALL b UNION ALL
//! c` is `((a ∪ b) ∪ c)`. SQL's operators are left-associative and this node executes the arms in
//! order, so the tree is flattened into a list — the first arm and then one [`plan::SetArm`] per
//! operator. Nothing is lost by that for `UNION ALL`, which is what commit one is; an operator
//! chain that mixes precedence keeps the arm's own operator so the difference is still there when
//! the other two are built.
//!
//! # What belongs to the arm and what belongs to the set
//!
//! Measured (`tests/captures/pg19_set_operations.txt`), and it is the rule the whole lowering
//! turns on: an arm may carry its own `ORDER BY` or `LIMIT` **only inside parentheses**, and
//! written bare before `UNION` both are `42601 syntax error at or near "UNION"`. So a clause
//! outside the parentheses is the *set's* — which is why the first arm carries it, that select
//! being where [`plan::Select::order_by`] and its neighbours already live.

use sqlparser::ast::{SetExpr, SetOperator, SetQuantifier};

use crate::error::{Result, SqlError};
use crate::plan;

/// Flattens a set operation into its first arm and the arms after it.
///
/// The outer query's `ORDER BY`, `LIMIT`, `OFFSET` and `WITH` are the caller's to apply, because
/// they belong to the set rather than to any arm.
pub(super) fn lower(
    body: &SetExpr,
    arm: &dyn Fn(&SetExpr) -> Result<plan::Select>,
) -> Result<plan::Select> {
    let mut arms = Vec::new();
    let first = flatten(body, arm, &mut arms)?;
    Ok(plan::Select {
        set_arms: arms,
        ..first
    })
}

/// Walks the left spine, collecting each right-hand arm as it comes back up.
fn flatten(
    body: &SetExpr,
    arm: &dyn Fn(&SetExpr) -> Result<plan::Select>,
    arms: &mut Vec<plan::SetArm>,
) -> Result<plan::Select> {
    let SetExpr::SetOperation {
        op,
        set_quantifier,
        left,
        right,
    } = body
    else {
        return arm(body);
    };
    let first = flatten(left, arm, arms)?;
    arms.push(plan::SetArm {
        op: operator(*op)?,
        all: keeps_duplicates(*set_quantifier, *op)?,
        select: arm(right)?,
    });
    Ok(first)
}

fn operator(op: SetOperator) -> Result<plan::SetOp> {
    Ok(match op {
        SetOperator::Union => plan::SetOp::Union,
        SetOperator::Intersect => plan::SetOp::Intersect,
        SetOperator::Except => plan::SetOp::Except,
        // `MINUS` is Oracle's spelling of `EXCEPT` and PostgreSQL does not have it, so it is
        // refused by its own name rather than answered as something it is not. Named rather than
        // matched by a wildcard, so that a fourth operator is a compile error here.
        SetOperator::Minus => return Err(SqlError::unsupported("MINUS")),
    })
}

/// Whether the operator keeps duplicates: `ALL` does, `DISTINCT` and the bare form do not.
///
/// **The bare form is `DISTINCT`**, which is the half that surprises: `UNION` deduplicates where
/// `UNION ALL` does not, and the same is true of the other two.
fn keeps_duplicates(quantifier: SetQuantifier, op: SetOperator) -> Result<bool> {
    Ok(match quantifier {
        SetQuantifier::All | SetQuantifier::AllByName => true,
        SetQuantifier::None | SetQuantifier::Distinct | SetQuantifier::DistinctByName => false,
        // `UNION BY NAME` matches the arms' columns by name rather than by position — DuckDB's,
        // not PostgreSQL's, and a different operation rather than a spelling of this one.
        SetQuantifier::ByName => {
            return Err(SqlError::unsupported(format!("{op} BY NAME")));
        }
    })
}
