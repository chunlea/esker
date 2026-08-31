//! `SELECT`: choosing an access path, and pulling rows through it.
//!
//! Two halves. [`plan`] turns a lowered `SELECT` into a tree of [`Node`]s — resolving every column
//! name to a position and every `WHERE` clause to an access path — and [`Rows`] pulls rows through
//! that tree one at a time.
//!
//! # Where the streaming stops, and why
//!
//! Every node here is a pull iterator, so a `LIMIT 1` over a million-row table reads one chunk and
//! stops. The one exception is [`Node::Sort`], which cannot stream by definition: the last row of
//! its input can be the first row of its output. It buffers, and the buffer is bounded — past
//! [`SORT_LIMIT`] rows it is `53400`, which is an honest refusal rather than an unbounded
//! allocation on behalf of a client (`CLAUDE.md`: predictable tail latency, no unbounded stalls).
//! `TODO(post-v1)`: an external sort, which is the thing that lifts the limit rather than raising
//! it.
//!
//! The scan is chunked rather than read whole. `Txn::scan` returns a `Vec`, so a scan of the whole
//! table would be the whole table in memory; asking for [`SCAN_CHUNK`] keys at a time and
//! restarting after the last one is the same range read in bounded pieces.

use std::cmp::Ordering;

use crate::backend::Txn;
use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::plan::{BinaryOp, Expr, Node, Select, SelectItem, SortKey};
use crate::row;
use crate::value::{ColumnType, Datum};

/// Rows read from the store in one round trip.
const SCAN_CHUNK: u32 = 1024;

/// The most rows a `Sort` will hold. Past it, `53400` rather than an unbounded allocation.
pub(super) const SORT_LIMIT: usize = 1_000_000;

/// A planned query, with everything the executor needs to describe its output before running it.
#[derive(Debug)]
pub(super) struct Planned {
    /// The tree to pull rows through.
    pub(super) node: Node,
    /// One name and type per output column, for `RowDescription`.
    pub(super) columns: Vec<(String, ColumnType)>,
    /// The table's name, for `EXPLAIN`.
    pub(super) table: String,
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
        let predicate = resolve(filter, Some(table))?;
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
pub(super) fn plan(select: &Select, tenant: u64, table: Option<&TableDef>) -> Result<Planned> {
    let mut node = match table {
        None => Node::OneRow,
        Some(table) => access_path(select.filter.as_ref(), tenant, table)?,
    };

    if let Some(filter) = &select.filter {
        let predicate = resolve(filter, table)?;
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
                    expr: resolve(&dealias(&item.expr, select), table)?,
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

    let columns = output_columns(select, table)?;
    let exprs = projection_exprs(select, table)?;
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
        table: table.map_or_else(|| "-".to_owned(), |table| table.name.clone()),
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
                (Expr::Column(name), Expr::Literal(literal)) => (name, literal, *op),
                (Expr::Literal(literal), Expr::Column(name)) => (name, literal, flip(*op)),
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
                (Expr::Column(name), Expr::Literal(literal))
                | (Expr::Literal(literal), Expr::Column(name)) => (name, literal),
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
    resolve(expr, Some(table))
}

/// `ORDER BY x` where `x` is an output alias means the expression that alias names.
fn dealias(expr: &Expr, select: &Select) -> Expr {
    let Expr::Column(name) = expr else {
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
fn resolve(expr: &Expr, table: Option<&TableDef>) -> Result<Expr> {
    Ok(match expr {
        Expr::Column(name) => {
            let table = table.ok_or_else(|| SqlError::UndefinedColumn(name.clone()))?;
            let at = table
                .column(name)
                .ok_or_else(|| SqlError::UndefinedColumn(name.clone()))?;
            Expr::Ordinal {
                at,
                ty: table.columns[at].ty,
            }
        }
        Expr::Binary { op, left, right } => {
            let (left, right) = (resolve(left, table)?, resolve(right, table)?);
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
        Expr::Not(operand) => Expr::Not(Box::new(resolve(operand, table)?)),
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, table)?),
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
fn output_columns(select: &Select, table: Option<&TableDef>) -> Result<Vec<(String, ColumnType)>> {
    let mut columns = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard => {
                let table = table.ok_or_else(|| SqlError::Syntax {
                    message: "SELECT * with no tables specified is not valid".to_owned(),
                    position: None,
                })?;
                columns.extend(
                    table
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), column.ty)),
                );
            }
            SelectItem::Expr { expr, alias } => {
                let ty = expr_type(expr, table)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column(name) => name.clone(),
                    _ => "?column?".to_owned(),
                });
                columns.push((name, ty));
            }
        }
    }
    Ok(columns)
}

/// One expression per output column, with `*` expanded.
fn projection_exprs(select: &Select, table: Option<&TableDef>) -> Result<Vec<Expr>> {
    let mut exprs = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard => {
                let table = table.ok_or_else(|| SqlError::Syntax {
                    message: "SELECT * with no tables specified is not valid".to_owned(),
                    position: None,
                })?;
                exprs.extend((0..table.columns.len()).map(|at| Expr::Ordinal {
                    at,
                    ty: table.columns[at].ty,
                }));
            }
            SelectItem::Expr { expr, .. } => exprs.push(resolve(expr, table)?),
        }
    }
    Ok(exprs)
}

/// What type an output column has. A literal with no column to take a type from falls back the way
/// PostgreSQL does: a quoted string is `text`, an integer is `bigint`.
fn expr_type(expr: &Expr, table: Option<&TableDef>) -> Result<ColumnType> {
    use crate::plan::Literal;
    Ok(match expr {
        Expr::Column(name) => {
            let table = table.ok_or_else(|| SqlError::UndefinedColumn(name.clone()))?;
            let at = table
                .column(name)
                .ok_or_else(|| SqlError::UndefinedColumn(name.clone()))?;
            table.columns[at].ty
        }
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
        columns: Vec<ColumnType>,
        next: Vec<u8>,
        end: Vec<u8>,
        batch: std::vec::IntoIter<(bytes::Bytes, bytes::Bytes)>,
        exhausted: bool,
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
                exhausted: false,
            },
            Node::PointGet { .. } | Node::IndexLookup { .. } => Kind::Point {
                node: node.clone(),
                looked: false,
            },
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
                exhausted,
            } => {
                loop {
                    if let Some((key, value)) = batch.next() {
                        *next = successor(&key);
                        return Ok(Some(row::decode_row(columns, &value)?));
                    }
                    if *exhausted {
                        return Ok(None);
                    }
                    let read = self.txn.scan(next, end, SCAN_CHUNK)?;
                    // A short chunk means the range is finished; a full one might not be, so the
                    // next call asks again from after the last key it saw.
                    *exhausted = read.len() < SCAN_CHUNK as usize;
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

/// A point read or an index lookup: at most one row, and the store asked at most twice.
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
                Some(value) => row::decode_row(columns, &value).map(Some),
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

/// The first key after `key`. Appending a zero byte works for any key: nothing sorts between `k`
/// and `k ++ 0x00`.
fn successor(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
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
        Expr::Column(name) => {
            return Err(SqlError::Internal(format!(
                "column \"{name}\" reached the executor unresolved"
            )));
        }
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),

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

fn truth(value: &Datum) -> Result<Option<bool>> {
    match value {
        Datum::Bool(value) => Ok(Some(*value)),
        Datum::Null => Ok(None),
        other => Err(SqlError::DatatypeMismatch(format!(
            "argument of AND/OR must be type boolean, not {other:?}"
        ))),
    }
}
