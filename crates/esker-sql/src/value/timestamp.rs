//! `timestamp with time zone`, in and out of text.
//!
//! The stored value is PostgreSQL's own: microseconds from 2000-01-01 00:00:00 UTC as an `i64`,
//! with `i64::MIN` and `i64::MAX` reserved for `-infinity` and `infinity`. Sharing the
//! representation is not imitation for its own sake — it is what makes a value survive a round
//! trip through PostgreSQL's *binary* format without arithmetic, and it puts our range ends
//! exactly where a client already expects them.
//!
//! Only UTC is ever printed. This node has no `SET TimeZone` (`docs/plans/phase-6a.md` §3 keeps
//! every session-behaviour `SET` out of phase 6a), so the offset in the output is always `+00`,
//! which is what a server whose `TimeZone` is `Etc/UTC` prints.
//!
//! # What is read, and what is honestly refused
//!
//! The input side reads ISO 8601 — `YYYY-MM-DD`, optionally `YYYYMMDD`, optionally a time, a
//! fraction, and a `Z`/`±HH[:MM]` offset — plus `infinity`. PostgreSQL's own datetime parser reads
//! far more: `01/02/2024` under `DateStyle`, named zones like `America/New_York`, and the special
//! values `now`, `today`, `epoch`. Those are not answered with a wrong instant and not answered
//! with a syntax error about input that is in fact valid; they are contract C2's `0A000`, naming
//! what was not understood. `tests/value_parity.rs` holds that list against the server.
//!
//! Five rules here came off the server rather than out of the specification: `24:00:00` is a legal
//! way to write the next midnight but `24:00:01` is not; `23:59:60` is legal and rolls over;
//! fractions beyond six digits **round**, and rounding can carry into the second; a zone
//! displacement past `±15:59` is `22009` and not `22008`; and an era suffix prints *after* the
//! offset, as `0001-01-01 00:00:00+00 BC`.

use super::PgType;
use crate::error::{Result, SqlError};
use crate::value::ColumnType;

/// `infinity`. PostgreSQL's `DT_NOEND`, and larger than every finite value by construction.
pub const POS_INFINITY: i64 = i64::MAX;

/// `-infinity`. PostgreSQL's `DT_NOBEGIN`.
pub const NEG_INFINITY: i64 = i64::MIN;

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const MICROS_PER_DAY: i64 = SECONDS_PER_DAY * MICROS_PER_SECOND;

/// Days from the Unix epoch to PostgreSQL's, which is where our zero is.
pub(super) const UNIX_TO_PG_EPOCH_DAYS: i64 = 10_957;

/// The earliest instant: 4714-11-24 00:00:00 BC, the start of Julian day 0, which is where
/// PostgreSQL's range starts too.
pub const MIN_MICROS: i64 = days_from_pg_epoch(-4713, 11, 24) * MICROS_PER_DAY;

/// The latest instant: 294276-12-31 23:59:59.999999.
pub const MAX_MICROS: i64 =
    days_from_pg_epoch(294_276, 12, 31) * MICROS_PER_DAY + MICROS_PER_DAY - 1;

/// The largest zone displacement PostgreSQL accepts is under 16 hours; `+15` is read and `+16` is
/// `22009`.
const MAX_ZONE_SECONDS: i64 = 16 * 3600;

/// What PostgreSQL's `timestamptz_out` writes, for a server whose `TimeZone` is UTC.
/// `micros` rounded to `precision` fractional digits, which is what a `timestamp(p)` column does
/// to a value on the way in.
///
/// **Half away from zero, and zero is PostgreSQL's epoch.** Two rules, both measured
/// (`tests/corpus/pg19_typmod.txt`), and neither is the one the parser uses a few lines below —
/// which breaks a microsecond tie to the *even* neighbour. They are different functions in
/// PostgreSQL too (`AdjustTimestampForTypmod` against the datetime parser), and this is what it
/// looks like when one type has two rounding rules.
///
/// The sign is what makes it strange from outside. A timestamp before 2000-01-01 is a negative
/// count, so rounding its magnitude away from zero moves it *earlier*: `1970-01-01 00:00:00.0005`
/// at `timestamp(3)` is `1970-01-01 00:00:00`, where the same fraction in 2020 rounds up. A tie
/// therefore goes opposite ways on the two sides of the year 2000, which is not a bug here and
/// not one there — it is what negate-round-negate does, and a client sees it either way.
///
/// The infinities have no fraction and are returned as they are.
pub(super) fn round_to_precision(micros: i64, precision: u32) -> i64 {
    if precision >= 6 || micros == POS_INFINITY || micros == NEG_INFINITY {
        return micros;
    }
    // 10^(6 - precision): the number of microseconds one digit of the kept fraction is worth.
    let scale = 10i64.pow(6 - precision);
    let half = scale / 2;
    let magnitude = micros.unsigned_abs();
    let scale = scale.unsigned_abs();
    let half = half.unsigned_abs();
    // In `u64`, so that a value near the ends of the range cannot overflow on the way to being
    // rounded down to something well inside it.
    let rounded = (magnitude + half) / scale * scale;
    let rounded = i64::try_from(rounded).unwrap_or(i64::MAX);
    if micros < 0 { -rounded } else { rounded }
}

pub(super) fn to_text(micros: i64) -> String {
    with_zone(micros, Zoning::Utc)
}

/// The same instant **in a session's time zone**, which is what a client is sent.
///
/// `None` is UTC, which is the boot value and what every path with no session uses — an index
/// key, an error message and a stored catalog default must not move when someone runs a `SET`.
///
/// The offset is printed in the shortest form that says it, which is PostgreSQL's rule and is not
/// cosmetic: `-05`, `+05:45` and `-04:56:02` are all measured
/// (`tests/captures/pg19_time_zone.txt`), the last being New York before it had a standard time.
pub(super) fn to_text_in(micros: i64, zone: Option<&super::zone::Zone>) -> String {
    match zone {
        None => with_zone(micros, Zoning::Utc),
        Some(zone) => {
            // The offset is the one in force **at this instant**, so it is looked up from the UTC
            // value and only then added: looking it up from the local value would ask the zone
            // about a time that does not exist twice a year.
            let unix = micros.div_euclid(MICROS_PER_SECOND) + PG_EPOCH_UNIX_SECONDS;
            let offset = zone.offset_at(unix).seconds;
            with_zone(
                micros.saturating_add(i64::from(offset) * MICROS_PER_SECOND),
                Zoning::Offset(offset),
            )
        }
    }
}

/// 2000-01-01T00:00:00Z as a Unix time: what converts this crate's epoch to the zone table's.
pub(crate) const PG_EPOCH_UNIX_SECONDS: i64 = UNIX_TO_PG_EPOCH_DAYS * SECONDS_PER_DAY;

/// What, if anything, follows the time.
#[derive(Clone, Copy)]
enum Zoning {
    /// No offset at all: a `timestamp`, which does no conversion and shows no zone.
    None,
    /// `+00`, the only offset a node with no zone table could print.
    Utc,
    /// Seconds east of UTC, printed as `±HH`, `±HH:MM` or `±HH:MM:SS`.
    Offset(i32),
}

/// What PostgreSQL's `timestamp_out` writes: the same instant with **no offset**.
///
/// That absence is the whole visible difference between the two types, and it is not cosmetic — a
/// `timestamp` does no conversion at all, so the value that went in is the value that comes out
/// whatever the session's `TimeZone` is, where a `timestamptz` is an instant rendered in a zone.
/// This node only ever means UTC (`src/parameter.rs`), so the two agree here on every value and
/// disagree on one string.
pub(super) fn to_text_without_zone(micros: i64) -> String {
    with_zone(micros, Zoning::None)
}

fn with_zone(micros: i64, zone: Zoning) -> String {
    match micros {
        POS_INFINITY => return "infinity".to_owned(),
        NEG_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }

    let days = micros.div_euclid(MICROS_PER_DAY);
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days + UNIX_TO_PG_EPOCH_DAYS);
    let (hour, minute, second, fraction) = (
        time / (3600 * MICROS_PER_SECOND),
        (time / (60 * MICROS_PER_SECOND)) % 60,
        (time / MICROS_PER_SECOND) % 60,
        time % MICROS_PER_SECOND,
    );

    // Year 1 BC is astronomical year 0, and the era goes after the offset, not after the year.
    let (printed_year, era) = if year <= 0 {
        (1 - year, " BC")
    } else {
        (year, "")
    };
    let mut out =
        format!("{printed_year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if fraction != 0 {
        out.push('.');
        out.push_str(format!("{fraction:06}").trim_end_matches('0'));
    }
    match zone {
        Zoning::None => {}
        Zoning::Utc => out.push_str("+00"),
        Zoning::Offset(seconds) => out.push_str(&offset_text(seconds)),
    }
    out.push_str(era);
    out
}

/// `-05`, `+05:45`, `-04:56:02` — the shortest form that says the offset, sign always written.
fn offset_text(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let magnitude = seconds.unsigned_abs();
    let (hours, minutes, seconds) = (magnitude / 3600, (magnitude / 60) % 60, magnitude % 60);
    if seconds != 0 {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    } else if minutes != 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}")
    }
}

/// Reads the ISO subset for `timestamp` **without** time zone.
///
/// The same lexer, and two differences from the zoned one.
///
/// **A trailing offset is read and discarded**, which is what PostgreSQL's `timestamp_in` does:
/// `'2011-01-02 12:30:00+13'::timestamp` is `2011-01-02 12:30:00`, and so is the same text with
/// `-05`. This function used to *apply* the displacement, on the reasoning that "this node's only
/// zone is UTC, so the two functions differ in exactly one thing". That was true and invisible
/// while every offset printed was `+00`; the moment a session could be in Auckland,
/// `at::timestamp` answered the UTC clock where a real server answers the local one. A cast is a
/// text round trip, so the input function's rule about the offset decides the cast's answer.
///
/// And **the name in the error**: a real server says
/// `invalid input syntax for type timestamp: "not a date"`, naming the short form, where the zoned
/// one names `timestamp with time zone`. Two messages about two types, and neither is the
/// other's.
pub(super) fn from_text_without_zone(text: &str) -> Result<i64> {
    read(text, Zoned::Ignored).map_err(|error| match error {
        SqlError::InvalidDatetimeFormat { value, .. } => SqlError::InvalidDatetimeFormat {
            // `timestamp`, not `timestamp without time zone`: the input function's own name and
            // not the type's long one.
            ty: "timestamp",
            value,
        },
        other => other,
    })
}

/// Reads the ISO subset, or says which construct it did not read.
pub(super) fn from_text(text: &str) -> Result<i64> {
    read(text, Zoned::Applied)
}

fn read(text: &str, zoned: Zoned) -> Result<i64> {
    let body = super::datetime_body(text);
    let lower = body.to_ascii_lowercase();
    match lower.as_str() {
        "infinity" | "+infinity" => return Ok(POS_INFINITY),
        "-infinity" => return Ok(NEG_INFINITY),
        // PostgreSQL's special inputs. Each is a real instant there, so refusing them as malformed
        // would be a lie; `now` in particular has to come from the transaction's timestamp rather
        // than a wall clock (`CLAUDE.md` invariant 6) and so is not this module's to answer.
        "now" | "today" | "tomorrow" | "yesterday" | "epoch" | "allballs" => {
            return Err(unsupported(
                "one of PostgreSQL's special datetime inputs",
                body,
            ));
        }
        _ => {}
    }
    // A timestamp needs a numeric field somewhere. Without one, PostgreSQL fails too, and with the
    // same code -- which is what lets the empty string and `abc` be answered exactly.
    if !body.bytes().any(|byte| byte.is_ascii_digit()) {
        return Err(invalid_format(text));
    }

    match parse_iso_in(body, zoned) {
        Ok(micros) if (MIN_MICROS..=MAX_MICROS).contains(&micros) => Ok(micros),
        // **`timestamp`, not `timestamp without time zone`** — and the same word for a
        // `timestamptz`, whose *syntax* error names the full type. Measured: PostgreSQL words
        // this one condition with the short name for both zone variants.
        Ok(_) | Err(Reject::OutOfRange) => Err(SqlError::DatetimeOutOfRange {
            ty: "timestamp",
            value: text.to_owned(),
        }),
        Err(Reject::FieldOutOfRange { datestyle_hint }) => Err(SqlError::DatetimeFieldOutOfRange {
            value: text.to_owned(),
            datestyle_hint,
        }),
        Err(Reject::ZoneOutOfRange) => {
            Err(SqlError::TimeZoneDisplacementOutOfRange(text.to_owned()))
        }
        Err(Reject::Invalid) => Err(invalid_format(text)),
        Err(Reject::Unsupported(construct)) => Err(unsupported(construct, body)),
    }
}

/// Contract C2 for a value: name what was not read, the way PostgreSQL names a construct.
fn unsupported(construct: &str, body: &str) -> SqlError {
    SqlError::unsupported(format!("{construct} in the timestamp input \"{body}\""))
}

fn invalid_format(text: &str) -> SqlError {
    SqlError::InvalidDatetimeFormat {
        ty: ColumnType::TimestampTz.name(),
        value: text.to_owned(),
    }
}

/// Why an input was not turned into an instant.
///
/// The distinction that matters is the first two. [`Reject::Invalid`] means the shape is broken in
/// a way PostgreSQL refuses too — an hour with no minutes, say — and is reported as PostgreSQL
/// reports it. [`Reject::Unsupported`] means the parse stopped somewhere PostgreSQL would have
/// carried on, and is the only one that becomes contract C2's `0A000`; it carries the name of what
/// was found there, because "not supported" without a construct tells a user nothing.
enum Reject {
    Invalid,
    Unsupported(&'static str),
    /// A field outside its own range, with PostgreSQL's `DateStyle` hint rule
    /// ([`crate::error::SqlError::DatetimeFieldOutOfRange`]).
    FieldOutOfRange {
        datestyle_hint: bool,
    },
    ZoneOutOfRange,
    OutOfRange,
}

/// Whether a trailing offset in the text moves the instant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Zoned {
    /// `timestamptz`: `2011-01-02 12:30:00+13` is 23:30 UTC on the 1st.
    Applied,
    /// `timestamp`: the offset is **read and discarded**, which is what PostgreSQL's
    /// `timestamp_in` does — `'2011-01-02 12:30:00+13'::timestamp` is `2011-01-02 12:30:00` and so
    /// is the same text with `-05`. Measured on 19beta1.
    Ignored,
}

fn parse_iso_in(body: &str, zoned: Zoned) -> std::result::Result<i64, Reject> {
    // The era is a suffix, and BC years count backwards through astronomical year 0.
    let (body, bc) = match () {
        () if body.len() > 3 && body[body.len() - 3..].eq_ignore_ascii_case(" bc") => {
            (body[..body.len() - 3].trim_end(), true)
        }
        () if body.len() > 3 && body[body.len() - 3..].eq_ignore_ascii_case(" ad") => {
            (body[..body.len() - 3].trim_end(), false)
        }
        () => (body, false),
    };

    let mut scan = Scan::new(body);
    let (year, month, day) = scan.date()?;
    let year = if bc { 1 - year } else { year };
    let (time_micros, zone_seconds) = scan.time_and_zone()?;
    if !scan.done() {
        return Err(Reject::Invalid);
    }

    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        // The hint only where the month could have been a day, which is the rule a real server
        // follows for both datetime types.
        return Err(Reject::FieldOutOfRange {
            datestyle_hint: month > 12 && month <= 31,
        });
    }
    days_from_pg_epoch_checked(year, month, day)
        .and_then(|days| days.checked_mul(MICROS_PER_DAY))
        .and_then(|micros| micros.checked_add(time_micros))
        .and_then(|micros| match zoned {
            Zoned::Applied => micros.checked_sub(zone_seconds.checked_mul(MICROS_PER_SECOND)?),
            Zoned::Ignored => Some(micros),
        })
        .ok_or(Reject::OutOfRange)
}

/// A cursor over the ASCII of a timestamp literal.
struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Self {
        Scan {
            bytes: text.as_bytes(),
            at: 0,
        }
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            return true;
        }
        false
    }

    /// Consecutive digits, with their count, or `None` when there are none.
    fn digits(&mut self) -> Option<(i64, usize)> {
        let start = self.at;
        let mut value: i64 = 0;
        while let Some(byte) = self.peek() {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
            self.at += 1;
        }
        (self.at > start).then_some((value, self.at - start))
    }

    /// `YYYY-MM-DD` with one- or two-digit month and day, or a compact `YYYYMMDD`.
    fn date(&mut self) -> std::result::Result<(i64, i64, i64), Reject> {
        let (first, width) = self.digits().ok_or(Reject::Invalid)?;
        if !self.eat(b'-') {
            if width == 8 {
                return Ok((first / 10_000, (first / 100) % 100, first % 100));
            }
            // `01/02/2024` is a date to PostgreSQL, and which date depends on `DateStyle`.
            return Err(match self.peek() {
                Some(b'/' | b'.') => Reject::Unsupported("a non-ISO date"),
                _ => Reject::Invalid,
            });
        }
        let (month, _) = self.digits().ok_or(Reject::Invalid)?;
        if !self.eat(b'-') {
            return Err(Reject::Invalid);
        }
        let (day, _) = self.digits().ok_or(Reject::Invalid)?;
        Ok((first, month, day))
    }

    /// The optional time and the optional zone, as microseconds into the day and the zone's
    /// displacement in seconds east of UTC.
    fn time_and_zone(&mut self) -> std::result::Result<(i64, i64), Reject> {
        if self.done() {
            return Ok((0, 0));
        }
        match self.peek() {
            Some(b' ' | b'T' | b't') => self.at += 1,
            _ => return Err(Reject::Invalid),
        }

        let (hour, _) = self.digits().ok_or(Reject::Invalid)?;
        // An hour on its own is not a time: PostgreSQL refuses `2024-01-01 10` too.
        if !self.eat(b':') {
            return Err(Reject::Invalid);
        }
        let (minute, _) = self.digits().ok_or(Reject::Invalid)?;
        let mut second = 0;
        let mut fraction = 0;
        if self.eat(b':') {
            second = self.digits().ok_or(Reject::Invalid)?.0;
            if self.eat(b'.') {
                fraction = self.fraction()?;
            }
        }

        // `24:00:00` and `23:59:60` are both ways to write the next midnight, and both are legal;
        // one microsecond past it is not. The check is on the whole time rather than on the hour,
        // which is what lets those two through and keeps `24:00:01` out.
        if !(0..=24).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
            // No hint: a clock field is never a date field the setting could have reordered.
            return Err(Reject::FieldOutOfRange {
                datestyle_hint: false,
            });
        }
        let micros = ((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + fraction;
        if micros > MICROS_PER_DAY {
            return Err(Reject::FieldOutOfRange {
                datestyle_hint: false,
            });
        }
        Ok((micros, self.zone()?))
    }

    /// Up to six digits kept, the seventh rounding the sixth. A carry out of the fraction is left
    /// in the returned microseconds and becomes a whole second when the time is summed.
    fn fraction(&mut self) -> std::result::Result<i64, Reject> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = &self.bytes[start..self.at];
        if digits.is_empty() {
            return Err(Reject::Invalid);
        }
        let mut micros: i64 = 0;
        for index in 0..6 {
            micros = micros * 10 + i64::from(digits.get(index).map_or(0, |digit| digit - b'0'));
        }
        // The seventh digit and everything after it decide, and the rule is **not** "five rounds
        // up": PostgreSQL reads the fraction as a double and applies `rint`, which rounds a tie to
        // **even**. Measured on 19beta1, four halves rounded two different ways —
        //
        //     .1234565 -> .123456      .1234575 -> .123458
        //     .1234555 -> .123456      .1234545 -> .123454
        //
        // — each landing on the even neighbour. And a tie is only a tie when nothing follows it:
        // `.12345650001` is above the half and rounds up whatever the parity. `timestamptz` goes
        // through this same function and had the same bug, which is why the fix is here rather
        // than in the type that found it.
        let rest = digits.get(6..).unwrap_or_default();
        let round_up = match rest {
            [] | [b'0'..=b'4', ..] => false,
            // Exactly half: the digit is a 5 and nothing but zeros follows it.
            [b'5', tail @ ..] if tail.iter().all(|&digit| digit == b'0') => micros % 2 == 1,
            _ => true,
        };
        if round_up {
            micros += 1;
        }
        Ok(micros)
    }

    /// `Z`, `±HH`, `±HHMM` or `±HH:MM`. Nothing at all means UTC, because that is this node's only
    /// time zone.
    fn zone(&mut self) -> std::result::Result<i64, Reject> {
        // A zone may be held off by spaces -- `10:00:00 +05` and `10:00:00 UTC` both.
        while self.eat(b' ') {}
        if self.done() {
            return Ok(0);
        }
        if self.eat(b'Z') || self.eat(b'z') {
            return Ok(0);
        }
        let sign = match self.peek() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            // `UTC`, `America/New_York`: a zone PostgreSQL looks up in a database we do not carry.
            Some(byte) if byte.is_ascii_alphabetic() => {
                return Err(Reject::Unsupported("a named time zone"));
            }
            _ => return Err(Reject::Invalid),
        };
        self.at += 1;
        let (value, width) = self.digits().ok_or(Reject::Invalid)?;
        let (hours, mut minutes) = match width {
            1 | 2 => (value, 0),
            4 => (value / 100, value % 100),
            _ => return Err(Reject::Invalid),
        };
        if self.eat(b':') {
            minutes = self.digits().ok_or(Reject::Invalid)?.0;
        }
        if minutes > 59 {
            return Err(Reject::ZoneOutOfRange);
        }
        let seconds = hours * 3600 + minutes * 60;
        if seconds >= MAX_ZONE_SECONDS {
            return Err(Reject::ZoneOutOfRange);
        }
        Ok(sign * seconds)
    }
}

/// Days from 2000-01-01 to `year-month-day` in the proleptic Gregorian calendar, where year 0 is
/// 1 BC. Howard Hinnant's `days_from_civil`, shifted to our epoch; it is exact for every year an
/// `i64` of microseconds can reach and has no branches on leap rules.
pub(super) const fn days_from_pg_epoch(year: i64, month: i64, day: i64) -> i64 {
    let year = year - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468 - UNIX_TO_PG_EPOCH_DAYS
}

/// [`days_from_pg_epoch`] for a year that may not fit, which is how a five-digit year past the
/// range is caught before it overflows.
pub(super) fn days_from_pg_epoch_checked(year: i64, month: i64, day: i64) -> Option<i64> {
    // Wide enough for both types that use it: a `timestamp` stops at 294276 AD and a `date` at
    // **5874897 AD**, which is what this bound has to clear. Everything past it is refused here so
    // the multiplication below cannot overflow on the way to being found out of range.
    (-6_000_000..=6_000_000)
        .contains(&year)
        .then(|| days_from_pg_epoch(year, month, day))
}

/// The inverse: the civil date `days` after 1970-01-01.
pub(super) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

pub(super) fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
