//! Turning an aggregate plan into fragments, asking every region for one, and finishing what
//! comes back.
//!
//! `docs/plans/phase-10-routing.md` U2 and U4. [`crate::plan::routing`] decides *whether*; this
//! decides *what to send* and *what to do with the answer*, and both halves are written against one
//! rule: **a routing decision must never change an answer.**
//!
//! # The substitution, and why it is exactly one shape
//!
//! The only sub-plan replaced is `Aggregate { input: [Filter] { SeqScan } }`, and it is replaced by
//! a node whose output row is the grouping keys followed by the aggregate values — which is
//! precisely what [`Node::Aggregate`] produces and precisely what every expression above it was
//! rewritten against. So the substitution is *exact* by construction rather than by care, and
//! nothing above it changes at all.
//!
//! A bare projection scan is deliberately **not** routed, and the reason is memory rather than
//! taste: a fragment returns its whole answer in one message, so a rows-output fragment is a whole
//! region materialised on the SQL node and framed as one response, where the row path streams a
//! page at a time. An aggregate's answer is one row per group, which is what makes it the shape
//! that fits (`docs/plans/phase-10-routing.md` §6).
//!
//! # Refuse, never partially honour
//!
//! The filter sits *below* the aggregate, so a fragment that aggregated without it would aggregate
//! the wrong rows. Every part of the sub-plan is therefore expressed or the whole substitution is
//! declined (ADR 0022 Decision 3), and declining is free: the row plan is what was already there.
//!
//! # `sum(double)` does not associate
//!
//! Combining partials adds numbers in an order a single-level fold would not, so a `sum(double)`
//! finished from many regions may differ in its last bits from the same query over one. It is
//! inherent to two-level aggregation and it is documented at each place a reader meets it —
//! `esker_columnar`'s `Partial::combine`, `esker_proto::fragment::result`, and here. What this
//! module owes it is **determinism**: regions are folded in key order, always, so the answer does
//! not depend on which learner replied first.

use std::collections::BTreeMap;

use esker_columnar::fragment::Aggregate as ColAggregate;
use esker_proto::fragment::result::{Body, Partial, Value as WireValue};
use esker_proto::fragment::{RefusalReason, ScanStats};

use crate::backend::Txn;
use crate::catalog::TableDef;
use crate::exec::query::Planned;
use crate::fragment::{Answer, FragmentSource, Shard};
use crate::plan::routing::{
    Columnar, Decision, Engine, Finish, Output, Reason, Run, Setting, Shape,
};
use crate::plan::{AggregateFunc, AggregateSpec, BinaryOp, Expr, Node, Probe, routing};
use crate::value::{Datum, PgDatum};

/// Built, or the decision that says why not.
///
/// The error side is a [`Decision`] rather than a [`crate::error::SqlError`] on purpose: nothing
/// in this module can fail a statement. Every refusal is *a plan on rows*, which is the plan that
/// was already there, and carrying the reason in the `Err` is what lets one `?` per rule read as
/// "and here is what `EXPLAIN` will say".
type Routed<T> = Result<T, Decision>;

/// A group's key, ordered the way this system orders values.
///
/// **`pg_cmp`, not `Datum`'s own `PartialEq`**, and it is the same rule `GROUP BY` uses everywhere
/// else here: `-0.0` and `0.0` are one group and so are two `NaN`s. Merging partials from several
/// regions is exactly a `GROUP BY` across them, so a key that compared bitwise would split a group
/// the row engine keeps together — the disagreement this whole milestone's differential exists to
/// catch.
///
/// It also buys determinism: a `BTreeMap` keyed by this folds regions in key order whichever
/// learner answered first, which is what makes a `sum(double)` comparison meaningful rather than
/// flaky.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupKey(Vec<Datum>);

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        for (left, right) in self.0.iter().zip(&other.0) {
            match left.pg_cmp(right) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Rewrites `planned` to run on columns, when every rule says it may.
///
/// Never fails the statement: a shape that cannot be routed, a catalog read that cannot say, or a
/// node with nothing to ask leaves the plan exactly as it was. The reason is attached either way,
/// so `EXPLAIN` can name it.
pub(super) fn route(
    txn: &dyn Txn,
    tenant: u64,
    table: &TableDef,
    inners: &[&TableDef],
    source: Option<&dyn FragmentSource>,
    setting: Setting,
    planned: &mut Planned,
) {
    let decision = match consider(txn, tenant, table, inners, source, setting, planned) {
        Ok(built) => return apply(planned, built),
        Err(reason) => reason,
    };
    planned.engine = Some(decision);
}

/// The decision, and the node that carries it, when there is one.
struct Built {
    columnar: Columnar,
    /// Where in the plan the aggregate sat, so the caller can put the node back in its place.
    having: Option<Expr>,
}

/// Puts the columnar node where the aggregate was.
///
/// `HAVING` comes *out* of the aggregate and becomes a [`Node::Filter`] above the substitution, on
/// both paths. The two are the same operator over the same row — the aggregate's docs say `HAVING`
/// is "resolved against the **output** row", and a filter keeps the rows its predicate is true for
/// — so moving it changes nothing, and moving it on *both* paths is what stops it being applied
/// twice when the fallback runs.
fn apply(planned: &mut Planned, built: Built) {
    planned.engine = Some(built.columnar.decision.clone());
    let mut node = Node::Columnar(Box::new(built.columnar));
    if let Some(having) = built.having {
        node = Node::Filter {
            input: Box::new(node),
            predicate: having,
        };
    }
    replace_aggregate(&mut planned.node, node);
}

/// Swaps the plan's one `Aggregate` for `replacement`, wherever it sits.
///
/// A walk rather than an index, because the nodes above an aggregate are whatever the statement
/// asked for — a `Project`, a `Sort`, a `Limit`, or all three.
fn replace_aggregate(node: &mut Node, replacement: Node) {
    if matches!(node, Node::Aggregate { .. }) {
        *node = replacement;
        return;
    }
    match node {
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::Distinct { input } => replace_aggregate(input, replacement),
        _ => {}
    }
}

/// Builds the fragment, or says which rule said no.
#[allow(
    clippy::too_many_lines,
    reason = "one rule per block, each with its reason"
)]
/// Which of the semi-join rewrite's conditions this join fails, as `EXPLAIN` will print it.
///
/// `docs/plans/phase-16-mpp.md` §J3. A join reaches the columnar path by becoming a **semi-join**:
/// the inner side's key set is read on this node and pushed into the outer table's fragment as a
/// membership test. That substitution is exact only under conditions the plan tree already
/// records, and each of them refuses with its own sentence — because a refusal for the wrong
/// reason is a bug that still passes a test which only checks that it fell back.
///
/// `None` is no objection — the shape becomes a semi-join and [`absorb_join`] builds it.
fn why_this_join_stays_on_rows(scan: &Node) -> Option<&'static str> {
    let Node::NestedLoop {
        left_join,
        probe,
        residual,
        inner_view,
        inner_plan,
        ..
    } = scan
    else {
        return Some("this access path");
    };
    if *left_join {
        // A LEFT JOIN keeps an unmatched outer row with every inner column NULL; a filter removes
        // it. The two answers differ by exactly the rows the join exists to preserve.
        return Some("a LEFT JOIN, whose unmatched rows a filter would drop");
    }
    if inner_view.is_some() || inner_plan.is_some() {
        return Some("a join whose inner side is a catalog view or a derived table");
    }
    if !matches!(probe, Probe::PrimaryKey { .. } | Probe::UniqueIndex { .. }) {
        // The condition the whole rewrite rests on: only a unique inner key makes "at most one
        // inner row per outer row" true, and only that makes a membership test preserve a count.
        return Some("a join on a non-unique inner column, where one outer row may match many");
    }
    if residual.is_some() {
        return Some("a join with a condition the probe did not express");
    }
    None
}

/// Every table column the fragment must read, in table order and deduplicated.
///
/// The grouping keys, the aggregate arguments, and everything the filter names. The projection is
/// the only place a table column index appears in a fragment (`docs/DESIGN.md` §16.2), so this
/// list is also what decides the ratio.
fn columns_the_fragment_reads(
    keys: &[Expr],
    aggregates: &[AggregateSpec],
    filter: Option<&Expr>,
) -> Routed<Vec<usize>> {
    let mut columns: Vec<usize> = Vec::new();
    for key in keys {
        columns.push(ordinal(key).ok_or_else(|| {
            Decision::rows(Reason::NotExpressible("a GROUP BY over an expression"))
        })?);
    }
    for spec in aggregates {
        if spec.distinct {
            return Err(Decision::rows(Reason::NotExpressible(
                "a DISTINCT aggregate",
            )));
        }
        if let Some(arg) = &spec.arg {
            columns.push(ordinal(arg).ok_or_else(|| {
                Decision::rows(Reason::NotExpressible("an aggregate over an expression"))
            })?);
        }
    }
    if let Some(filter) = filter {
        collect_columns(filter, &mut columns);
    }
    columns.sort_unstable();
    columns.dedup();
    Ok(columns)
}

/// The rules that hold whatever the plan looks like, answered before its shape is examined.
///
/// **Order matters and is the argument.** A table nobody asked for a columnar copy of has no
/// second engine to choose between, and every reason after this one describes a *choice* — which
/// `EXPLAIN` then prints. Deciding it here is what keeps an ordinary table's plan from growing a
/// line about a feature it is not using (ADR 0040 Decision 3's first deliberate silence).
fn before_the_shape<'a>(
    txn: &dyn Txn,
    tenant: u64,
    table: &TableDef,
    source: Option<&'a dyn FragmentSource>,
    setting: Setting,
) -> Routed<(&'a dyn FragmentSource, u8)> {
    if setting == Setting::Row {
        return Err(Decision::rows(Reason::Override(Setting::Row)));
    }
    let Some(source) = source else {
        return Err(Decision::rows(Reason::NoFragmentService));
    };
    // ADR 0022 Decision 2 rule 2, and the only rule here that is about a wrong answer rather than
    // a slow one: a learner has not seen this transaction's uncommitted writes, so a read that
    // must see them cannot be answered there at any price.
    if txn.has_written() {
        return Err(Decision::rows(Reason::NotExpressible(
            "a read of a transaction's own writes",
        )));
    }
    let replicas = replicas(txn, tenant, table.id);
    if replicas == 0 {
        return Err(Decision::rows(Reason::NotAsked));
    }
    Ok((source, replicas))
}

/// The unbounded sequential scan a fragment needs, or the reason this access path is not one.
///
/// A point get or an index lookup is what ADR 0022 rule 1 keeps on rows; a join names which of the
/// semi-join conditions refused it; a catalog view is a shape no fragment expresses. Each answers
/// `Bounded` only when it really is bounded.
fn the_scan_under_it(scan: &Node) -> Routed<(u64, bool)> {
    match scan {
        Node::SeqScan {
            table_id, narrowed, ..
        } => Ok((*table_id, *narrowed)),
        Node::PointGet { .. } | Node::IndexLookup { .. } => Err(Decision::rows(Reason::Bounded)),
        Node::NestedLoop { .. } => Err(Decision::rows(Reason::NotExpressible(
            why_this_join_stays_on_rows(scan).unwrap_or("a join this build cannot express"),
        ))),
        _ => Err(Decision::rows(Reason::NotExpressible("this access path"))),
    }
}

/// The conjuncts of an `AND` chain, flattened. A non-`AND` expression is one conjunct.
fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut out = conjuncts(left);
            out.extend(conjuncts(right));
            out
        }
        other => vec![other],
    }
}

/// Rebuilds `expr` with every column position shifted down by `outer_width`.
///
/// **A whitelist, not a general walk, and that is deliberate.** A rebase that missed one
/// expression variant would leave an ordinal pointing at the wrong column and return a wrong
/// answer silently — the failure this whole milestone exists to avoid. So exactly two shapes are
/// accepted, `column op literal` and its mirror, and every other conjunct refuses. `d.bucket = 1`
/// is the shape the measured query has; a richer grammar is worth having only with a test per
/// variant behind it.
fn rebased_onto_the_inner_row(expr: &Expr, outer_width: usize) -> Option<Expr> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    let shift = |side: &Expr| -> Option<Expr> {
        match side {
            Expr::Ordinal { at, ty, typmod } => Some(Expr::Ordinal {
                at: at.checked_sub(outer_width)?,
                ty: *ty,
                typmod: *typmod,
            }),
            Expr::Literal(literal) => Some(Expr::Literal(literal.clone())),
            _ => None,
        }
    };
    match (&**left, &**right) {
        (Expr::Ordinal { .. }, Expr::Literal(_)) | (Expr::Literal(_), Expr::Ordinal { .. }) => {
            Some(Expr::Binary {
                op: *op,
                left: Box::new(shift(left)?),
                right: Box::new(shift(right)?),
            })
        }
        _ => None,
    }
}

/// Every column position in `expr`, as a set.
fn ordinals_of(expr: &Expr) -> Vec<usize> {
    let mut out = Vec::new();
    collect_columns(expr, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

/// Splits the join's `WHERE` by which side each conjunct names.
///
/// An outer-only conjunct goes into the fragment; an inner-only one filters the key set; one
/// naming **both** is a condition over the *pair*, which is not a semi-join at all and refuses.
fn split_by_side(
    filter: Option<&Expr>,
    outer_filter: Option<&Expr>,
    outer_width: usize,
) -> Routed<(Vec<Expr>, Vec<Expr>)> {
    let plain = |reason: &'static str| Decision::rows(Reason::NotExpressible(reason));
    let mut outer_side: Vec<Expr> = outer_filter.into_iter().cloned().collect();
    let mut inner_side: Vec<Expr> = Vec::new();
    for conjunct in filter.map(conjuncts).unwrap_or_default() {
        let ordinals = ordinals_of(conjunct);
        if ordinals.iter().all(|at| *at < outer_width) {
            outer_side.push(conjunct.clone());
        } else if ordinals.iter().all(|at| *at >= outer_width) {
            inner_side.push(
                rebased_onto_the_inner_row(conjunct, outer_width)
                    .ok_or_else(|| plain("a join condition over a shape this build cannot move"))?,
            );
        } else {
            return Err(plain("a join condition naming both sides"));
        }
    }
    Ok((outer_side, inner_side))
}

/// What a join, absorbed, leaves for the fragment to be built from.
struct Absorbed {
    /// The outer table's scan.
    table_id: u64,
    narrowed: bool,
    /// The predicate the fragment carries: the outer side's own, and every conjunct of the
    /// join's `WHERE` that names only outer columns.
    filter: Option<Expr>,
    /// How to read the inner key set, when a join was absorbed.
    semi: Option<routing::SemiJoin>,
    /// The outer table column the membership test is applied to.
    outer_column: Option<usize>,
}

/// Turns `Aggregate { [Filter] { NestedLoop … } }` into a scan the fragment can express, by
/// rewriting the join as a **semi-join** (`docs/plans/phase-16-mpp.md` §J3).
///
/// Every refusal names its own condition, because a refusal for the wrong reason is a bug that
/// still passes a test which only checks that the query fell back.
fn absorb_join(
    scan: &Node,
    filter: Option<&Expr>,
    tenant: u64,
    outer: &TableDef,
    inners: &[&TableDef],
) -> Routed<Absorbed> {
    let plain = |reason: &'static str| Decision::rows(Reason::NotExpressible(reason));
    let Node::NestedLoop {
        outer: outer_side,
        inner_table_id,
        inner_columns,
        probe,
        ..
    } = scan
    else {
        return Err(plain(
            why_this_join_stays_on_rows(scan).unwrap_or("this access path"),
        ));
    };
    if let Some(refusal) = why_this_join_stays_on_rows(scan) {
        return Err(plain(refusal));
    }

    // The outer side must itself be the shape a fragment expresses: an unbounded scan, with its
    // own filter if it has one.
    let (outer_filter, outer_scan) = match &**outer_side {
        Node::Filter { input, predicate } => (Some(predicate), &**input),
        other => (None, other),
    };
    let (table_id, narrowed) = the_scan_under_it(outer_scan)?;

    let inner = inners
        .iter()
        .find(|def| def.id == *inner_table_id)
        .ok_or_else(|| plain("a join whose inner table this node cannot resolve"))?;
    if !inner.child_scans.is_empty() {
        // A scan of an inherited table returns its children's rows too, and the key plan built
        // below reads only the table's own range. Refused rather than silently reading less.
        return Err(plain("a join whose inner table has children"));
    }
    let [key_column] = inner.primary_key[..] else {
        return Err(plain(
            "a join whose inner key is not a single primary-key column",
        ));
    };
    let key_def = inner
        .columns
        .get(key_column)
        .ok_or_else(|| plain("a join whose inner key names no column"))?;

    let (fragment_conjuncts, key_conjuncts) =
        split_by_side(filter, outer_filter, outer.columns.len())?;

    let (Probe::PrimaryKey { outer: at } | Probe::UniqueIndex { outer: at, .. }) = probe else {
        return Err(plain(
            why_this_join_stays_on_rows(scan).unwrap_or("this access path"),
        ));
    };

    let (start, end) = esker_keys::row::table_row_range(tenant, inner.id);
    let mut keys = Node::SeqScan {
        table_id: inner.id,
        columns: inner_columns.clone(),
        start,
        end,
        narrowed: false,
        inherited: Vec::new(),
    };
    if let Some(predicate) = key_conjuncts
        .into_iter()
        .reduce(|left, right| Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        })
    {
        keys = Node::Filter {
            input: Box::new(keys),
            predicate,
        };
    }
    let keys = Node::Project {
        input: Box::new(keys),
        exprs: vec![Expr::Ordinal {
            at: key_column,
            ty: key_def.ty,
            typmod: key_def.typmod,
        }],
    };

    Ok(Absorbed {
        table_id,
        narrowed,
        filter: fragment_conjuncts
            .into_iter()
            .reduce(|left, right| Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(left),
                right: Box::new(right),
            }),
        semi: Some(routing::SemiJoin {
            keys: Box::new(keys),
            outer_slot: 0,
            inner_table: inner.name.clone(),
        }),
        outer_column: Some(*at),
    })
}

/// One columnar type per projection slot, or the refusal for a column with no columnar type.
fn slot_types_of(table: &TableDef, columns: &[usize]) -> Routed<Vec<esker_columnar::ColumnType>> {
    let types: Vec<esker_columnar::ColumnType> = columns
        .iter()
        .filter_map(|column| {
            table
                .columns
                .get(*column)
                .and_then(|def| column_type(def.ty))
        })
        .collect();
    if types.len() == columns.len() {
        Ok(types)
    } else {
        Err(Decision::rows(Reason::NotExpressible(
            "a column this table's record does not describe",
        )))
    }
}

/// The scan a fragment will be built over: a plain one absorbs nothing, a join becomes a
/// semi-join or refuses saying which rule stopped it (`docs/plans/phase-16-mpp.md` §J3).
fn absorbed_scan(
    scan: &Node,
    filter: Option<&Expr>,
    tenant: u64,
    table: &TableDef,
    inners: &[&TableDef],
) -> Routed<Absorbed> {
    match scan {
        Node::NestedLoop { .. } => absorb_join(scan, filter, tenant, table, inners),
        other => {
            let (table_id, narrowed) = the_scan_under_it(other)?;
            Ok(Absorbed {
                table_id,
                narrowed,
                filter: filter.cloned(),
                semi: None,
                outer_column: None,
            })
        }
    }
}

/// The fragment's projection: what the aggregate needs, plus the outer join column.
///
/// The membership test a semi-join pushes down reads that column, and the projection is the only
/// place a table column index appears in a fragment (`docs/DESIGN.md` §16.2) — so a join that did
/// not add it here would name a slot the fragment never asked for.
fn projected_columns(
    keys: &[Expr],
    aggregates: &[AggregateSpec],
    filter: Option<&Expr>,
    outer_column: Option<usize>,
) -> Routed<Vec<usize>> {
    let mut columns = columns_the_fragment_reads(keys, aggregates, filter)?;
    if let Some(column) = outer_column {
        columns.push(column);
        columns.sort_unstable();
        columns.dedup();
    }
    Ok(columns)
}

fn consider(
    txn: &dyn Txn,
    tenant: u64,
    table: &TableDef,
    inners: &[&TableDef],
    source: Option<&dyn FragmentSource>,
    setting: Setting,
    planned: &Planned,
) -> Routed<Built> {
    let (source, replicas) = before_the_shape(txn, tenant, table, source, setting)?;

    let Some(aggregate) = find_aggregate(&planned.node) else {
        return Err(Decision::rows(Reason::NotExpressible("this plan's shape")));
    };
    let Node::Aggregate {
        input,
        keys,
        aggregates,
        having,
        grouped,
    } = aggregate
    else {
        unreachable!("find_aggregate returns an Aggregate");
    };

    // The scan under it, and the filter between them if there is one.
    let (filter, scan) = match &**input {
        Node::Filter { input, predicate } => (Some(predicate), &**input),
        other => (None, other),
    };
    let absorbed = absorbed_scan(scan, filter, tenant, table, inners)?;
    let Absorbed {
        table_id,
        narrowed,
        filter,
        mut semi,
        outer_column,
    } = absorbed;
    if narrowed {
        return Err(Decision::rows(Reason::Bounded));
    }

    let filter = filter.as_ref();
    let columns = projected_columns(keys, aggregates, filter, outer_column)?;
    let slot = |column: usize| -> u32 {
        u32::try_from(columns.binary_search(&column).unwrap_or(0)).unwrap_or(0)
    };
    let projection: Vec<u32> = columns
        .iter()
        .map(|column| u32::try_from(*column).unwrap_or(u32::MAX))
        .collect();

    // The aggregates, in the order the output row carries them, and the plan for finishing each.
    let mut asks: Vec<ColAggregate> = Vec::new();
    let mut outputs: Vec<Output> = (0..keys.len()).map(Output::Key).collect();
    for spec in aggregates {
        outputs.push(Output::Aggregate(push_down(spec, &mut asks, &slot)?));
    }

    let group_by: Vec<u32> = keys
        .iter()
        .filter_map(|key| ordinal(key).map(&slot))
        .collect();

    let slot_types = slot_types_of(table, &columns)?;

    if let (Some(semi), Some(column)) = (semi.as_mut(), outer_column) {
        semi.outer_slot = slot(column);
    }

    let mut fragment = esker_columnar::Fragment::aggregate(
        esker_columnar::TableRef { tenant, table_id },
        projection,
        group_by,
        asks,
    );
    if let Some(filter) = filter {
        fragment.filter = Some(push_filter(filter, &slot, &slot_types)?);
    }

    let shape = Shape {
        stored: table.columns.len(),
        projected: columns.len(),
        bounded: false,
        replicas,
    };
    let decision = routing::decide(shape, setting);
    if decision.engine == Engine::Row {
        return Err(decision);
    }

    // Last, because it is the only step that costs a round trip: a query the rule was never going
    // to route should not pay for a routing table.
    let (start, end) = esker_keys::row::table_row_range(tenant, table.id);
    let shards = source
        .shards(&start, &end)
        .map_err(|_| Decision::rows(Reason::NoFragmentService))?;
    if shards.is_empty() || !shards.iter().all(Shard::is_columnar) {
        return Err(Decision::rows(Reason::NoLearner));
    }

    let mut fallback = aggregate.clone();
    if let Node::Aggregate { having, .. } = &mut fallback {
        // Taken out of both paths, so the `Filter` above applies it exactly once whichever ran.
        *having = None;
    }
    Ok(Built {
        columnar: Columnar {
            table_id,
            table: planned.table.clone(),
            fragment,
            outputs,
            grouped: *grouped,
            decision,
            shards,
            fallback: Box::new(fallback),
            run: None,
            semi_join: semi,
        },
        having: having.clone(),
    })
}

/// One aggregate, pushed down: what to ask the regions for, and how to finish it.
fn push_down(
    spec: &AggregateSpec,
    asks: &mut Vec<ColAggregate>,
    slot: &impl Fn(usize) -> u32,
) -> Routed<Finish> {
    let at = asks.len();
    let arg = spec.arg.as_ref().and_then(ordinal).map(slot);
    Ok(match (spec.func, arg) {
        (AggregateFunc::Count, None) => {
            asks.push(ColAggregate::CountStar);
            Finish::Count(at)
        }
        (AggregateFunc::Count, Some(slot)) => {
            asks.push(ColAggregate::Count(slot));
            Finish::Count(at)
        }
        (AggregateFunc::Sum, Some(slot)) => {
            asks.push(ColAggregate::Sum(slot));
            Finish::Sum(at)
        }
        (AggregateFunc::Min, Some(slot)) => {
            asks.push(ColAggregate::Min(slot));
            Finish::Min(at)
        }
        (AggregateFunc::Max, Some(slot)) => {
            asks.push(ColAggregate::Max(slot));
            Finish::Max(at)
        }
        // **`avg` has no partial that combines**, so it is asked for as two that do. Averaging two
        // averages is right only when the groups are the same size, and they are not.
        (AggregateFunc::Avg, Some(slot)) => {
            asks.push(ColAggregate::Sum(slot));
            asks.push(ColAggregate::Count(slot));
            Finish::Avg {
                sum: at,
                count: at + 1,
            }
        }
        // `sum(*)`, `min(*)` and the rest have no spelling in SQL; a `count(*)` with an argument
        // does not either. Reaching here means the aggregate was built from something this
        // function has not been taught, and refusing is the rule.
        _ => {
            return Err(Decision::rows(Reason::NotExpressible(
                "this aggregate over this argument",
            )));
        }
    })
}

/// The `WHERE` clause, over projection slots.
///
/// Every node this crate's expression language has that a fragment's does not is a refusal, named.
/// `IN` is the interesting one: it is not lowered to a chain of `OR`s, because the fragment's
/// evaluator would then evaluate the left-hand side once per item — the same reason
/// [`crate::plan::Expr::InList`] is carried as itself here.
fn push_filter(
    expr: &Expr,
    slot: &impl Fn(usize) -> u32,
    types: &[esker_columnar::ColumnType],
) -> Routed<esker_columnar::fragment::Expr> {
    use crate::plan::BinaryOp;
    use esker_columnar::fragment::{CompareOp, Expr as ColExpr};

    let refused = |what: &'static str| Decision::rows(Reason::NotExpressible(what));
    Ok(match expr {
        Expr::Ordinal { at, .. } => ColExpr::Column(slot(*at)),
        // A cast is not expressible in the fragment language, so the filter stays on the row side.
        Expr::ToText { .. } => return Err(refused("a cast to text")),
        // The fragment language has no array value to build, so a constructor keeps its filter on
        // the row side rather than being half-pushed.
        Expr::Array { .. } => return Err(refused("an ARRAY constructor")),
        // Neither is arithmetic: the fragment language compares and combines, and every operator
        // brings an overflow rule the scan would have to reproduce exactly to be worth pushing.
        Expr::Arithmetic { .. } | Expr::Negate(_) => return Err(refused("arithmetic")),
        // Not expressible in the fragment language; the filter stays on the row side.
        Expr::Scalar { .. } => return Err(refused("a scalar function")),
        // The fragment language has no pattern match; the filter stays on the row side.
        Expr::Like { .. } => return Err(refused("LIKE")),
        Expr::RegexMatch { .. } => return Err(refused("a regular-expression match")),
        // The fragment language has no conditional, and a `CASE` is the one expression whose
        // branches must **not** all be evaluated — pushing it down as anything else would change
        // which of them raises. Rows, and the row evaluator answers it.
        // A fragment is pushed down to a learner that has no expression evaluator of its own, so
        // the two conditional constructs are refused there and computed here.
        // A fragment is a projection a learner evaluates, and a set-returning call makes rows —
        // which is a shape the fragment protocol has no room for.
        Expr::SetFunc(_) => return Err(refused("a set-returning function")),
        Expr::Coalesce(_) => return Err(refused("a COALESCE expression")),
        Expr::Case { .. } => return Err(refused("a CASE expression")),
        // The fragment language has no array. Rows, and the row evaluator answers it.
        Expr::AnyArray { .. } => return Err(refused("= ANY over an array value")),
        Expr::Subscript { .. } => return Err(refused("an array subscript")),
        // Volatile: a fragment the columnar side evaluated would answer a different UUID from the
        // row side, which is the one thing a differential must never allow.
        Expr::Uuid(_) => return Err(refused("a UUID function")),
        Expr::Literal(literal) => ColExpr::Literal(literal_value(literal)?),
        // A catalog function is a function of the catalog, not of the fragment's columns, and the
        // columnar reader has no expression for it. Rows, and the row evaluator answers it.
        Expr::CatalogFunc(_) => return Err(refused("a catalog function")),
        Expr::Not(inner) => ColExpr::Not(Box::new(push_filter(inner, slot, types)?)),
        Expr::IsNull { operand, negated } => ColExpr::IsNull {
            operand: Box::new(push_filter(operand, slot, types)?),
            negated: *negated,
        },
        // **A null-safe comparison is not one of the fragment's six.** The columnar reader's
        // `Compare` follows the ordinary NULL rule, so pushing `IS NOT DISTINCT FROM` down as an
        // `Eq` would answer unknown where it must answer `false`. Rows, and the row evaluator.
        Expr::Binary {
            op: op @ (BinaryOp::Distinct | BinaryOp::NotDistinct),
            ..
        } => return Err(refused(op.symbol())),
        Expr::Binary { op, left, right } => {
            let left = push_filter(left, slot, types)?;
            let right = push_filter(right, slot, types)?;
            if op.is_comparison() {
                comparable(&left, &right, types)?;
            }
            let (left, right) = (Box::new(left), Box::new(right));
            match op {
                BinaryOp::And => ColExpr::And(left, right),
                BinaryOp::Or => ColExpr::Or(left, right),
                BinaryOp::Eq => ColExpr::Compare {
                    op: CompareOp::Eq,
                    left,
                    right,
                },
                BinaryOp::NotEq => ColExpr::Compare {
                    op: CompareOp::NotEq,
                    left,
                    right,
                },
                BinaryOp::Lt => ColExpr::Compare {
                    op: CompareOp::Lt,
                    left,
                    right,
                },
                BinaryOp::LtEq => ColExpr::Compare {
                    op: CompareOp::LtEq,
                    left,
                    right,
                },
                BinaryOp::Gt => ColExpr::Compare {
                    op: CompareOp::Gt,
                    left,
                    right,
                },
                BinaryOp::GtEq => ColExpr::Compare {
                    op: CompareOp::GtEq,
                    left,
                    right,
                },
                BinaryOp::Distinct | BinaryOp::NotDistinct => {
                    unreachable!("refused above")
                }
            }
        }
        Expr::InList { .. } => return Err(refused("IN inside a pushed-down filter")),
        // A fragment's filter language has no subquery in it, and adding one would mean sending a
        // plan to a learner rather than an expression. `docs/plans/phase-12-subquery.md` §4 says
        // this refusal is by construction rather than by remembering to check, and this is the
        // line it means: a query with a subquery in its `WHERE` runs on rows.
        Expr::Subquery(sub) => return Err(refused(sub.kind.describe())),
        Expr::Column { .. } => return Err(refused("an unresolved column")),
        Expr::Outer { .. } => return Err(refused("a correlated column reference")),
        Expr::Parameter(_) => return Err(refused("a parameter inside a pushed-down filter")),
        Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentUser
        | Expr::CurrentSetting { .. }
        | Expr::Advisory { .. } => {
            return Err(refused("current_schema inside a pushed-down filter"));
        }
        Expr::Default | Expr::Sequence(_) | Expr::Aggregate(_) => {
            return Err(refused("this expression"));
        }
    })
}

/// A literal, as a columnar value.
///
/// The two vocabularies are the same six types plus the two ADR 0033 added, which is not a
/// coincidence — `esker_columnar::value` copies the row side's tag bytes and a test called
/// `tags_match_the_row_side` says so. What can still fail is a literal that has no type yet,
/// which the planner resolves before a comparison and which is refused here rather than guessed.
fn literal_value(literal: &crate::plan::Literal) -> Routed<esker_columnar::Value> {
    use crate::plan::Literal;
    use esker_columnar::Value;

    // **The row evaluator's own mapping, and that is the specification rather than a
    // convenience.** `crate::exec::cursor`'s row evaluator reads a bare integer as `int8`, a
    // decimal as `float8` and a quoted string as `text`, and a fragment has to compare what the
    // row engine would have compared. Anything the planner already resolved against a column's
    // type arrives as `Typed` and is used as it is. Whether the result may be compared with the
    // column beside it is [`comparable`]'s question, not this one's.
    Ok(match literal {
        Literal::Null | Literal::TypedNull(_) => Value::Null,
        Literal::Bool(flag) => Value::Bool(*flag),
        Literal::Integer(int) => Value::Int8(*int),
        Literal::String(text) => Value::Text(text.clone()),
        Literal::Typed(datum) => datum_to_value(datum),
        Literal::Decimal(digits) => {
            let Ok(Datum::Double(double)) =
                Datum::from_text(crate::value::ColumnType::Double, digits)
            else {
                return Err(Decision::rows(Reason::NotExpressible(
                    "a decimal literal this node cannot read as float8",
                )));
            };
            Value::Double(double)
        }
    })
}

/// Whether a comparison may be pushed down: its two sides must be the same type.
///
/// **The far side compares with a second implementation of `pg_cmp`**
/// (`esker_columnar::ValueRef::pg_cmp`), and two implementations agree about same-typed values by
/// construction and about mixed ones only by luck. So a literal whose type is not the column's is
/// refused rather than sent — the row plan above still evaluates the same predicate, so the cost
/// is a fallback and never an answer. NULL fits every type and is not a mismatch: it makes a
/// comparison unknown on both sides, which is the same answer.
fn comparable(
    left: &esker_columnar::fragment::Expr,
    right: &esker_columnar::fragment::Expr,
    types: &[esker_columnar::ColumnType],
) -> Routed<()> {
    use esker_columnar::fragment::Expr as ColExpr;

    let refused = Decision::rows(Reason::NotExpressible(
        "a comparison between a column and a value of another type",
    ));
    let (slot, value) = match (left, right) {
        (ColExpr::Column(slot), ColExpr::Literal(value))
        | (ColExpr::Literal(value), ColExpr::Column(slot)) => (*slot, value),
        // Two literals, or two columns. Column-to-column is the useful one and it is left for a
        // milestone that can type it; a fragment that guessed here would be comparing whatever
        // the evaluator decided.
        _ => {
            return Err(Decision::rows(Reason::NotExpressible(
                "a comparison that is not a column against a value",
            )));
        }
    };
    let Some(ty) = types.get(slot as usize) else {
        return Err(refused);
    };
    if value.fits(*ty) {
        Ok(())
    } else {
        Err(refused)
    }
}

/// A row-side column type as the columnar vocabulary spells it.
///
/// A total match, so a ninth type on either side is a compile error here rather than a column that
/// silently stops being comparable — the same rule `esker_store::columnar::wire` keeps for the
/// value vocabulary, and for the same reason.
fn column_type(ty: crate::value::ColumnType) -> Option<esker_columnar::ColumnType> {
    use crate::value::ColumnType as Row;
    use esker_columnar::ColumnType as Col;

    Some(match ty {
        // **Not a columnar type.** A `regtype` cannot be a stored column here (ADR 0077), so the
        // scan never sees one; `None` keeps the filter on the row side rather than inventing a
        // representation for a value the columnar format has no tag for.
        Row::RegType | Row::RegTypeArray => return None,
        Row::Int8 => Col::Int8,
        Row::Time => Col::Time,
        Row::Uuid => Col::Uuid,
        Row::Interval => Col::Interval,
        Row::Oid => Col::Oid,
        Row::Int4 => Col::Int4,
        Row::Int2 => Col::Int2,
        Row::Real => Col::Real,
        Row::Text => Col::Text,
        Row::Varchar => Col::Varchar,
        Row::Bpchar => Col::Bpchar,
        Row::Json => Col::Json,
        Row::Jsonb => Col::Jsonb,
        Row::Bool => Col::Bool,
        Row::Bytea => Col::Bytea,
        Row::TimestampTz => Col::TimestampTz,
        Row::Timestamp => Col::Timestamp,
        Row::Double => Col::Double,
        Row::Date => Col::Date,
        Row::Numeric => Col::Numeric,
        // **The columnar format has no array run yet**, so a table with an array column is read
        // by the row engine — which is the answer the caller's length check already produces,
        // and a `NotExpressible` rather than a wrong one. The alternative is an array run in
        // `esker-columnar`'s own vocabulary, which is a unit of its own.
        // An hstore is not columnar, the same deliberate gap an array is — see
        // `esker_store::columnar::decode::columnar_type`, which says why.
        Row::Int8Array
        | Row::Int4Array
        | Row::Int2Array
        | Row::NumericArray
        | Row::TextArray
        | Row::Hstore
        | Row::HstoreArray
        | Row::TsVector
        | Row::TsQuery
        | Row::TsVectorArray
        | Row::TsQueryArray
        | Row::Citext
        | Row::TsRange
        | Row::TstzRange
        | Row::Int4Range
        | Row::TsRangeArray
        | Row::DateRange
        | Row::NumRange
        | Row::Int8Range
        | Row::FloatRange
        | Row::VarcharRange
        | Row::TstzRangeArray
        | Row::Int4RangeArray
        | Row::DateRangeArray
        | Row::NumRangeArray
        | Row::Int8RangeArray
        | Row::Point
        | Row::PointArray
        // **A money is refused here and is an index key**, which is not a contradiction: the row
        // codec knows it is cents in an `i64` and this vocabulary has no way to carry a *type*
        // that shares its bits with `int8` — `value_to_datum` reads a wire value with no column
        // type in hand, so a money pushed down would come back a `bigint`.
        | Row::Money
        | Row::MoneyArray
        | Row::Inet
        | Row::Cidr
        | Row::MacAddr
        | Row::InetArray
        | Row::CidrArray
        | Row::MacAddrArray
        | Row::Bit
        | Row::VarBit
        | Row::BitArray
        | Row::VarBitArray
        | Row::Lseg
        | Row::Box
        | Row::Path
        | Row::Polygon
        | Row::Circle
        | Row::Line
        // `xml` has no columnar run for `json`'s reason without `json`'s exception: that crate's
        // vocabulary has a `Json` and no `Xml`, and adding one is a unit in `esker-columnar`.
        | Row::Xml
        | Row::XmlArray
        // Not columnar for `citext`'s reason: an ltree's comparison is not its bytes', so a
        // columnar run could not sort or filter one without a vocabulary for that order.
        | Row::Ltree
        | Row::LtreeArray
        | Row::LQuery
        | Row::BoolArray
        | Row::ByteaArray
        | Row::BpcharArray
        | Row::VarcharArray
        | Row::DateArray
        | Row::TimeArray
        | Row::TimestampArray
        | Row::TimestampTzArray
        | Row::IntervalArray
        | Row::RealArray
        | Row::DoubleArray
        | Row::UuidArray
        | Row::JsonArray
        | Row::JsonbArray
        | Row::OidArray
        | Row::CitextArray => {
            return None;
        }
    })
}

/// A row-side datum as a columnar value. Total on both sides by construction.
fn datum_to_value(datum: &Datum) -> esker_columnar::Value {
    use esker_columnar::Value;
    match datum {
        // An array never reaches this: `column_type` refuses the column, so no fragment is built
        // over one. `Null` rather than a panic — a total function over a value vocabulary, where
        // a wrong *answer* would be a filter that silently matched.
        // A citext never reaches this either — the column is refused above, for the same reason
        // an array is: this vocabulary has no way to carry a comparison that folds.
        // A point never reaches this either: `column_type` refuses the column, for the reason
        // a citext's is refused — this vocabulary has no way to carry a type with no
        // comparison at all.
        Datum::Point { .. }
        | Datum::Money(_)
        | Datum::Inet { .. }
        | Datum::MacAddr(_)
        | Datum::Bit { .. }
        | Datum::Geometry { .. }
        | Datum::Null
        | Datum::Array(_)
        | Datum::Citext(_)
        | Datum::Ltree(_)
        | Datum::Hstore(_)
        | Datum::TsVector(_)
        | Datum::TsQuery(_)
        // A regtype joins them: `column_type` refuses the column, so no fragment is built over
        // one, and this vocabulary has no tag for a value whose printed form is a name.
        | Datum::RegType { .. }
        | Datum::Range { .. } => Value::Null,
        Datum::Int8(int) => Value::Int8(*int),
        Datum::Int4(int) => Value::Int4(*int),
        Datum::Int2(int) => Value::Int2(*int),
        Datum::Real(float) => Value::Real(*float),
        Datum::Text(text) => Value::Text(text.clone()),
        Datum::Bool(flag) => Value::Bool(*flag),
        Datum::Bytea(bytes) => Value::Bytea(bytes.clone()),
        Datum::TimestampTz(ts) => Value::TimestampTz(*ts),
        Datum::Timestamp(ts) => Value::Timestamp(*ts),
        Datum::Double(double) => Value::Double(*double),
        Datum::Date(day) => Value::Date(*day),
        Datum::Numeric(value) => Value::Numeric(crate::value::numeric::to_text(value)),
        Datum::Time(micros) => Value::Time(*micros),
        Datum::Uuid(bytes) => Value::Uuid(*bytes),
        Datum::Oid(v) => Value::Oid(*v),
        Datum::Interval {
            months,
            days,
            micros,
        } => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&months.to_le_bytes());
            bytes[4..8].copy_from_slice(&days.to_le_bytes());
            bytes[8..].copy_from_slice(&micros.to_le_bytes());
            Value::Interval(bytes)
        }
    }
}

/// A wire value as a row-side datum. The other direction of the same total match.
fn value_to_datum(value: &WireValue) -> Datum {
    match value {
        WireValue::Null => Datum::Null,
        WireValue::Int8(int) => Datum::Int8(*int),
        WireValue::Int4(int) => Datum::Int4(*int),
        WireValue::Date(day) => Datum::Date(*day),
        WireValue::Time(micros) => Datum::Time(*micros),
        WireValue::Uuid(bytes) => Datum::Uuid(*bytes),
        WireValue::Oid(v) => Datum::Oid(*v),
        WireValue::Interval(bytes) => Datum::Interval {
            months: i32::from_le_bytes(bytes[..4].try_into().unwrap_or([0; 4])),
            days: i32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            micros: i64::from_le_bytes(bytes[8..].try_into().unwrap_or([0; 8])),
        },
        // Carried across the wire as its **text**, which is lossless for this type: the scale is
        // in the digits, so the string a fragment sends reads back as the value it was.
        WireValue::Numeric(text) => {
            Datum::from_text(crate::value::ColumnType::Numeric, text).unwrap_or(Datum::Null)
        }
        WireValue::Int2(int) => Datum::Int2(*int),
        WireValue::Real(float) => Datum::Real(*float),
        WireValue::Text(text) => Datum::Text(text.clone()),
        WireValue::Bool(flag) => Datum::Bool(*flag),
        WireValue::Bytea(bytes) => Datum::Bytea(bytes.clone()),
        WireValue::TimestampTz(ts) => Datum::TimestampTz(*ts),
        WireValue::Timestamp(ts) => Datum::Timestamp(*ts),
        WireValue::Double(double) => Datum::Double(*double),
    }
}

/// The position an expression names, or `None` for anything that is not a bare column.
fn ordinal(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Ordinal { at, .. } => Some(*at),
        _ => None,
    }
}

/// Every column position an expression names.
///
/// Its own walk rather than a method on [`Expr`]: the expression type is the type lane's on
/// `main`, and a walk this module owns is one hunk fewer to resolve at rebase time. It is total
/// over the variants that have children, which is what makes a new one a compile error here
/// rather than a column quietly left out of a projection.
fn collect_columns(expr: &Expr, into: &mut Vec<usize>) {
    match expr {
        Expr::Ordinal { at, .. } => into.push(*at),
        Expr::Array { elements, .. } => {
            for element in elements {
                collect_columns(element, into);
            }
        }
        Expr::Binary { left, right, .. } | Expr::Arithmetic { left, right, .. } => {
            collect_columns(left, into);
            collect_columns(right, into);
        }
        Expr::Not(inner)
        | Expr::Negate(inner)
        | Expr::ToText { operand: inner, .. }
        | Expr::Scalar { operand: inner, .. } => collect_columns(inner, into),
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::RegexMatch {
            operand, pattern, ..
        } => {
            collect_columns(operand, into);
            collect_columns(pattern, into);
        }
        Expr::IsNull { operand, .. } => collect_columns(operand, into),
        Expr::InList { operand, list, .. } => {
            collect_columns(operand, into);
            for item in list {
                collect_columns(item, into);
            }
        }
        Expr::SetFunc(call) => {
            for arg in &call.args {
                collect_columns(arg, into);
            }
        }
        Expr::Coalesce(args) => {
            for arg in args {
                collect_columns(arg, into);
            }
        }
        Expr::Case {
            branches,
            otherwise,
        } => {
            for branch in branches {
                collect_columns(&branch.when, into);
                collect_columns(&branch.then, into);
            }
            if let Some(otherwise) = otherwise {
                collect_columns(otherwise, into);
            }
        }
        Expr::AnyArray { operand, array } => {
            collect_columns(operand, into);
            collect_columns(array, into);
        }
        Expr::Subscript { operand, index, .. } => {
            collect_columns(operand, into);
            collect_columns(index, into);
        }
        Expr::Aggregate(call) => {
            for arg in &call.args {
                collect_columns(arg, into);
            }
        }
        Expr::CatalogFunc(call) => {
            for arg in &call.args {
                collect_columns(arg, into);
            }
        }
        // Only the operand: everything inside the sub-select is resolved against the sub-select's
        // own row, so a position in it is a position in a different row entirely.
        Expr::Subquery(sub) => {
            if let Some(operand) = &sub.operand {
                collect_columns(operand, into);
            }
        }
        Expr::Literal(_)
        | Expr::Uuid(_)
        | Expr::Parameter(_)
        | Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentUser
        | Expr::CurrentSetting { .. }
        // Folded to a literal before a fragment is ever built, and its arguments are constants by
        // then, so it names no column either.
        | Expr::Advisory { .. }
        | Expr::Column { .. }
        // A position in a row **outside** this plan, so it names no column of the one being read.
        | Expr::Outer { .. }
        | Expr::Default
        | Expr::Sequence(_) => {}
    }
}

/// The plan's `Aggregate`, if it has one.
fn find_aggregate(node: &Node) -> Option<&Node> {
    match node {
        Node::Aggregate { .. } => Some(node),
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::Distinct { input } => find_aggregate(input),
        _ => None,
    }
}

/// How many columnar replicas the table's catalog record asks for. A read that cannot say answers
/// zero, which routes to rows — the safe direction.
fn replicas(txn: &dyn Txn, tenant: u64, table_id: u64) -> u8 {
    crate::catalog::table_columnar_replicas(txn, tenant, table_id)
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Runs every columnar node in a plan, and puts the row plan back where one refused.
///
/// **Before the cursor opens, and that is structural**: a `Cursor` works inside one transaction
/// against one store and has no way to make a network call, which is why it refuses a `Columnar`
/// it finds unresolved rather than answering an empty result. Doing it here also means the
/// fallback runs *in the same transaction, at the same snapshot* — the whole of why a routing
/// decision cannot change an answer.
pub(super) fn resolve(
    node: &mut Node,
    txn: &dyn Txn,
    tenant: u64,
    source: &dyn FragmentSource,
    ts: u64,
) {
    if let Node::Columnar(columnar) = node {
        // **The key set, read here and not while planning.** Planning does no I/O; this runs in
        // the same transaction at the same snapshot as both the fragment and the fallback, which
        // is what makes absorbing a join unable to change an answer
        // (`docs/plans/phase-16-mpp.md` §J2).
        if let Err(why) = push_the_semi_join_down(columnar, txn, tenant) {
            columnar.decision = Decision::rows(Reason::Refused(why));
            columnar.run = Some(Run {
                asked: 0,
                answered: 0,
                stats: ScanStats::default(),
                rows: None,
            });
            return;
        }
        let run = evaluate(columnar, source, ts);
        columnar.run = Some(run);
        // **The node stays even when it fell back.** Replacing it with its own fallback would
        // erase the only record that a routing decision was made and then reversed — which is
        // exactly the sentence `EXPLAIN` exists to be able to write, and the one an operator asks
        // for when a query "was fast yesterday". The cursor reads `run.rows`: rows when the
        // fragments answered, and the fallback subtree when they did not.
        return;
    }
    match node {
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::Distinct { input } => resolve(input, txn, tenant, source, ts),
        _ => {}
    }
}

/// Reads the inner key set and folds it into the fragment's filter as an `Expr::In`.
///
/// Does nothing for a fragment that absorbed no join. A refusal here is a *reason*, not an error:
/// the caller answers it with the row plan the node already carries, at this same snapshot.
fn push_the_semi_join_down(
    columnar: &mut Columnar,
    txn: &dyn Txn,
    tenant: u64,
) -> Result<(), &'static str> {
    let Some(semi) = columnar.semi_join.as_ref() else {
        return Ok(());
    };
    let mut cursor = crate::exec::cursor::Cursor::open(txn, tenant, &[], &semi.keys)
        .map_err(|_| "the join's inner side could not be read")?;
    let mut values: Vec<esker_columnar::Value> = Vec::new();
    while let Some(row) = cursor.next().map_err(|_| "the join's inner side failed")? {
        // **A NULL key matches nothing**, in the join and in the membership test alike, so it is
        // dropped rather than carried — and `Expr::In` refuses a list holding one (ADR 0074).
        let Some(datum) = row.first() else { continue };
        if matches!(datum, Datum::Null) {
            continue;
        }
        // `datum_to_value` answers `Null` for a vocabulary it cannot carry, and a key silently
        // turned into a NULL would be a membership test that quietly matched the wrong rows.
        let value = datum_to_value(datum);
        if matches!(value, esker_columnar::Value::Null) {
            return Err("a join key of a type no fragment carries");
        }
        values.push(value);
        if values.len() > esker_columnar::fragment::MAX_IN_VALUES {
            return Err("a join whose inner side has more keys than a fragment carries");
        }
    }
    // Strictly ascending in `pg_cmp` order, which is what the decoder requires and what gives the
    // evaluator its binary search (ADR 0074 Decision 2).
    values.sort_by(esker_columnar::Value::pg_cmp);
    values.dedup_by(|left, right| left.pg_cmp(right) == std::cmp::Ordering::Equal);
    if values.is_empty() {
        // An inner side with no rows means the join matches nothing. `Expr::In` refuses an empty
        // list, so this is expressed as a predicate that is false for every row instead.
        columnar.fragment.filter = Some(esker_columnar::Expr::Literal(
            esker_columnar::Value::Bool(false),
        ));
        return Ok(());
    }
    let membership = esker_columnar::Expr::In {
        operand: Box::new(esker_columnar::Expr::Column(semi.outer_slot)),
        values,
    };
    columnar.fragment.filter = Some(match columnar.fragment.filter.take() {
        Some(existing) => esker_columnar::Expr::And(Box::new(existing), Box::new(membership)),
        None => membership,
    });
    Ok(())
}

/// Asks every region for its fragment and finishes what comes back.
///
/// Returns the rows, or `None` when any region refused — in which case the caller runs
/// [`Columnar::fallback`] against the same transaction, at the same snapshot, and the client sees
/// the answer it always would have.
pub(super) fn evaluate(columnar: &mut Columnar, source: &dyn FragmentSource, ts: u64) -> Run {
    let bytes = esker_columnar::fragment::encode(&columnar.fragment);
    // **The shards the plan was built against, not fresh ones.** They carry the epochs the
    // planner saw, so a region that has split since is refused by the store rather than answered
    // about the half that is left — which is what makes "one fragment per region" cover the whole
    // table or nothing.
    let shards = columnar.shards.clone();

    let mut stats = ScanStats::default();
    let mut merged: BTreeMap<GroupKey, Vec<Partial>> = BTreeMap::new();
    let mut answered = 0;
    for shard in &shards {
        // `min_apply_index` is zero and that is the whole of what a SQL node can honestly say: the
        // learner's `ReadIndex` round is what makes the answer fresh (ADR 0022 Decision 4, and
        // `docs/plans/phase-10-routing.md` §2).
        let Ok(answer) = source.evaluate(shard, &bytes, ts, 0) else {
            return refused(columnar, shards.len(), "a region could not be reached");
        };
        let (result, cost) = match answer {
            Answer::Answered { result, stats } => (result, stats),
            Answer::Refused { reason, .. } => {
                return refused(columnar, shards.len(), refusal_text(reason));
            }
        };
        add(&mut stats, cost);
        answered += 1;
        let Ok(body) = esker_proto::fragment::result::decode(&result) else {
            return refused(columnar, shards.len(), "an answer this node cannot decode");
        };
        let Body::Groups { groups, .. } = body else {
            return refused(columnar, shards.len(), "an aggregate answered as rows");
        };
        for group in groups {
            let key = GroupKey(group.key.iter().map(value_to_datum).collect());
            match merged.entry(key) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(group.partials);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if let Err(why) = combine(slot.get_mut(), &group.partials) {
                        return refused(columnar, shards.len(), why);
                    }
                }
            }
        }
    }

    Run {
        asked: shards.len(),
        answered,
        stats,
        rows: Some(finish(columnar, merged)),
    }
}

/// A run that fell back, with the reason `EXPLAIN` will print.
fn refused(columnar: &mut Columnar, asked: usize, why: &'static str) -> Run {
    columnar.decision = Decision::rows(Reason::Refused(why));
    Run {
        asked,
        answered: 0,
        stats: ScanStats::default(),
        rows: None,
    }
}

fn refusal_text(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::Unsupported => "this build cannot evaluate the fragment",
        RefusalReason::TooFarBehind => "too far behind",
        RefusalReason::NotColumnar => "no columnar copy",
    }
}

fn add(into: &mut ScanStats, cost: ScanStats) {
    into.stripes_considered += cost.stripes_considered;
    into.stripes_read += cost.stripes_read;
    into.chunks_decoded += cost.chunks_decoded;
    into.rows_scanned += cost.rows_scanned;
    into.rows_matched += cost.rows_matched;
}

/// Folds one region's partials into the group's running ones.
///
/// **Per aggregate, by kind**, and a mismatch in kind or in length leaves the running value alone:
/// two regions answering different shapes for the same fragment is a protocol failure, and the
/// safe reading of it is not to mix them. Every fragment sent is the same bytes, so it cannot
/// happen without one of them being a different build — which is what refusal exists for.
fn combine(running: &mut [Partial], arriving: &[Partial]) -> Result<(), &'static str> {
    for (running, arriving) in running.iter_mut().zip(arriving) {
        *running = match (&*running, arriving) {
            (Partial::Count(a), Partial::Count(b)) => Partial::Count(
                a.checked_add(*b)
                    .ok_or("a count of more rows than a u64 holds")?,
            ),
            (Partial::Sum(a), Partial::Sum(b)) => Partial::Sum(sum(a.as_ref(), b.as_ref())?),
            (Partial::Min(a), Partial::Min(b)) => {
                Partial::Min(extreme(a.as_ref(), b.as_ref(), std::cmp::Ordering::Less))
            }
            (Partial::Max(a), Partial::Max(b)) => {
                Partial::Max(extreme(a.as_ref(), b.as_ref(), std::cmp::Ordering::Greater))
            }
            // Two regions answering different *kinds* for one aggregate. Every region is sent the
            // same fragment bytes, so this needs two different builds — which is what refusal
            // exists for, and is why it is not quietly resolved in favour of either side.
            (running, arriving) => {
                let _ = (running, arriving);
                return Err("two regions answered different aggregates for one fragment");
            }
        };
    }
    Ok(())
}

/// Two partial sums. **NULL is "no rows", not zero**, which is why this is not an addition with a
/// zero identity: `sum` over nothing is NULL on a real server, and a region that matched nothing
/// must not turn another region's sum into a different number.
fn sum(
    left: Option<&WireValue>,
    right: Option<&WireValue>,
) -> Result<Option<WireValue>, &'static str> {
    Ok(match (left, right) {
        // Absence is *no rows contributed*, not zero, on either side.
        (None, right) => right.cloned(),
        (left, None) => left.cloned(),
        // **Overflow is an error, not a wrap**, and that is `esker_columnar`'s own rule one level
        // down (`scan::group::add`): PostgreSQL's `sum(bigint)` is `numeric` and cannot overflow,
        // this node has no `numeric`, and a wrong total is worse than a missing one. A fold that
        // wrapped where the evaluator refuses would make the two levels of one aggregate disagree
        // about the same arithmetic.
        (Some(WireValue::Int8(a)), Some(WireValue::Int8(b))) => Some(WireValue::Int8(
            a.checked_add(*b).ok_or("a bigint sum that does not fit")?,
        )),
        (Some(WireValue::Double(a)), Some(WireValue::Double(b))) => Some(WireValue::Double(a + b)),
        // Int8 and Double are the only sums that exist: `esker_columnar::scan::group::add` refuses
        // every other pair, so a partial of another type is a peer that does not agree with this
        // build about what a sum is.
        _ => return Err("a sum of a type this build cannot add"),
    })
}

/// The lesser or greater of two partial extremes, in `pg_cmp` order — this system's ordering, the
/// one the fragment's own evaluator used, and the reason `NaN` is the maximum of a column that
/// holds one.
fn extreme(
    left: Option<&WireValue>,
    right: Option<&WireValue>,
    want: std::cmp::Ordering,
) -> Option<WireValue> {
    match (left, right) {
        (None, right) => right.cloned(),
        (left, None) => left.cloned(),
        (Some(left), Some(right)) => {
            let (a, b) = (value_to_datum(left), value_to_datum(right));
            Some(if a.pg_cmp(&b) == want {
                left.clone()
            } else {
                right.clone()
            })
        }
    }
}

/// The finished rows: the grouping keys followed by the aggregate values, in the order
/// [`Columnar::outputs`] names them.
///
/// In key order, which is what a `BTreeMap` gives — and what makes the answer independent of which
/// region replied first. Order among groups is not guaranteed by SQL, so this is determinism for
/// the differential rather than a promise to a client.
fn finish(columnar: &Columnar, merged: BTreeMap<GroupKey, Vec<Partial>>) -> Vec<Vec<Datum>> {
    if merged.is_empty() && !columnar.grouped {
        // **The empty-input rule, and it is two rules.** An ungrouped aggregate over no rows is
        // *one* row — `count` zero, everything else NULL — and a grouped one is no rows at all.
        // Measured both ways (`crate::plan::Node::Aggregate::grouped`), and a fragment that
        // matched nothing returns no groups either way, so the distinction has to be made here.
        return vec![
            columnar
                .outputs
                .iter()
                .map(|output| match output {
                    // `count` is zero and every other aggregate is NULL — PostgreSQL's rule, and
                    // the reason `sum` over nothing is not zero. A grouping key cannot be reached
                    // here (there are no groups), and NULL is the honest value if it ever were.
                    Output::Aggregate(Finish::Count(_)) => Datum::Int8(0),
                    Output::Key(_) | Output::Aggregate(_) => Datum::Null,
                })
                .collect(),
        ];
    }
    merged
        .into_iter()
        .map(|(key, partials)| {
            columnar
                .outputs
                .iter()
                .map(|output| match output {
                    Output::Key(at) => key.0.get(*at).cloned().unwrap_or(Datum::Null),
                    Output::Aggregate(finish) => value_of(*finish, &partials),
                })
                .collect()
        })
        .collect()
}

/// One finished aggregate value.
fn value_of(finish: Finish, partials: &[Partial]) -> Datum {
    let at = |index: usize| partials.get(index);
    match finish {
        Finish::Count(index) => match at(index) {
            Some(Partial::Count(count)) => Datum::Int8(i64::try_from(*count).unwrap_or(i64::MAX)),
            _ => Datum::Null,
        },
        // One arm, because the three partials are read the same way and differ only in how the
        // *regions* folded them: NULL over no rows, which is not zero.
        Finish::Sum(index) | Finish::Min(index) | Finish::Max(index) => extreme_datum(at(index)),
        // `avg` is finished here because there is no partial average that combines. Both operands
        // come from the same group, so a count of zero means the group matched nothing — which a
        // group that exists cannot — and NULL is the honest answer if it ever did.
        Finish::Avg { sum, count } => {
            let total = match at(sum) {
                Some(Partial::Sum(Some(value))) => value_to_datum(value),
                _ => return Datum::Null,
            };
            let rows = match at(count) {
                Some(Partial::Count(count)) if *count > 0 => *count,
                _ => return Datum::Null,
            };
            #[allow(
                clippy::cast_precision_loss,
                reason = "avg is float8 here; `avg(int8)` is numeric on a real server and this \
                          node has no numeric, which ADR 0031 records as a divergence"
            )]
            // Only the two a sum can be. Anything else cannot reach here — `sum` above refuses
            // every other pair — and NULL is the honest answer if it ever did, because a division
            // this function cannot do is not a number it may invent.
            match total {
                Datum::Double(total) => Datum::Double(total / rows as f64),
                Datum::Int8(total) => Datum::Double(total as f64 / rows as f64),
                _ => Datum::Null,
            }
        }
    }
}

/// A `sum`, `min` or `max` partial as a datum: NULL over no rows, which is not zero.
fn extreme_datum(partial: Option<&Partial>) -> Datum {
    match partial {
        Some(Partial::Sum(Some(value)) | Partial::Min(Some(value)) | Partial::Max(Some(value))) => {
            value_to_datum(value)
        }
        _ => Datum::Null,
    }
}
