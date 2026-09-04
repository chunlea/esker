//! `time` without time zone: a time of day, and the two rules that make it not a clamped integer.
//!
//! Eight bytes, a microsecond count since midnight — PostgreSQL's own representation, so the
//! number a row holds is the number a real server holds.
//!
//! # The range is closed at **both** ends
//!
//! `00:00:00` through `24:00:00` inclusive. A twenty-fifth hour is storable, `86_400_000_000` is a
//! legal value, and `24:00:01` is the first thing past the end. There are three ways to arrive at
//! the top: writing it, writing `23:59:60` (a leap second, which normalises), and writing
//! `23:59:59.9999999`, whose seventh digit **rounds up into it**. Nothing here may assume a time
//! is strictly less than a day.
//!
//! # Two spellings of wrong get two different SQLSTATEs
//!
//! `'25:00:00'` and `'12:60:00'` are `22008 date/time field value out of range`: they parsed, and
//! a field was too large. `'-01:00:00'` and `'not a time'` are `22007 invalid input syntax for
//! type time`: they did not parse at all. **A negative time is a syntax error, not a range
//! error** — the one an implementation that clamps first and validates second gets backwards.
//! Both are measured, in `tests/corpus/pg19_time.txt`.

use crate::error::{Result, SqlError};

/// Microseconds in one second, minute, hour and day.
const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;

/// `24:00:00`, the largest value this type holds — and a value, not a bound to reject at.
pub const MAX_MICROS: i64 = 24 * MICROS_PER_HOUR;

/// `00:00:00`.
pub const MIN_MICROS: i64 = 0;

/// What PostgreSQL's `time_out` writes: `HH:MM:SS`, and a fraction only when there is one.
///
/// The fraction is printed to six digits with trailing zeros stripped, which is why `12:34:56.1`
/// and `12:34:56.100000` are one string — the same rule `timestamp` prints by.
#[must_use]
pub fn to_text(micros: i64) -> String {
    let hour = micros / MICROS_PER_HOUR;
    let minute = (micros % MICROS_PER_HOUR) / MICROS_PER_MINUTE;
    let second = (micros % MICROS_PER_MINUTE) / MICROS_PER_SECOND;
    let fraction = micros % MICROS_PER_SECOND;
    let mut out = format!("{hour:02}:{minute:02}:{second:02}");
    if fraction != 0 {
        out.push('.');
        out.push_str(format!("{fraction:06}").trim_end_matches('0'));
    }
    out
}

/// PostgreSQL's `time_in`, for the inputs this node reads.
///
/// The accepted shapes, all measured: `12:34:56`, `12:34` (seconds default to zero), `12:34:56.123`
/// with up to six kept digits and a seventh that rounds, `123456` — six bare digits, the same
/// rule `date` has for `20200101` — a `AM`/`PM` suffix, and the word `allballs`, which is
/// midnight and is a `time` word only: the same literal against `date` is `22007`.
pub fn from_text(text: &str) -> Result<i64> {
    let body = super::datetime_body(text);
    if body.eq_ignore_ascii_case("allballs") {
        return Ok(0);
    }
    let body = strip_date_and_zone(body);
    let (body, meridiem) = split_meridiem(body);
    let (hour, minute, second, fraction) = fields(body).ok_or_else(|| invalid(text))?;
    let hour = apply_meridiem(hour, meridiem).ok_or_else(|| invalid(text))?;
    // The field bounds PostgreSQL checks, and it checks them before it adds anything up: an hour
    // of 24 is legal on its own, a minute of 60 is not, and a second of 60 is a leap second that
    // normalises upward rather than an error.
    if hour > 24 || minute > 59 || second > 60 {
        return Err(out_of_range(text));
    }
    let micros =
        hour * MICROS_PER_HOUR + minute * MICROS_PER_MINUTE + second * MICROS_PER_SECOND + fraction;
    // **The check is on the total, not on the hour**, which is what makes `24:00:00` a value and
    // `24:00:01` an error, and what lets `23:59:60` and a rounded `23:59:59.9999999` both land
    // exactly on the top rather than past it.
    if !(MIN_MICROS..=MAX_MICROS).contains(&micros) {
        return Err(out_of_range(text));
    }
    Ok(micros)
}

/// `micros` rounded to `precision` fractional digits, as a `time(p)` column does.
///
/// **Half away from zero**, which is `numeric`'s rule and not the round-to-even the unqualified
/// type truncates a seventh digit with. Two rounding rules in one type, exactly as `timestamp`
/// has them, and both are measured: `.5` at `time(0)` is `12:34:57` and `.1234565` unqualified is
/// `.123456`.
///
/// Rounding can carry past the end of the day — `23:59:59.9999` at `time(3)` is `24:00:00` — which
/// is a value here, so nothing needs clamping.
#[must_use]
pub fn round_to_precision(micros: i64, precision: u32) -> i64 {
    if precision >= 6 {
        return micros;
    }
    let step = 10i64.pow(6 - precision);
    let (whole, part) = (micros / step, micros % step);
    // Away from zero, and a time is never negative, so "away from zero" is "up".
    if part * 2 >= step {
        (whole + 1) * step
    } else {
        whole * step
    }
}

/// An **ISO date** ahead of the time, and a zone offset behind it, both discarded.
///
/// What makes `'2020-01-01 12:34:56'::time` a value and `'2020-01-01 12:34:56'::timestamp::time`
/// the clock of that instant. The accepted shape is narrow and was measured one spelling at a
/// time: the date must be ISO and the separator must be a **space** — `'2020-01-01T12:34:56'` is
/// `22007` on a real server, where the same string is a perfectly good `timestamp` — a date with
/// no time at all is `22007`, and `'Jan 2 2020 12:34:56'` is too. So this is not "parse a
/// timestamp and keep the clock"; it is one specific prefix.
fn strip_date_and_zone(body: &str) -> &str {
    // The zone goes first: `12:34:56+02` is `12:34:56`, and a `-` offset must not be mistaken for
    // the sign that makes a bare `-01:00:00` a syntax error, so it is only stripped after a time.
    let body = match body.rfind(['+', '-']) {
        Some(at) if at > 0 && body[..at].contains(':') => &body[..at],
        _ => body,
    };
    let Some((head, tail)) = body.split_once(' ') else {
        return body;
    };
    // An ISO date is digits and dashes with two dashes in it. Anything else — a month name, a
    // meridiem — is not a date and the split belongs to whoever comes next.
    let is_iso_date = head.matches('-').count() == 2
        && head
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-');
    if is_iso_date && tail.contains(':') {
        tail.trim_start()
    } else {
        body
    }
}

/// A trailing `AM` or `PM`, split off the body.
fn split_meridiem(body: &str) -> (&str, Option<bool>) {
    for (suffix, is_pm) in [("am", false), ("pm", true)] {
        if body.len() > 2 && body[body.len() - 2..].eq_ignore_ascii_case(suffix) {
            let head = body[..body.len() - 2].trim_end();
            if !head.is_empty() {
                return (head, Some(is_pm));
            }
        }
    }
    (body, None)
}

/// The twelve-hour clock's two wrap-arounds: `12 AM` is hour 0 and `12 PM` is hour 12.
fn apply_meridiem(hour: i64, meridiem: Option<bool>) -> Option<i64> {
    let Some(is_pm) = meridiem else {
        return Some(hour);
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    Some(match (is_pm, hour) {
        (false, 12) => 0,
        (false, _) => hour,
        (true, 12) => 12,
        (true, _) => hour + 12,
    })
}

/// The four fields of a time with no meridiem: hour, minute, second and microseconds.
fn fields(body: &str) -> Option<(i64, i64, i64, i64)> {
    // `123456`: six bare digits and nothing else, unambiguous and with its own rule.
    if body.len() == 6 && body.bytes().all(|b| b.is_ascii_digit()) {
        return Some((
            body[..2].parse().ok()?,
            body[2..4].parse().ok()?,
            body[4..].parse().ok()?,
            0,
        ));
    }
    let (clock, fraction) = match body.split_once('.') {
        Some((clock, digits)) => (clock, fraction_micros(digits)?),
        None => (body, 0),
    };
    let mut parts = clock.split(':');
    let hour = digits(parts.next()?)?;
    let minute = digits(parts.next()?)?;
    // Seconds are optional: `12:34` is a time and its seconds are zero.
    let second = match parts.next() {
        Some(part) => digits(part)?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((hour, minute, second, fraction))
}

/// One clock field: digits only, so a sign or a space is not one. **This is where `-01:00:00`
/// becomes `22007` rather than `22008`** — it never parses, so no field is ever out of range.
fn digits(part: &str) -> Option<i64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// A fractional-second string as microseconds, with a seventh digit **rounding**.
///
/// Round half to even, which is what an unqualified `time` does and is not what `time(p)` does:
/// `.1234565` is `.123456` and not `.123457`.
fn fraction_micros(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut micros: i64 = 0;
    for index in 0..6 {
        let digit = text
            .as_bytes()
            .get(index)
            .map_or(0, |b| i64::from(b - b'0'));
        micros = micros * 10 + digit;
    }
    let Some(&next) = text.as_bytes().get(6) else {
        return Some(micros);
    };
    let rest_is_zero = text.as_bytes()[7..].iter().all(|b| *b == b'0');
    let round_up = match next {
        b'0'..=b'4' => false,
        b'5' if rest_is_zero => micros % 2 == 1,
        _ => true,
    };
    Some(micros + i64::from(round_up))
}

fn invalid(text: &str) -> SqlError {
    SqlError::InvalidDatetimeFormat {
        // **`time`, not `time without time zone`**: the input function's own name and not the
        // type's long one, the same split `timestamp` makes at the same place. Measured — PG19
        // answers `invalid input syntax for type time: "-01:00:00"`.
        ty: "time",
        value: text.to_owned(),
    }
}

fn out_of_range(text: &str) -> SqlError {
    SqlError::DatetimeFieldOutOfRange {
        value: text.to_owned(),
        // No hint: a clock field is never a date field a `DateStyle` could have reordered.
        datestyle_hint: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{from_text, round_to_precision, to_text};

    /// Every input shape the capture holds, and what a real server answers for it.
    #[test]
    fn the_shapes_postgresql_reads() {
        for (text, expect) in [
            ("12:34:56", "12:34:56"),
            ("12:34", "12:34:00"),
            ("123456", "12:34:56"),
            ("00:00:00", "00:00:00"),
            // The top of the closed range, three ways to reach it.
            ("24:00:00", "24:00:00"),
            ("23:59:60", "24:00:00"),
            ("23:59:59.9999999", "24:00:00"),
            ("allballs", "00:00:00"),
            ("12:34:56 PM", "12:34:56"),
            ("12:34:56 AM", "00:34:56"),
            ("1:00 PM", "13:00:00"),
            ("12:34:56.123456", "12:34:56.123456"),
            // A seventh digit rounds to **even**, which is the unqualified type's rule.
            ("12:34:56.1234565", "12:34:56.123456"),
        ] {
            let micros = from_text(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(to_text(micros), expect, "{text}");
        }
    }

    /// The split nobody guesses: a field too large is `22008`, an unparseable one is `22007`.
    #[test]
    fn two_kinds_of_wrong_get_two_sqlstates() {
        for text in ["24:00:01", "25:00:00", "12:60:00"] {
            let error = from_text(text).unwrap_err();
            assert_eq!(error.sqlstate(), "22008", "{text}");
            assert_eq!(
                error.to_string(),
                format!("date/time field value out of range: \"{text}\"")
            );
        }
        // **A negative time never parses**, so it is a syntax error and not a range one.
        for text in [
            "-01:00:00",
            "not a time",
            "",
            "12:34:56:78",
            "12:",
            "1a:00:00",
        ] {
            let error = from_text(text).unwrap_err();
            assert_eq!(error.sqlstate(), "22007", "{text}");
        }
    }

    /// `time(p)` rounds half **away from zero**, where the bare type rounds to even.
    #[test]
    fn the_typmod_rounds_the_other_way() {
        let at = |text: &str, precision: u32| {
            to_text(round_to_precision(from_text(text).unwrap(), precision))
        };
        assert_eq!(at("12:34:56.5", 0), "12:34:57");
        assert_eq!(at("12:34:56.9999", 3), "12:34:57");
        assert_eq!(at("12:34:56.0005", 3), "12:34:56.001");
        assert_eq!(at("12:34:56", 3), "12:34:56");
        // Rounding can carry into the twenty-fifth hour, which is a value and needs no clamp.
        assert_eq!(at("23:59:59.9999", 3), "24:00:00");
    }

    /// The text output trims a fraction's trailing zeros and prints none when there is none.
    #[test]
    fn the_output_trims_what_postgresql_trims() {
        assert_eq!(to_text(0), "00:00:00");
        assert_eq!(to_text(86_400_000_000), "24:00:00");
        assert_eq!(to_text(45_296_000_000), "12:34:56");
        assert_eq!(to_text(45_296_100_000), "12:34:56.1");
        assert_eq!(to_text(45_296_123_456), "12:34:56.123456");
    }
}
