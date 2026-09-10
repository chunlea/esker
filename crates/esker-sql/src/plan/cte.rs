//! `WITH a AS (…) SELECT …`, and why a CTE is a derived table wearing a name.
//!
//! `docs/plans/phase-12-subquery.md` §3 unit 3. A non-recursive CTE is **inlined at each
//! reference**: every `FROM a` that names one is replaced by the derived table `FROM (…) AS a`,
//! and from there unit 2's machinery does the rest — a synthetic relation, a plan under it, the
//! alias list on top. There is no CTE node, no CTE scope and no second name-resolution path.
//!
//! # Why inlining is not an approximation
//!
//! PostgreSQL 12+ **inlines a single-reference CTE** and materialises a multi-reference one, and
//! `MATERIALIZED` / `NOT MATERIALIZED` ask for one or the other by hand. All three spellings
//! return the same rows — measured, `tests/corpus/pg19_cte.txt` — because a non-recursive CTE over
//! a read-only statement has nothing in it that can be run twice to different effect: this node
//! refuses every volatile function inside a subquery, so there is no `nextval` and no `random()`
//! to disagree with itself. What inlining changes is the **plan**, and [ADR
//! 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md) matches observable behaviour
//! rather than plans.
//!
//! The cost is real and is recorded rather than hidden: a CTE referenced twice is **read twice**.
//! `TODO(post-v1)`: materialise a multi-reference CTE once, which is a shared plan node and a
//! lifetime question this phase does not need to answer.
//!
//! # The three things the capture decided
//!
//! * **An unreferenced CTE is still analysed.** `WITH t AS (SELECT nope FROM a) SELECT 1` is
//!   `42703` on a real server, and inlining alone would never look at `t`. So every CTE is carried
//!   on [`crate::plan::Select::ctes`] whether anything referenced it or not, and the planner plans
//!   all of them.
//! * **A CTE may not be referenced before it is written, including by itself**, and both are
//!   `42P01` with a `DETAIL` naming the WITH item and a `HINT` suggesting `WITH RECURSIVE`. That
//!   falls out of substituting in order: a name not yet defined is simply not substituted, and the
//!   relation lookup that follows is the error — with the two extra fields added here, because a
//!   bare `relation "a" does not exist` would send a reader looking for a missing table.
//! * **A CTE shadows a real table of the same name.** Substitution replaces the `FROM` entry
//!   before anything looks in the catalog, so this is the order of operations rather than a rule.

use crate::error::{Result, SqlError};
use crate::plan::{Derived, Expr, Select, TableRef};

/// Replaces every reference to `name` in `select` with the derived table `body` is, in place.
///
/// The reference's own alias survives: `FROM t AS u` becomes a derived table called `u`, which is
/// what a real server calls it (measured, `ORDER BY u.id` resolves). Answers whether anything
/// referenced it, which is only used to say so — every CTE is analysed either way.
pub fn inline(select: &mut Select, name: &str, body: &Select, columns: &[String]) -> bool {
    let mut referenced = false;
    for_each_from_mut(select, &mut |entry| {
        // `hidden_cte` is the one thing that stops this: an entry inside an **earlier** item's
        // body that names this one is a forward reference, and substituting it would answer the
        // statement PostgreSQL refuses. It was marked when that body was lowered, before this item
        // existed to be substituted.
        if entry.derived.is_some() || entry.hidden_cte || entry.name != name {
            return;
        }
        referenced = true;
        // The alias replaces the CTE's name the way it replaces a table's, so what the derived
        // table is called is what a qualifier in this query has to write.
        *entry = TableRef {
            values: None,
            name: entry.referred_as().to_owned(),
            alias: None,
            derived: Some(Box::new(Derived::from_cte(
                Box::new(body.clone()),
                columns.to_vec(),
            ))),
            function: None,
            hidden_cte: false,
            written: None,
        };
    });
    referenced
}

/// Substitutes a **recursive** CTE: the same derived table every other item becomes, carrying its
/// second half.
///
/// `seed` is the non-recursive term and `step` the recursive one, already planted with its working
/// table. Everything above reads the result as an ordinary derived table, which is what lets the
/// outer query name it twice, qualify it and sort by it with no further rules.
pub fn inline_recursive(
    select: &mut Select,
    name: &str,
    seed: &Select,
    step: &crate::plan::RecursiveTerm,
    columns: &[String],
) -> bool {
    let mut referenced = false;
    for_each_from_mut(select, &mut |entry| {
        if entry.derived.is_some() || entry.hidden_cte || entry.name != name {
            return;
        }
        referenced = true;
        let mut derived = Derived::from_cte(Box::new(seed.clone()), columns.to_vec());
        derived.recursive = Some(Box::new(step.clone()));
        *entry = TableRef {
            values: None,
            name: entry.referred_as().to_owned(),
            alias: None,
            derived: Some(Box::new(derived)),
            function: None,
            hidden_cte: false,
            written: None,
        };
    });
    referenced
}

/// Replaces every reference to `name` with the **working table**, and answers how many there were.
///
/// The twin of [`inline`] for the recursive term of a `WITH RECURSIVE`: where that one substitutes
/// the body, this one substitutes a relation whose rows the fixpoint supplies a round at a time.
/// `seed` is carried for its *shape* only — a working table is as wide as the non-recursive term,
/// which is what makes the seed's types the whole query's.
///
/// The count is the answer to PostgreSQL's `42P19 recursive reference to query "t" must not appear
/// more than once`, and it is counted over **table factors** rather than over the rendered text:
/// `JOIN t ON c.firm_id = t.id` writes the name twice and references the relation once.
pub fn plant_working_table(
    select: &mut Select,
    name: &str,
    seed: &Select,
    columns: &[String],
) -> usize {
    let mut found = 0;
    for_each_from_mut(select, &mut |entry| {
        if entry.derived.is_some() || entry.hidden_cte || entry.name != name {
            return;
        }
        found += 1;
        let mut derived = Derived::from_cte(Box::new(seed.clone()), columns.to_vec());
        derived.working_table = true;
        *entry = TableRef {
            values: None,
            name: entry.referred_as().to_owned(),
            alias: None,
            derived: Some(Box::new(derived)),
            function: None,
            hidden_cte: false,
            written: None,
        };
    });
    found
}

/// Whether the working table sits on the **nullable** side of an outer join.
///
/// Asked *after* [`plant_working_table`] has run, so the entry to look for is the one it made and
/// not the name it replaced.
///
/// PostgreSQL refuses that and only that: `FROM t LEFT JOIN c` keeps every row of `t` and is
/// legal, while `FROM c LEFT JOIN t` invents NULL rows for the recursive term to read and is
/// `42P19 recursive reference to query "t" must not appear within an outer join`. Measured both
/// ways — and the legal one is *unbounded*, which is how the first capture of this feature hung.
#[must_use]
pub fn on_a_nullable_side(select: &Select) -> bool {
    select.joins.iter().any(|join| {
        matches!(
            join.kind,
            crate::plan::JoinKind::Left | crate::plan::JoinKind::Full
        ) && join
            .table
            .derived
            .as_ref()
            .is_some_and(|derived| derived.working_table)
    })
}

/// Marks every `FROM` entry in `select` that names a `WITH` item it cannot see.
///
/// Called on one CTE's body once the items **before** it have been inlined, so what is left of
/// `names` is exactly what this body may not reference: the items after it, and itself. The flag
/// only changes the message, and only when the catalog lookup fails — a later CTE does not hide a
/// real table of the same name from an earlier body, measured.
pub fn mark_hidden(select: &mut Select, names: &[String]) {
    for_each_from_mut(select, &mut |entry| {
        if entry.derived.is_none() && names.contains(&entry.name) {
            entry.hidden_cte = true;
        }
    });
}

/// Two CTEs of one name. `42712`, and the noun is **`WITH query name`** rather than `table name` —
/// the same SQLSTATE two `FROM` entries of one name get, with a different sentence. Measured.
pub fn refuse_duplicate(names: &[String], name: &str) -> Result<()> {
    if names.iter().any(|earlier| earlier == name) {
        return Err(SqlError::DuplicateCteName(name.to_owned()));
    }
    Ok(())
}

/// Every `FROM` entry reachable from this statement, including the ones inside its subqueries and
/// inside the derived tables it already has.
///
/// The recursion is what makes `WHERE id IN (SELECT id FROM t)` and `FROM (SELECT id FROM t) AS u`
/// resolve `t` — both measured. It stops at nothing, because a `WITH` nested inside a subquery has
/// already been inlined by the time this runs: `lower_query` handles its own `WITH` before the
/// statement holding it does, so a name an inner CTE defined is gone before an outer one looks for
/// it. That is the shadowing rule, obtained by construction rather than by a scope.
fn for_each_from_mut(select: &mut Select, visit: &mut impl FnMut(&mut TableRef)) {
    for entry in std::iter::once(&mut select.from)
        .flatten()
        .chain(select.joins.iter_mut().map(|join| &mut join.table))
    {
        // Whether it was derived **before** the visit, which is what stops a self-reference from
        // substituting for ever: `WITH t AS (SELECT id FROM t)` puts a body containing `FROM t`
        // where `FROM t` was, and descending into it would find the same name again. A body that
        // was just substituted in is already complete, because the items before it were inlined
        // into it before it was stored.
        let descend = entry.derived.is_some();
        visit(entry);
        if descend && let Some(derived) = entry.derived.as_mut() {
            for_each_from_mut(&mut derived.select, visit);
        }
    }
    for cte in &mut select.ctes {
        if let Some(derived) = cte.derived.as_mut() {
            for_each_from_mut(&mut derived.select, visit);
        }
    }
    let mut walk = |expr: &mut Expr| for_each_subquery_mut(expr, visit);
    for item in &mut select.projection {
        if let crate::plan::SelectItem::Expr { expr, .. } = item {
            walk(expr);
        }
    }
    for join in &mut select.joins {
        if let Some(on) = &mut join.on {
            walk(on);
        }
    }
    for expr in select
        .filter
        .iter_mut()
        .chain(&mut select.having)
        .chain(&mut select.limit)
        .chain(&mut select.offset)
        .chain(&mut select.group_by)
    {
        walk(expr);
    }
    for item in &mut select.order_by {
        for_each_subquery_mut(&mut item.expr, visit);
    }
    // **Every arm of a set operation.** A `WITH` written outside `SELECT … UNION ALL SELECT …`
    // belongs to the whole set, and the first arm is the set's own `Select` — so a walk that
    // stopped at `from` substituted the name in the first arm and left `FROM w` in the second to
    // reach the catalog as `42P01`. The arms are selects like any other, and a set inside a derived
    // table reaches here through `derived` above for the same reason.
    for arm in &mut select.set_arms {
        for_each_from_mut(&mut arm.select, visit);
    }
}

/// Into every sub-select an expression holds.
fn for_each_subquery_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut TableRef)) {
    match expr {
        Expr::Array { elements, .. } => {
            for element in elements {
                for_each_subquery_mut(element, visit);
            }
        }
        Expr::Subquery(sub) => {
            for_each_from_mut(&mut sub.select, visit);
            for operand in &mut sub.operands {
                for_each_subquery_mut(operand, visit);
            }
        }
        Expr::Binary { left, right, .. } | Expr::Arithmetic { left, right, .. } => {
            for_each_subquery_mut(left, visit);
            for_each_subquery_mut(right, visit);
        }
        Expr::Negate(operand) => for_each_subquery_mut(operand, visit),
        Expr::Not(inner) => for_each_subquery_mut(inner, visit),
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::RegexMatch {
            operand, pattern, ..
        } => {
            for_each_subquery_mut(operand, visit);
            for_each_subquery_mut(pattern, visit);
        }
        Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::ToText { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::Scalar { operand, .. } => {
            for_each_subquery_mut(operand, visit);
        }
        Expr::InList { operand, list, .. } => {
            for_each_subquery_mut(operand, visit);
            for item in list {
                for_each_subquery_mut(item, visit);
            }
        }
        Expr::SetFunc(call) => {
            for arg in &mut call.args {
                for_each_subquery_mut(arg, visit);
            }
        }
        Expr::Coalesce(args) => {
            for arg in args {
                for_each_subquery_mut(arg, visit);
            }
        }
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(operand) = operand {
                for_each_subquery_mut(operand, visit);
            }
            for branch in branches {
                for_each_subquery_mut(&mut branch.when, visit);
                for_each_subquery_mut(&mut branch.then, visit);
            }
            if let Some(otherwise) = otherwise {
                for_each_subquery_mut(otherwise, visit);
            }
        }
        Expr::QuantifiedArray { operand, array, .. } => {
            for_each_subquery_mut(operand, visit);
            for_each_subquery_mut(array, visit);
        }
        Expr::Subscript { operand, index, .. } => {
            for_each_subquery_mut(operand, visit);
            for_each_subquery_mut(index, visit);
        }
        Expr::Aggregate(call) => {
            for arg in &mut call.args {
                for_each_subquery_mut(arg, visit);
            }
        }
        Expr::CatalogFunc(call) => {
            for arg in &mut call.args {
                for_each_subquery_mut(arg, visit);
            }
        }
        Expr::Literal(_)
        | Expr::Uuid(_)
        | Expr::Parameter(_)
        | Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentUser
        | Expr::CurrentSetting { .. }
        | Expr::Advisory { .. }
        | Expr::Column { .. }
        | Expr::Ordinal { .. }
        | Expr::Outer { .. }
        | Expr::Default
        | Expr::Sequence(_) => {}
    }
}
