//! `generate_subscripts` — the one set-returning function that may stand where a relation does.
//!
//! `ActiveRecord`'s schema dump reads every foreign key and unique constraint through it: it turns
//! `pg_constraint.conkey`, an array of attribute numbers, into the column *names* in the order the
//! constraint declares them. It is boot statements 35 and 36 and the ladder's rung-3 blocker.
//!
//! # It yields the array's own subscripts, not `1..length`
//!
//! For `'[0:2]={a,b,c}'` it yields **0, 1, 2**. That is the whole reason the function exists
//! rather than `generate_series(1, array_length(a, 1))`, and an implementation built on the latter
//! is right for every array `ActiveRecord` happens to send and wrong by construction.
//!
//! # Every "nothing to do" case is zero rows
//!
//! An empty array, a NULL array, a dimension the array does not have, and dimensions `0` and `-1`
//! all produce **no rows** — never NULL, never an error. So a `SELECT` of it returns nothing and a
//! `count(*)` over it returns `0`. Measured, all five.

use crate::error::{Result, SqlError};
use crate::plan::TableFunction;
use crate::value::ColumnType;
use crate::value::vector::Array;
use crate::value::vector::Array as TextArray;
use esker_keys::value::Datum;

/// The type one generated **value** has: the column a client is described.
///
/// `generate_subscripts` is an `integer` whatever the array is — a subscript is an `int4` on a real
/// server — and `generate_series` takes its arguments' type, which for this node's integer
/// constants is `int8` where PostgreSQL's are `int4`: the standing constant-width divergence, one
/// more surface. `unnest` answers its array's **element** type, which is the only one of the three
/// that has to look at what it was given.
pub(super) fn result_type(call: &TableFunction, scope: &super::query::Scope<'_>) -> ColumnType {
    match call.name.as_str() {
        "generate_series" => call
            .args
            .first()
            .and_then(|arg| super::query::expr_type(arg, scope).ok())
            .unwrap_or(ColumnType::Int8),
        "unnest" => call
            .args
            .first()
            .and_then(|arg| super::query::expr_type(arg, scope).ok())
            .and_then(esker_keys::array::ArrayValue::element_of)
            .unwrap_or(ColumnType::Text),
        _ => ColumnType::Int4,
    }
}

/// `unnest(anyarray)`: one row per element, **in the array's own order**.
///
/// **An empty array yields nothing, and so takes its input row with it** — measured: with
/// `unnest(tags)` in the target list, a row whose array is `{}` does not appear in the result at
/// all. A NULL array is the same nothing; a NULL *element* is a row, because an array of one NULL
/// is not an empty array.
///
/// One argument only. PostgreSQL's `unnest` takes several and expands them in lockstep, which is
/// the `ROWS FROM` behaviour under another name; this node has the lockstep and not the multi-
/// argument spelling, so a second argument is `42883` naming the arity rather than a silent
/// truncation.
fn unnest(call: &TableFunction, row: &[Datum]) -> Result<Vec<Vec<Datum>>> {
    let [arg] = call.args.as_slice() else {
        return Err(SqlError::UndefinedFunction(format!(
            "unnest({})",
            vec!["unknown"; call.args.len()].join(", ")
        )));
    };
    Ok(match super::cursor::evaluate(arg, row)? {
        Datum::Array(array) => array
            .values
            .into_iter()
            .map(|value| vec![value.unwrap_or(Datum::Null)])
            .collect(),
        Datum::Null => Vec::new(),
        // An array this node keeps as text — the catalog's `int2vector`s are the ones that reach
        // here — read through the same parser a literal goes through.
        Datum::Text(text) => TextArray::read(&text)
            .map(|array| {
                array
                    .elements
                    .into_iter()
                    .map(|value| vec![value.map_or(Datum::Null, Datum::Text)])
                    .collect()
            })
            .unwrap_or_default(),
        other => {
            return Err(SqlError::UndefinedFunctionTypes(format!(
                "unnest({})",
                other
                    .column_type()
                    .map_or("unknown", crate::value::PgType::name)
            )));
        }
    })
}

/// The rows a call yields, one column wide.
pub(super) fn rows(call: &TableFunction, row: &[Datum]) -> Result<Vec<Vec<Datum>>> {
    if call.name == "generate_series" {
        return series(call, row);
    }
    if call.name == "unnest" {
        return unnest(call, row);
    }
    if call.name != "generate_subscripts" {
        return Err(SqlError::unsupported(format!(
            "the table function {}",
            call.name
        )));
    }
    // Two- and three-argument, and PostgreSQL treats them as **different functions**: one
    // argument is `42883 … does not exist` there with a `DETAIL` about the number of arguments.
    if !matches!(call.args.len(), 2 | 3) {
        // PostgreSQL treats the two- and three-argument forms as different functions, so one
        // argument is the *arity* refusal — "the given number of arguments" — and not the types.
        return Err(SqlError::UndefinedFunction(format!(
            "generate_subscripts({})",
            vec!["unknown"; call.args.len()].join(", ")
        )));
    }
    let value = super::cursor::evaluate(&call.args[0], row)?;
    let dimension = super::cursor::evaluate(&call.args[1], row)?;
    let reverse = match call.args.get(2) {
        Some(expr) => matches!(super::cursor::evaluate(expr, row)?, Datum::Bool(true)),
        None => false,
    };
    // **A NULL array is zero rows**, which is the first of the five "nothing to do" cases and the
    // one an implementation is most likely to turn into an error.
    let Datum::Int8(dimension) = &dimension else {
        if matches!(value, Datum::Null) || matches!(dimension, Datum::Null) {
            return Ok(Vec::new());
        }
        return Err(SqlError::UndefinedFunctionTypes(
            "generate_subscripts".to_owned(),
        ));
    };
    // **A real array knows its own bounds**, which is what an array column and an array cast
    // produce now. The text form beside it is the catalog's `int2vector`s, which are held as text
    // and reach this function through the same door (`crate::value::vector`).
    let bounds = match &value {
        Datum::Array(array) => array_subscripts(array, *dimension),
        Datum::Text(text) => subscripts(text, *dimension),
        Datum::Null => return Ok(Vec::new()),
        _ => {
            return Err(SqlError::UndefinedFunctionTypes(
                "generate_subscripts".to_owned(),
            ));
        }
    };
    let Some(bounds) = bounds else {
        return Ok(Vec::new());
    };
    let mut subscripts: Vec<i32> = bounds.collect();
    if reverse {
        subscripts.reverse();
    }
    Ok(subscripts
        .into_iter()
        .map(|at| vec![Datum::Int4(at)])
        .collect())
}

/// `generate_series(start, stop [, step])`: the values from `start` to `stop` **inclusive**.
///
/// Three rules, measured: a step that walks away from `stop` yields **no rows** rather than
/// looping — `generate_series(1, 3, -1)` is empty — a **zero** step is `22023 step size cannot
/// equal zero`, and a NULL in any argument is no rows at all.
fn series(call: &TableFunction, row: &[Datum]) -> Result<Vec<Vec<Datum>>> {
    if !matches!(call.args.len(), 2 | 3) {
        return Err(SqlError::UndefinedFunction(format!(
            "generate_series({})",
            vec!["unknown"; call.args.len()].join(", ")
        )));
    }
    let mut bounds = [0i64; 3];
    for (at, slot) in bounds.iter_mut().enumerate() {
        // The step defaults to one, which is the two-argument form.
        let Some(expr) = call.args.get(at) else {
            *slot = 1;
            continue;
        };
        *slot = match super::cursor::evaluate(expr, row)? {
            Datum::Int8(value) => value,
            Datum::Int4(value) => i64::from(value),
            Datum::Int2(value) => i64::from(value),
            Datum::Null => return Ok(Vec::new()),
            _ => {
                return Err(SqlError::UndefinedFunctionTypes(
                    "generate_series".to_owned(),
                ));
            }
        };
    }
    let [start, last, step] = bounds;
    if step == 0 {
        return Err(SqlError::ZeroStep);
    }
    let mut out = Vec::new();
    let mut at = start;
    while (step > 0 && at <= last) || (step < 0 && at >= last) {
        out.push(vec![Datum::Int8(at)]);
        let Some(next) = at.checked_add(step) else {
            break;
        };
        at = next;
    }
    Ok(out)
}

/// The subscripts of a real array's `dimension`, or `None` when it has none.
///
/// Every "nothing to do" case is here rather than at the caller: a dimension the array does not
/// have, and the empty array, which has no dimensions at all.
fn array_subscripts(
    array: &esker_keys::array::ArrayValue,
    dimension: i64,
) -> Option<std::ops::RangeInclusive<i32>> {
    let at = usize::try_from(dimension).ok().filter(|at| *at >= 1)?;
    let length = *array.dims.get(at - 1)?;
    // The lower bound belongs to the first dimension; every other starts at one, which is what a
    // real server reports for an array whose bounds were not written out.
    let lower = if at == 1 { array.lower } else { 1 };
    (length > 0).then(|| lower..=lower + length - 1)
}

/// The subscripts of `text`'s `dimension`, or `None` when there are none.
///
/// The text is either an array literal — `{a,b,c}`, or `[0:2]={a,b,c}` with an explicit lower
/// bound — or an `int2vector`, which is how `pg_index.indkey` and `pg_constraint.conkey` print
/// here: space-separated, one-based, no braces.
fn subscripts(text: &str, dimension: i64) -> Option<std::ops::RangeInclusive<i32>> {
    // Only the first dimension of a one-dimensional array exists, and `0` and `-1` are neither.
    let body = text.trim();
    // `[0:2]={a,b,c}` — an **explicit lower bound**, which `Array::read` does not model because
    // nothing else here needs it. It is exactly what this function exists to expose, so the
    // prefix is read here: the bounds are the answer, and the elements after it are not looked at.
    if let Some(rest) = body.strip_prefix('[')
        && let Some((bounds, _)) = rest.split_once("]=")
        && let Some((low, high)) = bounds.split_once(':')
    {
        if dimension != 1 {
            return None;
        }
        let low: i32 = low.trim().parse().ok()?;
        let high: i32 = high.trim().parse().ok()?;
        return (low <= high).then_some(low..=high);
    }
    if body.starts_with('{') {
        let array = Array::read(body)?;
        let lower = array.lower;
        let upper = array.upper()?;
        // A multi-dimensional array's second dimension is its inner length, which `Array` does
        // not model — this node stores arrays as text and reads them one level deep.
        if dimension != 1 {
            return None;
        }
        (lower <= upper).then_some(lower..=upper)
    } else {
        if dimension != 1 {
            return None;
        }
        // **An `int2vector` is subscripted from zero**, which is why `ActiveRecord` writes
        // `pg_get_indexdef(…, k + 1, true)` over `generate_subscripts(d.indkey, 1)`: measured on
        // 19beta1, an index on two columns gives `k` of 0 and 1 and `array_lower(indkey, 1)` is 0.
        // This branch returned `1..=count` when it was written — an assumption, and the unit test
        // beside it asserted the same assumption, so nothing caught it until boot statement 32
        // read the *second* column where PostgreSQL reads the first.
        let count = i32::try_from(body.split_whitespace().count()).ok()?;
        (count > 0).then_some(0..=count - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::subscripts;

    /// The subscripts each shape yields, from the capture.
    #[test]
    fn the_five_nothing_to_do_cases_and_the_bounds() {
        assert_eq!(
            subscripts("{a,b,c}", 1).map(Iterator::collect),
            Some(vec![1, 2, 3])
        );
        // A custom lower bound is honoured, which is the whole reason this function exists.
        assert_eq!(
            subscripts("[0:2]={a,b,c}", 1).map(Iterator::collect),
            Some(vec![0, 1, 2])
        );
        // **An `int2vector` is 0-based**, which is `pg_index.indkey`'s whole difference from an
        // array — measured, not assumed. This assertion said `[1, 2]` when it was written and
        // agreed with an implementation that had made the same mistake.
        assert_eq!(
            subscripts("2 1", 1).map(Iterator::collect),
            Some(vec![0, 1])
        );
        // Every "nothing to do" case is no rows.
        for (text, dimension) in [
            ("{}", 1),
            ("{a,b,c}", 2),
            ("{a,b,c}", 0),
            ("{a,b,c}", -1),
            ("", 1),
        ] {
            assert!(
                subscripts(text, dimension).is_none(),
                "{text} dimension {dimension}"
            );
        }
    }
}
