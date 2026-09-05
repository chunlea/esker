//! The POSIX `TZ` string in a `TZif` footer: the rule for instants past the last transition.
//!
//! `America/New_York` is `EST5EDT,M3.2.0,M11.1.0` — Eastern Standard, five hours *behind* UTC,
//! Eastern Daylight, starting the second Sunday of March and ending the first Sunday of November.
//! It is the half of the format that is a parser rather than a lookup, and it is not optional:
//! the transition tables stop around 2037, and `'2099-07-01 00:00:00+00'` is `2099-06-30
//! 20:00:00-04` on a real server (`tests/captures/pg19_time_zone.txt`).
//!
//! # The sign is inverted, and that is the trap
//!
//! POSIX writes **the offset to add to local time to get UTC**, so `EST5` is five hours *west* and
//! this module stores `-5 * 3600`. A real server shows the same inversion on the way out:
//! `SET TIME ZONE -5` reads back as `<-05>+05`.
//!
//! # The grammar
//!
//! ```text
//! std offset [ dst [offset] [ , start[/time] , end[/time] ] ]
//! std, dst    three or more letters, or <...> around letters, digits, + and -
//! offset      [+|-]hh[:mm[:ss]]              dst's defaults to std's plus an hour
//! start, end  Jn      day n of the year, 1-365, never counting 29 February
//!             n       day n of the year, 0-365, counting it
//!             Mm.w.d  the w-th weekday d of month m; w = 5 means the last
//! time        [+|-]hh[:mm[:ss]], default 02:00:00, in local time before the change
//! ```

use super::super::timestamp::{UNIX_TO_PG_EPOCH_DAYS, civil_from_days, days_from_pg_epoch};

const SECONDS_PER_DAY: i64 = 86_400;

/// A parsed footer.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Posix {
    standard: Named,
    daylight: Option<Daylight>,
}

#[derive(Debug, PartialEq, Eq)]
struct Named {
    abbrev: String,
    /// Seconds **east** of UTC, having undone POSIX's inversion.
    offset: i32,
}

#[derive(Debug, PartialEq, Eq)]
struct Daylight {
    name: Named,
    start: When,
    end: When,
}

#[derive(Debug, PartialEq, Eq)]
struct When {
    date: Date,
    /// Seconds after local midnight; may be negative or past a day, which POSIX allows.
    time: i64,
}

#[derive(Debug, PartialEq, Eq)]
enum Date {
    /// `Jn`: 1-365, 29 February never counted.
    Julian(u16),
    /// `n`: 0-365, 29 February counted.
    Ordinal(u16),
    /// `Mm.w.d`.
    Weekday { month: u8, week: u8, day: u8 },
}

/// What the rule says about one instant.
pub(super) struct Answer<'a> {
    pub(super) offset: i32,
    pub(super) is_dst: bool,
    pub(super) abbrev: &'a str,
}

impl Posix {
    /// The offset, whether it is daylight saving, and what it is called, at one UTC instant.
    pub(super) fn at(&self, utc: i64) -> Answer<'_> {
        let Some(daylight) = &self.daylight else {
            return Answer {
                offset: self.standard.offset,
                is_dst: false,
                abbrev: &self.standard.abbrev,
            };
        };
        // **The year is the local one**, because the rule's dates are local dates. Which offset is
        // used to find it does not matter: no rule's transition is within a day of new year.
        let (year, ..) =
            civil_from_days((utc + i64::from(self.standard.offset)).div_euclid(SECONDS_PER_DAY));
        // Each edge is expressed in the time in force **before** it: the start in standard time,
        // the end in daylight time. Getting that backwards moves both edges by an hour.
        let start = daylight.start.instant(year) - i64::from(self.standard.offset);
        let end = daylight.end.instant(year) - i64::from(daylight.name.offset);
        // A southern-hemisphere zone starts its daylight saving *after* it ends it in the same
        // year, so the interval wraps the new year and the test is the other way round.
        let is_dst = if start <= end {
            utc >= start && utc < end
        } else {
            utc >= start || utc < end
        };
        if is_dst {
            Answer {
                offset: daylight.name.offset,
                is_dst: true,
                abbrev: &daylight.name.abbrev,
            }
        } else {
            Answer {
                offset: self.standard.offset,
                is_dst: false,
                abbrev: &self.standard.abbrev,
            }
        }
    }
}

impl When {
    /// The local instant this rule names in one year, as seconds since the Unix epoch.
    fn instant(&self, year: i64) -> i64 {
        let day = match self.date {
            Date::Julian(n) => {
                let ordinal = i64::from(n);
                // 29 February is not counted, so from the 60th day on, a leap year shifts by one.
                let leap = is_leap(year) && ordinal >= 60;
                first_of_year(year) + ordinal - 1 + i64::from(leap)
            }
            Date::Ordinal(n) => first_of_year(year) + i64::from(n),
            Date::Weekday { month, week, day } => weekday_of(year, month, week, day),
        };
        day * SECONDS_PER_DAY + self.time
    }
}

/// Days since the Unix epoch for 1 January of a year.
fn first_of_year(year: i64) -> i64 {
    days_from_pg_epoch(year, 1, 1) + UNIX_TO_PG_EPOCH_DAYS
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// The `week`-th `day` of `month`, where week 5 means the last one — days since the Unix epoch.
fn weekday_of(year: i64, month: u8, week: u8, day: u8) -> i64 {
    let first = days_from_pg_epoch(year, i64::from(month), 1) + UNIX_TO_PG_EPOCH_DAYS;
    // 1970-01-01 was a Thursday, and POSIX counts Sunday as 0.
    let first_weekday = (first + 4).rem_euclid(7);
    let ahead = (i64::from(day) - first_weekday).rem_euclid(7);
    let candidate = first + ahead + 7 * (i64::from(week) - 1);
    let last = first + super::super::timestamp::days_in_month(year, i64::from(month)) - 1;
    // Week 5 means "the last one", and a month with four of that weekday makes week 5 the fourth.
    if candidate > last {
        candidate - 7
    } else {
        candidate
    }
}

/// Reads a footer, or says which part of it did not read.
pub(super) fn parse(text: &str) -> Result<Posix, &'static str> {
    let mut at = Scan {
        bytes: text.as_bytes(),
        at: 0,
    };
    let standard = Named {
        abbrev: at.abbrev()?,
        offset: at
            .offset()?
            .ok_or("a POSIX TZ string with no standard offset")?,
    };
    if at.done() {
        return Ok(Posix {
            standard,
            daylight: None,
        });
    }
    let abbrev = at.abbrev()?;
    // **A daylight name with no offset means one hour ahead**, which is the common spelling:
    // `EST5EDT` and `EST5EDT4` are the same rule.
    let offset = at.offset()?.unwrap_or(standard.offset + 3600);
    let name = Named { abbrev, offset };
    // A daylight name with no rules is legal POSIX and means "the system decides", which nothing
    // here can do. Every zone in the database that names one gives its rules.
    if !at.eat(b',') {
        return Err("a POSIX TZ string with a daylight name and no rules");
    }
    let start = at.when()?;
    if !at.eat(b',') {
        return Err("a POSIX TZ string with one rule instead of two");
    }
    let end = at.when()?;
    if !at.done() {
        return Err("trailing bytes after a POSIX TZ string");
    }
    Ok(Posix {
        standard,
        daylight: Some(Daylight { name, start, end }),
    })
}

/// A field this scanner has already range-checked, as the byte it is stored in.
///
/// The check above is what makes the conversion total; this says so to the compiler rather than
/// asserting it, so that loosening the check cannot turn into a silent truncation.
fn narrow(value: i64) -> Result<u8, &'static str> {
    u8::try_from(value).map_err(|_| "a POSIX TZ field out of range")
}

struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
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

    /// `<+0545>` or `CET`.
    fn abbrev(&mut self) -> Result<String, &'static str> {
        let start = self.at;
        if self.eat(b'<') {
            while self.peek().is_some_and(|byte| byte != b'>') {
                self.at += 1;
            }
            if !self.eat(b'>') {
                return Err("a POSIX TZ abbreviation with no closing angle bracket");
            }
            let inner = &self.bytes[start + 1..self.at - 1];
            if inner.is_empty() {
                return Err("an empty POSIX TZ abbreviation");
            }
            return std::str::from_utf8(inner)
                .map(str::to_owned)
                .map_err(|_| "a POSIX TZ abbreviation that is not UTF-8");
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.at += 1;
        }
        if self.at - start < 3 {
            return Err("a POSIX TZ abbreviation shorter than three letters");
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .map(str::to_owned)
            .map_err(|_| "a POSIX TZ abbreviation that is not UTF-8")
    }

    /// `[+|-]hh[:mm[:ss]]`, **inverted** into seconds east of UTC. `None` when there is none.
    fn offset(&mut self) -> Result<Option<i32>, &'static str> {
        if !self
            .peek()
            .is_some_and(|byte| byte == b'+' || byte == b'-' || byte.is_ascii_digit())
        {
            return Ok(None);
        }
        let seconds = self.clock()?;
        let seconds = i32::try_from(seconds).map_err(|_| "a POSIX TZ offset out of range")?;
        Ok(Some(-seconds))
    }

    /// `Jn`, `n`, or `Mm.w.d`, with an optional `/time`.
    fn when(&mut self) -> Result<When, &'static str> {
        let date = if self.eat(b'J') {
            let day = self.number()?;
            if !(1..=365).contains(&day) {
                return Err("a POSIX TZ Julian day outside 1-365");
            }
            Date::Julian(u16::try_from(day).map_err(|_| "a POSIX TZ Julian day out of range")?)
        } else if self.eat(b'M') {
            let month = self.number()?;
            if !self.eat(b'.') {
                return Err("a POSIX TZ month rule with no week");
            }
            let week = self.number()?;
            if !self.eat(b'.') {
                return Err("a POSIX TZ month rule with no weekday");
            }
            let day = self.number()?;
            if !(1..=12).contains(&month) || !(1..=5).contains(&week) || !(0..=6).contains(&day) {
                return Err("a POSIX TZ month rule outside its ranges");
            }
            Date::Weekday {
                month: narrow(month)?,
                week: narrow(week)?,
                day: narrow(day)?,
            }
        } else {
            let day = self.number()?;
            if !(0..=365).contains(&day) {
                return Err("a POSIX TZ ordinal day outside 0-365");
            }
            Date::Ordinal(u16::try_from(day).map_err(|_| "a POSIX TZ ordinal day out of range")?)
        };
        // **Two in the morning is the default**, and it is the local time before the change.
        let time = if self.eat(b'/') { self.clock()? } else { 7200 };
        Ok(When { date, time })
    }

    /// `[+|-]hh[:mm[:ss]]` as a signed number of seconds, sign as written.
    fn clock(&mut self) -> Result<i64, &'static str> {
        let negative = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        let hours = self.number()?;
        let minutes = if self.eat(b':') { self.number()? } else { 0 };
        let seconds = if self.eat(b':') { self.number()? } else { 0 };
        if !(0..=167).contains(&hours)
            || !(0..=59).contains(&minutes)
            || !(0..=59).contains(&seconds)
        {
            return Err("a POSIX TZ time outside its ranges");
        }
        let total = hours * 3600 + minutes * 60 + seconds;
        Ok(if negative { -total } else { total })
    }

    fn number(&mut self) -> Result<i64, &'static str> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        if self.at == start {
            return Err("a POSIX TZ number with no digits");
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or("a POSIX TZ number that does not fit")
    }
}
