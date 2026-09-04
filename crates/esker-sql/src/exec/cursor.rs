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
    /// The tenant's relations, read at most once and only if something asks.
    ///
    /// `pg_get_indexdef(d.indexrelid)` is a function of the **catalog**, and its argument is a
    /// column, so unlike `'x'::regclass` it cannot be resolved before the plan is built — it
    /// really does answer differently per row. Reading the catalog per row would make a schema
    /// dump quadratic in the number of relations, so the snapshot is taken once, here, by whichever
    /// cursor node first needs it. A cursor whose plan calls none of these functions never builds
    /// one.
    catalog: std::cell::OnceCell<crate::catalog::pg_relations::Relations>,
    kind: Kind<'a>,
}

enum Kind<'a> {
    /// One row of nothing, then done.
    One(bool),
    /// Rows that came from nowhere: a `pg_catalog` relation, computed rather than read.
    Rows(std::vec::IntoIter<Vec<Datum>>),
    /// A key range, read a chunk at a time.
    Scan {
        columns: RowSchema,
        next: Vec<u8>,
        end: Vec<u8>,
        batch: std::vec::IntoIter<(bytes::Bytes, bytes::Bytes)>,
        /// Children still to read, innermost last — a scan of a parent returns their rows too.
        inherited: Vec<crate::catalog::ChildScan>,
        /// How to line the range being read up with the parent's columns, or `None` while the
        /// parent's own range is being read and the rows already are its shape.
        project: Option<Vec<usize>>,
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
        /// Rows one input row expanded into, when the target list holds a **set-returning
        /// function**. Empty for every other projection, which is all but one in a hundred: a
        /// `Vec` that is never filled costs an allocation nobody makes.
        pending: std::vec::IntoIter<Vec<Datum>>,
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
        /// The inner side's call, when it is a set-returning function: its rows depend on the
        /// outer row, so they are recomputed for each one instead of materialised once.
        lateral: Option<Box<crate::plan::TableFunction>>,
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

/// The inner side of a nested loop, read once, or nothing when the probe reads it per outer row.
///
/// A materialised inner side is read here rather than re-scanned per outer row, and it is bounded
/// by the same limit a sort is, for the same reason: an unbounded buffer on behalf of a client is
/// a stall nobody asked for.
///
/// A **catalog view** is materialised too, and where its rows come from is the difference: there
/// is no key range to scan, so they are computed. Always [`Probe::Materialize`] —
/// `exec::query::join_node` will not build any other probe over a relation that has no key.
fn inner_side(
    txn: &dyn Txn,
    tenant: u64,
    inner_table_id: u64,
    inner_view: Option<&crate::plan::CatalogView>,
    inner_plan: Option<&Node>,
    inner_columns: &RowSchema,
    probe: &Probe,
) -> Result<Vec<Vec<Datum>>> {
    if let Some(view) = inner_view {
        return view.rows_of(txn, tenant);
    }
    if !matches!(probe, Probe::Materialize) {
        return Ok(Vec::new());
    }
    let (start, end) = row::table_row_range(tenant, inner_table_id);
    let mut rows = Vec::new();
    // A **derived table**'s rows come from its own plan rather than from a key range, which is why
    // the same `Cursor` is opened over one instead of over a `Scan`. Everything below it is
    // unchanged — the bound and the `53400` past it — because what makes a materialised inner side
    // expensive is the same either way: a whole relation held in memory for one query.
    let mut scan = match inner_plan {
        Some(plan) => Cursor::open(txn, tenant, plan)?,
        None => Cursor {
            txn,
            tenant,
            catalog: std::cell::OnceCell::new(),
            kind: Kind::Scan {
                columns: inner_columns.clone(),
                next: start,
                end,
                batch: Vec::new().into_iter(),
                // A join's inner side is materialised from one table's range; a parent on that
                // side reaches this through its own `SeqScan` above, not through here.
                inherited: Vec::new(),
                project: None,
            },
        },
    };
    while let Some(row) = scan.next()? {
        if rows.len() == SORT_LIMIT {
            return Err(SqlError::ConfigurationLimitExceeded(format!(
                "a join whose inner side is more than {SORT_LIMIT} rows needs more memory than \
                 this server will use for one query"
            )));
        }
        rows.push(row);
    }
    Ok(rows)
}

impl<'a> Cursor<'a> {
    /// Opens a cursor over a plan.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per node kind, the same shape as `next`"
    )]
    pub(super) fn open(txn: &'a dyn Txn, tenant: u64, node: &Node) -> Result<Self> {
        let kind = match node {
            Node::OneRow => Kind::One(false),
            // Computed here, once, rather than page by page: `pg_type` is six rows and `pg_range`
            // is none. If a catalog view ever is not small, this is the line that changes.
            Node::CatalogView { view, .. } => Kind::Rows(view.rows_of(txn, tenant)?.into_iter()),
            // Rows written into the statement, evaluated here for the same reason a catalog view's
            // are: nothing is stored, so there is no key range to seek in and the row count is the
            // length of the list.
            Node::Values { list, .. } => Kind::Rows(super::values::rows(list, txn)?.into_iter()),
            // A set-returning function in `FROM`: its rows are computed here, once, exactly as a
            // catalog view's are — there is no key range to seek in and the row count is the
            // length of one array. Its arguments are evaluated against **no row**, which is what
            // makes an argument that reads a column the refusal below rather than a wrong answer.
            Node::TableFunction { call, .. } => {
                Kind::Rows(super::table_function::rows(call, &[])?.into_iter())
            }
            Node::SeqScan {
                columns,
                start,
                end,
                inherited,
                ..
            } => Kind::Scan {
                columns: columns.clone(),
                next: start.clone(),
                end: end.clone(),
                batch: Vec::new().into_iter(),
                // Reversed, so the walk can `pop` and still read them in the order the parent
                // lists its children.
                inherited: inherited.iter().rev().cloned().collect(),
                project: None,
            },
            Node::PointGet { .. } | Node::IndexLookup { .. } => Kind::Point {
                node: node.clone(),
                looked: false,
            },
            Node::NestedLoop {
                outer,
                left_join,
                inner_table_id,
                inner_view,
                inner_plan,
                inner_columns,
                probe,
                residual,
                ..
            } => {
                let materialized = inner_side(
                    txn,
                    tenant,
                    *inner_table_id,
                    inner_view.as_ref(),
                    inner_plan.as_deref(),
                    inner_columns,
                    probe,
                )?;
                Kind::NestedLoop {
                    // **The inner side is a set-returning function, so it is lateral.** Its rows
                    // are computed from the *outer* row rather than once at open — which is what
                    // `FROM lt l, unnest(l.tags) u` means and what makes `l` visible inside it. A
                    // function with no outer reference recomputes the same rows per outer row,
                    // which is the cross join it is.
                    lateral: match inner_plan.as_deref() {
                        Some(Node::TableFunction { call, .. }) => Some(call.clone()),
                        _ => None,
                    },
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
                pending: Vec::new().into_iter(),
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
            // It computes nothing: the sub-select's plan already produced the rows, and the name
            // and column names this node carries are for `EXPLAIN`. Opening the input directly is
            // what makes that true rather than merely intended.
            Node::Derived { input, .. } => return Cursor::open(txn, tenant, input),
            Node::Distinct { input } => Kind::Distinct {
                input: Box::new(Cursor::open(txn, tenant, input)?),
                seen: BTreeSet::new(),
            },
            // **Resolved before the cursor is opened, never here.** A columnar node's rows come
            // from a network call to a columnar learner, which this type has no way to make and
            // deliberately does not: everything a `Cursor` does happens inside one transaction
            // against one store. `crate::exec::fragment::resolve` walks the plan first and leaves
            // either the rows the fragments produced or the row plan they fell back to, so what
            // reaches here is one of those two. A `Columnar` that did not is a bug in this crate
            // and says so rather than answering an empty result, which is the one thing it must
            // not do — an aggregate with no rows is a *number*, and zero would look like an answer.
            Node::Columnar(columnar) => match &columnar.run {
                // The fragments answered. Their finished rows are the aggregate's output rows,
                // which is what makes the substitution exact.
                Some(run) if run.rows.is_some() => {
                    Kind::Rows(run.rows.clone().unwrap_or_default().into_iter())
                }
                // Something refused, so the rows answer — **in this transaction, at this
                // snapshot**, which is what makes the fallback silent to the client and correct.
                // The node stays in the plan so `EXPLAIN` can still say what was tried.
                Some(_) => return Cursor::open(txn, tenant, &columnar.fallback),
                None => {
                    return Err(SqlError::Internal(
                        "a columnar node reached the cursor without being resolved".to_owned(),
                    ));
                }
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
        Ok(Cursor {
            txn,
            tenant,
            catalog: std::cell::OnceCell::new(),
            kind,
        })
    }

    /// The next row, or `None` when there are no more.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per node kind; splitting it would hide the pipeline rather than clarify it"
    )]
    pub(super) fn next(&mut self) -> Result<Option<Vec<Datum>>> {
        // Copied out before the match borrows `self.kind`, and it is why a correlated subquery can
        // run at all: the evaluator is handed the transaction this cursor is already reading in,
        // so the sub-plan it opens sees the same snapshot as the row it is correlated to.
        let env = Env {
            txn: Some(self.txn),
            tenant: self.tenant,
            catalog: Some(&self.catalog),
        };
        match &mut self.kind {
            Kind::One(used) => Ok(if std::mem::replace(used, true) {
                None
            } else {
                Some(Vec::new())
            }),

            Kind::Rows(rows) => Ok(rows.next()),

            Kind::Scan {
                columns,
                next,
                end,
                batch,
                inherited,
                project,
            } => {
                loop {
                    if let Some((key, value)) = batch.next() {
                        *next = successor(&key);
                        let row = row::decode_row(columns, &value)?;
                        // **A child's row is decoded as the child and answered as the parent.**
                        // The two layouts differ whenever the child has a row id the parent has
                        // not, or a column of its own, so the values are lifted by position from
                        // a map built by name (`catalog::ChildScan`).
                        return Ok(Some(match project {
                            Some(project) => project
                                .iter()
                                .map(|&at| row.get(at).cloned().unwrap_or(Datum::Null))
                                .collect(),
                            None => row,
                        }));
                    }
                    // An **empty** chunk ends the range, not a short one. A short chunk is not
                    // evidence of anything: the store may cap a scan below what was asked
                    // (`esker_client::Router::bounded_limit` does, at a ceiling the operator
                    // configures), and this used to stop on one -- so a `max_scan_limit` under
                    // `SCAN_CHUNK` would have made every `SELECT` return a prefix of its rows and
                    // say nothing. The price is one extra round trip per scan.
                    let read = self.txn.scan(next, end, SCAN_CHUNK)?;
                    if read.is_empty() {
                        // The range is done. A parent moves on to its next child's range and
                        // answers those rows as its own; a table with no children stops here,
                        // which is what every scan did before `INHERITS`.
                        let Some(child) = inherited.pop() else {
                            return Ok(None);
                        };
                        let (start, stop) = row::table_row_range(self.tenant, child.table_id);
                        *columns = child.schema;
                        *next = start;
                        *end = stop;
                        *project = Some(child.project);
                        continue;
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
                lateral,
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
                        // A lateral inner side is computed from this outer row, once, when the
                        // row is first reached. An **empty** result is what makes the outer row
                        // disappear from an inner join and survive a left one, with no rule of
                        // its own: the loop below already does both.
                        if let Some(call) = lateral
                            && *position == 0
                        {
                            *materialized = super::table_function::rows(call, row)?;
                        }
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
                                matches!(evaluate_in(condition, &joined, env)?, Datum::Bool(true))
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
                    if matches!(evaluate_in(predicate, &row, env)?, Datum::Bool(true)) {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }

            // **The projection is where a set-returning call becomes rows.** One input row makes
            // as many output rows as the longest call in the list yields, every other expression
            // repeating and every shorter call padded with NULL — measured, and not a cross join:
            // `generate_series(1,3), generate_series(1,2)` is three rows, not six. A call that
            // yields nothing takes its input row with it.
            Kind::Project {
                input,
                exprs,
                pending,
            } => loop {
                if let Some(row) = pending.next() {
                    return Ok(Some(row));
                }
                let Some(row) = input.next()? else {
                    return Ok(None);
                };
                if !exprs.iter().any(has_set_func) {
                    return exprs
                        .iter()
                        .map(|expr| evaluate_in(expr, &row, env))
                        .collect::<Result<Vec<_>>>()
                        .map(Some);
                }
                *pending = expand(exprs, &row, env)?.into_iter();
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
                        compare_rows(keys, left, right, env).unwrap_or_else(|error| {
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
                    *groups = fold(
                        &mut source,
                        keys,
                        aggregates,
                        having.as_ref(),
                        *grouped,
                        env,
                    )?
                    .into_iter();
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
#[allow(
    clippy::too_many_arguments,
    reason = "one aggregate fold, and every argument is a field of the node it folds"
)]
fn fold(
    input: &mut Cursor<'_>,
    keys: &[Expr],
    aggregates: &[AggregateSpec],
    having: Option<&Expr>,
    grouped: bool,
    env: Env<'_>,
) -> Result<Vec<Vec<Datum>>> {
    let mut groups: BTreeMap<GroupKey, Vec<Accumulator>> = BTreeMap::new();
    while let Some(row) = input.next()? {
        let key = GroupKey(
            keys.iter()
                .map(|key| evaluate_in(key, &row, env))
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
                Some(arg) => evaluate_in(arg, &row, env)?,
            };
            // The aggregate's **own** `ORDER BY`, evaluated against the same input row as its
            // argument: `array_agg(x ORDER BY y)` sorts by a column it does not return, so the key
            // has to travel with the value rather than be recovered from it.
            let mut sort_key = Vec::with_capacity(spec.order_by.len());
            for key in &spec.order_by {
                sort_key.push(evaluate_in(&key.expr, &row, env)?);
            }
            accumulator.push(&value, sort_key)?;
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
            Some(having) => matches!(evaluate_in(having, &row, env)?, Datum::Bool(true)),
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

fn compare_rows(
    keys: &[SortKey],
    left: &[Datum],
    right: &[Datum],
    env: Env<'_>,
) -> Result<Ordering> {
    let mut a = Vec::with_capacity(keys.len());
    let mut b = Vec::with_capacity(keys.len());
    for key in keys {
        a.push(evaluate_in(&key.expr, left, env)?);
        b.push(evaluate_in(&key.expr, right, env)?);
    }
    Ok(compare_values(keys, &a, &b))
}

/// The same ordering over key values that have **already been evaluated**.
///
/// Shared rather than copied, because `ORDER BY` inside an `array_agg` sorts by exactly the rule
/// `ORDER BY` on the query does — including the half nobody remembers, that PostgreSQL's default
/// null placement follows the direction. `array_agg(n ORDER BY n)` is `{10,20,30,NULL}` and
/// `ORDER BY n DESC` is `{NULL,30,20,10}`; measured, and it falls out of this function rather than
/// being asserted twice.
pub(super) fn compare_values(keys: &[SortKey], left: &[Datum], right: &[Datum]) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let (a, b) = (
            left.get(at).unwrap_or(&Datum::Null),
            right.get(at).unwrap_or(&Datum::Null),
        );
        let ordering = match (matches!(a, Datum::Null), matches!(b, Datum::Null)) {
            (true, true) => Ordering::Equal,
            // NULL is the largest value, and `NULLS FIRST` is what moves it. PostgreSQL's default
            // is last for ascending and first for descending, which is not two rules -- it is this
            // one rule with `DESC` reversing it like everything else.
            (true, false) => nulls(key.nulls_first, Ordering::Less, Ordering::Greater),
            (false, true) => nulls(key.nulls_first, Ordering::Greater, Ordering::Less),
            (false, false) => {
                let ordering = a.pg_cmp(b);
                if key.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    Ordering::Equal
}

fn nulls(first: bool, when_first: Ordering, otherwise: Ordering) -> Ordering {
    if first { when_first } else { otherwise }
}

/// Evaluates an expression against a row.
/// What a correlated subquery needs from the executor: a transaction to run in.
///
/// [`Env::none`] is what everything that is not a `Cursor` passes — `crate::exec::dml`'s
/// `RETURNING` and its `UPDATE ... SET`, which evaluate expressions over a row they already have.
/// A correlated subquery cannot appear in either, because this phase does not lower a subquery
/// into a statement that writes, and one arriving there says so rather than answering NULL.
#[derive(Clone, Copy)]
pub(super) struct Env<'a> {
    txn: Option<&'a dyn Txn>,
    tenant: u64,
    /// Where the cursor keeps its catalog snapshot, or `None` for an evaluator that has no cursor
    /// behind it — `RETURNING` and `UPDATE ... SET`, neither of which can hold a catalog function.
    catalog: Option<&'a std::cell::OnceCell<crate::catalog::pg_relations::Relations>>,
}

impl Env<'_> {
    /// No transaction: a caller that evaluates over a row and cannot start a query.
    pub(super) fn none() -> Self {
        Env {
            txn: None,
            tenant: 0,
            catalog: None,
        }
    }

    /// A transaction and nothing else: what a column `DEFAULT` is evaluated in.
    ///
    /// It has no catalog snapshot, which is right for the job — a default is one value out of no
    /// row, and the clock is the only thing outside itself it can reach.
    pub(super) fn in_txn(txn: &dyn Txn) -> Env<'_> {
        Env {
            txn: Some(txn),
            tenant: 0,
            catalog: None,
        }
    }

    /// The tenant's relations, read the first time one is asked for and shared after that.
    fn relations(&self) -> Result<&crate::catalog::pg_relations::Relations> {
        let (Some(cell), Some(txn)) = (self.catalog, self.txn) else {
            return Err(SqlError::Internal(
                "a catalog function reached an evaluator with no transaction to read in".to_owned(),
            ));
        };
        if cell.get().is_none() {
            // `OnceCell::get_or_init` cannot fail, and reading the catalog can, so the read
            // happens outside it. A racing `set` is impossible — a cursor is not shared — and
            // would be harmless anyway: both snapshots are of the same transaction.
            let read = crate::catalog::pg_relations::Relations::read(txn, self.tenant)?;
            let _ = cell.set(read);
        }
        cell.get().ok_or_else(|| {
            SqlError::Internal("a catalog snapshot that was just read is gone".to_owned())
        })
    }
}

/// Evaluates an expression over a row, with no way to run a subquery of its own.
/// The three functions `crate::value::range` answers: `daterange`, `isempty` and `&&`.
///
/// Split out of [`catalog_function`] because they are one family and it is long enough already.
fn range_function(func: crate::plan::CatalogFunc, args: &[Datum]) -> Result<Datum> {
    use crate::plan::CatalogFunc;
    Ok(match func {
        // **`daterange(low, high)`** — a NULL bound is *unbounded*, not NULL, so the call answers a
        // range either way and is never NULL itself. That is the fact that makes
        // `daterange(NULL, NULL)` overlap everything rather than nothing.
        CatalogFunc::DateRange => {
            // A bare `'2026-01-01'` reaches here as text, the way an unknown literal reaches a
            // real server's `daterange(unknown, unknown)` — so it is coerced rather than refused.
            let day = |value: Option<&Datum>| match value {
                Some(Datum::Date(day)) => Ok(Some(*day)),
                Some(Datum::Null) | None => Ok(None),
                Some(Datum::Text(text)) => Ok(Some(crate::value::date::from_text(text, 0)?)),
                Some(other) => Err(SqlError::UndefinedFunctionTypes(format!(
                    "daterange({})",
                    other
                        .column_type()
                        .map_or("unknown", crate::value::PgType::name)
                ))),
            };
            Datum::Text(
                crate::value::range::DateRange::new(day(args.first())?, day(args.get(1))?)
                    .to_text(),
            )
        }
        CatalogFunc::IsEmpty => match range_argument(args.first())? {
            None => Datum::Null,
            Some(range) => Datum::Bool(range.empty),
        },
        // Strict on both sides: a NULL range makes the answer unknown, the way every other
        // operator over a NULL does.
        _ => match (range_argument(args.first())?, range_argument(args.get(1))?) {
            (Some(left), Some(right)) => Datum::Bool(left.overlaps(right)),
            _ => Datum::Null,
        },
    })
}

/// A range argument, or `None` for NULL — and `42883` for a value that is not one.
fn range_argument(value: Option<&Datum>) -> Result<Option<crate::value::range::DateRange>> {
    match value {
        Some(Datum::Null) | None => Ok(None),
        Some(Datum::Text(text)) => match crate::value::range::DateRange::from_text(text) {
            Some(range) => Ok(Some(range)),
            None => Err(SqlError::UndefinedOperator {
                op: "&&",
                left: "text",
                right: "text",
            }),
        },
        Some(other) => Err(SqlError::UndefinedOperator {
            op: "&&",
            left: other
                .column_type()
                .map_or("unknown", crate::value::PgType::name),
            right: "unknown",
        }),
    }
}

/// A `LIKE` operand as text, or `None` for NULL — and `42883` for anything that is not a string.
///
/// **A number has no `LIKE` operator**: `100 LIKE '1%'` is
/// `operator does not exist: integer ~~ unknown` on a real server, not a cast to text. `~~` is
/// `LIKE`'s internal name and is what the message shows.
fn like_text(value: &Datum) -> Result<Option<String>> {
    match value {
        Datum::Null => Ok(None),
        Datum::Text(text) => Ok(Some(text.clone())),
        other => Err(SqlError::UndefinedOperator {
            op: "~~",
            left: other
                .column_type()
                .map_or("unknown", crate::value::PgType::name),
            right: "unknown",
        }),
    }
}

/// A `~` operand, which must be text.
///
/// **A non-text operand is `42883`, not a cast**: `1 ~ 'a'` is `operator does not exist: integer ~
/// unknown`, with the *other* side reported as `unknown` whichever side is at fault. Measured,
/// both ways round, and it is the same shape `~~` gets.
fn regex_text(value: &Datum, operator: &'static str) -> Result<Option<String>> {
    match value {
        Datum::Null => Ok(None),
        Datum::Text(text) => Ok(Some(text.clone())),
        other => Err(SqlError::UndefinedOperator {
            op: operator,
            left: other
                .column_type()
                .map_or("unknown", crate::value::PgType::name),
            right: "unknown",
        }),
    }
}

/// Whether an expression holds a set-returning call anywhere inside it.
fn has_set_func(expr: &Expr) -> bool {
    let mut found = false;
    super::bind::descend(expr, &mut |expr| {
        found |= matches!(expr, Expr::SetFunc(_));
    });
    found
}

/// One input row, expanded into the rows its set-returning calls make.
///
/// **Lockstep, not a cross join.** Every call in the target list is run once, the output has as
/// many rows as the longest of them, and a call that ran out contributes NULL from there on —
/// which is what a real server does since it moved set-returning functions out of the executor's
/// projection loop, and is measured in `tests/corpus/pg19_srf_target_list.txt`. A call yielding
/// nothing therefore makes **no** rows at all, taking its input row with it: `unnest('{}')` is why
/// the `id = 3` row disappears there.
///
/// The calls are collected by a walk rather than by position, because one may sit **inside** an
/// expression: `abs(generate_series(-1,1))` is `1, 0, 1`, the function expanding and the
/// expression evaluated per generated value.
fn expand(exprs: &[Expr], row: &[Datum], env: Env<'_>) -> Result<Vec<Vec<Datum>>> {
    let mut calls: Vec<crate::plan::TableFunction> = Vec::new();
    for expr in exprs {
        super::bind::descend(expr, &mut |expr| {
            if let Expr::SetFunc(call) = expr {
                calls.push((**call).clone());
            }
        });
    }
    let mut values = Vec::with_capacity(calls.len());
    for call in &calls {
        values.push(super::table_function::rows(call, row)?);
    }
    let longest = values.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::with_capacity(longest);
    for at in 0..longest {
        let mut substituted = exprs.to_vec();
        let mut next = 0;
        for expr in &mut substituted {
            super::bind::walk_expr_mut(expr, &mut |expr| {
                if matches!(expr, Expr::SetFunc(_)) {
                    // One column wide, which every set-returning function this node has is; a
                    // record-returning one would expand to several and is not one of them.
                    let value = values
                        .get(next)
                        .and_then(|rows| rows.get(at))
                        .and_then(|row| row.first())
                        .cloned()
                        .unwrap_or(Datum::Null);
                    *expr = Expr::Literal(crate::plan::Literal::Typed(Box::new(value)));
                    next += 1;
                }
            });
        }
        out.push(
            substituted
                .iter()
                .map(|expr| evaluate_in(expr, row, env))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(out)
}

pub(super) fn evaluate(expr: &Expr, row: &[Datum]) -> Result<Datum> {
    evaluate_in(expr, row, Env::none())
}

/// The same, in a transaction — what a column `DEFAULT` needs, because its clock is the
/// transaction's instant and `now()` reads it from there.
pub(super) fn evaluate_in_txn(expr: &Expr, row: &[Datum], txn: &dyn Txn) -> Result<Datum> {
    evaluate_in(expr, row, Env::in_txn(txn))
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per expression shape; splitting it would hide the vocabulary rather than clarify it"
)]
pub(super) fn evaluate_in(expr: &Expr, row: &[Datum], env: Env<'_>) -> Result<Datum> {
    use crate::plan::Literal;
    Ok(match expr {
        Expr::Ordinal { at, .. } => row.get(*at).cloned().unwrap_or(Datum::Null),
        // The type was settled when the expression was resolved. Where it was not — a `DEFAULT`
        // evaluated by the DDL path, which never resolves against a row — the operands' own types
        // answer the same question, and a NULL operand makes the question moot.
        Expr::Negate(operand) => crate::value::arith::negate(&evaluate_in(operand, row, env)?)?,
        Expr::Arithmetic {
            op,
            left,
            right,
            ty,
        } => {
            let left = evaluate_in(left, row, env)?;
            let right = evaluate_in(right, row, env)?;
            let ty = match ty {
                Some(ty) => *ty,
                None => match (left.column_type(), right.column_type()) {
                    (Some(left), Some(right)) => {
                        crate::value::arith::result_type(*op, left, right)?
                    }
                    _ => return Ok(Datum::Null),
                },
            };
            crate::value::arith::apply(*op, ty, &left, &right)?
        }
        // A cast to `text` is the operand's own output function, and NULL stays NULL: a cast
        // changes a value's type and never invents one.
        // Rust's own case conversion, which is full Unicode and agrees with PostgreSQL's
        // under a UTF-8 locale — measured on an accented pair, since that is where a byte-wise
        // implementation would differ. NULL in, NULL out.
        // `abs` is not a text function and does not reach the arm below: its argument is a
        // number and its answer is one of the same type, overflow included.
        Expr::Scalar {
            func: crate::plan::ScalarFunc::Abs,
            operand,
        } => crate::value::arith::abs(&evaluate_in(operand, row, env)?)?,
        // **`LIKE` is not a comparison**: the two sides are a subject and a pattern, and a NULL in
        // either is unknown — which the negation does not rescue, because the negation of unknown
        // is unknown.
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            escape,
        } => {
            let subject = evaluate_in(operand, row, env)?;
            let pattern_value = evaluate_in(pattern, row, env)?;
            match (like_text(&subject)?, like_text(&pattern_value)?) {
                (Some(subject), Some(pattern)) => {
                    let fold = |text: String| {
                        if *case_insensitive {
                            text.to_lowercase()
                        } else {
                            text
                        }
                    };
                    let subject: Vec<char> = fold(subject).chars().collect();
                    let pattern: Vec<char> = fold(pattern).chars().collect();
                    let matched =
                        crate::plan::like_matches(&subject, &pattern, escape.or(Some('\\')));
                    Datum::Bool(matched != *negated)
                }
                _ => Datum::Null,
            }
        }
        // **`~` is not a comparison either**, and a NULL in either side is unknown — which the
        // negated spelling does not rescue, exactly as `LIKE`'s does not.
        Expr::RegexMatch {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => {
            let subject = evaluate_in(operand, row, env)?;
            let pattern_value = evaluate_in(pattern, row, env)?;
            let operator = if *case_insensitive { "~*" } else { "~" };
            match (
                regex_text(&subject, operator)?,
                regex_text(&pattern_value, operator)?,
            ) {
                (Some(subject), Some(pattern)) => {
                    // Compiled per row, which is what a plan without a constant-folding pass can
                    // do honestly. The pattern is almost always a literal, so this is where a
                    // cache would go once one is measured to be needed.
                    let compiled = crate::value::regex::compile(&pattern)?;
                    Datum::Bool(compiled.is_match(&subject, *case_insensitive) != *negated)
                }
                _ => Datum::Null,
            }
        }
        Expr::Scalar { func, operand } => match evaluate_in(operand, row, env)? {
            Datum::Null => Datum::Null,
            Datum::Text(text) => Datum::Text(match func {
                crate::plan::ScalarFunc::Lower => text.to_lowercase(),
                crate::plan::ScalarFunc::Upper => text.to_uppercase(),
                // Unreachable: the arm above catches `abs` before this one is tried.
                crate::plan::ScalarFunc::Abs => text,
            }),
            other => {
                // A non-text argument: `lower(1)` is `42883 function lower(integer) does not
                // exist` on a real server, not a cast. Measured.
                return Err(SqlError::UndefinedFunctionTypes(format!(
                    "{}({})",
                    func.name(),
                    other
                        .column_type()
                        .map_or("unknown", crate::value::PgType::name)
                )));
            }
        },
        Expr::ToText {
            operand,
            strip_blanks,
        } => match evaluate_in(operand, row, env)? {
            Datum::Null => Datum::Null,
            // **A boolean is the one type whose cast is not its output function.** `SELECT true`
            // prints `t` and `SELECT true::text` is `true`; PostgreSQL has a separate `booltext`
            // for the cast. Measured — every other type here casts to exactly what it prints.
            Datum::Bool(flag) => Datum::Text(if flag { "true" } else { "false" }.to_owned()),
            value => {
                let text = value.to_text().unwrap_or_default();
                Datum::Text(if *strip_blanks {
                    text.trim_end_matches(' ').to_owned()
                } else {
                    text
                })
            }
        },
        // **One branch is evaluated, and the others are not.** `CASE WHEN true THEN 1 ELSE 1/0
        // END` is `1` on a real server and `CASE WHEN false THEN 1 WHEN 1/0 = 0 THEN 2 ELSE 3 END`
        // is `22012` — the second condition is reached and the first result is not. So this walks
        // and returns rather than computing the branches and selecting among them, which would get
        // both of those wrong in opposite directions.
        //
        // A condition is this branch only when it is **`true`**: NULL and false are both "not
        // this one", which is why `CASE WHEN NULL THEN 'a' ELSE 'b' END` is `b`.
        // **The first argument that is not NULL**, and NULL when they all are. Every argument is
        // evaluated in turn and none after the answer, which is what makes
        // `COALESCE(a, 1/0)` safe when `a` is not NULL — the same short circuit a `CASE` has.
        // **Unreachable through the projection**, which expands a set-returning call into rows
        // before any expression is evaluated (`Kind::Project`). One here is a call somewhere the
        // expansion does not reach, and PostgreSQL names those two places itself: a `WHERE` and
        // the inside of an aggregate. Both are refused where they are resolved, so this is the
        // internal error it looks like.
        Expr::SetFunc(call) => {
            return Err(SqlError::Internal(format!(
                "the set-returning function {} reached the row evaluator",
                call.name
            )));
        }
        Expr::Coalesce(args) => {
            let mut answer = Datum::Null;
            for arg in args {
                answer = evaluate_in(arg, row, env)?;
                if !matches!(answer, Datum::Null) {
                    break;
                }
            }
            answer
        }
        Expr::Case {
            branches,
            otherwise,
        } => {
            let mut answer = Datum::Null;
            for branch in branches {
                match evaluate_in(&branch.when, row, env)? {
                    Datum::Bool(true) => return evaluate_in(&branch.then, row, env),
                    Datum::Bool(false) | Datum::Null => {}
                    other => {
                        // Caught where the expression is resolved for every shape whose type is
                        // known then; this is the one that is not — an `unknown` condition, whose
                        // type nothing gives it.
                        return Err(SqlError::DatatypeMismatch(format!(
                            "argument of CASE/WHEN must be type boolean, not type {}",
                            other
                                .column_type()
                                .map_or("unknown", crate::value::PgType::name)
                        )));
                    }
                }
            }
            if let Some(otherwise) = otherwise {
                answer = evaluate_in(otherwise, row, env)?;
            }
            answer
        }
        // **A fresh value per call**, which is what volatile means: two of these in one statement
        // are two different UUIDs, and neither is cached.
        Expr::Uuid(_) => Datum::Uuid(crate::value::random::uuid_v4()?),
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
        // Substituted before the sub-plan it lives in is opened, the way an aggregate is rewritten
        // before the tree is built. One here means a correlated sub-plan was run without being
        // given the row it is correlated to.
        Expr::Outer { level, at, .. } => {
            return Err(SqlError::Internal(format!(
                "an outer reference to column {at}, {level} scopes out, reached the row evaluator"
            )));
        }
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),
        // Resolved before the plan was built (`crate::exec::Executor::bound`), exactly as a
        // `::regclass` is. One here means the resolution was skipped, and answering it from the
        // row would be reading a session this evaluator cannot see.
        Expr::CurrentSchema { .. } | Expr::CurrentDatabase | Expr::CurrentSetting { .. } => {
            return Err(SqlError::Internal(
                "a current_schema reached the row evaluator unresolved".to_owned(),
            ));
        }
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

        // A catalog function **is** a row function, unlike the two above it, and this is where it
        // is answered: a value of its arguments, once per row, with nothing to resolve first.
        Expr::CatalogFunc(call) => catalog_function(call, row, env)?,

        // Its rows were produced before this cursor was opened, by `crate::exec::subquery::resolve`
        // -- the same arrangement a `Node::Columnar` has, and for the same reason: this function
        // has a row and no transaction. What is left here is turning those rows into one value,
        // which is three-valued logic and lives beside the rules it implements.
        Expr::Subquery(sub) => {
            let operand = match &sub.operand {
                Some(operand) => Some(evaluate_in(operand, row, env)?),
                None => None,
            };
            match (sub.correlated, env.txn) {
                // Uncorrelated: its rows were produced before this cursor was opened, by
                // `crate::exec::subquery::resolve`.
                (false, _) => crate::exec::subquery::value(sub, operand)?,
                // Correlated: a different answer for this row, so it runs now. The nested loop
                // this makes is the shape, not an accident (`docs/plans/phase-12-subquery.md` §1).
                (true, Some(txn)) => {
                    let values = crate::exec::subquery::run_correlated(sub, row, txn, env.tenant)?;
                    crate::exec::subquery::value_of(
                        sub.kind,
                        operand,
                        &values,
                        sub.column.as_ref().map(|(_, ty)| *ty),
                    )?
                }
                (true, None) => {
                    return Err(SqlError::Internal(format!(
                        "{} reached an evaluator with no transaction to run in",
                        sub.kind.describe()
                    )));
                }
            }
        }

        Expr::IsNull { operand, negated } => {
            let value = evaluate_in(operand, row, env)?;
            Datum::Bool(matches!(value, Datum::Null) != *negated)
        }

        Expr::Not(operand) => match evaluate_in(operand, row, env)? {
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
        } => in_list(operand, list, *negated, row, env)?,
        // The same three-valued rule as the line above, over an array that is a value of the row
        // rather than a list the lowering could see. Shared rather than copied: `IN` and
        // `= ANY` are one operator on a real server, and a second implementation of "a match wins
        // over a NULL" is a second place for it to be wrong.
        // **Every way of missing is NULL**: out of range at either end, an empty array, a NULL
        // array, a NULL subscript. Five shapes and one answer, which is what lets a caller walk an
        // array without checking its length first — and none of them is an error.
        Expr::Subscript {
            operand,
            index,
            element,
        } => {
            let operand = evaluate_in(operand, row, env)?;
            // **A real array's element is already a value**, of the element type, so it is taken
            // rather than re-read from text: `('{1,2,3}'::int[])[1]` is an `integer` and not the
            // characters `1`. The text path below is the catalog's `int2vector`s.
            if let Datum::Array(array) = &operand {
                let index = match evaluate_in(index, row, env)? {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Int8(at) => at,
                    Datum::Int4(at) => i64::from(at),
                    Datum::Int2(at) => i64::from(at),
                    other => {
                        return Err(SqlError::DatatypeMismatch(format!(
                            "array subscript must be type integer, not {other:?}"
                        )));
                    }
                };
                // **One subscript of a multi-dimensional array is NULL**, not its first row —
                // measured, and the reason a slice is the operator that returns an array.
                if array.dims.len() > 1 {
                    return Ok(Datum::Null);
                }
                let at = index - i64::from(array.lower);
                return Ok(usize::try_from(at)
                    .ok()
                    .and_then(|at| array.values.get(at))
                    .and_then(Clone::clone)
                    .unwrap_or(Datum::Null));
            }
            let Some(array) = read_array(&operand)? else {
                return Ok(Datum::Null);
            };
            let index = match evaluate_in(index, row, env)? {
                Datum::Null => return Ok(Datum::Null),
                Datum::Int8(at) => at,
                Datum::Int4(at) => i64::from(at),
                Datum::Int2(at) => i64::from(at),
                other => {
                    return Err(SqlError::DatatypeMismatch(format!(
                        "array subscript must be type integer, not {other:?}"
                    )));
                }
            };
            // The subscript is **absolute**, so the array's own lower bound is subtracted to find
            // the position: `indkey[0]` is the first element of an `int2vector` and `conkey[1]` is
            // the first of an `int2[]`.
            let at = index - i64::from(array.lower);
            match usize::try_from(at)
                .ok()
                .and_then(|at| array.elements.get(at))
            {
                Some(Some(text)) => Datum::from_text(*element, text)?,
                Some(None) | None => Datum::Null,
            }
        }
        Expr::AnyArray { operand, array } => {
            let operand = evaluate_in(operand, row, env)?;
            if matches!(operand, Datum::Null) {
                return Ok(Datum::Null);
            }
            let Some(array) = read_array(&evaluate_in(array, row, env)?)? else {
                // A NULL array, which is not an empty one: `1 = ANY(NULL::int[])` is NULL where
                // `1 = ANY('{}')` is false. Measured, both.
                return Ok(Datum::Null);
            };
            // Each element is read **as the operand's type**, which is the same rule the plan-time
            // form uses: an array's elements have no type of their own here, and what gives them
            // one is what they are being compared against.
            let ty = operand.column_type().unwrap_or(ColumnType::Text);
            let mut values = Vec::with_capacity(array.elements.len());
            for element in &array.elements {
                values.push(match element {
                    Some(text) => Datum::from_text(ty, text)?,
                    None => Datum::Null,
                });
            }
            three_valued_match(&operand, &values, false)
        }

        Expr::Binary { op, left, right } => {
            let (left, right) = (evaluate_in(left, row, env)?, evaluate_in(right, row, env)?);
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
                // **The two comparisons that never answer unknown.** `IS NOT DISTINCT FROM` is
                // `=` made total: two NULLs are not distinct, one NULL is, and neither answer is
                // unknown. It is decided before the NULL rule below rather than inside it,
                // because the NULL rule is exactly what these two opt out of.
                BinaryOp::Distinct | BinaryOp::NotDistinct => {
                    let same = match (&left, &right) {
                        (Datum::Null, Datum::Null) => true,
                        (Datum::Null, _) | (_, Datum::Null) => false,
                        // The type's own `=`, not representation equality: `1.0` and `1.00` are
                        // one `numeric` value written two ways and are not distinct.
                        _ => left.pg_cmp(&right).is_eq(),
                    };
                    Datum::Bool(same == matches!(op, BinaryOp::NotDistinct))
                }
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
                        BinaryOp::And
                        | BinaryOp::Or
                        | BinaryOp::Distinct
                        | BinaryOp::NotDistinct => unreachable!("handled above"),
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
/// One call to a `pg_catalog` function that prints a definition.
///
/// The arity was checked where the call was lowered, so an argument that is not there is a bug
/// rather than a user's mistake — and it is answered as NULL rather than as a panic, because every
/// one of these functions is NULL-propagating anyway ([`crate::catalog::def_functions`]).
/// `convert_to(text, encoding)`: the bytes `text` has in `encoding`.
///
/// **Strict in both arguments**, the encoding name included — `convert_to('A', NULL)` is NULL,
/// measured. A name that is not one of PostgreSQL's encodings is `22023` and not a refusal,
/// because that is a real server's own answer; a name that *is* one but is not UTF-8 is `0A000`,
/// because transcoding is a conversion table this node does not have and returning the UTF-8
/// bytes under another encoding's name would be a wrong answer wearing a right one's label.
fn convert_to(text: Option<&Datum>, encoding: Option<&Datum>) -> Result<Datum> {
    let (Some(text), Some(encoding)) = (text, encoding) else {
        return Ok(Datum::Null);
    };
    if matches!(text, Datum::Null) || matches!(encoding, Datum::Null) {
        return Ok(Datum::Null);
    }
    let (Some(text), Some(encoding)) = (text.to_text(), encoding.to_text()) else {
        return Ok(Datum::Null);
    };
    // `pg_char_to_encoding` matches case-insensitively and ignores `-` and `_`, so `UTF8`, `utf8`
    // and `Utf-8` are one name. Measured, all three.
    let folded: String = encoding
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if folded == "UTF8" || folded == "UNICODE" {
        return Ok(Datum::Bytea(text.into_bytes()));
    }
    if crate::value::encoding::is_postgresql_encoding(&folded) {
        return Err(SqlError::unsupported(format!("the encoding {encoding}")));
    }
    Err(SqlError::InvalidDestinationEncoding(encoding))
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per catalog function; the list being in one place is what it is for"
)]
fn catalog_function(
    call: &crate::plan::CatalogFuncCall,
    row: &[Datum],
    env: Env<'_>,
) -> Result<Datum> {
    use crate::plan::CatalogFunc;

    let mut args = Vec::with_capacity(call.args.len());
    for arg in &call.args {
        args.push(evaluate_in(arg, row, env)?);
    }
    Ok(match call.func {
        // **The transaction's instant**, so two calls in one transaction are equal and their
        // difference is `00:00:00`. It comes from the TSO and never from a clock this node reads.
        //
        // **One instant, six spellings, five types.** Every row of a multi-row `VALUES` therefore
        // gets the same timestamp, which is what `insert_all` compares and what an implementation
        // that read a clock per row would get wrong. `statement_timestamp` and `clock_timestamp`
        // are here too and are *declared divergences*: a real server advances them per statement
        // and per call, and invariant 6 says the TSO's physical half is the only clock this node
        // may read — a transaction has exactly one. The corpus pins the consequence that survives
        // it, `clock_timestamp() >= transaction_timestamp()`.
        CatalogFunc::Now
        | CatalogFunc::CurrentDate
        | CatalogFunc::LocalTimestamp
        | CatalogFunc::LocalTime
        | CatalogFunc::StatementTimestamp
        | CatalogFunc::ClockTimestamp => {
            let Some(txn) = env.txn else {
                return Err(SqlError::Internal(format!(
                    "{}() reached an evaluator with no transaction",
                    call.func.name()
                )));
            };
            let micros = crate::time_machine::micros_of_ts(txn.start_ts());
            match call.func {
                // Floor division, so an instant before the epoch lands on the day containing it.
                CatalogFunc::CurrentDate => {
                    let day = micros.div_euclid(86_400_000_000);
                    Datum::Date(i32::try_from(day).unwrap_or(i32::MAX))
                }
                CatalogFunc::LocalTimestamp => Datum::Timestamp(micros),
                // The time of day *within* that instant, so the same floor division decides the
                // day and the remainder is what is left of it.
                CatalogFunc::LocalTime => Datum::Time(micros.rem_euclid(86_400_000_000)),
                _ => Datum::TimestampTz(micros),
            }
        }
        // **Not strict**: a NULL argument is skipped, not propagated, so `concat(NULL, NULL)` is
        // the empty string. Each argument is rendered by its own output function.
        CatalogFunc::Concat => Datum::Text(
            args.iter()
                .filter(|arg| !matches!(arg, Datum::Null))
                .filter_map(PgDatum::to_text)
                .collect::<String>(),
        ),
        CatalogFunc::ConvertTo => convert_to(args.first(), args.get(1))?,
        // The value's own type. An untyped NULL has none and is `text`, which is what it is
        // everywhere else in this crate.
        CatalogFunc::PgTypeof => Datum::Text(
            args.first()
                .and_then(Datum::column_type)
                .map_or("text", crate::value::PgType::name)
                .to_owned(),
        ),
        // **Per call, and the corpus pins the consequence rather than a value**: two calls in one
        // statement differ, and every draw is inside `[0, 1)`. The bytes come from the OS pool
        // through the same file `gen_random_uuid` reads (`crate::value::random`).
        CatalogFunc::Random => Datum::Double(crate::value::random::random_f64()?),
        CatalogFunc::FormatType => crate::catalog::def_functions::format_type(
            type_oid_argument(args.first())?,
            typmod_argument(args.get(1))?,
        ),
        // **The five array operators, over the text the catalog holds** — see
        // `crate::value::vector` for why an array is text here and not a `Datum`. Every one is
        // strict: a NULL array or a NULL argument is NULL, never an error and never 0.
        //
        // The dimension argument is accepted and only dimension 1 has an answer, because these
        // arrays are one-dimensional; a real server answers NULL for any other dimension, which is
        // what asking for one of a one-dimensional array means.
        CatalogFunc::ArrayPosition
        | CatalogFunc::ArrayLower
        | CatalogFunc::ArrayUpper
        | CatalogFunc::ArrayLength
        | CatalogFunc::Cardinality => array_function(call.func, &args)?,
        // The identity on its first argument, which is where the printed expression already is —
        // and NULL-propagating, so a `LEFT JOIN pg_attrdef` that matched nothing is NULL rather
        // than an error. The third argument is `pretty`, which changes nothing this node prints.
        CatalogFunc::PgGetExpr => args.first().cloned().unwrap_or(Datum::Null),
        // The catalog it reads is snapshotted by the cursor, so a projection over every row of
        // `pg_index` reads it once rather than once per index.
        // **Strict in its second argument when there is one**, and that is not the same as
        // having none: `pg_get_indexdef(oid)` is the whole definition and
        // `pg_get_indexdef(oid, NULL, true)` is NULL. Measured, and the difference is invisible in
        // an `Option` that flattens the two.
        CatalogFunc::PgGetIndexdef if matches!(args.get(1), Some(Datum::Null)) => Datum::Null,
        CatalogFunc::PgGetIndexdef => crate::catalog::pg_index::index_definition(
            env.relations()?,
            oid_argument(args.first())?,
            column_argument(args.get(1))?,
        ),
        // The `pretty` flag changes nothing this node prints: it re-wraps a long `CHECK`
        // expression on a real server, and there are no `CHECK` constraints here.
        // **The inverse of `'x'::regclass`, and per row.** An oid that names nothing is not an
        // error: it prints the number back, and oid 0 prints `-`, PostgreSQL's rendering of
        // `InvalidOid`. Measured, both — raising here would break a `LEFT JOIN` that legitimately
        // has no match.
        CatalogFunc::RegClassName => match oid_argument(args.first())? {
            None => Datum::Null,
            Some(oid) => match env.relations()?.by_oid(oid) {
                Some(relation) => Datum::Text(relation.name.clone()),
                None if oid == 0 => Datum::Text("-".to_owned()),
                None => Datum::Text(oid.to_string()),
            },
        },
        // The one encoding this node speaks. Anything else is the empty string, which is what a
        // real server answers for a number that names no encoding.
        CatalogFunc::PgEncodingToChar => match oid_argument(args.first())? {
            Some(6) => Datum::Text("UTF8".to_owned()),
            Some(_) => Datum::Text(String::new()),
            None => Datum::Null,
        },
        // **The sequence a column's default draws from, schema-qualified.** Its arguments are
        // names rather than oids, which is why it is the one catalog function here that looks a
        // relation up by name — and a table or a column that is not there is NULL, not an error,
        // like the rest of this surface.
        CatalogFunc::PgGetSerialSequence => {
            let (Some(Datum::Text(table)), Some(Datum::Text(column))) = (args.first(), args.get(1))
            else {
                return Ok(Datum::Null);
            };
            let relations = env.relations()?;
            // **Its argument is a name inside a string**, so `pg_get_serial_sequence('s.t', 'id')`
            // has to be split the way `::regclass`'s argument is — the table it names may be in
            // any schema, and looking the whole string up finds nothing.
            let stored = crate::catalog::parse_qualified(table);
            let sequence = relations
                .by_name(&stored)
                .and_then(|row| relations.table(row))
                .and_then(|table| {
                    let at = table.column(column)?;
                    table
                        .sequences
                        .iter()
                        .find(|sequence| sequence.column == Some(at))
                });
            match sequence {
                // **The schema is the table's**, not `public`: a `bigserial` in `s` owns `s.t_id_seq`
                // there, and the qualified text is what goes back out to `setval`.
                Some(sequence) => Datum::Text(format!(
                    "{}.{}",
                    crate::catalog::split_qualified(&stored).0,
                    crate::catalog::split_qualified(&sequence.name).1
                )),
                None => Datum::Null,
            }
        }
        CatalogFunc::PgGetConstraintdef => crate::catalog::pg_constraint::constraint_definition(
            env.relations()?,
            oid_argument(args.first())?,
        ),
        // **Nothing found is NULL and never an error** — an uncommented object, an attnum out of
        // range, a negative one, an oid that names nothing, an unknown catalog name and a NULL
        // argument are all NULL on a real server. There is no not-found error anywhere in this
        // surface, which is what makes a `LEFT JOIN` over it work.
        CatalogFunc::ObjDescription => match oid_argument(args.first())? {
            None => Datum::Null,
            Some(oid) => match env.relations()?.comment_of(oid) {
                Some(comment) => Datum::Text(comment.to_owned()),
                None => Datum::Null,
            },
        },
        // **The attnum is an `int2` when it comes from `pg_attribute`**, which is the shape the
        // schema dump writes: `col_description(a.attrelid, a.attnum)`. Reading only the wider
        // integers answered NULL for every column of a real join while a hand-written
        // `col_description(oid, 2)` worked — the same function, two widths, one of them wrong.
        CatalogFunc::ColDescription => {
            match (oid_argument(args.first())?, oid_argument(args.get(1))?) {
                (Some(oid), Some(attnum)) => match env.relations()?.column_comment(oid, attnum) {
                    Some(comment) => Datum::Text(comment.to_owned()),
                    None => Datum::Null,
                },
                _ => Datum::Null,
            }
        }
        CatalogFunc::DateRange | CatalogFunc::IsEmpty | CatalogFunc::RangeOverlaps => {
            range_function(call.func, &args)?
        }
        // **`LIST (city_id)`** — the strategy word and the key columns, and NULL for a relation
        // that is not partitioned, which is what a real server answers there too.
        CatalogFunc::PgGetPartkeydef => {
            crate::catalog::partition_key_definition(env.relations()?, oid_argument(args.first())?)
        }
        // **`EXECUTE PROCEDURE` prints back as `EXECUTE FUNCTION`**, so the text out is not the
        // text in — statement 762 writes the first spelling and statement 790 the second.
        CatalogFunc::PgGetTriggerdef => {
            crate::catalog::trigger_definition(env.relations()?, oid_argument(args.first())?)
        }
        // Resolved before the plan was built (`crate::exec::Executor::bound`). One here means the
        // resolution was skipped, and answering it from the row would be a catalog read per row.
        CatalogFunc::RegClass => {
            return Err(SqlError::Internal(
                "a ::regclass reached the row evaluator unresolved".to_owned(),
            ));
        }
    })
}

/// `format_type`'s first argument: an `oid`.
///
/// **Text is taken as a type name**, and that is not a liberty — it is the one coercion this node
/// cannot express any other way. A real server writes `format_type('integer'::regtype, NULL)` and
/// coerces `regtype` to `oid` for free; here `'integer'::regtype` lowers to the *name as text*
/// (`crate::parse::lower::lower_cast`), so the same statement arrives with a string in it and
/// resolving it is what makes the answer identical. A name that is no type of this server's is
/// `42704`, which is what `'x'::regtype` itself answers.
fn type_oid_argument(arg: Option<&Datum>) -> Result<Option<i64>> {
    Ok(match arg {
        None | Some(Datum::Null) => None,
        Some(Datum::Int8(oid)) => Some(*oid),
        Some(Datum::Int4(oid)) => Some(i64::from(*oid)),
        Some(Datum::Int2(oid)) => Some(i64::from(*oid)),
        Some(Datum::Text(name)) => {
            use crate::value::PgType as _;
            let ty = crate::value::type_by_name(name)?
                .ok_or_else(|| SqlError::UndefinedType(name.trim().to_owned()))?;
            Some(i64::from(ty.oid()))
        }
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "format_type() takes an oid, not {other:?}"
            )));
        }
    })
}

/// An `oid` argument, which is an integer of whatever width the column it came from has.
fn oid_argument(arg: Option<&Datum>) -> Result<Option<i64>> {
    Ok(match arg {
        None | Some(Datum::Null) => None,
        Some(Datum::Int8(oid)) => Some(*oid),
        Some(Datum::Int4(oid)) => Some(i64::from(*oid)),
        Some(Datum::Int2(oid)) => Some(i64::from(*oid)),
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "an oid is an integer, not {other:?}"
            )));
        }
    })
}

/// `pg_get_indexdef`'s optional column number. `None` is the one-argument form, which is a
/// different answer from column `0` — that one prints the definition unqualified.
fn column_argument(arg: Option<&Datum>) -> Result<Option<i32>> {
    Ok(match arg {
        // A NULL is answered above, before this is called: the function is strict in this
        // argument and `None` here means the one-argument form, which prints the whole
        // definition. Flattening the two would print a definition where a real server says NULL.
        None | Some(Datum::Null) => None,
        Some(Datum::Int8(at)) => Some(i32::try_from(*at).unwrap_or(i32::MAX)),
        Some(Datum::Int4(at)) => Some(*at),
        Some(Datum::Int2(at)) => Some(i32::from(*at)),
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "a column number is an integer, not {other:?}"
            )));
        }
    })
}

/// `format_type`'s second argument: a type modifier, or NULL — **which is not the same as `-1`**,
/// and is the whole reason this returns an `Option` rather than defaulting.
fn typmod_argument(arg: Option<&Datum>) -> Result<Option<i32>> {
    Ok(match arg {
        None | Some(Datum::Null) => None,
        // An `int4` is what `pg_attribute.atttypmod` is on both servers, and an `int8` is what a
        // literal written in the statement is. Both are the same number.
        Some(Datum::Int4(typmod)) => Some(*typmod),
        Some(Datum::Int2(typmod)) => Some(i32::from(*typmod)),
        Some(Datum::Int8(typmod)) => Some(i32::try_from(*typmod).unwrap_or(i32::MAX)),
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "a type modifier is an integer, not {other:?}"
            )));
        }
    })
}

/// Measured on 19beta1, `tests/corpus/pg19_in.txt`. A scan rather than a rewrite to `= a OR = b`,
/// so the left-hand side is evaluated once — which also keeps a `nextval` on the left from
/// running per item.
fn in_list(
    operand: &Expr,
    list: &[Expr],
    negated: bool,
    row: &[Datum],
    env: Env<'_>,
) -> Result<Datum> {
    let operand = evaluate_in(operand, row, env)?;
    // A NULL on the left can neither match nor definitely fail to, so nothing in the list can
    // change the answer.
    if matches!(operand, Datum::Null) {
        return Ok(Datum::Null);
    }
    let mut values = Vec::with_capacity(list.len());
    for item in list {
        values.push(evaluate_in(item, row, env)?);
    }
    Ok(three_valued_match(&operand, &values, negated))
}

/// `x IN (…)` and `x = ANY(…)`, over values that have already been evaluated.
///
/// **The rule is not "NULL means false".** A match wins over a NULL and a NULL wins over no match,
/// so `1 IN (1, NULL)` is true, `1 IN (2, NULL)` is NULL, and `1 NOT IN (2, NULL)` is NULL — which
/// is why a `NOT IN` over a list containing NULL matches nothing at all. Measured,
/// `tests/corpus/pg19_in.txt`; the caller has already answered NULL for a NULL operand.
fn three_valued_match(operand: &Datum, values: &[Datum], negated: bool) -> Datum {
    let mut unknown = false;
    for value in values {
        if matches!(value, Datum::Null) {
            unknown = true;
            continue;
        }
        if operand.pg_cmp(value).is_eq() {
            return Datum::Bool(!negated);
        }
    }
    if unknown {
        Datum::Null
    } else {
        Datum::Bool(negated)
    }
}

/// The array a value holds, or `None` for a NULL — which is a different answer from an empty one.
///
/// A value that is not text at all is `42883`, the way a real server answers an operator it has no
/// overload for: `array_length(1, 1)` names the function and the type rather than pretending.
fn read_array(value: &Datum) -> Result<Option<crate::value::vector::Array>> {
    match value {
        Datum::Null => Ok(None),
        // **A real array value**, which is what an array column and an array cast produce now.
        // The text form below is still here and still needed: `pg_index.indkey` and
        // `pg_constraint.conkey` are `int2vector`s that this node holds as text, and they reach
        // the same operators (`crate::value::vector`).
        Datum::Array(array) => Ok(Some(crate::value::vector::Array {
            elements: array
                .values
                .iter()
                .map(|element| element.as_ref().and_then(PgDatum::to_text))
                .collect(),
            lower: array.lower,
        })),
        Datum::Text(text) => crate::value::vector::Array::read(text)
            .map(Some)
            .ok_or_else(|| {
                SqlError::UndefinedFunctionTypes(format!(
                    "an array literal this node cannot read: {text}"
                ))
            }),
        other => Err(SqlError::UndefinedFunctionTypes(format!(
            "array operator on {}",
            other
                .column_type()
                .map_or("unknown", crate::value::PgType::name)
        ))),
    }
}

/// The five array operators, over the text form the catalog holds.
fn array_function(func: crate::plan::CatalogFunc, args: &[Datum]) -> Result<Datum> {
    use crate::plan::CatalogFunc;
    let Some(array) = read_array(args.first().unwrap_or(&Datum::Null))? else {
        return Ok(Datum::Null);
    };
    // Every one of these is strict, so a NULL in any argument is a NULL answer.
    if args.iter().any(|arg| matches!(arg, Datum::Null)) {
        return Ok(Datum::Null);
    }
    // **Only three of the five take a dimension.** `cardinality(a)` has one argument, and
    // `array_position(a, value)`'s second argument is the value it is looking for — reading that
    // as a dimension answers NULL for every needle that is not `1`, which is a wrong answer and
    // not a gap.
    let takes_dimension = matches!(
        func,
        CatalogFunc::ArrayLower | CatalogFunc::ArrayUpper | CatalogFunc::ArrayLength
    );
    let wanted = match args.get(1) {
        Some(Datum::Int8(at)) => i32::try_from(*at).unwrap_or(i32::MAX),
        Some(Datum::Int4(at)) => *at,
        Some(Datum::Int2(at)) => i32::from(*at),
        _ => 1,
    };
    // **The shape decides which dimensions exist**, and a real array carries one:
    // `array_length('{{1,2},{3,4}}', 2)` is 2 where the same call on a one-dimensional array is
    // NULL. A text `int2vector` from the catalog has no shape, so its one dimension is its length
    // — which is what it has always been.
    let shape: Vec<i32> = match args.first() {
        Some(Datum::Array(value)) => value.dims.clone(),
        _ if array.elements.is_empty() => Vec::new(),
        _ => vec![i32::try_from(array.elements.len()).unwrap_or(i32::MAX)],
    };
    let dimension_length = usize::try_from(wanted)
        .ok()
        .filter(|at| *at >= 1)
        .and_then(|at| shape.get(at - 1))
        .copied();
    let first_dimension = !takes_dimension || dimension_length.is_some();
    let length =
        dimension_length.unwrap_or_else(|| i32::try_from(array.elements.len()).unwrap_or(i32::MAX));
    Ok(match func {
        // Every element of every dimension, which is what makes it 4 for a two-by-two where
        // `array_length(a, 1)` is 2.
        CatalogFunc::Cardinality => {
            Datum::Int4(i32::try_from(array.elements.len()).unwrap_or(i32::MAX))
        }
        _ if !first_dimension => Datum::Null,
        // An empty array has **no dimensions**, so its bounds and its length are NULL where its
        // cardinality is 0. Measured, and it is the shape that breaks a `LIMIT` computed from it.
        CatalogFunc::ArrayLength | CatalogFunc::ArrayLower if length == 0 => Datum::Null,
        CatalogFunc::ArrayLength => Datum::Int4(length),
        CatalogFunc::ArrayLower => Datum::Int4(array.lower),
        CatalogFunc::ArrayUpper => array.upper().map_or(Datum::Null, Datum::Int4),
        CatalogFunc::ArrayPosition => {
            // The needle is compared **as text**, which is what the elements are: the argument was
            // already read at its own type, and its text form is the one the array was written in.
            args.get(1)
                .and_then(Datum::to_text)
                .and_then(|needle| array.position_of(&needle))
                .map_or(Datum::Null, Datum::Int4)
        }
        other => {
            return Err(SqlError::Internal(format!(
                "{} reached the array evaluator",
                other.name()
            )));
        }
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
