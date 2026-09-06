//! A named time zone: the offset from UTC at an instant, and what that offset is called.
//!
//! [ADR 0080](../../../../docs/adr/0080-the-time-zone-table-is-data-the-reader-is-ours.md) settles
//! where this comes from. **The table is a dependency and the reader is ours**: `jiff-tzdb`
//! embeds the IANA database as `TZif` bytes and nothing else — one crate, no dependencies of its
//! own, no build script — and this module's `tzif` and `posix` read them.
//!
//! # A zone is not an offset
//!
//! `America/New_York` is `-05:00` in January, `-04:00` in July, and `-04:56:02` before 1883, when
//! the city kept local mean time and the offset was not a whole minute. Past the last transition
//! the file records — around 2037 — the answer comes from a rule written in the footer rather than
//! from a table, so `'2099-07-01 00:00:00+00'` is `2099-06-30 20:00:00-04` on a real server and
//! here. Every one of those is measured in `tests/captures/pg19_time_zone.txt`.
//!
//! # The table is `slim`, and that decides which half of the reader matters
//!
//! `jiff-tzdb` ships files written by `zic -b slim`: the transition list stops at the last time
//! the *rules* changed, and everything after it is the footer's POSIX string. New York's table
//! ends in 2007, so `'2020-07-01'` — which reads like a table lookup — is answered by
//! `EST5EDT,M3.2.0,M11.1.0`. The `posix` reader is therefore the live path for every present-day
//! instant, not a fallback for the far future, and the test named
//! `the_footer_is_what_answers_a_present_day_instant` pins it so that a future release shipping
//! `fat` data cannot move that traffic silently.
//!
//! # Names
//!
//! Lookup is **case-insensitive and answers with the canonical spelling**, which is what a real
//! server does: `SET TIME ZONE 'america/new_york'` is accepted and `SHOW TimeZone` then says
//! `America/New_York`. Measured, both cases and `'utc'` → `UTC`.

use std::collections::HashMap;

mod posix;
mod tzif;

/// What a zone answers about one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offset<'a> {
    /// Seconds **east of UTC**: the number added to an instant to get local time.
    pub seconds: i32,
    /// Whether daylight saving is in force. `pg_timezone_names.is_dst` is this, at `now()`.
    pub is_dst: bool,
    /// `EST`, `EDT`, `+0545`, `LMT`.
    pub abbrev: &'a str,
}

/// One zone from the IANA database, read.
#[derive(Debug)]
pub struct Zone {
    /// The canonical spelling, which is not necessarily the one that was asked for.
    name: &'static str,
    file: tzif::Tzif,
    rule: Option<posix::Posix>,
}

impl Zone {
    /// The zone with this name, or `None` if the database has no such name.
    ///
    /// Case-insensitive, because a real server is: the exact spelling is tried first, since that
    /// is what almost every caller sends, and only a miss walks the names.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        let (canonical, bytes) = jiff_tzdb::get(name).or_else(|| {
            jiff_tzdb::available()
                .find(|candidate| candidate.eq_ignore_ascii_case(name))
                .and_then(jiff_tzdb::get)
        })?;
        Self::read(canonical, bytes).ok()
    }

    /// The zone with this name, parsed once for the life of the process.
    ///
    /// **Kept rather than freed, on purpose.** A session's zone has to reach the renderer, and a
    /// renderer that re-read the file for every row would parse a thousand transitions to print
    /// one timestamp. The set is bounded by the table — 598 names, of which a process typically
    /// touches one — so this is a static table built lazily rather than a leak that grows.
    ///
    /// A `&'static` is also what keeps [`crate::value::Rendering`] `Copy`, and that is what let
    /// the zone reach the cursor without a second argument at seventeen call sites.
    #[must_use]
    pub fn shared(name: &str) -> Option<&'static Zone> {
        static PARSED: std::sync::OnceLock<std::sync::Mutex<HashMap<&'static str, &'static Zone>>> =
            std::sync::OnceLock::new();
        let table = PARSED.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let zone = Zone::by_name(name)?;
        let mut table = table.lock().ok()?;
        // Keyed by the **canonical** name, so that every spelling of one zone shares one parse.
        if let Some(found) = table.get(zone.name) {
            return Some(found);
        }
        let leaked: &'static Zone = Box::leak(Box::new(zone));
        table.insert(leaked.name, leaked);
        Some(leaked)
    }

    /// Every zone name the table carries, in the database's own order.
    pub fn names() -> impl Iterator<Item = &'static str> {
        jiff_tzdb::available()
    }

    /// The IANA release this node's table came from — what `SHOW` can report and what says whether
    /// a zone's rules are current.
    ///
    /// `None` where the table carries no version, which the crate allows and its own data does
    /// not do; a caller that wants a string can say so itself rather than be given one invented
    /// here.
    #[must_use]
    pub fn database_version() -> Option<&'static str> {
        jiff_tzdb::VERSION
    }

    /// The canonical name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The offset at one instant, given as seconds since the Unix epoch.
    ///
    /// Three cases, and the order matters: an instant **before** the first recorded transition
    /// takes the first type that is not daylight saving — which is how `1880-01-01` gets New
    /// York's local mean time rather than its modern standard time — an instant **inside** the
    /// table takes the type the last transition at or before it names, and an instant **past** the
    /// last transition takes the footer's rule when there is one.
    #[must_use]
    pub fn offset_at(&self, utc_seconds: i64) -> Offset<'_> {
        let past_the_table = self
            .file
            .transitions
            .last()
            .is_none_or(|&last| utc_seconds >= last);
        if past_the_table && let Some(rule) = &self.rule {
            let answer = rule.at(utc_seconds);
            return Offset {
                seconds: answer.offset,
                is_dst: answer.is_dst,
                abbrev: answer.abbrev,
            };
        }
        let local = match self
            .file
            .transitions
            .partition_point(|&at| at <= utc_seconds)
        {
            // Before everything the file records.
            0 => self.first_standard(),
            after => {
                let index = usize::from(self.file.indices[after - 1]);
                self.file.types.get(index).unwrap_or_else(|| {
                    // The reader checks every index against `typecnt` as it reads, so this is
                    // unreachable rather than defended against; falling back keeps it from being
                    // a panic if that check is ever loosened.
                    self.first_standard()
                })
            }
        };
        Offset {
            seconds: local.offset,
            is_dst: local.is_dst,
            abbrev: &local.abbrev,
        }
    }

    /// What RFC 8536 says to use before the first transition: the first type that is not daylight
    /// saving, or the first type of all if every one of them is.
    fn first_standard(&self) -> &tzif::LocalTime {
        self.file
            .types
            .iter()
            .find(|local| !local.is_dst)
            .unwrap_or(&self.file.types[0])
    }

    fn read(name: &'static str, bytes: &'static [u8]) -> Result<Self, &'static str> {
        let file = tzif::read(bytes)?;
        if file.types.is_empty() {
            return Err("a TZif file with no local time types");
        }
        let rule = file.footer.as_deref().map(posix::parse).transpose()?;
        Ok(Zone { name, file, rule })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instants `tests/captures/pg19_time_zone.txt` measured, in Unix seconds.
    const WINTER: i64 = 1_577_836_800; // 2020-01-01T00:00:00Z
    const SUMMER: i64 = 1_593_561_600; // 2020-07-01T00:00:00Z
    const BEFORE_STANDARD_TIME: i64 = -2_840_140_800; // 1880-01-01T00:00:00Z
    const PAST_THE_TABLE: i64 = 4_086_547_200; // 2099-07-01T00:00:00Z
    const PAST_THE_TABLE_WINTER: i64 = 4_070_908_800; // 2099-01-01T00:00:00Z

    fn new_york() -> Zone {
        Zone::by_name("America/New_York").expect("the database has New York")
    }

    #[test]
    fn a_zone_answers_the_offsets_postgresql_prints() {
        let zone = new_york();
        assert_eq!(zone.offset_at(WINTER).seconds, -5 * 3600);
        assert_eq!(zone.offset_at(WINTER).abbrev, "EST");
        assert!(!zone.offset_at(WINTER).is_dst);
        assert_eq!(zone.offset_at(SUMMER).seconds, -4 * 3600);
        assert_eq!(zone.offset_at(SUMMER).abbrev, "EDT");
        assert!(zone.offset_at(SUMMER).is_dst);
    }

    /// The row that says an offset is a number of **seconds**: New York kept local mean time until
    /// 1883, and PostgreSQL prints `1879-12-31 19:03:58-04:56:02`.
    #[test]
    fn an_offset_before_standard_time_is_not_a_whole_minute() {
        let zone = new_york();
        let offset = zone.offset_at(BEFORE_STANDARD_TIME);
        assert_eq!(offset.seconds, -17_762);
        assert_eq!(offset.seconds % 60, -2);
        assert_eq!(offset.abbrev, "LMT");
    }

    /// Both sides of both 2020 transitions, to the second.
    #[test]
    fn the_transitions_are_where_postgresql_puts_them() {
        let zone = new_york();
        assert_eq!(zone.offset_at(1_583_650_799).seconds, -5 * 3600); // 06:59:59Z, 01:59:59-05
        assert_eq!(zone.offset_at(1_583_650_800).seconds, -4 * 3600); // 07:00:00Z, 03:00:00-04
        assert_eq!(zone.offset_at(1_604_210_399).seconds, -4 * 3600); // 05:59:59Z, 01:59:59-04
        assert_eq!(zone.offset_at(1_604_210_400).seconds, -5 * 3600); // 06:00:00Z, 01:00:00-05
    }

    /// **The table this node reads is `slim`, so the footer is the live path and not an edge
    /// case.** `zic -b slim` writes transitions only up to the last *rule change* and leaves
    /// everything after to the `TZ` string, so New York's table stops in 2007 and every
    /// present-day instant is answered by `EST5EDT,M3.2.0,M11.1.0` rather than by a row.
    ///
    /// Found by breaking the rule evaluation and watching **six** of these nine tests redden,
    /// including the one about 2020 — which reads as a table lookup and is not one. Pinned here
    /// because a future `jiff-tzdb` shipping `fat` data would move the whole of that traffic onto
    /// the other path silently.
    #[test]
    fn the_footer_is_what_answers_a_present_day_instant() {
        let zone = new_york();
        let last = *zone
            .file
            .transitions
            .last()
            .expect("New York has transitions");
        assert!(
            last < WINTER,
            "New York's table reaches {last}, past 2020: the data is no longer slim and the \
             POSIX footer is no longer what answers a present-day instant"
        );
        assert!(
            zone.rule.is_some(),
            "a slim table with no footer answers nothing"
        );
    }

    /// Past the last transition the file records, the answer comes from the footer's rule —
    /// `EST5EDT,M3.2.0,M11.1.0` — and PostgreSQL agrees in both directions.
    #[test]
    fn the_posix_rule_governs_instants_past_the_table() {
        let zone = new_york();
        assert_eq!(zone.offset_at(PAST_THE_TABLE).seconds, -4 * 3600);
        assert!(zone.offset_at(PAST_THE_TABLE).is_dst);
        assert_eq!(zone.offset_at(PAST_THE_TABLE_WINTER).seconds, -5 * 3600);
        assert!(!zone.offset_at(PAST_THE_TABLE_WINTER).is_dst);
    }

    #[test]
    fn an_offset_can_be_a_quarter_of_an_hour_or_a_half_hour_step() {
        let kathmandu = Zone::by_name("Asia/Kathmandu").expect("Kathmandu");
        assert_eq!(kathmandu.offset_at(WINTER).seconds, 5 * 3600 + 45 * 60);
        let lord_howe = Zone::by_name("Australia/Lord_Howe").expect("Lord Howe");
        assert_eq!(lord_howe.offset_at(WINTER).seconds, 11 * 3600);
        let shanghai = Zone::by_name("Asia/Shanghai").expect("Shanghai");
        assert_eq!(shanghai.offset_at(WINTER).seconds, 8 * 3600);
    }

    /// A southern-hemisphere zone's daylight saving wraps the new year, which is the case the
    /// rule evaluation gets wrong if it tests an interval that cannot wrap.
    #[test]
    fn a_southern_zone_is_in_daylight_saving_in_january() {
        let sydney = Zone::by_name("Australia/Sydney").expect("Sydney");
        assert_eq!(sydney.offset_at(WINTER).seconds, 11 * 3600);
        assert!(sydney.offset_at(WINTER).is_dst);
        assert_eq!(sydney.offset_at(SUMMER).seconds, 10 * 3600);
        assert!(!sydney.offset_at(SUMMER).is_dst);
        // And past the table, where the same question is the footer's.
        assert_eq!(sydney.offset_at(PAST_THE_TABLE_WINTER).seconds, 11 * 3600);
        assert_eq!(sydney.offset_at(PAST_THE_TABLE).seconds, 10 * 3600);
    }

    /// One parse per zone, however many spellings ask for it.
    #[test]
    fn a_shared_zone_is_parsed_once_and_shared_by_every_spelling() {
        let one = Zone::shared("America/New_York").expect("New York");
        let two = Zone::shared("america/new_york").expect("lowercase");
        assert!(std::ptr::eq(one, two), "two spellings, two parses");
        assert_eq!(one.offset_at(SUMMER).abbrev, "EDT");
        assert!(Zone::shared("Nowhere/Notreal").is_none());
    }

    #[test]
    fn a_name_resolves_whatever_its_case_and_answers_the_canonical_one() {
        assert_eq!(
            Zone::by_name("america/new_york").expect("lowercase").name(),
            "America/New_York"
        );
        assert_eq!(
            Zone::by_name("AMERICA/NEW_YORK").expect("uppercase").name(),
            "America/New_York"
        );
        assert_eq!(Zone::by_name("UTC").expect("UTC").name(), "UTC");
        assert!(Zone::by_name("Nowhere/Notreal").is_none());
        assert!(Zone::by_name("").is_none());
    }

    /// **Every zone in the table, read.** The point is the ones nobody would choose: a file with
    /// no transitions at all, one whose footer is empty, one whose designations overlap. A reader
    /// tested on three zones is a reader tested on the three that happen to work.
    #[test]
    fn every_zone_in_the_database_reads() {
        let mut count = 0;
        for name in Zone::names() {
            let zone = Zone::by_name(name)
                .unwrap_or_else(|| panic!("{name} is in the table and did not read"));
            // Ask each one something, so a zone that parses into nothing is not a pass.
            let offset = zone.offset_at(SUMMER);
            assert!(
                offset.seconds.abs() <= 26 * 3600,
                "{name} answered an offset of {} seconds",
                offset.seconds
            );
            assert!(!offset.abbrev.is_empty(), "{name} has no abbreviation");
            count += 1;
        }
        // Printed so the number is evidence in a test run rather than something to go and
        // derive, the way `dep_budget.rs` prints the crate count.
        println!(
            "time zones read: {count}, IANA {:?}",
            Zone::database_version()
        );
        for name in ["Universal", "Zulu", "Greenwich", "Etc/UTC", "UTC", "GMT"] {
            println!("  {name}: {}", Zone::by_name(name).is_some());
        }
        assert!(count > 300, "only {count} zones were read");
    }

    /// The instants either side of every transition in every zone, which is where an off-by-one
    /// in the search would hide. Nothing is compared against PostgreSQL here — that is what the
    /// corpus is for — only that the table and the search agree with each other.
    #[test]
    fn every_transition_changes_the_offset_it_is_asked_about() {
        for name in Zone::names() {
            let Some(zone) = Zone::by_name(name) else {
                continue;
            };
            for (at, &instant) in zone.file.transitions.iter().enumerate() {
                let expected = &zone.file.types[usize::from(zone.file.indices[at])];
                assert_eq!(
                    zone.offset_at(instant).seconds,
                    expected.offset,
                    "{name} at transition {at} ({instant})"
                );
            }
        }
    }
}
