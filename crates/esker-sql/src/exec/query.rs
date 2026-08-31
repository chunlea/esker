//! `SELECT`, planned: choosing an access path and resolving every name to a position.
//!
//! [`plan`] turns a lowered `SELECT` into a tree of [`Node`]s, and [`crate::exec::cursor`] pulls
//! rows through that tree. The split is the usual one and it is worth keeping: everything here
//! decides *what to read*, and nothing here reads anything.
//!
//! # What the planner is allowed to decide
//!
//! Rule-based, and the rules are few enough to name:
//!
//! 1. A `WHERE` that pins the **whole primary key** to constants is a point read, not a scan.
//! 2. A `WHERE` that pins the whole key of a **unique index** is a lookup in that index followed by
//!    a point read of the row it names.
//! 3. A `WHERE` that bounds a single-column primary key **narrows the scanned range** instead of
//!    filtering every row out of it.
//!
//! Anything else is a sequential scan with a filter over it, which is always correct and sometimes
//! slow. `EXPLAIN` prints which one was chosen, because a plan a user cannot see is a plan they
//! cannot fix.
//!
//! Only conjunctions count towards any of it. A constant under an `OR` is not *required* by the
//! query, and reading only its range would silently lose the rows the other branch matches — which
//! is the kind of wrong answer nothing reports.

use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Result, SqlError};
use crate::plan::{BinaryOp, Expr, Node, Select, SelectItem, SortKey};
use crate::row;
use crate::value::{ColumnType, Datum};

/// The tables a column reference in one query may name, in the order their columns appear in a
/// row.
///
/// A join's output row is the outer table's columns followed by the inner table's, so a scope of
/// two tables is also the map from a name to a position in that combined row — which is the whole
/// of what resolution needs, and the reason this is a list rather than a pair.
#[derive(Debug, Default)]
pub(super) struct Scope<'a> {
    /// In the order their columns appear in a row — which for a join is the order the executor
    /// reads them, outer first, and **not** necessarily the order the user wrote them in.
    tables: Vec<&'a TableDef>,
    /// Indexes into `tables`, in the order the user wrote them. `SELECT *` expands in this order,
    /// because the columns a user gets back must not depend on which side the planner chose to
    /// drive the loop from.
    written: Vec<usize>,
}

impl<'a> Scope<'a> {
    /// No tables: `SELECT 1`.
    fn empty() -> Self {
        Scope {
            tables: Vec::new(),
            written: Vec::new(),
        }
    }

    /// One table, which is every statement that is not a join.
    pub(super) fn single(table: &'a TableDef) -> Self {
        Scope {
            tables: vec![table],
            written: vec![0],
        }
    }

    /// Two tables: the first is the one the loop is driven from, and `swapped` says whether that
    /// is the one the user wrote second.
    fn joined(outer: &'a TableDef, inner: &'a TableDef, swapped: bool) -> Self {
        Scope {
            tables: vec![outer, inner],
            written: if swapped { vec![1, 0] } else { vec![0, 1] },
        }
    }

    /// Where `table`'s columns start in a row.
    fn offset(&self, table: usize) -> usize {
        self.tables[..table]
            .iter()
            .map(|table| table.columns.len())
            .sum()
    }

    /// The columns `t.*` expands to, or every column for `*`.
    ///
    /// A qualifier that names no table in the query is the same `42P01` a qualified *column*
    /// reference gives, which is what a real server answers for `SELECT wrong.* FROM o`.
    fn expand(
        &self,
        qualifier: Option<&str>,
    ) -> Result<std::vec::IntoIter<(usize, &'a ColumnDef)>> {
        if let Some(qualifier) = qualifier {
            let index = self
                .tables
                .iter()
                .position(|table| table.name == qualifier)
                .ok_or_else(|| SqlError::MissingFromEntry(qualifier.to_owned()))?;
            let offset = self.offset(index);
            let columns: Vec<_> = self.tables[index]
                .user_columns()
                .map(|(at, column)| (offset + at, column))
                .collect();
            return Ok(columns.into_iter());
        }
        let columns: Vec<_> = self
            .written
            .iter()
            .flat_map(|&index| {
                let offset = self.offset(index);
                self.tables[index]
                    .user_columns()
                    .map(move |(at, column)| (offset + at, column))
            })
            .collect();
        Ok(columns.into_iter())
    }

    /// A column reference, resolved to a position in the combined row.
    ///
    /// The three failures are PostgreSQL's, captured from a real server rather than invented:
    /// a qualifier naming a table the query does not have is `42P01 missing FROM-clause entry for
    /// table "x"`; a name no table has is `42703`; and a bare name **two** tables have is `42702
    /// column reference "x" is ambiguous` — which is an error rather than a silent choice of the
    /// first one, because the user's intent is genuinely unknown and guessing it returns the wrong
    /// column without saying so.
    fn resolve_column(&self, qualifier: Option<&str>, name: &str) -> Result<(usize, ColumnType)> {
        if let Some(qualifier) = qualifier {
            let index = self
                .tables
                .iter()
                .position(|table| table.name == qualifier)
                .ok_or_else(|| SqlError::MissingFromEntry(qualifier.to_owned()))?;
            let at = self.tables[index].column(name).ok_or_else(|| {
                if let Some(system) = SYSTEM_COLUMNS.iter().find(|system| **system == name) {
                    return SqlError::unsupported(format!("the system column {system}"));
                }
                SqlError::UndefinedQualifiedColumn {
                    qualifier: qualifier.to_owned(),
                    column: name.to_owned(),
                }
            })?;
            return Ok((self.offset(index) + at, self.tables[index].columns[at].ty));
        }

        let mut found = None;
        for (index, table) in self.tables.iter().enumerate() {
            if let Some(at) = table.column(name) {
                if found.is_some() {
                    return Err(SqlError::AmbiguousColumn(name.to_owned()));
                }
                found = Some((self.offset(index) + at, table.columns[at].ty));
            }
        }
        found.ok_or_else(|| undefined_column(name))
    }
}

/// A planned query, with everything the executor needs to describe its output before running it.
#[derive(Debug)]
pub(super) struct Planned {
    /// The tree to pull rows through.
    pub(super) node: Node,
    /// One name and type per output column, for `RowDescription`.
    pub(super) columns: Vec<(String, ColumnType)>,
    /// The table's name, for `EXPLAIN`.
    pub(super) table: String,
    /// The table's column names, so `EXPLAIN` can print the names a user typed rather than the
    /// positions the executor resolved them to.
    pub(super) column_names: Vec<String>,
}

/// The access path and filter for every row of `table` a predicate matches — the half of a plan
/// that `UPDATE` and `DELETE` share with `SELECT`.
///
/// It yields whole rows, because a statement that rewrites a row needs all of it: the columns it
/// is not changing still have to be written back, and the index entries it is replacing were built
/// from the old ones.
pub(super) fn matching_rows(filter: Option<&Expr>, tenant: u64, table: &TableDef) -> Result<Node> {
    let mut node = access_path(filter, tenant, table)?;
    if let Some(filter) = filter {
        let predicate = resolve(filter, &Scope::single(table))?;
        check_predicate(&predicate)?;
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }
    Ok(node)
}

/// Turns a lowered `SELECT` into a plan against a table.
///
/// `table` is `None` for `SELECT 1`, which has no table and one row.
pub(super) fn plan(
    select: &Select,
    tenant: u64,
    table: Option<&TableDef>,
    inner: Option<&TableDef>,
) -> Result<Planned> {
    // Which side drives the loop. An inner join is commutative, so this is free to choose — and
    // it has to choose, because the probe only works on the *inner* side: without this, `FROM c
    // JOIN o ON c.id = o.cid` reads the whole of `o` for every row of `c`, while the same query
    // written the other way round costs one key read per row. A user should not have to know
    // which order to type.
    let (scope, swapped) = match (table, inner) {
        (None, _) => (Scope::empty(), false),
        (Some(table), None) => (Scope::single(table), false),
        (Some(left), Some(right)) => {
            let on = select.join.as_ref().and_then(|join| join.on.as_ref());
            // As written first: a probe on the right-hand table keeps the order the user chose,
            // which keeps `EXPLAIN` easiest to read when both would work.
            if on.is_some_and(|on| {
                probe_for(on, &Scope::joined(left, right, false), right).is_some()
            }) {
                (Scope::joined(left, right, false), false)
            } else if on
                .is_some_and(|on| probe_for(on, &Scope::joined(right, left, true), left).is_some())
            {
                (Scope::joined(right, left, true), true)
            } else {
                (Scope::joined(left, right, false), false)
            }
        }
    };
    let (outer_table, inner_table) = match (table, inner, swapped) {
        (Some(left), Some(right), false) => (Some(left), Some(right)),
        (Some(left), Some(right), true) => (Some(right), Some(left)),
        (table, _, _) => (table, None),
    };

    let mut node = match outer_table {
        None => Node::OneRow,
        // With a join the outer access path only gets the `WHERE` when the whole of it belongs to
        // the outer table. A predicate mentioning the inner one cannot narrow the outer scan --
        // its value is not known until an outer row has been read -- and handing it to
        // `access_path`, which resolves against one table, would be an error rather than a plan.
        Some(table) => {
            let usable = select
                .filter
                .as_ref()
                .filter(|filter| inner_table.is_none() || mentions_only(filter, table));
            access_path(usable, tenant, table)?
        }
    };

    if let Some(join) = &select.join {
        let inner = inner_table.ok_or_else(|| SqlError::UndefinedTable(join.table.clone()))?;
        node = join_node(node, join, &scope, inner)?;
    }

    if let Some(filter) = &select.filter {
        let predicate = resolve(filter, &scope)?;
        check_predicate(&predicate)?;
        // A predicate the access path already guarantees is not re-checked -- but one it only
        // *narrowed* still is, because a range is not an equality.
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }

    // The sort goes *below* the projection, so it can order on a column the target list does not
    // return -- `SELECT n FROM s1 ORDER BY id` is ordinary SQL, and a sort above the projection
    // could not see `id` at all. An `ORDER BY` naming an output alias is substituted first, which
    // is the other half of what PostgreSQL allows.
    if !select.order_by.is_empty() {
        let keys = select
            .order_by
            .iter()
            .map(|item| {
                Ok(SortKey {
                    expr: resolve(&dealias(&item.expr, select), &scope)?,
                    descending: item.descending,
                    // PostgreSQL's default is NULLS LAST ascending and NULLS FIRST descending,
                    // which is one rule: NULL is the largest value, and `DESC` reverses the order
                    // it sits in like everything else.
                    nulls_first: item.nulls_first.unwrap_or(item.descending),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        node = Node::Sort {
            input: Box::new(node),
            keys,
        };
    }

    let columns = output_columns(select, &scope)?;
    let exprs = projection_exprs(select, &scope)?;
    node = Node::Project {
        input: Box::new(node),
        exprs,
    };

    if select.limit.is_some() || select.offset.is_some() {
        node = Node::Limit {
            input: Box::new(node),
            offset: count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0),
            limit: count(select.limit.as_ref(), "LIMIT")?,
        };
    }

    Ok(Planned {
        node,
        columns,
        table: outer_table.map_or_else(|| "-".to_owned(), |table| table.name.clone()),
        // Every column of every table in scope, in row order, so `EXPLAIN` can print the name a
        // user typed for any position the executor resolved.
        column_names: scope
            .tables
            .iter()
            .flat_map(|table| table.columns.iter().map(|column| column.name.clone()))
            .collect(),
    })
}

/// Whether every column reference in an expression belongs to `table`.
fn mentions_only(expr: &Expr, table: &TableDef) -> bool {
    let mut only = true;
    for_each_column(expr, &mut |qualifier, name| {
        only &= qualifier.is_none_or(|qualifier| qualifier == table.name)
            && table.column(name).is_some();
    });
    only
}

fn for_each_column(expr: &Expr, visit: &mut impl FnMut(Option<&str>, &str)) {
    match expr {
        Expr::Column { table, name } => visit(table.as_deref(), name),
        Expr::Binary { left, right, .. } => {
            for_each_column(left, visit);
            for_each_column(right, visit);
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => for_each_column(operand, visit),
        _ => {}
    }
}

/// The join, and the choice this whole unit exists to make: read the inner table once per outer
/// row, or read one key.
///
/// A probe is possible when the `ON` condition is an equality between one outer column and one
/// inner column, and that inner column is the inner table's whole primary key or the whole key of
/// one of its unique indexes. Those are the two access paths the planner already has for a
/// `WHERE` — a join is a `WHERE` whose right-hand side changes per row — and reusing them is why
/// this is a small addition rather than a second planner.
///
/// Anything else materialises the inner table and pairs every outer row with all of it, which is
/// always correct and, for a large inner table, always slow. `EXPLAIN` says which, because that is
/// the difference a user changes their schema over.
fn join_node(
    outer: Node,
    join: &crate::plan::Join,
    scope: &Scope<'_>,
    inner: &TableDef,
) -> Result<Node> {
    let probe = join
        .on
        .as_ref()
        .and_then(|on| probe_for(on, scope, inner))
        .unwrap_or(crate::plan::Probe::Materialize);
    // A probe answers the equality exactly, so the condition it came from is not re-checked. A
    // materialised inner side has nothing to answer it, so the whole condition is the filter.
    let residual = match (&probe, &join.on) {
        (crate::plan::Probe::Materialize, Some(on)) => Some(resolve(on, scope)?),
        _ => None,
    };
    Ok(Node::NestedLoop {
        outer: Box::new(outer),
        inner_table_id: inner.id,
        inner_table: inner.name.clone(),
        inner_columns: inner.column_types(),
        probe,
        residual,
    })
}

/// The probe an `ON` condition allows, or `None` for one that needs the inner table read whole.
fn probe_for(on: &Expr, scope: &Scope<'_>, inner: &TableDef) -> Option<crate::plan::Probe> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = on
    else {
        return None;
    };
    // The inner table's columns start where the outer's end, so a resolved position at or above
    // that offset belongs to the inner side.
    let boundary = scope.offset(scope.tables.len() - 1);
    let (left, right) = (resolve(left, scope).ok()?, resolve(right, scope).ok()?);
    let (Expr::Ordinal { at: a, .. }, Expr::Ordinal { at: b, .. }) = (&left, &right) else {
        return None;
    };
    let (outer_at, inner_at) = match (*a < boundary, *b < boundary) {
        (true, false) => (*a, *b - boundary),
        (false, true) => (*b, *a - boundary),
        // Both sides on one table joins nothing; it is a filter, and the materialised path
        // applies it correctly.
        _ => return None,
    };
    if inner.primary_key == [inner_at] {
        return Some(crate::plan::Probe::PrimaryKey { outer: outer_at });
    }
    inner
        .indexes
        .iter()
        .find(|index| index.unique && index.columns == [inner_at])
        .map(|index| crate::plan::Probe::UniqueIndex {
            index_id: index.id,
            index_name: index.name.clone(),
            outer: outer_at,
            primary_key_types: inner.primary_key_types(),
        })
}

/// Rule 1, 2 and 3 from `plan::query`: pin the whole primary key, bound its first column, or pin a
/// unique index's whole key. Otherwise a scan.
fn access_path(filter: Option<&Expr>, tenant: u64, table: &TableDef) -> Result<Node> {
    let columns = table.column_types();
    let Some(filter) = filter else {
        return Ok(seq_scan(tenant, table, &columns, false));
    };
    let equalities = equality_constants(filter, table)?;

    // Rule 1: the whole primary key, pinned.
    if let Some(key) = pinned(&table.primary_key, &equalities) {
        return Ok(Node::PointGet {
            table_id: table.id,
            columns,
            key,
        });
    }

    // Rule 3: a unique index's whole key, pinned. A key with a NULL in it is not pinned at all --
    // `= NULL` is never true -- so this cannot pick up the many-NULLs case by accident.
    for index in &table.indexes {
        if !index.unique {
            continue;
        }
        if let Some(key) = pinned(&index.columns, &equalities)
            && row::unique_index_key_is_unique_by_value(&key)
        {
            return Ok(Node::IndexLookup {
                table_id: table.id,
                columns,
                index_id: index.id,
                index_name: index.name.clone(),
                key,
                primary_key_types: table.primary_key_types(),
            });
        }
    }

    // Rule 2: a bound on the first primary key column narrows the range.
    Ok(narrowed_scan(tenant, table, &columns, filter))
}

fn seq_scan(tenant: u64, table: &TableDef, columns: &[ColumnType], narrowed: bool) -> Node {
    let (start, end) = row::table_row_range(tenant, table.id);
    Node::SeqScan {
        table_id: table.id,
        columns: columns.to_vec(),
        start,
        end,
        narrowed,
    }
}

/// Rule 2: a bound on the *first* primary key column moves the ends of the scanned range.
///
/// Only the first, and only under `AND`. The key encoding is order-preserving, so `id > 5` on a
/// single-column key really does mean "start after the key for 5" — but `b > 5` on a key of
/// `(a, b)` does not mean anything about where to start, because the rows for `b = 9` are spread
/// through every value of `a`. The filter still runs either way; narrowing only decides how much
/// is read.
fn narrowed_scan(tenant: u64, table: &TableDef, columns: &[ColumnType], filter: &Expr) -> Node {
    let (mut start, mut end) = row::table_row_range(tenant, table.id);
    let mut narrowed = false;

    if table.primary_key.len() == 1 {
        let first = table.primary_key[0];
        for (op, value) in bounds(filter, table, first) {
            let Ok(key) = row::row_key(tenant, table.id, &[value]) else {
                continue;
            };
            match op {
                BinaryOp::Gt if key >= start => {
                    start = successor(&key);
                    narrowed = true;
                }
                BinaryOp::GtEq if key > start => {
                    start = key;
                    narrowed = true;
                }
                BinaryOp::Lt if key < end => {
                    end = key;
                    narrowed = true;
                }
                BinaryOp::LtEq if successor(&key) < end => {
                    end = successor(&key);
                    narrowed = true;
                }
                _ => {}
            }
        }
    }

    Node::SeqScan {
        table_id: table.id,
        columns: columns.to_vec(),
        start,
        end,
        narrowed,
    }
}

/// Every `column <op> constant` on `ordinal` that the predicate *requires* — conjunctions only,
/// for the same reason `equality_constants` walks only `AND`.
fn bounds(expr: &Expr, table: &TableDef, ordinal: usize) -> Vec<(BinaryOp, Datum)> {
    let mut found = Vec::new();
    collect_bounds(expr, table, ordinal, &mut found);
    found
}

fn collect_bounds(
    expr: &Expr,
    table: &TableDef,
    ordinal: usize,
    found: &mut Vec<(BinaryOp, Datum)>,
) {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_bounds(left, table, ordinal, found);
            collect_bounds(right, table, ordinal, found);
        }
        Expr::Binary { op, left, right } if op.is_comparison() => {
            // `5 < id` is `id > 5` with the operands the other way round.
            let (name, literal, op) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(literal)) => (name, literal, *op),
                (Expr::Literal(literal), Expr::Column { name, .. }) => (name, literal, flip(*op)),
                _ => return,
            };
            if table.column(name) != Some(ordinal) {
                return;
            }
            let column = &table.columns[ordinal];
            if let Ok(value) = literal.assign(column.ty, &column.name)
                && !matches!(value, Datum::Null)
            {
                found.push((op, value));
            }
        }
        _ => {}
    }
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Every `column = constant` the predicate requires, as `(ordinal, value)`.
///
/// Only conjunctions count: a constant under an `OR` is not required, and treating it as though it
/// were would return the wrong rows. That is the whole reason this walks `AND` and stops.
fn equality_constants(expr: &Expr, table: &TableDef) -> Result<Vec<(usize, Datum)>> {
    let mut found = Vec::new();
    collect_equalities(expr, table, &mut found)?;
    Ok(found)
}

fn collect_equalities(
    expr: &Expr,
    table: &TableDef,
    found: &mut Vec<(usize, Datum)>,
) -> Result<()> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_equalities(left, table, found)?;
            collect_equalities(right, table, found)
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let pair = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(literal))
                | (Expr::Literal(literal), Expr::Column { name, .. }) => (name, literal),
                _ => return Ok(()),
            };
            let Some(ordinal) = table.column(pair.0) else {
                return Ok(());
            };
            let column = &table.columns[ordinal];
            // A literal that will not assign is not a plan decision; the filter will report it.
            if let Ok(value) = pair.1.assign(column.ty, &column.name)
                && !matches!(value, Datum::Null)
            {
                found.push((ordinal, value));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The values for `ordinals`, in order, when every one of them is pinned.
fn pinned(ordinals: &[usize], equalities: &[(usize, Datum)]) -> Option<Vec<Datum>> {
    ordinals
        .iter()
        .map(|ordinal| {
            equalities
                .iter()
                .find(|(at, _)| at == ordinal)
                .map(|(_, value)| value.clone())
        })
        .collect()
}

/// Resolves a column reference against one table — what `UPDATE`'s `SET` expressions need.
pub(super) fn resolve_against(expr: &Expr, table: &TableDef) -> Result<Expr> {
    resolve(expr, &Scope::single(table))
}

/// `ORDER BY x` where `x` is an output alias means the expression that alias names.
fn dealias(expr: &Expr, select: &Select) -> Expr {
    let Expr::Column { table: None, name } = expr else {
        return expr.clone();
    };
    for item in &select.projection {
        if let SelectItem::Expr {
            expr: aliased,
            alias: Some(alias),
        } = item
            && alias == name
        {
            return aliased.clone();
        }
    }
    expr.clone()
}

/// Replaces every column name with its position and type. A name the table does not have is
/// `42703` here rather than a wrong answer later.
/// The six columns every PostgreSQL table has and this one does not, so that asking for one is
/// answered by name (contract C2) rather than with `42703 column "ctid" does not exist`, which
/// would be untrue — on a real server it does.
///
/// Measured rather than recalled: these six answer on a real PostgreSQL 19 and `oid` does not,
/// because a table's OID column went away in PostgreSQL 12.
///
/// `ctid` is the interesting one and the reason this list exists at all. It is a *physical*
/// address — `(block, offset)` — and it moves when the row is rewritten. The internal row id a
/// keyless table gets here is a *logical* identity that never moves, because an index entry points
/// at the row key and a key that moved would mean rewriting every index on every update. They are
/// not the same thing under different names, so `ctid` is refused rather than answered with
/// something that would behave differently the first time somebody updated a row.
fn undefined_column(name: &str) -> SqlError {
    if let Some(system) = SYSTEM_COLUMNS.iter().find(|system| **system == name) {
        return SqlError::unsupported(format!("the system column {system}"));
    }
    SqlError::UndefinedColumn(name.to_owned())
}

const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

fn resolve(expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
    Ok(match expr {
        Expr::Column { table, name } => {
            let (at, ty) = scope.resolve_column(table.as_deref(), name)?;
            Expr::Ordinal { at, ty }
        }
        Expr::Binary { op, left, right } => {
            let (left, right) = (resolve(left, scope)?, resolve(right, scope)?);
            let (left, right) = if op.is_comparison() {
                // A literal has no type until something gives it one, and here that something is
                // the other operand. Without this the comparison would run between a `text` and an
                // `int8` and simply be false, which is a wrong answer rather than an error.
                reconcile(*op, left, right)?
            } else {
                (left, right)
            };
            Expr::Binary {
                op: *op,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        Expr::Not(operand) => Expr::Not(Box::new(resolve(operand, scope)?)),
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, scope)?),
            negated: *negated,
        },
        other => other.clone(),
    })
}

/// Gives a literal the type of the column it is being compared against, or says the comparison is
/// between types no operator covers.
fn reconcile(op: BinaryOp, left: Expr, right: Expr) -> Result<(Expr, Expr)> {
    Ok(match (&left, &right) {
        (Expr::Ordinal { ty, .. }, Expr::Literal(literal)) => (
            left.clone(),
            Expr::Literal(retype(*ty, literal, op, false)?),
        ),
        (Expr::Literal(literal), Expr::Ordinal { ty, .. }) => (
            Expr::Literal(retype(*ty, literal, op, true)?),
            right.clone(),
        ),
        _ => (left, right),
    })
}

/// One literal, resolved against a column's type. A literal that will not assign is
/// `42883 operator does not exist`, which is what PostgreSQL answers rather than a type mismatch:
/// from its point of view there is simply no `text = integer` to call.
fn retype(
    ty: ColumnType,
    literal: &crate::plan::Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> Result<crate::plan::Literal> {
    use crate::plan::Literal;
    if matches!(literal, Literal::Null) {
        return Ok(Literal::Null);
    }
    // Assignment would take `42` into a `text` column; comparison will not, and turning the error
    // into a silent `false` would be a wrong answer rather than a missing feature.
    if !literal.comparable_with(ty) {
        return Err(undefined_operator(ty, literal, op, literal_on_the_left));
    }
    match literal.assign(ty, "?column?") {
        // Reduced to a value of the column's own type, so the comparison is between two of them.
        Ok(value) => Ok(match value {
            Datum::Int8(value) => Literal::Integer(value),
            Datum::Bool(value) => Literal::Bool(value),
            Datum::Text(value) => Literal::String(value),
            other => Literal::Typed(Box::new(other)),
        }),
        // A literal of the right *category* that still will not read -- `WHERE ts = 'not a date'`
        // -- keeps the error its input function raised, which says what is actually wrong.
        Err(error) => Err(error),
    }
}

fn undefined_operator(
    ty: ColumnType,
    literal: &crate::plan::Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> SqlError {
    let (left, right) = if literal_on_the_left {
        (literal.type_name(), ty.name())
    } else {
        (ty.name(), literal.type_name())
    };
    SqlError::UndefinedOperator {
        left,
        op: op.symbol(),
        right,
    }
}

/// A `WHERE` clause has to be a boolean. PostgreSQL says so, and a `WHERE t` where `t` is text is
/// a mistake worth catching before it silently keeps every row.
fn check_predicate(expr: &Expr) -> Result<()> {
    match expr {
        Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::Literal(crate::plan::Literal::Bool(_) | crate::plan::Literal::Null)
        | Expr::Ordinal {
            ty: ColumnType::Bool,
            ..
        } => Ok(()),
        _ => Err(SqlError::DatatypeMismatch(
            "argument of WHERE must be type boolean".to_owned(),
        )),
    }
}

/// The name and type of every output column.
///
/// A bare column keeps its name; anything else is `?column?`, which is PostgreSQL's own answer and
/// what `psql` prints as a header.
/// The table a `*` is qualified with, if any.
fn qualifier_of(item: &SelectItem) -> Option<&str> {
    match item {
        SelectItem::QualifiedWildcard(table) => Some(table),
        _ => None,
    }
}

fn output_columns(select: &Select, scope: &Scope<'_>) -> Result<Vec<(String, ColumnType)>> {
    let mut columns = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                if scope.tables.is_empty() {
                    return Err(SqlError::Syntax {
                        message: "SELECT * with no tables specified is not valid".to_owned(),
                        position: None,
                        hint: None,
                    });
                }
                // The user's columns, so an internal row id stays hidden: `SELECT *` on a table
                // with no declared key returns what the user declared and nothing else. Across a
                // join it is every table's, left to right, which is the order PostgreSQL gives,
                // and `t.*` is one table's.
                columns.extend(
                    scope
                        .expand(qualifier_of(item))?
                        .map(|(_, column)| (column.name.clone(), column.ty)),
                );
            }
            SelectItem::Expr { expr, alias } => {
                let ty = expr_type(expr, scope)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { name, .. } => name.clone(),
                    _ => "?column?".to_owned(),
                });
                columns.push((name, ty));
            }
        }
    }
    Ok(columns)
}

/// One expression per output column, with `*` expanded.
fn projection_exprs(select: &Select, scope: &Scope<'_>) -> Result<Vec<Expr>> {
    let mut exprs = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                if scope.tables.is_empty() {
                    return Err(SqlError::Syntax {
                        message: "SELECT * with no tables specified is not valid".to_owned(),
                        position: None,
                        hint: None,
                    });
                }
                exprs.extend(
                    scope
                        .expand(qualifier_of(item))?
                        .map(|(at, column)| Expr::Ordinal { at, ty: column.ty }),
                );
            }
            SelectItem::Expr { expr, .. } => exprs.push(resolve(expr, scope)?),
        }
    }
    Ok(exprs)
}

/// What type an output column has. A literal with no column to take a type from falls back the way
/// PostgreSQL does: a quoted string is `text`, an integer is `bigint`.
fn expr_type(expr: &Expr, scope: &Scope<'_>) -> Result<ColumnType> {
    use crate::plan::Literal;
    Ok(match expr {
        Expr::Column { table, name } => scope.resolve_column(table.as_deref(), name)?.1,
        Expr::Ordinal { ty, .. } => *ty,
        Expr::Literal(Literal::Integer(_)) => ColumnType::Int8,
        Expr::Literal(Literal::Decimal(_)) => ColumnType::Double,

        Expr::Literal(Literal::String(_) | Literal::Null) => ColumnType::Text,
        Expr::Literal(Literal::Typed(value)) => value.column_type().unwrap_or(ColumnType::Text),
        Expr::Literal(Literal::Bool(_))
        | Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. } => ColumnType::Bool,
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),
    })
}

/// A `LIMIT` or `OFFSET` value: an integer, and not a negative one.
fn count(expr: Option<&Expr>, what: &'static str) -> Result<Option<usize>> {
    let Some(expr) = expr else { return Ok(None) };
    let value = expr.evaluate(ColumnType::Int8, what)?;
    match value {
        // `LIMIT NULL` means no limit, which is PostgreSQL's rule and not an oversight.
        Datum::Null => Ok(None),
        Datum::Int8(value) if value < 0 => Err(SqlError::NegativeLimit(what)),
        Datum::Int8(value) => Ok(Some(usize::try_from(value).unwrap_or(usize::MAX))),
        other => Err(SqlError::DatatypeMismatch(format!(
            "argument of {what} must be type bigint, not {other:?}"
        ))),
    }
}

/// The first key after `key`.
///
/// Appending a zero byte works for any key at all: nothing sorts between `k` and `k ++ 0x00`. Both
/// halves need it — the planner to move a range's end past an inclusive bound, the cursor to
/// resume a chunked scan after the last key it saw.
pub(super) fn successor(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}
