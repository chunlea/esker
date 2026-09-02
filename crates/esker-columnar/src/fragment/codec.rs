//! A fragment's bytes.
//!
//! Hand-written little-endian and LEB128 varints behind a version byte, with a CRC32C over the
//! lot — the same rules as everything else this project puts on a wire or a disk (ADR 0002). No
//! magic and no trailer: a fragment arrives inside a frame that already said how long it is.
//!
//! ```text
//! fragment := version:u8 ++ body ++ crc32c:u32          the CRC covers version ++ body
//! body     := table ++ range ++ projection ++ filter ++ output
//!
//! table      := tenant:varint ++ table_id:varint
//! range      := start_len:varint ++ start ++ end_len:varint ++ end
//! projection := count:varint ++ column:varint *          indexes into the file's schema
//! filter     := present:u8 ++ [expr]
//! output     := kind:u8 ++ (rows | aggregates)
//!   rows       := has_limit:u8 ++ [limit:varint]
//!   aggregates := group_count:varint ++ slot:varint *
//!              ++ agg_count:varint ++ (kind:u8 ++ [slot:varint]) *
//!
//! expr := node:u8 ++ ...
//!   1 Column  ++ slot:varint
//!   2 Literal ++ type_tag:u8 ++ value                    tag 0 is NULL; 1..6 are the row tags
//!   3 Compare ++ op:u8 ++ expr ++ expr
//!   4 And     ++ expr ++ expr
//!   5 Or      ++ expr ++ expr
//!   6 Not     ++ expr
//!   7 IsNull  ++ negated:u8 ++ expr
//! ```
//!
//! # Corruption and refusal are different answers
//!
//! **The checksum is verified before anything else is read**, and that ordering is what makes the
//! distinction below meaningful rather than a guess:
//!
//! * a **structural** failure — a short buffer, a truncated varint, a length nothing could hold,
//!   text that is not UTF-8, a boolean byte that is neither 0 nor 1 — is
//!   [`Error::Corruption`]. The bytes do not mean what the format says
//!   they must.
//! * an **unknown** version, node tag, comparison operator, aggregate kind, output kind or type
//!   tag is [`Error::Refused`]. The bytes are intact, so this is a build
//!   that does not implement what was asked, and the caller falls back to a row scan rather than
//!   treating the sender as broken.
//!
//! Getting that backwards would report every rolling upgrade as data corruption.
//!
//! # Depth
//!
//! The expression decoder recurses, so it counts. A message of a few hundred bytes can describe
//! an expression thousands deep, and a decoder without a limit meets it with a stack overflow —
//! a panic on untrusted input by another name (invariant 9). The limit is
//! [`MAX_EXPR_DEPTH`] and it is enforced on the way *down*, before any
//! node is built.

use esker_base::{crc32c, varint};

use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::format::MAX_VALUE_LEN;
use crate::fragment::expr::{CompareOp, Expr};
use crate::fragment::{
    Aggregate, Fragment, KeyRange, MAX_AGGREGATES, MAX_EXPR_DEPTH, MAX_GROUP_BY, MAX_KEY_BOUND,
    Output, TableRef,
};
use crate::value::{ColumnType, MAX_COLUMNS, Value};

/// The version byte every fragment starts with.
pub const FRAGMENT_FORMAT_VERSION: u8 = 1;

/// `Output::Rows`.
const OUTPUT_ROWS: u8 = 0;
/// `Output::Aggregates`.
const OUTPUT_AGGREGATES: u8 = 1;

/// The type tag of an untyped NULL literal, below the six real ones.
const TAG_NULL: u8 = 0;

/// Lays a fragment out as the module documents.
#[must_use]
pub fn encode(fragment: &Fragment) -> Vec<u8> {
    let mut out = vec![FRAGMENT_FORMAT_VERSION];
    varint::put_u64(fragment.table.tenant, &mut out);
    varint::put_u64(fragment.table.table_id, &mut out);
    put_bytes(&fragment.range.start, &mut out);
    put_bytes(&fragment.range.end, &mut out);

    varint::put_u64(fragment.projection.len() as u64, &mut out);
    for column in &fragment.projection {
        varint::put_u64(u64::from(*column), &mut out);
    }

    match &fragment.filter {
        None => out.push(0),
        Some(filter) => {
            out.push(1);
            put_expr(filter, &mut out);
        }
    }

    match &fragment.output {
        Output::Rows { limit } => {
            out.push(OUTPUT_ROWS);
            match limit {
                None => out.push(0),
                Some(limit) => {
                    out.push(1);
                    varint::put_u64(*limit, &mut out);
                }
            }
        }
        Output::Aggregates {
            group_by,
            aggregates,
        } => {
            out.push(OUTPUT_AGGREGATES);
            varint::put_u64(group_by.len() as u64, &mut out);
            for slot in group_by {
                varint::put_u64(u64::from(*slot), &mut out);
            }
            varint::put_u64(aggregates.len() as u64, &mut out);
            for aggregate in aggregates {
                out.push(aggregate.as_u8());
                if let Some(slot) = aggregate.slot() {
                    varint::put_u64(u64::from(slot), &mut out);
                }
            }
        }
    }

    let checksum = crc32c::checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Reads a fragment back.
///
/// See the module docs for why an unknown tag is a refusal and a malformed one is corruption.
pub fn decode(bytes: &[u8]) -> Result<Fragment> {
    if bytes.len() < 5 {
        return Err(Error::corruption(
            "fragment",
            format!(
                "{} bytes is too few for a version and a checksum",
                bytes.len()
            ),
        ));
    }
    let (checked, crc_bytes) = bytes.split_at(bytes.len() - 4);
    let mut stored = [0u8; 4];
    stored.copy_from_slice(crc_bytes);
    let stored = u32::from_le_bytes(stored);
    let actual = crc32c::checksum(checked);
    if actual != stored {
        return Err(Error::corruption(
            "fragment",
            format!("checksum {actual:#010x} does not match the stored {stored:#010x}"),
        ));
    }

    // The bytes are intact from here, so an unknown version is a version and not a bit flip.
    let (&version, body) = checked
        .split_first()
        .unwrap_or_else(|| unreachable!("the length was checked above"));
    if version != FRAGMENT_FORMAT_VERSION {
        return Err(Error::refused(format!(
            "fragment format version {version}, this build reads {FRAGMENT_FORMAT_VERSION}"
        )));
    }

    let mut cursor = Cursor::new(body, "fragment");
    let table = TableRef {
        tenant: cursor.varint("tenant")?,
        table_id: cursor.varint("table id")?,
    };
    let range = KeyRange {
        start: take_bytes(&mut cursor, "range start")?,
        end: take_bytes(&mut cursor, "range end")?,
    };

    let count = cursor.count("projection", 1)?;
    if count > MAX_COLUMNS {
        return Err(Error::refused(format!("a projection of {count} columns")));
    }
    let mut projection = Vec::with_capacity(count);
    for _ in 0..count {
        projection.push(take_u32(&mut cursor, "projection column")?);
    }

    let filter = match cursor.u8("filter presence")? {
        0 => None,
        1 => Some(take_expr(&mut cursor, 1)?),
        other => {
            return Err(Error::corruption(
                "fragment",
                format!("filter presence byte {other}"),
            ));
        }
    };

    let output = take_output(&mut cursor)?;

    cursor.finish()?;
    Ok(Fragment {
        table,
        range,
        projection,
        filter,
        output,
    })
}

/// Reads the output clause, which is where most of a fragment's shape lives.
///
/// Split out of [`decode`] so that neither is long enough to hide a missing bound.
fn take_output(cursor: &mut Cursor<'_>) -> Result<Output> {
    Ok(match cursor.u8("output kind")? {
        OUTPUT_ROWS => Output::Rows {
            limit: match cursor.u8("limit presence")? {
                0 => None,
                1 => Some(cursor.varint("limit")?),
                other => {
                    return Err(Error::corruption(
                        "fragment",
                        format!("limit presence byte {other}"),
                    ));
                }
            },
        },
        OUTPUT_AGGREGATES => {
            let count = cursor.count("group by", 1)?;
            if count > MAX_GROUP_BY {
                return Err(Error::refused(format!("a grouping over {count} columns")));
            }
            let mut group_by = Vec::with_capacity(count);
            for _ in 0..count {
                group_by.push(take_u32(cursor, "group by slot")?);
            }

            let count = cursor.count("aggregates", 1)?;
            if count > MAX_AGGREGATES {
                return Err(Error::refused(format!("{count} aggregates")));
            }
            let mut aggregates = Vec::with_capacity(count);
            for _ in 0..count {
                aggregates.push(take_aggregate(cursor)?);
            }
            Output::Aggregates {
                group_by,
                aggregates,
            }
        }
        other => return Err(Error::refused(format!("fragment output kind {other}"))),
    })
}

fn put_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    varint::put_u64(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn take_bytes(cursor: &mut Cursor<'_>, field: &str) -> Result<Vec<u8>> {
    let len = cursor.count(field, 1)?;
    if len > MAX_KEY_BOUND {
        return Err(Error::corruption(
            "fragment",
            format!("{field} is {len} bytes, over the {MAX_KEY_BOUND} limit"),
        ));
    }
    Ok(cursor.bytes(len, field)?.to_vec())
}

fn take_u32(cursor: &mut Cursor<'_>, field: &str) -> Result<u32> {
    let value = cursor.varint(field)?;
    u32::try_from(value).map_err(|_| Error::corruption("fragment", format!("{field} is {value}")))
}

fn take_aggregate(cursor: &mut Cursor<'_>) -> Result<Aggregate> {
    let kind = cursor.u8("aggregate kind")?;
    Ok(match kind {
        1 => Aggregate::CountStar,
        2 => Aggregate::Count(take_u32(cursor, "aggregate slot")?),
        3 => Aggregate::Sum(take_u32(cursor, "aggregate slot")?),
        4 => Aggregate::Min(take_u32(cursor, "aggregate slot")?),
        5 => Aggregate::Max(take_u32(cursor, "aggregate slot")?),
        other => return Err(Error::refused(format!("aggregate kind {other}"))),
    })
}

fn put_expr(expr: &Expr, out: &mut Vec<u8>) {
    match expr {
        Expr::Column(slot) => {
            out.push(1);
            varint::put_u64(u64::from(*slot), out);
        }
        Expr::Literal(value) => {
            out.push(2);
            put_literal(value, out);
        }
        Expr::Compare { op, left, right } => {
            out.push(3);
            out.push(op.as_u8());
            put_expr(left, out);
            put_expr(right, out);
        }
        Expr::And(left, right) => {
            out.push(4);
            put_expr(left, out);
            put_expr(right, out);
        }
        Expr::Or(left, right) => {
            out.push(5);
            put_expr(left, out);
            put_expr(right, out);
        }
        Expr::Not(operand) => {
            out.push(6);
            put_expr(operand, out);
        }
        Expr::IsNull { operand, negated } => {
            out.push(7);
            out.push(u8::from(*negated));
            put_expr(operand, out);
        }
    }
}

/// Reads one expression node, refusing before it descends past [`MAX_EXPR_DEPTH`].
fn take_expr(cursor: &mut Cursor<'_>, depth: usize) -> Result<Expr> {
    if depth > MAX_EXPR_DEPTH {
        return Err(Error::refused(format!(
            "a filter nested past {MAX_EXPR_DEPTH}"
        )));
    }
    Ok(match cursor.u8("expression node")? {
        1 => Expr::Column(take_u32(cursor, "column slot")?),
        2 => Expr::Literal(take_literal(cursor)?),
        3 => {
            let tag = cursor.u8("comparison operator")?;
            let op = CompareOp::from_u8(tag)
                .ok_or_else(|| Error::refused(format!("comparison operator {tag}")))?;
            Expr::Compare {
                op,
                left: Box::new(take_expr(cursor, depth + 1)?),
                right: Box::new(take_expr(cursor, depth + 1)?),
            }
        }
        4 => Expr::And(
            Box::new(take_expr(cursor, depth + 1)?),
            Box::new(take_expr(cursor, depth + 1)?),
        ),
        5 => Expr::Or(
            Box::new(take_expr(cursor, depth + 1)?),
            Box::new(take_expr(cursor, depth + 1)?),
        ),
        6 => Expr::Not(Box::new(take_expr(cursor, depth + 1)?)),
        7 => Expr::IsNull {
            negated: match cursor.u8("is-null negation")? {
                0 => false,
                1 => true,
                other => {
                    return Err(Error::corruption(
                        "fragment",
                        format!("is-null negation byte {other}"),
                    ));
                }
            },
            operand: Box::new(take_expr(cursor, depth + 1)?),
        },
        other => return Err(Error::refused(format!("expression node {other}"))),
    })
}

fn put_literal(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Int8(v) | Value::TimestampTz(v) | Value::Timestamp(v) => {
            out.push(value.column_type().map_or(TAG_NULL, ColumnType::tag));
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int4(v) => {
            out.push(ColumnType::Int4.tag());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int2(v) => {
            out.push(ColumnType::Int2.tag());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Date(v) => {
            out.push(ColumnType::Date.tag());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Double(v) => {
            out.push(ColumnType::Double.tag());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Real(v) => {
            out.push(ColumnType::Real.tag());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Bool(v) => {
            out.push(ColumnType::Bool.tag());
            out.push(u8::from(*v));
        }
        Value::Text(v) => {
            out.push(ColumnType::Text.tag());
            put_bytes(v.as_bytes(), out);
        }
        Value::Bytea(v) => {
            out.push(ColumnType::Bytea.tag());
            put_bytes(v, out);
        }
    }
}

fn take_literal(cursor: &mut Cursor<'_>) -> Result<Value> {
    let tag = cursor.u8("literal type")?;
    if tag == TAG_NULL {
        return Ok(Value::Null);
    }
    // An unknown type tag is a type this build does not have, not a damaged byte: the checksum
    // already passed. Refuse, so a newer sender is a fallback rather than an alarm.
    let ty = ColumnType::from_tag(tag)
        .map_err(|_| Error::refused(format!("literal of type tag {tag}")))?;

    Ok(match ty {
        ColumnType::Int8 => Value::Int8(take_i64(cursor)?),
        ColumnType::TimestampTz => Value::TimestampTz(take_i64(cursor)?),
        ColumnType::Timestamp => Value::Timestamp(take_i64(cursor)?),
        ColumnType::Int4 => Value::Int4(i32::from_le_bytes(
            cursor.u32_le("literal int4")?.to_le_bytes(),
        )),
        ColumnType::Int2 => Value::Int2(i16::from_le_bytes(
            cursor.u16_le("literal int2")?.to_le_bytes(),
        )),
        ColumnType::Date => Value::Date(i32::from_le_bytes(
            cursor.u32_le("literal date")?.to_le_bytes(),
        )),
        ColumnType::Double => Value::Double(f64::from_bits(cursor.u64_le("literal double")?)),
        ColumnType::Real => Value::Real(f32::from_bits(cursor.u32_le("literal real")?)),
        ColumnType::Bool => match cursor.u8("literal boolean")? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            other => {
                return Err(Error::corruption(
                    "fragment",
                    format!("literal boolean byte {other}"),
                ));
            }
        },
        ColumnType::Text
        | ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Json
        | ColumnType::Jsonb => {
            let bytes = take_literal_bytes(cursor, "literal text")?;
            Value::Text(String::from_utf8(bytes).map_err(|error| {
                Error::corruption("fragment", format!("literal text is not utf-8: {error}"))
            })?)
        }
        ColumnType::Bytea => Value::Bytea(take_literal_bytes(cursor, "literal bytes")?),
    })
}

fn take_i64(cursor: &mut Cursor<'_>) -> Result<i64> {
    #[allow(clippy::cast_possible_wrap, reason = "the round trip of `to_le_bytes`")]
    let value = cursor.u64_le("literal integer")? as i64;
    Ok(value)
}

fn take_literal_bytes(cursor: &mut Cursor<'_>, field: &str) -> Result<Vec<u8>> {
    let len = cursor.count(field, 1)?;
    if len > MAX_VALUE_LEN {
        return Err(Error::corruption(
            "fragment",
            format!("{field} is {len} bytes, over the {MAX_VALUE_LEN} limit"),
        ));
    }
    Ok(cursor.bytes(len, field)?.to_vec())
}
