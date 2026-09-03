//! The evaluator's answer, in the shape the wire declares.
//!
//! `esker_columnar` produces `FragmentOutput` and `esker-proto` carries
//! `fragment::result::Body`: the same shapes, defined twice on purpose. `esker-proto` must not
//! link `esker-columnar` — *"a leaf whose format versions would then be able to break the wire"*
//! (`esker_proto::fragment::result`) — so somebody has to convert, and it is this crate, which
//! already links both.
//!
//! Every match here is **total**. A seventh value type on either side is a compile error in this
//! file rather than a column that silently answers NULL, which is the same rule
//! [`super::decode`] follows for the row vocabulary and for the same reason.

use esker_columnar::{FragmentOutput, Partial, Value};
use esker_proto::fragment::result::{
    AggregateKind, Body, Group, Partial as WirePartial, Value as WireValue, ValueType,
};

/// The wire body an evaluation produced.
///
/// # Errors
///
/// A column whose values disagree about their type, which the writer cannot produce and a reader
/// would have refused — reported rather than encoded, because a result decoded against the wrong
/// types is intact bytes with the wrong meaning.
pub fn body_of(output: &FragmentOutput) -> Result<Body, String> {
    match output {
        FragmentOutput::Rows(rows) => Ok(Body::Rows {
            types: column_types(rows.iter().map(Vec::as_slice))?,
            rows: rows
                .iter()
                .map(|row| row.iter().map(value_of).collect())
                .collect(),
        }),
        FragmentOutput::Groups(groups) => {
            let key_types = column_types(groups.iter().map(|group| group.key.as_slice()))?;
            let aggregates = groups
                .first()
                .map(|group| group.aggregates.iter().map(aggregate_of).collect())
                .unwrap_or_default();
            Ok(Body::Groups {
                key_types,
                aggregates,
                groups: groups
                    .iter()
                    .map(|group| Group {
                        key: group.key.iter().map(value_of).collect(),
                        partials: group.aggregates.iter().map(partial_of).collect(),
                    })
                    .collect(),
            })
        }
    }
}

/// One type per column, from the first non-NULL value in it.
///
/// A column of nothing but NULLs has no type to declare and takes `Int8`, which is what the
/// decoder needs to be told *something*: a NULL decodes the same against every type, so the choice
/// cannot change an answer — and the alternative, refusing, would make a legitimate result
/// unsendable.
fn column_types<'a>(
    rows: impl Iterator<Item = &'a [Value]> + Clone,
) -> Result<Vec<ValueType>, String> {
    let width = rows.clone().next().map_or(0, <[Value]>::len);
    let mut types: Vec<Option<ValueType>> = vec![None; width];
    for row in rows {
        if row.len() != width {
            return Err(format!(
                "a result with rows of {} and {width} columns",
                row.len()
            ));
        }
        for (slot, value) in row.iter().enumerate() {
            let Some(found) = type_of(value) else {
                continue;
            };
            match types[slot] {
                None => types[slot] = Some(found),
                Some(known) if known == found => {}
                Some(known) => {
                    return Err(format!("column {slot} holds both {known:?} and {found:?}"));
                }
            }
        }
    }
    Ok(types
        .into_iter()
        .map(|found| found.unwrap_or(ValueType::Int8))
        .collect())
}

fn type_of(value: &Value) -> Option<ValueType> {
    Some(match value {
        Value::Null => return None,
        Value::Int8(_) => ValueType::Int8,
        Value::Int4(_) => ValueType::Int4,
        Value::Int2(_) => ValueType::Int2,
        Value::Real(_) => ValueType::Real,
        Value::Text(_) => ValueType::Text,
        Value::Bool(_) => ValueType::Bool,
        Value::Bytea(_) => ValueType::Bytea,
        Value::TimestampTz(_) => ValueType::TimestampTz,
        Value::Timestamp(_) => ValueType::Timestamp,
        Value::Double(_) => ValueType::Double,
        Value::Date(_) => ValueType::Date,
        Value::Numeric(_) => ValueType::Numeric,
        Value::Time(_) => ValueType::Time,
        Value::Uuid(_) => ValueType::Uuid,
        Value::Interval(_) => ValueType::Interval,
        Value::Oid(_) => ValueType::Oid,
    })
}

fn value_of(value: &Value) -> WireValue {
    match value {
        Value::Null => WireValue::Null,
        Value::Int8(int) => WireValue::Int8(*int),
        Value::Int4(int) => WireValue::Int4(*int),
        Value::Int2(int) => WireValue::Int2(*int),
        Value::Real(real) => WireValue::Real(*real),
        Value::Text(text) => WireValue::Text(text.clone()),
        Value::Bool(flag) => WireValue::Bool(*flag),
        Value::Bytea(bytes) => WireValue::Bytea(bytes.clone()),
        Value::TimestampTz(ts) => WireValue::TimestampTz(*ts),
        Value::Timestamp(ts) => WireValue::Timestamp(*ts),
        Value::Double(double) => WireValue::Double(*double),
        Value::Date(day) => WireValue::Date(*day),
        Value::Numeric(text) => WireValue::Numeric(text.clone()),
        Value::Time(micros) => WireValue::Time(*micros),
        Value::Uuid(bytes) => WireValue::Uuid(*bytes),
        Value::Interval(bytes) => WireValue::Interval(*bytes),
        Value::Oid(v) => WireValue::Oid(*v),
    }
}

fn partial_of(partial: &Partial) -> WirePartial {
    let carried = |value: &Option<Value>| value.as_ref().map(value_of);
    match partial {
        Partial::Count(count) => WirePartial::Count(*count),
        Partial::Sum(value) => WirePartial::Sum(carried(value)),
        Partial::Min(value) => WirePartial::Min(carried(value)),
        Partial::Max(value) => WirePartial::Max(carried(value)),
    }
}

/// What a partial is a partial *of*, which the wire declares once in the header.
fn aggregate_of(partial: &Partial) -> (AggregateKind, Option<ValueType>) {
    // A partial that is NULL over no rows declares `Int8`, for the reason `column_types` gives: a
    // NULL decodes the same against every type, and the alternative is a legitimate answer that
    // cannot be sent.
    let carried =
        |value: &Option<Value>| Some(value.as_ref().and_then(type_of).unwrap_or(ValueType::Int8));
    match partial {
        Partial::Count(_) => (AggregateKind::Count, None),
        Partial::Sum(value) => (AggregateKind::Sum, carried(value)),
        Partial::Min(value) => (AggregateKind::Min, carried(value)),
        Partial::Max(value) => (AggregateKind::Max, carried(value)),
    }
}
