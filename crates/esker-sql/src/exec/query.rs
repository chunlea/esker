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

use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::plan::{BinaryOp, Expr, Node, Select, SelectItem, SortKey};
use crate::row;
use crate::value::{ColumnType, Datum};

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
