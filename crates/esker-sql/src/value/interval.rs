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

use std::fmt::Write as _;

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

/// Which of PostgreSQL's four `IntervalStyle` settings an `interval` prints under.
///
/// **Not a rendering preference: `ActiveRecord` cannot read an interval without it.** The adapter
/// sends `SET intervalstyle = iso_8601` when it connects, and its `OID::Interval#cast_value`
/// parses the text with `ActiveSupport::Duration.parse` and **returns `nil`** when that raises —
/// so a server that answers `6 years 5 mons` to a client expecting `P6Y5M` hands back no value at
/// all rather than an error anyone can see. Measured, all four styles over forty-one values:
/// `tests/captures/pg19_interval_style.txt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// `6 years 5 mons 4 days 03:02:01` — the boot value.
    #[default]
    Postgres,
    /// `@ 6 years 5 mons 4 days 3 hours 2 mins 1 sec`, with ` ago` for a value that starts
    /// negative.
    PostgresVerbose,
    /// `+6-5 +4 +3:02:01`, or one unsigned part when the value has only one.
    SqlStandard,
    /// `P6Y5M4DT3H2M1S` — what `ActiveRecord` asks for.
    Iso8601,
}

impl Style {
    /// The setting's value as `SET intervalstyle` spells it, or `None` for a word that is not one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "postgres" => Some(Style::Postgres),
            "postgres_verbose" => Some(Style::PostgresVerbose),
            "sql_standard" => Some(Style::SqlStandard),
            "iso_8601" => Some(Style::Iso8601),
            _ => None,
        }
    }
}

/// What PostgreSQL's `interval_out` writes under the default `IntervalStyle` of `postgres`.
#[must_use]
pub fn to_text(value: &Interval) -> String {
    to_text_under(value, Style::Postgres)
}

/// The same value under any of the four styles.
///
/// # The four sign rules, and no two are the same
///
/// Each style decides differently what to do with a value whose fields disagree in sign, and every
/// one of these was measured rather than reasoned about (`1 month -1 day` and `-1 day 1 second`
/// are the two that separate them):
///
/// | | `-1 mon 1 day` | `-4:05:06` |
/// |---|---|---|
/// | `postgres` | `-1 mons +1 day` — a **`+` on a field that follows a negative one** | `-04:05:06` |
/// | `iso_8601` | `P-1M1D` — a sign per field and never a `+` | `PT-4H-5M-6S` |
/// | `postgres_verbose` | `@ 1 mon -1 days ago` — the **first** field's sign becomes ` ago` and every field is written relative to it | `@ 4 hours 5 mins 6 secs ago` |
/// | `sql_standard` | `-0-1 +1 +0:00:00` — all three parts, each signed, whenever the parts disagree or both halves are present | `-4:05:06` |
///
/// The plural rules differ too: an integer part pluralises on its **signed** value, so `-1` is
/// `-1 days`, while the verbose seconds pluralise on the **absolute** one, so the same `-1` is
/// `-1 sec`. Both measured.
#[must_use]
pub fn to_text_under(value: &Interval, style: Style) -> String {
    let (years, months) = (value.months / 12, value.months % 12);
    let days = value.days;
    let (hours, minutes, seconds, fraction) = clock_parts(value.micros);
    match style {
        Style::Postgres => postgres(years, months, days, value.micros),
        Style::Iso8601 => iso_8601_text(years, months, days, hours, minutes, seconds, fraction),
        Style::PostgresVerbose => verbose(years, months, days, hours, minutes, seconds, fraction),
        Style::SqlStandard => sql_standard(years, months, days, hours, minutes, seconds, fraction),
    }
}

/// The time part split into its four signed pieces, each keeping the sign of the whole — which is
/// what makes `-4:05:06` four negative numbers rather than one negative and three positive.
fn clock_parts(micros: i64) -> (i64, i64, i64, i64) {
    (
        micros / MICROS_PER_HOUR,
        (micros / MICROS_PER_MINUTE) % 60,
        (micros / MICROS_PER_SECOND) % 60,
        micros % MICROS_PER_SECOND,
    )
}

/// `postgres`: a unit list, and a `+` on any field that follows a negative one.
fn postgres(years: i32, months: i32, days: i32, micros: i64) -> String {
    let mut out = String::new();
    let mut before = false;
    let mut empty = true;
    for (count, unit) in [(years, "year"), (months, "mon"), (days, "day")] {
        if count == 0 {
            continue;
        }
        if !empty {
            out.push(' ');
        }
        if before && count > 0 {
            out.push('+');
        }
        let _ = write!(out, "{count} {unit}{}", plural(count));
        before = count < 0;
        empty = false;
    }
    if micros != 0 || empty {
        if !empty {
            out.push(' ');
            if before && micros > 0 {
                out.push('+');
            }
        }
        out.push_str(&clock(micros));
    }
    out
}

/// `iso_8601`: `P` and `T`, a sign per field, and `PT0S` for a value with no fields at all.
///
/// Named apart from [`iso_8601`], which reads the same shape rather than writing it.
fn iso_8601_text(
    years: i32,
    months: i32,
    days: i32,
    hours: i64,
    minutes: i64,
    seconds: i64,
    fraction: i64,
) -> String {
    if years == 0
        && months == 0
        && days == 0
        && hours == 0
        && minutes == 0
        && seconds == 0
        && fraction == 0
    {
        return "PT0S".to_owned();
    }
    let mut out = String::from("P");
    for (count, unit) in [(years, 'Y'), (months, 'M'), (days, 'D')] {
        if count != 0 {
            let _ = write!(out, "{count}{unit}");
        }
    }
    if hours != 0 || minutes != 0 || seconds != 0 || fraction != 0 {
        out.push('T');
    }
    for (count, unit) in [(hours, 'H'), (minutes, 'M')] {
        if count != 0 {
            let _ = write!(out, "{count}{unit}");
        }
    }
    if seconds != 0 || fraction != 0 {
        if seconds < 0 || fraction < 0 {
            out.push('-');
        }
        out.push_str(&seconds_text(seconds.abs(), fraction.abs()));
        out.push('S');
    }
    out
}

/// `postgres_verbose`: the **first** non-zero field's sign decides ` ago`, and every field is
/// written relative to it.
fn verbose(
    years: i32,
    months: i32,
    days: i32,
    hours: i64,
    minutes: i64,
    seconds: i64,
    fraction: i64,
) -> String {
    let mut out = String::from("@");
    // `None` until a non-zero field has been seen; then `true` if that field was negative, which
    // is both the ` ago` at the end and the direction every later field is written in.
    let mut before: Option<bool> = None;
    for (count, unit) in [
        (i64::from(years), "year"),
        (i64::from(months), "mon"),
        (i64::from(days), "day"),
        (hours, "hour"),
        (minutes, "min"),
    ] {
        if count == 0 {
            continue;
        }
        let count = match before {
            None => {
                before = Some(count < 0);
                count.abs()
            }
            Some(true) => -count,
            Some(false) => count,
        };
        let _ = write!(out, " {count} {unit}{}", plural64(count));
    }
    if seconds != 0 || fraction != 0 {
        // The whole and the fraction turn together, so `-1.5` stays one number rather than
        // becoming `-1` seconds and half a positive one.
        let flip = match before {
            None => {
                before = Some(seconds < 0 || fraction < 0);
                before == Some(true)
            }
            Some(turned) => turned,
        };
        let (seconds, fraction) = if flip {
            (-seconds, -fraction)
        } else {
            (seconds, fraction)
        };
        let sign = if seconds < 0 || fraction < 0 { "-" } else { "" };
        let _ = write!(
            out,
            " {sign}{}",
            seconds_text(seconds.abs(), fraction.abs())
        );
        // **The absolute value pluralises here**, where a signed one does above: `-1 sec` and
        // `-1 days` in the same sentence. Measured, both.
        out.push_str(if seconds.abs() == 1 && fraction == 0 {
            " sec"
        } else {
            " secs"
        });
    }
    if out.len() == 1 {
        out.push_str(" 0");
    }
    if before == Some(true) {
        out.push_str(" ago");
    }
    out
}

/// `sql_standard`: one unsigned part when the value has only one and its fields agree in sign, and
/// all three parts each explicitly signed otherwise.
fn sql_standard(
    years: i32,
    months: i32,
    days: i32,
    hours: i64,
    minutes: i64,
    seconds: i64,
    fraction: i64,
) -> String {
    let time = [hours, minutes, seconds, fraction];
    let negative = years < 0 || months < 0 || days < 0 || time.iter().any(|part| *part < 0);
    let positive = years > 0 || months > 0 || days > 0 || time.iter().any(|part| *part > 0);
    let year_month = years != 0 || months != 0;
    let day_time = days != 0 || time.iter().any(|part| *part != 0);
    let clock = |sign: &str| {
        let mut out = format!(
            "{sign}{}:{:02}:{:02}",
            hours.abs(),
            minutes.abs(),
            seconds.abs()
        );
        if fraction != 0 {
            out.push('.');
            out.push_str(format!("{:06}", fraction.abs()).trim_end_matches('0'));
        }
        out
    };
    // The three-part form, whenever the parts disagree in sign or both halves are present. The
    // year-month and day parts are printed **even when they are zero**: `+0-0 +1 -1:00:00`.
    if (negative && positive) || (year_month && day_time) {
        let ym_sign = if years < 0 || months < 0 { "-" } else { "+" };
        let day_sign = if days < 0 { "-" } else { "+" };
        let time_sign = if time.iter().any(|part| *part < 0) {
            "-"
        } else {
            "+"
        };
        return format!(
            "{ym_sign}{}-{} {day_sign}{} {}",
            years.abs(),
            months.abs(),
            days.abs(),
            clock(time_sign)
        );
    }
    let sign = if negative { "-" } else { "" };
    if year_month {
        return format!("{sign}{}-{}", years.abs(), months.abs());
    }
    if !day_time {
        // A value with nothing in it at all, which is the one place this style writes no units.
        return "0".to_owned();
    }
    if days != 0 {
        return format!("{sign}{} {}", days.abs(), clock(""));
    }
    clock(sign)
}

/// A sign, digits and at most one point — which is what PostgreSQL's interval grammar admits for
/// a quantity with no unit. `3e2`, `inf` and `nan` all parse as `f64` and none of them is one.
fn is_plain_decimal(token: &str) -> bool {
    let digits = token.strip_prefix(['+', '-']).unwrap_or(token);
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.chars().filter(|c| *c == '.').count() <= 1
        && digits.chars().any(|c| c.is_ascii_digit())
}

/// `s` unless the number is exactly one — and **`-1` is plural**, which is the rule a reader
/// guesses wrong: `-1 days`, not `-1 day`.
fn plural(count: i32) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The same rule for the fields the verbose style widens.
fn plural64(count: i64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// `s[.ffffff]` with trailing zeros stripped, for values that already carry no sign.
fn seconds_text(seconds: i64, fraction: i64) -> String {
    let mut out = seconds.to_string();
    if fraction != 0 {
        out.push('.');
        out.push_str(format!("{fraction:06}").trim_end_matches('0'));
    }
    out
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
    // Whether a clock has already given the time part, which is what makes `'5 days 3'` three
    // seconds and `'1:00 5'` an error. Measured, both.
    let mut clocked = false;
    let mut tokens = body.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        // `04:05:06` and `-12:00:00`: a clock, wherever it appears in the list.
        if token.contains(':') {
            add_micros(&mut total, clock_micros(text, token)?)?;
            seen = true;
            clocked = true;
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
        // **A trailing bare number is a count of seconds.** `'5'::interval` is `00:00:05`, and so
        // is the `3` in `'5 days 3'` and in `'1-2 3'` — measured, and the comment that used to
        // stand here said `'1'::interval` was `22007`, which was a guess and was wrong.
        //
        // Three conditions, each of them a measured refusal rather than caution: the number must
        // be **last** (`'3 5 days'` is `22007`), the time part must not already have been given
        // (`'1:00 5'` is `22007`), and there must be no `ago` (`'3 ago'` is `22007` where
        // `'1 day ago'` is not). The spelling is stricter than a `f64` parse, because `'3e2'` is
        // `22007` there and `"inf"` and `"nan"` parse as floats here.
        if tokens.peek().is_none() && !clocked && !negate && is_plain_decimal(token) {
            apply_unit(&mut total, amount, "seconds", text)?;
            seen = true;
            continue;
        }
        // Otherwise a number and the unit that names it.
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
