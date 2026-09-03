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
use crate::value::vector::Array;
use esker_keys::value::Datum;

/// The rows a call yields, one column wide.
pub(super) fn rows(call: &TableFunction, row: &[Datum]) -> Result<Vec<Vec<Datum>>> {
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
    let (Datum::Text(text), Datum::Int8(dimension)) = (&value, &dimension) else {
        if matches!(value, Datum::Null) || matches!(dimension, Datum::Null) {
            return Ok(Vec::new());
        }
        return Err(SqlError::UndefinedFunctionTypes(
            "generate_subscripts".to_owned(),
        ));
    };
    let Some(bounds) = subscripts(text, *dimension) else {
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
        let count = i32::try_from(body.split_whitespace().count()).ok()?;
        (count > 0).then_some(1..=count)
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
        // An `int2vector`, which is how `pg_index.indkey` prints here.
        assert_eq!(
            subscripts("2 1", 1).map(Iterator::collect),
            Some(vec![1, 2])
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
