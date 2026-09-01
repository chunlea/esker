//! Pulling rows through a plan: one call, one row.
//!
//! This is what makes `LIMIT` cheap. A `SELECT * FROM big LIMIT 10` opens a scan, reads one chunk
//! of keys, and stops — it never asks the store for the rest. Materialising the input and slicing
//! it afterwards would give the same answer and read the whole table to do it.
//!
//! Two nodes cannot be lazy, and both say so. [`Node::Sort`] drains its input by definition, since
//! its input's last row can be its output's first; it is bounded at [`SORT_LIMIT`] rows and refuses
//! past that with `53400`, which is an honest answer rather than an unbounded allocation on behalf
//! of a client (`CLAUDE.md`: predictable tail latency, no unbounded stalls). `TODO(post-v1)`: an
//! external sort, which is what lifts the limit rather than raising it.
//!
//! The scan is chunked rather than read whole, because `Txn::scan` returns a `Vec` and a scan of a
//! whole table would be a whole table in memory. Asking for [`SCAN_CHUNK`] keys at a time and
//! restarting after the last one is the same range read in bounded pieces.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::exec::aggregate::{Accumulator, GROUP_LIMIT, GroupKey};
use crate::exec::query::successor;
use crate::plan::{AggregateSpec, BinaryOp, Expr, Node, Probe, SortKey};
use crate::row;
use crate::row::RowSchema;
use crate::value::{ColumnType, Datum};

/// Rows read from the store in one round trip. Shared with everything else that walks a range
/// ([`crate::exec::for_each_page`]).
use crate::exec::SCAN_CHUNK;
use crate::value::PgDatum;

/// The most rows a `Sort` will hold. Past it, `53400` rather than an unbounded allocation.
pub(super) const SORT_LIMIT: usize = 1_000_000;

/// A pull iterator over a plan: one call, one row.
///
/// This is what makes `LIMIT` cheap. A `SELECT * FROM big LIMIT 10` opens a scan, reads one chunk
/// of keys, and stops — it never asks the store for the rest. Materialising the input and slicing
/// it afterwards would give the same answer and read the whole table to do it.
pub(super) struct Cursor<'a> {
    txn: &'a dyn Txn,
    tenant: u64,
    kind: Kind<'a>,
}

enum Kind<'a> {
    /// One row of nothing, then done.
    One(bool),
    /// A key range, read a chunk at a time.
    Scan {
        columns: RowSchema,
        next: Vec<u8>,
        end: Vec<u8>,
        batch: std::vec::IntoIter<(bytes::Bytes, bytes::Bytes)>,
    },
    /// At most one row, already found or not yet looked for.
    Point { node: Node, looked: bool },
    Filter {
        input: Box<Cursor<'a>>,
        predicate: Expr,
    },
    Project {
        input: Box<Cursor<'a>>,
        exprs: Vec<Expr>,
    },
    /// The one node that cannot stream: it drains its input on the first call.
    Sort {
        input: Option<Box<Cursor<'a>>>,
        keys: Vec<SortKey>,
        sorted: std::vec::IntoIter<Vec<Datum>>,
    },
    Limit {
        input: Box<Cursor<'a>>,
        to_skip: usize,
        remaining: Option<usize>,
    },
    /// The other node that cannot stream: it drains its input on the first call, because the last
    /// input row can change the first output row.
    Aggregate {
        input: Option<Box<Cursor<'a>>>,
        /// The [`Node::Aggregate`] itself — keys, aggregates, `HAVING` and the empty-input rule —
        /// kept whole rather than unpacked into four fields that would then have to be kept in
        /// step with it.
        spec: Node,
        groups: std::vec::IntoIter<Vec<Datum>>,
    },
    /// `SELECT DISTINCT`: streams, keeping the first row of each distinct value.
    Distinct {
        input: Box<Cursor<'a>>,
        seen: BTreeSet<GroupKey>,
    },
    /// A nested-loop join. The outer side streams; the inner side is either one probe per outer
    /// row (at most one row back) or the whole inner table, read once and paired with each.
    NestedLoop {
        outer: Box<Cursor<'a>>,
        left_join: bool,
        inner_table_id: u64,
        inner_columns: RowSchema,
        probe: Probe,
        residual: Option<Expr>,
        /// The outer row being matched, and how far through the materialised inner side it is.
        current: Option<(Vec<Datum>, usize)>,
        /// Whether the outer row in `current` has produced a pair yet.
        ///
        /// A left join's whole question, and it cannot be answered until the inner side is
        /// exhausted: an outer row that matched nothing is emitted with every inner column NULL,
        /// and one that matched is not emitted again.
        matched: bool,
        /// The inner table, read once, for [`Probe::Materialize`] only.
        materialized: Vec<Vec<Datum>>,
    },
}

impl<'a> Cursor<'a> {
    /// Opens a cursor over a plan.
    pub(super) fn open(txn: &'a dyn Txn, tenant: u64, node: &Node) -> Result<Self> {
        let kind = match node {
            Node::OneRow => Kind::One(false),
            Node::SeqScan {
                columns,
                start,
                end,
                ..
            } => Kind::Scan {
                columns: columns.clone(),
                next: start.clone(),
                end: end.clone(),
                batch: Vec::new().into_iter(),
            },
            Node::PointGet { .. } | Node::IndexLookup { .. } => Kind::Point {
                node: node.clone(),
                looked: false,
            },
            Node::NestedLoop {
                outer,
                left_join,
                inner_table_id,
                inner_columns,
                probe,
                residual,
                ..
            } => {
                // A materialised inner side is read once, here, rather than re-scanned per outer
                // row. It is bounded by the same limit a sort is, and for the same reason: an
                // unbounded buffer on behalf of a client is a stall nobody asked for.
                let materialized = if matches!(probe, Probe::Materialize) {
                    let (start, end) = row::table_row_range(tenant, *inner_table_id);
                    let mut rows = Vec::new();
                    let mut scan = Cursor {
                        txn,
                        tenant,
                        kind: Kind::Scan {
                            columns: inner_columns.clone(),
                            next: start,
                            end,
                            batch: Vec::new().into_iter(),
                        },
                    };
                    while let Some(row) = scan.next()? {
                        if rows.len() == SORT_LIMIT {
                            return Err(SqlError::ConfigurationLimitExceeded(format!(
                                "a join whose inner side is more than {SORT_LIMIT} rows needs \
                                 more memory than this server will use for one query"
                            )));
                        }
                        rows.push(row);
                    }
                    rows
                } else {
                    Vec::new()
                };
                Kind::NestedLoop {
                    outer: Box::new(Cursor::open(txn, tenant, outer)?),
                    left_join: *left_join,
                    inner_table_id: *inner_table_id,
                    inner_columns: inner_columns.clone(),
                    probe: probe.clone(),
                    residual: residual.clone(),
                    current: None,
                    matched: false,
                    materialized,
                }
            }
            Node::Filter { input, predicate } => Kind::Filter {
                input: Box::new(Cursor::open(txn, tenant, input)?),
                predicate: predicate.clone(),
            },
            Node::Project { input, exprs } => Kind::Project {
                input: Box::new(Cursor::open(txn, tenant, input)?),
                exprs: exprs.clone(),
            },
            Node::Sort { input, keys } => Kind::Sort {
                input: Some(Box::new(Cursor::open(txn, tenant, input)?)),
                keys: keys.clone(),
                sorted: Vec::new().into_iter(),
            },
            Node::Aggregate { input, .. } => Kind::Aggregate {
                input: Some(Box::new(Cursor::open(txn, tenant, input)?)),
                spec: node.clone(),
                groups: Vec::new().into_iter(),
            },
            Node::Distinct { input } => Kind::Distinct {
                input: Box::new(Cursor::open(txn, tenant, input)?),
                seen: BTreeSet::new(),
            },
            Node::Limit {
                input,
                offset,
                limit,
            } => Kind::Limit {
                input: Box::new(Cursor::open(txn, tenant, input)?),
                to_skip: *offset,
                remaining: *limit,
            },
        };
        Ok(Cursor { txn, tenant, kind })
    }

    /// The next row, or `None` when there are no more.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per node kind; splitting it would hide the pipeline rather than clarify it"
    )]
    pub(super) fn next(&mut self) -> Result<Option<Vec<Datum>>> {
        match &mut self.kind {
            Kind::One(used) => Ok(if std::mem::replace(used, true) {
                None
            } else {
                Some(Vec::new())
            }),

            Kind::Scan {
                columns,
                next,
                end,
                batch,
            } => {
                loop {
                    if let Some((key, value)) = batch.next() {
                        *next = successor(&key);
                        return Ok(Some(row::decode_row(columns, &value)?));
                    }
                    // An **empty** chunk ends the range, not a short one. A short chunk is not
                    // evidence of anything: the store may cap a scan below what was asked
                    // (`esker_client::Router::bounded_limit` does, at a ceiling the operator
                    // configures), and this used to stop on one -- so a `max_scan_limit` under
                    // `SCAN_CHUNK` would have made every `SELECT` return a prefix of its rows and
                    // say nothing. The price is one extra round trip per scan.
                    let read = self.txn.scan(next, end, SCAN_CHUNK)?;
                    if read.is_empty() {
                        return Ok(None);
                    }
                    *batch = read.into_iter();
                }
            }

            Kind::Point { node, looked } => {
                if std::mem::replace(looked, true) {
                    return Ok(None);
                }
                point(self.txn, self.tenant, node)
            }

            Kind::NestedLoop {
                outer,
                left_join,
                inner_table_id,
                inner_columns,
                probe,
                residual,
                current,
                matched,
                materialized,
            } => loop {
                let Some((row, position)) = current else {
                    let Some(next) = outer.next()? else {
                        return Ok(None);
                    };
                    *current = Some((next, 0));
                    *matched = false;
                    continue;
                };

                match probe {
                    // At most one inner row per outer row, so the probe is taken once and the
                    // outer row is done with either way. This is the whole point of the node:
                    // one key read instead of a pass over the inner table.
                    Probe::PrimaryKey { .. } | Probe::UniqueIndex { .. } => {
                        if *position > 0 {
                            *current = None;
                            continue;
                        }
                        *position = 1;
                        // `NULL = anything` is unknown, and unknown keeps no pair. Probing with a
                        // NULL would build a key out of it -- which a primary key cannot hold --
                        // so three-valued logic has to be applied *before* the read, not after.
                        let at = match probe {
                            Probe::PrimaryKey { outer } | Probe::UniqueIndex { outer, .. } => {
                                *outer
                            }
                            Probe::Materialize => unreachable!("handled below"),
                        };
                        if matches!(row[at], Datum::Null) {
                            // A NULL key matches nothing — but a left join still keeps the row.
                            let unmatched = left_extend(*left_join, row, inner_columns.len());
                            *current = None;
                            if unmatched.is_some() {
                                return Ok(unmatched);
                            }
                            continue;
                        }
                        let node = probe_node(probe, *inner_table_id, inner_columns, row);
                        if let Some(inner) = point(self.txn, self.tenant, &node)? {
                            let mut joined = row.clone();
                            joined.extend(inner);
                            *current = None;
                            return Ok(Some(joined));
                        }
                        let unmatched = left_extend(*left_join, row, inner_columns.len());
                        *current = None;
                        if unmatched.is_some() {
                            return Ok(unmatched);
                        }
                    }
                    Probe::Materialize => {
                        let Some(inner) = materialized.get(*position) else {
                            // The inner side is exhausted. A left join whose outer row kept no
                            // pair is emitted now, with every inner column NULL — and it is
                            // decided **here**, after the `ON` has been applied to every pair and
                            // before any `WHERE` above this node runs, which is the whole of what
                            // separates the two clauses.
                            let unmatched =
                                left_extend(*left_join && !*matched, row, inner_columns.len());
                            *current = None;
                            if unmatched.is_some() {
                                return Ok(unmatched);
                            }
                            continue;
                        };
                        *position += 1;
                        let mut joined = row.clone();
                        joined.extend(inner.iter().cloned());
                        // NULL is not true here either: a join condition that is unknown keeps
                        // no pair, which is the same rule a `WHERE` follows.
                        let keep = match residual {
                            None => true,
                            Some(condition) => {
                                matches!(evaluate(condition, &joined)?, Datum::Bool(true))
                            }
                        };
                        if keep {
                            *matched = true;
                            return Ok(Some(joined));
                        }
                    }
                }
            },

            Kind::Filter { input, predicate } => {
                while let Some(row) = input.next()? {
                    // NULL is not true. That is the whole of three-valued logic in a `WHERE`: only
                    // a definite true keeps a row, which is why `n = NULL` matches nothing.
                    if matches!(evaluate(predicate, &row)?, Datum::Bool(true)) {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }

            Kind::Project { input, exprs } => match input.next()? {
                None => Ok(None),
                Some(row) => exprs
                    .iter()
                    .map(|expr| evaluate(expr, &row))
                    .collect::<Result<Vec<_>>>()
                    .map(Some),
            },

            Kind::Sort {
                input,
                keys,
                sorted,
            } => {
                if let Some(mut source) = input.take() {
                    let mut rows = Vec::new();
                    while let Some(row) = source.next()? {
                        if rows.len() == SORT_LIMIT {
                            return Err(SqlError::ConfigurationLimitExceeded(format!(
                                "a sort of more than {SORT_LIMIT} rows needs more memory than \
                                 this node will use; add a LIMIT or an index"
                            )));
                        }
                        rows.push(row);
                    }
                    let mut failure = None;
                    rows.sort_by(|left, right| {
                        compare_rows(keys, left, right).unwrap_or_else(|error| {
                            failure.get_or_insert(error);
                            Ordering::Equal
                        })
                    });
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    *sorted = rows.into_iter();
                }
                Ok(sorted.next())
            }

            Kind::Aggregate {
                input,
                spec,
                groups,
            } => {
                if let Some(mut source) = input.take() {
                    let Node::Aggregate {
                        keys,
                        aggregates,
                        having,
                        grouped,
                        ..
                    } = spec
                    else {
                        return Err(SqlError::Internal(
                            "an aggregate cursor over a node that is not one".to_owned(),
                        ));
                    };
                    *groups =
                        fold(&mut source, keys, aggregates, having.as_ref(), *grouped)?.into_iter();
                }
                Ok(groups.next())
            }

            Kind::Distinct { input, seen } => {
                while let Some(row) = input.next()? {
                    // Streams, and still holds every distinct row it has seen -- so it is bounded
                    // the way `Sort` and the group table are, and answers `53400` rather than
                    // allocating without limit on a client's behalf.
                    if seen.len() == GROUP_LIMIT {
                        return Err(SqlError::ConfigurationLimitExceeded(format!(
                            "a SELECT DISTINCT of more than {GROUP_LIMIT} distinct rows needs                              more memory than this node will use; add a WHERE or a LIMIT"
                        )));
                    }
                    // First seen wins, so the output keeps the input's order rather than the set's
                    // -- which for a `DISTINCT` with no `ORDER BY` is primary key order, and is
                    // deterministic where a real server's is not.
                    if seen.insert(GroupKey(row.clone())) {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }

            Kind::Limit {
                input,
                to_skip,
                remaining,
            } => {
                while *to_skip > 0 {
                    if input.next()?.is_none() {
                        return Ok(None);
                    }
                    *to_skip -= 1;
                }
                if *remaining == Some(0) {
                    return Ok(None);
                }
                let row = input.next()?;
                if row.is_some()
                    && let Some(left) = remaining
                {
                    *left -= 1;
                }
                Ok(row)
            }
        }
    }
}

/// The row a left join emits for an outer row that matched nothing: the outer row, and one NULL
/// per column of the inner table.
///
/// `None` for an inner join, which drops it — which is the one difference between the two, stated
/// once so that neither probe path can implement it slightly differently.
fn left_extend(left_join: bool, row: &[Datum], inner_width: usize) -> Option<Vec<Datum>> {
    left_join.then(|| {
        let mut extended = row.to_vec();
        extended.extend(std::iter::repeat_n(Datum::Null, inner_width));
        extended
    })
}

/// Drains an input into groups and folds each one down to a row.
///
/// The output row is the grouping keys followed by the aggregate values, which is the row every
/// expression above this node was rewritten against (`crate::exec::aggregate`).
///
/// **The empty-input rule is two rules.** With no input rows at all, an *ungrouped* aggregate
/// still produces one row — `count` 0 and everything else NULL, because "how many rows are there"
/// has an answer even when there are none — and a *grouped* one produces nothing, because there is
/// no group to describe. Both were measured; getting the first wrong turns `SELECT count(*)` on an
/// empty table into an empty result, which a client reads as a failed query.
fn fold(
    input: &mut Cursor<'_>,
    keys: &[Expr],
    aggregates: &[AggregateSpec],
    having: Option<&Expr>,
    grouped: bool,
) -> Result<Vec<Vec<Datum>>> {
    let mut groups: BTreeMap<GroupKey, Vec<Accumulator>> = BTreeMap::new();
    while let Some(row) = input.next()? {
        let key = GroupKey(
            keys.iter()
                .map(|key| evaluate(key, &row))
                .collect::<Result<Vec<_>>>()?,
        );
        if !groups.contains_key(&key) && groups.len() == GROUP_LIMIT {
            return Err(SqlError::ConfigurationLimitExceeded(format!(
                "a GROUP BY of more than {GROUP_LIMIT} groups needs more memory than this node                  will use; group by fewer columns or add a WHERE"
            )));
        }
        let accumulators = groups
            .entry(key)
            .or_insert_with(|| aggregates.iter().map(Accumulator::new).collect());
        for (accumulator, spec) in accumulators.iter_mut().zip(aggregates) {
            // `count(*)` reads no value at all, which is why it counts a row whose every column
            // is NULL. Handing it a non-NULL placeholder keeps that in one place.
            let value = match &spec.arg {
                None => Datum::Bool(true),
                Some(arg) => evaluate(arg, &row)?,
            };
            accumulator.push(&value)?;
        }
    }

    if groups.is_empty() && !grouped {
        groups.insert(
            GroupKey(Vec::new()),
            aggregates.iter().map(Accumulator::new).collect(),
        );
    }

    let mut rows = Vec::with_capacity(groups.len());
    for (key, accumulators) in groups {
        let mut row = key.0;
        row.extend(accumulators.iter().map(Accumulator::finish));
        // `HAVING` filters groups, including the one implicit group of an ungrouped aggregate:
        // `SELECT count(*) FROM t HAVING count(*) > 99` returns **no rows** where the same query
        // without the clause returns one row of zero. Measured, and not a shape anybody guesses.
        let keep = match having {
            None => true,
            Some(having) => matches!(evaluate(having, &row)?, Datum::Bool(true)),
        };
        if keep {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// A point read or an index lookup: at most one row, and the store asked at most twice.
/// The single-row access the probe describes, built for one outer row.
///
/// A `Node` rather than a bespoke read, so that a join's inner side and a `WHERE`'s access path
/// go through exactly the same code — there is one implementation of "a point read" and one of
/// "a unique index lookup", and a join cannot drift away from what a `WHERE` does.
fn probe_node(probe: &Probe, table_id: u64, columns: &RowSchema, outer: &[Datum]) -> Node {
    match probe {
        Probe::PrimaryKey { outer: at } => Node::PointGet {
            table_id,
            columns: columns.clone(),
            key: vec![outer[*at].clone()],
        },
        Probe::UniqueIndex {
            index_id,
            index_name,
            outer: at,
            primary_key_types,
        } => Node::IndexLookup {
            table_id,
            columns: columns.clone(),
            index_id: *index_id,
            index_name: index_name.clone(),
            key: vec![outer[*at].clone()],
            primary_key_types: primary_key_types.clone(),
        },
        // Never built: the materialised path does not probe.
        Probe::Materialize => Node::OneRow,
    }
}

fn point(txn: &dyn Txn, tenant: u64, node: &Node) -> Result<Option<Vec<Datum>>> {
    match node {
        Node::PointGet {
            table_id,
            columns,
            key,
        } => {
            let key = row::row_key(tenant, *table_id, key)?;
            txn.get(&key)?
                .map(|value| row::decode_row(columns, &value))
                .transpose()
                .map_err(SqlError::from)
        }
        Node::IndexLookup {
            table_id,
            columns,
            index_id,
            key,
            primary_key_types,
            ..
        } => {
            let index_key = row::index_key(tenant, *table_id, *index_id, key, None)?;
            let Some(entry) = txn.get(&index_key)? else {
                return Ok(None);
            };
            let primary_key = row::decode_row(primary_key_types, &entry)?;
            let key = row::row_key(tenant, *table_id, &primary_key)?;
            match txn.get(&key)? {
                Some(value) => row::decode_row(columns, &value)
                    .map(Some)
                    .map_err(SqlError::from),
                // An index entry pointing at a row that is not there is corruption, not a miss:
                // the entry and the row are written by one transaction.
                None => Err(SqlError::DataCorrupted(format!(
                    "an entry in index {index_id} points at a row that is not there"
                ))),
            }
        }
        other => Err(SqlError::Internal(format!(
            "{other:?} is not a point access path"
        ))),
    }
}

fn compare_rows(keys: &[SortKey], left: &[Datum], right: &[Datum]) -> Result<Ordering> {
    for key in keys {
        let (a, b) = (evaluate(&key.expr, left)?, evaluate(&key.expr, right)?);
        let ordering = match (matches!(a, Datum::Null), matches!(b, Datum::Null)) {
            (true, true) => Ordering::Equal,
            // NULL is the largest value, and `NULLS FIRST` is what moves it. PostgreSQL's default
            // is last for ascending and first for descending, which is not two rules -- it is this
            // one rule with `DESC` reversing it like everything else.
            (true, false) => nulls(key.nulls_first, Ordering::Less, Ordering::Greater),
            (false, true) => nulls(key.nulls_first, Ordering::Greater, Ordering::Less),
            (false, false) => {
                let ordering = a.pg_cmp(&b);
                if key.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

fn nulls(first: bool, when_first: Ordering, otherwise: Ordering) -> Ordering {
    if first { when_first } else { otherwise }
}

/// Evaluates an expression against a row.
pub(super) fn evaluate(expr: &Expr, row: &[Datum]) -> Result<Datum> {
    use crate::plan::Literal;
    Ok(match expr {
        Expr::Ordinal { at, .. } => row.get(*at).cloned().unwrap_or(Datum::Null),
        Expr::Literal(Literal::Null) => Datum::Null,
        Expr::Literal(Literal::Bool(value)) => Datum::Bool(*value),
        Expr::Literal(Literal::Integer(value)) => Datum::Int8(*value),
        Expr::Literal(Literal::Decimal(digits)) => Datum::from_text(ColumnType::Double, digits)?,
        Expr::Literal(Literal::String(text)) => Datum::Text(text.clone()),
        Expr::Literal(Literal::Typed(value)) => (**value).clone(),
        Expr::Column { name, .. } => {
            return Err(SqlError::Internal(format!(
                "column \"{name}\" reached the executor unresolved"
            )));
        }
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),
        // A value of a group, not of a row. The planner replaces every one of these with an
        // `Ordinal` into the aggregated row, so one arriving here is a planner bug and says so
        // rather than returning a number nobody can check.
        Expr::Aggregate(_) => {
            return Err(SqlError::Internal(
                "an aggregate reached the row evaluator".to_owned(),
            ));
        }
        // Both are resolved before a plan is built -- a `DEFAULT` by the statement that knows
        // which column it is for, a sequence call by the executor, which runs it once rather than
        // once per row. Either one here is a planner bug.
        Expr::Default => {
            return Err(SqlError::Internal(
                "a DEFAULT reached the row evaluator".to_owned(),
            ));
        }
        Expr::Sequence(_) => {
            return Err(SqlError::Internal(
                "a sequence function reached the row evaluator".to_owned(),
            ));
        }

        Expr::IsNull { operand, negated } => {
            let value = evaluate(operand, row)?;
            Datum::Bool(matches!(value, Datum::Null) != *negated)
        }

        Expr::Not(operand) => match evaluate(operand, row)? {
            Datum::Bool(value) => Datum::Bool(!value),
            // NOT of unknown is unknown.
            Datum::Null => Datum::Null,
            other => {
                return Err(SqlError::DatatypeMismatch(format!(
                    "argument of NOT must be type boolean, not {other:?}"
                )));
            }
        },

        Expr::InList {
            operand,
            list,
            negated,
        } => in_list(operand, list, *negated, row)?,

        Expr::Binary { op, left, right } => {
            let (left, right) = (evaluate(left, row)?, evaluate(right, row)?);
            match op {
                // Three-valued AND and OR, and they are not symmetric: a definite `false` makes an
                // AND false whatever the other side is, and a definite `true` makes an OR true.
                BinaryOp::And => match (truth(&left)?, truth(&right)?) {
                    (Some(false), _) | (_, Some(false)) => Datum::Bool(false),
                    (Some(true), Some(true)) => Datum::Bool(true),
                    _ => Datum::Null,
                },
                BinaryOp::Or => match (truth(&left)?, truth(&right)?) {
                    (Some(true), _) | (_, Some(true)) => Datum::Bool(true),
                    (Some(false), Some(false)) => Datum::Bool(false),
                    _ => Datum::Null,
                },
                comparison => {
                    // Any NULL operand makes a comparison unknown. This is why `x = NULL` never
                    // matches and `x IS NULL` exists.
                    if matches!(left, Datum::Null) || matches!(right, Datum::Null) {
                        return Ok(Datum::Null);
                    }
                    let ordering = left.pg_cmp(&right);
                    Datum::Bool(match comparison {
                        BinaryOp::Eq => ordering.is_eq(),
                        BinaryOp::NotEq => !ordering.is_eq(),
                        BinaryOp::Lt => ordering.is_lt(),
                        BinaryOp::LtEq => ordering.is_le(),
                        BinaryOp::Gt => ordering.is_gt(),
                        BinaryOp::GtEq => ordering.is_ge(),
                        BinaryOp::And | BinaryOp::Or => unreachable!("handled above"),
                    })
                }
            }
        }
    })
}

/// `x IN (a, b, …)` — three-valued, and the rule is **not** "a NULL means false":
///
/// * an equal item wins outright, whatever else is in the list — `1 IN (1, NULL)` is true;
/// * with no match, a NULL anywhere (in the list or on the left) makes the answer unknown —
///   `1 IN (2, NULL)` is NULL, so `1 NOT IN (2, NULL)` is NULL too and a `NOT IN` over a list
///   containing NULL matches **nothing at all**;
/// * only a list of definite non-matches is false.
///
/// A NULL item does **not** stop the scan: `1 IN (NULL, 1)` is true. Giving up at the first NULL
/// answers NULL and drops a row the user asked for, which is what this crate did until the corpus
/// was extended with a NULL before the match.
///
/// Measured on 19beta1, `tests/corpus/pg19_in.txt`. A scan rather than a rewrite to `= a OR = b`,
/// so the left-hand side is evaluated once — which also keeps a `nextval` on the left from
/// running per item.
fn in_list(operand: &Expr, list: &[Expr], negated: bool, row: &[Datum]) -> Result<Datum> {
    let operand = evaluate(operand, row)?;
    // A NULL on the left can neither match nor definitely fail to, so nothing in the list can
    // change the answer.
    if matches!(operand, Datum::Null) {
        return Ok(Datum::Null);
    }
    let mut unknown = false;
    let mut matched = false;
    for item in list {
        let item = evaluate(item, row)?;
        if matches!(item, Datum::Null) {
            unknown = true;
            continue;
        }
        if operand.pg_cmp(&item).is_eq() {
            matched = true;
            break;
        }
    }
    Ok(match (matched, unknown) {
        (true, _) => Datum::Bool(!negated),
        (false, true) => Datum::Null,
        (false, false) => Datum::Bool(negated),
    })
}

fn truth(value: &Datum) -> Result<Option<bool>> {
    match value {
        Datum::Bool(value) => Ok(Some(*value)),
        Datum::Null => Ok(None),
        other => Err(SqlError::DatatypeMismatch(format!(
            "argument of AND/OR must be type boolean, not {other:?}"
        ))),
    }
}
