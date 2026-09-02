//! Running a subquery: once before the cursor, and what its rows mean.
//!
//! `docs/plans/phase-12-subquery.md` §1. Three passes live here and they run in this order:
//!
//! 1. [`plan_subqueries`] — **before the outer plan is built**, because the outer statement cannot
//!    be typed until every subquery in it has been: `WHERE n IN (SELECT a_id FROM b)` is
//!    `42883 operator does not exist: text = bigint`, and nothing can say so without knowing that
//!    the subquery's column is a `bigint`.
//! 2. [`resolve`] — **before the cursor opens**, and this is where a subquery actually runs. It is
//!    the same shape [`crate::exec::fragment::resolve`] uses to fill a `Node::Columnar` from the
//!    fragments, for the same reason: a `Cursor` evaluates a row at a time with no transaction of
//!    its own to start a second query in, and running the subquery here puts its answer in the
//!    *same* transaction at the *same* snapshot as the plan that reads it.
//! 3. [`value`] — from the row evaluator, turning the rows a subquery produced into the one value
//!    the expression around it wants.
//!
//! # Why not run it from the evaluator
//!
//! Because it would run **per row**. `WHERE id IN (SELECT a_id FROM b)` over a million-row table
//! would open the inner scan a million times for an answer that cannot change, and no cache put in
//! front of that is as simple as not asking. What is left for the evaluator is the shape where the
//! answer *does* change per row, which is a correlated subquery and is unit 4.

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::exec::cursor::{Cursor, SORT_LIMIT};
use crate::plan::{
    AggregateSpec, BinaryOp, Expr, Literal, Node, Select, SelectItem, SubqueryExpr, SubqueryKind,
};
use crate::value::{Datum, PgDatum};

/// Whether a statement has a subquery anywhere the planner will look.
///
/// `Cow`-shaped by hand the way [`crate::exec::Executor::resolve_sequence_calls`] is: a statement
/// with none — which is nearly all of them — is planned from the `Select` the caller already has,
/// and nothing is cloned.
pub(super) fn present(select: &Select) -> bool {
    if !select.ctes.is_empty()
        || select
            .from
            .iter()
            .chain(select.joins.iter().map(|join| &join.table))
            .any(|entry| entry.derived.is_some())
    {
        return true;
    }
    let mut found = false;
    for_each_written_expr(select, &mut |expr| {
        found = found || contains_subquery(expr);
    });
    found
}

/// Whether this expression, or one under it, is a subquery.
fn contains_subquery(expr: &Expr) -> bool {
    let mut found = false;
    walk(expr, &mut |expr| {
        found = found || matches!(expr, Expr::Subquery(_));
    });
    found
}

/// Plans every subquery in a statement, innermost first, and types it.
///
/// The recursion is over the *statement* rather than over the plan, because a subquery's plan is
/// what this produces. A subquery inside a subquery is planned by the same call on the inner
/// `Select` before the outer one is planned, so by the time a `SubqueryExpr` is typed everything
/// it contains already is.
pub(super) fn plan_subqueries(
    select: &mut Select,
    tenant: u64,
    txn: &dyn Txn,
    tables: &dyn Tables,
) -> Result<()> {
    let mut outcome = Ok(());
    for_each_written_expr_mut(select, &mut |expr| {
        if outcome.is_ok() {
            outcome = walk_mut(expr, &mut |expr| match expr {
                Expr::Subquery(sub) => plan_one(sub, tenant, txn, tables),
                _ => Ok(()),
            });
        }
    });
    outcome?;
    // The `FROM` entries after the expressions, because a derived table's plan is what the
    // statement's own scope is built from and a `LIMIT` may name neither.
    // The `WITH` items first, and **all** of them: an unreferenced CTE is still analysed on a real
    // server (`WITH t AS (SELECT nope FROM a) SELECT 1` is `42703`, measured) and inlining alone
    // would never look at one. The plan each produces here is thrown away; what is kept is the
    // error it would have raised.
    for cte in &mut select.ctes {
        plan_derived(cte, tenant, txn, tables)?;
    }
    for entry in std::iter::once(&mut select.from)
        .flatten()
        .chain(select.joins.iter_mut().map(|join| &mut join.table))
    {
        plan_derived(entry, tenant, txn, tables)?;
    }
    fold_counts(select, txn, tenant)
}

/// One `FROM (SELECT …) AS t`: its plan, and the relation it looks like from above.
///
/// The relation is a **synthetic [`TableDef`]** — one column per output column of the sub-select,
/// under [`crate::catalog::DERIVED_TABLE_ID`] — and building one is the whole trick this unit
/// turns on. With it, `Scope` resolves a name, `SELECT *` expands, `EXPLAIN` prints the names the
/// user typed and the join machinery probes or materialises, all without learning that nothing
/// stores these rows. Without it every one of those would need a second code path.
fn plan_derived(
    entry: &mut crate::plan::TableRef,
    tenant: u64,
    txn: &dyn Txn,
    tables: &dyn Tables,
) -> Result<()> {
    let name = entry.referred_as().to_owned();
    let Some(derived) = entry.derived.as_mut() else {
        return Ok(());
    };
    plan_subqueries(&mut derived.select, tenant, txn, tables)?;
    let planned = plan_select_of(&derived.select, tenant, tables)?;

    // A column alias list **may be shorter** than the target list — `AS t(a)` over two columns
    // renames the first and leaves the second alone, measured — and only a longer one is an error.
    // Refusing a short one reads like the obvious symmetry and would refuse a statement a real
    // server runs.
    if derived.columns.len() > planned.columns.len() {
        // The **noun differs**: a CTE's is `WITH query "t" has …` and a derived table's is
        // `table "t" has …`, under the same SQLSTATE. Measured, both, and a client that greps the
        // text sees two sentences.
        let what = if derived.cte { "WITH query" } else { "table" };
        return Err(SqlError::InvalidColumnReference(format!(
            "{what} \"{name}\" has {} columns available but {} columns specified",
            planned.columns.len(),
            derived.columns.len()
        )));
    }
    let columns: Vec<crate::catalog::ColumnDef> = planned
        .columns
        .iter()
        .enumerate()
        .map(|(at, (column, ty, typmod))| crate::catalog::ColumnDef {
            // The alias replaces the name outright: after `AS t(a, b)`, `t.id` is `42703`.
            name: derived
                .columns
                .get(at)
                .cloned()
                .unwrap_or_else(|| column.clone()),
            ty: *ty,
            typmod: *typmod,
            // Nothing is ever written into a derived table, so neither of these can be read: a
            // `NOT NULL` is checked on insert and a default is applied on one.
            not_null: false,
            default: None,
            // Every row this relation produces is exactly as wide as its target list, because the
            // projection above the sub-plan built it — so no row is ever short and there is
            // nothing to pad with.
            missing: None,
        })
        .collect();
    derived.def = Some(std::sync::Arc::new(crate::catalog::TableDef {
        id: crate::catalog::DERIVED_TABLE_ID,
        name,
        columns,
        primary_key: Vec::new(),
        indexes: Vec::new(),
        // Empty would mean "the user declared no primary key, so column 0 is an internal row id"
        // — which is why `TableDef::row_id` names this id explicitly rather than reading this
        // field. Left empty because it is the truth: there is no constraint here to name.
        primary_key_name: String::new(),
        schema_version: 1,
        sequences: Vec::new(),
    }));
    // Wrapped rather than used bare, so `EXPLAIN` has a node to change the relation's name at:
    // the plan text threads one table name down the whole tree, and without this a scan of `dt_a`
    // inside `FROM (SELECT … FROM dt_a) AS t` prints `Seq Scan on t`.
    derived.plan = Some(Box::new(Node::Derived {
        input: Box::new(planned.node),
        alias: derived
            .def
            .as_ref()
            .map(|def| def.name.clone())
            .unwrap_or_default(),
        input_table: planned.table,
        input_columns: planned.column_names,
    }));
    Ok(())
}

/// Plans one sub-`SELECT` against the tables it names. Shared by a subquery and a derived table.
fn plan_select_of(
    select: &Select,
    tenant: u64,
    tables: &dyn Tables,
) -> Result<crate::exec::query::Planned> {
    let table = match &select.from {
        // A derived table inside a derived table: its own pass has already run, so the relation it
        // looks like is there to be borrowed.
        Some(from) => Some(relation_of(from, tables)?),
        None => None,
    };
    let inners = select
        .joins
        .iter()
        .map(|join| relation_of(&join.table, tables))
        .collect::<Result<Vec<_>>>()?;
    let inner_refs: Vec<&crate::catalog::TableDef> = inners.iter().map(AsRef::as_ref).collect();
    // **Never routed.** `crate::exec::query::plan` leaves `Planned::engine` at `None` and only
    // `crate::exec::fragment::route` fills it in; this is the call that does not make it, which is
    // what ADR 0040 asks a plan carrying a subquery to be able to say
    // (`docs/plans/phase-12-subquery.md` §4). A fragment's filter language has no subquery in it
    // and its answer arrives whole in one message, so there is nothing here for a replica to do.
    crate::exec::query::plan(select, tenant, table.as_deref(), &inner_refs)
}

/// The relation one `FROM` entry stands for: a real table, or a derived table's synthetic one.
pub(super) fn relation_of(
    entry: &crate::plan::TableRef,
    tables: &dyn Tables,
) -> Result<std::sync::Arc<crate::catalog::TableDef>> {
    match entry.derived.as_ref() {
        // A name that is a `WITH` item this part of the query cannot see becomes PostgreSQL's
        // three-part answer -- **only when the catalog has no such relation**, because a later CTE
        // does not hide a real table of the same name from an earlier body (measured).
        None => tables.get(&entry.name).map_err(|error| match error {
            SqlError::UndefinedTable(name) if entry.hidden_cte => {
                SqlError::ForwardCteReference(name)
            }
            other => other,
        }),
        Some(derived) => derived.def.clone().ok_or_else(|| {
            SqlError::Internal("a derived table reached the planner without a shape".to_owned())
        }),
    }
}

/// `LIMIT (SELECT …)` and `OFFSET (SELECT …)`, run here and replaced by the number they answered.
///
/// The one place a subquery cannot wait for [`resolve`]: a `LIMIT` is a *number in the plan* —
/// `Node::Limit` holds a `usize`, because a limit that changed per row would not be a limit — so
/// it has to be known before the node is built. Running it here is the same thing the executor
/// already does with a `nextval` in a target list, and it happens exactly once per statement,
/// which is what a real server does too.
fn fold_counts(select: &mut Select, txn: &dyn Txn, tenant: u64) -> Result<()> {
    for expr in select.limit.iter_mut().chain(select.offset.iter_mut()) {
        let Expr::Subquery(sub) = expr else { continue };
        run_one(sub, txn, tenant)?;
        // `LIMIT NULL` means no limit, which is PostgreSQL's rule and what `Literal::Null`
        // already reaches; anything else is the value the subquery answered with.
        *expr = Expr::Literal(match value(sub, None)? {
            Datum::Null => Literal::Null,
            Datum::Int8(count) => Literal::Integer(count),
            other => Literal::Typed(Box::new(other)),
        });
    }
    Ok(())
}

/// The catalog, as much of it as planning a subquery needs.
///
/// A trait rather than the `Executor` itself so that this module does not have to name a type that
/// carries a backend, a session and a transaction — and so that the one thing it *does* need is
/// visible in the signature: a subquery can only name tables the outer statement could have named.
pub(super) trait Tables {
    /// The table under this name, or the `42P01` a real server gives.
    fn get(&self, name: &str) -> Result<std::sync::Arc<crate::catalog::TableDef>>;
}

/// One subquery: its own subqueries first, then its plan, then the column it answers with.
fn plan_one(sub: &mut SubqueryExpr, tenant: u64, txn: &dyn Txn, tables: &dyn Tables) -> Result<()> {
    plan_subqueries(&mut sub.select, tenant, txn, tables)?;

    let planned = plan_select_of(&sub.select, tenant, tables)?;

    // `EXISTS` reads rows and not values, so any number of columns is legal under it — measured,
    // `SELECT EXISTS (SELECT id, n FROM sq_a)` is `t`. Every other kind wants exactly one, and
    // PostgreSQL gives the two cases **different sentences under the same SQLSTATE**: a scalar
    // subquery is `subquery must return only one column` and an `IN`/`ANY`/`ALL` is `subquery has
    // too many columns`. A client that greps the text sees two messages, so this node sends two.
    if sub.kind.reads_a_value() && planned.columns.len() != 1 {
        return Err(SqlError::SubqueryColumns(match sub.kind {
            SubqueryKind::Scalar => "subquery must return only one column",
            _ => "subquery has too many columns",
        }));
    }
    sub.column = planned
        .columns
        .first()
        .map(|(name, ty, _)| (name.clone(), *ty));
    sub.plan = Some(Box::new(planned.node));
    Ok(())
}

/// Runs every subquery in a plan and writes its answer into it.
///
/// Called once, before `Cursor::open`, on the plan the executor is about to pull rows through.
/// A subquery inside a subquery's *plan* is resolved by the same call on that plan before the one
/// holding it runs, which is what makes `WHERE id IN (SELECT a_id FROM b WHERE a_id IN (SELECT …))`
/// two runs rather than a panic.
pub(super) fn resolve(node: &mut Node, txn: &dyn Txn, tenant: u64) -> Result<()> {
    let mut outcome = Ok(());
    for_each_node_expr_mut(node, &mut |expr| {
        if outcome.is_ok() {
            outcome = walk_mut(expr, &mut |expr| match expr {
                Expr::Subquery(sub) => run_one(sub, txn, tenant),
                _ => Ok(()),
            });
        }
    });
    outcome
}

/// One subquery, run: its plan drained into the values the expression around it reads.
fn run_one(sub: &mut SubqueryExpr, txn: &dyn Txn, tenant: u64) -> Result<()> {
    let Some(plan) = sub.plan.as_deref() else {
        return Err(SqlError::Internal(format!(
            "{} reached the executor without a plan",
            sub.kind.describe()
        )));
    };
    let mut plan = plan.clone();
    resolve(&mut plan, txn, tenant)?;
    sub.run = Some(rows_of(&plan, sub.kind, txn, tenant)?);
    Ok(())
}

/// The values one run of a sub-plan produced, bounded.
///
/// The bound is the one `Node::Sort` and a materialised join's inner side already answer with
/// (`53400`), and it is not decoration: an `IN (SELECT …)` over an unbounded table is a whole
/// column of it in memory on behalf of a client. The kinds that need fewer rows read fewer —
/// `EXISTS` stops at one, a scalar at two, because the second row *is* the `21000` and a third
/// would be read for nobody.
fn rows_of(plan: &Node, kind: SubqueryKind, txn: &dyn Txn, tenant: u64) -> Result<Vec<Datum>> {
    let mut cursor = Cursor::open(txn, tenant, plan)?;
    let wanted = kind.rows_needed();
    let mut values = Vec::new();
    while let Some(row) = cursor.next()? {
        if values.len() == SORT_LIMIT {
            return Err(SqlError::ConfigurationLimitExceeded(format!(
                "{} over more than {SORT_LIMIT} rows needs more memory than this server will use \
                 for one query",
                kind.describe()
            )));
        }
        // `EXISTS` reads no value at all, so a row of a subquery with three columns and a row of
        // one with none are the same evidence: there was a row. Anything else has exactly one
        // column by the check in `plan_one`.
        values.push(row.first().cloned().unwrap_or(Datum::Null));
        if wanted.is_some_and(|wanted| values.len() >= wanted) {
            break;
        }
    }
    Ok(values)
}

/// What a subquery expression evaluates to, given the values its plan produced.
///
/// Every rule here is a line of `tests/corpus/pg19_subquery_expr.txt`. The order of the two
/// guards at the top of the comparing kinds is the trap the capture exists for: **empty is decided
/// before NULL**, so `NULL IN (SELECT … no rows)` is `false` where `NULL IN (1)` is NULL.
pub(super) fn value(sub: &SubqueryExpr, operand: Option<Datum>) -> Result<Datum> {
    let Some(values) = sub.run.as_deref() else {
        return Err(SqlError::Internal(format!(
            "{} reached the row evaluator without being run",
            sub.kind.describe()
        )));
    };
    Ok(match sub.kind {
        // No rows is NULL, one row is the value, and two is an error rather than the first of
        // them. The error is per *execution*, which is why it is raised here and not when the
        // subquery was planned.
        SubqueryKind::Scalar => match values {
            [] => Datum::Null,
            [only] => only.clone(),
            _ => return Err(SqlError::CardinalityViolation),
        },
        // A row is a row whatever is in it: `EXISTS (SELECT NULL FROM t)` is true.
        SubqueryKind::Exists { negated } => Datum::Bool(values.is_empty() == negated),
        // `IN` is `= ANY` and `NOT IN` is `<> ALL`, measured side by side rather than reasoned —
        // see `crate::plan::subquery`. One implementation of the asymmetric NULL rule.
        SubqueryKind::In { negated } => quantified(
            if negated {
                BinaryOp::NotEq
            } else {
                BinaryOp::Eq
            },
            negated,
            operand,
            values,
        ),
        SubqueryKind::Quantified { op, all } => quantified(op, all, operand, values),
    })
}

/// `x <op> ANY (values)` and `x <op> ALL (values)`, three-valued.
///
/// Four rules, in the order they are checked, and each is a captured line:
///
/// 1. **no values at all** — `ANY` is false and `ALL` is true, *whatever* the left-hand side is.
///    `NULL = ANY (empty)` is `f` and `NULL <> ALL (empty)` is `t`, which is the one place a NULL
///    operand does not make the answer unknown.
/// 2. **a NULL left-hand side** over a non-empty set is unknown; nothing in the set can decide it.
/// 3. a **definite** answer wins outright: one true makes an `ANY` true and one false makes an
///    `ALL` false, however many NULLs are beside it. `100 = ANY (100, 200, NULL)` is `t` and
///    `100 > ALL (100, 200, NULL)` is `f`.
/// 4. with no definite answer, a NULL anywhere makes it unknown — `300 > ALL (100, 200, NULL)` is
///    NULL — and a set of definite non-answers is `ALL`'s own value: false for `ANY`, true for
///    `ALL`.
fn quantified(op: BinaryOp, all: bool, operand: Option<Datum>, values: &[Datum]) -> Datum {
    if values.is_empty() {
        return Datum::Bool(all);
    }
    let Some(operand) = operand else {
        return Datum::Null;
    };
    if matches!(operand, Datum::Null) {
        return Datum::Null;
    }
    let mut unknown = false;
    for value in values {
        if matches!(value, Datum::Null) {
            unknown = true;
            continue;
        }
        let holds = compare(op, &operand, value);
        if all {
            if !holds {
                return Datum::Bool(false);
            }
        } else if holds {
            return Datum::Bool(true);
        }
    }
    if unknown {
        Datum::Null
    } else {
        Datum::Bool(all)
    }
}

/// One comparison, in the ordering everything else in this crate compares by.
///
/// The connectives cannot arrive: PostgreSQL's grammar has no `x AND ANY (…)`, and the lowering
/// refuses every operator but the six. `false` for them is the answer that keeps this total
/// without a panic on a shape the parser will not produce.
fn compare(op: BinaryOp, left: &Datum, right: &Datum) -> bool {
    let ordering = left.pg_cmp(right);
    match op {
        BinaryOp::Eq => ordering.is_eq(),
        BinaryOp::NotEq => !ordering.is_eq(),
        BinaryOp::Lt => ordering.is_lt(),
        BinaryOp::LtEq => ordering.is_le(),
        BinaryOp::Gt => ordering.is_gt(),
        BinaryOp::GtEq => ordering.is_ge(),
        BinaryOp::And | BinaryOp::Or => false,
    }
}

/// Every expression a *statement* was written with, in one place.
///
/// Its own walk rather than a method on `Select`, for the reason
/// `crate::exec::fragment::collect_columns` gives for its own: the statement type is shared with
/// another lane, and a walk this module owns is one hunk fewer to resolve when `main` is merged.
fn for_each_written_expr(select: &Select, visit: &mut impl FnMut(&Expr)) {
    for item in &select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            visit(expr);
        }
    }
    for join in &select.joins {
        if let Some(on) = &join.on {
            visit(on);
        }
    }
    for expr in select
        .filter
        .iter()
        .chain(&select.having)
        .chain(&select.limit)
        .chain(&select.offset)
        .chain(&select.group_by)
    {
        visit(expr);
    }
    for item in &select.order_by {
        visit(&item.expr);
    }
}

/// The same walk, mutably. Written twice rather than made generic over the borrow: the two
/// callers want different things (one asks a question, one rewrites), and a macro or a trait to
/// share nine lines would be harder to read than the nine lines.
fn for_each_written_expr_mut(select: &mut Select, visit: &mut impl FnMut(&mut Expr)) {
    for item in &mut select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            visit(expr);
        }
    }
    for join in &mut select.joins {
        if let Some(on) = &mut join.on {
            visit(on);
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
        visit(expr);
    }
    for item in &mut select.order_by {
        visit(&mut item.expr);
    }
}

/// Every expression a *plan node* holds, in one place. Recursive over the tree.
fn for_each_node_expr_mut(node: &mut Node, visit: &mut impl FnMut(&mut Expr)) {
    match node {
        Node::Filter { input, predicate } => {
            visit(predicate);
            for_each_node_expr_mut(input, visit);
        }
        Node::Project { input, exprs } => {
            for expr in exprs {
                visit(expr);
            }
            for_each_node_expr_mut(input, visit);
        }
        Node::Sort { input, keys } => {
            for key in keys {
                visit(&mut key.expr);
            }
            for_each_node_expr_mut(input, visit);
        }
        Node::Aggregate {
            input,
            keys,
            aggregates,
            having,
            ..
        } => {
            for expr in keys.iter_mut().chain(having.iter_mut()) {
                visit(expr);
            }
            for AggregateSpec { arg, .. } in aggregates {
                if let Some(arg) = arg {
                    visit(arg);
                }
            }
            for_each_node_expr_mut(input, visit);
        }
        Node::NestedLoop {
            outer, residual, ..
        } => {
            if let Some(residual) = residual {
                visit(residual);
            }
            for_each_node_expr_mut(outer, visit);
        }
        Node::Limit { input, .. } | Node::Distinct { input } | Node::Derived { input, .. } => {
            for_each_node_expr_mut(input, visit);
        }
        // A routed aggregate carries the row plan it falls back to, and that plan is the one that
        // runs when a fragment refuses — so a subquery in it has to be resolved whichever way the
        // decision went. The fragment half has no subquery in it by construction
        // (`crate::exec::fragment::push_filter` refuses one).
        Node::Columnar(columnar) => for_each_node_expr_mut(&mut columnar.fallback, visit),
        Node::OneRow
        | Node::CatalogView { .. }
        | Node::SeqScan { .. }
        | Node::PointGet { .. }
        | Node::IndexLookup { .. } => {}
    }
}

/// Every expression under this one, itself included, outermost first.
fn walk(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            walk(left, visit);
            walk(right, visit);
        }
        Expr::Not(inner) => walk(inner, visit),
        Expr::IsNull { operand, .. } => walk(operand, visit),
        Expr::InList { operand, list, .. } => {
            walk(operand, visit);
            for item in list {
                walk(item, visit);
            }
        }
        Expr::Aggregate(call) => {
            for arg in &call.args {
                walk(arg, visit);
            }
        }
        // **Not into the sub-select.** A subquery's own expressions belong to its own plan, which
        // is walked separately once it has one; visiting them here would type them against the
        // outer statement's scope.
        Expr::Subquery(sub) => {
            if let Some(operand) = &sub.operand {
                walk(operand, visit);
            }
        }
        Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::Column { .. }
        | Expr::Ordinal { .. }
        | Expr::Default
        | Expr::Sequence(_) => {}
    }
}

/// The same walk, mutably, and **innermost first** — which is the half that matters. A subquery
/// nested inside another one has to be planned and run before the one holding it, because the
/// outer one's rows are what the inner one is asked about.
fn walk_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr) -> Result<()>) -> Result<()> {
    match expr {
        Expr::Binary { left, right, .. } => {
            walk_mut(left, visit)?;
            walk_mut(right, visit)?;
        }
        Expr::Not(inner) => walk_mut(inner, visit)?,
        Expr::IsNull { operand, .. } => walk_mut(operand, visit)?,
        Expr::InList { operand, list, .. } => {
            walk_mut(operand, visit)?;
            for item in list {
                walk_mut(item, visit)?;
            }
        }
        Expr::Aggregate(call) => {
            for arg in &mut call.args {
                walk_mut(arg, visit)?;
            }
        }
        Expr::Subquery(sub) => {
            if let Some(operand) = &mut sub.operand {
                walk_mut(operand, visit)?;
            }
        }
        Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::Column { .. }
        | Expr::Ordinal { .. }
        | Expr::Default
        | Expr::Sequence(_) => {}
    }
    visit(expr)
}
