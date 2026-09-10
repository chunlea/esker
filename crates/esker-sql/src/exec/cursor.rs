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
use crate::value::{PgDatum, PgType, ltree, range};

/// The most rows a `Sort` will hold. Past it, `53400` rather than an unbounded allocation.
pub(super) const SORT_LIMIT: usize = 1_000_000;

/// A relation name to its oid: the executor's own rule, carried to wherever a value is.
///
/// The mirror of `crate::row::NameOfRelation`, which goes the other way for the same reason —
/// see `Settings::names`.
pub(super) type OidOfRelation<'a> = &'a dyn Fn(&str) -> Result<i64>;

/// A pull iterator over a plan: one call, one row.
///
/// This is what makes `LIMIT` cheap. A `SELECT * FROM big LIMIT 10` opens a scan, reads one chunk
/// of keys, and stops — it never asks the store for the rest. Materialising the input and slicing
/// it afterwards would give the same answer and read the whole table to do it.
/// The session settings a cursor and everything under it read.
///
/// **Not a bag of options: these are the answers that are a property of the session rather than of
/// the snapshot**, and there is no other way for them to arrive. `search_path` was the first —
/// `'x'::regclass::text` qualifies a name only when its schema is not on the path, measured:
/// `g1_rc.t` under the default path and `t` after `SET search_path = g1_rc, public`.
/// `IntervalStyle` is the second, and it arrived with the same argument from the other end: a
/// client that set `iso_8601` and is answered `1 mon` cannot read the value at all
/// (`tests/interval_style.rs`). They are one struct so that the third does not need seventeen call
/// sites changed again.
#[derive(Clone, Copy)]
pub(super) struct Settings<'a> {
    /// The session's **resolved** `search_path`; empty for an evaluator that has no session behind
    /// it, which prints every name qualified — the answer that cannot mislead.
    pub(super) search_path: &'a [String],
    /// How an `interval` is rendered for a client.
    pub(super) rendering: crate::value::Rendering,
    /// What this session has prepared, which is the whole of `pg_prepared_statements`.
    ///
    /// The third, and the one the note above predicted: it is session state with no other way in,
    /// and the view over it is read by a plan like any other.
    pub(super) prepared: &'a [crate::session::PreparedStatement],
    /// The node's advisory lock table, which is the whole of `pg_locks`'s `advisory` rows.
    ///
    /// The fourth, and the note above holds. `None` is an evaluator with no session behind it — a
    /// column `DEFAULT` or an index key — which cannot be reading `pg_locks` in the first place.
    /// The snapshot is taken inside the view rather than here, because that is where the rows are
    /// wanted and `Locks::rows` already holds the mutex for exactly as long as the copy takes.
    pub(super) advisory: Option<&'a crate::advisory::Locks>,
    /// **A relation name to its oid** — the one direction `crate::row::decode_row`'s rule does not
    /// go (`debts-v1.1.md` #41).
    ///
    /// `regclassin` resolves a name, and every rule for doing so is the executor's:
    /// `stored_name_written` rewrites a `pg_temp` prefix and then walks the resolved `search_path`
    /// against the catalog view. Writing a second resolver here — `Env` does carry a transaction, a
    /// catalog snapshot and the path — would be a second reader of one name grammar, which is the
    /// mistake this crate has already paid for. So the executor builds the rule and this carries
    /// it, exactly as the *oid to name* direction is carried into `decode_row`.
    ///
    /// `None` is an evaluator with no session behind it, and it is why a `Datum::Text` cast to
    /// `regclass` there is a refusal rather than a wrong relation.
    pub(super) names: Option<OidOfRelation<'a>>,
}

impl Settings<'_> {
    /// No session at all: what a column `DEFAULT` and an index key are evaluated under.
    pub(super) fn none() -> Self {
        Settings {
            search_path: &[],
            rendering: crate::value::Rendering::default(),
            prepared: &[],
            advisory: None,
            names: None,
        }
    }
}

pub(super) struct Cursor<'a> {
    txn: &'a dyn Txn,
    tenant: u64,
    /// What the session decides about the rows this cursor produces.
    settings: Settings<'a>,
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
    /// A set operation's arms, drained one after another.
    ///
    /// **It streams**, which is what makes `UNION ALL` cost what its arms cost: the arms are
    /// opened lazily, one at a time, so a set over two scans never holds both. `at` is the arm
    /// being drained and the plans behind it are opened as it reaches them.
    Append {
        /// The arms still to open, in the order written.
        rest: std::vec::IntoIter<Node>,
        /// The arm being drained, or `None` before the first is opened.
        current: Option<Box<Cursor<'a>>>,
    },
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
        /// A `FULL JOIN`'s other half: an inner row no outer row matched is emitted after the
        /// outer side ends, with every outer column NULL.
        keep_right: bool,
        /// How many NULLs such a row is extended with. Carried rather than learned from an outer
        /// row, because a full join over an **empty** outer side still returns every inner row and
        /// there is no row to learn from.
        outer_columns: usize,
        /// Which materialised inner rows have been paired with something.
        ///
        /// Empty unless `keep_right`: a left or inner join never asks, and the answer costs a bit
        /// per inner row. Written where a pair is kept, read once when the outer side ends.
        matched_inner: Vec<bool>,
        /// How far the drain has got through `matched_inner`, once the outer side is done.
        draining: Option<usize>,
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
        /// `materialized` grouped by its half of an equality in the `ON`, when the `ON` has one.
        ///
        /// **This is what keeps a materialised join off the cross product.** Without it every
        /// outer row is paired with every inner row and the `ON` decides — which is correct and is
        /// `O(outer × inner)`. `ActiveRecord`'s `eager_load` over the suite's generated
        /// `citations` fixture is 65,536 rows joined to itself, so that is 4.3 **billion** pairs
        /// for a statement whose answer is 65,536 rows: the node held a core for the whole of run
        /// 53's 120 s watchdog and was still going when the client was killed.
        ///
        /// It changes no answer and no order. The rows a bucket holds are in the order they were
        /// materialised, so the pairs come out in the order a full pass produces them, and the
        /// **whole `ON` is still evaluated on every pair** — the bucket only skips pairs whose
        /// equality conjunct is false, and a false conjunct makes the conjunction false.
        buckets: Option<EquiBuckets>,
        /// Whether [`Kind::NestedLoop::buckets`] has been decided; see why it cannot be at open.
        buckets_built: bool,
        /// The positions in `materialized` this outer row may pair with, or `None` for all of them.
        candidates: Option<Vec<usize>>,
    },
}

/// One equality out of a join's `ON`, and the inner rows grouped by its inner half.
///
/// The key is a [`Datum`] ordered by `pg_cmp`, which is this crate's SQL comparison — the one the
/// `=` in the `ON` is decided by, and the one a unique index groups equal values with. Keying on
/// anything else (the bytes, the text) would be a second opinion about equality, and `citext` and
/// `float8` are where two opinions differ.
struct EquiBuckets {
    /// Where the outer half of the equality sits in an outer row.
    outer_at: usize,
    /// Inner-row positions by key, each in the order they were materialised.
    by_key: BTreeMap<PgKey, Vec<usize>>,
}

/// A [`Datum`] ordered by `pg_cmp` so it can key a map.
#[derive(PartialEq, Eq)]
struct PgKey(Datum);

impl Ord for PgKey {
    fn cmp(&self, other: &Self) -> Ordering {
        PgDatum::pg_cmp(&self.0, &other.0)
    }
}

impl PartialOrd for PgKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl EquiBuckets {
    /// The buckets an `ON` allows, or `None` when it has no `outer = inner` conjunct to group by.
    ///
    /// Only an equality **between the two sides** counts: one whose halves are both on the inner
    /// side is a filter, and one with an expression on either side is not a key this can group by.
    /// Anything it declines leaves the join exactly as it was.
    fn build(residual: Option<&Expr>, rows: &[Vec<Datum>], boundary: usize) -> Option<Self> {
        let (outer_at, inner_at) = equi_positions(residual?, boundary)?;
        let mut by_key: BTreeMap<PgKey, Vec<usize>> = BTreeMap::new();
        for (at, row) in rows.iter().enumerate() {
            // A row too short for the position is a plan that does not match its rows; the full
            // pass is always correct, so this declines rather than guessing.
            let key = row.get(inner_at)?;
            // **A NULL key pairs with nothing**, because `NULL = anything` is unknown and unknown
            // keeps no pair — the same rule the indexed probes apply before their read. Leaving it
            // out of the map is what makes the lookup below exact rather than approximate.
            if matches!(key, Datum::Null) {
                continue;
            }
            by_key.entry(PgKey(key.clone())).or_default().push(at);
        }
        Some(EquiBuckets { outer_at, by_key })
    }

    /// The inner positions an outer row may pair with.
    fn candidates(&self, row: &[Datum]) -> Vec<usize> {
        match row.get(self.outer_at) {
            // A NULL on the outer half matches nothing, for the reason above.
            None | Some(Datum::Null) => Vec::new(),
            Some(value) => self
                .by_key
                .get(&PgKey(value.clone()))
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// The `(outer, inner)` positions of an equality between the two sides of a join, out of the
/// `AND` spine of an `ON`.
fn equi_positions(on: &Expr, boundary: usize) -> Option<(usize, usize)> {
    match on {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => equi_positions(left, boundary).or_else(|| equi_positions(right, boundary)),
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            // **The inner half is relative to the inner row**, which is what `materialized` holds:
            // the `ON`'s ordinals are positions in the *joined* row, so the inner one is past the
            // boundary and has to come back to it. `exec::query::probe_for` subtracts the same
            // amount for the same reason.
            (Expr::Ordinal { at: a, .. }, Expr::Ordinal { at: b, .. }) => {
                match (*a < boundary, *b < boundary) {
                    (true, false) => Some((*a, *b - boundary)),
                    (false, true) => Some((*b, *a - boundary)),
                    // Both halves on one side joins nothing: it is a filter, and the full pass
                    // applies it correctly.
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
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
#[allow(
    clippy::too_many_arguments,
    reason = "the inner side of a hash join is described by exactly these; grouping them into a \
              struct would name the same fields twice"
)]
fn inner_side(
    txn: &dyn Txn,
    tenant: u64,
    settings: Settings<'_>,
    inner_table_id: u64,
    inner_view: Option<&crate::plan::CatalogView>,
    inner_plan: Option<&Node>,
    inner_columns: &RowSchema,
    probe: &Probe,
) -> Result<Vec<Vec<Datum>>> {
    if let Some(view) = inner_view {
        return view.rows_of(
            txn,
            tenant,
            settings.rendering,
            settings.prepared,
            settings.advisory,
        );
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
        Some(plan) => Cursor::open(txn, tenant, settings, plan)?,
        None => Cursor {
            txn,
            tenant,
            settings,
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
    pub(super) fn open(
        txn: &'a dyn Txn,
        tenant: u64,
        settings: Settings<'a>,
        node: &Node,
    ) -> Result<Self> {
        let kind = match node {
            Node::OneRow => Kind::One(false),
            // Computed here, once, rather than page by page: `pg_type` is six rows and `pg_range`
            // is none. If a catalog view ever is not small, this is the line that changes.
            Node::CatalogView { view, .. } => Kind::Rows(
                view.rows_of(
                    txn,
                    tenant,
                    settings.rendering,
                    settings.prepared,
                    settings.advisory,
                )?
                .into_iter(),
            ),
            // Rows written into the statement, evaluated here for the same reason a catalog view's
            // are: nothing is stored, so there is no key range to seek in and the row count is the
            // length of the list.
            Node::Values { list, .. } => Kind::Rows(super::values::rows(list, txn)?.into_iter()),
            // **The fixpoint.** The seed is drained first and becomes the first working table;
            // every round after it runs the step with that round's rows in place of the CTE's
            // name. Materialised a round at a time, which is the one place this differs from a
            // real server: PostgreSQL streams its working table, so a `LIMIT` can stop an
            // unbounded recursion there and here the cap has to (`tests/recursive_cte.rs`).
            Node::Recursive {
                seed,
                step,
                distinct,
                ..
            } => Kind::Rows(
                super::recursive::run(txn, tenant, settings, seed, step, *distinct)?.into_iter(),
            ),
            // Filled in by the round above it; empty anywhere else, which is what an unreferenced
            // working table is.
            Node::WorkingTable { rows, .. } => Kind::Rows(rows.clone().into_iter()),
            // A set-returning function in `FROM`: its rows are computed here, once, exactly as a
            // catalog view's are — there is no key range to seek in and the row count is the
            // length of one array. Its arguments are evaluated against **no row**, which is what
            // makes an argument that reads a column the refusal below rather than a wrong answer.
            // One row, read from the catalog at open exactly as a catalog view's rows are.
            Node::SequenceRead { state, .. } => {
                let Some((last, is_called)) = *state else {
                    return Err(SqlError::Internal(
                        "a sequence read reached the cursor before its value was taken".to_owned(),
                    ));
                };
                Kind::Rows(
                    vec![vec![
                        Datum::Int8(last),
                        // **`log_cnt` is 0**, which is what a freshly written sequence shows on a
                        // real server too: it counts values left in a WAL-logged batch, and this
                        // node reaches crash safety another way.
                        Datum::Int8(0),
                        Datum::Bool(is_called),
                    ]]
                    .into_iter(),
                )
            }
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
                keep_right,
                outer_columns,
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
                    settings,
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
                    outer: Box::new(Cursor::open(txn, tenant, settings, outer)?),
                    left_join: *left_join,
                    keep_right: *keep_right,
                    outer_columns: *outer_columns,
                    matched_inner: if *keep_right {
                        vec![false; materialized.len()]
                    } else {
                        Vec::new()
                    },
                    draining: None,
                    inner_table_id: *inner_table_id,
                    inner_columns: inner_columns.clone(),
                    probe: probe.clone(),
                    residual: residual.clone(),
                    current: None,
                    matched: false,
                    // **Built from the first outer row, not here**, because the boundary between
                    // the two halves of the `ON`'s ordinals is the outer row's width and nothing
                    // at open knows it. A join whose outer side is empty never builds them, which
                    // is the right answer for the work as well as for the rows.
                    buckets: None,
                    buckets_built: false,
                    candidates: None,
                    materialized,
                }
            }
            // **Lazily**: only the arms are taken here, and the first is opened on the first
            // pull. Opening them all would read every arm's first chunk before a client has asked
            // for one row.
            Node::Append { arms } => Kind::Append {
                rest: arms.clone().into_iter(),
                current: None,
            },
            Node::Filter { input, predicate } => Kind::Filter {
                input: Box::new(Cursor::open(txn, tenant, settings, input)?),
                predicate: predicate.clone(),
            },
            Node::Project { input, exprs } => Kind::Project {
                input: Box::new(Cursor::open(txn, tenant, settings, input)?),
                exprs: exprs.clone(),
                pending: Vec::new().into_iter(),
            },
            Node::Sort { input, keys } => Kind::Sort {
                input: Some(Box::new(Cursor::open(txn, tenant, settings, input)?)),
                keys: keys.clone(),
                sorted: Vec::new().into_iter(),
            },
            Node::Aggregate { input, .. } => Kind::Aggregate {
                input: Some(Box::new(Cursor::open(txn, tenant, settings, input)?)),
                spec: node.clone(),
                groups: Vec::new().into_iter(),
            },
            // It computes nothing: the sub-select's plan already produced the rows, and the name
            // and column names this node carries are for `EXPLAIN`. Opening the input directly is
            // what makes that true rather than merely intended.
            Node::Derived { input, .. } => return Cursor::open(txn, tenant, settings, input),
            Node::Distinct { input } => Kind::Distinct {
                input: Box::new(Cursor::open(txn, tenant, settings, input)?),
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
                Some(_) => return Cursor::open(txn, tenant, settings, &columnar.fallback),
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
                input: Box::new(Cursor::open(txn, tenant, settings, input)?),
                to_skip: *offset,
                remaining: *limit,
            },
        };
        Ok(Cursor {
            txn,
            tenant,
            settings,
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
            settings: self.settings,
            catalog: Some(&self.catalog),
        };
        // The same three, for the one arm that opens a cursor of its own as it goes.
        let (txn, tenant, settings) = (self.txn, self.tenant, self.settings);
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
                        let row = row::decode_row(columns, &value, Some(&relation_namer(env)))?;
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
                let namer = relation_namer(env);
                point(self.txn, self.tenant, node, Some(&namer))
            }

            Kind::NestedLoop {
                lateral,
                outer,
                left_join,
                keep_right,
                outer_columns,
                matched_inner,
                draining,
                inner_table_id,
                inner_columns,
                probe,
                residual,
                current,
                matched,
                materialized,
                buckets,
                buckets_built,
                candidates,
            } => loop {
                // The outer side is finished and this is a full join: what is left is every
                // inner row nobody paired with, each in front of a row of NULLs.
                if let Some(at) = draining {
                    while let Some(seen) = matched_inner.get(*at) {
                        let inner = materialized.get(*at).cloned();
                        let unpaired = !*seen;
                        *at += 1;
                        if let Some(inner) = inner
                            && unpaired
                        {
                            let mut joined = vec![Datum::Null; *outer_columns];
                            joined.extend(inner);
                            return Ok(Some(joined));
                        }
                    }
                    return Ok(None);
                }
                let Some((row, position)) = current else {
                    let Some(next) = outer.next()? else {
                        if *keep_right {
                            *draining = Some(0);
                            continue;
                        }
                        return Ok(None);
                    };
                    // The `ON`'s ordinals split at the outer row's width, which this is the first
                    // point that knows.
                    if !*buckets_built {
                        *buckets_built = true;
                        *buckets = EquiBuckets::build(residual.as_ref(), materialized, next.len());
                    }
                    *candidates = buckets.as_ref().map(|buckets| buckets.candidates(&next));
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
                        let namer = relation_namer(env);
                        if let Some(inner) = point(self.txn, self.tenant, &node, Some(&namer))? {
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
                        // The bucket's positions when the `ON` gave one to group by, and every
                        // row otherwise — the same rows in the same order either way.
                        let at = match candidates {
                            Some(candidates) => candidates.get(*position).copied(),
                            None => Some(*position),
                        };
                        let Some(inner) = at.and_then(|at| materialized.get(at)) else {
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
                            // The index into `materialized`, which is what the drain reads back.
                            if let Some(seen) = at.and_then(|at| matched_inner.get_mut(at)) {
                                *seen = true;
                            }
                            return Ok(Some(joined));
                        }
                    }
                }
            },

            Kind::Append { rest, current } => {
                loop {
                    if let Some(arm) = current.as_mut()
                        && let Some(row) = arm.next()?
                    {
                        return Ok(Some(row));
                    }
                    // The arm is exhausted, or there is none yet: open the next one.
                    let Some(next) = rest.next() else {
                        return Ok(None);
                    };
                    *current = Some(Box::new(Cursor::open(txn, tenant, settings, &next)?));
                }
            }
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
            // `string_agg`'s delimiter, read from the same row as its value — see
            // `plan::AggregateFunc::StringAgg` for why it is not folded once.
            let delimiter = match &spec.delimiter {
                None => None,
                Some(expr) => Some(evaluate_in(expr, &row, env)?),
            };
            accumulator.push(&value, sort_key, delimiter.as_ref())?;
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
        for accumulator in &accumulators {
            row.push(accumulator.finish()?);
        }
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

fn point(
    txn: &dyn Txn,
    tenant: u64,
    node: &Node,
    name_of: Option<row::NameOfRelation<'_>>,
) -> Result<Option<Vec<Datum>>> {
    match node {
        Node::PointGet {
            table_id,
            columns,
            key,
        } => {
            let key = row::row_key(tenant, *table_id, key)?;
            txn.get(&key)?
                .map(|value| row::decode_row(columns, &value, name_of))
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
            let primary_key = row::decode_row(primary_key_types, &entry, name_of)?;
            let key = row::row_key(tenant, *table_id, &primary_key)?;
            match txn.get(&key)? {
                Some(value) => row::decode_row(columns, &value, name_of)
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
/// A relation's name as `::regclass::text` prints it for this session.
///
/// PostgreSQL qualifies a name exactly when its schema is **not** on the `search_path`, so the
/// same oid prints two ways in two sessions — which is what `DumpSchemasTest` turns on: the `:all`
/// case wants `test_schema.test_table` and the `test_schema` case wants `test_table`, from one
/// database.
fn qualified_for(
    settings: Settings<'_>,
    relation: &crate::catalog::pg_relations::RelationRow,
) -> String {
    if visible_schema(settings, &relation.schema) {
        return relation.name.clone();
    }
    crate::catalog::display_name(&crate::catalog::qualify(&relation.schema, &relation.name))
}

/// A **type's** name as `format_type` prints it for this session, which is the rule
/// [`qualified_for`] applies to a relation and not a second one.
///
/// Measured on 19beta1, one table and two enums, in one session and then a narrower one:
///
/// ```text
/// SET search_path = g1f_a, public    a g1f_a.mood    -> mood
///                                    b g1f_b.hidden  -> g1f_b.hidden
/// SET search_path = public           a g1f_a.mood    -> g1f_a.mood
///                                    b g1f_b.hidden  -> g1f_b.hidden
/// ```
///
/// It is what `ActiveRecord`'s schema dump reads as a column's `sql_type`, so a type printed
/// qualified where a real server prints it bare puts the schema inside
/// `t.enum "current_mood", enum_type: "…"` — which `enum_test` asserts bare while the
/// `create_enum` line above it, built from a different query, stays qualified.
fn type_qualified_for(settings: Settings<'_>, stored: &str) -> String {
    let (schema, bare) = crate::catalog::split_qualified(stored);
    if visible_schema(settings, schema) {
        return bare.to_owned();
    }
    crate::catalog::display_name(stored)
}

/// Whether a schema is one this session resolves a bare name in.
///
/// An **unknown** path is the default one, on which `public` sits — so a caller with no session
/// behind it prints an ordinary name bare and a schema-qualified one qualified, which is what
/// every statement outside a session wants.
fn visible_schema(settings: Settings<'_>, schema: &str) -> bool {
    if settings.search_path.is_empty() {
        return schema == crate::catalog::PUBLIC_SCHEMA;
    }
    settings.search_path.iter().any(|on_path| on_path == schema)
}

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
    /// What the session decides: the resolved `search_path` a catalog function prints a name
    /// against, and the `IntervalStyle` a value is rendered in.
    settings: Settings<'a>,
}

impl Env<'_> {
    /// No transaction: a caller that evaluates over a row and cannot start a query.
    pub(super) fn none() -> Self {
        Env {
            txn: None,
            tenant: 0,
            catalog: None,
            settings: Settings::none(),
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
            settings: Settings::none(),
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
/// The hstore operators and functions, over the canonical text a column holds.
///
/// **Every one is strict**: a NULL operand is a NULL answer, never an error and never a default,
/// which is what a real server does for all of them. Parsing the operand rather than keeping a map
/// beside the value is deliberate — an hstore *is* its canonical text here
/// (`crate::value::hstore`), so there is nothing else to keep, and a parse that fails is bytes this
/// node did not write.
///
/// Split out of [`catalog_function`] for the reason [`range_function`] is: one family, and that
/// function is long enough already.
/// Whether a `||` operand is **declared** `json` or `jsonb`, which is a question the values cannot
/// answer.
///
/// `jsonb` has no `Datum` of its own — it is a `Datum::Text`, where `hstore`, `ltree`, `citext`
/// and `tsvector` each have a variant — so by the time this operator has two values in hand a
/// document and a string are the same thing. Concatenating them is a **wrong answer** where
/// refusing is a gap, and [ADR 0031](../../../docs/adr/0031-the-rails-suite-is-the-measure.md)
/// ranks a wrong answer worse, so the declared type is read from the plan instead and the operator
/// gives back the `0A000` it gave before `||` over text existed.
///
/// A column is an [`Expr::Ordinal`], which carries its type for exactly this kind of question —
/// the same reason it carries `typmod` so a `character(3)` can be told from a `text`. A *literal*
/// cast is folded away before the executor sees it, so `'{"a":1}'::jsonb` is caught one layer up,
/// in `parse::lower`, where the `::jsonb` is still written down.
///
/// The real fix is `docs/plans/jsonb-representation.md`: `jsonb` gets a `Datum` and `||` becomes
/// document merge.
fn is_jsonb_typed(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Ordinal {
            ty: ColumnType::Jsonb,
            ..
        }
    )
}

/// `doc -> key` / `doc ->> key`, on a value whose text is already canonical.
///
/// A NULL on either side is NULL, which is what makes the operator usable on a nullable column
/// without a guard. A key that is neither a string nor an integer is the `42883` a real server
/// gives for an operator it has no overload of.
fn json_fetch(doc: Option<&Datum>, key: Option<&Datum>, as_text: bool) -> Result<Datum> {
    use crate::value::json;
    let (Some(doc), Some(key)) = (doc, key) else {
        return Ok(Datum::Null);
    };
    let text = match doc {
        Datum::Text(text) => text.as_str(),
        Datum::Null => return Ok(Datum::Null),
        other => {
            return Err(SqlError::UndefinedOperator {
                left: other
                    .column_type()
                    .map_or("unknown", PgType::name)
                    .to_owned(),
                op: if as_text { "->>" } else { "->" },
                right: "text".to_owned(),
            });
        }
    };
    let key = match key {
        Datum::Text(name) => json::Key::Member(name),
        Datum::Int8(at) => json::Key::At(*at),
        Datum::Int4(at) => json::Key::At(i64::from(*at)),
        Datum::Int2(at) => json::Key::At(i64::from(*at)),
        Datum::Null => return Ok(Datum::Null),
        other => {
            return Err(SqlError::UndefinedOperator {
                left: "jsonb".to_owned(),
                op: if as_text { "->>" } else { "->" },
                right: other
                    .column_type()
                    .map_or("unknown", PgType::name)
                    .to_owned(),
            });
        }
    };
    Ok(json::fetch(text, Some(&key), as_text)?.map_or(Datum::Null, Datum::Text))
}

/// `jsonb || jsonb`, on two values whose text is already canonical.
///
/// **Both** operands, never one: there is no `jsonb || text` on a real server, so a jsonb column
/// beside a text one is `text || text` and answers `{"a": 1}x` rather than merging or refusing.
///
/// Strict like every other `||`: a NULL operand is a NULL answer, which is what `value::json`'s
/// own rule cannot say because it never sees one.
fn jsonb_concat(left: Option<&Datum>, right: Option<&Datum>) -> Result<Datum> {
    let (Some(Datum::Text(left)), Some(Datum::Text(right))) = (left, right) else {
        return Ok(Datum::Null);
    };
    crate::value::json::concat(left, right).map(Datum::Text)
}

/// `text || anynonarray`, `anynonarray || text` and `text || text` — PostgreSQL's string
/// concatenation, and the plainest meaning of the symbol.
///
/// Three rules, measured on 19beta1 in one rolled-back session:
///
/// * **The answer is `text` whatever went in.** `'x'::varchar || 'y'::varchar` is `text`, not
///   `character varying` — `pg_typeof` says so — and a date, a numeric or a uuid contributes the
///   characters it prints as: `'d' || '2024-01-02'::date` is `d2024-01-02`.
/// * **It is strict.** `'a' || NULL` and `NULL || 'a'` are both NULL, which is exactly what
///   separates the operator from `concat()`, whose whole point is that it skips a NULL —
///   `concat('a', NULL, 'b')` is `ab`. The two are next to each other in the corpus for that.
/// * **At least one side has to be text.** `1 || 2` is not integer concatenation, it is
///   `42883 operator does not exist: integer || integer`, and `true || false` the same with
///   `boolean`. PostgreSQL has no `anynonarray || anynonarray`, so neither does this.
fn text_concat(left: Option<&Datum>, right: Option<&Datum>) -> Result<Datum> {
    let textual = |value: Option<&Datum>| {
        matches!(
            value.and_then(Datum::column_type),
            Some(ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar | ColumnType::Citext)
        )
    };
    // A NULL operand has no type to speak for it, so it cannot be the one that makes this string
    // concatenation — but it also cannot make it a type error, because on a real server the
    // *other* side's `text` is what resolves the operator and the answer is then NULL.
    let untyped = |value: Option<&Datum>| matches!(value, Some(Datum::Null) | None);
    if !textual(left) && !textual(right) && !untyped(left) && !untyped(right) {
        return Err(SqlError::UndefinedOperator {
            left: left
                .and_then(Datum::column_type)
                .map_or("unknown", PgType::name)
                .to_owned(),
            op: "||",
            right: right
                .and_then(Datum::column_type)
                .map_or("unknown", PgType::name)
                .to_owned(),
        });
    }
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(Datum::Null);
    };
    if matches!(left, Datum::Null) || matches!(right, Datum::Null) {
        return Ok(Datum::Null);
    }
    // **A boolean contributes `true`, not `t`.** `PgDatum::to_text` renders what a client is
    // *shown* — `t`/`f`, which is what psql prints — and `||` concatenates what the value casts
    // to, which for a boolean is the word. Measured: `'a' || true` is `atrue`.
    let cast = |value: &Datum| match value {
        Datum::Bool(flag) => Some((if *flag { "true" } else { "false" }).to_owned()),
        other => PgDatum::to_text(other),
    };
    let (Some(left), Some(right)) = (cast(left), cast(right)) else {
        return Ok(Datum::Null);
    };
    Ok(Datum::Text(left + &right))
}

fn hstore_function(func: crate::plan::CatalogFunc, args: &[Datum]) -> Result<Datum> {
    use crate::plan::CatalogFunc;
    use crate::value::hstore;

    // **Strict, and only strict.** A NULL operand is a NULL answer; an operand that is *not* NULL
    // and not an hstore is the refusal the operator already had, **not** a NULL — these operators
    // are spelled the same as several others (`@>` over arrays, `||` over text), so answering NULL
    // for a shape this function does not handle would turn another type's declared gap into a
    // silent wrong answer. `tests/array.rs` caught exactly that.
    let wrong_type = |value: Option<&Datum>| {
        SqlError::unsupported(format!(
            "{} over {}",
            func.name(),
            value
                .and_then(Datum::column_type)
                .map_or("unknown", PgType::name)
        ))
    };
    // **An `ltree` on either side takes this operator away from `hstore`.** `@>`, `<@` and `||`
    // are spelled the same for both, and the operand is the only place that can decide — the rule
    // the `||` regression taught, one type later. A bare `Datum::Text` beside an ltree is the
    // `unknown` literal, which is what `'a.b'::ltree || 'c'::text` is.
    if args.iter().any(|value| matches!(value, Datum::Ltree(_))) {
        let path = |value: Option<&Datum>| match value {
            Some(Datum::Ltree(text)) => Ok(Some(text.clone())),
            Some(Datum::Text(text)) => ltree::from_text(text).map(Some),
            Some(Datum::Null) | None => Ok(None),
            other => Err(wrong_type(other)),
        };
        return Ok(match (func, path(args.first())?, path(args.get(1))?) {
            (CatalogFunc::HstoreContains, Some(outer), Some(inner)) => {
                Datum::Bool(ltree::contains(&outer, &inner))
            }
            (CatalogFunc::HstoreConcat, Some(left), Some(right)) => {
                Datum::Ltree(ltree::concat(&left, &right))
            }
            (CatalogFunc::HstoreContains | CatalogFunc::HstoreConcat, _, _) => Datum::Null,
            _ => return Err(wrong_type(args.first())),
        });
    }
    // **At least one operand has to be a real hstore.** A `Datum::Text` is accepted only as the
    // `unknown` literal beside one — `h @> 'a=>b'` is how the suite writes containment — and never
    // on its own: `||` is spelled the same for text, and reading *both* sides as hstores turned
    // `title || $1` into `42601 syntax error in hstore` where a real server concatenates two
    // strings. An operator this crate carries for one type must not answer for another's.
    let anchored = args.iter().any(|value| matches!(value, Datum::Hstore(_)));
    let map = |value: Option<&Datum>| match value {
        Some(Datum::Hstore(text)) => hstore::from_text(text).map(Some),
        Some(Datum::Text(text)) if anchored => hstore::from_text(text).map(Some),
        Some(Datum::Null) | None => Ok(None),
        other => Err(wrong_type(other)),
    };
    let text = |value: Option<&Datum>| match value {
        Some(Datum::Text(text) | Datum::Citext(text)) => Ok(Some(text.clone())),
        Some(Datum::Null) | None => Ok(None),
        other => Err(wrong_type(other)),
    };
    Ok(match func {
        // `h -> k`: the value, or NULL for a key the hstore does not hold **and** for one whose
        // value is NULL — the two are indistinguishable through this operator, which is why `?`
        // exists.
        CatalogFunc::HstoreFetch => match (map(args.first())?, text(args.get(1))?) {
            (Some(map), Some(key)) => map
                .get(&hstore::Key(key))
                .cloned()
                .flatten()
                .map_or(Datum::Null, Datum::Text),
            _ => Datum::Null,
        },
        // `h ? k`: **true for a key whose value is NULL**, which is the whole difference from
        // `(h -> k) IS NOT NULL`.
        CatalogFunc::HstoreHasKey => match (map(args.first())?, text(args.get(1))?) {
            (Some(map), Some(key)) => Datum::Bool(map.contains_key(&hstore::Key(key))),
            _ => Datum::Null,
        },
        // `a @> b`: every pair of `b` is in `a`, values compared as they are — a NULL value in `b`
        // is contained only by a NULL value in `a`.
        CatalogFunc::HstoreContains => match (map(args.first())?, map(args.get(1))?) {
            (Some(left), Some(right)) => Datum::Bool(
                right
                    .iter()
                    .all(|(key, value)| left.get(key) == Some(value)),
            ),
            _ => Datum::Null,
        },
        // `a || b`: **the right wins a shared key**, which is the opposite of the first-wins rule
        // a repeated key inside one literal follows. Both measured.
        CatalogFunc::HstoreConcat => match (map(args.first())?, map(args.get(1))?) {
            (Some(mut left), Some(right)) => {
                left.extend(right);
                Datum::Hstore(hstore::to_text(&left))
            }
            _ => Datum::Null,
        },
        CatalogFunc::HstoreAkeys | CatalogFunc::HstoreAvals => match map(args.first())? {
            None => Datum::Null,
            Some(map) => {
                let wants_keys = func == CatalogFunc::HstoreAkeys;
                // **A NULL value is a NULL element**, and `akeys` never has one because a key is
                // never NULL. In canonical order, which is the map's own.
                let values: Vec<Option<Datum>> = map
                    .iter()
                    .map(|(key, value)| {
                        if wants_keys {
                            Some(Datum::Text(key.0.clone()))
                        } else {
                            value.clone().map(Datum::Text)
                        }
                    })
                    .collect();
                Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
                    ColumnType::Text,
                    1,
                    values,
                ))
            }
        },
        // `hstore(k, v)` — one pair — and `hstore(keys[], vals[])`, which pairs them by position.
        CatalogFunc::HstoreBuild => build_hstore(args.first(), args.get(1)),
        other => {
            return Err(SqlError::Internal(format!(
                "{} reached the hstore evaluator",
                other.name()
            )));
        }
    })
}

/// `hstore(k, v)` and `hstore(keys[], vals[])`.
///
/// Two shapes of one name, told apart by whether the arguments are arrays — which is how a real
/// server tells them apart too, by overload rather than by a different name.
fn build_hstore(left: Option<&Datum>, right: Option<&Datum>) -> Datum {
    use crate::value::hstore;

    let mut out = hstore::Hstore::new();
    match (left, right) {
        (Some(Datum::Array(keys)), Some(Datum::Array(values))) => {
            for (at, key) in keys.values.iter().enumerate() {
                let Some(Datum::Text(key)) = key else {
                    continue;
                };
                out.insert(
                    hstore::Key(key.clone()),
                    match values.values.get(at) {
                        Some(Some(Datum::Text(value))) => Some(value.clone()),
                        _ => None,
                    },
                );
            }
        }
        (Some(Datum::Text(key)), value) => {
            out.insert(
                hstore::Key(key.clone()),
                match value {
                    Some(Datum::Text(value)) => Some(value.clone()),
                    _ => None,
                },
            );
        }
        _ => return Datum::Null,
    }
    Datum::Hstore(hstore::to_text(&out))
}

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
                    other.column_type().map_or("unknown", PgType::name)
                ))),
            };
            // **A `Datum::Range` now that `daterange` is a type**, where it used to be text.
            // `pg_typeof(daterange(a, b))` is `daterange` on a real server, and a value that
            // carries its subtype is what makes it one here; the text is the same either way,
            // because `DateRange::to_text` is what writes it — and `new` is fallible now,
            // because bounds the wrong way round are `22000` rather than an empty range.
            Datum::Range {
                subtype: Box::new(ColumnType::Date),
                text: range::DateRange::new(day(args.first())?, day(args.get(1))?)?.to_text(),
            }
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

/// The range functions over a **stored** range column, which `range::Range` models
/// with its bounds' inclusivity as data.
///
/// Separate from [`range_function`], which answers the `daterange(a, b)` *expression* the suite's
/// `EXCLUDE` key is built from — that one has a half-open `DateRange` and no brackets to report.
/// The two will merge when the expression form is rebuilt on this type; until then each is right
/// about its own shape and neither is guessing about the other's.
fn range_value_function(func: crate::plan::CatalogFunc, args: &[Datum]) -> Result<Datum> {
    use crate::plan::CatalogFunc;

    let stored = |value: Option<&Datum>| match value {
        Some(Datum::Range { subtype, text }) => range::from_text(**subtype, text).map(Some),
        Some(Datum::Null) | None => Ok(None),
        other => Err(SqlError::UndefinedFunctionTypes(format!(
            "{}({})",
            func.name(),
            other
                .and_then(Datum::column_type)
                .map_or("unknown", PgType::name)
        ))),
    };
    Ok(match func {
        // **`lower_inf` is true for an *absent* bound and false for `-infinity`**, which is the
        // distinction the whole type turns on: an infinite timestamp is a value, and unbounded is
        // the lack of one.
        CatalogFunc::IsEmpty => match stored(args.first())? {
            None => Datum::Null,
            Some(range) => Datum::Bool(range.empty),
        },
        // Two ranges share a point. An empty range overlaps nothing, itself included.
        CatalogFunc::RangeOverlaps => match (stored(args.first())?, stored(args.get(1))?) {
            (Some(left), Some(right)) => Datum::Bool(overlaps(&left, &right)),
            _ => Datum::Null,
        },
        CatalogFunc::RangeLowerInc
        | CatalogFunc::RangeUpperInc
        | CatalogFunc::RangeLowerInf
        | CatalogFunc::RangeUpperInf => match stored(args.first())? {
            None => Datum::Null,
            Some(range) => Datum::Bool(match func {
                CatalogFunc::RangeLowerInc => range.lower_inc,
                CatalogFunc::RangeUpperInc => range.upper_inc,
                CatalogFunc::RangeLowerInf => !range.empty && range.lower.is_none(),
                _ => !range.empty && range.upper.is_none(),
            }),
        },
        // `a @> b`, and `b <@ a` is the same question with the arguments the other way round —
        // the lowering swaps them so there is one rule here. The right side may be a bare value
        // rather than a range, which is what `ts_range @> '…'::timestamp` sends.
        // **`a <@ b` over two ltrees arrives here**, because the lowering sends `<@` to the range
        // containment with its arguments swapped and `@>` to the hstore one. One symbol, three
        // types, and the operand is the only place that can tell them apart.
        CatalogFunc::RangeContains if args.iter().any(|value| matches!(value, Datum::Ltree(_))) => {
            hstore_function(CatalogFunc::HstoreContains, args)?
        }
        CatalogFunc::RangeContains => match (stored(args.first())?, args.get(1)) {
            (Some(outer), Some(inner)) => match inner {
                Datum::Null => Datum::Null,
                Datum::Range { subtype, text } => {
                    let inner = range::from_text(**subtype, text)?;
                    Datum::Bool(contains_range(&outer, &inner))
                }
                point => Datum::Bool(contains_point(&outer, point)),
            },
            _ => Datum::Null,
        },
        // `tsrange(a, b)` is `[a,b)` and the third argument names the brackets.
        CatalogFunc::RangeBuild => {
            let bounds = match args.get(2) {
                Some(Datum::Text(text)) => text.clone(),
                _ => "[)".to_owned(),
            };
            let mut chars = bounds.chars();
            // **A bare `'2026-01-01'` is an *unknown* literal**, which a real server reads at the
            // subtype rather than keeping as text — so the range prints
            // `["2026-01-01 00:00:00",…)` and not `[2026-01-01,…)`. Reading it here is what makes
            // the bound a value that can be compared, rather than characters that sort like one.
            let subtype = args
                .iter()
                .find_map(|arg| match arg {
                    Datum::Text(_) | Datum::Null => None,
                    other => other.column_type(),
                })
                .unwrap_or(ColumnType::Timestamp);
            let bound = |value: Option<&Datum>| -> Result<Option<Datum>> {
                match value {
                    None | Some(Datum::Null) => Ok(None),
                    Some(Datum::Text(text)) => {
                        Ok(Some(<Datum as PgDatum>::from_text(subtype, text)?))
                    }
                    Some(other) => Ok(Some(other.clone())),
                }
            };
            let mut range = range::Range {
                empty: false,
                lower: bound(args.first())?,
                upper: bound(args.get(1))?,
                lower_inc: chars.next() == Some('['),
                upper_inc: chars.next() == Some(']'),
            };
            // **The same normalisation the literal takes**, which is what makes the two spellings
            // one value: `tsrange(hi, lo)` is `22000 range lower bound must be less than or equal
            // to range upper bound` exactly as `'[hi,lo)'::tsrange` is, and a zero-width range
            // collapses to `empty` on both roads. Building the value without it answered an
            // impossible range object for one spelling and `22000` for the other.
            range::canonicalise(subtype, &mut range)?;
            Datum::Range {
                subtype: Box::new(subtype),
                text: range.to_text(),
            }
        }
        other => {
            return Err(SqlError::Internal(format!(
                "{} reached the range evaluator",
                other.name()
            )));
        }
    })
}

/// Whether two ranges share a point. An empty range overlaps nothing, itself included.
fn overlaps(left: &range::Range, right: &range::Range) -> bool {
    if left.empty || right.empty {
        return false;
    }
    // `left` starts before `right` ends, and `right` starts before `left` ends — with each
    // bound's inclusivity deciding the touching case.
    starts_before_end(left, right) && starts_before_end(right, left)
}

/// Whether `a`'s lower bound is below `b`'s upper, honouring both inclusivities.
fn starts_before_end(a: &range::Range, b: &range::Range) -> bool {
    let (Some(lower), Some(upper)) = (&a.lower, &b.upper) else {
        return true;
    };
    match lower.pg_cmp(upper) {
        Ordering::Less => true,
        Ordering::Equal => a.lower_inc && b.upper_inc,
        Ordering::Greater => false,
    }
}

/// Whether every point of `inner` is in `outer`. An empty range is contained by everything.
fn contains_range(outer: &range::Range, inner: &range::Range) -> bool {
    if inner.empty {
        return true;
    }
    if outer.empty {
        return false;
    }
    let lower_ok = match (&outer.lower, &inner.lower) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => match a.pg_cmp(b) {
            Ordering::Less => true,
            Ordering::Equal => outer.lower_inc || !inner.lower_inc,
            Ordering::Greater => false,
        },
    };
    let upper_ok = match (&outer.upper, &inner.upper) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => match a.pg_cmp(b) {
            Ordering::Greater => true,
            Ordering::Equal => outer.upper_inc || !inner.upper_inc,
            Ordering::Less => false,
        },
    };
    lower_ok && upper_ok
}

/// Whether a bare value falls inside a range, honouring each bound's inclusivity.
fn contains_point(outer: &range::Range, point: &Datum) -> bool {
    if outer.empty {
        return false;
    }
    let above_lower = match &outer.lower {
        None => true,
        Some(lower) => match lower.pg_cmp(point) {
            Ordering::Less => true,
            Ordering::Equal => outer.lower_inc,
            Ordering::Greater => false,
        },
    };
    let below_upper = match &outer.upper {
        None => true,
        Some(upper) => match upper.pg_cmp(point) {
            Ordering::Greater => true,
            Ordering::Equal => outer.upper_inc,
            Ordering::Less => false,
        },
    };
    above_lower && below_upper
}

/// A range argument, or `None` for NULL — and `42883` for a value that is not one.
fn range_argument(value: Option<&Datum>) -> Result<Option<range::DateRange>> {
    match value {
        Some(Datum::Null) | None => Ok(None),
        Some(Datum::Text(text)) => match range::DateRange::from_text(text) {
            Some(range) => Ok(Some(range)),
            None => Err(SqlError::UndefinedOperator {
                op: "&&",
                left: "text".to_owned(),
                right: "text".to_owned(),
            }),
        },
        Some(other) => Err(SqlError::UndefinedOperator {
            op: "&&",
            left: other
                .column_type()
                .map_or_else(|| "unknown".to_owned(), |ty| PgType::name(ty).to_owned()),
            right: "unknown".to_owned(),
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
        Datum::Text(text) | Datum::Citext(text) => Ok(Some(text.clone())),
        other => Err(SqlError::UndefinedOperator {
            op: "~~",
            left: other
                .column_type()
                .map_or_else(|| "unknown".to_owned(), |ty| PgType::name(ty).to_owned()),
            right: "unknown".to_owned(),
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
                .map_or_else(|| "unknown".to_owned(), |ty| PgType::name(ty).to_owned()),
            right: "unknown".to_owned(),
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
        // **The element type comes from the plan, not from the values.** Resolution settled it
        // once against the scope, so every row builds the same type of array — a row whose
        // elements all happen to be NULL must not produce a differently-typed array from the one
        // before it.
        Expr::Array { elements, element } => {
            let mut operands = Vec::with_capacity(elements.len());
            for expr in elements {
                operands.push(evaluate_in(expr, row, env)?);
            }
            // **An array of arrays is one array with another dimension**, which is what a real
            // server does and what `ArrayValue` already models — flat elements and `dims`. The
            // operands' own dimensions have to agree, a NULL array has none, and an operand that
            // is not an array at all never reaches here: mixing them is a type failure at
            // resolution (*ARRAY types integer[] and integer cannot be matched*).
            // **The declared type decides, not the values.** `ARRAY[NULL::int[]]` has no array
            // operand to look at and is still `{}` rather than `{NULL}`: what says so is the
            // element type being an array type, which resolution settled before a row was read.
            let stacking = element
                .and_then(esker_keys::array::ArrayValue::element_of)
                .is_some()
                || operands
                    .iter()
                    .any(|value| matches!(value, Datum::Array(_)));
            if stacking {
                stack_arrays(&operands, *element)?
            } else {
                let values = operands
                    .into_iter()
                    .map(|value| match value {
                        Datum::Null => None,
                        value => Some(value),
                    })
                    .collect();
                Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
                    element.unwrap_or(ColumnType::Text),
                    1,
                    values,
                ))
            }
        }
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
            // **A citext folds `LIKE` too**, which is the type's own rule rather than the
            // statement's: `'ABC'::citext LIKE 'abc'` is `t` where the same `LIKE` over `text` is
            // `f`, and `citext_test.rb` reads it. So `ILIKE` and a citext operand are two roads to
            // the same fold.
            let case_insensitive = &(*case_insensitive || matches!(subject, Datum::Citext(_)));
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
            // **`~` over an `ltree` is a *pattern* match and not a regular expression.** One
            // symbol, two languages, and the left operand is the only place that can tell them
            // apart — the rule `||` and `@>` already follow, a third time. `path ~ 'a.*'` is an
            // `lquery` and `title ~ 'a.*'` is POSIX, and the two answer differently for the same
            // characters.
            if let Datum::Ltree(path) = &subject {
                return Ok(match regex_text(&pattern_value, "~")? {
                    Some(pattern) => Datum::Bool(ltree::matches(path, &pattern)? != *negated),
                    None => Datum::Null,
                });
            }
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
            // **`lower` and `upper` are overloaded on a range**, which is how a real server spells
            // them too: over a string they fold case, over a range they are the bounds. The
            // subtype's value comes back, so `lower(ts_range)` is a `timestamp` — measured — and
            // an absent bound and an empty range are both NULL.
            // **`length(tsvector)` is the lexeme count, not a character count.** `length` is one
            // name over several types on a real server, told apart by the operand — the rule this
            // match already follows for a range's `lower`/`upper`. Measured: the four lexemes of
            // `'The Fat Cats ate a rat'`.
            Datum::TsVector(vector) if matches!(func, crate::plan::ScalarFunc::Length) => {
                Datum::Int4(
                    i32::try_from(crate::value::tsvector::from_text(&vector)?.len())
                        .unwrap_or(i32::MAX),
                )
            }
            Datum::Range { subtype, text } if !matches!(func, crate::plan::ScalarFunc::Abs) => {
                let range = range::from_text(*subtype, &text)?;
                match func {
                    crate::plan::ScalarFunc::Lower => range.lower.unwrap_or(Datum::Null),
                    _ => range.upper.unwrap_or(Datum::Null),
                }
            }
            // **A citext is binary-coercible to `text`**, so every text function takes one — and
            // the *result* is a `text`, not a citext: measured, `pg_typeof(lower('X'::citext))` is
            // `text`. The type survives a column, a cast and a comparison, and nothing else.
            Datum::Text(text) | Datum::Citext(text) => match func {
                crate::plan::ScalarFunc::Lower => Datum::Text(text.to_lowercase()),
                crate::plan::ScalarFunc::Upper => Datum::Text(text.to_uppercase()),
                // **By character, not by byte** — `reverse` on a multi-byte string has to keep
                // each code point whole, and reversing the bytes would not.
                crate::plan::ScalarFunc::Reverse => Datum::Text(text.chars().rev().collect()),
                // The one scalar function whose result is not its argument's type: the **first**
                // character's code point, and `0` for the empty string. Measured.
                crate::plan::ScalarFunc::Ascii => {
                    Datum::Int4(text.chars().next().map_or(0, |first| first as i32))
                }
                // Characters against bytes, and the two differ for anything non-ASCII.
                crate::plan::ScalarFunc::Length => {
                    Datum::Int4(i32::try_from(text.chars().count()).unwrap_or(i32::MAX))
                }
                crate::plan::ScalarFunc::OctetLength => {
                    Datum::Int4(i32::try_from(text.len()).unwrap_or(i32::MAX))
                }
                // Unreachable: the arm above catches `abs` before this one is tried.
                crate::plan::ScalarFunc::Abs => Datum::Text(text),
            },
            other => {
                // A non-text argument: `lower(1)` is `42883 function lower(integer) does not
                // exist` on a real server, not a cast. Measured.
                return Err(SqlError::UndefinedFunctionTypes(format!(
                    "{}({})",
                    func.name(),
                    other.column_type().map_or("unknown", PgType::name)
                )));
            }
        },
        // **The target's input function over the value's text**, which is PostgreSQL's own I/O
        // conversion for a cast with no binary function — and the reason a cast that folds at plan
        // time and one that runs per row give the same answer: both go through `from_text`.
        // Permission was decided at lowering, from `pg_cast`.
        Expr::Cast {
            operand,
            to,
            typmod,
        } => match evaluate_in(operand, row, env)? {
            Datum::Null => Datum::Null,
            // **A `regclass` to a number is the oid, not a text round trip.** Its text is the
            // relation's *name*, so the round trip would hand `rc` to an integer parser; a real
            // server's cast here is a binary coercion between two four-byte values and this one
            // is between an `i64` and whatever width was asked for.
            Datum::RegClass { oid, .. }
                if matches!(
                    to,
                    ColumnType::Oid | ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2
                ) =>
            {
                crate::value::assignment_cast(
                    Datum::Int8(oid),
                    *to,
                    crate::value::Rendering::default(),
                )?
            }
            // **A bit string and an integer convert, they do not round-trip through the text.**
            // `pg_cast` has `bit->integer`, `bit->bigint` and both backs as explicit casts by
            // function, and a function is what they are: the digits of `'101'::bit(3)` are a
            // *number written in base two*, so reading them as decimal answered `101` where a real
            // server says `5`. `value::bit` holds the two rules, each measured, and the typmod is
            // the width the integer is written in — which is why this cannot live in
            // `assignment_cast`, where there is no typmod to read. A `bit varying` reaches neither
            // arm: it has no numeric cast at all, and `parse::lower` refuses it before here.
            Datum::Bit {
                varying: false,
                bits,
            } if matches!(to, ColumnType::Int4 | ColumnType::Int8) => {
                let (width, name) = if *to == ColumnType::Int4 {
                    (32, "integer")
                } else {
                    (64, "bigint")
                };
                let value = crate::value::bit::to_integer(&bits, width, name)?;
                if *to == ColumnType::Int4 {
                    Datum::Int4(
                        i32::try_from(value)
                            .map_err(|_| SqlError::IntegerLiteralOutOfRange("integer"))?,
                    )
                } else {
                    Datum::Int8(value)
                }
            }
            // **A bare `bit` is `bit(1)`**, which is the grammar's rule and the one `lower_type`
            // already applies; `NO_TYPMOD` here means the cast was written without a length, so
            // the target is one bit wide and `5::int4::bit` is `1`.
            Datum::Int4(value) if *to == ColumnType::Bit => Datum::Bit {
                varying: false,
                bits: crate::value::bit::from_integer(
                    i64::from(value),
                    32,
                    u32::try_from(*typmod).unwrap_or(1).max(1),
                ),
            },
            Datum::Int8(value) if *to == ColumnType::Bit => Datum::Bit {
                varying: false,
                bits: crate::value::bit::from_integer(
                    value,
                    64,
                    u32::try_from(*typmod).unwrap_or(1).max(1),
                ),
            },
            // **A name that only exists per row** (`debts-v1.1.md` #41). `'x'::regclass` is
            // resolved once per statement, before the plan, by the pass that has a transaction; a
            // cast whose operand is a *value* cannot be, and answered `an oid is an integer, not
            // Text(…)` — a value arriving somewhere its type was decided without it. The rule is
            // the executor's, carried in rather than rewritten here, and the printed form comes
            // back through `regclass_of` so the search-path qualification is the same one every
            // other `regclass` gets.
            Datum::Text(name) if *to == ColumnType::RegClass => {
                let Some(names) = env.settings.names else {
                    return Err(SqlError::unsupported(
                        "a relation name read as a regclass without a catalog",
                    ));
                };
                regclass_of(env, names(&name)?)?
            }
            // **An array to `regclass[]` resolves every element**, because the type is a name per
            // element and the names come from the catalog. `array_in` cannot do it — the input
            // function of a `regclass` needs a relation lookup and `crate::value` has none — so it
            // is done here, where the row evaluator already has `env`, and by the same
            // `regclass_of` the scalar direction uses so the two cannot disagree about a name.
            Datum::Array(mut values) if *to == ColumnType::RegClassArray => {
                for value in &mut values.values {
                    if let Some(element) = value
                        && let Some(oid) = oid_argument(Some(element))?
                    {
                        *element = regclass_of(env, oid)?;
                    }
                }
                values.element = ColumnType::RegClass;
                Datum::Array(values)
            }
            // **And the inverse, element by element**: `regclass[]::oid[]` is the numbers. It is
            // the same `stored_shape` the scalar direction uses, so the two cannot disagree about
            // what an oid past four bytes is; going through the text handed `pg_class` to `oidin`,
            // which is `22P02` for a statement a real server answers `{1259}`.
            Datum::Array(mut values)
                if matches!(
                    values.element,
                    ColumnType::RegType | ColumnType::RegProc | ColumnType::RegClass
                ) && let Some(element) = esker_keys::array::ArrayValue::element_of(*to) =>
            {
                for datum in values.values.iter_mut().flatten() {
                    *datum = crate::value::stored_shape(
                        datum.clone(),
                        element,
                        crate::value::Rendering::default(),
                    )?;
                }
                values.element = element;
                Datum::Array(values)
            }
            // **A `regtype` or a `regproc` to a number is the oid too**, for the same reason and
            // with one difference: their oid is already four bytes. Without this arm
            // `typinput::oid` rendered `boolin` and handed it to `oidin`, which is
            // `22P02 invalid input syntax for type oid: "boolin"` for a statement a real server
            // answers with 1242 — `pg_cast` calls the pair implicit and method `b`, a
            // reinterpretation, and a reinterpretation is what this is (ADR 0098).
            Datum::RegType { oid, .. } | Datum::RegProc { oid, .. }
                if matches!(
                    to,
                    ColumnType::Oid | ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2
                ) =>
            {
                crate::value::assignment_cast(
                    Datum::Oid(oid),
                    *to,
                    crate::value::Rendering::default(),
                )?
            }
            // **A `numeric` to an integer rounds; it does not go through text.** `numeric`'s
            // output function writes `2.5` and `int4in` refuses it, so the round trip made a
            // conversion a real server performs into a `22P02`. It became reachable when a lossy
            // cast started keeping its node and converting per row (`debts-v1.1.md` #30) — before
            // that the fold did it at parse time and nothing asked the evaluator.
            value @ Datum::Numeric(_)
                if matches!(to, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8) =>
            {
                crate::value::assignment_cast(value, *to, crate::value::Rendering::default())?
            }
            value => {
                // **The session's output function, not the boot one.** A cast between two types
                // here is a text round trip, so the text it goes through has to be the text the
                // session would see: under `SET TimeZone = 'Pacific/Auckland'`,
                // `'2011-01-01 23:30:00+00'::timestamptz::date` is `2011-01-02` on a real server
                // and was `2011-01-01` here, because the round trip rendered the instant in UTC
                // and the date parser read the day off it. Measured
                // (`tests/captures/pg19_time_zone.txt`), and the same reasoning `ToText` below
                // already applied for an `interval`'s dialect.
                let text = crate::value::to_text_under(&value, env.settings.rendering)
                    .ok_or_else(|| SqlError::DatatypeMismatch("a value with no text".to_owned()))?;
                // The modifier the cast wrote, applied the way a column's is: `$1::varchar(3)`
                // bounds the string exactly as a `varchar(3)` column would.
                crate::value::truncate_to_typmod(Datum::from_text(*to, &text)?, *to, *typmod)?
            }
        },
        Expr::ToText {
            operand,
            strip_blanks,
            enum_labels,
        } => match evaluate_in(operand, row, env)? {
            Datum::Null => Datum::Null,
            // An enum's output function is its label, which is a lookup and not a rendering.
            Datum::Int2(ordinal) if enum_labels.is_some() => {
                match enum_labels
                    .as_deref()
                    .and_then(|labels| crate::catalog::enum_label(labels, ordinal))
                {
                    Some(label) => Datum::Text(label.to_owned()),
                    None => Datum::Null,
                }
            }
            // **A boolean is the one type whose cast is not its output function.** `SELECT true`
            // prints `t` and `SELECT true::text` is `true`; PostgreSQL has a separate `booltext`
            // for the cast. Measured — every other type here casts to exactly what it prints.
            Datum::Bool(flag) => Datum::Text(if flag { "true" } else { "false" }.to_owned()),
            value => {
                // **The output function, which is the session's for an `interval`** — the same
                // rule the `SELECT` funnel applies to a column, applied here so that
                // `SELECT term` and `SELECT term::text` cannot answer in two dialects in one
                // session. Measured on 19beta1: under `iso_8601` the cast, `||`, `format`,
                // `::varchar`, `array_to_string` and `jsonb_build_object` all say `P1Y`.
                let text =
                    crate::value::to_text_under(&value, env.settings.rendering).unwrap_or_default();
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
            operand,
            branches,
            otherwise,
        } => {
            // **The simple form's operand is evaluated once**, not once per branch: a real server
            // evaluates `CASE`'s `arg` a single time, and a volatile one would otherwise be a
            // different value at every `WHEN`. `None` here is the searched form, whose `WHEN` is
            // the condition itself.
            let subject = match operand {
                Some(operand) => Some(evaluate_in(operand, row, env)?),
                None => None,
            };
            let mut answer = Datum::Null;
            for branch in branches {
                // **`=`, and not `IS NOT DISTINCT FROM`.** A NULL on either side makes the
                // comparison unknown and the branch is not taken, so
                // `CASE NULL WHEN NULL THEN 1 ELSE 2 END` is `2` — measured, and the same rule the
                // `Binary` arm above applies to every other comparison. `pg_cmp` is the type's own
                // equality, so `1.0` and `1.00` match.
                if let Some(subject) = &subject {
                    let value = evaluate_in(&branch.when, row, env)?;
                    let same = !matches!(subject, Datum::Null)
                        && !matches!(value, Datum::Null)
                        && subject.pg_cmp(&value).is_eq();
                    if same {
                        return evaluate_in(&branch.then, row, env);
                    }
                    continue;
                }
                match evaluate_in(&branch.when, row, env)? {
                    Datum::Bool(true) => return evaluate_in(&branch.then, row, env),
                    Datum::Bool(false) | Datum::Null => {}
                    other => {
                        // Caught where the expression is resolved for every shape whose type is
                        // known then; this is the one that is not — an `unknown` condition, whose
                        // type nothing gives it.
                        return Err(SqlError::DatatypeMismatch(format!(
                            "argument of CASE/WHEN must be type boolean, not type {}",
                            other.column_type().map_or("unknown", PgType::name)
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
        // **Which function it is decides which UUID it is**, and the four namespaces are
        // constants: RFC 4122's own, byte for byte, and the same numbers a real server answers.
        Expr::Uuid(func) => Datum::Uuid(match func {
            crate::plan::UuidFunc::GenRandomUuid | crate::plan::UuidFunc::UuidGenerateV4 => {
                crate::value::random::uuid_v4()?
            }
            crate::plan::UuidFunc::UuidGenerateV1 | crate::plan::UuidFunc::UuidGenerateV1Mc => {
                let Some(txn) = env.txn else {
                    return Err(SqlError::Internal(format!(
                        "{}() reached an evaluator with no transaction",
                        func.name()
                    )));
                };
                // The transaction's instant, from the same place `now()` reads it — invariant 6
                // says the TSO's physical half is the only clock this node may read, and a
                // version-1 UUID is a timestamp. Two calls in one transaction are told apart by
                // the tick counter, not by the clock.
                crate::value::random::uuid_v1(
                    crate::time_machine::micros_of_ts(txn.start_ts()),
                    matches!(func, crate::plan::UuidFunc::UuidGenerateV1Mc),
                )?
            }
            crate::plan::UuidFunc::UuidNil => [0; 16],
            crate::plan::UuidFunc::UuidNsDns => {
                crate::value::uuid::from_text("6ba7b810-9dad-11d1-80b4-00c04fd430c8")?
            }
            crate::plan::UuidFunc::UuidNsUrl => {
                crate::value::uuid::from_text("6ba7b811-9dad-11d1-80b4-00c04fd430c8")?
            }
            crate::plan::UuidFunc::UuidNsOid => {
                crate::value::uuid::from_text("6ba7b812-9dad-11d1-80b4-00c04fd430c8")?
            }
            // **`814`, not `813`.** RFC 4122 skips one: the X.500 namespace is `…814…` and there
            // is no `…813…`. Measured, because it is exactly the digit a reader would fill in.
            crate::plan::UuidFunc::UuidNsX500 => {
                crate::value::uuid::from_text("6ba7b814-9dad-11d1-80b4-00c04fd430c8")?
            }
        }),
        Expr::Literal(Literal::Null | Literal::TypedNull(_)) => Datum::Null,
        Expr::Literal(Literal::Bool(value)) => Datum::Bool(*value),
        Expr::Literal(Literal::Integer(value)) => Datum::Int8(*value),
        // **A `numeric`, not a `float8`.** The scale a literal is written with is part of its
        // value — `1.10` is not `1.1` — and reading it as a float threw that away before anything
        // could ask.
        Expr::Literal(Literal::Decimal(digits)) => Datum::from_text(ColumnType::Numeric, digits)?,
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
        Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentUser
        | Expr::CurrentSetting { .. }
        | Expr::Advisory { .. } => {
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
            let operand = sub
                .operands
                .iter()
                .map(|operand| evaluate_in(operand, row, env))
                .collect::<Result<Vec<_>>>()?;
            match (sub.correlated, env.txn) {
                // Uncorrelated: its rows were produced before this cursor was opened, by
                // `crate::exec::subquery::resolve`.
                (false, _) => crate::exec::subquery::value(sub, &operand)?,
                // Correlated: a different answer for this row, so it runs now. The nested loop
                // this makes is the shape, not an accident (`docs/plans/phase-12-subquery.md` §1).
                (true, Some(txn)) => {
                    let values = crate::exec::subquery::run_correlated(sub, row, txn, env.tenant)?;
                    crate::exec::subquery::value_of(
                        sub.kind,
                        &operand,
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
            any: _,
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
        Expr::QuantifiedArray {
            operand,
            op,
            all,
            array,
        } => {
            let operand = evaluate_in(operand, row, env)?;
            let array_value = evaluate_in(array, row, env)?;
            // **An array whose element type already *is* the operand's is used as it stands.**
            // The text path below re-reads every element through the operand type's input
            // function, which was right while an array was text and is wrong for a type whose
            // input function needs something the evaluator cannot hand it: `regclassin` is a
            // catalog lookup, so `'rc'::regclass = ANY('{rc}'::regclass[])` — a statement whose
            // two sides are already the right type — came back `0A000` for want of a catalog.
            // Rendering a datum and reading it back is a round trip, and a round trip is only
            // ever as good as the pair of functions it goes through.
            if let Datum::Array(value) = &array_value
                && value.element == operand.column_type().unwrap_or(ColumnType::Text)
            {
                let values: Vec<Datum> = value
                    .values
                    .iter()
                    .map(|element| element.clone().unwrap_or(Datum::Null))
                    .collect();
                return Ok(crate::exec::subquery::quantified_over(
                    *op, *all, &operand, &values,
                ));
            }
            let Some(array) = read_array(&array_value)? else {
                // A NULL array, which is not an empty one: `1 = ANY(NULL::int[])` is NULL where
                // `1 = ANY('{}')` is false. Measured, both.
                return Ok(Datum::Null);
            };
            // Each element is read **as the operand's type**, which is the same rule the plan-time
            // form uses: an array's elements have no type of their own here, and what gives them
            // one is what they are being compared against.
            //
            // **A NULL operand is decided here and not before the array is read**, which is the
            // one edge that made this arm wrong for as long as it existed:
            // `NULL = ANY (ARRAY[]::integer[])` is `f` and `NULL = ALL (…)` of an empty array is
            // `t`, because an empty array settles the quantifier with no comparison at all —
            // measured, and the subquery form's own doc has said so since it was written. The old
            // `if operand is NULL { return NULL }` above ran first and answered NULL.
            let ty = operand.column_type().unwrap_or(ColumnType::Text);
            let mut values = Vec::with_capacity(array.elements.len());
            for element in &array.elements {
                values.push(match element {
                    Some(text) => Datum::from_text(ty, text)?,
                    None => Datum::Null,
                });
            }
            crate::exec::subquery::quantified_over(*op, *all, &operand, &values)
        }

        Expr::Binary { op, left, right } => {
            let (left, right) = (evaluate_in(left, row, env)?, evaluate_in(right, row, env)?);
            match op {
                // Three-valued AND and OR, and they are not symmetric: a definite `false` makes an
                // AND false whatever the other side is, and a definite `true` makes an OR true.
                BinaryOp::And => match (truth_of(&left, "AND")?, truth_of(&right, "AND")?) {
                    (Some(false), _) | (_, Some(false)) => Datum::Bool(false),
                    (Some(true), Some(true)) => Datum::Bool(true),
                    _ => Datum::Null,
                },
                BinaryOp::Or => match (truth_of(&left, "OR")?, truth_of(&right, "OR")?) {
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
                    // **`point <>` is the one comparison a `point` has, and it is fuzzy.**
                    // `point_ne` is `|ax-bx| > 1e-6 || |ay-by| > 1e-6`, so two points differing by
                    // `1e-6` are not different and by `1e-5` they are; and every comparison
                    // against a `NaN` is false, so a `NaN` is not different from itself. Neither
                    // is what `Datum`'s own `PartialEq` for a `point` does — that is bitwise, and
                    // its comment says why it is not a SQL equality. `point =` does not exist at
                    // all and is refused at resolution, so this arm answers `<>` and nothing else.
                    if let (
                        Datum::Point { x: ax, y: ay },
                        Datum::Point { x: bx, y: by },
                        BinaryOp::NotEq,
                    ) = (&left, &right, comparison)
                    {
                        const EPSILON: f64 = 1.0e-6;
                        return Ok(Datum::Bool(
                            (ax - bx).abs() > EPSILON || (ay - by).abs() > EPSILON,
                        ));
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

/// Stack array operands into one array with another dimension, for an `ARRAY[…]` constructor.
///
/// The mechanism is `ArrayValue::stacked`; what belongs here is the **sentence**. PostgreSQL has
/// two for one cause and raises them from two functions, so this one is the constructor's and
/// `array_agg`'s is its own — a shared message would be wrong about half of the family.
fn stack_arrays(operands: &[Datum], element: Option<ColumnType>) -> Result<Datum> {
    let parts: Vec<Option<&esker_keys::array::ArrayValue>> = operands
        .iter()
        .map(|value| match value {
            Datum::Array(array) => Some(array),
            _ => None,
        })
        .collect();
    // The declared type is the *array*'s, so the fallback element is what it is an array of —
    // needed only when every operand is NULL and no value can say.
    let fallback = element
        .and_then(esker_keys::array::ArrayValue::element_of)
        .or(element)
        .unwrap_or(ColumnType::Text);
    esker_keys::array::ArrayValue::stacked(&parts, fallback)
        .map(Datum::Array)
        .ok_or(SqlError::ArrayExpressionDimensions)
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
/// The text-search functions, every answer measured against PostgreSQL 19beta1.
///
/// **Strict**: a NULL argument gives NULL, which is what a real server does and what keeps
/// `name_vector @@ to_tsquery(…)` from claiming a match on a row that has no vector.
fn text_search_function(func: crate::plan::CatalogFunc, args: &[Datum]) -> Result<Datum> {
    use crate::plan::CatalogFunc;
    use crate::value::{tsquery, tsvector};

    // One argument means `default_text_search_config`, which this node reports as
    // `pg_catalog.english`; two name the configuration.
    let (config, subject) = match (args.first(), args.get(1)) {
        (Some(Datum::Text(name)), Some(subject)) => (tsvector::Config::resolve(name)?, subject),
        // Everything else takes the default configuration and reads the subject from the first
        // argument — a one-argument call, and the two-argument ones whose first operand is the
        // value rather than a configuration name (`setweight`, `@@`).
        (Some(subject), _) => (tsvector::Config::English, subject),
        (None, _) => return Ok(Datum::Null),
    };
    let text_of = |datum: &Datum| match datum {
        Datum::Text(text) => Some(text.clone()),
        _ => None,
    };

    Ok(match func {
        CatalogFunc::ToTsVector => match text_of(subject) {
            Some(text) => Datum::TsVector(tsvector::to_text(&tsvector::to_tsvector(config, &text))),
            None => Datum::Null,
        },
        CatalogFunc::ToTsQuery => match text_of(subject) {
            Some(text) => query_datum(tsquery::to_tsquery(config, &text)?),
            None => Datum::Null,
        },
        CatalogFunc::PlainToTsQuery => match text_of(subject) {
            Some(text) => query_datum(tsquery::plainto_tsquery(config, &text)),
            None => Datum::Null,
        },
        CatalogFunc::PhraseToTsQuery => match text_of(subject) {
            Some(text) => query_datum(tsquery::phraseto_tsquery(config, &text)),
            None => Datum::Null,
        },
        CatalogFunc::WebsearchToTsQuery => match text_of(subject) {
            Some(text) => query_datum(tsquery::websearch_to_tsquery(config, &text)),
            None => Datum::Null,
        },
        // **`@@` exists in both argument orders**, so the operands decide which is the vector and
        // which the query rather than the position doing it.
        CatalogFunc::TsMatch => match (args.first(), args.get(1)) {
            (Some(Datum::TsVector(vector)), Some(Datum::TsQuery(query)))
            | (Some(Datum::TsQuery(query)), Some(Datum::TsVector(vector))) => Datum::Bool(
                tsquery::matches(&tsvector::from_text(vector)?, &tsquery::from_text(query)?),
            ),
            _ => Datum::Null,
        },
        CatalogFunc::TsStrip => match args.first() {
            Some(Datum::TsVector(vector)) => {
                let mut lexemes = tsvector::from_text(vector)?;
                for lexeme in &mut lexemes {
                    lexeme.positions.clear();
                }
                Datum::TsVector(tsvector::to_text(&lexemes))
            }
            _ => Datum::Null,
        },
        CatalogFunc::SetWeight => match (args.first(), args.get(1)) {
            (Some(Datum::TsVector(vector)), Some(Datum::Text(letter))) => {
                let weight = tsvector::Weight::of(letter.chars().next().unwrap_or('D'))
                    .ok_or_else(|| SqlError::InvalidTextRepresentation {
                        ty: "\"char\"",
                        value: letter.clone(),
                    })?;
                let mut lexemes = tsvector::from_text(vector)?;
                for lexeme in &mut lexemes {
                    for position in &mut lexeme.positions {
                        position.1 = weight;
                    }
                }
                Datum::TsVector(tsvector::to_text(&lexemes))
            }
            _ => Datum::Null,
        },
        // `ts_headline(config, text, query)` — three arguments, so the config/subject split above
        // does not describe it and it reads its own.
        CatalogFunc::TsHeadline => {
            let (config, text, query) = match (args.first(), args.get(1), args.get(2)) {
                (Some(Datum::Text(name)), Some(Datum::Text(text)), Some(Datum::TsQuery(query))) => {
                    (tsvector::Config::resolve(name)?, text, query)
                }
                (Some(Datum::Text(text)), Some(Datum::TsQuery(query)), None) => {
                    (tsvector::Config::English, text, query)
                }
                _ => return Ok(Datum::Null),
            };
            let lexemes = tsquery::lexemes(&tsquery::from_text(query)?);
            Datum::Text(tsvector::headline(config, text, &lexemes))
        }
        CatalogFunc::TsRank => match (args.first(), args.get(1)) {
            (Some(Datum::TsVector(vector)), Some(Datum::TsQuery(query))) => Datum::Real(
                tsquery::rank(&tsvector::from_text(vector)?, &tsquery::from_text(query)?),
            ),
            _ => Datum::Null,
        },
        CatalogFunc::NumNode => match args.first() {
            Some(Datum::TsQuery(query)) => Datum::Int4(
                i32::try_from(tsquery::numnode(&tsquery::from_text(query)?)).unwrap_or(i32::MAX),
            ),
            _ => Datum::Null,
        },
        other => {
            return Err(SqlError::Internal(format!(
                "{} reached the text-search evaluator",
                other.name()
            )));
        }
    })
}

/// **A query that is nothing but stop words is not an error and not a NULL** — it is the empty
/// tsquery, which prints as nothing and matches nothing. Measured: `plainto_tsquery('english',
/// 'the a of')` answers an empty value with a `NOTICE`, not a failure.
fn query_datum(query: Option<crate::value::tsquery::Node>) -> Datum {
    match query {
        Some(node) => Datum::TsQuery(crate::value::tsquery::to_text(&node)),
        None => Datum::TsQuery(String::new()),
    }
}

/// `date_trunc(unit, value)` and `date_trunc(unit, value, zone)`.
///
/// **Which of the three types the value is decides the answer's type**, so this is also where the
/// zone question is settled: a `timestamptz` is cut in the session's zone, and the three-argument
/// form cuts in the zone it names instead — including when its value is an unzoned `timestamp`,
/// which a real server casts to `timestamptz` before doing anything else.
///
/// Strict in both arguments, measured: either one NULL is NULL, and the unit is not looked at when
/// the value is missing.
fn date_trunc(args: &[Datum], session: Option<&'static crate::value::zone::Zone>) -> Result<Datum> {
    use crate::value::trunc;

    let (Some(unit), Some(value)) = (args.first(), args.get(1)) else {
        return Ok(Datum::Null);
    };
    let (Datum::Text(spelling), false) = (unit, matches!(value, Datum::Null)) else {
        return Ok(Datum::Null);
    };

    // The zone the third argument names, resolved through the same table `SET TimeZone` uses. A
    // name it does not hold is `22023` — a different sentence from the one `SET` gives, measured.
    let named = match args.get(2) {
        None => None,
        Some(Datum::Null) => return Ok(Datum::Null),
        Some(zone) => {
            let name = zone.to_text().unwrap_or_default();
            Some(
                crate::value::zone::Zone::shared(&name)
                    .ok_or(SqlError::TimeZoneNotRecognized(name))?,
            )
        }
    };

    // The type's own name, which both refusals quote.
    let ty = match (value, named) {
        (_, Some(_)) | (Datum::TimestampTz(_) | Datum::Date(_), None) => "timestamp with time zone",
        (Datum::Interval { .. }, None) => "interval",
        _ => "timestamp without time zone",
    };
    let unit = match trunc::lookup(spelling) {
        trunc::Lookup::Field(unit) => unit,
        trunc::Lookup::Inapplicable => return Err(trunc::not_supported(spelling, ty, "")),
        trunc::Lookup::Unknown => return Err(trunc::not_recognized(spelling, ty)),
    };

    Ok(match value {
        Datum::Interval {
            months,
            days,
            micros,
        } => {
            let (months, days, micros) = trunc::interval(*months, *days, *micros, unit, spelling)?;
            Datum::Interval {
                months,
                days,
                micros,
            }
        }
        // **A `date` resolves to the `timestamptz` overload**, not the unzoned one, so it is cut
        // in a zone and comes back zoned. Measured.
        // Through `as_micros`, which knows the two infinities: `date_trunc('day', 'infinity'::date)`
        // is `infinity` on a real server, where the bare multiply overflowed.
        Datum::Date(days) => Datum::TimestampTz(trunc::timestamptz(
            crate::value::date::as_micros(*days),
            unit,
            named.or(session),
        )),
        Datum::TimestampTz(micros) => {
            Datum::TimestampTz(trunc::timestamptz(*micros, unit, named.or(session)))
        }
        // A `timestamp` under a named zone is that zone's `timestamptz`; without one it is cut
        // where it is written, and the session's `TimeZone` does not reach it.
        Datum::Timestamp(micros) => match named {
            Some(zone) => Datum::TimestampTz(trunc::timestamptz(*micros, unit, Some(zone))),
            None => Datum::Timestamp(trunc::timestamp(*micros, unit)),
        },
        // **A value none of the overloads take is `42883` naming the signature**, which is what a
        // real server answers at resolution: `date_trunc('day', 42)` is
        // `function date_trunc(unknown, integer) does not exist`. Answering NULL here instead would
        // be the worst class of divergence — a value where PostgreSQL raises. The unit is spelled
        // `unknown` because that is what an unadorned literal resolves to, and a literal is what a
        // client writes.
        other => {
            return Err(SqlError::UndefinedFunctionTypes(format!(
                "date_trunc(unknown, {})",
                other.column_type().map_or("text", PgType::name)
            )));
        }
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "one dispatch over the whole catalog-function vocabulary; splitting it would put \
              half the vocabulary somewhere else, which is the argument `esker_keys::row`'s own \
              type match makes"
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
        // **Reached only for the types that *have* `~=` on a real server**, since resolution has
        // already refused the rest with PostgreSQL's own sentence. A geometric value carries its
        // own `ColumnType`, so the shape decides the rule: a polygon is the same as its own
        // rotation and its own reversal, and everything else is its vertices in order.
        CatalogFunc::SameAs => match (args.first(), args.get(1)) {
            (
                Some(Datum::Geometry { kind, text: left }),
                Some(Datum::Geometry { text: right, .. }),
            ) => crate::value::geometric::same_as(**kind, left, right)
                .map_or(Datum::Null, Datum::Bool),
            (Some(Datum::Point { x: ax, y: ay }), Some(Datum::Point { x: bx, y: by })) => {
                Datum::Bool(crate::value::geometric::same_point((*ax, *ay), (*bx, *by)))
            }
            (Some(Datum::Null), _) | (_, Some(Datum::Null)) => Datum::Null,
            _ => return Err(SqlError::unsupported("the operator ~=")),
        },
        // **Polygon containment and overlap**, over the canonical text the type stores — the
        // predicates and the epsilon they share live in `crate::value::geometric`. A value this
        // pass cannot read as a ring is NULL rather than a wrong answer.
        CatalogFunc::PolygonContains | CatalogFunc::PolygonOverlaps => {
            match (args.first(), args.get(1)) {
                // **A geometric value carries its own type**, unlike a `jsonb`: `Datum::Geometry`
                // holds the `ColumnType` beside the canonical text. The rewrite at resolution is
                // still what picks this function — `@>` is spelled the same for four types — but
                // the value needs no help saying which it is.
                (
                    Some(Datum::Geometry { text: left, .. }),
                    Some(Datum::Geometry { text: right, .. }),
                ) => {
                    let answer = if call.func == CatalogFunc::PolygonContains {
                        crate::value::geometric::polygon_contains(left, right)
                    } else {
                        crate::value::geometric::polygons_overlap(left, right)
                    };
                    answer.map_or(Datum::Null, Datum::Bool)
                }
                _ => Datum::Null,
            }
        }
        // **A `jsonb` containment.** Both operands are the canonical text the type stores, so
        // re-parsing is faithful. NULL in, NULL out.
        CatalogFunc::JsonbContains => match (args.first(), args.get(1)) {
            (Some(Datum::Text(left)), Some(Datum::Text(right))) => {
                Datum::Bool(crate::value::json::contains(left, right)?)
            }
            _ => Datum::Null,
        },
        // **A `jsonb` comparison, as `-1`, `0` or `1`.** Both operands are the canonical text the
        // type stores, so re-parsing them is faithful — `crate::value::json::canonicalise` ran on
        // the way in. NULL in, NULL out, as every comparison is.
        CatalogFunc::JsonbCompare => match (args.first(), args.get(1)) {
            (Some(Datum::Text(left)), Some(Datum::Text(right))) => {
                Datum::Int4(match crate::value::json::compare(left, right)? {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                })
            }
            _ => Datum::Null,
        },
        // **Sleeps in short steps and checks between them.** A single `sleep` for the whole
        // duration would ignore `statement_timeout` and a cancel until it was over, and being
        // interruptible is the entire reason this node has `pg_sleep` — it is how a test makes a
        // statement that is *working* rather than waiting (`tests/statement_cancellation.rs`).
        //
        // NULL in, NULL out: `pg_sleep` is strict on a real server and sleeps for nothing.
        CatalogFunc::PgSleep => {
            let Some(seconds) = args
                .first()
                .and_then(Datum::to_text)
                .and_then(|text| text.trim().parse::<f64>().ok())
            else {
                return Ok(Datum::Null);
            };
            if seconds.is_finite() && seconds > 0.0 {
                let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
                loop {
                    super::cancel::check()?;
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    // A short step so a cancel is acted on promptly rather than at the end.
                    std::thread::sleep(left.min(std::time::Duration::from_millis(10)));
                }
            }
            // `void`, which prints as the empty string.
            Datum::Text(String::new())
        }
        // **Which session is asking.** The pid rides on the thread with the cancellation flag,
        // installed by `Executor::execute`, because the expression evaluator is handed a
        // transaction and a catalog and has no other way to know.
        CatalogFunc::PgBackendPid => super::cancel::current_pid()
            .and_then(|pid| i32::try_from(pid).ok())
            .map_or(Datum::Null, Datum::Int4),
        // **Cancels another session's statement, or this one's.**
        //
        // Cancelling yourself stops the statement that asked, measured against PG19: probing
        // `pg_cancel_backend(pg_backend_pid())` answered `canceling statement due to user request`
        // rather than a row. So the flag is set and then read straight back, which is what turns a
        // self-cancel into an error here instead of a value.
        CatalogFunc::PgCancelBackend => {
            let Some(pid) = args
                .first()
                .and_then(Datum::to_text)
                .and_then(|text| text.trim().parse::<u32>().ok())
            else {
                // Strict, as on a real server: `pg_cancel_backend(NULL)` is NULL.
                return Ok(Datum::Null);
            };
            let asked = crate::session::cancel_pid(pid);
            super::cancel::check()?;
            Datum::Bool(asked)
        }
        CatalogFunc::PgTerminateBackend => {
            let Some(pid) = args
                .first()
                .and_then(Datum::to_text)
                .and_then(|text| text.trim().parse::<u32>().ok())
            else {
                // Strict, as on a real server: `pg_terminate_backend(NULL)` is NULL.
                return Ok(Datum::Null);
            };
            // **Including this session's own pid**, which a real server allows and one Rails
            // helper reaches. The connection loop checks the flag again once the statement is
            // done, so the `t` computed here never reaches the client: PostgreSQL answers that
            // statement with `FATAL` and closes, and so does this.
            let asked = crate::session::terminate_pid(pid);
            super::cancel::check()?;
            Datum::Bool(asked)
        }
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
        // **An empty `from` is a no-op**, which is the one rule `str::replace` does not share:
        // it matches between every character and answers `XaXbXcX` where PostgreSQL answers
        // `abc`. Everything else — left to right, non-overlapping, case-sensitive — is the same.
        // `split_part(text, sep, n)`. Measured on 19beta1, and the edges are the specification:
        // past the end is `''` and not NULL, a negative `n` counts from the end, an empty
        // separator gives the whole string back, and `n = 0` is an error rather than an answer.
        // `string_to_array(text, delimiter [, null_string])`. Measured on 19beta1, and **four of
        // its five edges are nothing a guess would produce**:
        //
        // ```text
        // ('a,b,c', ',')      {a,b,c}      ('', ',')        {}     -- empty, not one empty element
        // ('single', ',')     {single}     (NULL, ',')      NULL
        // ('abc', '')         {abc}        -- an empty delimiter does not split at all
        // ('a,b', NULL)       {a,",",b}    -- a NULL delimiter splits into single characters
        // ('a,,b', ',')       {a,"",b}     -- an empty field is kept
        // ('axxbxxc', 'xx')   {a,b,c}      -- the delimiter is a string, not a character
        // ('a,b,NULL', ',', 'NULL')        {a,b,NULL}  -- the third argument names the NULL text
        // ```
        CatalogFunc::StringToArray => match (args.first(), args.get(1)) {
            (Some(Datum::Text(text)), Some(delimiter)) => {
                let null_string = match args.get(2) {
                    Some(Datum::Text(null_string)) => Some(null_string.as_str()),
                    _ => None,
                };
                let fields: Vec<String> = match delimiter {
                    // A NULL delimiter splits into characters — measured, and the one edge that
                    // reads as a mistake until the server is asked.
                    Datum::Null => text.chars().map(|c| c.to_string()).collect(),
                    Datum::Text(delimiter) if delimiter.is_empty() => {
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![text.clone()]
                        }
                    }
                    Datum::Text(delimiter) => {
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            text.split(delimiter.as_str()).map(str::to_owned).collect()
                        }
                    }
                    _ => return Ok(Datum::Null),
                };
                let elements: Vec<Option<Datum>> = fields
                    .into_iter()
                    .map(|field| match null_string {
                        Some(null_string) if field == null_string => None,
                        _ => Some(Datum::Text(field)),
                    })
                    .collect();
                Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
                    ColumnType::Text,
                    1,
                    elements,
                ))
            }
            _ => Datum::Null,
        },
        CatalogFunc::SplitPart => match (args.first(), args.get(1), args.get(2)) {
            (Some(Datum::Text(text)), Some(Datum::Text(sep)), Some(position)) => {
                let Some(n) = whole_number(Some(position)) else {
                    return Ok(Datum::Null);
                };
                if n == 0 {
                    return Err(SqlError::InvalidFunctionArgument(
                        "field position must not be zero",
                    ));
                }
                if sep.is_empty() {
                    return Ok(Datum::Text(text.clone()));
                }
                let fields: Vec<&str> = text.split(sep.as_str()).collect();
                let at = if n > 0 {
                    usize::try_from(n - 1).ok()
                } else {
                    // `-1` is the last field, `-len` the first, and anything further is past the
                    // start — which answers the empty string, the same as past the end.
                    usize::try_from(n.checked_neg().unwrap_or(i64::MAX))
                        .ok()
                        .and_then(|back| fields.len().checked_sub(back))
                };
                Datum::Text(
                    at.and_then(|at| fields.get(at))
                        .map_or_else(String::new, |field| (*field).to_owned()),
                )
            }
            _ => Datum::Null,
        },
        // `strpos(haystack, needle)`: 1-based, `0` for absent, and `1` for an empty needle —
        // measured, it matches at the start rather than nowhere. Counted in **characters**, which
        // is what makes it agree with `substr`.
        CatalogFunc::StrPos => match (args.first(), args.get(1)) {
            (Some(Datum::Text(haystack)), Some(Datum::Text(needle))) => {
                Datum::Int4(haystack.find(needle.as_str()).map_or(0, |byte| {
                    i32::try_from(haystack[..byte].chars().count() + 1).unwrap_or(i32::MAX)
                }))
            }
            _ => Datum::Null,
        },
        // `substr(text, from[, count])`, 1-based and **clamped at both ends**: the characters at
        // positions `max(from, 1) ..= from + count - 1`, so `substr('hello', -1, 3)` is `h` —
        // positions -1, 0 and 1, of which only 1 exists. Getting that wrong by clamping `from`
        // before applying `count` gives `hel`, which is the plausible answer and not the measured
        // one.
        // **A bit string is substringed as its digits**, and comes back a bit string: measured,
        // `substring('10110'::varbit from 2 for 3)` is `011` and `pg_typeof` of it is `bit` — a
        // plain `bit`, whichever of the two the argument was. The digits *are* the value, so the
        // arithmetic below is the same arithmetic; what was wrong was that a `Datum::Bit` matched
        // none of these arms and fell through to the NULL at the end, which is a wrong answer
        // wearing a right one's clothes.
        CatalogFunc::Substr | CatalogFunc::Substring => match (args.first(), args.get(1)) {
            (Some(Datum::Text(text) | Datum::Bit { bits: text, .. }), Some(from)) => {
                let Some(from) = whole_number(Some(from)) else {
                    return Ok(Datum::Null);
                };
                let end = match args.get(2) {
                    None => None,
                    Some(Datum::Null) => return Ok(Datum::Null),
                    Some(count) => match whole_number(Some(count)) {
                        None => return Ok(Datum::Null),
                        Some(count) => {
                            if count < 0 {
                                return Err(SqlError::InvalidFunctionArgument(
                                    "negative substring length not allowed",
                                ));
                            }
                            Some(from.saturating_add(count))
                        }
                    },
                };
                let first = from.max(1);
                let taken: String = text
                    .chars()
                    .enumerate()
                    .filter_map(|(at, ch)| {
                        let position = i64::try_from(at).unwrap_or(i64::MAX).saturating_add(1);
                        let within = position >= first && end.is_none_or(|end| position < end);
                        within.then_some(ch)
                    })
                    .collect();
                if matches!(args.first(), Some(Datum::Bit { .. })) {
                    Datum::Bit {
                        varying: false,
                        bits: taken,
                    }
                } else {
                    Datum::Text(taken)
                }
            }
            _ => Datum::Null,
        },
        CatalogFunc::Replace => match (args.first(), args.get(1), args.get(2)) {
            (Some(Datum::Text(text)), Some(Datum::Text(from)), Some(Datum::Text(to))) => {
                if from.is_empty() {
                    Datum::Text(text.clone())
                } else {
                    Datum::Text(text.replace(from.as_str(), to))
                }
            }
            // Strict: any NULL argument is a NULL answer, and a non-text one has no `replace`.
            _ => Datum::Null,
        },
        // **The characters are a set, not a prefix**: `TRIM(BOTH 'ab' FROM 'abcba')` is `c`,
        // measured. One argument trims whitespace, which is what a bare `TRIM(x)` means.
        CatalogFunc::Btrim | CatalogFunc::Ltrim | CatalogFunc::Rtrim => {
            match (args.first(), args.get(1)) {
                (Some(Datum::Null) | None, _) | (_, Some(Datum::Null)) => Datum::Null,
                (Some(value), set) => {
                    let Some(text) = value.to_text() else {
                        return Ok(Datum::Null);
                    };
                    let set: Vec<char> = match set {
                        // **A space, and nothing else**: `btrim(text)` removes "a space by default"
                        // and that is the whole default — `length(btrim(E'\t x \n'))` is 5 on a real
                        // server, measured, where a whitespace class would have said 1.
                        None => vec![' '],
                        Some(chars) => chars.to_text().unwrap_or_default().chars().collect(),
                    };
                    let cut = |text: &str| -> String {
                        let trimmed = match call.func {
                            CatalogFunc::Ltrim => text.trim_start_matches(|c| set.contains(&c)),
                            CatalogFunc::Rtrim => text.trim_end_matches(|c| set.contains(&c)),
                            _ => text.trim_matches(|c| set.contains(&c)),
                        };
                        trimmed.to_owned()
                    };
                    Datum::Text(cut(&text))
                }
            }
        }
        // **Not strict**: a NULL argument is skipped, and the answer is NULL only when every one
        // of them is. Measured — `GREATEST(1, NULL, 3)` is `3`, where almost everything else in
        // this evaluator propagates a NULL.
        CatalogFunc::Greatest | CatalogFunc::Least => {
            let mut best: Option<&Datum> = None;
            for arg in &args {
                if matches!(arg, Datum::Null) {
                    continue;
                }
                best = Some(match best {
                    None => arg,
                    Some(sofar) => {
                        let take = if call.func == CatalogFunc::Greatest {
                            arg.pg_cmp(sofar) == Ordering::Greater
                        } else {
                            arg.pg_cmp(sofar) == Ordering::Less
                        };
                        if take { arg } else { sofar }
                    }
                });
            }
            let Some(best) = best else {
                return Ok(Datum::Null);
            };
            // **The value carries the declared type.** `greatest(3::int4, 2::int8)` is described as
            // `bigint` (`query::greatest_type`, the same promotion), and used to hand back the
            // `int4` it chose — text format hid it, `pg_typeof` and a binary-format reader did
            // not. The winner is cast the way it would be assigned into a column of that type.
            let promoted = args
                .iter()
                .filter(|arg| !matches!(arg, Datum::Null))
                .filter_map(Datum::column_type)
                .reduce(|sofar, ty| {
                    if sofar == ty {
                        sofar
                    } else {
                        crate::value::arith::result_type(crate::plan::ArithOp::Add, sofar, ty)
                            .unwrap_or(sofar)
                    }
                });
            match promoted {
                Some(ty)
                    if best.column_type() != Some(ty)
                        && crate::value::has_assignment_cast(best.column_type(), ty) =>
                {
                    crate::value::assignment_cast(best.clone(), ty, env.settings.rendering)?
                }
                _ => best.clone(),
            }
        }
        // **`nullif(a, b)` is `a`, or NULL when the two are equal** -- and not strict in the
        // other direction from `GREATEST` above. `nullif(NULL, 1)` is NULL because `a` is what
        // comes back; `nullif(1, NULL)` is `1`, because a comparison against NULL is *unknown*
        // rather than equal, so the "they matched" branch is not taken. Measured, both.
        //
        // The answer carries the type `query::nullif_type` described, for the reason the
        // `GREATEST` arm above states at length: handing back the operand's own datum where the
        // `RowDescription` said something wider is invisible in text format and wrong in binary.
        // Here the recomputation is the same rule read off the datums -- a `varchar` operand
        // resolves through `texteq`, so it answers `text`.
        CatalogFunc::NullIf => {
            // Two arguments, guaranteed by `CatalogFunc::arities` before evaluation -- so the
            // `else` here is not a case to handle but the answer a one-argument call would have
            // had, and it never reaches this arm.
            let (Some(left), Some(right)) = (args.first(), args.get(1)) else {
                return Ok(Datum::Null);
            };
            if !matches!(left, Datum::Null)
                && !matches!(right, Datum::Null)
                && left.pg_cmp(right) == Ordering::Equal
            {
                return Ok(Datum::Null);
            }
            match crate::exec::query::nullif_datum_type(left, right) {
                Some(ty)
                    if left.column_type() != Some(ty)
                        && crate::value::has_assignment_cast(left.column_type(), ty) =>
                {
                    crate::value::assignment_cast(left.clone(), ty, env.settings.rendering)?
                }
                _ => left.clone(),
            }
        }
        // **`mod(a, b)` delegates to the operator it shares an implementation with.** PostgreSQL's
        // `%` is the same C function, so there is one remainder here too — what the call keeps is
        // its *spelling*, so that `pg_get_indexdef` prints `mod(id, 10)` and not `id % 10`
        // (`postgresql_adapter_test#test_expression_index` asserts that string exactly).
        CatalogFunc::Mod => {
            let (Some(left), Some(right)) = (args.first(), args.get(1)) else {
                return Ok(Datum::Null);
            };
            match (left.column_type(), right.column_type()) {
                (Some(l), Some(r)) => {
                    let ty = crate::value::arith::result_type(crate::plan::ArithOp::Modulo, l, r)?;
                    crate::value::arith::apply(crate::plan::ArithOp::Modulo, ty, left, right)?
                }
                _ => Datum::Null,
            }
        }
        CatalogFunc::Concat => Datum::Text(
            args.iter()
                .filter(|arg| !matches!(arg, Datum::Null))
                .filter_map(PgDatum::to_text)
                .collect::<String>(),
        ),
        CatalogFunc::ConvertTo => convert_to(args.first(), args.get(1))?,
        // The value's own type. An untyped NULL has none and is `text`, which is what it is
        // everywhere else in this crate.
        // **An integer literal answers the width it was *declared*, not the width it is held
        // in.** The datum stays an `i64` whatever rung the literal is on — narrowing it re-types
        // every function argument — so without this `pg_typeof(1)` reads `bigint` while the
        // `RowDescription` for the same expression says `integer`. One expression with two
        // answers is the shape the enum unit removed from this crate, and this is where it would
        // have come straight back in.
        // **The type `exec::query::resolve` worked out, carried as the second argument.** It is
        // the argument's *declared* type — a datum cannot say which of the types sharing its
        // representation it is — and it rides here rather than replacing the call so that a
        // set-returning first argument still produces its rows.
        CatalogFunc::PgTypeof if args.len() == 2 => args[1].clone(),
        // A `pg_typeof` that never reached `resolve` — there is no such path in the executor
        // today, and a datum's own type is the honest answer if one appears.
        CatalogFunc::PgTypeof => crate::value::regtype_of_oid(
            args.first()
                .and_then(Datum::column_type)
                .unwrap_or(ColumnType::Text)
                .oid(),
        ),
        // **Per call, and the corpus pins the consequence rather than a value**: two calls in one
        // statement differ, and every draw is inside `[0, 1)`. The bytes come from the OS pool
        // through the same file `gen_random_uuid` reads (`crate::value::random`).
        CatalogFunc::Random => Datum::Double(crate::value::random::random_f64()?),
        // **A user-defined type is asked about only after the built-in ones**, so a tenant's id
        // sequence can never shadow one of PostgreSQL's fixed oids: everything that already had a
        // name keeps it, and this can only turn a `???` into an answer. `pg_attribute.atttypid`
        // reports a column's user type rather than its storage, so this is the call that prints
        // `mood` where the row holds an `int2` (ADR 0050).
        CatalogFunc::FormatType => {
            // **A `regtype` over a user type arrives as its *name***, not its oid — that is what
            // `UserRegType` resolves to, and ADR 0053 says why position cannot decide otherwise.
            // So the name is looked up before the built-in table is asked, which is where it
            // would have been `42704 type "mood" does not exist`: measured,
            // `format_type('mood'::regtype, NULL)` is `mood`, and a typmod does not change it.
            if let Some(Datum::Text(name)) = args.first()
                && crate::value::type_by_name(name)?.is_none()
                && let Some(def) = env.relations()?.user_type_by_name(name)
            {
                return Ok(Datum::Text(type_qualified_for(env.settings, &def.name)));
            }
            let oid = type_oid_argument(args.first())?;
            let built_in =
                crate::catalog::def_functions::format_type(oid, typmod_argument(args.get(1))?);
            match (&built_in, oid.and_then(|oid| u64::try_from(oid).ok())) {
                (Datum::Text(printed), Some(oid))
                    if printed == crate::catalog::def_functions::UNKNOWN_TYPE =>
                {
                    match env.relations()?.user_type_name(oid) {
                        // **`schema.name`, not the stored bytes.** A type in a schema is stored
                        // `schema ++ NUL ++ name` (`catalog::SCHEMA_SEPARATOR`), and printing it
                        // raw put a NUL on the wire where PostgreSQL writes a dot — measured,
                        // `ds_s.ds`.
                        Some(name) => Datum::Text(type_qualified_for(env.settings, name)),
                        None => built_in,
                    }
                }
                _ => built_in,
            }
        }
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
        // than an error.
        //
        // **The third argument is `pretty`, and it takes the deparser's pair back off**: measured,
        // one stored generated column prints `(c1 + 1)` from two arguments and `c1 + 1` from three.
        // It used to change nothing here and that was right by accident — the stored text carried
        // no pair at all, so the two-argument form was wrong and the three-argument form was
        // right. Now the text is the printed form (`crate::catalog::ExprShape`) and the pretty
        // form is that form with its own pair removed
        // (`crate::catalog::unparenthesised`, `tests/generated_parens.rs`).
        CatalogFunc::PgGetExpr => match args.first() {
            None | Some(Datum::Null) => Datum::Null,
            Some(Datum::Text(expr)) if pretty_argument(args.get(2))? => {
                Datum::Text(crate::catalog::unparenthesised(expr).to_owned())
            }
            Some(other) => other.clone(),
        },
        // The catalog it reads is snapshotted by the cursor, so a projection over every row of
        // `pg_index` reads it once rather than once per index.
        // **Strict in its second argument when there is one**, and that is not the same as
        // having none: `pg_get_indexdef(oid)` is the whole definition and
        // `pg_get_indexdef(oid, NULL, true)` is NULL. Measured, and the difference is invisible in
        // an `Option` that flattens the two.
        // The stored `SELECT`, not a deparse of it — the divergence `tests/view_debts.rs`
        // declares. `pretty` changes nothing, because there is no layout of ours to change.
        CatalogFunc::PgGetViewdef => match oid_argument(args.first())? {
            None => Datum::Null,
            Some(oid) => u64::try_from(oid)
                .ok()
                .and_then(|oid| env.relations().ok()?.view_definition(oid))
                .map_or(Datum::Null, |text| Datum::Text(text.to_owned())),
        },
        // **`pg_get_constraintdef` shares this guard**, being strict in its `pretty` flag the same
        // way: `pg_get_constraintdef(oid, NULL)` is NULL and `pg_get_constraintdef(oid)` is the
        // definition. Measured on both.
        CatalogFunc::PgGetIndexdef | CatalogFunc::PgGetConstraintdef
            if matches!(args.get(1), Some(Datum::Null)) =>
        {
            Datum::Null
        }
        CatalogFunc::PgGetIndexdef => crate::catalog::pg_index::index_definition(
            env.relations()?,
            oid_argument(args.first())?,
            column_argument(args.get(1))?,
        ),
        // **The inverse of `'x'::regclass`, and per row.** An oid that names nothing is not an
        // error: it prints the number back, and oid 0 prints `-`, PostgreSQL's rendering of
        // `InvalidOid`. Measured, both — raising here would break a `LEFT JOIN` that legitimately
        // has no match.
        // **A `Datum::Text` means the cast was written over a value, and the direction flips.**
        // `'x'::regclass` is a name resolved before the plan; `c::regclass` over a *text* column
        // reaches here per row, and lowering cannot tell the two apart because it has no types —
        // so the datum decides, exactly as it does for `RegTypeName` one arm down. Without this
        // the text went to the oid reader and answered `an oid is an integer, not Text(…)`
        // (`debts-v1.1.md` #41, the shape r1's wire gate found).
        CatalogFunc::RegClassName if matches!(args.first(), Some(Datum::Text(_))) => {
            let Some(Datum::Text(name)) = args.first() else {
                unreachable!("the guard above matched a text argument")
            };
            let Some(names) = env.settings.names else {
                return Err(SqlError::unsupported(
                    "a relation name read as a regclass without a catalog",
                ));
            };
            regclass_of(env, names(name)?)?
        }
        CatalogFunc::RegClassName => match oid_argument(args.first())? {
            None => Datum::Null,
            // **A `regclass`, not the name it prints as.** The three answers below are the
            // *output function*; the datum carries the oid beside them, which is what makes
            // `array_agg(oid::regclass)` a `regclass[]` (2210) and `min` of one an `oid`. It was a
            // `Datum::Text` here, and every one of those read the right characters off a column
            // described as 25 — the difference only a `Describe` sees, which is what r1's wire
            // sweep is for.
            //
            // **The catalog's own oids print as names too**, and they are asked for first: a
            // catalog relation is not in `Relations`, which reads the name records, so an oid of
            // one used to print its digits back. `CatalogView::name` is the printed form and
            // already carries the rule — `pg_class` bare because `pg_catalog` is in the search
            // path, `information_schema.tables` qualified because that schema is not.
            Some(oid) => regclass_of(env, oid)?,
        },
        // **The inverse of `'x'::regtype`, and per row**, with the three answers `RegClassName`
        // has and each of them measured: a type's printed name, `-` for oid 0 — which is what
        // every non-array row of `pg_type` holds in `typelem` — and the number back for an oid
        // this node has no type for.
        // **Which direction a `::regtype` goes is decided by what it casts *from*.** A `regtype`
        // has an input function and an output one: `t::regtype` over a **text** column is
        // `regtypein` — it resolves a name — and `i::regtype` over an integer is the oid read as
        // one. Both answer `integer` for `int4` and 23, measured. Lowering cannot tell them apart
        // because it has no types, so the datum decides here; before this, a text operand was
        // handed to `oid_argument` and `SELECT t::regtype FROM t` was
        // `an oid is an integer, not Text("int4")` — an internal representation in a user's face,
        // and the reason r1's wire sweep saw it through three array shapes that were not the
        // defect.
        //
        // **The answer is a `regtype` and not its name.** It used to be a `Datum::Text`, so the
        // `RowDescription` said 25 where a real server says 2206 — right bytes, wrong declared
        // type, the shape only a `Describe` sees.
        CatalogFunc::RegTypeName => match args.first() {
            None | Some(Datum::Null) => Datum::Null,
            Some(Datum::Text(name)) => {
                let named = crate::value::named_type(name)?
                    .ok_or_else(|| SqlError::UndefinedType(name.trim().to_owned()))?;
                Datum::RegType {
                    oid: named.oid(),
                    name: named.printed().into_boxed_str(),
                }
            }
            other => match oid_argument(other)? {
                None => Datum::Null,
                Some(oid) => {
                    let printed = u32::try_from(oid)
                        .ok()
                        .and_then(crate::value::type_by_oid)
                        .map_or_else(
                            // An oid no type has prints its digits, and 0 prints `-`.
                            || {
                                if oid == 0 {
                                    "-".to_owned()
                                } else {
                                    oid.to_string()
                                }
                            },
                            |ty| crate::value::Named::Scalar(ty).printed(),
                        );
                    Datum::RegType {
                        oid: u32::try_from(oid).unwrap_or(0),
                        name: printed.into_boxed_str(),
                    }
                }
            },
        },
        // Every element's oid, space separated: what `pg_proc.proargtypes` holds, so the two
        // compare exactly. A NULL array is NULL, and an element that is not an oid-shaped value
        // has no number to render, which is the `42804` a real server gives the same cast.
        CatalogFunc::OidVector => match args.first() {
            Some(Datum::Array(array)) => {
                let mut out = String::new();
                for value in &array.values {
                    let oid = match value {
                        Some(Datum::Oid(oid) | Datum::RegType { oid, .. }) => u64::from(*oid),
                        Some(Datum::RegClass { oid, .. }) => oid.unsigned_abs(),
                        Some(Datum::Int8(value)) => u64::try_from(*value).unwrap_or(0),
                        Some(Datum::Int4(value)) => u64::try_from(*value).unwrap_or(0),
                        // An element with no oid to render. `42846`, the class a cast that does
                        // not exist gets, and not the array's own error: the array is fine and
                        // the *cast* is what has no meaning for it.
                        other => {
                            return Err(SqlError::CannotCast {
                                from: other.as_ref().map_or("unknown", |value| {
                                    value.column_type().map_or("unknown", |ty| ty.name())
                                }),
                                to: "oidvector",
                            });
                        }
                    };
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&oid.to_string());
                }
                Datum::Text(out)
            }
            Some(Datum::Null) | None => Datum::Null,
            // `x::oidvector` where `x` is not an array at all.
            Some(other) => {
                return Err(SqlError::CannotCast {
                    from: other.column_type().map_or("unknown", |ty| ty.name()),
                    to: "oidvector",
                });
            }
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
            // **A table that is not there RAISES**, and so does a column that is not — measured on
            // 19beta1, `relation "zomg" does not exist` and `column "nosuch" of relation "g1z"
            // does not exist`. Answering NULL for either is what `ActiveRecord` cannot tell from
            // "this column owns no sequence", which is the real NULL: `default_sequence_name`
            // rescues the exception and falls back to `<table>_<pk>_seq`, so a node that never
            // raises never produces the fallback.
            let Some(def) = relations
                .by_name(&stored)
                .and_then(|row| relations.table(row))
            else {
                return Err(SqlError::UndefinedTable(crate::catalog::written_display(
                    table,
                )));
            };
            let Some(at) = def.column(column) else {
                return Err(SqlError::UndefinedColumnInRelation {
                    column: column.clone(),
                    relation: crate::catalog::split_qualified(&def.name).1.to_owned(),
                });
            };
            // A column that owns no sequence **is** the NULL this function has.
            let sequence = def
                .sequences
                .iter()
                .find(|sequence| sequence.column == Some(at));
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
            pretty_argument(args.get(1))?,
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
        // `@>` again: an hstore's containment and a range's are one operator, told apart by what
        // is on the left.
        CatalogFunc::HstoreContains if matches!(args.first(), Some(Datum::Range { .. })) => {
            range_value_function(CatalogFunc::RangeContains, &args)?
        }
        // **`||` over text is the *string* concatenation operator**, which is what the symbol
        // means before it means anything else — and it was the one spelling of it this node
        // refused, with `0A000 || over text is not supported`, while the type dispatch in
        // `exec::query` had already been answering `text` for it.
        //
        // Fifth spelling, and the last one that is not a family of its own (arrays and `jsonb`
        // are). Told apart by the operands like the four above it: this arm is reached only when
        // **no** operand is an hstore, an ltree or a tsvector, because a bare `Datum::Text` beside
        // one of those is the `unknown` literal that belongs to *that* type's operator.
        // **`jsonb || jsonb` merges documents**, and it reaches here two ways: as its own variant
        // when a cast told the lowerer the type, and as an ordinary `||` whose operand is a jsonb
        // *column*, which still carries its type in `Expr::Ordinal`. A jsonb literal is
        // canonicalised into a `Datum::Text` long before this, so the values cannot decide it.
        // `doc -> key` and `doc ->> key`. The **key** decides which member is asked for: a string
        // names one, an integer indexes an array from either end.
        CatalogFunc::JsonFetch | CatalogFunc::JsonbFetch | CatalogFunc::JsonFetchText => {
            json_fetch(
                args.first(),
                args.get(1),
                call.func == CatalogFunc::JsonFetchText,
            )?
        }
        CatalogFunc::JsonbConcat => jsonb_concat(args.first(), args.get(1))?,
        CatalogFunc::HstoreConcat if call.args.iter().all(is_jsonb_typed) => {
            jsonb_concat(args.first(), args.get(1))?
        }
        // **An array operand makes `||` array concatenation**, which is a sixth spelling of the
        // symbol and the one this crate did not have at all: `text[] || text[]` was
        // `42883 operator does not exist`, and so was every other element type — r1's wire sweep
        // saw it through `regtype[]` and `name[]` and it was never about those.
        //
        // Three shapes, all measured, and two rules reasoning gets backwards:
        //
        // ```text
        //   ARRAY[1,2] || ARRAY[3]      {1,2,3}
        //   ARRAY[1,2] || 3             {1,2,3}     an element appends
        //   3 || ARRAY[1,2]             {3,1,2}     and prepends
        //   ARRAY[1,2] || NULL::int4    {1,2,NULL}  a NULL *element* is an element
        //   NULL::int4[] || ARRAY[1]    {1}         a NULL *array* is empty, not NULL
        // ```
        //
        // The last two are `array_cat`'s own rules and neither follows from the other: the same
        // NULL is a value on one side of the operator and an absence on the other.
        CatalogFunc::HstoreConcat
            if args.iter().any(|arg| matches!(arg, Datum::Array(_)))
                || matches!(
                    (args.first(), args.get(1)),
                    (Some(Datum::Null), Some(Datum::Array(_)))
                        | (Some(Datum::Array(_)), Some(Datum::Null))
                ) =>
        {
            array_concat(args.first(), args.get(1))
        }
        CatalogFunc::HstoreConcat
            if !args.iter().any(|value| {
                matches!(
                    value,
                    Datum::Hstore(_) | Datum::Ltree(_) | Datum::TsVector(_)
                )
            }) =>
        {
            text_concat(args.first(), args.get(1))?
        }
        // **`||` over two tsvectors concatenates and renumbers**, which is not what `||` over two
        // strings does: the right operand's positions are shifted by the left's maximum, so
        // `a || b` is not `b || a`. Measured both ways round. Told apart here for the reason this
        // match already tells an hstore's `@>` from a range's — by the operands.
        CatalogFunc::HstoreConcat if matches!(args.first(), Some(Datum::TsVector(_))) => {
            match (args.first(), args.get(1)) {
                (Some(Datum::TsVector(left)), Some(Datum::TsVector(right))) => {
                    Datum::TsVector(crate::value::tsvector::concat(left, right)?)
                }
                _ => Datum::Null,
            }
        }
        CatalogFunc::DateTrunc => date_trunc(&args, env.settings.rendering.zone)?,
        CatalogFunc::HstoreFetch
        | CatalogFunc::HstoreHasKey
        | CatalogFunc::HstoreContains
        | CatalogFunc::HstoreConcat
        | CatalogFunc::HstoreAkeys
        | CatalogFunc::HstoreAvals
        | CatalogFunc::HstoreBuild => hstore_function(call.func, &args)?,
        CatalogFunc::ToTsVector
        | CatalogFunc::ToTsQuery
        | CatalogFunc::PlainToTsQuery
        | CatalogFunc::PhraseToTsQuery
        | CatalogFunc::TsMatch
        | CatalogFunc::TsStrip
        | CatalogFunc::SetWeight
        | CatalogFunc::NumNode
        | CatalogFunc::TsHeadline
        | CatalogFunc::WebsearchToTsQuery
        | CatalogFunc::TsRank => text_search_function(call.func, &args)?,
        // **Strict, each of them**, and `text2ltree` validates: a path that is not a path is
        // `ltree`'s own `42601` here exactly as it is through the cast.
        CatalogFunc::LtreeNlevel => match args.first() {
            Some(Datum::Ltree(path)) => Datum::Int4(ltree::nlevel(path)),
            Some(Datum::Text(path)) => Datum::Int4(ltree::nlevel(&ltree::from_text(path)?)),
            _ => Datum::Null,
        },
        CatalogFunc::LtreeToText => match args.first() {
            Some(Datum::Ltree(path)) => Datum::Text(path.clone()),
            _ => Datum::Null,
        },
        CatalogFunc::TextToLtree => match args.first() {
            Some(Datum::Text(path) | Datum::Ltree(path)) => Datum::Ltree(ltree::from_text(path)?),
            _ => Datum::Null,
        },
        CatalogFunc::RangeLowerInc
        | CatalogFunc::RangeUpperInc
        | CatalogFunc::RangeLowerInf
        | CatalogFunc::RangeUpperInf
        | CatalogFunc::RangeContains
        | CatalogFunc::RangeBuild => range_value_function(call.func, &args)?,
        // **`isempty` and `&&` are spelled the same for both range shapes**, so the operand
        // decides: a stored range column is a `Datum::Range` and the `daterange(a, b)` expression
        // is a `Datum::Text`. Dispatching on the value is the same rule `@>` follows.
        CatalogFunc::IsEmpty | CatalogFunc::RangeOverlaps
            if args.iter().any(|arg| matches!(arg, Datum::Range { .. })) =>
        {
            range_value_function(call.func, &args)?
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
        CatalogFunc::RegClass | CatalogFunc::ToRegClass => {
            return Err(SqlError::Internal(format!(
                "{}() reached the row evaluator unresolved",
                call.func.name()
            )));
        }
        // The same, one cast over: resolved in the same pass and for the same reason.
        CatalogFunc::UserCast => {
            return Err(SqlError::Internal(
                "a cast to a user-defined type reached the row evaluator unresolved".to_owned(),
            ));
        }
        // **The bracket, not the geometry.** A `path` keeps the one it was written with — `[…]`
        // open, `(…)` closed — so the two functions read the first character of the canonical
        // text. A NULL path is a NULL answer, which is what every strict function here does.
        CatalogFunc::PathIsOpen | CatalogFunc::PathIsClosed => match args.first() {
            Some(Datum::Geometry { text, .. }) => {
                let open = text.starts_with('[');
                Datum::Bool(open == (call.func == CatalogFunc::PathIsOpen))
            }
            _ => Datum::Null,
        },
        CatalogFunc::UserRegType => {
            return Err(SqlError::Internal(
                "a regtype over a user-defined type reached the row evaluator unresolved"
                    .to_owned(),
            ));
        }
        CatalogFunc::UserFunc => {
            return Err(SqlError::Internal(
                "a call to a user-defined function reached the row evaluator unresolved".to_owned(),
            ));
        }
    })
}

/// `format_type`'s first argument: an `oid`.
///
/// **Text is taken as a type name**, and that is not a liberty — it is the one coercion this node
/// cannot express any other way. A real server writes `format_type('integer'::regtype, NULL)` and
/// coerces `regtype` to `oid` for free, **and so does this node now** — a `regtype` is an oid here
/// too (ADR 0077). The `Text` arm below is what the old `regtype`-is-text model needed and stays
/// because `format_type` is also written with a bare name in this suite; a name that is no type of
/// this server's is `42704`, which is what `'x'::regtype` itself answers.
fn type_oid_argument(arg: Option<&Datum>) -> Result<Option<i64>> {
    Ok(match arg {
        None | Some(Datum::Null) => None,
        // A `regclass` is an `i64` already, so it joins the `int8` arm rather than repeating it.
        Some(Datum::Int8(oid) | Datum::RegClass { oid, .. }) => Some(*oid),
        Some(Datum::Int4(oid)) => Some(i64::from(*oid)),
        Some(Datum::Int2(oid)) => Some(i64::from(*oid)),
        // **And a real `oid`**, which is what `'integer'::regtype::oid` folds to now: the two
        // spellings of that question had drifted, and this had only ever been handed the
        // integer one. **A `regtype` is one too** since ADR 0077, which is what makes
        // `format_type('integer'::regtype, NULL)` the free coercion a real server makes rather
        // than the string this used to be handed.
        Some(Datum::Oid(oid) | Datum::RegType { oid, .. }) => Some(i64::from(*oid)),
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

/// `array_cat`: two arrays, an array and an element, or an element and an array.
///
/// **A NULL array is empty and a NULL element is an element.** Measured on 19beta1:
/// `NULL::int4[] || ARRAY[1]` is `{1}` and `ARRAY[1,2] || NULL::int4` is `{1,2,NULL}`. The two
/// rules are about the same NULL on the two sides of one operator and neither follows from the
/// other, which is why both are written down.
///
/// One dimension only, which is what this crate stores; the element type comes from whichever
/// operand is an array, so `ARRAY[1,2] || 3` keeps `integer[]`.
fn array_concat(left: Option<&Datum>, right: Option<&Datum>) -> Datum {
    let element_type = [left, right]
        .into_iter()
        .flatten()
        .find_map(|value| match value {
            Datum::Array(array) => Some(array.element),
            _ => None,
        })
        .unwrap_or(ColumnType::Text);
    let mut values: Vec<Option<Datum>> = Vec::new();
    let mut push = |side: Option<&Datum>| match side {
        // A NULL *array* contributes nothing; a NULL element cannot be told from it here, and
        // this arm is reached only when the other side is an array, so the operand's own declared
        // type is what decides. `ARRAY[1,2] || NULL::int4` reaches the element arm below because
        // the plan gives the NULL the element's type — see `exec::query::concat_type`.
        None | Some(Datum::Null) => {}
        Some(Datum::Array(array)) => values.extend(array.values.iter().cloned()),
        Some(other) => values.push(Some(other.clone())),
    };
    push(left);
    push(right);
    // **Lower bound 1**, which the second argument is — it is the bound and not the length, and
    // passing the length printed `[3:5]={1,2,3}`. PostgreSQL keeps the left operand's bounds and
    // every array this crate builds starts at 1, so a concatenation does too.
    Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
        element_type,
        1,
        values,
    ))
}

/// One oid as the `regclass` it is: the number, and the name it prints as.
///
/// **The output function of a `regclass`, in one place**, because three callers need exactly the
/// same three answers and a second copy of them is how two readers of one fact come to disagree.
/// A catalog view is asked for first — it is not in `Relations`, which reads the name records, so
/// an oid of one used to print its digits back. `CatalogView::name` already carries the
/// search-path rule: `pg_class` bare because `pg_catalog` is on the path,
/// `information_schema.tables` qualified because that schema is not.
///
/// A relation outside `public` is **qualified only when its schema is not on the `search_path`** —
/// measured: `'g1_rc.t'::regclass::text` is `g1_rc.t` under the default path and `t` after
/// `SET search_path = g1_rc, public`. Oid 0 is `-`, PostgreSQL's rendering of `InvalidOid`, and an
/// oid naming nothing prints its digits: measured, both, and neither is an error — raising here
/// would break a `LEFT JOIN` that legitimately has no match.
/// The rule a decoded row's `regclass` columns get their names from.
///
/// **A `regclass` column stores eight bytes and no name** (`debts-v1.1.md` #35) — a name in a row
/// goes stale the moment its relation is renamed — so the name is put back here, where the session
/// and the catalog both are. Measured on a real server: after `ALTER TABLE rc_a RENAME TO rc_b` a
/// stored `regclass` prints `rc_b`, and after the relation is dropped it prints the oid's digits.
///
/// **A lookup that fails falls back to the digits**, which is that second measured answer: an oid
/// naming nothing prints as its number there, so a catalog this cursor cannot read degrades to a
/// real server's rendering rather than to an error in the middle of a scan.
fn relation_namer(env: Env<'_>) -> impl Fn(i64) -> Box<str> + '_ {
    move |oid| match regclass_of(env, oid) {
        Ok(Datum::RegClass { name, .. }) => name,
        _ => oid.to_string().into_boxed_str(),
    }
}

fn regclass_of(env: Env<'_>, oid: i64) -> Result<Datum> {
    let printed = match crate::catalog::pg_catalog::view_by_oid(oid) {
        Some(view) => view.name().to_owned(),
        None => match env.relations()?.by_oid(oid) {
            Some(relation) => qualified_for(env.settings, relation),
            None if oid == 0 => "-".to_owned(),
            None => oid.to_string(),
        },
    };
    Ok(Datum::RegClass {
        oid,
        name: printed.into(),
    })
}

/// An `oid` argument, which is an integer of whatever width the column it came from has.
fn oid_argument(arg: Option<&Datum>) -> Result<Option<i64>> {
    Ok(match arg {
        None | Some(Datum::Null) => None,
        // A `regclass` is an `i64` already, so it joins the `int8` arm rather than repeating it.
        Some(Datum::Int8(oid) | Datum::RegClass { oid, .. }) => Some(*oid),
        Some(Datum::Int4(oid)) => Some(i64::from(*oid)),
        Some(Datum::Int2(oid)) => Some(i64::from(*oid)),
        // An `oid` is what a catalog column really holds; `23::oid::regtype` sends one. **A
        // `regtype` is one too** — that is the whole model (ADR 0077), and it is what makes
        // `format_type('integer'::regtype, NULL)` the statement a real server answers rather than
        // a type error.
        Some(Datum::Oid(oid) | Datum::RegType { oid, .. }) => Some(i64::from(*oid)),
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "an oid is an integer, not {other:?}"
            )));
        }
    })
}

/// `pg_get_indexdef`'s optional column number. `None` is the one-argument form, which is a
/// different answer from column `0` — that one prints the definition unqualified.
/// An integer function argument as an `i64`, or `None` for anything that is not one.
///
/// The three string functions are **strict**: a NULL argument is a NULL answer, and a non-integer
/// one has no meaning to give, so both come back `None` and the caller answers NULL.
fn whole_number(arg: Option<&Datum>) -> Option<i64> {
    match arg? {
        Datum::Int8(n) => Some(*n),
        Datum::Int4(n) => Some(i64::from(*n)),
        Datum::Int2(n) => Some(i64::from(*n)),
        _ => None,
    }
}

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

/// `pg_get_constraintdef`'s optional `pretty` flag.
///
/// **Absent is `false`**, measured: the one-argument form and `pretty => false` are the same
/// string for every contype. A NULL is answered before this is called, the function being strict
/// in both arguments.
fn pretty_argument(arg: Option<&Datum>) -> Result<bool> {
    Ok(match arg {
        None | Some(Datum::Null) => false,
        Some(Datum::Bool(pretty)) => *pretty,
        Some(other) => {
            return Err(SqlError::DatatypeMismatch(format!(
                "a pretty flag is a boolean, not {other:?}"
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
            other.column_type().map_or("unknown", PgType::name)
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

/// One operand of `AND`/`OR` as a three-valued boolean, or PostgreSQL's `42804`, naming the
/// construct the condition belongs to.
///
/// **The message names the type and never the value.** It was built from the datum a row happened
/// to hold — `not Text("one")` — which leaks a user's row into an error and gives one query a
/// different message per row. A real server says `argument of AND must be type boolean, not type
/// character varying`, with the word `type` twice, and names the one construct rather than the
/// pair `AND/OR`: measured, along with `argument of CASE/WHEN` for the other place a condition is
/// read.
fn truth_of(value: &Datum, construct: &'static str) -> Result<Option<bool>> {
    match value {
        Datum::Bool(value) => Ok(Some(*value)),
        Datum::Null => Ok(None),
        other => Err(SqlError::NonBooleanArgument {
            construct,
            found: other.column_type().map_or("text", PgType::name).to_owned(),
        }),
    }
}
