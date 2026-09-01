//! `GROUP BY` and the five aggregates over it: the grouping key, the accumulators, and the
//! rewrite that turns a query written about rows into a plan about groups.
//!
//! # One row of a group is not one row of a table
//!
//! Everything above [`crate::plan::Node::Aggregate`] is evaluated against a row that does not
//! exist in any table: **the grouping keys, followed by the aggregate values**, in that order.
//! `SELECT g, count(*) FROM t GROUP BY g` projects positions 0 and 1 of that row, and its
//! `HAVING` and its `ORDER BY` are resolved against the same two positions.
//!
//! That rewrite is what [`Aggregation::rewrite`] does, and it is also where `42803` comes from:
//! an expression that survives the rewrite still holding a column reference is a column the query
//! neither grouped by nor aggregated, which is the one thing a grouped query may not ask for.
//! PostgreSQL says so by name and so does this — `column "t.n" must appear in the GROUP BY clause
//! or be used in an aggregate function`, with the qualifier, because in a join it is the only form
//! that says which table.
//!
//! # The semantics are the columnar evaluator's, and both come from a capture
//!
//! `esker-columnar`'s fragment evaluator defined these first (`docs/plans/phase-7-columnar.md`
//! M2), when there were no aggregates on this side to match. The row side now agrees with it,
//! rule for rule, and every rule was measured on a real PostgreSQL 19 rather than recalled —
//! `tests/corpus/pg19_aggregate.txt` is the file, and `tests/aggregate_parity.rs` replays it.
//!
//! | Rule | |
//! |---|---|
//! | `count(*)` counts every row, including one whose every column is NULL | it reads no value, so there is nothing to be NULL |
//! | `count(col)` skips NULLs | and is therefore a different aggregate wearing the same name |
//! | `sum`, `min`, `max`, `avg` over no rows or only NULLs are **NULL**, not zero | a zero here is a wrong answer that looks like data |
//! | `min`/`max` order by [`PgDatum::pg_cmp`] — `NaN` largest, `-0.0` equal to `0.0`, text by bytes | the order everything else in this system uses |
//! | a NULL grouping key is one group of its own | `pg_cmp` makes NULL equal only to NULL |
//! | groups come back in `pg_cmp` order of their keys | see below |
//! | `sum(int8)` overflowing is an error | ADR 0031: PostgreSQL's `sum(bigint)` is `numeric` and cannot overflow; a wrapped number would be silently wrong |
//! | `sum(float8)` and `avg(float8)` accumulate left to right in row order | floating-point addition is not associative, so the order is part of the answer |
//!
//! **The group order is a promise PostgreSQL does not make.** A real server returned the NULL
//! group first for one query here and last for the same query with an `ORDER BY`; the order is
//! whatever its hash table gave. Ours is `pg_cmp` order of the key, which is deterministic, is a
//! superset of what PostgreSQL guarantees, and — because `pg_cmp` puts NULL last — is the same
//! order `ORDER BY <key>` would have produced anyway.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::error::{Result, SqlError};
use crate::exec::query::Scope;
use crate::plan::{AggregateCall, AggregateFunc, AggregateSpec, Expr, Literal, Select, SelectItem};
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// The most groups one aggregation will hold, before `53400` rather than an unbounded allocation
/// on a client's behalf. The same bound, for the same reason, as `Sort`'s.
pub(super) const GROUP_LIMIT: usize = 1_000_000;

/// A grouping key, ordered the way SQL orders values.
///
/// A newtype rather than a `Vec<Datum>` because [`Datum`]'s own `PartialEq` is **bitwise** — it
/// exists so a round-trip test cannot pass by turning `-0.0` into `0.0` — and grouping needs the
/// other comparison, the one a user sees: `-0.0` and `0.0` are one group and two `NaN`s are one
/// group, which is what a real server does. Deriving `Ord` here would have grouped by bytes and
/// been wrong in exactly the two places nobody tests.
#[derive(Debug, Clone)]
pub(super) struct GroupKey(pub(super) Vec<Datum>);

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for GroupKey {}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // `pg_cmp` already puts NULL last, which is why a group table walked in key order comes
        // out in the same order `ORDER BY <key>` would have given.
        self.0
            .iter()
            .zip(&other.0)
            .map(|(left, right)| left.pg_cmp(right))
            .find(|ordering| !ordering.is_eq())
            .unwrap_or_else(|| self.0.len().cmp(&other.0.len()))
    }
}

/// What the planner worked out about one query's grouping.
///
/// `None` from [`Aggregation::build`] means the query does not aggregate at all and nothing below
/// this module runs — which is every `SELECT` phase 6a already executed.
pub(super) struct Aggregation {
    /// The grouping keys, resolved against the **input** row.
    pub(super) keys: Vec<Expr>,
    /// Their types, so a rewritten reference to one carries a type.
    key_types: Vec<ColumnType>,
    /// One per distinct aggregate call in the statement, in output-row order after the keys.
    pub(super) specs: Vec<AggregateSpec>,
    /// The result types of those calls.
    spec_types: Vec<ColumnType>,
    /// `HAVING`, resolved against the **output** row.
    pub(super) having: Option<Expr>,
    /// Whether a `GROUP BY` was written — the empty-input rule, and nothing else.
    pub(super) grouped: bool,
}

impl Aggregation {
    /// Whether a statement aggregates at all.
    ///
    /// Three ways in, and `HAVING` is the one that surprises: `SELECT 1 FROM t HAVING true`
    /// aggregates, because `HAVING` filters a group and a query with no `GROUP BY` still has one.
    fn wanted(select: &Select) -> bool {
        !select.group_by.is_empty()
            || select.having.is_some()
            || select
                .projection
                .iter()
                .filter_map(|item| match item {
                    SelectItem::Expr { expr, .. } => Some(expr),
                    SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => None,
                })
                .chain(select.order_by.iter().map(|item| &item.expr))
                .any(contains_aggregate)
    }

    /// The type an aggregate call answers with, and the refusal when it has no answer.
    ///
    /// Both refusals are measured. `sum(text)` and `min(boolean)` do not exist on a real server
    /// either, so `42883` there is **parity**; `avg(bigint)` does exist and is `numeric`, which
    /// this node has no way to be right with, so it is `0A000` naming the type it would need
    /// (`docs/adr/0031-rails-compatibility-is-measured.md`).
    fn result_type(func: AggregateFunc, arg: Option<ColumnType>) -> Result<ColumnType> {
        let Some(arg) = arg else {
            // `count(*)`.
            return Ok(ColumnType::Int8);
        };
        let undefined = || {
            Err(SqlError::UndefinedAggregate {
                func: func.name(),
                argument: arg.name(),
            })
        };
        match func {
            AggregateFunc::Count => Ok(ColumnType::Int8),
            AggregateFunc::Sum => match arg {
                ColumnType::Int8 | ColumnType::Double => Ok(arg),
                _ => undefined(),
            },
            // Every type has an ordering here, and `bool` is the one PostgreSQL has no aggregate
            // for. Refusing it is being right rather than being incomplete.
            AggregateFunc::Min | AggregateFunc::Max => match arg {
                ColumnType::Bool => undefined(),
                _ => Ok(arg),
            },
            AggregateFunc::Avg => match arg {
                ColumnType::Double => Ok(ColumnType::Double),
                ColumnType::Int8 => Err(SqlError::unsupported(
                    "avg over a bigint column, which PostgreSQL answers as numeric with sixteen \
                     fractional digits and this node has no numeric type to reproduce",
                )),
                _ => undefined(),
            },
        }
    }

    /// Plans the aggregation of one statement, or `None` if it does not aggregate.
    ///
    /// Order matters here and is PostgreSQL's: `WHERE` is checked for aggregates **first**,
    /// because `WHERE count(*) > 1` is a different error from anything the grouping keys could
    /// produce and a user who wrote it wants to be told which clause was wrong.
    pub(super) fn build(select: &Select, scope: &Scope<'_>) -> Result<Option<Self>> {
        if let Some(filter) = &select.filter
            && contains_aggregate(filter)
        {
            return Err(SqlError::AggregateNotAllowed(
                "aggregate functions are not allowed in WHERE",
            ));
        }
        if !Self::wanted(select) {
            return Ok(None);
        }

        // A grouping key may be an output alias or a **position** in the target list. Both are
        // PostgreSQL's, and a position out of range is `42P10` naming the position.
        let mut keys = Vec::new();
        let mut key_types = Vec::new();
        for expr in &select.group_by {
            let written = match expr {
                Expr::Literal(Literal::Integer(position)) => {
                    ordinal(*position, select, "GROUP BY")?
                }
                other => super::query::dealias(other, select),
            };
            if contains_aggregate(&written) {
                return Err(SqlError::AggregateNotAllowed(
                    "aggregate functions are not allowed in GROUP BY",
                ));
            }
            let resolved = super::query::resolve(&written, scope)?;
            // `GROUP BY g, g` is one key on a real server, and a duplicate here would put a second
            // copy of the same value in every output row.
            if keys.contains(&resolved) {
                continue;
            }
            key_types.push(super::query::expr_type(&resolved, scope)?);
            keys.push(resolved);
        }

        // Every aggregate call the statement makes, once each: two `count(*)`s in one target list
        // are one accumulator and one output position.
        let ungrouped = keys.is_empty();
        let mut specs: Vec<AggregateSpec> = Vec::new();
        let mut spec_types = Vec::new();
        let mut collect = |expr: &Expr| -> Result<()> {
            check_not_nested(expr)?;
            for call in aggregate_calls(expr) {
                // Every one of the five takes exactly one argument, and PostgreSQL's refusal for
                // any other arity names the **types** of what was written -- which is why the
                // check is here, where they are known, rather than in the lowering.
                if !call.star && call.args.len() != 1 {
                    let mut arguments = Vec::with_capacity(call.args.len());
                    for arg in &call.args {
                        let resolved = super::query::resolve(arg, scope)?;
                        arguments.push(super::query::expr_type(&resolved, scope)?.name());
                    }
                    return Err(SqlError::UndefinedAggregateArity {
                        func: call.func.name(),
                        arguments: arguments.join(", "),
                    });
                }
                let arg = call
                    .arg()
                    .map(|arg| super::query::resolve(arg, scope))
                    .transpose()?;
                let arg_type = arg
                    .as_ref()
                    .map(|arg| super::query::expr_type(arg, scope))
                    .transpose()?;
                let spec = AggregateSpec {
                    func: call.func,
                    arg,
                    distinct: call.distinct,
                    arg_type,
                };
                if specs.contains(&spec) {
                    continue;
                }
                spec_types.push(Self::result_type(spec.func, spec.arg_type)?);
                specs.push(spec);
            }
            Ok(())
        };
        for item in &select.projection {
            if let SelectItem::Expr { expr, .. } = item {
                collect(expr)?;
            }
        }
        if let Some(having) = &select.having {
            collect(having)?;
        }
        for item in &select.order_by {
            collect(&super::query::dealias(&item.expr, select))?;
        }

        let mut aggregation = Aggregation {
            keys,
            key_types,
            specs,
            spec_types,
            having: None,
            // `GROUP BY ()` writes the clause and contributes no key, and it is the grand total:
            // one row even over an empty table (measured). So the empty-input rule keys off the
            // keys that survived rather than off whether the clause was written.
            grouped: !ungrouped,
        };
        // `HAVING` is resolved and rewritten last, and **without** `dealias`: `GROUP BY gg` may
        // name an output alias and `HAVING c > 0` may not — measured, a real server answers
        // `42703 column "c" does not exist` for the second. The two clauses do not share a scope.
        aggregation.having = select
            .having
            .as_ref()
            .map(|having| {
                let resolved = super::query::resolve(having, scope)?;
                // The type check comes **before** the rewrite, because PostgreSQL's does:
                // `HAVING g` over an ungrouped query is `42804 argument of HAVING must be type
                // boolean, not type text` there, not the `42803` the grouping rule would give.
                // Measured, and the two are a statement about different mistakes.
                aggregation.check_boolean(&resolved, scope)?;
                aggregation.rewrite(&resolved, scope)
            })
            .transpose()?;
        Ok(Some(aggregation))
    }

    /// `HAVING` must be a boolean, and PostgreSQL names the type it got instead.
    ///
    /// Its own check rather than `crate::exec::query`'s, for two reasons: it runs on the resolved
    /// expression *before* the rewrite, so that a non-boolean beats the grouping rule to the
    /// answer the way it does on a real server; and an aggregate call has no type until this
    /// module has resolved its argument, so nothing outside here can name one.
    fn check_boolean(&self, expr: &Expr, scope: &Scope<'_>) -> Result<()> {
        let ty = match expr {
            Expr::Binary { .. }
            | Expr::Not(_)
            | Expr::IsNull { .. }
            | Expr::InList { .. }
            | Expr::Literal(Literal::Bool(_) | Literal::Null) => return Ok(()),
            Expr::Ordinal { ty, .. } => *ty,
            Expr::Aggregate(call) => self
                .specs
                .iter()
                .position(|spec| spec.func == call.func && spec.distinct == call.distinct)
                .and_then(|at| self.spec_types.get(at).copied())
                .unwrap_or(ColumnType::Int8),
            other => super::query::expr_type(other, scope).unwrap_or(ColumnType::Text),
        };
        if ty == ColumnType::Bool {
            return Ok(());
        }
        Err(SqlError::DatatypeMismatch(format!(
            "argument of HAVING must be type boolean, not type {}",
            ty.name()
        )))
    }

    /// The type of output column `at`, for `RowDescription`.
    fn output_type(&self, at: usize) -> ColumnType {
        self.key_types
            .get(at)
            .or_else(|| self.spec_types.get(at - self.key_types.len()))
            .copied()
            .unwrap_or(ColumnType::Text)
    }

    /// Rewrites an expression resolved against the input row into one over the **output** row.
    ///
    /// A grouping key becomes its position; an aggregate call becomes its position after the keys;
    /// anything else recurses. What cannot be rewritten is a column reference that is neither, and
    /// that is `42803` — the whole rule of a grouped query, enforced in one place.
    pub(super) fn rewrite(&self, expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
        if let Some(at) = self.keys.iter().position(|key| key == expr) {
            return Ok(Expr::Ordinal {
                at,
                ty: self.output_type(at),
            });
        }
        Ok(match expr {
            Expr::Aggregate(call) => {
                let at = self
                    .specs
                    .iter()
                    .position(|spec| {
                        spec.func == call.func
                            && spec.distinct == call.distinct
                            && match (&spec.arg, call.arg()) {
                                (None, None) => true,
                                (Some(theirs), Some(ours)) => super::query::resolve(ours, scope)
                                    .is_ok_and(|ours| theirs == &ours),
                                _ => false,
                            }
                    })
                    .ok_or_else(|| {
                        SqlError::Internal(
                            "an aggregate reached the rewrite without being collected".to_owned(),
                        )
                    })?;
                let at = self.keys.len() + at;
                Expr::Ordinal {
                    at,
                    ty: self.output_type(at),
                }
            }
            // The one failure this function exists for. Resolution has already turned the name
            // into a position, so the qualified name is put back for the message.
            Expr::Ordinal { at, .. } => {
                return Err(SqlError::GroupingError(scope.qualified_name(*at)));
            }
            Expr::Binary { op, left, right } => Expr::Binary {
                op: *op,
                left: Box::new(self.rewrite(left, scope)?),
                right: Box::new(self.rewrite(right, scope)?),
            },
            Expr::Not(operand) => Expr::Not(Box::new(self.rewrite(operand, scope)?)),
            Expr::InList {
                operand,
                list,
                negated,
            } => Expr::InList {
                operand: Box::new(self.rewrite(operand, scope)?),
                list: list
                    .iter()
                    .map(|item| self.rewrite(item, scope))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            Expr::IsNull { operand, negated } => Expr::IsNull {
                operand: Box::new(self.rewrite(operand, scope)?),
                negated: *negated,
            },
            other => other.clone(),
        })
    }
}

/// `ORDER BY 2`, `GROUP BY 1`: a position in the target list, one-based.
///
/// PostgreSQL's message names the clause and the position, and `GROUP BY 0` gets the same one as
/// `GROUP BY 5` — measured, both are `42P10 GROUP BY position N is not in select list`.
pub(super) fn ordinal(position: i64, select: &Select, clause: &str) -> Result<Expr> {
    let item = usize::try_from(position)
        .ok()
        .filter(|position| *position >= 1)
        .and_then(|position| select.projection.get(position - 1));
    match item {
        Some(SelectItem::Expr { expr, .. }) => Ok(expr.clone()),
        // A `*` is not one column, so a position cannot name it. PostgreSQL expands the wildcard
        // before numbering and this crate does not, so the honest answer is the same `42P10`
        // rather than a position that means something different here.
        _ => Err(SqlError::InvalidColumnReference(format!(
            "{clause} position {position} is not in select list"
        ))),
    }
}

/// Whether an expression contains an aggregate call anywhere inside it.
pub(super) fn contains_aggregate(expr: &Expr) -> bool {
    !aggregate_calls(expr).is_empty()
}

/// Every aggregate call in an expression, outermost first.
///
/// A call **inside** another call is `42803` rather than a second entry: PostgreSQL says
/// `aggregate function calls cannot be nested`, and a `sum(count(*))` that quietly computed
/// something would be worse than a refusal.
fn aggregate_calls(expr: &Expr) -> Vec<&AggregateCall> {
    let mut found = Vec::new();
    walk(expr, &mut found);
    found
}

fn walk<'a>(expr: &'a Expr, found: &mut Vec<&'a AggregateCall>) {
    match expr {
        Expr::Aggregate(call) => found.push(call),
        Expr::Binary { left, right, .. } => {
            walk(left, found);
            walk(right, found);
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => walk(operand, found),
        Expr::InList { operand, list, .. } => {
            walk(operand, found);
            for item in list {
                walk(item, found);
            }
        }
        _ => {}
    }
}

/// `sum(count(*))` and friends.
pub(super) fn check_not_nested(expr: &Expr) -> Result<()> {
    for call in aggregate_calls(expr) {
        if call.args.iter().any(contains_aggregate) {
            return Err(SqlError::AggregateNotAllowed(
                "aggregate function calls cannot be nested",
            ));
        }
    }
    Ok(())
}

/// One aggregate, mid-fold.
#[derive(Debug)]
pub(super) struct Accumulator {
    func: AggregateFunc,
    /// The values already folded in, for a `DISTINCT` call. One `BTreeSet` per aggregate rather
    /// than per group is not possible — distinctness is per group — so this lives here.
    seen: Option<BTreeSet<GroupKey>>,
    state: State,
}

#[derive(Debug)]
enum State {
    /// `count(*)` and `count(col)`, which differ only in whether a NULL reaches here.
    Count(i64),
    /// `sum(int8)`. `None` until a non-NULL arrives, because a sum over nothing is NULL.
    SumInt(Option<i64>),
    /// `sum(float8)`, and the running half of `avg(float8)`.
    SumFloat(Option<f64>),
    /// `min`/`max`, holding the best value seen.
    Extreme(Option<Datum>),
    /// `avg(float8)`: the sum, and how many values went into it.
    AvgFloat { sum: f64, seen: i64 },
}

impl Accumulator {
    /// A fresh accumulator for one group.
    pub(super) fn new(spec: &AggregateSpec) -> Self {
        let state = match (spec.func, spec.arg_type) {
            (AggregateFunc::Count, _) => State::Count(0),
            (AggregateFunc::Sum, Some(ColumnType::Int8)) => State::SumInt(None),
            (AggregateFunc::Avg, _) => State::AvgFloat { sum: 0.0, seen: 0 },
            (AggregateFunc::Sum, _) => State::SumFloat(None),
            (AggregateFunc::Min | AggregateFunc::Max, _) => State::Extreme(None),
        };
        Accumulator {
            func: spec.func,
            seen: spec.distinct.then(BTreeSet::new),
            state,
        }
    }

    /// Folds one value in. `Datum::Null` is the argument's value, not its absence: `count(*)`
    /// passes a non-NULL placeholder, so a NULL here always means the column was NULL.
    pub(super) fn push(&mut self, value: &Datum) -> Result<()> {
        // Every aggregate but `count(*)` skips NULLs, and `count(*)` never sees one.
        if matches!(value, Datum::Null) {
            return Ok(());
        }
        // `DISTINCT` is one rule for all five rather than five implementations of it: a value
        // already folded into this group is dropped before it reaches the state.
        if let Some(seen) = &mut self.seen {
            // Bounded for the same reason the group table and the sort are: a `count(DISTINCT c)`
            // over a column with a billion values is a billion values held on a client's behalf.
            if seen.len() == GROUP_LIMIT {
                return Err(SqlError::ConfigurationLimitExceeded(format!(
                    "a DISTINCT aggregate over more than {GROUP_LIMIT} values needs more memory                      than this node will use; add a WHERE"
                )));
            }
            if !seen.insert(GroupKey(vec![value.clone()])) {
                return Ok(());
            }
        }
        match (&mut self.state, value) {
            (State::Count(count), _) => *count += 1,
            (State::SumInt(total), Datum::Int8(value)) => {
                let sum = total.unwrap_or(0);
                // ADR 0031: PostgreSQL's `sum(bigint)` is `numeric` and cannot overflow. Ours is
                // `int8` and errors rather than wrapping, which is what the columnar evaluator
                // does with the same input.
                *total = Some(sum.checked_add(*value).ok_or(SqlError::BigintOutOfRange)?);
            }
            (State::SumFloat(total), Datum::Double(value)) => {
                *total = Some(total.unwrap_or(0.0) + value);
            }
            (State::AvgFloat { sum, seen }, Datum::Double(value)) => {
                *sum += value;
                *seen += 1;
            }
            (State::Extreme(best), value) => {
                let replace = match best {
                    None => true,
                    Some(best) => {
                        let ordering = value.pg_cmp(best);
                        match self.func {
                            AggregateFunc::Min => ordering.is_lt(),
                            _ => ordering.is_gt(),
                        }
                    }
                };
                if replace {
                    *best = Some(value.clone());
                }
            }
            // The planner resolved the argument's type and built the state from it, so a value of
            // another type here is a planner bug rather than a user's mistake.
            (state, value) => {
                return Err(SqlError::Internal(format!(
                    "{value:?} reached a {state:?} accumulator"
                )));
            }
        }
        Ok(())
    }

    /// The group's value.
    ///
    /// The empty case is the one worth reading: `count` is **zero** and everything else is
    /// **NULL**, which is PostgreSQL's rule and the reason a sum over no rows must not be `0` — a
    /// zero there is a wrong answer that looks like data.
    pub(super) fn finish(&self) -> Datum {
        match &self.state {
            State::Count(count) => Datum::Int8(*count),
            State::SumInt(total) => total.map_or(Datum::Null, Datum::Int8),
            State::SumFloat(total) => total.map_or(Datum::Null, Datum::Double),
            State::Extreme(best) => best.clone().unwrap_or(Datum::Null),
            State::AvgFloat { seen: 0, .. } => Datum::Null,
            #[allow(
                clippy::cast_precision_loss,
                reason = "the count is the divisor PostgreSQL's own float8 average divides by"
            )]
            State::AvgFloat { sum, seen } => Datum::Double(sum / (*seen as f64)),
        }
    }
}
