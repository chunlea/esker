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
//! * `search_path` is honoured **only where it means `public`**, because a schema qualifier is
//!   `0A000` here and there is exactly one place a name can be. `SET search_path TO other` is
//!   accepted by a real server and would make an unqualified name resolve to nothing; refusing it
//!   is the safe direction.
//! * `max_identifier_length` is **read-only**, as it is on a real server — `55P02`, which is a
//!   different answer from `42704` and means a different thing.
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
    /// database this node does not carry, so what narrows both here is [`honour`].
    Free,
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
        values: Values::Free,
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
            // One schema, because a schema qualifier is `0A000` here. Both spellings
            // `ActiveRecord` sends resolve to it; anything else would leave an unqualified name
            // resolving where a real server would find nothing.
            ("search_path", path) if !is_public(path) => Err(SqlError::unsupported(format!(
                "a search_path of \"{path}\""
            ))),
            // What is left is what this node means. `client_min_messages` is honoured for real —
            // `Executor::take_notices` filters against it — and `IntervalStyle` is inert and
            // measured to be: it decides how an `interval` prints and there is none here.
            _ => Ok(()),
        }
    }
}

/// The spellings of UTC this node can print in. `Etc/UTC` is the same instant offset and reads
/// back as itself, which is what a real server does with it.
fn is_utc(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "utc" | "etc/utc" | "universal" | "zulu" | "z" | "+00:00" | "utc+0" | "utc-0"
    )
}

/// A `search_path` whose every entry resolves to the one schema this node has.
///
/// `"$user"` is a schema named after the connected role, which does not exist here — and on a real
/// server a `search_path` entry that names no schema is skipped rather than refused, so
/// `"$user", public` *is* `public`. That is why `ActiveRecord`'s two spellings are one value.
fn is_public(value: &str) -> bool {
    let mut entries = value
        .split(',')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty() && *entry != "$user")
        .peekable();
    // An empty path is not `public`: on a real server it leaves an unqualified name resolving to
    // nothing, where this node would still find the table.
    entries.peek().is_some() && entries.all(|entry| entry == "public")
}
