//! Arithmetic over the date and time types: which pairs have an operator, and what it yields.
//!
//! The table is PostgreSQL's and almost none of it is derivable. `date - date` is an **integer**
//! while `timestamp - timestamp` is an **interval**; `date + integer` is a `date` while
//! `date + interval` is a **timestamp**; `time * 2` is an `interval` and not a time. Each row was
//! measured, and the shape of the table is the reason this is a lookup rather than a rule.
//!
//! # An interval is three numbers and they never carry into each other
//!
//! Months, days and microseconds, each with its own sign. `'25 hours' + '1 day'` is
//! `1 day 25:00:00` and not `2 days 01:00:00`, and `'1.5 days' * 2` is `2 days 24:00:00` — the
//! fields multiply independently and nothing is normalised. That is not sloppiness in the
//! printing: a day is not always 24 hours across a daylight-saving boundary and a month is not a
//! fixed number of days, so carrying would change the value's meaning. Measured, both.
//!
//! # A month lands on the last day it can
//!
//! `'2020-01-31' + '1 month'` is `2020-02-29`, and adding another month gives `2020-03-29` rather
//! than the 31st it started from. Month addition clamps to the target month's length and does not
//! remember where it came from, which is why it is not associative. Measured, both steps.
//!
//! # A time wraps and drops the days
//!
//! `'23:00' + '2 hours'` is `01:00`, `'01:00' - '2 hours'` is `23:00`, and `'12:00' + '1 day'` is
//! `12:00` — the interval's day and month fields do not exist for a `time`. All three measured.

use crate::error::{Result, SqlError};
use crate::plan::ArithOp;
use crate::value::{ColumnType, PgType};
use esker_keys::value::Datum;

/// Microseconds in a day.
const DAY_MICROS: i64 = 86_400_000_000;

/// The type `left <op> right` has when either side is a date or a time, or `None` when the pair
/// has no operator at all — which is a `42883` and not a refusal, because PostgreSQL has none
/// either (`date + numeric`).
#[must_use]
#[allow(
    clippy::match_same_arms,
    reason = "one arm per row of PostgreSQL's table; merging rows that happen to share a result \
              would hide which pairs have an operator, which is the whole content of this function"
)]
pub fn result_type(op: ArithOp, left: ColumnType, right: ColumnType) -> Option<ColumnType> {
    use ArithOp::{Add, Divide, Multiply, Subtract};
    use ColumnType::{Date, Interval, Time, Timestamp, TimestampTz};
    Some(match (op, left, right) {
        // **A difference of dates is a count of days**, where every other difference is an
        // interval. The one row of this table nobody guesses.
        (Subtract, Date, Date) => ColumnType::Int4,
        (Add | Subtract, Date, ty) | (Add, ty, Date) if integer(ty) => Date,
        (Add | Subtract, Date, Interval) | (Add, Interval, Date) => Timestamp,
        (Add | Subtract, Date, Time) | (Add, Time, Date) => Timestamp,

        (Subtract, Time, Time) => Interval,
        (Add | Subtract, Time, Interval) | (Add, Interval, Time) => Time,
        // `time * 2` is an **interval**: twenty-four hours is not a time of day.
        (Multiply | Divide, Time, ty) | (Multiply, ty, Time) if number(ty) => Interval,

        (Subtract, Timestamp, Timestamp) => Interval,
        (Subtract, TimestampTz, TimestampTz) => Interval,
        (Subtract, Timestamp, Date) | (Subtract, Date, Timestamp) => Interval,
        (Add | Subtract, Timestamp, Interval) | (Add, Interval, Timestamp) => Timestamp,
        (Add | Subtract, TimestampTz, Interval) | (Add, Interval, TimestampTz) => TimestampTz,

        (Add | Subtract, Interval, Interval) => Interval,
        (Multiply | Divide, Interval, ty) | (Multiply, ty, Interval) if number(ty) => Interval,
        _ => return None,
    })
}

/// The type `-operand` has, or `None` for one that has no negation.
///
/// **`-time` is an `interval`** and `-date` and `-timestamp` are `42883`. Measured, all three: an
/// instant has no negative and a duration does.
#[must_use]
pub fn negate_type(operand: ColumnType) -> Option<ColumnType> {
    match operand {
        ColumnType::Time | ColumnType::Interval => Some(ColumnType::Interval),
        _ => None,
    }
}

fn integer(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
}

fn number(ty: ColumnType) -> bool {
    integer(ty)
        || matches!(
            ty,
            ColumnType::Real | ColumnType::Double | ColumnType::Numeric
        )
}

/// `-operand`, for the two types that have a negation.
pub fn negate(value: &Datum) -> Result<Datum> {
    Ok(match value {
        Datum::Null => Datum::Null,
        Datum::Time(micros) => interval(0, 0, -micros),
        Datum::Interval {
            months,
            days,
            micros,
        } => interval(
            months.saturating_neg(),
            days.saturating_neg(),
            micros.saturating_neg(),
        ),
        other => {
            return Err(SqlError::UndefinedUnaryOperator {
                op: "-",
                operand: other.column_type().map_or("unknown", PgType::name),
            });
        }
    })
}

/// `left <op> right`, where [`result_type`] has already said the pair has an operator.
pub fn apply(op: ArithOp, ty: ColumnType, left: &Datum, right: &Datum) -> Result<Datum> {
    if matches!(left, Datum::Null) || matches!(right, Datum::Null) {
        return Ok(Datum::Null);
    }
    match (op, left, right) {
        // `date - date`, in days, and **an infinite date has no difference** — `22008` with its
        // own sentence, which is a real server's answer and not an overflow.
        (ArithOp::Subtract, Datum::Date(left), Datum::Date(right)) => {
            if infinite_date(*left) || infinite_date(*right) {
                return Err(SqlError::InfiniteDateSubtraction);
            }
            Ok(Datum::Int4(left.saturating_sub(*right)))
        }
        (ArithOp::Add | ArithOp::Subtract, Datum::Date(day), other)
            if matches!(ty, ColumnType::Date) =>
        {
            Ok(Datum::Date(shift_date(*day, days_of(other)?, op)?))
        }
        (ArithOp::Add, other, Datum::Date(day)) if matches!(ty, ColumnType::Date) => Ok(
            Datum::Date(shift_date(*day, days_of(other)?, ArithOp::Add)?),
        ),
        // Everything that answers a timestamp goes through microseconds, which is what a
        // timestamp is: a date contributes its midnight and a time its time of day.
        _ if matches!(ty, ColumnType::Timestamp | ColumnType::TimestampTz) => {
            timestamp_result(op, ty, left, right)
        }
        _ if matches!(ty, ColumnType::Interval) => interval_result(op, left, right),
        _ if matches!(ty, ColumnType::Time) => time_result(op, left, right),
        _ => Err(SqlError::Internal(format!(
            "{ty:?} reached temporal arithmetic with {left:?} and {right:?}"
        ))),
    }
}

fn infinite_date(day: i32) -> bool {
    day == super::date::POS_INFINITY || day == super::date::NEG_INFINITY
}

/// The whole days one operand of a date shift contributes.
fn days_of(value: &Datum) -> Result<i64> {
    match value {
        Datum::Int8(value) => Ok(*value),
        Datum::Int4(value) => Ok(i64::from(*value)),
        Datum::Int2(value) => Ok(i64::from(*value)),
        other => Err(SqlError::Internal(format!(
            "{other:?} reached a date shift"
        ))),
    }
}

/// `date ± days`, with PostgreSQL's `22008` past the type's ends.
///
/// **An infinite date absorbs the shift**: `'infinity'::date + 1` is `infinity`, measured.
fn shift_date(day: i32, days: i64, op: ArithOp) -> Result<i32> {
    if infinite_date(day) {
        return Ok(day);
    }
    let days = if op == ArithOp::Subtract { -days } else { days };
    i64::from(day)
        .checked_add(days)
        .and_then(|day| i32::try_from(day).ok())
        .filter(|day| (super::date::MIN_DAY..=super::date::MAX_DAY).contains(day))
        .ok_or(SqlError::DateOutOfRange)
}

/// The microseconds an operand contributes to a timestamp result, and whether it is an interval.
fn timestamp_result(op: ArithOp, ty: ColumnType, left: &Datum, right: &Datum) -> Result<Datum> {
    let build = |micros: i64| {
        if ty == ColumnType::TimestampTz {
            Datum::TimestampTz(micros)
        } else {
            Datum::Timestamp(micros)
        }
    };
    // One side is an instant and the other is a duration, in whichever order they were written.
    let (instant, other, reversed) = match (left, right) {
        // Written the other way round: a duration first and the instant after it.
        (Datum::Interval { .. }, _) | (Datum::Time(_), Datum::Date(_)) => (right, left, true),
        _ => (left, right, false),
    };
    let base = instant_micros(instant)?;
    match other {
        Datum::Interval {
            months,
            days,
            micros,
        } => {
            // A subtraction written the other way round does not exist — `interval - date` is not
            // an operator — so a reversed pair is always an addition.
            let sign = if op == ArithOp::Subtract && !reversed {
                -1
            } else {
                1
            };
            Ok(build(add_interval(base, *months, *days, *micros, sign)?))
        }
        // `date + time` and `date - time`: the time of day, added to or taken off the midnight.
        Datum::Time(micros) => {
            let sign = if op == ArithOp::Subtract { -1 } else { 1 };
            Ok(build(base.saturating_add(sign * micros)))
        }
        other => Err(SqlError::Internal(format!(
            "{other:?} reached timestamp arithmetic"
        ))),
    }
}

/// The microseconds an instant is at, whatever type it is written as.
fn instant_micros(value: &Datum) -> Result<i64> {
    match value {
        Datum::Date(day) => Ok(super::date::as_micros(*day)),
        Datum::Timestamp(micros) | Datum::TimestampTz(micros) => Ok(*micros),
        other => Err(SqlError::Internal(format!("{other:?} is not an instant"))),
    }
}

/// An instant shifted by an interval's three fields, months first.
///
/// **The months are applied to the calendar and the days are not.** A month lands on the last day
/// of the target month when the day it started from does not exist there — `2020-01-31 + 1 month`
/// is `2020-02-29` — and the days and microseconds are then plain addition.
fn add_interval(base: i64, months: i32, days: i32, micros: i64, sign: i64) -> Result<i64> {
    let mut micros_out = base;
    if months != 0 {
        let shifted = i64::from(months).saturating_mul(sign);
        micros_out = shift_months(micros_out, shifted)?;
    }
    let day_micros = i64::from(days)
        .saturating_mul(DAY_MICROS)
        .saturating_mul(sign);
    Ok(micros_out
        .saturating_add(day_micros)
        .saturating_add(micros.saturating_mul(sign)))
}

/// An instant moved by whole months, clamped to the target month's length.
fn shift_months(micros: i64, months: i64) -> Result<i64> {
    let day = micros.div_euclid(DAY_MICROS);
    let time = micros.rem_euclid(DAY_MICROS);
    let (year, month, dom) =
        super::timestamp::civil_from_days(day + super::timestamp::UNIX_TO_PG_EPOCH_DAYS);
    let total = year
        .saturating_mul(12)
        .saturating_add(month - 1)
        .saturating_add(months);
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    // The clamp: the 31st of a month with thirty days is that month's last day.
    let dom = dom.min(super::timestamp::days_in_month(year, month));
    let day = super::timestamp::days_from_pg_epoch_checked(year, month, dom)
        .ok_or(SqlError::DateOutOfRange)?;
    Ok(day.saturating_mul(DAY_MICROS).saturating_add(time))
}

/// The interval-valued forms: two intervals, two instants, or an interval scaled by a number.
fn interval_result(op: ArithOp, left: &Datum, right: &Datum) -> Result<Datum> {
    match (left, right) {
        (
            Datum::Interval {
                months: lm,
                days: ld,
                micros: lu,
            },
            Datum::Interval {
                months: rm,
                days: rd,
                micros: ru,
            },
        ) => {
            let sign = if op == ArithOp::Subtract { -1 } else { 1 };
            // **Checked, not saturating**: `'2147483647 days' + '1 day'` is
            // `22008 interval out of range` on a real server, and a day field pinned at its
            // maximum would be a wrong answer that looks like data. Three fields, three
            // independent ranges — nothing normalises between them, so nothing borrows.
            let field = |left: i32, right: i32| {
                right
                    .checked_mul(sign)
                    .and_then(|right| left.checked_add(right))
                    .ok_or(SqlError::IntervalOutOfRange)
            };
            Ok(interval(
                field(*lm, *rm)?,
                field(*ld, *rd)?,
                ru.checked_mul(i64::from(sign))
                    .and_then(|right| lu.checked_add(right))
                    .ok_or(SqlError::IntervalOutOfRange)?,
            ))
        }
        // `time - time` and `timestamp - timestamp`: a duration in microseconds alone, because
        // neither operand knows anything about months.
        (Datum::Time(left), Datum::Time(right)) => Ok(interval(0, 0, left - right)),
        (Datum::Timestamp(_) | Datum::TimestampTz(_) | Datum::Date(_), _) => {
            let (left, right) = (instant_micros(left)?, instant_micros(right)?);
            Ok(interval(0, 0, left.saturating_sub(right)))
        }
        // An interval scaled: **each field independently**, and nothing normalised.
        _ => {
            let (value, factor, is_interval_left) = match left {
                Datum::Interval { .. } | Datum::Time(_) => (left, right, true),
                _ => (right, left, false),
            };
            // **A `time` scaled is an interval too** — `'12:00'::time * 2` is `24:00:00`, which is
            // not a time of day — so it enters here as the duration it is being treated as.
            let (months, days, micros) = match value {
                Datum::Interval {
                    months,
                    days,
                    micros,
                } => (months, days, micros),
                Datum::Time(micros) => (&0, &0, micros),
                other => {
                    return Err(SqlError::Internal(format!(
                        "{other:?} reached interval scaling"
                    )));
                }
            };
            let factor = scale_factor(factor)?;
            if op == ArithOp::Divide {
                if !is_interval_left {
                    return Err(SqlError::Internal(
                        "a number divided by an interval reached the evaluator".to_owned(),
                    ));
                }
                if factor == 0.0 {
                    return Err(SqlError::DivisionByZero);
                }
                // **An interval has no infinity here**, and on this side that leaves only a NaN:
                // dividing *by* an infinity is zero and zero is representable, so
                // `'1 day' / 'Infinity'::float8` is `00:00:00` on a real server and here.
                if factor.is_nan() {
                    return Err(SqlError::unsupported(
                        "an interval scaled by an infinite factor",
                    ));
                }
                // **Divided, not multiplied by the reciprocal.** PostgreSQL's `interval_div`
                // divides each field by the factor, and the two are not the same double:
                // `1.0 / 3.0` is not exactly a third, so a reciprocal moves the last bits of
                // every field before the cascade below rounds them.
                return scale_interval(*months, *days, *micros, &|value| value / factor);
            }
            // **An interval has no infinity here.** PostgreSQL 17 gave the type one, so
            // `'1 day' * 'Infinity'::float8` is `infinity` there; this node's interval is three
            // finite fields and scaling by an infinite factor would truncate to `00:00:00`, a
            // wrong answer where a refusal is available (ADR 0031).
            if !factor.is_finite() {
                return Err(SqlError::unsupported(
                    "an interval scaled by an infinite factor",
                ));
            }
            scale_interval(*months, *days, *micros, &|value| value * factor)
        }
    }
}

/// A number an interval is scaled by, whatever numeric type it arrived as.
#[allow(
    clippy::cast_precision_loss,
    reason = "the factor is a multiplier, and PostgreSQL's interval scaling is a double too"
)]
fn scale_factor(value: &Datum) -> Result<f64> {
    match value {
        Datum::Int8(value) => Ok(*value as f64),
        Datum::Int4(value) => Ok(f64::from(*value)),
        Datum::Int2(value) => Ok(f64::from(*value)),
        Datum::Double(value) => Ok(*value),
        Datum::Real(value) => Ok(f64::from(*value)),
        Datum::Numeric(value) => Ok(super::numeric::as_f64(value)),
        other => Err(SqlError::Internal(format!(
            "{other:?} reached interval scaling"
        ))),
    }
}

/// Days in the month an interval's month field cascades into, and seconds in its day.
///
/// Neither is a fact about the calendar: nothing normalises between an interval's three fields, so
/// these numbers exist only for the *fraction* left over by a scaling, and only downwards.
const DAYS_PER_MONTH: f64 = 30.0;
const SECONDS_PER_DAY: f64 = 86_400.0;

/// Each field scaled, with a month's fraction cascading into days and a day's into microseconds —
/// and **rounded twice on the way down**, which is the whole of what makes this hard.
///
/// This is PostgreSQL's `interval_mul` and `interval_div`, which are the same function with one
/// operator different; `scale` is that operator, already closed over its factor. `avg(interval)`
/// is the divide case with the count as the factor, so all three answers come from here.
///
/// # Why the rounding is where it is
///
/// A scaling that only truncated would answer `9 days 23:59:59.999999` for `'1 mon' / 3`, because
/// `(1/3) * 30` is `9.999999999999998` in a double. PostgreSQL answers `10 days`, and it gets
/// there by rounding the month's remainder **to six decimal places of a day** before taking its
/// whole days off. Two measured rows say that this is the placement and not merely *a* rounding:
///
/// * `'1 mon' / 9` is `3 days 07:59:59.9712`, not `3 days 08:00:00` — the remainder `0.333333` is
///   short of a third by a millionth of a day, and that shortfall survives into the answer.
/// * `'100 days' / 7` is `14 days 06:51:25.714286`, at full precision — so the same flattening
///   must **not** be applied to the day remainder, which would have given `.6896`.
///
/// The derivation, with the row that pins each step, is
/// `tests/captures/pg19_interval_aggregate.txt`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "PostgreSQL scales an interval in double, and every truncation here is a field it \
              truncates too — checked against the type's range first"
)]
fn scale_interval(
    months: i32,
    days: i32,
    micros: i64,
    scale: &dyn Fn(f64) -> f64,
) -> Result<Datum> {
    let scaled_months = fits_in_i32(scale(f64::from(months)))?;
    let whole_months = scaled_months.trunc();
    let remainder_days = round_to_millionths((scaled_months - whole_months) * DAYS_PER_MONTH);

    let scaled_days = fits_in_i32(scale(f64::from(days)))?;
    let mut whole_days = scaled_days.trunc();
    let mut remainder_seconds = round_to_millionths(
        (scaled_days - whole_days + remainder_days - remainder_days.trunc()) * SECONDS_PER_DAY,
    );
    // **The two remainders together can reach a whole day**, and that day is carried into the day
    // field: `'11 mons 29 days' / 16` is `22 days 10:30:00`, where the fractions are 0.625 of a
    // month's days and 0.8125 of a day. The carry is on the *remainder* being cascaded and never
    // on the result — `'1 mon 1 day 23:59:59.999999' / 16` is `1 day 24:00:00`, twenty-four hours
    // in the time field beside a day field of one, because nothing normalises a finished interval.
    if remainder_seconds.abs() >= SECONDS_PER_DAY {
        let carried = (remainder_seconds / SECONDS_PER_DAY).trunc();
        whole_days += carried;
        remainder_seconds -= carried * SECONDS_PER_DAY;
    }
    whole_days += remainder_days.trunc();
    // Ties to even, measured on both halves of the tie: `3 us / 2` is `2 us` (so not truncation)
    // and `5 us / 2` is `2 us` (so not half-away-from-zero).
    let scaled_micros = (scale(micros as f64) + remainder_seconds * 1e6).round_ties_even();
    Ok(interval(
        // Already in range: a truncation cannot leave one.
        whole_months as i32,
        // The day field is the one that can be pushed out of range by the cascade rather than by
        // the scaling — a month's remainder adds up to thirty days after the check above.
        // PostgreSQL adds it to a C `int` and lets it wrap; a wrapped day count is a wrong answer
        // that looks like data, so it is `22008` here.
        fits_in_i32(whole_days)? as i32,
        fits_in_i64(scaled_micros)? as i64,
    ))
}

/// One interval divided by a count: the arithmetic **`avg(interval)` is**.
///
/// An average over intervals is not a fold. PostgreSQL keeps the exact sum and divides it once, at
/// the end, through the same `interval_div` that `interval / n` uses — twenty values by nine
/// divisors were put to a real server both ways and all 180 pairs agree byte for byte
/// (`tests/captures/pg19_interval_aggregate.txt`). Folding would be wrong twice over: the mean of
/// two means is not the mean, and each fold would round again.
///
/// The divisor is a row count, so it is never zero — an average over no rows is NULL and never
/// reaches here.
#[allow(
    clippy::cast_precision_loss,
    reason = "the divisor is the count PostgreSQL's own interval average divides by, in a double"
)]
pub fn divide_interval(months: i32, days: i32, micros: i64, divisor: i64) -> Result<Datum> {
    let divisor = divisor as f64;
    scale_interval(months, days, micros, &|value| value / divisor)
}

/// `x` rounded to a millionth, ties to even — PostgreSQL's `TSROUND`, which is where an
/// interval's cascade loses the last bits of a repeating fraction.
fn round_to_millionths(value: f64) -> f64 {
    (value * 1e6).round_ties_even() / 1e6
}

/// The scaled field, or `22008` — the check PostgreSQL makes before every one of these casts.
///
/// The upper bound is **exclusive at 2^31**, not inclusive at `i32::MAX`, because what follows is
/// a truncation: `2147483647.5` days is in range and becomes `2147483647`, which is the answer a
/// real server gives.
fn fits_in_i32(value: f64) -> Result<f64> {
    const LIMIT: f64 = 2_147_483_648.0;
    if value.is_finite() && (-LIMIT..LIMIT).contains(&value) {
        Ok(value)
    } else {
        Err(SqlError::IntervalOutOfRange)
    }
}

/// The same for the microsecond field, whose range is an `i64`.
///
/// The bound is written as a power of two rather than as `i64::MAX as f64`, which rounds **up** to
/// 2^63 and would admit a value the cast cannot hold.
fn fits_in_i64(value: f64) -> Result<f64> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if value.is_finite() && (-LIMIT..LIMIT).contains(&value) {
        Ok(value)
    } else {
        Err(SqlError::IntervalOutOfRange)
    }
}

/// The time-valued forms: a time shifted by an interval, **wrapping** and ignoring its days.
fn time_result(op: ArithOp, left: &Datum, right: &Datum) -> Result<Datum> {
    let (time, other) = match (left, right) {
        // In whichever order they were written: `time + interval` and `interval + time` are
        // one operator.
        (Datum::Time(time), other) | (other, Datum::Time(time)) => (*time, other),
        _ => {
            return Err(SqlError::Internal(
                "time arithmetic without a time".to_owned(),
            ));
        }
    };
    let Datum::Interval { micros, .. } = other else {
        return Err(SqlError::Internal(format!(
            "{other:?} reached time arithmetic"
        )));
    };
    let sign = if op == ArithOp::Subtract { -1 } else { 1 };
    // **Modulo a day, and the interval's days are dropped**: `'12:00' + '1 day'` is `12:00` and
    // `'23:00' + '2 hours'` is `01:00`. Measured, both.
    Ok(Datum::Time(
        time.saturating_add(sign * micros).rem_euclid(DAY_MICROS),
    ))
}

fn interval(months: i32, days: i32, micros: i64) -> Datum {
    Datum::Interval {
        months,
        days,
        micros,
    }
}
