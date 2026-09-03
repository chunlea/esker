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
use crate::plan::{AggregateFunc, AggregateSpec, Expr, Node, routing};
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
    source: Option<&dyn FragmentSource>,
    setting: Setting,
    planned: &mut Planned,
) {
    let decision = match consider(txn, tenant, table, source, setting, planned) {
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
fn consider(
    txn: &dyn Txn,
    tenant: u64,
    table: &TableDef,
    source: Option<&dyn FragmentSource>,
    setting: Setting,
    planned: &Planned,
) -> Routed<Built> {
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

    // **Before any question about the plan's shape.** A table nobody asked for a columnar copy of
    // has no second engine to choose between, and every reason after this one describes a *choice*
    // — which `EXPLAIN` then prints. Deciding it here is what keeps an ordinary table's plan from
    // growing a line about a feature it is not using.
    let replicas = replicas(txn, tenant, table.id);
    if replicas == 0 {
        return Err(Decision::rows(Reason::NotAsked));
    }

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
    let Node::SeqScan {
        table_id, narrowed, ..
    } = scan
    else {
        // A point get or an index lookup, which rule 1 keeps on rows; a join or a catalog view,
        // which no fragment expresses. Both answer `Bounded` only when they really are bounded.
        return Err(Decision::rows(match scan {
            Node::PointGet { .. } | Node::IndexLookup { .. } => Reason::Bounded,
            _ => Reason::NotExpressible("this access path"),
        }));
    };
    if *narrowed {
        return Err(Decision::rows(Reason::Bounded));
    }

    // Every table column the fragment must read, in table order: the grouping keys, the aggregate
    // arguments, and everything the filter names. The projection is the only place a table column
    // index appears in a fragment, so this list is also what decides the ratio.
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

    // One columnar type per projection slot, for the comparison check below.
    let slot_types: Vec<esker_columnar::ColumnType> = columns
        .iter()
        .filter_map(|column| table.columns.get(*column).map(|def| column_type(def.ty)))
        .collect();
    if slot_types.len() != columns.len() {
        return Err(Decision::rows(Reason::NotExpressible(
            "a column this table's record does not describe",
        )));
    }

    let mut fragment = esker_columnar::Fragment::aggregate(
        esker_columnar::TableRef {
            tenant,
            table_id: *table_id,
        },
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
            table_id: *table_id,
            table: planned.table.clone(),
            fragment,
            outputs,
            grouped: *grouped,
            decision,
            shards,
            fallback: Box::new(fallback),
            run: None,
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
        // Not expressible in the fragment language; the filter stays on the row side.
        Expr::Scalar { .. } => return Err(refused("a scalar function")),
        // The fragment language has no conditional, and a `CASE` is the one expression whose
        // branches must **not** all be evaluated — pushing it down as anything else would change
        // which of them raises. Rows, and the row evaluator answers it.
        Expr::Case { .. } => return Err(refused("a CASE expression")),
        // The fragment language has no array. Rows, and the row evaluator answers it.
        Expr::AnyArray { .. } => return Err(refused("= ANY over an array value")),
        Expr::Literal(literal) => ColExpr::Literal(literal_value(literal)?),
        // A catalog function is a function of the catalog, not of the fragment's columns, and the
        // columnar reader has no expression for it. Rows, and the row evaluator answers it.
        Expr::CatalogFunc(_) => return Err(refused("a catalog function")),
        Expr::Not(inner) => ColExpr::Not(Box::new(push_filter(inner, slot, types)?)),
        Expr::IsNull { operand, negated } => ColExpr::IsNull {
            operand: Box::new(push_filter(operand, slot, types)?),
            negated: *negated,
        },
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
        Literal::Null => Value::Null,
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
fn column_type(ty: crate::value::ColumnType) -> esker_columnar::ColumnType {
    use crate::value::ColumnType as Row;
    use esker_columnar::ColumnType as Col;

    match ty {
        Row::Int8 => Col::Int8,
        Row::Time => Col::Time,
        Row::Uuid => Col::Uuid,
        Row::Interval => Col::Interval,
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
    }
}

/// A row-side datum as a columnar value. Total on both sides by construction.
fn datum_to_value(datum: &Datum) -> esker_columnar::Value {
    use esker_columnar::Value;
    match datum {
        Datum::Null => Value::Null,
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
        Expr::Binary { left, right, .. } => {
            collect_columns(left, into);
            collect_columns(right, into);
        }
        Expr::Not(inner)
        | Expr::ToText { operand: inner, .. }
        | Expr::Scalar { operand: inner, .. } => collect_columns(inner, into),
        Expr::IsNull { operand, .. } => collect_columns(operand, into),
        Expr::InList { operand, list, .. } => {
            collect_columns(operand, into);
            for item in list {
                collect_columns(item, into);
            }
        }
        // Every branch's columns, condition and result alike: a projection that left out a column
        // only one unreached branch names would still have to read it, because which branch is
        // reached is a property of the row and not of the plan.
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
        | Expr::Parameter(_)
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
pub(super) fn resolve(node: &mut Node, source: &dyn FragmentSource, ts: u64) {
    if let Node::Columnar(columnar) = node {
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
        | Node::Distinct { input } => resolve(input, source, ts),
        _ => {}
    }
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
