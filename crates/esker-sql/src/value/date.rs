//! `date`: a day, and the two things that make it not a rounded-down timestamp.
//!
//! Four bytes, a signed count of days from 2000-01-01 — PostgreSQL's own representation and its
//! own epoch, so the number a row holds is the number a real server holds. The calendar under it
//! is `super::timestamp`'s, which is the point of putting this beside it: one implementation of
//! `days_from_civil` and its inverse, exercised by both types.
//!
//! # `22008` has two messages and one of them sometimes has a hint
//!
//! Past the type's ends is `date out of range: "5874898-01-01"`. An impossible *field* is
//! `date/time field value out of range: "2020-02-30"`. They are not interchangeable, and only the
//! second kind sometimes carries `HINT: Perhaps you need a different "DateStyle" setting.` —
//! `2020-13-01` gets it and `2020-02-30` does not, **because a 13 could have been a day** and a 30
//! could not have been a month. That rule is the reason the hint is decided here, where the
//! offending field is still known, rather than at the error type.
//!
//! # The infinities are the ends of the `i32`, not the ends of the calendar
//!
//! `'infinity'::date` sorts above `5874897-12-31` and absorbs arithmetic. They are `i32::MAX` and
//! `i32::MIN`, the same sentinels PostgreSQL uses, which is what lets the ordinary integer
//! comparison be the right one.

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, PgType as _};

/// `'infinity'::date`, which sorts above every real day.
pub const POS_INFINITY: i32 = i32::MAX;

/// `'-infinity'::date`, which sorts below every real day.
pub const NEG_INFINITY: i32 = i32::MIN;

/// `4713-01-01 BC`, the first day PostgreSQL will store. Measured, not derived: one day earlier is
/// `22008`.
pub const MIN_DAY: i32 = -2_451_507;

/// `5874897-12-31`, the last. Measured the same way.
pub const MAX_DAY: i32 = 2_145_031_948;

/// Days from 2000-01-01 to 1970-01-01, which is what `'epoch'` means.
const EPOCH_DAY: i32 = -10_957;

/// The three-letter month names PostgreSQL accepts, in order.
///
/// A prefix match rather than an exact one: `January`, `Jan` and `JANUARY` are all the same month
/// on a real server, which is one rule and not three.
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// What PostgreSQL's `date_out` writes under the default `DateStyle` of `ISO, MDY`.
///
/// **`DateStyle` is a session setting this node does not have**, so this is the only spelling it
/// prints — declared in `tests/date.rs`, where the four PostgreSQL styles of one value are
/// captured. A year before 1 AD prints its astronomical year negated with a ` BC` suffix, and
/// every year is padded to four digits.
#[must_use]
pub fn to_text(day: i32) -> String {
    match day {
        POS_INFINITY => return "infinity".to_owned(),
        NEG_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }
    // `civil_from_days` counts from **1970-01-01** and a `date` counts from 2000-01-01, which is
    // the offset `super::timestamp` adds at the same place. Two epochs in one calendar, and the
    // bridge is written at both crossings rather than folded into either function.
    let (year, month, dom) =
        super::timestamp::civil_from_days(i64::from(day) + super::timestamp::UNIX_TO_PG_EPOCH_DAYS);
    // Astronomical year 0 is 1 BC, year -1 is 2 BC, and PostgreSQL prints the era rather than the
    // sign — so the printed number is one more than the negated astronomical one.
    let (year, era) = if year <= 0 {
        (1 - year, " BC")
    } else {
        (year, "")
    };
    format!("{year:04}-{month:02}-{dom:02}{era}")
}

/// PostgreSQL's `date_in`, for the inputs this node reads.
///
/// The accepted shapes, all measured: `2020-01-02`, `2020-1-2`, `20200101`, `2020-Jan-02`,
/// `Jan 2, 2020`, `2 Jan 2020`, `January 2, 2020`, `01/02/2020` (month first, which is `MDY`'s
/// rule and the default), any of those with a **time part** after it, which is discarded, and the
/// words `epoch`, `infinity`, `-infinity`, `today`, `tomorrow`, `yesterday` and `now`.
///
/// `today` and its neighbours resolve against `now`, which is the caller's — a value that changes
/// per statement cannot come from here without a clock, and this crate has none of its own
/// (`docs/DESIGN.md` §6: no node uses its wall clock).
pub fn from_text(text: &str, now_micros: i64) -> Result<i32> {
    let body = text.trim();
    if let Some(day) = keyword(body, now_micros) {
        return Ok(day);
    }
    // A time part is discarded, which is what a real server does: `'2020-01-01 12:34:56'::date` is
    // `2020-01-01`. The separator is a space or a `T`.
    let body = body
        .split_once([' ', 'T', 't'])
        .map_or(body, |(head, tail)| {
            // ...unless what follows is an era, which is part of the date and not a time.
            if tail.trim().eq_ignore_ascii_case("bc") || tail.trim().eq_ignore_ascii_case("ad") {
                body
            } else if head.chars().any(|c| c.is_ascii_digit()) && looks_like_time(tail.trim()) {
                head
            } else {
                body
            }
        });
    let (body, bc) = match () {
        () if ends_with_era(body, "bc") => (body[..body.len() - 3].trim_end(), true),
        () if ends_with_era(body, "ad") => (body[..body.len() - 3].trim_end(), false),
        () => (body, false),
    };
    let (year, month, dom) = fields(body).ok_or_else(|| invalid(text))?;
    // **There is no year zero, and the rule is about the year as written.** `0000-01-01` is
    // `22008` and `0001-01-01 BC` is a value — so the check comes *before* the era is applied,
    // and 1 BC then becomes astronomical year 0, which is a perfectly good number.
    if year == 0 || !(1..=12).contains(&month) {
        return Err(field_out_of_range(text, month > 12 && month <= 31));
    }
    let year = if bc { 1 - year } else { year };
    if dom < 1 || dom > super::timestamp::days_in_month(year, month) {
        return Err(field_out_of_range(text, false));
    }
    let day = super::timestamp::days_from_pg_epoch_checked(year, month, dom)
        .and_then(|day| i32::try_from(day).ok())
        .filter(|day| (MIN_DAY..=MAX_DAY).contains(day))
        .ok_or_else(|| SqlError::DatetimeOutOfRange {
            ty: ColumnType::Date.name(),
            value: text.to_owned(),
        })?;
    Ok(day)
}

/// Whether the tail of a split looks like a clock rather than the rest of a date.
fn looks_like_time(tail: &str) -> bool {
    tail.contains(':')
}

/// A ` BC`/` AD` suffix, which needs the space so that a month called `bc` could not be one.
fn ends_with_era(body: &str, era: &str) -> bool {
    body.len() > 3
        && body.as_bytes()[body.len() - 3] == b' '
        && body[body.len() - 2..].eq_ignore_ascii_case(era)
}

/// The clock words and the two infinities.
fn keyword(body: &str, now_micros: i64) -> Option<i32> {
    let today = || {
        let micros = now_micros;
        // Floor division, so a negative microsecond count lands on the day that contains it rather
        // than the one after.
        i32::try_from(micros.div_euclid(86_400_000_000)).unwrap_or(0)
    };
    Some(match () {
        () if body.eq_ignore_ascii_case("infinity") || body.eq_ignore_ascii_case("+infinity") => {
            POS_INFINITY
        }
        () if body.eq_ignore_ascii_case("-infinity") => NEG_INFINITY,
        () if body.eq_ignore_ascii_case("epoch") => EPOCH_DAY,
        () if body.eq_ignore_ascii_case("today") || body.eq_ignore_ascii_case("now") => today(),
        () if body.eq_ignore_ascii_case("tomorrow") => today().saturating_add(1),
        () if body.eq_ignore_ascii_case("yesterday") => today().saturating_sub(1),
        () => return None,
    })
}

/// The year, month and day of a date with no era and no time, in whichever order it was written.
fn fields(body: &str) -> Option<(i64, i64, i64)> {
    // `20200101`: eight digits and nothing else, which is unambiguous and has its own rule.
    if body.len() == 8 && body.bytes().all(|b| b.is_ascii_digit()) {
        return Some((
            body[..4].parse().ok()?,
            body[4..6].parse().ok()?,
            body[6..].parse().ok()?,
        ));
    }
    let parts: Vec<&str> = body
        .split(['-', '/', '.', ' ', ','])
        .filter(|part| !part.is_empty())
        .collect();
    let [a, b, c] = parts.as_slice() else {
        return None;
    };
    // Exactly one part may be a month name, and where it is decides where the others go.
    match (month_name(a), month_name(b), month_name(c)) {
        // `Jan 2, 2020` and `January 2, 2020`.
        (Some(month), None, None) => Some((c.parse().ok()?, month, b.parse().ok()?)),
        // `2020-Jan-02` and `2 Jan 2020`, told apart by which end the four-digit year is at.
        (None, Some(month), None) => {
            let (first, last): (i64, i64) = (a.parse().ok()?, c.parse().ok()?);
            if a.len() >= 4 {
                Some((first, month, last))
            } else {
                Some((last, month, first))
            }
        }
        (None, None, None) => {
            let (first, second, third): (i64, i64, i64) =
                (a.parse().ok()?, b.parse().ok()?, c.parse().ok()?);
            // A four-digit leading field is the year — the ISO order. Otherwise **month first**,
            // which is `MDY`'s rule and PostgreSQL's default: `01/02/2020` is 2 January there and
            // 1 February under `DMY`. One literal, two dates, no error either way, which is why
            // the setting is worth a divergence entry rather than a shrug.
            if a.len() >= 4 {
                Some((first, second, third))
            } else {
                Some((third, first, second))
            }
        }
        _ => None,
    }
}

/// The month a name is, one-based, or `None` — a prefix match of at least three letters.
fn month_name(part: &str) -> Option<i64> {
    if part.len() < 3 || !part.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let lower = part.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| month.starts_with(&lower))
        .and_then(|at| i64::try_from(at).ok())
        .map(|at| at + 1)
}

/// The microsecond instant a day names: **its midnight**.
///
/// What makes `'2020-01-01'::date = '2020-01-01'::timestamp` true. The infinities map onto the
/// timestamp infinities rather than onto an arithmetic product, because `i32::MAX` days is not
/// `i64::MAX` microseconds and the two types have to agree about which end they are at.
#[must_use]
pub fn as_micros(day: i32) -> i64 {
    match day {
        POS_INFINITY => super::timestamp::POS_INFINITY,
        NEG_INFINITY => super::timestamp::NEG_INFINITY,
        _ => i64::from(day) * 86_400_000_000,
    }
}

fn invalid(text: &str) -> SqlError {
    SqlError::InvalidDatetimeFormat {
        ty: ColumnType::Date.name(),
        value: text.to_owned(),
    }
}

/// `22008 date/time field value out of range`, with PostgreSQL's hint rule.
///
/// The hint appears when the offending field **could have been a day** — `2020-13-01` gets it
/// because a 13 is a plausible day under `DMY`, and `2020-02-30` does not because a 30 is not a
/// plausible month under anything. Measured, both ways.
fn field_out_of_range(text: &str, could_be_a_day: bool) -> SqlError {
    SqlError::DatetimeFieldOutOfRange {
        value: text.to_owned(),
        datestyle_hint: could_be_a_day,
    }
}
