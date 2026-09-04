//! The session parameters this node reports, and what each value actually means here.
//!
//! Eight of the thirty-six statements `ActiveRecord` sends at connect are `SET`s and `SHOW`s
//! (`docs/plans/phase-9-rails.md` §2, unit 5), and every one of them was `0A000` before this
//! module. They are the cheapest statements on the board and the easiest to get wrong, because the
//! tempting implementation — accept the name, store the string, do nothing — is the one answer
//! this project's contract forbids: it accepts what the oracle would have acted on.
//!
//! So each parameter is here with **what this node can honour**, measured rather than assumed
//! (`tests/corpus/pg19_set.txt`), and every value outside that is refused by name:
//!
//! * `client_min_messages` is **honoured**. This node raises real notices — `DROP TABLE IF EXISTS`
//!   for a table that is not there is one — and suppressing them is precisely why `ActiveRecord`
//!   sends the statement. [`Executor::take_notices`](crate::exec::Executor) filters against it.
//! * `standard_conforming_strings` takes `on` and nothing else, which is **PostgreSQL's own
//!   answer**: a real server refuses `off` with `0A000 non-standard string literals are not
//!   supported`. This crate's lexer is standard-conforming, so `on` is exact and `off` is the same
//!   refusal from the same class.
//! * `IntervalStyle` is **inert here and says so**: it decides how an `interval` prints, and this
//!   node has no `interval`. All four of PostgreSQL's values are stored and read back.
//! * `TimeZone` is honoured **only where it means UTC**, because `timestamptz` is printed in UTC
//!   and nowhere else (`crate::value::timestamp`). A real server takes `America/New_York`; this
//!   one refuses it by name rather than print an instant in the wrong zone.
//! * `search_path` is **not validated**, on a real server or here: an entry naming no schema is
//!   *skipped* rather than refused, which is what makes the default `"$user", public` resolve to
//!   `{public}`. `SHOW` gives the path as **set** and `current_schemas` gives it as **resolved**,
//!   and the resolving is `crate::exec::Executor`'s, where the catalog is.
//! * `statement_timeout` and `lock_timeout` are honoured **only at `0`**, which is what they
//!   permanently are here: nothing cancels a running statement and nothing waits for a row lock,
//!   so `SHOW` answering `0` is exact. A non-zero value is `0A000` naming the parameter — the one
//!   entry in this table whose absence was measured as a *hang* rather than a wrong answer
//!   (`tests/transaction_timeouts.rs`).
//! * `max_identifier_length` is **read-only**, as it is on a real server — `55P02`, which is a
//!   different answer from `42704` and means a different thing.
//! * `esker.engine` is **this node's own** and is honoured in the strongest sense in this table: it
//!   decides which engine a query runs on, and `EXPLAIN` names what it decided
//!   ([ADR 0022](../../docs/adr/0022-columnar-learner-replica.md) Decision 2). A real server
//!   accepts every value for it, because it validates no custom parameter at all; this one refuses
//!   anything but `row`, `columnar` and `auto`, which is the same direction every other row here
//!   takes.
//!
//! A parameter not in this table is `42704`, which is what a real server says and is *not*
//! contract C2's `0A000`: the statement is one this node runs, and what is missing is the
//! parameter.

use crate::error::{Result, SqlError};

/// One parameter, as PostgreSQL describes it and as this node answers for it.
#[derive(Debug, Clone, Copy)]
pub struct Parameter {
    /// The name a user writes, lower-cased — GUC names are case-insensitive.
    pub name: &'static str,
    /// PostgreSQL's **own** spelling, which is what its messages and `SHOW`'s column use.
    /// `SET intervalstyle = bogus` is refused naming `"IntervalStyle"`, and `SHOW intervalstyle`
    /// returns a column called `IntervalStyle`. Measured, both.
    pub reported: &'static str,
    /// The value at connect, which `RESET` and `TO DEFAULT` — one operation, not two — go back to.
    pub boot: &'static str,
    /// What PostgreSQL accepts, or [`Values::Free`] where it takes any text.
    pub values: Values,
    /// A parameter that exists and cannot be set: `55P02`, never `42704`.
    pub read_only: bool,
}

/// What a parameter's value may be.
#[derive(Debug, Clone, Copy)]
pub enum Values {
    /// One of these, case-insensitively. Anything else is `22023` with them as the `HINT`.
    Enum(&'static [&'static str]),
    /// A boolean, in any of PostgreSQL's spellings. Anything else is `22023` with a sentence of
    /// its own rather than a list — measured: `parameter "x" requires a Boolean value`.
    Boolean,
    /// Any text. PostgreSQL validates `search_path` not at all and `TimeZone` against a zone
    /// database this node does not carry, so what narrows both here is [`Parameter::honour`].
    ///
    /// Qualified, which is what a bare `[honour]` was missing: rustdoc resolves a name in the
    /// *module*'s scope, and an inherent method is not in one. It is not that the method is
    /// private — it is `pub`, on a `pub` struct — so the link resolves once it says which type it
    /// belongs to. Both lanes that met this under `-D warnings` fixed it; this is the fix that
    /// keeps the link.
    Free,
    /// An integer with an optional time unit: `10ms`, `2s`, `0`.
    ///
    /// **The unit stays in the value.** `SHOW idle_in_transaction_session_timeout` answers `10ms`,
    /// not `10`, and `pg_settings.unit` reports `ms` separately — so the two together would say
    /// `10ms ms` if the value were bare. Measured. Zero reads back as `0` with no unit, because
    /// that is what a real server stores for it.
    Duration,
    /// A comma-separated list of names, **requoted the way PostgreSQL prints one**.
    ///
    /// `'$user',public` reads back as `"$user", public`: an entry that is not a plain identifier
    /// is double-quoted, and the separator gains a space. A node that echoed the input would fail
    /// the tests that compare what `SHOW` gives against what they set.
    NameList,
}

/// PostgreSQL's boolean spellings, which are one value each side.
const TRUE_SPELLINGS: &[&str] = &["on", "true", "yes", "1"];
const FALSE_SPELLINGS: &[&str] = &["off", "false", "no", "0"];

/// The parameters this node has. Everything else is `42704`.
pub const PARAMETERS: &[Parameter] = &[
    Parameter {
        name: "client_min_messages",
        reported: "client_min_messages",
        boot: "notice",
        values: Values::Enum(&[
            "debug5", "debug4", "debug3", "debug2", "debug1", "log", "notice", "warning", "error",
        ]),
        read_only: false,
    },
    Parameter {
        name: "standard_conforming_strings",
        reported: "standard_conforming_strings",
        boot: "on",
        values: Values::Boolean,
        read_only: false,
    },
    Parameter {
        name: "intervalstyle",
        reported: "IntervalStyle",
        boot: "postgres",
        values: Values::Enum(&["postgres", "postgres_verbose", "sql_standard", "iso_8601"]),
        read_only: false,
    },
    Parameter {
        name: "timezone",
        reported: "TimeZone",
        boot: "UTC",
        values: Values::Free,
        read_only: false,
    },
    Parameter {
        name: "search_path",
        reported: "search_path",
        boot: "\"$user\", public",
        values: Values::NameList,
        read_only: false,
    },
    // **The four run 45's ranking is made of**, 33 tests between them. Every one is a setting the
    // suite changes and reads back rather than a feature it uses: `money_test` sets `lc_monetary`
    // to compare formatting, `connection_test` sets the idle timeout and `geqo`, and
    // `schema_authorization_test` sets `search_path`. Recorded and reported; see `honour` for
    // which of them this node *acts* on, which is a shorter list.
    Parameter {
        name: "lc_monetary",
        reported: "lc_monetary",
        // **`C` and not a real locale.** A server's boot value is a property of its container —
        // `esker-pg19` says `en_US.utf8` — and this node has no locale database at all, so `C`
        // is the honest default: the one locale whose rules are "no rules".
        boot: "C",
        values: Values::Free,
        read_only: false,
    },
    Parameter {
        name: "idle_in_transaction_session_timeout",
        reported: "idle_in_transaction_session_timeout",
        boot: "0",
        values: Values::Duration,
        read_only: false,
    },
    // **The two timeouts `adapters/postgresql/transaction_test.rb` sets**, and the reason that
    // file hung run 47 for twenty minutes. Both were `42704` — the wrong sentence about a
    // parameter a real server has — and both are here now so that `SHOW` can answer `0`, which is
    // *true*: no statement here is cancelled by a clock and nothing here waits for a row lock, and
    // `0` is PostgreSQL's own spelling of both. A non-zero value is refused by name in `honour`.
    Parameter {
        name: "statement_timeout",
        reported: "statement_timeout",
        boot: "0",
        values: Values::Duration,
        read_only: false,
    },
    // **`0`, which is PostgreSQL's: wait forever.** A ceiling was tried here and reverted, and the
    // reason is worth keeping: a long-held lock in another worker is *normal* in a Rails
    // application and a real server waits for it, so a node that gives up after some seconds fails
    // a production workload that PostgreSQL serves — and `SHOW` reporting the ceiling honestly
    // does not make the node compatible, it only makes the incompatibility documented. If this
    // node ever needs self-protection from an unbounded wait it belongs in a setting of its own,
    // named as esker's and off by default, never in the default of a parameter a client already
    // knows the meaning of.
    Parameter {
        name: "lock_timeout",
        reported: "lock_timeout",
        boot: "0",
        values: Values::Duration,
        read_only: false,
    },
    Parameter {
        name: "geqo",
        reported: "geqo",
        boot: "on",
        values: Values::Boolean,
        read_only: false,
    },
    Parameter {
        name: "debug_print_plan",
        reported: "debug_print_plan",
        boot: "off",
        values: Values::Boolean,
        read_only: false,
    },
    // **This node's own, and it is honoured** — `EXPLAIN` names the engine it chose and this is
    // what a user says when the estimate is wrong (ADR 0022 Decision 2, rule 4). Measured against
    // PostgreSQL 19 like everything else here, and the measurement is what makes it a *declared*
    // divergence rather than an accident: a real server accepts `SET esker.engine = 'sideways'`,
    // because it validates no custom parameter's value, ever. Accepting a value this node will not
    // act on is the one answer this module exists to refuse, so `22023` with the three spellings
    // as a `HINT` — PostgreSQL's own shape for an enum it *does* know
    // (`tests/corpus/pg19_routing_engine.txt`).
    Parameter {
        name: "esker.engine",
        reported: "esker.engine",
        // `auto` rather than "unset": this node has a real default and it is the rule. PostgreSQL
        // answers `42704` for a namespaced GUC until the first `SET`, which is the other half of
        // the declared divergence.
        boot: "auto",
        values: Values::Enum(&["row", "columnar", "auto"]),
        read_only: false,
    },
    Parameter {
        name: "max_identifier_length",
        reported: "max_identifier_length",
        // Sixty-three bytes, the same number `SqlError::IdentifierTruncated` is about. A client
        // that reads this to decide how long a name it may generate is told the truth.
        boot: "63",
        values: Values::Free,
        read_only: true,
    },
];

/// The unit every [`Values::Duration`] parameter in [`PARAMETERS`] is *stored* in.
///
/// PostgreSQL gives each such parameter a base unit and reports it separately as
/// `pg_settings.unit`; all three here are `ms`, measured. A parameter whose base unit were
/// seconds would have to carry it rather than read it from beside the function — there is none,
/// so this is a constant and not a field.
const DURATION_BASE_UNIT: &str = "ms";

/// What PostgreSQL admits for the three duration parameters here: **a C `int` of base units**,
/// and its own sentence quotes both ends. Measured, from the message itself —
/// `-5000 ms is outside the valid range for parameter "statement_timeout" (0 ms .. 2147483647 ms)`.
const DURATION_MAX: i64 = i32::MAX as i64;

/// Every unit PostgreSQL accepts, **largest first**, as a fraction of [`DURATION_BASE_UNIT`].
///
/// The order is what makes re-printing work: a stored value is written in the first unit that
/// divides it exactly, which is why `2000ms` reads back as `2s` and `120s` as `2min`. `us` is
/// last and is deliberately *not* a printing unit — it is smaller than the base, so a value in it
/// is converted away and never comes back (`1500us` reads back `2ms`, measured).
const DURATION_UNITS: &[(&str, i64, i64)] = &[
    ("d", 86_400_000, 1),
    ("h", 3_600_000, 1),
    ("min", 60_000, 1),
    ("s", 1_000, 1),
    ("ms", 1, 1),
    ("us", 1, 1_000),
];

/// Why a duration value was refused. Three conditions, because **PostgreSQL gives them two
/// different sentences** and a client can tell them apart.
enum DurationRejection {
    /// Not a count and a unit at all: `'banana'`. Quoted back as written.
    NotADuration,
    /// A count whose value in base units will not fit a C `int`: `'2147483648'`, `'25d'`.
    /// PostgreSQL quotes it back with the *same* sentence as [`Self::NotADuration`] and separates
    /// the two by a `HINT` alone — measured, and the reason this is not folded into that one.
    ExceedsIntegerRange,
    /// A count that fits and is outside what the parameter takes: `'-1'`, `'-5s'`. Here the
    /// sentence names the range, and reports the value **converted to the base unit**: `'-5s'` is
    /// reported as `-5000 ms`.
    OutOfRange(i64),
}

/// `10ms`, `2s`, `0` — a count and an optional unit, **converted to the base unit and re-printed
/// the way PostgreSQL prints one**.
///
/// Not "trim and keep", which is what this was and what three measurements refuted:
///
/// * **A bare count gains the base unit.** `SET … = '250'` reads back `250ms`, not `250` — and
///   `pg_settings.unit` says `ms` beside it, so the two together do not say `250ms ms`.
/// * **The value is re-printed in the largest unit that divides it exactly.** `'2000ms'` reads
///   back `2s`, `'120s'` reads back `2min`, `'60min'` reads back `1h`. A node that echoed the
///   unit it was given fails every test that compares a read-back against a literal.
/// * **A count below the base unit is converted, rounding half to even.** `'1500us'` and
///   `'2500us'` both read back `2ms`; `'3500us'` reads back `4ms`; `'500us'` reads back **`0`**,
///   which is the spelling of *off*. That is `rint`, which is what PostgreSQL's own conversion
///   uses, and it is why this rounds ties to even rather than up.
///
/// **Zero loses its unit** in every spelling: `'0ms'`, `'0s'` and `0` all read back `0`.
///
/// The count is parsed as a `f64` because PostgreSQL parses it with `strtod` and accepts a
/// fraction — `'1.5s'` is `1500ms`, measured.
fn normalise_duration(value: &str) -> std::result::Result<String, DurationRejection> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '-' | '+' | 'e' | 'E'))
        .unwrap_or(trimmed.len());
    let (count, unit) = trimmed.split_at(split);
    let count: f64 = count
        .trim()
        .parse()
        .map_err(|_| DurationRejection::NotADuration)?;
    if !count.is_finite() {
        return Err(DurationRejection::NotADuration);
    }
    let unit = unit.trim();
    let (_, numerator, denominator) = *DURATION_UNITS
        .iter()
        .find(|(name, ..)| *name == unit || (unit.is_empty() && *name == DURATION_BASE_UNIT))
        .ok_or(DurationRejection::NotADuration)?;

    // `round_ties_even` **is** `rint`, and the tie is not hypothetical: `2500us` is exactly half a
    // base unit away and PostgreSQL answers `2ms`, not `3ms`.
    #[allow(clippy::cast_precision_loss)]
    let scaled = (count * numerator as f64 / denominator as f64).round_ties_even();
    #[allow(clippy::cast_precision_loss)]
    if !(-(DURATION_MAX as f64) - 1.0..=DURATION_MAX as f64).contains(&scaled) {
        return Err(DurationRejection::ExceedsIntegerRange);
    }
    #[allow(clippy::cast_possible_truncation)]
    let base = scaled as i64;
    if base < 0 {
        return Err(DurationRejection::OutOfRange(base));
    }
    if base == 0 {
        return Ok("0".to_owned());
    }
    // `ms` has a numerator of 1 and divides everything, so this always finds a unit; `us` is never
    // reached, which is what keeps a sub-base value from printing in a unit it was converted out of.
    let (name, numerator, _) = DURATION_UNITS
        .iter()
        .find(|(_, numerator, denominator)| *denominator == 1 && base % *numerator == 0)
        .unwrap_or(&("ms", 1, 1));
    Ok(format!("{}{name}", base / numerator))
}

/// `'$user',public` → `"$user", public`.
///
/// An entry that is not a plain lower-case identifier is double-quoted, which is what makes
/// `$user` come back quoted and `public` bare; the separator is a comma **and a space**. Both are
/// measured, and both are what the read-back comparison in `schema_authorization_test` checks.
fn normalise_name_list(value: &str) -> String {
    value
        .split(',')
        .map(|entry| {
            let entry = entry.trim().trim_matches('"').trim_matches('\'');
            let plain = !entry.is_empty()
                && entry
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if plain {
                entry.to_owned()
            } else {
                format!("\"{entry}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The parameter of that name, or `42704` — PostgreSQL's answer for a name it has never heard of,
/// and the reason a `SHOW` of one is not contract C2's `0A000`.
pub fn lookup(name: &str) -> Result<&'static Parameter> {
    let folded = name.to_ascii_lowercase();
    PARAMETERS
        .iter()
        .find(|parameter| parameter.name == folded)
        .ok_or_else(|| SqlError::UnrecognizedParameter(name.to_owned()))
}

impl Parameter {
    /// The value as PostgreSQL would store it, or the condition it raises for one it will not.
    ///
    /// Two failures, and they are PostgreSQL's own sentences: an enum outside its list is `22023`
    /// with the list as a `HINT`, and a boolean that is not one is `22023` with no list at all.
    /// Both name the parameter in [`Parameter::reported`] spelling rather than the user's.
    pub fn normalise(&self, value: &str) -> Result<String> {
        if self.read_only {
            return Err(SqlError::CannotChangeParameter(self.reported));
        }
        let folded = value.to_ascii_lowercase();
        match self.values {
            // The stored value is the canonical spelling, not the user's: `'WARNING'` reads back
            // as `warning` on a real server.
            Values::Enum(allowed) => allowed
                .iter()
                .find(|candidate| **candidate == folded)
                .map(|candidate| (*candidate).to_owned())
                .ok_or_else(|| SqlError::InvalidParameterValue {
                    name: self.reported,
                    value: value.to_owned(),
                }),
            Values::Boolean => {
                if TRUE_SPELLINGS.contains(&folded.as_str()) {
                    Ok("on".to_owned())
                } else if FALSE_SPELLINGS.contains(&folded.as_str()) {
                    Ok("off".to_owned())
                } else {
                    Err(SqlError::NonBooleanParameter(self.reported))
                }
            }
            // Read back exactly as written: a real server hands `Etc/UTC` back as `Etc/UTC`.
            Values::Free => Ok(value.to_owned()),
            // Three refusals rather than one, because PostgreSQL gives them two sentences: a
            // value it cannot read and one whose magnitude will not fit are quoted back
            // identically and separated by a `HINT` alone, while one that fits and is out of
            // range names the range and reports itself **in the base unit**.
            Values::Duration => normalise_duration(value).map_err(|rejection| match rejection {
                DurationRejection::NotADuration => SqlError::InvalidParameterValue {
                    name: self.reported,
                    value: value.to_owned(),
                },
                DurationRejection::ExceedsIntegerRange => {
                    SqlError::ParameterValueExceedsIntegerRange {
                        name: self.reported,
                        value: value.to_owned(),
                    }
                }
                DurationRejection::OutOfRange(base) => SqlError::ParameterOutOfRange {
                    value: format!("{base} {DURATION_BASE_UNIT}"),
                    name: self.reported,
                    low: format!("0 {DURATION_BASE_UNIT}"),
                    high: format!("{DURATION_MAX} {DURATION_BASE_UNIT}"),
                },
            }),
            Values::NameList => Ok(normalise_name_list(value)),
        }
    }

    /// Whether this node *means* the value, or refuses it by name.
    ///
    /// The whole point of the module. Every arm is a measured claim about what this node does,
    /// and a value that reaches none of them is `0A000` naming itself — never accepted-and-ignored,
    /// which would be a setting a client asked for and did not get.
    pub fn honour(&self, value: &str) -> Result<()> {
        match (self.name, value) {
            // PostgreSQL's own answer, with PostgreSQL's own sentence. This crate's string lexer
            // is standard-conforming and cannot be made otherwise by a setting.
            ("standard_conforming_strings", "off") => Err(SqlError::NonStandardStringLiterals),
            // `timestamptz` is printed in UTC and nowhere else, so a zone that is not UTC would be
            // a setting honoured in `SHOW` and ignored in every row.
            ("timezone", zone) if !is_utc(zone) => {
                Err(SqlError::unsupported(format!("the time zone \"{zone}\"")))
            }
            // **A timeout this node cannot enforce, and `0` is the one value it can.** Nothing
            // here cancels a running statement — the executor runs one to completion on a
            // blocking thread and no clock interrupts it — and nothing here waits for a row lock,
            // because a Percolator prewrite that meets a live lock is `40001` after a bounded
            // backoff rather than a wait. So there is no wait for `lock_timeout` to bound and no
            // cancellation for `statement_timeout` to schedule.
            //
            // This is the one refusal in this table whose *absence* was measured as a hang rather
            // than as a wrong answer: a client told it holds a 150 ms cancellation waits for one,
            // and `adapters/postgresql/transaction_test.rb` waited twenty minutes. `0` is
            // accepted because it asks for what is already the case.
            // **`lock_timeout` is honoured now** — it is the first timeout this node can keep,
            // because a waiter is a loop the SQL layer drives and is cancellable in a way a
            // statement that is *working* is not (ADR 0057). It therefore falls through to the
            // catch-all below rather than having an arm of its own. `statement_timeout` stays
            // refused, and the refusal is narrower rather than gone.
            ("statement_timeout", value) if !is_no_timeout(value) => Err(SqlError::unsupported(
                format!("a non-zero {} ({value})", self.reported),
            )),
            // **A `search_path` is not validated**, on a real server or here: an entry naming no
            // schema is *skipped* rather than refused, which is what makes the default
            // `"$user", public` mean `{public}`. `SHOW` gives the path as **set** and
            // `current_schemas` gives it as **resolved**, and both are measured.
            // What is left is what this node means. `client_min_messages` is honoured for real —
            // `Executor::take_notices` filters against it — and `IntervalStyle` is inert and
            // measured to be: it decides how an `interval` prints and there is none here.
            _ => Ok(()),
        }
    }
}

/// A stored [`Values::Duration`] as milliseconds, or `None` where it means "off".
///
/// The inverse of `normalise_duration` (private, so a code span rather than a link), and it
/// reads **only what that function wrote**: a
/// non-negative whole count in one of the five printing units, or a bare `0`. That is what lets
/// it be this short — there is no fraction to round and no `us` to convert, because
/// that function has already done both. `None` for `0`, which is PostgreSQL's spelling
/// of "no limit", and `None` for a value that is not a duration at all, which a parameter of
/// another kind would be.
#[must_use]
pub fn duration_ms(value: &str) -> Option<u64> {
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (count, unit) = value.split_at(digits);
    let count: u64 = count.parse().ok()?;
    let (_, numerator, _) = DURATION_UNITS.iter().find(|(name, _, denominator)| {
        *denominator == 1 && (*name == unit || (unit.is_empty() && *name == DURATION_BASE_UNIT))
    })?;
    #[allow(clippy::cast_sign_loss)]
    let scaled = count.checked_mul(*numerator as u64)?;
    (scaled != 0).then_some(scaled)
}

/// Whether a duration means "no timeout", which is the only value the two timeouts can honour.
///
/// [`Values::Duration`] has already normalised the value, and its rule is that **zero loses its
/// unit** — `'0ms'` and `0` both store `0` — so one comparison covers every spelling PostgreSQL
/// accepts for off.
fn is_no_timeout(value: &str) -> bool {
    value == "0"
}

/// The spellings of UTC this node can print in. `Etc/UTC` is the same instant offset and reads
/// back as itself, which is what a real server does with it.
fn is_utc(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "utc" | "etc/utc" | "universal" | "zulu" | "z" | "+00:00" | "utc+0" | "utc-0"
    )
}

/// `lock_timeout`, which bounds how long a writer waits for the row in front of it (ADR 0057).
#[must_use]
pub fn lock_timeout() -> &'static Parameter {
    PARAMETERS
        .iter()
        .find(|parameter| parameter.name == "lock_timeout")
        .unwrap_or(&PARAMETERS[0])
}

/// `statement_timeout`, which bounds the same wait when `lock_timeout` does not.
#[must_use]
pub fn statement_timeout() -> &'static Parameter {
    PARAMETERS
        .iter()
        .find(|parameter| parameter.name == "statement_timeout")
        .unwrap_or(&PARAMETERS[0])
}

/// A timeout parameter's value in milliseconds, or `None` for one that is not a duration.
///
/// PostgreSQL reports these as a bare number of milliseconds or with a unit — `0`, `31s`, `300ms`
/// — and both spellings reach here, because both are spellings a client `SET`.
#[must_use]
pub fn timeout_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    for (suffix, scale) in [("ms", 1_u64), ("s", 1_000), ("min", 60_000)] {
        if let Some(number) = value.strip_suffix(suffix) {
            return number.trim().parse::<u64>().ok().map(|n| n * scale);
        }
    }
    value.parse::<u64>().ok()
}

/// The `search_path` parameter, for the executor that resolves it.
#[must_use]
pub fn search_path() -> &'static Parameter {
    PARAMETERS
        .iter()
        .find(|parameter| parameter.name == "search_path")
        .unwrap_or(&PARAMETERS[0])
}
