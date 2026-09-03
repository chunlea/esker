//! `interval`: three independent fields, and almost every surprise follows from that.
//!
//! OID 1186, sixteen bytes, `typcategory` T. Months, days and microseconds are stored separately
//! and **nothing normalises between them**: `1 mon 1 day` prints as itself for ever and
//! `400 days` never becomes a year. Months do carry into years on the way in (`1 year 13 months`
//! is `2 years 1 mon`) because years are only twelve months; days do not carry into months,
//! because a month is not a fixed number of days.
//!
//! # Equal is not identical
//!
//! **Comparison converts**, where storage does not: a month is thirty days and a day is
//! twenty-four hours, so `'1 mon' = '30 days'`, `'1 year' = '360 days'` and `'1 mon 1 day' =
//! '31 days'` are all `t`. Two intervals can be equal and print differently — which is why the
//! index key holds the converted total (`esker_keys::row::interval_total`) and the row holds the
//! fields.
//!
//! # Signs are per field
//!
//! `'1 day' - '12 hours'` is `1 day -12:00:00`: a positive day beside a negative time, equal to
//! `'12:00:00'` and left exactly as it is.
//!
//! # Three SQLSTATEs
//!
//! `'178956971 years'` is `22008 interval out of range` — the whole value overflows.
//! `'2147483648 months'` is `22015 interval field value out of range` — one *field* does, and
//! that code appears nowhere else in this project. Text that does not parse is `22007`.

use crate::error::{Result, SqlError};

/// Microseconds in a second, a minute, an hour and a day.
const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;

/// The three fields, as they are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    /// Whole months; years are twelve of these.
    pub months: i32,
    /// Whole days, which never carry into months.
    pub days: i32,
    /// The sub-day part, which never carries into days.
    pub micros: i64,
}

/// What PostgreSQL's `interval_out` writes under the default `IntervalStyle` of `postgres`.
///
/// The plural rules are its own and are measured, not guessed: `1 day` is singular and `-1 day`
/// prints **`-1 days`**, `1 mon` is abbreviated where `1 year` is not, and a zero interval is
/// `00:00:00` rather than empty. The time part appears when it is non-zero *or* when nothing else
/// is, and it carries its own sign.
#[must_use]
pub fn to_text(value: &Interval) -> String {
    let mut parts: Vec<String> = Vec::new();
    let (years, months) = (value.months / 12, value.months % 12);
    if years != 0 {
        parts.push(format!("{years} year{}", plural(years)));
    }
    if months != 0 {
        parts.push(format!("{months} mon{}", plural(months)));
    }
    if value.days != 0 {
        parts.push(format!("{} day{}", value.days, plural(value.days)));
    }
    if value.micros != 0 || parts.is_empty() {
        parts.push(clock(value.micros));
    }
    parts.join(" ")
}

/// `s` unless the number is exactly one — and **`-1` is plural**, which is the rule a reader
/// guesses wrong: `-1 days`, not `-1 day`.
fn plural(count: i32) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The time part as `HH:MM:SS[.ffffff]`, with the sign on the whole thing.
fn clock(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let micros = micros.unsigned_abs();
    let hours = micros / MICROS_PER_HOUR.unsigned_abs();
    let minutes = (micros % MICROS_PER_HOUR.unsigned_abs()) / MICROS_PER_MINUTE.unsigned_abs();
    let seconds = (micros % MICROS_PER_MINUTE.unsigned_abs()) / MICROS_PER_SECOND.unsigned_abs();
    let fraction = micros % MICROS_PER_SECOND.unsigned_abs();
    let mut out = format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
    if fraction != 0 {
        out.push('.');
        out.push_str(format!("{fraction:06}").trim_end_matches('0'));
    }
    out
}

/// PostgreSQL's `interval_in`, for the shapes this node reads.
///
/// Three grammars in one, all measured: a **unit list** (`1 year 2 mons 3 days 04:05:06`, with
/// `ago` negating the whole thing), the **ISO-8601** form (`P1Y2M3DT4H5M6S`), and the SQL forms
/// `1-2` (year-month) and `3 4:05:06` (day and time). A fractional unit **cascades into the next
/// one down**: `1.5 months` is `1 mon 15 days` and `1.5 days` is `1 day 12:00:00`.
pub fn from_text(text: &str) -> Result<Interval> {
    let body = text.trim();
    if body.is_empty() {
        return Err(invalid(text));
    }
    if body.starts_with('P') || body.starts_with('p') {
        return iso_8601(text, body);
    }
    let (body, negate) = match body.strip_suffix(" ago") {
        Some(head) => (head.trim_end(), true),
        None => (body, false),
    };
    let mut total = Interval {
        months: 0,
        days: 0,
        micros: 0,
    };
    let mut seen = false;
    let mut tokens = body.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        // `04:05:06` and `-12:00:00`: a clock, wherever it appears in the list.
        if token.contains(':') {
            add_micros(&mut total, clock_micros(text, token)?)?;
            seen = true;
            continue;
        }
        // `1-2`: years and months, the SQL year-to-month form.
        if let Some((years, months)) = year_month(token) {
            add_months(&mut total, years.saturating_mul(12), text)?;
            add_months(&mut total, months, text)?;
            seen = true;
            continue;
        }
        let amount: f64 = token.parse().map_err(|_| invalid(text))?;
        // **A bare number before a clock is a count of days**, which is the SQL day-to-second
        // form: `'3 4:05:06'` is three days and a time, not three of anything else.
        if tokens.peek().is_some_and(|next| next.contains(':')) {
            apply_unit(&mut total, amount, "days", text)?;
            seen = true;
            continue;
        }
        // Otherwise a number and the unit that names it. A bare number with no unit is not an
        // interval — `'1'::interval` is `22007` — so the unit is required.
        let unit = tokens.next().ok_or_else(|| invalid(text))?;
        apply_unit(&mut total, amount, unit, text)?;
        seen = true;
    }
    if !seen {
        return Err(invalid(text));
    }
    if negate {
        total = Interval {
            months: total.months.checked_neg().ok_or_else(out_of_range)?,
            days: total.days.checked_neg().ok_or_else(out_of_range)?,
            micros: total.micros.checked_neg().ok_or_else(out_of_range)?,
        };
    }
    Ok(total)
}

/// `P1Y2M3DT4H5M6S`, which is the style `ActiveRecord` asks a real server to *print* in.
fn iso_8601(text: &str, body: &str) -> Result<Interval> {
    let mut total = Interval {
        months: 0,
        days: 0,
        micros: 0,
    };
    let (date_part, time_part) = match body[1..].split_once(['T', 't']) {
        Some((date, time)) => (date, Some(time)),
        None => (&body[1..], None),
    };
    let mut number = String::new();
    for byte in date_part.chars() {
        if byte.is_ascii_digit() || byte == '-' || byte == '.' {
            number.push(byte);
            continue;
        }
        let value: i64 = number.parse().map_err(|_| invalid(text))?;
        number.clear();
        match byte {
            'Y' | 'y' => add_months(&mut total, value.saturating_mul(12), text)?,
            'M' | 'm' => add_months(&mut total, value, text)?,
            'D' | 'd' => add_days(&mut total, value, text)?,
            'W' | 'w' => add_days(&mut total, value.saturating_mul(7), text)?,
            _ => return Err(invalid(text)),
        }
    }
    if !number.is_empty() {
        return Err(invalid(text));
    }
    let Some(time_part) = time_part else {
        return Ok(total);
    };
    for byte in time_part.chars() {
        if byte.is_ascii_digit() || byte == '-' || byte == '.' {
            number.push(byte);
            continue;
        }
        let value: f64 = number.parse().map_err(|_| invalid(text))?;
        number.clear();
        let unit = match byte {
            'H' | 'h' => MICROS_PER_HOUR,
            'M' | 'm' => MICROS_PER_MINUTE,
            'S' | 's' => MICROS_PER_SECOND,
            _ => return Err(invalid(text)),
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "the unit constants are exact in an `f64`, and the product is rounded to \
                      the microsecond, which is this type's floor"
        )]
        add_micros(&mut total, (value * unit as f64).round() as i64)?;
    }
    if number.is_empty() {
        Ok(total)
    } else {
        Err(invalid(text))
    }
}

/// One `<number> <unit>` pair, with a fraction cascading into the unit below.
///
/// **A fractional unit does not round, it cascades**: `1.5 months` is `1 mon 15 days`, `1.5 days`
/// is `1 day 12:00:00` and `1.5 hours` is `01:30:00`. A month's fraction is thirty days' worth
/// and a day's is twenty-four hours' worth — the same conversion comparison uses, and the only
/// place storage does any converting at all.
fn apply_unit(total: &mut Interval, amount: f64, unit: &str, text: &str) -> Result<()> {
    let unit = unit.trim_end_matches(',').to_ascii_lowercase();
    let whole = amount.trunc();
    let fraction = amount - whole;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the whole part of a parsed unit count, bounds-checked by the adders below"
    )]
    let whole = whole as i64;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a fraction of a unit, in microseconds, which is this type's floor"
    )]
    match unit.as_str() {
        "millennium" | "millennia" | "millenniums" => {
            add_months(total, whole.saturating_mul(12_000), text)?;
        }
        "century" | "centuries" => add_months(total, whole.saturating_mul(1_200), text)?,
        "decade" | "decades" => add_months(total, whole.saturating_mul(120), text)?,
        // **The two overflows are different errors.** A count of years whose months do not fit
        // is the whole value overflowing — `'178956971 years'` is `22008` — where a months field
        // too large to be one is `22015`. The multiplication is checked here so the right one is
        // raised; `add_months` below only ever sees a number that was written as months.
        "year" | "years" | "y" | "yr" | "yrs" => {
            let months = whole.checked_mul(12).ok_or_else(out_of_range)?;
            if i32::try_from(months).is_err() {
                return Err(out_of_range());
            }
            add_months(total, months, text)?;
            add_months(total, (fraction * 12.0) as i64, text)?;
        }
        "month" | "months" | "mon" | "mons" => {
            add_months(total, whole, text)?;
            // Half a month is fifteen days, which is thirty days' half.
            add_days(total, (fraction * 30.0) as i64, text)?;
        }
        "week" | "weeks" | "w" => {
            add_days(total, whole.saturating_mul(7), text)?;
            add_micros(total, (fraction * 7.0 * MICROS_PER_DAY as f64) as i64)?;
        }
        "day" | "days" | "d" => {
            add_days(total, whole, text)?;
            add_micros(total, (fraction * MICROS_PER_DAY as f64) as i64)?;
        }
        "hour" | "hours" | "h" | "hr" | "hrs" => {
            add_micros(total, (amount * MICROS_PER_HOUR as f64).round() as i64)?;
        }
        "minute" | "minutes" | "min" | "mins" | "m" => {
            add_micros(total, (amount * MICROS_PER_MINUTE as f64).round() as i64)?;
        }
        "second" | "seconds" | "sec" | "secs" | "s" => {
            add_micros(total, (amount * MICROS_PER_SECOND as f64).round() as i64)?;
        }
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => {
            add_micros(total, (amount * 1_000.0).round() as i64)?;
        }
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => {
            add_micros(total, amount.round() as i64)?;
        }
        _ => return Err(invalid(text)),
    }
    Ok(())
}

/// `1-2`, the SQL year-to-month form, as `(years, months)`.
fn year_month(token: &str) -> Option<(i64, i64)> {
    let (years, months) = token.strip_prefix('-').unwrap_or(token).split_once('-')?;
    let negative = token.starts_with('-');
    let years: i64 = years.parse().ok()?;
    let months: i64 = months.parse().ok()?;
    if negative {
        Some((-years, -months))
    } else {
        Some((years, months))
    }
}

/// A `HH:MM:SS[.ffffff]` clock as microseconds, with its own sign.
fn clock_micros(text: &str, token: &str) -> Result<i64> {
    let (token, negative) = match token.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let mut parts = token.split(':');
    let hours: i64 = parts
        .next()
        .ok_or_else(|| invalid(text))?
        .parse()
        .map_err(|_| invalid(text))?;
    let minutes: i64 = parts
        .next()
        .ok_or_else(|| invalid(text))?
        .parse()
        .map_err(|_| invalid(text))?;
    let seconds: f64 = match parts.next() {
        Some(part) => part.parse().map_err(|_| invalid(text))?,
        None => 0.0,
    };
    if parts.next().is_some() {
        return Err(invalid(text));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the unit constants are exact in an `f64`, and the product is rounded to the \
                  microsecond, which is this type's floor"
    )]
    let micros = hours.saturating_mul(MICROS_PER_HOUR)
        + minutes.saturating_mul(MICROS_PER_MINUTE)
        + (seconds * MICROS_PER_SECOND as f64).round() as i64;
    Ok(if negative { -micros } else { micros })
}

fn add_months(total: &mut Interval, months: i64, text: &str) -> Result<()> {
    let sum = i64::from(total.months) + months;
    total.months = i32::try_from(sum).map_err(|_| field_out_of_range(text))?;
    Ok(())
}

fn add_days(total: &mut Interval, days: i64, text: &str) -> Result<()> {
    let sum = i64::from(total.days) + days;
    total.days = i32::try_from(sum).map_err(|_| field_out_of_range(text))?;
    Ok(())
}

fn add_micros(total: &mut Interval, micros: i64) -> Result<()> {
    total.micros = total.micros.checked_add(micros).ok_or_else(out_of_range)?;
    Ok(())
}

/// Comparison's view: one number of microseconds, a month being thirty days.
#[must_use]
pub fn total_micros(value: &Interval) -> i64 {
    esker_keys::row::interval_total(value.months, value.days, value.micros)
}

fn invalid(text: &str) -> SqlError {
    SqlError::InvalidDatetimeFormat {
        ty: "interval",
        value: text.to_owned(),
    }
}

/// `22015`, which one *field* overflowing gets and nothing else in this project does.
fn field_out_of_range(text: &str) -> SqlError {
    SqlError::IntervalFieldOutOfRange(text.to_owned())
}

/// `22008`, which the whole value overflowing gets.
fn out_of_range() -> SqlError {
    SqlError::IntervalOutOfRange
}
