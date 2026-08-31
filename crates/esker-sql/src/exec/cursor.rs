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

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::exec::query::successor;
use crate::plan::{BinaryOp, Expr, Node, SortKey};
use crate::row;
use crate::value::{ColumnType, Datum};

/// Rows read from the store in one round trip.
const SCAN_CHUNK: u32 = 1024;

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
