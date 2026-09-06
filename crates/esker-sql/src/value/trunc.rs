//! `date_trunc`: the unit table, and the three types it cuts.
//!
//! Everything here is measured in `tests/captures/pg19_date_trunc.txt`. Three of the rules are not
//! what reasoning gives, and each is a comment where it is implemented: the unit table is a
//! synonym table with one member that has no synonyms, an unknown unit and an inapplicable one are
//! **different error classes**, and a `timestamptz` is cut in the session's zone — which makes
//! local midnight, the instant zones move, this function's hard case rather than an edge of it.

use super::timestamp::{
    NEG_INFINITY, PG_EPOCH_UNIX_SECONDS, POS_INFINITY, UNIX_TO_PG_EPOCH_DAYS, civil_from_days,
    days_from_pg_epoch,
};
use super::zone::Zone;
use crate::error::{Result, SqlError};

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const MICROS_PER_DAY: i64 = SECONDS_PER_DAY * MICROS_PER_SECOND;

/// A field `date_trunc` can cut at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    /// The whole value: this type's resolution is already a microsecond.
    Microseconds,
    /// A thousand microseconds.
    Milliseconds,
    /// The second, dropping the fraction.
    Second,
    /// The minute.
    Minute,
    /// The hour.
    Hour,
    /// Midnight, which for a zoned value is midnight *there*.
    Day,
    /// **Monday**, measured, and not Sunday.
    Week,
    /// The first of the month.
    Month,
    /// The first of January, April, July or October.
    Quarter,
    /// The first of January.
    Year,
    /// The years that share a first digit, so AD 1 falls into the decade named `0001 BC`.
    Decade,
    /// The century **beginning in year 1**: 2000 belongs to the one starting 1901.
    Century,
    /// The millennium beginning in year 1, by the same rule.
    Millennium,
}

/// What a spelling of the first argument resolves to.
///
/// The three answers are three different behaviours, and the last two are **different SQLSTATEs**:
/// a unit the table does not hold is `22023 … not recognized`, and one it holds that this function
/// cannot apply is `0A000 … not supported`. `epoch` and `dow` are real `EXTRACT` fields and land
/// in the *first* class, which is the way round reasoning does not put them.
#[derive(Debug)]
pub enum Lookup {
    /// A field this function cuts at.
    Field(Unit),
    /// A field the table holds and this function will not apply: the three `timezone` spellings.
    Inapplicable,
    /// Not a field at all.
    Unknown,
}

/// The unit a spelling names.
///
/// **The table is not regular**, and the irregularity is measured rather than reasoned: every unit
/// here has at least one alias except `quarter`, which has neither a plural nor an abbreviation —
/// `quarters` and `q` are both refused while `months`, `mons`, `centuries`, `cent`, `c`,
/// `millennia` and `mil` are all accepted. `m` is the **minute**. The comparison is
/// case-insensitive and the spelling is **not** trimmed: `' month'` is a refusal, because
/// PostgreSQL's own lookup does not trim either.
#[must_use]
pub fn lookup(name: &str) -> Lookup {
    match name.to_ascii_lowercase().as_str() {
        "microseconds" | "microsecond" | "usecs" | "usec" | "us" => {
            Lookup::Field(Unit::Microseconds)
        }
        "milliseconds" | "millisecond" | "msecs" | "msec" | "ms" => {
            Lookup::Field(Unit::Milliseconds)
        }
        "seconds" | "second" | "secs" | "sec" | "s" => Lookup::Field(Unit::Second),
        "minutes" | "minute" | "mins" | "min" | "m" => Lookup::Field(Unit::Minute),
        "hours" | "hour" | "hrs" | "hr" | "h" => Lookup::Field(Unit::Hour),
        "days" | "day" | "d" => Lookup::Field(Unit::Day),
        "weeks" | "week" | "w" => Lookup::Field(Unit::Week),
        "months" | "month" | "mons" | "mon" => Lookup::Field(Unit::Month),
        "quarter" => Lookup::Field(Unit::Quarter),
        "years" | "year" | "yrs" | "yr" | "y" => Lookup::Field(Unit::Year),
        "decades" | "decade" | "dec" => Lookup::Field(Unit::Decade),
        "centuries" | "century" | "cent" | "c" => Lookup::Field(Unit::Century),
        "millenniums" | "millennia" | "millennium" | "mil" => Lookup::Field(Unit::Millennium),
        "timezone" | "timezone_hour" | "timezone_minute" => Lookup::Inapplicable,
        _ => Lookup::Unknown,
    }
}

/// `22023`, for a spelling the table does not hold.
#[must_use]
pub fn not_recognized(unit: &str, ty: &str) -> SqlError {
    SqlError::DateTruncUnitNotRecognized {
        unit: unit.to_owned(),
        ty: ty.to_owned(),
    }
}

/// `0A000`, for a unit the table holds that this type cannot be cut at.
#[must_use]
pub fn not_supported(unit: &str, ty: &str, detail: &str) -> SqlError {
    SqlError::DateTruncUnitNotSupported {
        unit: unit.to_owned(),
        ty: ty.to_owned(),
        detail: detail.to_owned(),
    }
}

/// Cut an unzoned `timestamp`, in microseconds from 2000-01-01.
///
/// Both infinities pass through: `date_trunc('day', 'infinity'::timestamp)` is `infinity`,
/// measured.
#[must_use]
pub fn timestamp(micros: i64, unit: Unit) -> i64 {
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        return micros;
    }
    // Sub-day units never touch the date, so they never need the calendar.
    let below_a_day = match unit {
        Unit::Microseconds => return micros,
        Unit::Milliseconds => Some(1_000),
        Unit::Second => Some(MICROS_PER_SECOND),
        Unit::Minute => Some(60 * MICROS_PER_SECOND),
        Unit::Hour => Some(3_600 * MICROS_PER_SECOND),
        _ => None,
    };
    if let Some(step) = below_a_day {
        // `div_euclid`, not `/`: a timestamp before 2000 is negative here, and truncation toward
        // zero would round it the wrong way — *up*, to a later instant than the one asked for.
        return micros.div_euclid(step) * step;
    }

    let days = micros.div_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days + UNIX_TO_PG_EPOCH_DAYS);
    let (year, month, day) = match unit {
        // **Monday**, measured, and `rem_euclid` because the epoch is a Saturday and dates before
        // it are negative. 2000-01-01 was a Saturday, so day 0 is weekday 5 counting from Monday.
        Unit::Week => civil_from_days(days - (days + 5).rem_euclid(7) + UNIX_TO_PG_EPOCH_DAYS),
        Unit::Month => (year, month, 1),
        Unit::Quarter => (year, (month - 1) / 3 * 3 + 1, 1),
        Unit::Year => (year, 1, 1),
        Unit::Decade => (floor_div(year, 10) * 10, 1, 1),
        Unit::Century => (ceil_div(year, 100) * 100 - 99, 1, 1),
        Unit::Millennium => (ceil_div(year, 1_000) * 1_000 - 999, 1, 1),
        // `Day` keeps the date it was given, and everything below a day was handled above and
        // never reaches here.
        Unit::Day
        | Unit::Microseconds
        | Unit::Milliseconds
        | Unit::Second
        | Unit::Minute
        | Unit::Hour => (year, month, day),
    };
    days_from_pg_epoch(year, month, day) * MICROS_PER_DAY
}

/// A decade is the years sharing a first digit, so it **floors**: AD 1 truncated to a decade is
/// `0001-01-01 BC` — measured, and the one row that says the arithmetic is not `year / 10 * 10`
/// in a language whose division truncates toward zero.
const fn floor_div(value: i64, by: i64) -> i64 {
    value.div_euclid(by)
}

/// A century and a millennium **begin in year 1**, so they round the other way: 2000 belongs to
/// the century starting 1901 and 2001 opens its own. Measured on both sides of the boundary and
/// on both sides of the era.
const fn ceil_div(value: i64, by: i64) -> i64 {
    -((-value).div_euclid(by))
}

/// Cut a `timestamptz`, which happens **in the session's zone** and not in UTC.
///
/// The same statement answers a different *month* in New York and in UTC. The instant comes back
/// through a rule that is measured rather than derived, because none of the obvious ones fit all
/// five cases:
///
/// 1. Cut in local time, using the offset in force **at the input instant**.
/// 2. If that offset is still the one in force at the resulting instant, that is the answer —
///    which is why two instants an hour apart that are the same *local* time truncate to two
///    different instants, both correct.
/// 3. Otherwise re-resolve with the offset actually in force there.
/// 4. If neither offset is self-consistent the local time is one the zone skipped, and the answer
///    is **the transition itself** — local midnight in Beirut on the day it springs forward is
///    `01:00+03`, not `00:00` of either offset.
#[must_use]
pub fn timestamptz(micros: i64, unit: Unit, zone: Option<&Zone>) -> i64 {
    let Some(zone) = zone else {
        return timestamp(micros, unit);
    };
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        return micros;
    }
    let offset_at = |instant: i64| -> i64 {
        i64::from(
            zone.offset_at(instant.div_euclid(MICROS_PER_SECOND) + PG_EPOCH_UNIX_SECONDS)
                .seconds,
        ) * MICROS_PER_SECOND
    };

    let held = offset_at(micros);
    let local = timestamp(micros.saturating_add(held), unit);

    let candidate = local.saturating_sub(held);
    if offset_at(candidate) == held {
        return candidate;
    }
    let other = offset_at(candidate);
    let second = local.saturating_sub(other);
    if offset_at(second) == other {
        return second;
    }
    // A local time the zone never showed. The two candidates straddle the transition, and the
    // transition is the answer; a binary search finds it with the offsets alone, which is all a
    // zone is asked for anywhere else in this crate.
    let (mut low, mut high) = if candidate < second {
        (candidate, second)
    } else {
        (second, candidate)
    };
    let at_low = offset_at(low);
    while low + MICROS_PER_SECOND <= high {
        let middle = low + (high - low) / 2;
        if offset_at(middle) == at_low {
            low = middle;
        } else {
            high = middle;
        }
    }
    high
}

/// Cut an `interval`, whose fields are its own three and which loses everything below the unit.
///
/// **Truncating past its size gives zero**, not an empty interval: a 3-year interval cut at a
/// decade is `00:00:00`. `week` is the one field an interval refuses, and the reason is
/// PostgreSQL's own sentence — a month has no whole number of weeks.
pub fn interval(
    months: i32,
    days: i32,
    micros: i64,
    unit: Unit,
    spelling: &str,
) -> Result<(i32, i32, i64)> {
    let sub_day = |step: i64| (months, days, micros / step * step);
    Ok(match unit {
        Unit::Microseconds => (months, days, micros),
        Unit::Milliseconds => sub_day(1_000),
        Unit::Second => sub_day(MICROS_PER_SECOND),
        Unit::Minute => sub_day(60 * MICROS_PER_SECOND),
        Unit::Hour => sub_day(3_600 * MICROS_PER_SECOND),
        Unit::Day => (months, days, 0),
        Unit::Week => {
            return Err(not_supported(
                spelling,
                "interval",
                "Months usually have fractional weeks.",
            ));
        }
        Unit::Month => (months, 0, 0),
        Unit::Quarter => (months / 3 * 3, 0, 0),
        Unit::Year => (months / 12 * 12, 0, 0),
        Unit::Decade => (months / 120 * 120, 0, 0),
        Unit::Century => (months / 1_200 * 1_200, 0, 0),
        Unit::Millennium => (months / 12_000 * 12_000, 0, 0),
    })
}
