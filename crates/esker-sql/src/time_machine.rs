//! Reading the past: what a user may write, what it resolves to, and what refuses it.
//!
//! [ADR 0021](../../../docs/adr/0021-time-machine.md) Decision 1: *a historical read is a read
//! timestamp, and nothing else.* Every version of every key is already filed under
//! `txn_key(user_key, commit_ts)` and a read at `T` is already "the newest write with
//! `commit_ts ≤ T`", so this module builds no storage. It turns the two things a user can name —
//! an instant, or a distance back from now — into the one number a transaction needs, and it holds
//! the three refusals that make such a read safe.
//!
//! # The surface is PostgreSQL's, because the invented ones do not parse
//!
//! `docs/plans/phase-6d.md` §1 has the measurement. `AS OF SYSTEM TIME` is `CockroachDB`'s and
//! neither `sqlparser` 0.62.0 nor PostgreSQL 19 reads it; `SET esker.read_as_of` is PostgreSQL's
//! own custom-GUC namespace and both do. So the *syntax* here is PostgreSQL's and the *semantics*
//! are `CockroachDB`'s, which is the least divergent shape a feature PostgreSQL does not have can
//! take — and it needs no line inside `crate::parse`'s grammar.
//!
//! # Three refusals, never a clamp
//!
//! A read that cannot be answered correctly is refused with a sentence naming the window, because
//! the user asked for a time and the useful reply is how far back they *can* ask. Rounding their
//! request to the nearest answerable instant would return a correct-looking result for a question
//! they did not ask.
//!
//! * **Not in the future** — a timestamp above the oracle's high-water mark names an instant that
//!   has not happened, and a read there would see a prefix of it and call it complete.
//! * **Not below the window** — retention is how far back the collector has not yet swept, and a
//!   read below it cannot be answered correctly (`docs/txn-spec.md` §7).
//! * **No writes** — enforced in [`crate::backend`] and checked again before a statement is
//!   planned. A commit at `commit_ts > start_ts` against an old snapshot is a lost update that
//!   Percolator's conflict check does not catch, so this is correctness and not policy.

use esker_client::{TSO_LOGICAL_BITS, physical_ms, ts_at_ms};

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum};

/// The GUC a session reads the past through.
///
/// Namespaced, which is what makes it valid PostgreSQL: a real server accepts a custom GUC in a
/// namespace, stores it and hands it back to `SHOW`, and refuses an un-namespaced one with
/// `42704`. So every statement in this feature is one a real PostgreSQL executes; it simply does
/// not act on it.
pub const READ_AS_OF: &str = "esker.read_as_of";

/// Milliseconds from the Unix epoch to PostgreSQL's, which is 2000-01-01 00:00:00 UTC.
///
/// `crate::value` stores an instant as microseconds from PostgreSQL's epoch, because that is what
/// survives a binary round trip without arithmetic; a TSO timestamp's physical half is
/// milliseconds from the Unix epoch. One constant converts between them, in one place.
const POSTGRES_EPOCH_UNIX_MS: i64 = 946_684_800_000;

/// The prefix of an exported snapshot token.
///
/// PostgreSQL's own ids are `%08X-%08X-%d` and carry a transaction id; ours carry a `start_ts` and
/// are a different shape, so they are prefixed rather than disguised. A client that treats the
/// token as opaque — which is the documented contract on both sides — cannot tell the difference.
const TOKEN_PREFIX: &str = "esker-";

/// What a snapshot identifier turned out to name.
///
/// One namespace holds both, which is what lets `SET TRANSACTION SNAPSHOT` take an exported token
/// and a checkpoint name without the user having to say which they have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotId {
    /// An exported token, carrying the timestamp directly. No lookup.
    Timestamp(u64),
    /// A checkpoint's name, to be looked up in the catalog.
    Checkpoint(String),
}

/// Renders a timestamp as an exported snapshot token.
#[must_use]
pub fn token(start_ts: u64) -> String {
    format!("{TOKEN_PREFIX}{start_ts:016x}")
}

/// Reads a snapshot identifier as either an exported token or a checkpoint name.
///
/// The two failures are PostgreSQL's own and they are *different*, which a capture is the only way
/// to know: a string that cannot be an identifier at all is `22023 invalid snapshot identifier`,
/// and one that is well formed but is not there is `42704 snapshot "..." does not exist`. This
/// function raises the first; the second belongs to whoever does the lookup.
///
/// A string in PostgreSQL's own `%08X-%08X-%d` shape is well formed and belongs to another server,
/// so it resolves to a checkpoint name that will not be found — `42704`, which is exactly what a
/// real server answers for an id it does not have.
pub fn parse_snapshot_id(id: &str) -> Result<SnapshotId> {
    if let Some(hex) = id.strip_prefix(TOKEN_PREFIX)
        && hex.len() == 16
        && let Ok(start_ts) = u64::from_str_radix(hex, 16)
    {
        return Ok(SnapshotId::Timestamp(start_ts));
    }
    // Anything that could be a name is one. What cannot: the empty string, and anything past the
    // identifier length, since a checkpoint's name is stored as an identifier like any other.
    if id.is_empty() || id.len() > crate::catalog::MAX_IDENTIFIER_BYTES {
        return Err(SqlError::InvalidSnapshotIdentifier(id.to_owned()));
    }
    Ok(SnapshotId::Checkpoint(id.to_owned()))
}

/// Whether a string may be a checkpoint's name.
///
/// The **same rule** [`parse_snapshot_id`] reads a name by, and shared rather than restated: a name
/// that could be written and then not imported would be a checkpoint nobody could use.
pub fn check_name(name: &str) -> Result<()> {
    match parse_snapshot_id(name)? {
        SnapshotId::Checkpoint(_) => Ok(()),
        // A name shaped like an exported token would shadow the token it looks like: importing it
        // would read the timestamp out of the string and never reach the record.
        SnapshotId::Timestamp(_) => Err(SqlError::InvalidSnapshotIdentifier(name.to_owned())),
    }
}

/// Reads what a user set [`READ_AS_OF`] to, against the oracle's `now`.
///
/// Two spellings, and the reason there are two is that a user has one of two things: an instant
/// they read off a log, or a distance back from now. Both resolve here, **once, at `SET` time** —
/// resolving `'-1h'` per statement would make it a different instant in every statement of a
/// session, which is not a snapshot.
pub fn resolve(text: &str, now: u64) -> Result<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(invalid_value(text));
    }
    // An interval always starts with a sign or a digit and always ends with a unit letter; a
    // timestamp starts with a digit too, so the unit letter is what tells them apart. Trying the
    // interval first and falling back keeps one grammar from swallowing the other's errors.
    if let Some(millis) = interval_ms(trimmed) {
        let now_ms = i64::try_from(physical_ms(now)).unwrap_or(i64::MAX);
        let at_ms = now_ms
            .checked_add(millis)
            .filter(|ms| *ms >= 0)
            .ok_or_else(|| invalid_value(text))?;
        return Ok(ts_at_ms(u64::try_from(at_ms).unwrap_or(0)));
    }
    let Ok(Datum::TimestampTz(micros)) = Datum::from_text(ColumnType::TimestampTz, trimmed) else {
        return Err(invalid_value(text));
    };
    ts_of_micros(micros).ok_or_else(|| invalid_value(text))
}

/// Reads a retention: a *distance*, in the same interval grammar [`resolve`] takes.
///
/// One grammar for both, because they are the same kind of quantity — a user who learned `'-1h'`
/// for a read timestamp should not have to learn a second spelling for how long the past is kept.
/// A retention is written without a sign, and a negative one is refused rather than read as its
/// magnitude: `'-1h'` of retention is not a thing somebody means.
pub fn retention_ms(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let millis = interval_ms(trimmed).ok_or_else(|| invalid_retention(text))?;
    u64::try_from(millis).map_err(|_| invalid_retention(text))
}

/// `22023`, PostgreSQL's own condition for a `SET` value it cannot read, named for the storage
/// parameter rather than for the GUC.
fn invalid_retention(text: &str) -> SqlError {
    SqlError::InvalidParameterValue {
        name: "retention",
        value: text.to_owned(),
    }
}

/// A timestamp from an instant in `crate::value`'s representation.
///
/// `ts = physical_ms << TSO_LOGICAL_BITS` with the logical bits **zero**, which is the first
/// timestamp of that millisecond: a read at it sees every transaction that committed strictly
/// before that millisecond and none that committed within it. That is the correct rounding for
/// "as of 14:00" — a transaction committing *at* 14:00:00.000 is not yet visible at 14:00:00.000,
/// exactly as it is not visible at the instant before it commits.
///
/// `None` for an instant before the Unix epoch, and for `±infinity`: there are no timestamps there.
fn ts_of_micros(micros: i64) -> Option<u64> {
    if micros == crate::value::POS_INFINITY || micros == crate::value::NEG_INFINITY {
        return None;
    }
    let unix_ms = micros
        .div_euclid(1000)
        .checked_add(POSTGRES_EPOCH_UNIX_MS)?;
    let unix_ms = u64::try_from(unix_ms).ok()?;
    // Past the physical half's range the shift would drop bits and name a different instant.
    (unix_ms < (1u64 << (64 - TSO_LOGICAL_BITS))).then(|| ts_at_ms(unix_ms))
}

/// An instant back in `crate::value`'s representation, for a message that names a window.
fn micros_of_ts(ts: u64) -> i64 {
    let unix_ms = i64::try_from(physical_ms(ts)).unwrap_or(i64::MAX);
    unix_ms
        .saturating_sub(POSTGRES_EPOCH_UNIX_MS)
        .saturating_mul(1000)
}

/// A timestamp rendered the way this crate renders an instant, for an error message.
#[must_use]
pub fn render(ts: u64) -> String {
    Datum::TimestampTz(micros_of_ts(ts))
        .to_text()
        .unwrap_or_else(|| ts.to_string())
}

/// Reads `[-]<number><unit>...` — `-1h`, `-30m`, `-1h30m`, `-500ms` — into milliseconds.
///
/// `CockroachDB`'s short interval form, which is the one `AS OF SYSTEM TIME '-1h'` uses and the one
/// a user reaching for a time machine writes. PostgreSQL's full `interval` grammar (`-1 hour`,
/// `-00:30:00`) is **not** read here: there is no `interval` type in this crate to be right with,
/// and a grammar half-implemented would accept `'-1 hour'` and silently mean something else. It is
/// a documented divergence rather than a partial parser.
///
/// `None` when the text is not an interval at all, which is how [`resolve`] falls through to the
/// timestamp grammar.
fn interval_ms(text: &str) -> Option<i64> {
    let (sign, body) = match text.as_bytes().first()? {
        b'-' => (-1i64, &text[1..]),
        b'+' => (1i64, &text[1..]),
        _ => (1i64, text),
    };
    if body.is_empty() {
        return None;
    }

    let mut total: i64 = 0;
    let mut rest = body;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
        let (number, tail) = rest.split_at(digits);
        let unit_len = tail
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let scale = match unit {
            "ms" => 1i64,
            "s" => 1_000,
            "m" => 60 * 1_000,
            "h" => 60 * 60 * 1_000,
            "d" => 24 * 60 * 60 * 1_000,
            _ => return None,
        };
        total = total.checked_add(number.parse::<i64>().ok()?.checked_mul(scale)?)?;
        rest = tail;
    }
    total.checked_mul(sign)
}

/// The travel window, as the SQL layer can see it.
///
/// **Advisory and conservative, and the docstring says so because the code cannot.** PD publishes
/// the safepoint and the store is what enforces it (ADR 0021 Decision 2); what this layer knows is
/// retention, which is the number the safepoint is *derived* from. The check exists so that a user
/// asking for last week when retention is an hour gets a sentence naming the window instead of an
/// empty result set.
///
/// Per-table overrides make the real window per table. This is the **cluster default**, so a table
/// with a longer override can be read further back than this admits — a refusal that is too strict
/// rather than too lax, which is the right direction for a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The oracle's high-water mark: the newest instant that has happened.
    pub now: u64,
    /// The oldest instant still readable, or `None` when retention is
    /// [`crate::catalog::RETENTION_FOREVER`] and nothing is ever collected.
    pub floor: Option<u64>,
}

impl Window {
    /// The window `retention_ms` gives, as of `now`.
    #[must_use]
    pub fn new(now: u64, retention_ms: u64) -> Self {
        // `RETENTION_FOREVER` is a sentinel and not a duration: subtracting it underflows, and the
        // rule it stands for is "collect nothing", which is a window with no floor. So is any
        // retention too long to express as a timestamp distance — the shift would drop its high
        // bits and name a floor far *nearer* than the user asked for, which would refuse reads a
        // longer retention was meant to allow.
        let floor = (retention_ms <= (u64::MAX >> TSO_LOGICAL_BITS))
            .then(|| now.saturating_sub(retention_ms << TSO_LOGICAL_BITS));
        Window { now, floor }
    }

    /// Refuses a timestamp outside the window, naming the window rather than repeating the input.
    ///
    /// The message is PostgreSQL's own shape for a `SET` value out of range — measured:
    /// `-5 ms is outside the valid range for parameter "lock_timeout" (0 ms .. 2147483647 ms)`.
    /// Wearing PostgreSQL's sentence here is not decoration: the answer a user needs is the pair
    /// of instants they *can* ask for, and that sentence is built to carry exactly that.
    pub fn admits(&self, ts: u64) -> Result<()> {
        if ts <= self.now && self.floor.is_none_or(|floor| ts >= floor) {
            return Ok(());
        }
        Err(SqlError::ParameterOutOfRange {
            value: render(ts),
            name: READ_AS_OF,
            low: self
                .floor
                .map_or_else(|| "the beginning".to_owned(), render),
            high: render(self.now),
        })
    }
}

/// `22023 invalid value for parameter "esker.read_as_of": "..."`, PostgreSQL's own sentence for a
/// `SET` it cannot read.
fn invalid_value(text: &str) -> SqlError {
    SqlError::InvalidParameterValue {
        name: READ_AS_OF,
        value: text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One millisecond, as a timestamp distance.
    const MS: u64 = 1 << TSO_LOGICAL_BITS;

    /// A plausible present: 2026-08-30 14:00:00+00 in Unix milliseconds.
    const NOW_MS: u64 = 1_787_493_600_000;

    #[test]
    fn an_interval_is_a_distance_back_from_now() {
        let now = ts_at_ms(NOW_MS);
        assert_eq!(resolve("-1h", now).unwrap(), now - 3_600_000 * MS);
        assert_eq!(resolve("-30m", now).unwrap(), now - 1_800_000 * MS);
        assert_eq!(resolve("-45s", now).unwrap(), now - 45_000 * MS);
        assert_eq!(resolve("-500ms", now).unwrap(), now - 500 * MS);
        assert_eq!(resolve("-2d", now).unwrap(), now - 172_800_000 * MS);
        assert_eq!(resolve("-1h30m", now).unwrap(), now - 5_400_000 * MS);
    }

    /// A user with an instant rather than a distance writes the instant, and it is the same
    /// datetime grammar every other value in this crate goes through.
    #[test]
    fn an_absolute_timestamp_resolves_to_the_first_ts_of_its_millisecond() {
        let ts = resolve("2026-08-30 14:00:00+00", ts_at_ms(u64::MAX >> 20)).unwrap();
        assert_eq!(ts & (MS - 1), 0, "the logical bits must be zero");
        assert_eq!(render(ts), "2026-08-30 14:00:00+00");
    }

    /// The two spellings are one feature: naming the same instant two ways reads the same data.
    #[test]
    fn an_interval_and_a_timestamp_naming_one_instant_agree() {
        let now = resolve("2026-08-30 15:00:00+00", u64::MAX >> 1).unwrap();
        let absolute = resolve("2026-08-30 14:00:00+00", now).unwrap();
        assert_eq!(resolve("-1h", now).unwrap(), absolute);
    }

    #[test]
    fn what_is_not_an_interval_and_not_a_timestamp_is_22023() {
        for text in [
            "",
            "  ",
            "zzz",
            "-1",
            "1h-",
            "-1x",
            "-1 hour",
            "yesterday",
            "-",
            "--1h",
        ] {
            let error = resolve(text, ts_at_ms(NOW_MS)).unwrap_err();
            assert_eq!(
                error.sqlstate(),
                crate::sqlstate::INVALID_PARAMETER_VALUE,
                "{text}"
            );
        }
    }

    /// PostgreSQL's `interval` grammar is a documented divergence, not a half-parser: `'-1 hour'`
    /// is refused rather than read as something near enough.
    #[test]
    fn postgresqls_interval_words_are_refused_rather_than_guessed_at() {
        let error = resolve("-1 hour", ts_at_ms(NOW_MS)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid value for parameter \"esker.read_as_of\": \"-1 hour\""
        );
    }

    #[test]
    fn a_token_round_trips() {
        let ts = ts_at_ms(1_756_000_000_000);
        assert_eq!(
            parse_snapshot_id(&token(ts)).unwrap(),
            SnapshotId::Timestamp(ts)
        );
    }

    /// PostgreSQL's own id shape is well formed and belongs to another server, so it is a name
    /// that will not be found — `42704`, which is what a real server answers for it too.
    #[test]
    fn a_postgresql_snapshot_id_is_a_name_that_will_not_be_found() {
        assert_eq!(
            parse_snapshot_id("00000003-0000001B-1").unwrap(),
            SnapshotId::Checkpoint("00000003-0000001B-1".to_owned())
        );
    }

    #[test]
    fn what_cannot_be_an_identifier_at_all_is_22023() {
        for id in ["", &"x".repeat(64)] {
            let error = parse_snapshot_id(id).unwrap_err();
            assert_eq!(error.sqlstate(), crate::sqlstate::INVALID_PARAMETER_VALUE);
        }
    }

    /// A token that is nearly one is a *name*, not a malformed token: the prefix is ours and a
    /// user may legitimately call a checkpoint `esker-something`.
    #[test]
    fn a_near_token_is_a_name() {
        assert_eq!(
            parse_snapshot_id("esker-nightly").unwrap(),
            SnapshotId::Checkpoint("esker-nightly".to_owned())
        );
    }

    /// There are no timestamps before the Unix epoch, so an interval that reaches past it is
    /// refused rather than clamped to zero — which would silently be a read of an empty database.
    #[test]
    fn an_interval_reaching_before_the_epoch_is_refused() {
        let error = resolve("-999999d", ts_at_ms(NOW_MS)).unwrap_err();
        assert_eq!(error.sqlstate(), crate::sqlstate::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn the_window_admits_what_retention_kept_and_refuses_the_rest() {
        let now = ts_at_ms(NOW_MS);
        let window = Window::new(now, 3_600_000);
        assert!(window.admits(now).is_ok());
        assert!(window.admits(now - 3_600_000 * MS).is_ok());
        assert!(window.admits(now - 3_600_001 * MS).is_err());
        assert!(window.admits(now + MS).is_err());
    }

    /// `RETENTION_FOREVER` is a sentinel, not a duration. Subtracting it underflows, and the rule
    /// it stands for is that nothing is ever collected — a window with no floor.
    #[test]
    fn forever_is_a_window_with_no_floor() {
        let now = ts_at_ms(NOW_MS);
        let window = Window::new(now, crate::catalog::RETENTION_FOREVER);
        assert_eq!(window.floor, None);
        assert!(window.admits(1).is_ok());
        assert!(
            window.admits(now + MS).is_err(),
            "the future is still the future"
        );
    }

    /// The refusal has to say how far back the user *can* ask, or it tells them nothing they did
    /// not already know.
    #[test]
    fn a_refusal_names_the_window() {
        let now = resolve("2026-08-30 15:00:00+00", u64::MAX >> 1).unwrap();
        let error = Window::new(now, 3_600_000)
            .admits(resolve("2020-01-01 00:00:00+00", now).unwrap())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "2020-01-01 00:00:00+00 is outside the valid range for parameter \
             \"esker.read_as_of\" (2026-08-30 14:00:00+00 .. 2026-08-30 15:00:00+00)"
        );
    }
}
