//! The six types phase 6a stores, and the text a client reads them as.
//!
//! This is contract C3's smallest surface and its sharpest one. A row that comes back with the
//! right rows and the wrong bytes in them is worse than an error, because nothing reports it: a
//! client that renders `true` where PostgreSQL renders `t`, or `1e-5` where PostgreSQL renders
//! `1e-05`, has silently disagreed with every other PostgreSQL client in the world.
//!
//! So none of the formats here were written from the documentation. Each one was put to a running
//! PostgreSQL 19beta1 and the answer recorded in `tests/corpus/pg19_values.txt`, which
//! `tests/value_parity.rs` replays against this module in both directions. Four of the rules below
//! came back different from what reading alone would have produced:
//!
//! * **A boolean on the wire is `t`, not `true`.** `true::text` really is `true` — PostgreSQL has
//!   a cast function that says so — but the *output function*, which is what fills a `DataRow`,
//!   writes one character. Capturing the cast instead of the value would have pinned the wrong
//!   format in a golden file.
//! * **`float8` switches to exponent notation outside `10^-4 .. 10^14`**, so `100000000000000`
//!   prints in full and `1e+15` does not, and the exponent carries a sign and at least two digits
//!   (`1e-05`). The boundary was measured, not guessed; it is not `%g`'s.
//! * **`int8` input accepts `1_000`, `0x1f`, `0o17` and `0b101`** — PostgreSQL 16 gave the input
//!   function the non-decimal integer literals the lexer had, and a client can send any of them.
//! * **`boolean` input accepts any unambiguous prefix**: `tr` is true and `ye` is true, `of` is
//!   false, and `o` is an error because it could be either `on` or `off`.
//!
//! # NULL is not a value here
//!
//! [`Datum::Null`] is a variant rather than an `Option<Datum>` wrapper because a column's value
//! and its absence travel together everywhere in this crate — in a row encoding's bitmap, in an
//! index key's leading byte, in a `DataRow`'s -1 length. Keeping them in one type is what lets a
//! single `match` be exhaustive over what a column can hold.

// `arith` documents itself with `//!`. An outer `///` here as well would be **concatenated** with
// that block and resolve its links in *this* module's scope, where `result_type` and `apply` are
// not — which is what broke `cargo doc -D warnings` the moment the two met.
pub mod arith;
/// `array_in` and `array_out`: an array literal read, and an array value printed.
pub mod array;
pub mod bit;
pub mod char_type;
pub mod composite;
pub mod date;
/// PostgreSQL's character-set names, for the two errors `convert_to` tells apart.
pub mod encoding;
pub(crate) mod float;
pub mod geometric;
pub mod hstore;
pub mod inet;
pub mod interval;
pub(crate) mod json;
pub mod ltree;
pub mod money;
pub mod numeric;
pub mod oid;
pub mod point;
/// Random bytes from the OS, and the version-4 UUID built from them.
pub mod random;
pub mod range;
pub mod reg_proc;
pub mod regex;
/// Arithmetic over the date and time types: which pairs have an operator, and what it yields.
pub mod stemmer;
pub mod stopwords;
pub mod temporal;
pub mod time;
mod timestamp;
pub mod trunc;
pub mod tsquery;
pub mod tsvector;
pub mod uuid;
/// Arrays as the catalog holds them: text, read by the operators (`vector::Array`).
pub mod vector;
pub mod xml;
pub mod zone;

use std::cmp::Ordering;

use crate::error::{Result, SqlError};

pub use esker_keys::value::{ColumnType, Datum, f64_of_sort_bits, sort_bits_of_f64};

/// One value's text **under the session's `IntervalStyle`**.
///
/// [`PgDatum::to_text`] is the output function under the boot style, which is what an index key,
/// an error message and a stored catalog default all want: none of them belongs to a session and
/// none of them may change when one runs a `SET`. This is the other half — what a **client** is
/// sent — and the two differ for exactly one type, so everything else forwards.
///
/// Every path that reaches a client goes through one of the two, and which one is not a detail:
/// `ActiveRecord` reads an interval by parsing the text, and returns `nil` rather than an error
/// when the parse fails (`tests/interval_style.rs`).
#[must_use]
pub fn to_text_under(value: &Datum, rendering: Rendering) -> Option<String> {
    use PgDatum as _;
    match value {
        // **A zoned instant is the one value whose text depends on the session's zone**, and it
        // depends on it whatever the `IntervalStyle` is, which is why this arm is above the
        // short-circuit below rather than inside it.
        Datum::TimestampTz(micros) => Some(timestamp::to_text_in(*micros, rendering.zone)),
        _ if rendering.interval_style == IntervalStyle::Postgres => value.to_text(),
        Datum::Interval {
            months,
            days,
            micros,
        } => Some(interval::to_text_under(
            &interval::Interval {
                months: *months,
                days: *days,
                micros: *micros,
            },
            rendering.interval_style,
        )),
        // An array of intervals prints its elements the same way, which is what `all_terms` is.
        Datum::Array(array) => Some(array::to_text_under(array, rendering)),
        _ => value.to_text(),
    }
}

/// Everything about a **session** that changes the text of a value.
///
/// Two members so far and the second is why this is a struct: `IntervalStyle` was threaded to the
/// cursor as an argument of its own, and `TimeZone` arriving behind it would have been a second
/// one at every call site. A third will be free.
///
/// `Copy`, which the `&'static` zone is what makes possible ([`zone::Zone::shared`]), and
/// `Default` — the boot session: the `postgres` interval style and UTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rendering {
    /// Which of the four dialects an `interval` prints in.
    pub interval_style: IntervalStyle,
    /// The session's zone, or `None` for UTC — which is both the boot value and what every path
    /// with **no** session uses, because an index key and an error message must not move when
    /// someone runs a `SET`.
    pub zone: Option<&'static zone::Zone>,
}
pub use interval::Style as IntervalStyle;
pub use timestamp::{MAX_MICROS, MIN_MICROS, NEG_INFINITY, POS_INFINITY};

/// The four bytes a varlena's header takes, which PostgreSQL adds to a declared length to make a
/// typmod. Its own `VARHDRSZ`.
///
/// Measured rather than read: on 19beta1 a `varchar(5)` column's `pg_attribute.atttypmod` is `9`
/// and a `character(3)`'s is `7`, while a `timestamp(3)`'s is `3` — the string types carry the
/// header and the time types do not.
const VARHDRSZ: i32 = 4;

/// No typmod: the number a column declared without one carries.
pub const NO_TYPMOD: i32 = -1;

/// A range's binary wire form, which is its canonical text.
fn binary_range(ty: ColumnType, bytes: &[u8]) -> Result<Datum> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SqlError::ProtocolViolation("a binary range is not UTF-8".into()))?;
    let subtype = range_subtype(ty);
    Ok(Datum::Range {
        subtype: Box::new(subtype),
        text: range::from_text(subtype, text)?.to_text(),
    })
}

/// The text-shaped types' binary wire form, which is the text itself.
///
/// A citext keeps its spelling and answers a [`Datum::Citext`], because the folding is the
/// comparison's and a comparison sees only values.
fn binary_text(ty: ColumnType, bytes: &[u8]) -> Result<Datum> {
    let text = String::from_utf8(bytes.to_vec()).map_err(|error| {
        let at = error.utf8_error().valid_up_to();
        SqlError::InvalidByteSequence(error.as_bytes().get(at).copied().unwrap_or(0))
    })?;
    Ok(match ty {
        ColumnType::Citext => Datum::Citext(text),
        ColumnType::Ltree => Datum::Ltree(ltree::from_text(&text)?),
        _ => Datum::Text(text),
    })
}

/// `hstore`'s oid, and its array's.
///
/// **Chosen in the user-oid range** (PostgreSQL's own fixed types stop below 16384), because that
/// is where a real server puts an extension's types: they are allocated at `CREATE EXTENSION` time
/// and differ per database, which is why `ActiveRecord` looks hstore up by `typname` and why
/// nothing may hard-code the number. Fixed here, and invisible to a client that reads it the way
/// the adapter does.
pub const HSTORE_OID: u32 = 16400;
/// See [`HSTORE_OID`].
pub const HSTORE_ARRAY_OID: u32 = 16401;
/// `citext`'s oid, and the array type this node does not build. See [`HSTORE_OID`].
pub const CITEXT_OID: u32 = 16402;
/// See [`CITEXT_OID`].
pub const CITEXT_ARRAY_OID: u32 = 16403;
/// `ltree`'s oid and its array's, in the same user range and for the same reason. See
/// [`HSTORE_OID`] — `ActiveRecord` finds this type by `typname` too.
pub const LTREE_OID: u32 = 16404;
/// See [`LTREE_OID`].
pub const LTREE_ARRAY_OID: u32 = 16405;
/// `lquery`'s oid. It has no array here — no column is a pattern — which is the same named gap
/// the six geometric shapes have.
pub const LQUERY_OID: u32 = 16406;
/// The subtype a range column's bounds are, which the column type names.
///
/// `int4range`'s is `Int8` and not `Int4`, because a bare integer constant is an `int8` here — the
/// standing constant-width trade, and the bound has to read as the type the literal makes.
#[must_use]
pub fn range_subtype(ty: ColumnType) -> ColumnType {
    esker_keys::row::range_subtype(ty)
}

/// `tsrange[]`'s oid — PostgreSQL's own `_tsrange`, which is built in and therefore fixed.
pub const TSRANGE_ARRAY_OID: u32 = 3909;

/// The typmod a declared **length** makes, for `varchar(n)` and `character(n)`.
///
/// The one place this arithmetic happens. `ColumnDef::typmod` holds PostgreSQL's number because
/// two wire surfaces are defined as it; nothing else in this crate should know that the number is
/// `n + 4`.
#[must_use]
pub fn typmod_of_length(length: u32) -> i32 {
    i32::try_from(length).unwrap_or(i32::MAX - VARHDRSZ) + VARHDRSZ
}

/// The declared length back out of a typmod, or `None` when there was none.
///
/// **Strictly greater than the header**, which is PostgreSQL's own rule and not an off-by-one:
/// `varchartypmodout` prints nothing at all for a typmod of `4`, so `format_type(1043, 4)` is
/// `character varying` and `format_type(1043, 5)` is `character varying(1)` (measured). A typmod
/// of exactly `4` means a length of zero, which no column can have — `varchar(0)` is `22023` on
/// input — so the only thing that can reach this branch is a hand-written `format_type`.
#[must_use]
pub fn length_of_typmod(typmod: i32) -> Option<u32> {
    if typmod <= VARHDRSZ {
        return None;
    }
    u32::try_from(typmod - VARHDRSZ).ok()
}

/// The typmod a declared **precision** makes, for `timestamp(p)`. It is `p` itself.
#[must_use]
pub fn typmod_of_precision(precision: u32) -> i32 {
    i32::try_from(precision).unwrap_or(i32::MAX)
}

/// The declared precision back out of a typmod, or `None` when there was none.
#[must_use]
pub fn precision_of_typmod(typmod: i32) -> Option<u32> {
    u32::try_from(typmod).ok()
}

/// The largest fractional-seconds precision PostgreSQL keeps, for every type that has one.
pub const MAX_TIME_PRECISION: u32 = 6;

/// Every interval field kept — the high half of a typmod written as `interval(p)`.
const INTERVAL_FULL_RANGE: i32 = 0x7FFF;

/// The low half of an interval typmod when **no** precision was written.
const INTERVAL_NO_PRECISION: i32 = 0xFFFF;

/// The typmod a declared precision makes for `interval(p)`, which is **not** the precision.
///
/// PostgreSQL packs `(range_mask << 16) | precision`, with `0xFFFF` in the low half meaning no
/// precision was written and `0x7FFF` in the high half meaning every field is kept. Measured on
/// 19beta1, one `format_type(1186, …)` at a time:
///
/// ```text
/// -1           interval                  2147418115   interval(3)     -- 0x7FFF0003
/// 2147418118   interval(6)               589823       interval day    -- 0x0008FFFF, a mask
/// 67698687     interval day to hour      3            ERROR:  invalid INTERVAL typmod: 0x3
/// ```
///
/// **That last line is why this arithmetic exists** rather than storing `p`: a bare precision is
/// not an interval typmod at all, and [`crate::catalog::ColumnDef::typmod`] is handed to clients
/// raw on two wire surfaces. The field mask is a different question and stays a declared
/// divergence — it says which fields a value *keeps*, which is semantics and not a width.
#[must_use]
pub fn interval_typmod_of_precision(precision: u32) -> i32 {
    let precision = i32::try_from(precision.min(MAX_TIME_PRECISION)).unwrap_or(0);
    (INTERVAL_FULL_RANGE << 16) | precision
}

/// The declared precision back out of an interval typmod, or `None` for one that has none.
///
/// `None` covers both a typmod of `-1` and a field mask like `interval day`, whose low half is
/// `0xFFFF`. A mask this node never stores can still arrive through a hand-written `format_type`.
#[must_use]
pub fn interval_precision_of_typmod(typmod: i32) -> Option<u32> {
    if typmod < 0 {
        return None;
    }
    let precision = typmod & INTERVAL_NO_PRECISION;
    if precision == INTERVAL_NO_PRECISION {
        return None;
    }
    u32::try_from(precision).ok()
}

/// [`fit_to_typmod`] for an **explicit cast**, where a string too long is truncated rather than
/// refused.
///
/// **The split this function exists for is measured, not assumed**: `'42'::varchar(1)` is `4` on
/// 19beta1 and writing `'42'` into a `varchar(1)` column is `22001`. `fit_to_typmod`'s own doc
/// already names the same split for `bit`, where the cast pads and truncates and the assignment
/// refuses; the string types are the other half of it, and this is where a cast asks.
///
/// Every other type is `fit_to_typmod`'s answer unchanged — a `numeric(10,2)` rounds the same way
/// whichever side asks, which is what that function's own comment says.
pub fn truncate_to_typmod(value: Datum, ty: ColumnType, typmod: i32) -> Result<Datum> {
    if typmod == NO_TYPMOD {
        return Ok(value);
    }
    let (Datum::Text(text), ColumnType::Varchar | ColumnType::Bpchar) = (&value, ty) else {
        return fit_to_typmod(value, ty, typmod);
    };
    let Some(limit) = length_of_typmod(typmod) else {
        return Ok(value);
    };
    // **Characters, not bytes**, which is what a length means here — the same count
    // `fit_to_typmod` uses to pad, so the two halves of one rule agree.
    let limit = limit as usize;
    let kept: String = text.chars().take(limit).collect();
    Ok(match ty {
        // A `char(n)` cast pads as well as truncates: `'abc'::char(5)` is `abc  `, measured.
        ColumnType::Bpchar => {
            let short_by = limit - kept.chars().count();
            let mut padded = kept;
            padded.extend(std::iter::repeat_n(' ', short_by));
            Datum::Text(padded)
        }
        _ => Datum::Text(kept),
    })
}

/// A `regclass` or `regtype` bound for an integer or `oid` column, as the number the column stores.
///
/// `Datum::fits` says a `regclass` is one representation with an `int8` and an `oid`, which is true
/// of the *comparison* and false of the *bytes*: the row codec writes the datum's own shape — the
/// number and then its name — into a column the catalog says holds the number alone, and the next
/// read of that row refuses it as corruption. So the name comes off here, on every write path
/// (`exec::assign::into_column` and `plan::Literal::assign` both), before any `fits` shortcut can
/// hand the datum through unchanged; `esker_keys::row::encode_row` refuses one that still carries
/// it, as the second line of defence. Any other datum passes untouched.
pub fn stored_shape(value: Datum, ty: ColumnType, rendering: Rendering) -> Result<Datum> {
    match (value, ty) {
        (
            Datum::RegClass { oid, .. },
            ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2 | ColumnType::Oid,
        ) => assignment_cast(Datum::Int8(oid), ty, rendering),
        (Datum::RegType { oid, .. } | Datum::RegProc { oid, .. }, ColumnType::Oid) => {
            Ok(Datum::Oid(oid))
        }
        (
            Datum::RegType { oid, .. } | Datum::RegProc { oid, .. },
            ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2,
        ) => assignment_cast(Datum::Int8(i64::from(oid)), ty, rendering),
        (value, _) => Ok(value),
    }
}

/// One value as another type's, in **assignment context** — PostgreSQL's assignment cast.
///
/// **Three callers, one rule.** A column `DEFAULT` (`exec::dml::assign_default`, which is where
/// this was written and where it stayed too long), an `INSERT`/`UPDATE` target list
/// (`exec::assign::coerce`), and a literal that already carries a type
/// (`plan::expr::Literal::assign`). The second and third reached a `42804` instead, so the same
/// value narrowed in a `DEFAULT` and was refused in a `SET` — two paths disagreeing is what said
/// the rule belonged in one place.
///
/// The expression's type is rarely the column's — `now()` is a `timestamptz` filling a `date`,
/// `concat` a `text` filling a `varchar` — and PostgreSQL coerces the default to the column when
/// the table is created, so a row never sees the difference.
///
/// **This does not decide *whether* a cast exists**; [`has_assignment_cast`] does, and every
/// caller that is not a `DEFAULT` must ask it first. The tail here is a text round trip through
/// the target type's input function, which would happily turn `'abc'` into a refusal-shaped
/// `22P02` where a real server answers `42804` before ever looking at the value.
///
/// **A timestamp to a date is not a text round trip.** Both are counts from 2000-01-01, so the
/// conversion is a division; going through text would print a zone offset that `date`'s input
/// function then has to re-parse, and would answer the wrong day for the last hours of one.
#[expect(
    clippy::cast_possible_truncation,
    reason = "`in_range` checks the bound first, which is what makes each cast exact"
)]
pub fn assignment_cast(value: Datum, ty: ColumnType, rendering: Rendering) -> Result<Datum> {
    if matches!(value, Datum::Null) || value.column_type() == Some(ty) {
        return Ok(value);
    }
    // **A reg\* type and an `oid` are one representation**, implicit in both directions
    // (`pg_cast` 24↔26 and 2206↔26, method `b`) — so the cast takes the oid rather than printing
    // the name and reading it back. Without this, `typinput::oid` rendered `boolin` and handed it
    // to `oidin`, which is `22P02 invalid input syntax for type oid: "boolin"` for a statement a
    // real server answers with 1242 (ADR 0098).
    match (&value, ty) {
        (Datum::RegType { oid, .. } | Datum::RegProc { oid, .. }, ColumnType::Oid) => {
            return Ok(Datum::Oid(*oid));
        }
        (Datum::Oid(oid), ColumnType::RegProc) => {
            return Ok(Datum::RegProc {
                oid: *oid,
                name: reg_proc::to_text(*oid).into_boxed_str(),
            });
        }
        (Datum::Oid(oid), ColumnType::RegType) => return Ok(regtype_of_oid(*oid)),
        _ => {}
    }
    // **Which calendar day an instant falls on is a question about a place**, so a `timestamptz`
    // is moved into the session's zone before the day is taken off it and a `timestamp` is not:
    // `'2011-01-01 23:30:00+00'` is the 1st in UTC and the 2nd in `Pacific/Auckland`, and a real
    // server stores the second one (`tests/captures/pg19_time_zone.txt`).
    if let (ColumnType::Date, Datum::Timestamp(micros) | Datum::TimestampTz(micros)) = (ty, &value)
    {
        let local = match (&value, rendering.zone) {
            (Datum::TimestampTz(_), Some(zone)) => {
                let unix = micros.div_euclid(1_000_000) + timestamp::PG_EPOCH_UNIX_SECONDS;
                micros.saturating_add(i64::from(zone.offset_at(unix).seconds) * 1_000_000)
            }
            _ => *micros,
        };
        return Ok(Datum::Date(
            i32::try_from(local.div_euclid(86_400_000_000)).unwrap_or(i32::MAX),
        ));
    }
    // **A float into an integer rounds, and it rounds half to *even*.** Measured on 19beta1:
    // `0.5` is `0`, `1.5` is `2`, `2.5` is `2`, `3.5` is `4`, and the negatives mirror it. That is
    // `rint`, which is what PostgreSQL's `dtoi4` calls — **not** the away-from-zero rounding a
    // `numeric` gets, where `0.5` is `1` and `2.5` is `3`. The two casts differ and the difference
    // is measurable in one statement, so they are written as two rules rather than one.
    //
    // Going through text instead was a wrong answer rather than a rounding difference: it refused
    // the row outright, which is how `random() * 100` into an `integer` column — statement 738's
    // own default — reported `22P02` where a real server stores a number.
    if let Datum::Double(_) | Datum::Real(_) = value
        && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    {
        let raw = match value {
            Datum::Double(raw) => raw,
            Datum::Real(raw) => f64::from(raw),
            _ => unreachable!("the pattern above admits only the two floats"),
        };
        let rounded = raw.round_ties_even();
        let out_of_range = || SqlError::IntegerLiteralOutOfRange(ty.name());
        // The bound is checked before the conversion, because casting an out-of-range `f64` to an
        // integer saturates in Rust and would store the limit where a real server raises `22003`.
        return match ty {
            ColumnType::Int2 => in_range(rounded, f64::from(i16::MIN), f64::from(i16::MAX))
                .map(|value| Datum::Int2(value as i16))
                .ok_or_else(out_of_range),
            ColumnType::Int4 => in_range(rounded, f64::from(i32::MIN), f64::from(i32::MAX))
                .map(|value| Datum::Int4(value as i32))
                .ok_or_else(out_of_range),
            _ => in_range(
                rounded,
                -9_223_372_036_854_775_808.0,
                9_223_372_036_854_775_807.0,
            )
            .map(|value| Datum::Int8(value as i64))
            .ok_or_else(out_of_range),
        };
    }
    // **A `numeric` into an integer rounds the other way**, half *away from zero* — the comment
    // above says so and nothing implemented it, so this fell to the text path and refused the row:
    // `22P02 invalid input syntax for type integer: "10.50"` for a value PostgreSQL stores as 11.
    // Measured in one session against the float rule beside it: `12.5::numeric` is **13** and
    // `-12.5::numeric` is **-13**, where `12.5::float8` is **12**.
    if let Datum::Numeric(number) = &value
        && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    {
        let rounded = numeric::round_half_away_to_integer(number);
        let out_of_range = || SqlError::IntegerLiteralOutOfRange(ty.name());
        return match ty {
            ColumnType::Int2 => i16::try_from(rounded)
                .map(Datum::Int2)
                .map_err(|_| out_of_range()),
            ColumnType::Int4 => i32::try_from(rounded)
                .map(Datum::Int4)
                .map_err(|_| out_of_range()),
            _ => Ok(Datum::Int8(rounded)),
        };
    }
    // **An integer into a narrower integer**, which is PostgreSQL's numeric assignment cast and
    // has the *short* `22003`. Without this the value left through `to_text` and came back through
    // the type's **input function** — a different path with a different sentence: this node said
    // `value "5000000000" is out of range for type integer` where a real server says
    // `integer out of range`. Measured on 19beta1: `SELECT 5000000000::integer`,
    // `INSERT INTO t4 SELECT 5000000000::bigint` and `UPDATE t4 SET a = 5000000000::bigint` are
    // all the short one, and only a *string* — `'5000000000'::integer` — gets the long one.
    //
    // A constant is narrowed in the planner and always had the right message; this is the path a
    // value takes when it is not a constant, which is where `ON UPDATE CASCADE` carries a
    // `bigserial` parent's key into an `integer` child.
    if let Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) = value {
        let wide = match value {
            Datum::Int2(v) => i64::from(v),
            Datum::Int4(v) => i64::from(v),
            Datum::Int8(v) => v,
            _ => unreachable!("the pattern above admits three variants"),
        };
        let out_of_range = || SqlError::IntegerLiteralOutOfRange(ty.name());
        match ty {
            ColumnType::Int2 => {
                return i16::try_from(wide)
                    .map(Datum::Int2)
                    .map_err(|_| out_of_range());
            }
            ColumnType::Int4 => {
                return i32::try_from(wide)
                    .map(Datum::Int4)
                    .map_err(|_| out_of_range());
            }
            ColumnType::Int8 => return Ok(Datum::Int8(wide)),
            _ => {}
        }
    }
    match value.to_text() {
        Some(text) => Datum::from_text(ty, &text),
        None => Ok(Datum::Null),
    }
}

/// A rounded float, if it is inside an integer type's range — NaN and the infinities are not.
fn in_range(value: f64, low: f64, high: f64) -> Option<f64> {
    (value >= low && value <= high).then_some(value)
}

/// Whether PostgreSQL has an **assignment cast** from `from` to `to`.
///
/// The authority is `pg_cast`, read off 19beta1 rather than reasoned about:
///
/// ```sql
/// SELECT castsource::regtype, casttarget::regtype FROM pg_cast WHERE castcontext = 'a'
/// ```
///
/// Two rules cover everything this node needs from those rows, and one of them is not in the table
/// at all:
///
/// * **The numeric family casts to itself in both directions.** Narrowing is `'a'` in `pg_cast`
///   (`bigint→integer`, `numeric→smallint`, `double precision→real`, …) and widening is `'i'`,
///   which assignment context also allows. Only the *value* can then fail, with `22003`.
/// * **Everything casts *to* a string type and nothing casts *from* one.** That pair is
///   PostgreSQL's I/O-conversion rule rather than a `pg_cast` row — there is no `integer→text`
///   entry — and the asymmetry is what keeps this from being "cast anything to anything".
///   Measured, in assignment context: `UPDATE t SET txt = i4` is accepted and
///   `UPDATE t SET i4 = txt` is `42804 … HINT: You will need to rewrite or cast the expression.`
/// * `json` and `jsonb` are `'a'` to each other, which is one row and one arm.
///
/// The datetime family is **not** here: `exec::assign::coerce` already carries it, arm by arm,
/// with the measurement that produced each one, and moving it would be a rewrite rather than this
/// change.
///
/// **What is deliberately absent**, because it is in `pg_cast` and was not measured end to end:
/// `money` (a target of `integer` and `numeric`, a source to `numeric` — its text carries a
/// currency symbol, so [`assignment_cast`]'s text tail is not obviously the conversion) and
/// `interval → time`. Both stay `42804`, named rather than guessed at.
#[must_use]
pub fn has_assignment_cast(from: Option<ColumnType>, to: ColumnType) -> bool {
    let Some(from) = from else {
        return false;
    };
    if is_string_type(to) {
        return true;
    }
    if is_string_type(from) {
        return false;
    }
    if is_numeric_type(from) && is_numeric_type(to) {
        return true;
    }
    matches!(
        (from, to),
        (ColumnType::Json, ColumnType::Jsonb)
            | (ColumnType::Jsonb, ColumnType::Json)
            // **A `regclass` or `regtype` into an integer or `oid` column** — `castcontext = 'a'` for
            // the integers and `'i'` for `oid` on a real server; `stored_shape` is what makes the
            // value (the number, its name gone).
            | (
                ColumnType::RegClass | ColumnType::RegType,
                ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Oid
            )
            // **An instant into a `date` column**, which a real server takes at `castcontext = 'a'`
            // — `INSERT INTO t (d) VALUES (now())` is the calendar day *here*. Refused until ADR
            // 0080, because taking it while every instant printed in UTC would have stored the
            // wrong day rather than refused one; the gate and the zone landed together for that
            // reason.
            | (ColumnType::Timestamp | ColumnType::TimestampTz, ColumnType::Date)
    )
}

/// PostgreSQL's numeric category, restricted to the types this node has.
///
/// `money` is in that category on a real server and is left out on purpose — see
/// [`has_assignment_cast`].
#[must_use]
fn is_numeric_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Numeric
            | ColumnType::Real
            | ColumnType::Double
    )
}

/// The string category: what everything casts to and nothing casts from.
#[must_use]
fn is_string_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar
    )
}

/// A value as the column's **typmod** requires it, or the error PostgreSQL raises instead.
///
/// The three types that take a number each do something different with it, which is the whole of
/// the unit ([ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md),
/// `tests/corpus/pg19_typmod.txt`):
///
/// * **`varchar(n)` refuses.** Longer is `22001`, on `INSERT` and `UPDATE` alike. Trailing spaces
///   count: `'abc '` is four characters and `v = 'abc '` finds no row that holds `'abc'`.
/// * **`character(n)` pads.** Shorter is padded with spaces to exactly `n`, and longer is the same
///   `22001`. Those are one rule, not two: a value stored padded makes plain byte comparison *be*
///   PostgreSQL's blank-insensitive comparison, so `c = 'x'`, `c = 'x  '` and `c = 'x    '` all
///   match a `char(3)` holding `x`, and an index key over it still holds "equal values encode
///   identically". That invariant is why `character(n)` waited for the typmod: without an `n`
///   there is nowhere to pad to.
/// * **`timestamp(p)` rounds**, half away from zero, and carries — `.999999` at `timestamp(3)` is
///   the next whole second. `timestamp::round_to_precision` holds the two surprises.
///
/// A NULL and a column with no typmod are returned untouched, which is every column this crate
/// had before version 4 of the catalog record.
pub fn fit_to_typmod(value: Datum, ty: ColumnType, typmod: i32) -> Result<Datum> {
    if typmod == NO_TYPMOD {
        return Ok(value);
    }
    Ok(match (&value, ty) {
        // **An array's typmod is its element's, applied element by element.** Every rule below
        // is a rule about one value, and an array is a container: `'{1.245}'::numeric(10,2)[]`
        // rounds the number inside it and a `character varying(3)[]` refuses the element that is
        // too long, naming the *element's* type in the `22001` the way a real server does.
        (Datum::Array(array), _) => {
            let Some(element_type) = esker_keys::array::ArrayValue::element_of(ty) else {
                return Ok(value);
            };
            let mut fitted = array.clone();
            for element in &mut fitted.values {
                if let Some(datum) = element.take() {
                    *element = Some(fit_to_typmod(datum, element_type, typmod)?);
                }
            }
            Datum::Array(fitted)
        }
        // **The cast's rule, which is not the assignment's**, and this function serves both — so
        // what is here is the one that answers where PostgreSQL answers. A *cast* pads on the
        // right and truncates in silence (`'101'::bit(8)` is `10100000`, `'101010101'::bit(4)` is
        // `1010`); an *assignment* refuses either way (`22026` for a `bit(n)`, `22001` for a
        // `bit varying(n)`). The refusals are declared in `tests/bit_string.rs` rather than
        // answered, because a refusal where a real server pads would be the worse of the two.
        (Datum::Bit { varying, bits }, ColumnType::Bit | ColumnType::VarBit) => Datum::Bit {
            varying: *varying,
            bits: bit::fit(bits, usize::try_from(typmod).ok(), *varying),
        },
        // The declared scale is applied here rather than at the parse, which is what makes one
        // rule serve the cast, the assignment and the `INSERT`: `1.245::numeric(10,2)` and a
        // `1.245` written into a `numeric(10,2)` column are the same rounding.
        (Datum::Numeric(value), ColumnType::Numeric) => {
            Datum::Numeric(numeric::fit_to_typmod(value.clone(), typmod)?)
        }
        (Datum::Text(text), ColumnType::Varchar) => {
            let Some(limit) = length_of_typmod(typmod) else {
                return Ok(value);
            };
            refuse_if_longer(text, limit, ty, typmod)?;
            value
        }
        (Datum::Text(text), ColumnType::Bpchar) => {
            let Some(limit) = length_of_typmod(typmod) else {
                return Ok(value);
            };
            refuse_if_longer(text, limit, ty, typmod)?;
            // Counted in **characters**, not bytes, which is what the length means: a `char(3)`
            // holding `'é'` is padded with two spaces and occupies four bytes.
            let short_by = limit as usize - text.chars().count();
            let mut padded = String::with_capacity(text.len() + short_by);
            padded.push_str(text);
            padded.extend(std::iter::repeat_n(' ', short_by));
            Datum::Text(padded)
        }
        // **`interval(p)` rounds its time-of-day part**, and only that part: `1 mon 2 days
        // 00:00:59.9999` at `interval(0)` is `1 mon 2 days 00:01:00` — the carry reaches minutes
        // and stops at days, because an interval's three fields never carry into one another.
        // Half away from zero and symmetric: `0.0005` at `interval(3)` is `0.001` and the negative
        // is `-0.001`. All measured.
        //
        // The precision comes out of a **packed** typmod — an interval's is
        // `(range_mask << 16) | precision`, so a bare number is not a valid one — which is why
        // this asks `interval_precision_of_typmod` and not `precision_of_typmod`.
        (
            Datum::Interval {
                months,
                days,
                micros,
            },
            ColumnType::Interval,
        ) => match interval_precision_of_typmod(typmod) {
            Some(precision) => Datum::Interval {
                months: *months,
                days: *days,
                micros: timestamp::round_to_precision(*micros, precision),
            },
            None => value,
        },
        (Datum::Timestamp(micros), ColumnType::Timestamp) => match precision_of_typmod(typmod) {
            Some(precision) => Datum::Timestamp(timestamp::round_to_precision(*micros, precision)),
            None => value,
        },
        // The same rounding one type over, and it may carry past the end of the day — which is a
        // value here, so there is nothing to clamp: `23:59:59.9999` at `time(3)` is `24:00:00`.
        (Datum::Time(micros), ColumnType::Time) => match precision_of_typmod(typmod) {
            Some(precision) => Datum::Time(time::round_to_precision(*micros, precision)),
            None => value,
        },
        (Datum::TimestampTz(micros), ColumnType::TimestampTz) => {
            match precision_of_typmod(typmod) {
                Some(precision) => {
                    Datum::TimestampTz(timestamp::round_to_precision(*micros, precision))
                }
                None => value,
            }
        }
        _ => value,
    })
}

/// `22001` if `text` is longer than `limit` characters, naming the type the way `format_type`
/// writes it — `character varying(5)`, not `varchar`, which is what the *declaration* errors use.
fn refuse_if_longer(text: &str, limit: u32, ty: ColumnType, typmod: i32) -> Result<()> {
    if text.chars().count() > limit as usize {
        return Err(SqlError::StringDataRightTruncation(format_type(ty, typmod)));
    }
    Ok(())
}

/// A datetime literal's body: the text with surrounding whitespace **and quote characters**
/// stripped.
///
/// **PostgreSQL's datetime input skips `'` and `"` and the number types do not**, and the
/// difference is what makes `range_test.rb` work on a real server: the file writes
/// `date_range: "[''2012-01-02'', ''2012-01-04'']"`, so `['2012-01-02', '2012-01-04']` reaches the
/// server and each bound arrives as `'2012-01-02'` — quotes included, because `'` is not the range
/// literal's quoting character. `date_in` reads it anyway. Measured, and each half separately:
///
/// | probe | oracle |
/// |---|---|
/// | `'''2012-01-02'''::date` | `2012-01-02` |
/// | `'"2012-01-02"'::date` | `2012-01-02` |
/// | `'''2012-01-02'::date` (one quote, unbalanced) | `2012-01-02` |
/// | `'''1'''::int4` | `22P02 invalid input syntax for type integer: "'1'"` |
/// | `'[''0.1'', ''0.2'']'::numrange` | `22P02 … for type numeric: "'0.1'"` |
///
/// So it is a **strip of any number of quote characters at either end**, not a matched pair: the
/// third row has one and no closing partner. `text` and `varchar` keep theirs, which is why this
/// is a datetime function and not a general one.
#[must_use]
pub(crate) fn datetime_body(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\'' || ch == '"')
}

/// Which of the six shapes a column type names, or `None` for a type that is not one.
///
/// The two vocabularies are deliberately separate: `esker_keys` carries the column type and knows
/// nothing about shapes, and `geometric::Kind` is what the reader and the writer are written
/// against. This is the one place they meet.
#[must_use]
pub fn geometric_kind(ty: ColumnType) -> Option<geometric::Kind> {
    Some(match ty {
        ColumnType::Lseg => geometric::Kind::Lseg,
        ColumnType::Box => geometric::Kind::Box,
        ColumnType::Path => geometric::Kind::Path,
        ColumnType::Polygon => geometric::Kind::Polygon,
        ColumnType::Circle => geometric::Kind::Circle,
        ColumnType::Line => geometric::Kind::Line,
        _ => return None,
    })
}

/// Whether the type has an equality **operator class** — what `DISTINCT` and `GROUP BY` need.
///
/// Not the same question as "does `=` answer": an `lseg` has an `=` operator and no btree family
/// to put it in, so `'…'::lseg = '…'::lseg` is `t` and `count(DISTINCT a_line_segment)` is
/// `42883 could not identify an equality operator for type lseg`. Measured on 19beta1 for every
/// member, and for `jsonb` — which is **not** one, since it has a btree opclass and groups fine.
///
/// One list, read by the three places that ask: `count(DISTINCT x)`, `SELECT DISTINCT` and
/// `GROUP BY`. It was three lists that disagreed — `count(DISTINCT json)` refused where
/// `SELECT DISTINCT json` answered — which is exactly the drift a shared predicate removes.
///
/// It is **not** the list `esker_sql::exec::query`'s `same_family` keeps, and the difference is
/// the sentence above: that one asks whether `=` answers, and the shapes are on this list and not
/// on that one. Sharing the two was tried and the geometric corpus refused it in one run.
#[must_use]
pub fn has_equality_operator(ty: ColumnType) -> bool {
    !matches!(
        ty,
        ColumnType::Json
            | ColumnType::JsonArray
            | ColumnType::Xml
            | ColumnType::XmlArray
            | ColumnType::Point
            | ColumnType::PointArray
            | ColumnType::BoxArray
            | ColumnType::LsegArray
            | ColumnType::PathArray
            | ColumnType::PolygonArray
            | ColumnType::CircleArray
            | ColumnType::LineArray
            | ColumnType::Lseg
            | ColumnType::Box
            | ColumnType::Path
            | ColumnType::Polygon
            | ColumnType::Circle
            | ColumnType::Line
    )
}

/// A type as `format_type` writes it, with its typmod: what an error message and `\gdesc` say.
#[must_use]
pub fn format_type(ty: ColumnType, typmod: i32) -> String {
    match (ty, typmod) {
        // **`bpchar` is the one type whose bare name is not its parameterised one.** Measured:
        // `format_type(1042, -1)` is `bpchar` and `format_type(1042, 7)` is `character(3)`, where
        // `varchar` is `character varying` either way. It is what `min(c)` reports, since an
        // aggregate carries no typmod.
        (ColumnType::Bpchar, NO_TYPMOD) => "bpchar".to_owned(),
        // **`"bit"`, quoted, and it is the *literal's* type and not a column's.** `bit` is a
        // reserved word, so a real server writes `format_type(1560, -1)` as `"bit"` — and the only
        // thing that reaches here with no typmod is a `B'…'` literal, whose length is the value's.
        // A bare `bit` **column** carries `atttypmod` 1 and prints `bit(1)` through the arm below;
        // `crate::parse::lower` is where the 1 is put on.
        (ColumnType::Bit, NO_TYPMOD) => "\"bit\"".to_owned(),
        (_, NO_TYPMOD) => ty.name().to_owned(),
        // **`character varying(255)[]`, not `character varying[](255)`.** The typmod is the
        // element's and prints inside the element's name, with the brackets after the whole of
        // it — which is what `ActiveRecord`'s schema dumper reads back to write
        // `t.string "tags", limit: 255, array: true`.
        _ if esker_keys::array::ArrayValue::element_of(ty).is_some() => {
            match esker_keys::array::ArrayValue::element_of(ty) {
                Some(element) => format!("{}[]", format_type(element, typmod)),
                None => ty.name().to_owned(),
            }
        }
        // The typmod **is** the length here, where a `varchar`'s is the length plus a varlena
        // header.
        (ColumnType::Bit, length) => format!("bit({length})"),
        (ColumnType::VarBit, length) => format!("bit varying({length})"),
        (ColumnType::Varchar | ColumnType::Bpchar, _) => match length_of_typmod(typmod) {
            Some(length) => format!("{}({length})", ty.name()),
            None => ty.name().to_owned(),
        },
        // `timestamp(3) without time zone`, with the precision *inside* the name — which is why
        // this is not a suffix on `name()`.
        (ColumnType::Timestamp, _) => match precision_of_typmod(typmod) {
            Some(precision) => format!("timestamp({precision}) without time zone"),
            None => ty.name().to_owned(),
        },
        (ColumnType::TimestampTz, _) => match precision_of_typmod(typmod) {
            Some(precision) => format!("timestamp({precision}) with time zone"),
            None => ty.name().to_owned(),
        },
        // `time(3) without time zone`, the same shape one type over. Measured, including
        // `format_type(1083, 0)`, which is `time(0) without time zone` and not the bare name.
        (ColumnType::Time, _) => match precision_of_typmod(typmod) {
            Some(precision) => format!("time({precision}) without time zone"),
            None => ty.name().to_owned(),
        },
        // `interval(3)` — a suffix, unlike `timestamp`'s, and read out of a packed typmod
        // rather than off the number itself ([`interval_typmod_of_precision`]).
        (ColumnType::Interval, _) => match interval_precision_of_typmod(typmod) {
            Some(precision) => format!("interval({precision})"),
            None => ty.name().to_owned(),
        },
        // `numeric(10,2)`, and `numeric(11,-2)` — the scale is signed and prints signed.
        (ColumnType::Numeric, _) => numeric::format_typmod(typmod),
        _ => ty.name().to_owned(),
    }
}

/// A type name as written, split into its optional schema and the name itself.
///
/// **One grammar, one parser.** The same three-line rule decides `'public.mood'::regtype`,
/// `'public.mood'::text` and a `CREATE TABLE` column type, and it was three readers before this:
/// a `regtype` over a built-in stripped nothing, a `regtype` over a user type compared the whole
/// string against a bare name, and a cast did the same.
///
/// Measured on 19beta1, one spelling at a time:
///
/// * `public.mood`, `public."mood"`, `"public"."mood"` and ` public . mood ` all resolve. Space
///   around the dot is not part of either identifier;
/// * **`"public.mood"` does not**: the quotes make it *one* identifier holding a dot, so it is a
///   type nobody declared and a real server quotes the whole of it back;
/// * an unquoted part folds to lower case and a quoted one does not — `'"MOOD"'::regtype` is
///   `42704 type "MOOD" does not exist` where `'MOOD'::regtype` resolves;
/// * `""` inside a quoted part is one `"`.
///
/// The parts come back **already folded or already verbatim**, so what a caller compares and what
/// it quotes in an error are the same string.
#[must_use]
pub fn split_type_name(spelled: &str) -> (Option<String>, String) {
    let (schema, name, _) = split_type_name_parts(spelled);
    (schema, name)
}

/// [`split_type_name`], plus whether the name may use PostgreSQL's **SQL grammar**.
///
/// The third answer is the one nothing about the first two suggests, and it is measured:
///
/// | | |
/// |---|---|
/// | `'integer'::regtype` | `integer` |
/// | `'"integer"'::regtype` | `42704 type "integer" does not exist` |
/// | `'pg_catalog.integer'::regtype` | `42704 type "pg_catalog.integer" does not exist` |
/// | `'pg_catalog.int4'::regtype` | `integer` |
/// | `'"character varying"'::regtype` | `42704 …`, where `'character varying'` resolves |
/// | `'pg_catalog.character varying'` | `42601 syntax error at or near "varying"` |
///
/// **A bare unquoted name is read by the SQL grammar; a quoted or qualified one is an identifier**
/// and is looked up in `pg_type.typname` alone. `integer` and `character varying` are names only
/// the grammar has — `pg_type` holds `int4` and `varchar` — so the moment a name stops being
/// grammar and becomes an identifier, they stop resolving. A typmod and a `[]` still apply either
/// way: `'pg_catalog.varchar(255)'` and `'"int4"[]'` are both fine.
fn split_type_name_parts(spelled: &str) -> (Option<String>, String, bool) {
    let chars: Vec<char> = spelled.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut part = String::new();
    let mut quoted = false;
    // Whether a quote appeared anywhere: a quoted name is an identifier, not SQL grammar.
    let mut any_quote = false;
    let mut at = 0;
    let mut depth = 0usize;
    while at < chars.len() {
        let ch = chars[at];
        match ch {
            '"' if depth == 0 => {
                // `""` inside a quoted run is one quote; anywhere else it opens or closes one.
                if quoted && chars.get(at + 1) == Some(&'"') {
                    part.push('"');
                    at += 2;
                    continue;
                }
                // The quotes are not the part; what is inside them is, verbatim. A part is
                // pushed on the dot or at the end, so `"A"b` is the one name `Ab` — PostgreSQL
                // folds only the unquoted half of a mixed identifier.
                quoted = !quoted;
                any_quote = true;
            }
            // A typmod's parentheses hide their contents: `numeric(10,2)` has no schema in it.
            '(' if !quoted => {
                depth += 1;
                part.push(ch);
            }
            ')' if !quoted => {
                depth = depth.saturating_sub(1);
                part.push(ch);
            }
            '.' if !quoted && depth == 0 => parts.push(std::mem::take(&mut part)),
            _ if quoted => part.push(ch),
            // Unquoted: folded, and the space around a dot is not part of the name.
            _ => part.extend(ch.to_lowercase()),
        }
        at += 1;
    }
    parts.push(part);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .collect();
    match parts.as_slice() {
        [name] => (None, name.clone(), !any_quote),
        [schema, name] => (Some(schema.clone()), name.clone(), false),
        // Three or more is not a type name anywhere, and the whole of it is what a real server
        // would quote back — so it is handed on as a name nothing resolves.
        _ => (None, parts.join("."), false),
    }
}

/// The type a name means, under **every spelling PostgreSQL accepts for it**.
///
/// What `'x'::regtype` resolves, and what `pg_typeof` would answer. Three rules, all measured
/// against 19beta1 rather than assumed:
///
/// * **Case and surrounding space do not matter.** `'INTEGER'::regtype::oid` and
///   `' integer '::regtype::oid` are both `23`.
/// * **A typmod is parsed, *validated*, and then thrown away.** `'character varying(255)'` is
///   `1043` and `'numeric(10,2)'` is `1700` — the *type* is what a `regtype` names — but
///   `'numeric(1001,0)'` is `22023 NUMERIC precision 1001 must be between 1 and 1000` and
///   `'character varying(0)'` is `22023 length for type varchar must be at least 1`. Discarding
///   the number without reading it answered where a real server raises, which is the divergence
///   class ADR 0031 counts worst.
/// * **Every type answers to both of its names**, because PostgreSQL keeps two: the SQL name a
///   column is declared and complained about with (`integer`, `character varying`) and the
///   internal `pg_type.typname` (`int4`, `varchar`).
///
/// # This resolves from `ColumnType::ALL`, on purpose
///
/// It used to be a hand-written table of strings, and it drifted: `numeric` and `date` were in
/// `pg_type` — which derives itself — and missing here, so `'decimal(3,2)'::regtype` was `42704`
/// for a type the same node would happily create a column of. Every Rails suite file stopped on
/// that line. Deriving the names from the same array `pg_type` uses means a new type is reachable
/// here the moment it exists, and the only hand-written part left is `ALIASES` — the handful of
/// spellings that are neither of a type's two names.
pub fn named_type(spelled: &str) -> Result<Option<Named>> {
    let (schema, bare, sql_grammar) = split_type_name_parts(spelled);
    // **A schema this module does not know is not an error here**, and that it was is the half of
    // the qualified-name unit that got left behind: `schema_1.text` is a `CREATE DOMAIN` in a
    // schema a user made, and this function has no catalog to ask. So a name it cannot resolve is
    // `None`, and the *executor* decides between "no such schema" (`3F000`) and "no such type"
    // (`42704`) — it is the only place that can tell them apart. Only the three schemas a
    // built-in type can live in are answered here.
    if let Some(schema) = &schema
        && !matches!(
            schema.as_str(),
            "public" | "pg_catalog" | "information_schema"
        )
    {
        return Ok(None);
    }
    let lowered = bare.trim().to_ascii_lowercase();
    // **Dimensions are ignored and the internal name works.** `integer[]`, `integer[][]`,
    // `integer[3]` and `_int4` are all 1007 on a real server — an array's *shape* is not part of
    // its type — so every one of those spellings reduces to the element name here.
    let (element, is_array) = match lowered.split_once('[') {
        Some((head, tail))
            if tail
                .chars()
                .all(|c| c.is_ascii_digit() || c == '[' || c == ']') =>
        {
            (head.trim_end().to_owned(), true)
        }
        Some(_) => (lowered.clone(), false),
        None => match lowered.strip_prefix('_') {
            Some(rest) if !rest.is_empty() => (rest.to_owned(), true),
            _ => (lowered.clone(), false),
        },
    };
    if is_array {
        // A typmod on an array name is refused by the same rule the scalar is, so this goes
        // through the ordinary resolution and wraps whatever it finds.
        return Ok(named_by_grammar(&element, sql_grammar)?.map(Named::Array));
    }
    named_by_grammar(&lowered, sql_grammar).map(|found| found.map(Named::Scalar))
}

/// The type `pg_type.typname` holds under this name, with the SQL grammar **out** of scope.
///
/// What a *quoted* type name means, which is not always what the bare one means: `"char"` is oid 18
/// and bare `char` is `bpchar`. `ObjectName`'s own `Display` drops the quote style, so the caller
/// that has the identifier has to say which it saw — this is the half of [`type_by_name`] for the
/// quoted one.
#[must_use]
pub fn internal_type_by_name(bare: &str) -> Option<ColumnType> {
    resolve_type_name(&bare.trim().to_ascii_lowercase(), false)
}

/// The type a name means, ignoring arrays. See [`named_type`] for the whole answer.
pub fn type_by_name(spelled: &str) -> Result<Option<ColumnType>> {
    // **The quoting decides the grammar here as it does in `named_type`**, and it did not before:
    // this forced `sql_grammar` on, so a quoted name was read as SQL. The two entry points
    // disagreeing was invisible until `"char"` arrived — the one name where the quoted and the
    // bare spelling are *different types*, `char` being `bpchar` and `"char"` being oid 18.
    let (_, bare, sql_grammar) = split_type_name_parts(spelled);
    named_by_grammar(&bare, sql_grammar)
}

/// [`type_by_name`], told whether PostgreSQL's SQL names are in scope — see
/// [`split_type_name_parts`], which is where that question is decided.
fn named_by_grammar(spelled: &str, sql_grammar: bool) -> Result<Option<ColumnType>> {
    let lowered = spelled.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return Err(SqlError::InvalidTypeName(String::new()));
    }
    let (bare, arguments) = match (lowered.find('('), lowered.rfind(')')) {
        // `numeric(10,2)` -> `numeric` + `10,2`; `timestamp(6) without time zone` keeps its tail,
        // because the words after the parentheses are part of the name.
        (Some(open), Some(close)) if open < close => (
            format!("{}{}", &lowered[..open], &lowered[close + 1..]),
            Some(lowered[open + 1..close].to_owned()),
        ),
        _ => (lowered, None),
    };
    let bare = bare.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(ty) = resolve_type_name(&bare, sql_grammar) else {
        return Ok(None);
    };
    let Some(arguments) = arguments else {
        return Ok(Some(ty));
    };
    // **A typmod is only legal on a type that takes one.** `'json(10)'::regtype` is `42601 syntax
    // error at or near "("` on a real server — its *parser* refuses it, the way it refuses
    // `integer(4)` — and this node answers `42704` for the whole spelling instead. Both refuse;
    // the codes differ, which `tests/regtype.rs` declares.
    if !takes_typmod(ty) {
        // **Which `42601` you get is the grammar's choice, not the type's.** A spelling
        // PostgreSQL has a keyword for is a syntax error at the parenthesis, because its parser
        // never gets to a type; every other spelling parses as an identifier with a modifier and
        // is rejected by name. Measured for all seventeen spellings this node has.
        return Err(if TYPE_KEYWORDS.contains(&bare.as_str()) {
            SqlError::TypeNameSyntax("(".to_owned())
        } else {
            SqlError::TypeModifierNotAllowed(bare)
        });
    }
    validate_typmod(ty, &arguments)?;
    Ok(Some(ty))
}

/// PostgreSQL's ceiling on a declared string length, and its floor is one.
///
/// Measured on 19beta1, both ends: `varchar(10485761)` is `22023 length for type varchar cannot
/// exceed 10485760` and `varchar(0)` is `22023 length for type varchar must be at least 1`. Zero
/// is **not** legal, which is the one a reader would guess wrong — a `varchar(0)` holding only the
/// empty string is a perfectly coherent type and PostgreSQL declines to have it.
///
/// Here rather than in `parse::lower` because a `regtype` name and a column declaration have to
/// agree about it: `'character varying(0)'::regtype` raises the same `22023` that
/// `CREATE TABLE t (v varchar(0))` does.
pub const MAX_TYPE_LENGTH: u32 = 10_485_760;

/// The type an OID names: the inverse of `'x'::regtype`, for an oid read per row.
///
/// A scan of `ColumnType::ALL` rather than a table beside it, for the same reason `pg_type`'s rows
/// are derived from that list — a type added to this node cannot be left out of the answer.
#[must_use]
pub fn type_by_oid(oid: u32) -> Option<ColumnType> {
    ColumnType::ALL.into_iter().find(|ty| ty.oid() == oid)
}

/// The `regtype` an oid names, printing as the type's name or — for an oid that is no type — as
/// its own digits.
///
/// **Not an error for an unknown oid**, which is measured and is the asymmetry worth remembering:
/// `999999::regtype` is `999999` on a real server, where `'nosuchtype'::regtype` is `42704`. The
/// name is the *output* function and an oid is always a legal input to it
/// ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
#[must_use]
pub fn regclass_of_oid(oid: i64) -> Datum {
    // **The digits, because this layer has no catalog** (invariant 7) — and because the digits are
    // what a real server prints for an oid that names no relation: `999999::regclass` is `999999`.
    // A cast that *does* resolve a name goes through `CatalogFunc::RegClass`, where the catalog is,
    // and puts the name on the datum there.
    Datum::RegClass {
        oid,
        name: oid.to_string().into(),
    }
}

/// A `regtype` from an oid, with the name this node's own type table gives it.
pub fn regtype_of_oid(oid: u32) -> Datum {
    let name = type_by_oid(oid).map_or_else(|| oid.to_string(), |ty| ty.name().to_owned());
    Datum::RegType {
        oid,
        name: name.into(),
    }
}

/// A type name, which may name an **array** of a type this node has.
///
/// **Every array is storable now** — each of them is a `ColumnType`, because a `typarray` naming a
/// `pg_type` row that is not there is what left `ActiveRecord` unable to quote an array at all. So
/// `Named::Array(ty)` and `Named::Scalar(array_of(ty))` are two spellings of one type, and the
/// variant is kept because a *name* can be written either way: `'decimal[]'::regtype` and
/// `'_numeric'::regtype` reach it from opposite directions, and `numeric[][]` and `numeric[3]` are
/// both `numeric[]` — the dimensions in a name are not part of the type on a real server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// The type itself.
    Scalar(ColumnType),
    /// An array **of** that type: `numeric[]`, `_numeric`, `numeric[3]`, `numeric[][]`.
    Array(ColumnType),
}

impl Named {
    /// The OID a `regtype` answers for it.
    #[must_use]
    pub fn oid(self) -> u32 {
        match self {
            Named::Scalar(ty) => ty.oid(),
            Named::Array(ty) => array_oid(ty),
        }
    }

    /// The name a `regtype` prints it by: `integer[]`, not `_int4`.
    #[must_use]
    pub fn printed(self) -> String {
        match self {
            Named::Scalar(ty) => format_type(ty, NO_TYPMOD),
            Named::Array(ty) => format!("{}[]", format_type(ty, NO_TYPMOD)),
        }
    }

    /// The element type, whether or not this is an array.
    #[must_use]
    pub fn element(self) -> ColumnType {
        match self {
            Named::Scalar(ty) | Named::Array(ty) => ty,
        }
    }

    /// Whether it names an array.
    #[must_use]
    pub fn is_array(self) -> bool {
        matches!(self, Named::Array(_))
    }
}

/// A value cut to what a `name` holds: **63 bytes**, on a character boundary.
///
/// `NAMEDATALEN` is 64 and the last byte is C's terminator, so 63 is the limit — the off-by-one a
/// reader expects to be 64. The cut is by *bytes* and never through the middle of a character:
/// `repeat('é',64)` is 31 characters and 62 octets rather than 31 and a half, measured. Truncation
/// and not refusal is the type's own rule; a value too long for a `varchar(n)` is `22001` where
/// this one is simply shorter.
#[must_use]
pub fn truncate_to_name(text: &str) -> String {
    const LIMIT: usize = 63;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    // The last boundary at or before the limit, which is what stops a character being halved.
    let mut end = LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The OID of the array type PostgreSQL pairs with `ty`, from `pg_type.typarray`.
///
/// An exhaustive match, measured one row at a time off a real server, so that **a type added to
/// `ColumnType` has to answer this** rather than leaving its array name unresolvable. That is the
/// whole reason it is a match and not a lookup beside the scalar table: the scalar names already
/// derive from `ColumnType::ALL`, and this is what keeps the array half from drifting away from
/// them the way `type_by_name` once drifted from `pg_type`.
///
/// The numbers are not derivable — `_int4` is 1007 and `_int8` is 1016, out of order with their
/// element types, and `_json` is 199 where `json` is 114 — so each is a measurement.
#[must_use]
pub fn array_oid(ty: ColumnType) -> u32 {
    match ty {
        // `regtype` is 2206 and `_regtype` is 2211.
        ColumnType::RegType => 2211,
        // And `regproc` is 24 with `_regproc` 1008 — measured, and not adjacent the way the
        // `regtype` pair is: `regproc` is one of the oldest oids and its array is a later one.
        ColumnType::RegProc => 1008,
        // **`_name` is 1003**, measured. It arrived when run 106 lost ten tests to its absence:
        // `array_agg(enum.enumlabel)` over a `name` column is a `name[]` on a real server, and
        // without the type this node answered a scalar `text` and `ActiveRecord` kept the literal
        // as a string.
        ColumnType::Name => 1003,
        // `_char`, measured beside it.
        ColumnType::Char => 1002,
        // No `_regclass` here: an array of a regclass is not a type this node offers, so the
        // link is a zero rather than a pointer at a `pg_type` row that is not there.
        // Neither a `regclass` nor either vector has an array type on a real server.
        // There is no array of an array: an array type is a constructor over a *scalar* here, so
        // asking for one has no answer and `0` is `InvalidOid`, which is what a real server's
        // `typarray` holds for a type that has no array.
        ColumnType::RegClass
        // **`typarray` is 0 for a pseudo-type**, measured: there is no `_void`.
        | ColumnType::Void
        | ColumnType::Int2Vector
        | ColumnType::OidVector
        | ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::HstoreArray
        | ColumnType::TsVectorArray
        | ColumnType::TsQueryArray
        | ColumnType::TsRangeArray
        | ColumnType::BoolArray
        | ColumnType::ByteaArray
        | ColumnType::BpcharArray
        | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray
        | ColumnType::DateArray
        | ColumnType::TimeArray
        | ColumnType::TimestampArray
        | ColumnType::TimestampTzArray
        | ColumnType::IntervalArray
        | ColumnType::RealArray
        | ColumnType::DoubleArray
        | ColumnType::UuidArray
        | ColumnType::JsonArray
        | ColumnType::JsonbArray
        | ColumnType::OidArray
        | ColumnType::RegTypeArray
        | ColumnType::RegProcArray
        | ColumnType::CitextArray
        | ColumnType::TstzRangeArray
        | ColumnType::Int4RangeArray
        | ColumnType::DateRangeArray
        | ColumnType::NumRangeArray
        | ColumnType::Int8RangeArray
        | ColumnType::PointArray
        | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray
        | ColumnType::MoneyArray
        | ColumnType::InetArray
        | ColumnType::CidrArray
        | ColumnType::MacAddrArray
        | ColumnType::BitArray
        | ColumnType::VarBitArray
        // **And a user range**, which has no array type here: a real server builds `_floatrange`
        // with the type and `range_test.rb` never declares a column of one, so this is a named
        // gap rather than a guess at an oid that is allocated per database anyway.
        | ColumnType::FloatRange
        | ColumnType::VarcharRange
        | ColumnType::XmlArray
        | ColumnType::LtreeArray
        | ColumnType::LQuery => 0,
        // **Every range type has its array now**, which is what run 58 was: `range_test.rb`
        // declares two range arrays and an array type is built per element type, so three of the
        // four left its 46 tests exactly where they were. The oids are PostgreSQL's own and each
        // is the type's plus one — measured, all six.
        ColumnType::TstzRange => 3911,
        ColumnType::Int4Range => 3905,
        ColumnType::DateRange => 3913,
        ColumnType::NumRange => 3907,
        ColumnType::Int8Range => 3927,
        // `_money`, measured beside the type it is over.
        ColumnType::Money => 791,
        // `_inet`, `_cidr`, `_macaddr` — measured beside theirs, and out of order with the
        // element types the way `_int4` and `_int8` are.
        ColumnType::Inet => 1041,
        ColumnType::Cidr => 651,
        ColumnType::MacAddr => 1040,
        ColumnType::Bit => 1561,
        ColumnType::VarBit => 1563,
        ColumnType::Point => 1017,
        // `_box`, whose delimiter is `;` — the one array type in `pg_type` where it is not a comma.
        ColumnType::Box => 1020,
        // **The other five shapes, measured one row at a time**: `_lseg` and `_path` are adjacent
        // and `_circle` and `_line` sit beside their own element types rather than with them.
        ColumnType::Lseg => 1018,
        ColumnType::Path => 1019,
        ColumnType::Polygon => 1027,
        ColumnType::Circle => 719,
        ColumnType::Line => 629,
        ColumnType::Bool => 1000,
        ColumnType::Bytea => 1001,
        ColumnType::Int8 => 1016,
        ColumnType::Int2 => 1005,
        ColumnType::Int4 => 1007,
        ColumnType::Text => 1009,
        ColumnType::Oid => 1028,
        ColumnType::Json => 199,
        // Measured: `xml` is 142 and `_xml` is 143, adjacent where `json`'s pair is not.
        ColumnType::Xml => 143,
        ColumnType::Real => 1021,
        ColumnType::Double => 1022,
        ColumnType::Bpchar => 1014,
        ColumnType::Varchar => 1015,
        ColumnType::Date => 1182,
        ColumnType::Time => 1183,
        ColumnType::Timestamp => 1115,
        ColumnType::TimestampTz => 1185,
        ColumnType::Interval => 1187,
        ColumnType::Numeric => 1231,
        ColumnType::Uuid => 2951,
        // **In the user-oid range on purpose.** A real server allocates an extension's types when
        // it installs them, so their oids are above 16384 and differ per database; fixed ones here
        // are invisible to a client that reads them the way `ActiveRecord` does — by `typname`.
        ColumnType::Hstore => HSTORE_ARRAY_OID,
        ColumnType::TsVector => 3643,
        ColumnType::TsQuery => 3645,
        // **No `citext[]` here.** `citext_test.rb` never declares one and a real server's
        // `typarray` is non-zero, which the corpus reads as a boolean rather than a number — so
        // `0` would be a wrong answer there. `CITEXT_ARRAY_OID` is reserved and reported, and the
        // array type itself is not built.
        ColumnType::Citext => CITEXT_ARRAY_OID,
        ColumnType::Ltree => LTREE_ARRAY_OID,
        ColumnType::TsRange => TSRANGE_ARRAY_OID,
        ColumnType::Jsonb => 3807,
    }
}

/// The spellings PostgreSQL's grammar has a **keyword** for, among the types this node has.
///
/// Only reachable for a type that takes no typmod, and only to choose between its two `42601`s:
/// `integer(4)` is `syntax error at or near "("` and `int4(4)` is `type modifier is not allowed
/// for type "int4"`. Measured, one spelling at a time — it is not derivable from a type's two
/// names, because `json` is a keyword and `text` and `date` are not while all three spell
/// themselves identically in both.
const TYPE_KEYWORDS: [&str; 7] = [
    "boolean",
    "integer",
    "bigint",
    "smallint",
    "real",
    "double precision",
    "json",
];

/// The spellings that are neither a type's SQL name nor its `pg_type.typname`.
///
/// Everything else comes from `ColumnType::ALL`. Keep this list short: an entry here is a name
/// that cannot be derived, and a type added to the array needs one only if PostgreSQL gives it a
/// third spelling.
const ALIASES: [(&str, ColumnType); 4] = [
    ("int", ColumnType::Int4),
    // `float` with no precision is `float8` on a real server, not `float4`.
    ("float", ColumnType::Double),
    // `decimal` is `numeric`'s standard name and PostgreSQL's own alias for it: all four
    // spellings — `numeric`, `decimal`, and either with a typmod — resolve to 1700.
    ("decimal", ColumnType::Numeric),
    ("char", ColumnType::Bpchar),
];

/// A bare, normalised type name as one of this node's types.
fn resolve_type_name(name: &str, sql_grammar: bool) -> Option<ColumnType> {
    // **`typname` is always in scope and the SQL names are not.** A quoted or schema-qualified
    // name is an identifier, and `integer` is a name only the grammar has — `pg_type` holds
    // `int4`. See [`split_type_name_parts`] for the six spellings that settle it.
    let internal = ColumnType::ALL
        .into_iter()
        .find(|ty| crate::catalog::pg_catalog::typname(*ty) == name);
    if !sql_grammar {
        return internal;
    }
    // **`char` unquoted is `bpchar`, and it is the one name where the grammar beats `typname`.**
    // `pg_type` holds `char` for oid 18 and the SQL grammar spells that type `"char"` *with the
    // quotes*; bare `char` is `character(1)`. Measured: `'char'::regtype::oid` is **1042** and
    // `'"char"'::regtype::oid` is 18 — the quoted spelling is an identifier, which is the branch
    // above. Without this line the internal lookup won and a `regtype` corpus that had agreed for
    // fifteen ADRs started answering 18.
    if name == "char" {
        return Some(ColumnType::Bpchar);
    }
    internal
        .or_else(|| ColumnType::ALL.into_iter().find(|ty| ty.name() == name))
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == name)
                .map(|(_, ty)| *ty)
        })
}

/// Whether a type takes a typmod at all.
///
/// An exhaustive match rather than a `matches!` list, so that a type added to `ColumnType` has to
/// answer this question instead of silently inheriting "no".
fn takes_typmod(ty: ColumnType) -> bool {
    // **An array takes exactly its element's typmod**, because that is whose it is:
    // `character varying(255)[]` bounds each string and `numeric(10,2)[]` rounds each number.
    if let Some(element) = esker_keys::array::ArrayValue::element_of(ty) {
        return takes_typmod(element);
    }
    match ty {
        ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Timestamp
        | ColumnType::TimestampTz
        | ColumnType::Time
        // **It takes one and this node drops it.** `'interval(9)'::regtype` is 1186 on a real
        // server — the same silent tolerance `timestamp(9)` has — and the typmod itself is a
        // *bitmask*, fields in the high bits and precision in the low, not a plain number. The
        // name resolves; what the mask would restrict is declared in `tests/interval.rs`.
        | ColumnType::Interval
        // **The length is a typmod**, which is what `character_maximum_length` reports and what
        // the schema dumper writes as `limit: 8`.
        | ColumnType::Bit
        | ColumnType::VarBit
        | ColumnType::Numeric => true,
        // **A `money` has scale 2 and does not take one.** `information_schema` reports both
        // `numeric_precision` and `numeric_scale` as NULL for a money column, measured — the
        // `scale: 2` `ActiveRecord`'s schema dumper prints is the adapter's own constant.
        ColumnType::Lseg
        | ColumnType::Box
        | ColumnType::Path
        | ColumnType::Polygon
        | ColumnType::Circle
        | ColumnType::Line
        // `xml` takes no typmod: there is no `xml(n)`, and `XMLSERIALIZE`'s type modifiers are a
        // function's arguments rather than the type's.
        | ColumnType::Xml
        // Nor does `ltree`: a path has no declared depth.
        | ColumnType::Ltree
        | ColumnType::LQuery
        | ColumnType::Money
        | ColumnType::Inet
        | ColumnType::Cidr
        | ColumnType::MacAddr
        | ColumnType::Int8
        | ColumnType::Int4
        | ColumnType::Int2
        | ColumnType::Text
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Bool
        | ColumnType::Bytea
        | ColumnType::Double
        | ColumnType::Real
        | ColumnType::Uuid
        | ColumnType::Oid
        // A `regtype` takes none either: `pg_type.typmodin` is `-` for it.
        | ColumnType::RegType
        | ColumnType::RegProc
        | ColumnType::Date
        // Unreachable: every array type is answered above, from its element's answer. Kept as
        // arms rather than a `_` so that the next type added here has to answer the question.
        | ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        // **Nor does `name`**, and a real server says so in its own words: `'x'::name(10)` is
        // `42601 type modifier is not allowed for type "name"`. Fixed width is not a typmod.
        | ColumnType::Name
        | ColumnType::Char
        // An hstore takes no typmod either: `hstore(3)` is not a thing on a real server.
        | ColumnType::Hstore
        | ColumnType::HstoreArray
        | ColumnType::TsVector
        | ColumnType::TsQuery
        | ColumnType::TsVectorArray
        | ColumnType::TsQueryArray
        | ColumnType::Citext
        | ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                        | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                        | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray | ColumnType::BitArray | ColumnType::VarBitArray
        | ColumnType::Point
        | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::RegClass | ColumnType::Void | ColumnType::CitextArray | ColumnType::XmlArray | ColumnType::LtreeArray => false,
    }
}

/// The typmod arguments of a name, checked the way the declaration would check them.
///
/// The value is thrown away — a `regtype` is the type, not the type with its number — but the
/// **error is not**, which is the whole point: `'numeric(1001,0)'::regtype` raises on a real
/// server and answering `1700` would be an answer where PostgreSQL refuses.
fn validate_typmod(ty: ColumnType, arguments: &str) -> Result<()> {
    let parts: Vec<&str> = arguments.split(',').map(str::trim).collect();
    let number = |text: &str| -> Result<i32> {
        // PostgreSQL's *parser* stops at a sign inside a type name: `'timestamp(-1)'::regtype` is
        // `42601 syntax error at or near "-"`, not a bounds error, because a typmod argument is
        // grammatically an unsigned integer.
        if text.starts_with('-') || text.starts_with('+') {
            return Err(SqlError::TypeNameSyntax(text[..1].to_owned()));
        }
        text.parse::<i32>()
            .map_err(|_| SqlError::TypeNameSyntax(text.to_owned()))
    };
    match ty {
        // One length, and it is the typmod itself.
        ColumnType::Bit | ColumnType::VarBit => {
            let length = number(parts.first().copied().unwrap_or_default())?;
            if length < 1 {
                return Err(SqlError::TypeLengthTooSmall(if ty == ColumnType::Bit {
                    "bit"
                } else {
                    "varbit"
                }));
            }
        }
        // One length, and the message spells the type **short**: `length for type varchar`, not
        // `character varying`. Measured, both types and both bounds.
        ColumnType::Varchar | ColumnType::Bpchar => {
            let spelled = if matches!(ty, ColumnType::Varchar) {
                "varchar"
            } else {
                "char"
            };
            let length = number(parts.first().copied().unwrap_or_default())?;
            if length < 1 {
                return Err(SqlError::TypeLengthTooSmall(spelled));
            }
            if u32::try_from(length).is_ok_and(|length| length > MAX_TYPE_LENGTH) {
                return Err(SqlError::TypeLengthTooLarge(spelled, MAX_TYPE_LENGTH));
            }
        }
        // A precision past the maximum is **reduced, not refused** — `'timestamp(9)'::regtype` is
        // 1114 and `'time(9)'::regtype` is 1083, each with a `WARNING` and no error. The number is
        // discarded here in any case; only a negative one is a refusal, and it is the parser's.
        ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Time => {
            number(parts.first().copied().unwrap_or_default())?;
        }
        ColumnType::Numeric => {
            let precision = number(parts.first().copied().unwrap_or_default())?;
            let scale = match parts.get(1) {
                Some(text) => Some(number(text)?),
                None => None,
            };
            numeric::declared_typmod(Some(precision), scale)?;
        }
        // `takes_typmod` returned false for these and the caller stopped before here.
        _ => {}
    }
    Ok(())
}

/// What a stored type *means* to a PostgreSQL client.
///
/// The six shapes themselves are [`esker_keys::value`]'s — the storage layer's shared vocabulary,
/// named by the codecs that write them. Everything here is contract C3's surface instead, and none
/// of it was written from documentation: each rule was put to a running 19beta1 and recorded in
/// `tests/corpus/pg19_values.txt`.
///
/// A trait rather than inherent methods because the type is another crate's now, and that is the
/// seam working as intended: a crate that owns byte layout cannot accidentally answer a question
/// about what a client renders. See `docs/adr/0030-the-row-codec-moves-down.md`.
pub trait PgType: Copy {
    /// PostgreSQL's type OID, as it appears in `RowDescription`.
    #[must_use]
    fn oid(self) -> u32;
    /// The name PostgreSQL uses when it talks *about* the type — in an `invalid input syntax`
    /// message, for instance. Not the DDL spelling: a column is declared `int8` and complained
    /// about as `bigint`.
    #[must_use]
    fn name(self) -> &'static str;
    /// The width `RowDescription` reports: the fixed size in bytes, or -1 for a varlena.
    #[must_use]
    fn type_len(self) -> i16;
}

impl PgType for ColumnType {
    #[expect(
        clippy::too_many_lines,
        reason = "one match over the whole type vocabulary, and it is a list of names \
                  rather than of rules; splitting it would put half the vocabulary \
                  somewhere else and let a type be added to one half without the other"
    )]
    fn oid(self) -> u32 {
        // **An array type's OID is its element's `typarray`**, which this crate already knows —
        // `array_oid` is where `_int4` is 1007 and `_int8` is 1016. Derived rather than repeated,
        // so the two can never disagree.
        if let Some(element) = esker_keys::array::ArrayValue::element_of(self) {
            return array_oid(element);
        }
        match self {
            // Measured: `'void'::regtype::oid`.
            ColumnType::Void => 2278,
            ColumnType::Bool => 16,
            ColumnType::Bytea => 17,
            ColumnType::Char => 18,
            ColumnType::Name => 19,
            ColumnType::Int8 => 20,
            // PostgreSQL's own, measured: `'regtype'::regtype::oid` is 2206.
            ColumnType::RegType => 2206,
            // **24**, which is a lower oid than most base types: `regproc` is in the catalog from
            // the first initdb because `pg_type.typinput` needs it.
            ColumnType::RegProc => 24,
            ColumnType::RegClass => 2205,
            ColumnType::Int2Vector => 22,
            ColumnType::OidVector => 30,
            ColumnType::Int2 => 21,
            ColumnType::Int4 => 23,
            ColumnType::Text => 25,
            ColumnType::Varchar => 1043,
            ColumnType::Bpchar => 1042,
            ColumnType::Json => 114,
            ColumnType::Xml => 142,
            ColumnType::Jsonb => 3802,
            ColumnType::Hstore => HSTORE_OID,
            ColumnType::TsVector => 3614,
            ColumnType::TsQuery => 3615,
            ColumnType::Citext => CITEXT_OID,
            ColumnType::Ltree => LTREE_OID,
            ColumnType::LQuery => LQUERY_OID,
            // PostgreSQL's own, and fixed: unlike an extension's, a range type is built in.
            ColumnType::TsRange => 3908,
            ColumnType::TstzRange => 3910,
            ColumnType::Int4Range => 3904,
            ColumnType::DateRange => 3912,
            ColumnType::NumRange => 3906,
            ColumnType::Int8Range => 3926,
            // **Not an extension's**: `money` is built in, so its oid is fixed like a range's.
            ColumnType::Money => 790,
            ColumnType::MoneyArray => 791,
            ColumnType::Inet => 869,
            ColumnType::InetArray => 1041,
            ColumnType::Cidr => 650,
            ColumnType::CidrArray => 651,
            ColumnType::MacAddr => 829,
            ColumnType::MacAddrArray => 1040,
            ColumnType::Lseg => 601,
            ColumnType::Box => 603,
            ColumnType::Path => 602,
            ColumnType::Polygon => 604,
            ColumnType::Circle => 718,
            ColumnType::Line => 628,
            ColumnType::Bit => 1560,
            ColumnType::BitArray => 1561,
            ColumnType::VarBit => 1562,
            ColumnType::VarBitArray => 1563,
            ColumnType::Point => 600,
            ColumnType::TsRangeArray => TSRANGE_ARRAY_OID,
            ColumnType::HstoreArray => HSTORE_ARRAY_OID,
            ColumnType::TsVectorArray => 3643,
            ColumnType::TsQueryArray => 3645,
            ColumnType::Real => 700,
            ColumnType::Double => 701,
            ColumnType::Timestamp => 1114,
            ColumnType::TimestampTz => 1184,
            ColumnType::Date => 1082,
            ColumnType::Numeric => 1700,
            ColumnType::Time => 1083,
            ColumnType::Uuid => 2950,
            ColumnType::Interval => 1186,
            ColumnType::Oid => 26,
            // Unreachable: the four array types answered above, from their element's `typarray`.
            //
            // **A user-defined range joins them with a reason of its own**: its oid is allocated
            // by the `CREATE TYPE` that made it, so there is no constant to give. The wire gets
            // it from `crate::exec::query::OutputColumn::user_type`, the road an enum's oid
            // already travels (ADR 0050), and this is the `InvalidOid` a column that lost it
            // reports.
            ColumnType::FloatRange
            | ColumnType::VarcharRange
            | ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray
            | ColumnType::BoolArray
            | ColumnType::ByteaArray
            | ColumnType::BpcharArray
            | ColumnType::VarcharArray
            | ColumnType::NameArray
            | ColumnType::CharArray
            | ColumnType::DateArray
            | ColumnType::TimeArray
            | ColumnType::TimestampArray
            | ColumnType::TimestampTzArray
            | ColumnType::IntervalArray
            | ColumnType::RealArray
            | ColumnType::DoubleArray
            | ColumnType::UuidArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::OidArray
            | ColumnType::RegTypeArray
            | ColumnType::RegProcArray
            | ColumnType::CitextArray
            | ColumnType::XmlArray
            | ColumnType::LtreeArray
            | ColumnType::TstzRangeArray
            | ColumnType::Int4RangeArray
            | ColumnType::DateRangeArray
            | ColumnType::NumRangeArray
            | ColumnType::Int8RangeArray
            | ColumnType::PointArray
            | ColumnType::BoxArray
            | ColumnType::LsegArray
            | ColumnType::PathArray
            | ColumnType::PolygonArray
            | ColumnType::CircleArray
            | ColumnType::LineArray => 0,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one match over the whole type vocabulary, and it is a list of names \
                  rather than of rules; splitting it would put half the vocabulary \
                  somewhere else and let a type be added to one half without the other"
    )]
    fn name(self) -> &'static str {
        match self {
            ColumnType::Void => "void",
            // **With the quotes**, which is how it must be written — an unquoted `char` is
            // `bpchar` — and how `format_type` and `pg_typeof` print it. Measured.
            ColumnType::Char => "\"char\"",
            ColumnType::CharArray => "\"char\"[]",
            // What an error message calls it: the element's name with `[]`, which is how
            // PostgreSQL words `cannot cast type integer[] to uuid` — not the internal `_int4`
            // that `pg_type.typname` holds.
            ColumnType::Int8Array => "bigint[]",
            ColumnType::Int4Array => "integer[]",
            ColumnType::Int2Array => "smallint[]",
            ColumnType::NumericArray => "numeric[]",
            ColumnType::TextArray => "text[]",
            ColumnType::Hstore => "hstore",
            ColumnType::TsVector => "tsvector",
            ColumnType::TsQuery => "tsquery",
            ColumnType::Citext => "citext",
            ColumnType::TsRange => "tsrange",
            ColumnType::TstzRange => "tstzrange",
            ColumnType::Int4Range => "int4range",
            ColumnType::DateRange => "daterange",
            ColumnType::NumRange => "numrange",
            ColumnType::Int8Range => "int8range",
            // **Not PostgreSQL type names**: PostgreSQL has no built-in range over `float8` or
            // over `varchar`, and these are representations rather than types. A column of one
            // always carries a `user_type` oid and every client-visible name comes from the
            // catalog with it, so what is here is what an *internal* error prints — which is why
            // it names the subtype, the way `22P02 invalid input syntax for type double
            // precision: "abc"` names it on a real server for a bad `floatrange` bound.
            ColumnType::FloatRange => "float8range",
            ColumnType::VarcharRange => "varcharrange",
            ColumnType::Point => "point",
            ColumnType::PointArray => "point[]",
            ColumnType::BoxArray => "box[]",
            ColumnType::LsegArray => "lseg[]",
            ColumnType::PathArray => "path[]",
            ColumnType::PolygonArray => "polygon[]",
            ColumnType::CircleArray => "circle[]",
            ColumnType::LineArray => "line[]",
            ColumnType::Money => "money",
            ColumnType::MoneyArray => "money[]",
            ColumnType::Inet => "inet",
            ColumnType::InetArray => "inet[]",
            ColumnType::Cidr => "cidr",
            ColumnType::CidrArray => "cidr[]",
            ColumnType::MacAddr => "macaddr",
            ColumnType::MacAddrArray => "macaddr[]",
            // **`bit`, not `"bit"`.** The quoted spelling is what `format_type` writes inside a
            // default expression, and that is `format_type`'s business rather than the name's.
            ColumnType::Lseg => "lseg",
            ColumnType::Box => "box",
            ColumnType::Path => "path",
            ColumnType::Polygon => "polygon",
            ColumnType::Circle => "circle",
            ColumnType::Line => "line",
            ColumnType::Bit => "bit",
            ColumnType::BitArray => "bit[]",
            ColumnType::VarBit => "bit varying",
            ColumnType::VarBitArray => "bit varying[]",
            ColumnType::TstzRangeArray => "tstzrange[]",
            ColumnType::Int4RangeArray => "int4range[]",
            ColumnType::DateRangeArray => "daterange[]",
            ColumnType::NumRangeArray => "numrange[]",
            ColumnType::Int8RangeArray => "int8range[]",
            ColumnType::TsRangeArray => "tsrange[]",
            ColumnType::BoolArray => "boolean[]",
            ColumnType::ByteaArray => "bytea[]",
            ColumnType::BpcharArray => "character[]",
            ColumnType::VarcharArray => "character varying[]",
            ColumnType::NameArray => "name[]",
            ColumnType::DateArray => "date[]",
            ColumnType::TimeArray => "time without time zone[]",
            ColumnType::TimestampArray => "timestamp without time zone[]",
            ColumnType::TimestampTzArray => "timestamp with time zone[]",
            ColumnType::IntervalArray => "interval[]",
            ColumnType::RealArray => "real[]",
            ColumnType::DoubleArray => "double precision[]",
            ColumnType::UuidArray => "uuid[]",
            ColumnType::JsonArray => "json[]",
            ColumnType::JsonbArray => "jsonb[]",
            ColumnType::OidArray => "oid[]",
            ColumnType::RegTypeArray => "regtype[]",
            ColumnType::RegProcArray => "regproc[]",
            ColumnType::CitextArray => "citext[]",
            ColumnType::Xml => "xml",
            ColumnType::XmlArray => "xml[]",
            ColumnType::Ltree => "ltree",
            ColumnType::LtreeArray => "ltree[]",
            ColumnType::LQuery => "lquery",
            ColumnType::HstoreArray => "hstore[]",
            ColumnType::TsVectorArray => "tsvector[]",
            ColumnType::TsQueryArray => "tsquery[]",
            ColumnType::Int8 => "bigint",
            ColumnType::Int4 => "integer",
            ColumnType::Int2 => "smallint",
            ColumnType::Text => "text",
            ColumnType::Varchar => "character varying",
            // Its own name, and the same one `format_type` gives it: there is no longer spelling.
            ColumnType::Name => "name",
            ColumnType::Bpchar => "character",
            ColumnType::Json => "json",
            ColumnType::Jsonb => "jsonb",
            ColumnType::Bool => "boolean",
            ColumnType::Bytea => "bytea",
            ColumnType::TimestampTz => "timestamp with time zone",
            ColumnType::Timestamp => "timestamp without time zone",
            ColumnType::Double => "double precision",
            ColumnType::Real => "real",
            ColumnType::Date => "date",
            ColumnType::Numeric => "numeric",
            ColumnType::Time => "time without time zone",
            ColumnType::Uuid => "uuid",
            ColumnType::Interval => "interval",
            ColumnType::Oid => "oid",
            ColumnType::RegType => "regtype",
            ColumnType::RegProc => "regproc",
            ColumnType::RegClass => "regclass",
            ColumnType::Int2Vector => "int2vector",
            ColumnType::OidVector => "oidvector",
        }
    }

    fn type_len(self) -> i16 {
        match self {
            // **One byte**, which is the whole of the type.
            ColumnType::Bool | ColumnType::Char => 1,
            // **64 and positive**, where every other string type answers -1: `name` is fixed
            // width. A client reads this from the `RowDescription` and from `pg_attribute.attlen`.
            ColumnType::Name => 64,
            // Four bytes, unsigned, which is the whole of what makes it not an `int4`.
            ColumnType::Int4
            | ColumnType::Real
            | ColumnType::Date
            | ColumnType::Oid
            // Four on the wire as well: what a client reads is the oid's width, and the name is
            // the output function's business.
            | ColumnType::RegType
            | ColumnType::RegProc
            | ColumnType::RegClass
            // **And a `void`, positive and four**, which reasoning would make -1 or 0 for a value
            // that is nothing — measured beside its `typtype = 'p'`.
            | ColumnType::Void => 4,
            ColumnType::Int2 => 2,
            // Sixteen fixed bytes, which is what `pg_type.typlen` says.
            // Sixteen fixed bytes each: a uuid is one value, an interval is three fields.
            // Sixteen fixed bytes each: a uuid is one value, an interval is three fields,
            // and a point is two `float8` coordinates — `pg_type.typlen` says 16 for all
            // three, measured.
            ColumnType::Uuid | ColumnType::Interval | ColumnType::Point => 16,
            // **Six**, which is the whole of a `macaddr` — `typlen` says so and `typstorage` `p`
            // agrees. The two addresses are varlenas (`-1`) below, because an IPv4 and an IPv6
            // are not the same width.
            ColumnType::MacAddr => 6,
            // Measured, and not guessable: an `lseg` and a `box` are four `float8`s, a `circle`
            // and a `line` three, and a `path` and a `polygon` hold as many points as they were
            // given — which is what `-1` says.
            ColumnType::Lseg | ColumnType::Box => 32,
            ColumnType::Circle | ColumnType::Line => 24,
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Time
            // **Eight, and `typstorage` `p`.** A `money` is a count of cents in an `i64` and not
            // a varlena, which is exactly what bounds the type at `$92,233,720,368,547,758.07`.
            | ColumnType::Money
            | ColumnType::Double => 8,
            // Variable length, which `pg_type.typlen` spells `-1` — measured for hstore in the
            // adapter's own boot query.
            ColumnType::Hstore
            | ColumnType::HstoreArray
            | ColumnType::TsVector
            | ColumnType::TsQuery
            | ColumnType::TsVectorArray
            | ColumnType::TsQueryArray
            | ColumnType::Citext
            | ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                        | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                        | ColumnType::Inet | ColumnType::Cidr | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray | ColumnType::Bit | ColumnType::VarBit | ColumnType::BitArray | ColumnType::VarBitArray | ColumnType::Path | ColumnType::Polygon
            | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::CitextArray | ColumnType::XmlArray | ColumnType::LtreeArray
            | ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Xml
            | ColumnType::Ltree
            | ColumnType::LQuery
            | ColumnType::Numeric
            | ColumnType::Bytea
            // However many elements it has, which is the definition of a varlena.
            | ColumnType::Int8Array
            | ColumnType::Int4Array
        | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray => -1,
        }
    }
}

/// What a [`Datum`] means to a PostgreSQL client: its text, its binary form, and its order.
///
/// [`PgDatum::pg_cmp`] **disagrees with `Datum`'s `PartialEq`**, and is meant to. `PartialEq` is
/// bitwise, so a round-trip test cannot pass by turning `-0.0` into `0.0` or one `NaN` payload
/// into another. `pg_cmp` says `NaN` equals itself and sorts above `Infinity`, which is what
/// `WHERE x > 5` has to agree with. Storage asks whether the bytes survived; SQL asks how a user
/// orders them, and the two questions have different answers.
pub trait PgDatum: Sized {
    /// The characters PostgreSQL puts in a text-format `DataRow`, or `None` for NULL.
    ///
    /// NULL is `None` rather than an empty string because the protocol spells it as a length of
    /// -1: an empty `text` and a NULL `text` are different bytes on the wire, and a client that
    /// could not tell them apart would read every empty string as a missing value.
    #[must_use]
    fn to_text(&self) -> Option<String>;
    /// Reads a value of `ty` out of the text a client sent, exactly as PostgreSQL's input function
    /// would, or fails with the SQLSTATE PostgreSQL would have failed with.
    ///
    /// Where the real input function accepts something this one does not, the answer is contract
    /// C2's `0A000` naming the construct — never a wrong value and never a syntax error about
    /// valid input. `tests/value_parity.rs` holds the list of those from both sides.
    fn from_text(ty: ColumnType, text: &str) -> Result<Datum>;
    /// The bytes PostgreSQL puts in a **binary**-format field, or `None` when there are none to
    /// put there.
    ///
    /// `None` is a NULL **or** a type this node will not write in binary. The two are one answer
    /// because nothing sends binary results yet; the day something does, it must refuse the
    /// second kind before asking rather than send it as a NULL. `from_binary` refuses
    /// the same types on the way in, with the `0A000` that names the type.
    ///
    /// Big-endian throughout, which is the one place this project is: everything it writes for
    /// itself is little-endian and everything on this wire is not. The formats were captured with
    /// `COPY ... TO STDOUT (FORMAT binary)`, which uses the same `typsend` functions the protocol
    /// does, and one of them settled a bet made back in unit 3 — a `timestamptz` really is
    /// microseconds from 2000-01-01 with `i64::MAX` for `infinity`, so a value goes onto the wire
    /// exactly as it is stored, with no arithmetic at all.
    #[must_use]
    fn to_binary(&self) -> Option<Vec<u8>>;
    /// Reads a value out of a binary-format parameter.
    ///
    /// A wrong length is an error, never a partial read: a client that sends four bytes for an
    /// `int8` has a bug, and guessing at what it meant would turn that bug into a wrong number.
    fn from_binary(ty: ColumnType, bytes: &[u8]) -> Result<Datum>;
    /// The order PostgreSQL sorts these values in, which is not the order their bits are in.
    ///
    /// Three of its rules are its own and were confirmed against the server: `-0.0` and `0.0`
    /// compare equal, `NaN` compares greater than every other float including `Infinity` (and
    /// equal to itself), and NULL sorts **last**, which is what `ORDER BY x` means with no
    /// `NULLS FIRST`. [`crate::row`] encodes keys so that byte order reproduces this.
    ///
    /// Comparing two different types is not something a schema can produce; it falls back to a
    /// fixed order over the variants so the function is total.
    #[must_use]
    fn pg_cmp(&self, other: &Self) -> Ordering;
}

impl PgDatum for Datum {
    fn to_text(&self) -> Option<String> {
        Some(match self {
            Datum::Null => return None,
            // **The name, not the number** — that is the whole of what makes this a type of its
            // own (ADR 0077). An oid with no type carries its digits as its name, which is what a
            // real server prints for one.
            Datum::RegType { name, .. }
            | Datum::RegProc { name, .. }
            | Datum::RegClass { name, .. } => name.to_string(),
            Datum::Array(value) => array::to_text(value),
            Datum::Point { x, y } => point::to_text(*x, *y),
            Datum::Money(cents) => money::to_text(*cents),
            Datum::Inet {
                family,
                bits,
                cidr,
                addr,
            } => inet::to_text(
                &inet::Address {
                    family: *family,
                    bits: *bits,
                    addr: *addr,
                },
                *cidr,
            ),
            Datum::MacAddr(mac) => inet::mac_to_text(*mac),
            Datum::Bit { bits, .. } => bits.clone(),
            Datum::Geometry { text, .. } => text.clone(),
            Datum::Int8(v) => v.to_string(),
            Datum::Int4(v) => v.to_string(),
            Datum::Int2(v) => v.to_string(),
            // **As written**: what a client is sent is the spelling that was stored, never the
            // folded form the key holds.
            Datum::Text(v)
            | Datum::Citext(v)
            | Datum::Ltree(v)
            | Datum::Hstore(v)
            | Datum::TsVector(v)
            | Datum::TsQuery(v)
            | Datum::Range { text: v, .. } => v.clone(),
            // One character. See the module note: the `::text` cast says `true`, the output
            // function says `t`, and the wire carries the output function.
            Datum::Bool(v) => (if *v { "t" } else { "f" }).to_string(),
            Datum::Bytea(v) => {
                let mut out = String::with_capacity(2 + 2 * v.len());
                out.push_str("\\x");
                for byte in v {
                    out.push(HEX[(byte >> 4) as usize] as char);
                    out.push(HEX[(byte & 0x0f) as usize] as char);
                }
                out
            }
            Datum::TimestampTz(v) => timestamp::to_text(*v),
            // No offset, which is the whole visible difference between the two types.
            Datum::Timestamp(v) => timestamp::to_text_without_zone(*v),
            Datum::Double(v) => float::to_text(*v),
            Datum::Real(v) => float::to_text_f32(*v),
            Datum::Date(v) => date::to_text(*v),
            Datum::Time(v) => time::to_text(*v),
            Datum::Uuid(v) => uuid::to_text(v),
            Datum::Oid(v) => oid::to_text(*v),
            Datum::Interval {
                months,
                days,
                micros,
            } => interval::to_text(&interval::Interval {
                months: *months,
                days: *days,
                micros: *micros,
            }),
            Datum::Numeric(v) => numeric::to_text(v),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one input function per type, in one match; splitting it would put a type's \
                  reading somewhere other than beside every other type's"
    )]
    fn from_text(ty: ColumnType, text: &str) -> Result<Datum> {
        Ok(match ty {
            // **The first *byte*, printed the way the output function prints it.** `'abc'` is
            // `a` and `'é'` is the first byte of a two-byte character, which is not valid UTF-8
            // alone — a real server writes it as the octal escape `\303`, and the escape is
            // carried as the value so that it is a property of the value rather than of one
            // printer. The empty string is legal and stays empty.
            ColumnType::Char => Datum::Text(char_type::of_text(text)),
            // **A void has one value and it is zero characters**, so its input function ignores
            // what it was handed: `void_in` exists on a real server for the same reason, and
            // nothing calls it either.
            ColumnType::Void => Datum::Text(String::new()),
            // **A name, and `42704` for one that is not a type** — measured, `'nosuchtype'::regtype`
            // says `type "nosuchtype" does not exist`. The opposite direction does not match: an
            // *oid* that is no type is not an error, it prints as its digits (`999999::regtype`),
            // which is what `regtype_of_oid` answers.
            //
            // A type a `CREATE TYPE` made cannot be resolved here, because that needs the catalog
            // and this function has none; the executor's `UserRegType` is the seam for those, and
            // it builds the value with the name it looked up.
            // **A `regclass` from text needs the catalog, and this function has none.** Digits are
            // the half that does not: an oid naming no relation prints its digits and reads back
            // from them, which is the round trip `999999::regclass` makes. A *name* is resolved at
            // the seam that has a catalog — `CatalogFunc::RegClass`, where every `::regclass` cast
            // is lowered — so reaching here with one means a path that bypassed it.
            // Text's representation: the numbers space separated, read back as written.
            ColumnType::Int2Vector | ColumnType::OidVector => Datum::Text(text.to_owned()),
            ColumnType::RegClass => {
                return text.parse::<i64>().map(regclass_of_oid).map_err(|_| {
                    SqlError::unsupported("a relation name read as a regclass without a catalog")
                });
            }
            // **Digits are an oid and a name is resolved**, which is `regprocin`'s whole rule:
            // `24::regproc` round-trips as `24` because no function has that oid, and a name
            // nothing has is `42883` rather than a syntax error.
            ColumnType::RegProc => {
                let oid = reg_proc::from_text(text)?;
                Datum::RegProc {
                    oid,
                    name: reg_proc::to_text(oid).into_boxed_str(),
                }
            }
            ColumnType::RegType => {
                let named =
                    named_type(text)?.ok_or_else(|| SqlError::UndefinedType(text.to_owned()))?;
                // Through the oid, so the printed form is the **canonical** name and not the
                // spelling written: `'int4'::regtype` is `integer`, measured.
                regtype_of_oid(named.oid())
            }
            ColumnType::Point => {
                let (x, y) = point::from_text(text)?;
                Datum::Point { x, y }
            }
            ColumnType::Money => Datum::Money(money::from_text(text)?),
            ColumnType::Inet | ColumnType::Cidr => {
                let cidr = ty == ColumnType::Cidr;
                let address = inet::from_text(text, cidr)?;
                Datum::Inet {
                    family: address.family,
                    bits: address.bits,
                    cidr,
                    addr: address.addr,
                }
            }
            ColumnType::MacAddr => Datum::MacAddr(inet::mac_from_text(text)?),
            ColumnType::Lseg
            | ColumnType::Box
            | ColumnType::Path
            | ColumnType::Polygon
            | ColumnType::Circle
            | ColumnType::Line => Datum::Geometry {
                kind: Box::new(ty),
                text: geometric::from_text(
                    geometric_kind(ty).unwrap_or(geometric::Kind::Lseg),
                    text,
                )?,
            },
            ColumnType::Bit | ColumnType::VarBit => Datum::Bit {
                varying: ty == ColumnType::VarBit,
                bits: bit::from_text(text)?,
            },
            // The literal's *shape* is read here and each element by its own type's input
            // function, which is what makes `'{1,x}'::int[]` `int4`'s error and `'{a,,b}'` the
            // array's (`crate::value::array`).
            ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray
            | ColumnType::HstoreArray
            | ColumnType::TsVectorArray
            | ColumnType::TsQueryArray
            | ColumnType::TsRangeArray
            | ColumnType::TstzRangeArray
            | ColumnType::Int4RangeArray
            | ColumnType::DateRangeArray
            | ColumnType::NumRangeArray
            | ColumnType::Int8RangeArray
            | ColumnType::PointArray
            | ColumnType::BoxArray
            | ColumnType::LsegArray
            | ColumnType::PathArray
            | ColumnType::PolygonArray
            | ColumnType::CircleArray
            | ColumnType::LineArray
            | ColumnType::MoneyArray
            | ColumnType::InetArray
            | ColumnType::CidrArray
            | ColumnType::MacAddrArray
            | ColumnType::BitArray
            | ColumnType::VarBitArray
            | ColumnType::BoolArray
            | ColumnType::ByteaArray
            | ColumnType::BpcharArray
            | ColumnType::VarcharArray
            | ColumnType::NameArray
            | ColumnType::CharArray
            | ColumnType::DateArray
            | ColumnType::TimeArray
            | ColumnType::TimestampArray
            | ColumnType::TimestampTzArray
            | ColumnType::IntervalArray
            | ColumnType::RealArray
            | ColumnType::DoubleArray
            | ColumnType::UuidArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::OidArray
            | ColumnType::RegTypeArray
            | ColumnType::RegProcArray
            | ColumnType::CitextArray
            | ColumnType::XmlArray
            | ColumnType::LtreeArray => {
                let element =
                    esker_keys::array::ArrayValue::element_of(ty).unwrap_or(ColumnType::Text);
                Datum::Array(array::from_text(text, element)?)
            }
            ColumnType::Int8 => Datum::Int8(parse_int8(text)?),
            ColumnType::Int4 => Datum::Int4(parse_int4(text)?),
            ColumnType::Int2 => Datum::Int2(parse_int2(text)?),
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                Datum::Text(text.to_owned())
            }
            // **`namein` truncates**, which is the whole of what makes `name` not a `varchar`
            // with an oid of its own: the value is cut to 63 bytes on the way in, not checked
            // and refused. `repeat('a',64)::name = repeat('a',63)::name` is `t` because of this
            // line (measured, `tests/captures/pg19_name_type.txt`).
            ColumnType::Name => Datum::Text(truncate_to_name(text)),
            // `json` keeps the text exactly as sent, once it is known to be a document; `jsonb`
            // keeps the canonical form it prints as. ADR 0042 is why the two differ here and
            // nowhere else in this function.
            ColumnType::Json => {
                json::validate(text)?;
                Datum::Text(text.to_owned())
            }
            ColumnType::Jsonb => Datum::Text(json::canonicalise(text)?),
            // **`json`'s arm with a different validator**, and a different refusal class: the
            // text is kept exactly as sent once it is known to be well-formed XML *content*
            // (`crate::value::xml`), which `'plain text'` is and `'<a>'` is not.
            ColumnType::Xml => {
                let text = xml::strip_declaration(text);
                xml::validate(text)?;
                Datum::Text(text.to_owned())
            }
            // Read and written back **canonical**, the same road `jsonb` takes: the stored form is
            // what the type prints, so equality and ordering are the text's (`crate::value::hstore`).
            ColumnType::Hstore => Datum::Hstore(hstore::to_text(&hstore::from_text(text)?)),
            ColumnType::TsVector => Datum::TsVector(tsvector::to_text(&tsvector::from_text(text)?)),
            ColumnType::TsQuery => Datum::TsQuery(tsquery::to_text(&tsquery::from_text(text)?)),
            // **As written.** The folding is the comparison's, so nothing here touches the case.
            ColumnType::Citext => Datum::Citext(text.to_owned()),
            // **As written too**, once the labels are known to be labels. Nothing is normalised —
            // `a.b.c` comes back exactly as it went in — so equality is the text's and only the
            // *order* is the type's own (`crate::value::ltree`).
            ColumnType::Ltree => Datum::Ltree(ltree::from_text(text)?),
            // A pattern is its characters once it parses, which is `json`'s road: nothing is
            // built from it here, and `lquery::compile` is what reads it at match time.
            ColumnType::LQuery => Datum::Text(ltree::lquery_checked(text)?),
            // Parsed and rendered back **canonical**, which is what makes equality and grouping the
            // text's — the same road `hstore` and `jsonb` take.
            ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range
            | ColumnType::DateRange
            | ColumnType::NumRange
            | ColumnType::Int8Range
            | ColumnType::FloatRange
            | ColumnType::VarcharRange => {
                let subtype = range_subtype(ty);
                Datum::Range {
                    subtype: Box::new(subtype),
                    text: range::from_text(subtype, text)?.to_text(),
                }
            }
            ColumnType::Bool => Datum::Bool(parse_bool(text)?),
            ColumnType::Bytea => Datum::Bytea(parse_bytea(text)?),
            ColumnType::TimestampTz => Datum::TimestampTz(timestamp::from_text(text)?),
            ColumnType::Timestamp => Datum::Timestamp(timestamp::from_text_without_zone(text)?),
            ColumnType::Double => Datum::Double(float::from_text(text)?),
            ColumnType::Real => Datum::Real(float::from_text_f32(text)?),
            // The clock words -- `today`, `tomorrow` -- need an instant, and this function has
            // none: a value read from a wire parameter or a stored literal is not the place a
            // session's clock enters. `crate::exec` resolves them where it has the transaction's
            // start timestamp, which is the only clock this crate is allowed to read (DESIGN §6).
            ColumnType::Date => Datum::Date(date::from_text(text, 0)?),
            ColumnType::Time => Datum::Time(time::from_text(text)?),
            ColumnType::Uuid => Datum::Uuid(uuid::from_text(text)?),
            ColumnType::Oid => Datum::Oid(oid::from_text(text)?),
            ColumnType::Interval => {
                let value = interval::from_text(text)?;
                Datum::Interval {
                    months: value.months,
                    days: value.days,
                    micros: value.micros,
                }
            }
            ColumnType::Numeric => Datum::Numeric(numeric::from_text(text)?),
        })
    }

    fn to_binary(&self) -> Option<Vec<u8>> {
        Some(match self {
            // A NULL, and a `numeric`, whose binary wire form is its own four-`i16` header plus
            // base-10000 digit groups (`numeric_send(1.5)` is `\x000200000000000100011388`) —
            // nothing here has ever sent or read that shape, so it is refused rather than
            // guessed. See the contract above for why the two share one answer.
            // A uuid's binary form is its sixteen bytes, which is what `uuid_send` writes —
            // the same bytes the row holds, in the same order.
            // Four big-endian bytes, which is what `oidsend` writes.
            Datum::Oid(v) => v.to_be_bytes().to_vec(),
            // The oid, four bytes: a binary `regtype` is `oidsend`'s output on a real server, and
            // the name is the *text* format's business alone.
            Datum::RegType { oid, .. } | Datum::RegProc { oid, .. } => oid.to_be_bytes().to_vec(),
            Datum::RegClass { oid, .. } => oid.to_be_bytes().to_vec(),
            Datum::Uuid(v) => v.to_vec(),
            // `interval_send` writes microseconds, days and months in that order, big-endian.
            Datum::Interval {
                months,
                days,
                micros,
            } => {
                let mut out = Vec::with_capacity(16);
                out.extend_from_slice(&micros.to_be_bytes());
                out.extend_from_slice(&days.to_be_bytes());
                out.extend_from_slice(&months.to_be_bytes());
                out
            }
            // `array_send`'s form is a dimension header, a flags word, the element OID and then
            // each element's own binary form — a shape nothing here has ever sent or read, so an
            // array is refused with the `numeric` beside it rather than guessed.
            // **A point joins them.** `point_send` writes the two coordinates big-endian and
            // nothing here has ever been asked for that shape — the suite reads a point as
            // text — so it is refused rather than guessed, exactly as the other three are.
            //
            // **And a money**, for the same reason and with the same shape unmeasured:
            // `cash_send` writes the cents as eight big-endian bytes, which is easy to guess and
            // has not been probed, and a guess here is a wrong parse of every value in a column.
            // **And the three network types**, for the same reason: `inet_send` writes a family
            // byte, the prefix, a flag and the address, and `macaddr_send` its six bytes.
            // Neither shape has been read here, and a guess is a wrong parse of every value.
            Datum::Null
            | Datum::Numeric(_)
            | Datum::Array(_)
            | Datum::Point { .. }
            | Datum::Inet { .. }
            | Datum::MacAddr(_)
            // `bit_send` writes a length and the packed bits; nothing here has read that shape.
            | Datum::Bit { .. }
            // Each of the six has a `*_send` of its own, packing `float8`s; none has been read
            // here, and a guess is a wrong parse of every value in a column.
            | Datum::Geometry { .. }
            | Datum::Money(_) => {
                return None;
            }
            // A `time` joins them: `time_send` is the microsecond count as eight big-endian
            // bytes, measured with `COPY ... (FORMAT binary)` — `12:34:56` is `0x0a8bda1c00`
            // (45_296_000_000) and `24:00:00` is `0x141dd76000`, the top of the closed range.
            Datum::Int8(v) | Datum::TimestampTz(v) | Datum::Timestamp(v) | Datum::Time(v) => {
                v.to_be_bytes().to_vec()
            }
            // Four big-endian bytes for both, which is what `int4send` and `date_send` write.
            Datum::Int4(v) | Datum::Date(v) => v.to_be_bytes().to_vec(),
            Datum::Int2(v) => v.to_be_bytes().to_vec(),
            Datum::Bool(v) => vec![u8::from(*v)],
            Datum::Double(v) => v.to_be_bytes().to_vec(),
            Datum::Real(v) => v.to_be_bytes().to_vec(),
            Datum::Text(v)
            | Datum::Citext(v)
            | Datum::Ltree(v)
            | Datum::Hstore(v)
            | Datum::TsVector(v)
            | Datum::TsQuery(v)
            | Datum::Range { text: v, .. } => {
                v.as_bytes().to_vec()
            }
            Datum::Bytea(v) => v.clone(),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per type in the wire vocabulary, and the array half is a list of names \
                  rather than of rules: every one of them refuses, because `array_recv`'s shape \
                  has never been read here"
    )]
    fn from_binary(ty: ColumnType, bytes: &[u8]) -> Result<Datum> {
        let fixed = |width: usize| {
            (bytes.len() == width).then_some(bytes).ok_or_else(|| {
                SqlError::ProtocolViolation(format!(
                    "a binary {} is {width} bytes, not {}",
                    ty.name(),
                    bytes.len()
                ))
            })
        };
        Ok(match ty {
            // Its one value, whatever bytes arrived — there is no wire form to read.
            ColumnType::Void => Datum::Text(String::new()),
            // One byte on the wire, rendered the way its output function renders it.
            ColumnType::Char => {
                Datum::Text(char_type::render(fixed(1)?.first().copied().unwrap_or(0)))
            }
            // Four bytes, the oid, which is `oidrecv`'s shape — the name is derived, exactly as it
            // is for the `<oid>::regtype` cast.
            ColumnType::RegType => {
                let head = fixed(4)?;
                regtype_of_oid(u32::from_be_bytes(head.try_into().unwrap_or([0; 4])))
            }
            // The same four bytes, and the name is looked up here because a function's name comes
            // from a fixed table rather than from the catalog (`value::reg_proc`).
            ColumnType::RegProc => {
                let head = fixed(4)?;
                let oid = u32::from_be_bytes(head.try_into().unwrap_or([0; 4]));
                Datum::RegProc {
                    oid,
                    name: reg_proc::to_text(oid).into_boxed_str(),
                }
            }
            // The same four bytes; the name a resolvable oid prints is put on at the catalog seam.
            // **Refused, not guessed.** `int2vectorsend` writes an array header and shorts, not
            // the space-separated text; nothing here has ever sent one, so a client that does is
            // told so rather than handed a value built from a guess — the call `json` two arms
            // below makes for the same reason.
            ColumnType::Int2Vector | ColumnType::OidVector => {
                return Err(SqlError::unsupported(
                    "an int2vector or oidvector parameter in the binary format",
                ));
            }
            ColumnType::RegClass => {
                let head = fixed(4)?;
                regclass_of_oid(i64::from(u32::from_be_bytes(
                    head.try_into().unwrap_or([0; 4]),
                )))
            }
            // The mirror of `to_binary`: neither `point_recv`'s pair of coordinates, nor
            // `cash_recv`'s cents, nor `array_recv`'s shape has ever been read here, so a client
            // that sends one is told so rather than given a value built from a guess.
            ColumnType::Money
            | ColumnType::Inet
            | ColumnType::Cidr
            | ColumnType::MacAddr
            | ColumnType::Bit
            | ColumnType::VarBit
            | ColumnType::Xml
            | ColumnType::LQuery
            | ColumnType::Lseg
            | ColumnType::Box
            | ColumnType::Path
            | ColumnType::Polygon
            | ColumnType::Circle
            | ColumnType::Line
            | ColumnType::Point
            | ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray
            | ColumnType::HstoreArray
            | ColumnType::TsVectorArray
            | ColumnType::TsQueryArray
            | ColumnType::TsRangeArray
            | ColumnType::TstzRangeArray
            | ColumnType::Int4RangeArray
            | ColumnType::DateRangeArray
            | ColumnType::NumRangeArray
            | ColumnType::Int8RangeArray
            | ColumnType::PointArray
            | ColumnType::BoxArray
            | ColumnType::LsegArray
            | ColumnType::PathArray
            | ColumnType::PolygonArray
            | ColumnType::CircleArray
            | ColumnType::LineArray
            | ColumnType::MoneyArray
            | ColumnType::InetArray
            | ColumnType::CidrArray
            | ColumnType::MacAddrArray
            | ColumnType::BitArray
            | ColumnType::VarBitArray
            | ColumnType::BoolArray
            | ColumnType::ByteaArray
            | ColumnType::BpcharArray
            | ColumnType::VarcharArray
            | ColumnType::NameArray
            | ColumnType::CharArray
            | ColumnType::DateArray
            | ColumnType::TimeArray
            | ColumnType::TimestampArray
            | ColumnType::TimestampTzArray
            | ColumnType::IntervalArray
            | ColumnType::RealArray
            | ColumnType::DoubleArray
            | ColumnType::UuidArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::OidArray
            | ColumnType::RegTypeArray
            | ColumnType::RegProcArray
            | ColumnType::CitextArray
            | ColumnType::XmlArray
            | ColumnType::LtreeArray => {
                return Err(SqlError::unsupported(format!(
                    "a binary-format {}",
                    ty.name()
                )));
            }
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Time
            | ColumnType::Double => {
                let head: [u8; 8] = fixed(8)?.try_into().unwrap_or([0; 8]);
                match ty {
                    ColumnType::Int8 => Datum::Int8(i64::from_be_bytes(head)),
                    ColumnType::TimestampTz => Datum::TimestampTz(i64::from_be_bytes(head)),
                    ColumnType::Timestamp => Datum::Timestamp(i64::from_be_bytes(head)),
                    ColumnType::Time => Datum::Time(i64::from_be_bytes(head)),
                    _ => Datum::Double(f64::from_be_bytes(head)),
                }
            }
            ColumnType::Int4 => {
                let head: [u8; 4] = fixed(4)?.try_into().unwrap_or([0; 4]);
                Datum::Int4(i32::from_be_bytes(head))
            }
            ColumnType::Oid => {
                let head: [u8; 4] = fixed(4)?.try_into().unwrap_or([0; 4]);
                Datum::Oid(u32::from_be_bytes(head))
            }
            ColumnType::Real => {
                let head: [u8; 4] = fixed(4)?.try_into().unwrap_or([0; 4]);
                Datum::Real(f32::from_be_bytes(head))
            }
            ColumnType::Date => {
                let head: [u8; 4] = fixed(4)?.try_into().unwrap_or([0; 4]);
                Datum::Date(i32::from_be_bytes(head))
            }
            // `numeric_recv` reads a four-`i16` header and base-10000 digit groups. Nothing here
            // has ever sent that shape and the corpus cannot reach it — `psql` sends text — so it
            // is refused rather than guessed, the same call `json` makes two arms below.
            ColumnType::Numeric => {
                return Err(SqlError::unsupported(
                    "a numeric parameter in the binary format",
                ));
            }
            ColumnType::Uuid => {
                let head: [u8; 16] = fixed(16)?.try_into().unwrap_or([0; 16]);
                Datum::Uuid(head)
            }
            ColumnType::Interval => {
                let head: [u8; 16] = fixed(16)?.try_into().unwrap_or([0; 16]);
                Datum::Interval {
                    micros: i64::from_be_bytes(head[..8].try_into().unwrap_or([0; 8])),
                    days: i32::from_be_bytes(head[8..12].try_into().unwrap_or([0; 4])),
                    months: i32::from_be_bytes(head[12..].try_into().unwrap_or([0; 4])),
                }
            }
            ColumnType::Int2 => {
                let head: [u8; 2] = fixed(2)?.try_into().unwrap_or([0; 2]);
                Datum::Int2(i16::from_be_bytes(head))
            }
            ColumnType::Bool => match fixed(1)?[0] {
                0 => Datum::Bool(false),
                1 => Datum::Bool(true),
                other => {
                    return Err(SqlError::ProtocolViolation(format!(
                        "a binary boolean is 0 or 1, not {other}"
                    )));
                }
            },
            // A `json` or `jsonb` **binary** parameter is its text with a leading version byte
            // on a real server; this node has never sent one and the corpus does not cover it, so
            // it is refused rather than guessed. The text path is what a client actually uses.
            // An hstore arrives as its own text and is canonicalised on the way in, exactly as
            // it is from the text format — the wire carries the printed form either way.
            ColumnType::Hstore => Datum::Hstore(hstore::to_text(&hstore::from_text(
                std::str::from_utf8(bytes).map_err(|_| {
                    SqlError::ProtocolViolation("a binary hstore is not UTF-8".into())
                })?,
            )?)),
            ColumnType::TsVector => Datum::TsVector(tsvector::to_text(&tsvector::from_text(
                std::str::from_utf8(bytes).map_err(|_| {
                    SqlError::ProtocolViolation("a binary tsvector is not UTF-8".into())
                })?,
            )?)),
            ColumnType::TsQuery => Datum::TsQuery(tsquery::to_text(&tsquery::from_text(
                std::str::from_utf8(bytes).map_err(|_| {
                    SqlError::ProtocolViolation("a binary tsquery is not UTF-8".into())
                })?,
            )?)),
            // The wire carries the printed form either way, so this is `from_text`'s road with the
            // bytes read first.
            ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range
            | ColumnType::DateRange
            | ColumnType::NumRange
            | ColumnType::Int8Range
            | ColumnType::FloatRange
            | ColumnType::VarcharRange => binary_range(ty, bytes)?,
            ColumnType::Json | ColumnType::Jsonb => {
                return Err(SqlError::unsupported(
                    "a json or jsonb parameter in the binary format",
                ));
            }
            // A citext arrives as its own text and keeps its spelling, the same as from the text
            // format; only the comparison folds.
            // An ltree the same way, and **validated**: a binary parameter is still the path's
            // characters, so a client that sends `a..b` gets `ltree`'s own syntax error rather
            // than a row holding something that is not a path.
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Citext
            | ColumnType::Ltree => binary_text(ty, bytes)?,
            // The same truncation: a parameter sent in the binary format is still a `name`, and
            // `namerecv` cuts it exactly as `namein` does.
            ColumnType::Name => match binary_text(ty, bytes)? {
                Datum::Text(text) => Datum::Text(truncate_to_name(&text)),
                other => other,
            },
            ColumnType::Bytea => Datum::Bytea(bytes.to_vec()),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per comparable pair of variants; the list is the vocabulary"
    )]
    fn pg_cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Datum::Null, Datum::Null) => Ordering::Equal,
            // NULLS LAST, PostgreSQL's default for ascending order.
            (Datum::Null, _) => Ordering::Greater,
            (_, Datum::Null) => Ordering::Less,
            // **`array_cmp`**: element by element over the flattened elements, a NULL element
            // above every value, then by how many there are and by the shape. Measured:
            // `'{}'` sorts first, `'{1,2,3}'` before `'{{1,2},{3,4}}'` — four elements, longer —
            // and `'{1,NULL,3}'` after both. `esker_keys::row::encode_key_array` writes the same
            // order into an index key, and the two have to agree or a scan and a sort disagree.
            (Datum::Array(a), Datum::Array(b)) => {
                for (left, right) in a.values.iter().zip(&b.values) {
                    let ordering = match (left, right) {
                        (Some(left), Some(right)) => left.pg_cmp(right),
                        (None, None) => Ordering::Equal,
                        // A NULL element sorts **above** a value, which is the opposite of what
                        // `pg_cmp` does for a NULL *row* value and is measured for this one.
                        (None, Some(_)) => Ordering::Greater,
                        (Some(_), None) => Ordering::Less,
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                a.values
                    .len()
                    .cmp(&b.values.len())
                    .then_with(|| a.dims.cmp(&b.dims))
                    .then_with(|| a.lower.cmp(&b.lower))
            }
            // **The `cidr` flag is not compared**, which is what makes `inet = cidr` true: the
            // family first (every IPv4 below every IPv6), then the address, then the prefix
            // length — PostgreSQL's own order, and the one the key encoding reproduces byte for
            // byte.
            (
                Datum::Inet {
                    family: af,
                    bits: ab,
                    addr: aa,
                    ..
                },
                Datum::Inet {
                    family: bf,
                    bits: bb,
                    addr: ba,
                    ..
                },
            ) => af.cmp(bf).then_with(|| aa.cmp(ba)).then_with(|| ab.cmp(bb)),
            (Datum::MacAddr(a), Datum::MacAddr(b)) => a.cmp(b),
            // **The digits and not the flag**, which is what makes `bit varying = bit` true.
            // PostgreSQL compares bit by bit and then by length; `'0'` below `'1'` and a prefix
            // below what extends it is exactly that.
            (Datum::Bit { bits: a, .. }, Datum::Bit { bits: b, .. }) => a.as_bytes().cmp(b.as_bytes()),
            // **A `regclass` is an `i64` too**, and it compares with an `int8` in both
            // directions: every relation-oid column in this node's catalog is a `bigint`, so
            // `WHERE attrelid = 'iv'::regclass` is exactly this pair.
            (
                Datum::Int8(a) | Datum::RegClass { oid: a, .. },
                Datum::Int8(b) | Datum::RegClass { oid: b, .. },
            )
            // Plain integer order, and only against another `time`: this type compares with
            // nothing else, so there is no promotion arm to write beside it.
            | (Datum::Time(a), Datum::Time(b))
            // **A money is the same shape**: cents in an `i64`, compared with cents and with
            // nothing else — `money = numeric` and `money = bigint` are each `42883` on a real
            // server, and `same_family` refuses the pair before this is reached.
            | (Datum::Money(a), Datum::Money(b))
            // **The two timestamps compare across the zone as well as within it**, because
            // PostgreSQL has a `timestamp = timestamptz` operator and
            // `created_at = transaction_timestamp()` over a `timestamp` column is `t`. The
            // conversion is through the session zone there and the identity here: this node
            // honours `TimeZone` only where it means UTC (`crate::parameter`), so a `timestamptz`
            // and a `timestamp` holding the same microseconds are the same instant. Without the
            // mixed pairs the comparison fell through to the cross-variant order below and
            // answered `f` — a value where a real server answers `t`, which ADR 0031 ranks worst.
            | (
                Datum::Timestamp(a) | Datum::TimestampTz(a),
                Datum::Timestamp(b) | Datum::TimestampTz(b),
            ) => a.cmp(b),
            // Across the two widths, because PostgreSQL has an `int4 = int8` operator and answers
            // `1::integer = 1::bigint` with `t`. Widening is exact in this direction, so there is
            // no rounding to argue about — an `i32` is an `i64`.
            // **A date is the midnight it names**, which is how `'2020-01-01'::date =
            // '2020-01-01'::timestamp` is `t` on a real server. The infinities are the ends of
            // both types and stay at the ends after the promotion, so the comparison is total
            // without a special case for them.
            (Datum::Date(a), Datum::Timestamp(b) | Datum::TimestampTz(b)) => {
                date::as_micros(*a).cmp(b)
            }
            (Datum::Timestamp(a) | Datum::TimestampTz(a), Datum::Date(b)) => {
                a.cmp(&date::as_micros(*b))
            }
            (Datum::Numeric(a), Datum::Numeric(b)) => numeric::pg_cmp(a, b),
            // **Exact against an integer, lossy against a float** — PostgreSQL's own promotion
            // rule, and the reason the two pairings are written separately rather than folded
            // into one `as_f64`: `numeric + int4` stays `numeric` there and `numeric + float8`
            // does not, so a comparison that went through `f64` for both would answer `t` for a
            // pair of integers a real server tells apart.
            (Datum::Numeric(a), Datum::Int8(b)) => numeric::pg_cmp(a, &numeric::of_i64(*b)),
            (Datum::Int8(a), Datum::Numeric(b)) => numeric::pg_cmp(&numeric::of_i64(*a), b),
            (Datum::Numeric(a), Datum::Int4(b)) => {
                numeric::pg_cmp(a, &numeric::of_i64(i64::from(*b)))
            }
            (Datum::Int4(a), Datum::Numeric(b)) => {
                numeric::pg_cmp(&numeric::of_i64(i64::from(*a)), b)
            }
            (Datum::Numeric(a), Datum::Int2(b)) => {
                numeric::pg_cmp(a, &numeric::of_i64(i64::from(*b)))
            }
            (Datum::Int2(a), Datum::Numeric(b)) => {
                numeric::pg_cmp(&numeric::of_i64(i64::from(*a)), b)
            }
            (Datum::Numeric(a), Datum::Double(b)) => {
                Datum::Double(numeric::as_f64(a)).pg_cmp(&Datum::Double(*b))
            }
            (Datum::Double(a), Datum::Numeric(b)) => {
                Datum::Double(*a).pg_cmp(&Datum::Double(numeric::as_f64(b)))
            }
            (Datum::Numeric(a), Datum::Real(b)) => {
                Datum::Double(numeric::as_f64(a)).pg_cmp(&Datum::Double(f64::from(*b)))
            }
            (Datum::Real(a), Datum::Numeric(b)) => {
                Datum::Double(f64::from(*a)).pg_cmp(&Datum::Double(numeric::as_f64(b)))
            }
            (Datum::Int4(a), Datum::Int4(b)) | (Datum::Date(a), Datum::Date(b)) => a.cmp(b),
            // `uuid_cmp` is a `memcmp`, so this is the type's whole ordering.
            // **An `oid` is a number and compares as one.** Without these it would fall through
            // to the variant rank — which it shares with the integers — and every pair would
            // come back equal, which is the bug the `time` unit shipped and its ordering fixture
            // caught. Widened to `i64`, where every `u32` and every `i32` both fit exactly.
            // **A `regtype` is one of them**, and by its oid alone: measured,
            // `'text'::regtype < 'int4'::regtype` is false because it is `25 < 23` and not the
            // names, and `'text'::regtype = 25` is true against an uncast integer. That is the
            // whole model (ADR 0077), and it is why the pairs are shared rather than repeated.
            (
                Datum::Oid(a) | Datum::RegType { oid: a, .. } | Datum::RegProc { oid: a, .. },
                Datum::Oid(b) | Datum::RegType { oid: b, .. } | Datum::RegProc { oid: b, .. },
            ) => a.cmp(b),
            (
                Datum::Oid(a) | Datum::RegType { oid: a, .. } | Datum::RegProc { oid: a, .. },
                Datum::Int8(b),
            ) => i64::from(*a).cmp(b),
            (
                Datum::Int8(a),
                Datum::Oid(b) | Datum::RegType { oid: b, .. } | Datum::RegProc { oid: b, .. },
            ) => a.cmp(&i64::from(*b)),
            (Datum::RegClass { oid: a, .. }, Datum::Oid(b)) => a.cmp(&i64::from(*b)),
            (Datum::Oid(a), Datum::RegClass { oid: b, .. }) => i64::from(*a).cmp(b),
            (Datum::Oid(a), Datum::Int4(b)) => i64::from(*a).cmp(&i64::from(*b)),
            (Datum::Int4(a), Datum::Oid(b)) => i64::from(*a).cmp(&i64::from(*b)),
            (Datum::Oid(a), Datum::Int2(b)) => i64::from(*a).cmp(&i64::from(*b)),
            (Datum::Int2(a), Datum::Oid(b)) => i64::from(*a).cmp(&i64::from(*b)),
            (Datum::Uuid(a), Datum::Uuid(b)) => a.cmp(b),
            // **Converted, not compared field by field**: a month is thirty days and a day is
            // twenty-four hours, so `'1 mon'` and `'30 days'` are equal here and different rows.
            (
                Datum::Interval {
                    months: am,
                    days: ad,
                    micros: au,
                },
                Datum::Interval {
                    months: bm,
                    days: bd,
                    micros: bu,
                },
            ) => esker_keys::row::interval_total(*am, *ad, *au)
                .cmp(&esker_keys::row::interval_total(*bm, *bd, *bu)),
            (Datum::Int2(a), Datum::Int2(b)) => a.cmp(b),
            (Datum::Int2(a), Datum::Int4(b)) => i32::from(*a).cmp(b),
            (Datum::Int4(a), Datum::Int2(b)) => a.cmp(&i32::from(*b)),
            (Datum::Int2(a), Datum::Int8(b)) => i64::from(*a).cmp(b),
            (Datum::Int8(a), Datum::Int2(b)) => a.cmp(&i64::from(*b)),
            (Datum::Int4(a), Datum::Int8(b)) => i64::from(*a).cmp(b),
            (Datum::Int8(a), Datum::Int4(b)) => a.cmp(&i64::from(*b)),
            // Byte order, not the database's collation: see `crate::row` for why that is a
            // decision and not an oversight.
            // An hstore's comparison **is** text's — the canonical form is a function of the
            // content — which is the half citext does not share.
            // A shape joins them: the canonical text **is** the value, the road `hstore` and the
            // ranges take. It is not PostgreSQL's geometric order, which is by area or by length —
            // none of the six is an index key, so nothing here has to reproduce one.
            (Datum::Geometry { text: a, .. }, Datum::Geometry { text: b, .. })
            | (Datum::Text(a), Datum::Text(b))
            | (Datum::Hstore(a), Datum::Hstore(b))
            | (Datum::TsVector(a), Datum::TsVector(b))
            | (Datum::TsQuery(a), Datum::TsQuery(b)) => {
                a.as_bytes().cmp(b.as_bytes())
            }
            // **A range compares by its canonical text**, which is right because the text *is*
            // canonical: two ranges print the same exactly when they are the same range. Without
            // this arm the match falls through to the cross-type rank below, which says every
            // range equals every other — `count(DISTINCT ts_range)` answered 1 where a real
            // server says 5, and only a corpus row could see it. The same trap citext's
            // `PartialEq` fell into; a pair-exhaustive match has no compiler to remind it.
            //
            // The *order* is a different question and the text answers it wrong: sorted as text,
            // `empty` comes last where PostgreSQL puts it first, and `[10,21)` comes before
            // `[2,4)` because `1` precedes `2`. Both are wrong answers to `ORDER BY`, so the
            // bounds are compared as bounds — see [`range_cmp`], which agrees with the text
            // wherever the text is right, equality included.
            (
                Datum::Range {
                    subtype: sa,
                    text: a,
                },
                Datum::Range {
                    subtype: sb,
                    text: b,
                },
            ) => range_cmp(**sa, a, **sb, b),
            // **A citext compares folded**, which is the whole type: `'ABC' = 'abc'` is true, two
            // rows differing only in case are one group and one `DISTINCT`, and a unique index
            // over the column refuses the second. It is the *comparison* that folds and never the
            // value — `to_text` above returns the spelling that was stored.
            //
            // Folded with `to_lowercase`, which is Unicode's own mapping and not an ASCII one:
            // measured, `'Ä'::citext = 'ä'::citext` is `t` on a real server, and an ASCII-only
            // fold answers `f` there and is wrong for every non-English application.
            (Datum::Citext(a), Datum::Citext(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
            // **Label by label, not byte by byte** — `'a.b' < 'a-b'` is true as an ltree and
            // false as bytes, which is the whole of why this type has a `Datum` of its own
            // (`crate::value::ltree`). An `unknown` literal on one side takes the ltree's
            // comparison, as a citext's does, because that is how the operator resolves.
            (Datum::Ltree(a), Datum::Ltree(b)) => ltree::cmp(a, b),
            (Datum::Ltree(a), Datum::Text(b)) | (Datum::Text(b), Datum::Ltree(a)) => {
                ltree::cmp(a, b)
            }
            // An `unknown` literal on one side, which is what `cival = 'cased text'` is after
            // lowering: it takes the citext's comparison rather than text's, exactly as a real
            // server resolves the operator to `citext = citext`.
            (Datum::Citext(a), Datum::Text(b)) | (Datum::Text(b), Datum::Citext(a)) => {
                a.to_lowercase().cmp(&b.to_lowercase())
            }
            (Datum::Bool(a), Datum::Bool(b)) => a.cmp(b),
            (Datum::Bytea(a), Datum::Bytea(b)) => a.cmp(b),
            (Datum::Double(a), Datum::Double(b)) => float::pg_cmp(*a, *b),
            (Datum::Real(a), Datum::Real(b)) => float::pg_cmp_f32(*a, *b),
            // The two float widths compare as one type, as the integers do.
            (Datum::Real(a), Datum::Double(b)) => float::pg_cmp(f64::from(*a), *b),
            (Datum::Double(a), Datum::Real(b)) => float::pg_cmp(*a, f64::from(*b)),
            // **A float against an integer**, which PostgreSQL has an operator for at every width
            // and which was missing here: without these the pair fell through to the variant rank
            // below, where a `double precision` and an `int8` are different variants and every
            // comparison between them came back the same way whatever the numbers were. Found by
            // `SELECT random() >= 0 AND random() < 1`, which is `t` on a real server and was `f`
            // here.
            //
            // **Exactly**, not by widening the integer — see `float::pg_cmp_int` for the pair of
            // captured statements that decide it.
            (Datum::Double(a), Datum::Int8(b)) => float::pg_cmp_int(*b, *a).reverse(),
            (Datum::Int8(a), Datum::Double(b)) => float::pg_cmp_int(*a, *b),
            (Datum::Double(a), Datum::Int4(b)) => float::pg_cmp_int(i64::from(*b), *a).reverse(),
            (Datum::Int4(a), Datum::Double(b)) => float::pg_cmp_int(i64::from(*a), *b),
            (Datum::Double(a), Datum::Int2(b)) => float::pg_cmp_int(i64::from(*b), *a).reverse(),
            (Datum::Int2(a), Datum::Double(b)) => float::pg_cmp_int(i64::from(*a), *b),
            (Datum::Real(a), Datum::Int8(b)) => float::pg_cmp_int(*b, f64::from(*a)).reverse(),
            (Datum::Int8(a), Datum::Real(b)) => float::pg_cmp_int(*a, f64::from(*b)),
            (Datum::Real(a), Datum::Int4(b)) => {
                float::pg_cmp_int(i64::from(*b), f64::from(*a)).reverse()
            }
            (Datum::Int4(a), Datum::Real(b)) => float::pg_cmp_int(i64::from(*a), f64::from(*b)),
            (Datum::Real(a), Datum::Int2(b)) => {
                float::pg_cmp_int(i64::from(*b), f64::from(*a)).reverse()
            }
            (Datum::Int2(a), Datum::Real(b)) => float::pg_cmp_int(i64::from(*a), f64::from(*b)),
            (a, b) => variant_rank(a).cmp(&variant_rank(b)),
        }
    }
}

/// Which variant a value is, for the total order `pg_cmp` puts across types.
///
/// A free function rather than a method: [`Datum`] belongs to `esker-keys` now, so this crate
/// cannot give it inherent methods — and this one is an implementation detail of the ordering
/// rather than something a client can ask for.
fn variant_rank(value: &Datum) -> u8 {
    match value {
        // The rank an `Oid` has, because the value **is** an oid and the two must not sort into
        // separate blocks: `'text'::regtype = 25` is true, so they are one family.
        Datum::RegType { .. } | Datum::RegProc { .. } | Datum::RegClass { .. } => {
            variant_rank(&Datum::Oid(0))
        }
        // Above every scalar, which only decides the order between two values of *different*
        // types — a comparison SQL does not have and this crate's total order still needs.
        // **Ranked, and neither is a SQL order.** An array's rank decides only the order
        // between two values of *different* types; a point's is never reached at all, because
        // `point = point` is `42883` — but `pg_cmp` is total by construction and a value with
        // no rank would sort as some other type's.
        Datum::Array(_) | Datum::Point { .. } => 20,
        Datum::Bool(_) => 0,
        // **Its own rank**, above every number: a money never meets one in a comparison a schema
        // can produce (`same_family` refuses the pair), and a value with no rank of its own would
        // sort as some other type's.
        Datum::Money(_) => 22,
        // A rank each, above the numbers: neither meets one in a comparison a schema can produce.
        Datum::Inet { .. } => 23,
        Datum::MacAddr(_) => 24,
        Datum::Bit { .. } => 25,
        Datum::Geometry { .. } => 26,
        // The two integer widths share a rank: they are one type to a comparison, and `pg_cmp`
        // answers the pair above rather than falling through to here.
        // An `oid` shares the integers' rank: it is one, and `pg_cmp` answers every pairing
        // above rather than falling through to here.
        Datum::Int8(_) | Datum::Int4(_) | Datum::Int2(_) | Datum::Oid(_) => 1,
        Datum::Double(_) | Datum::Real(_) => 2,
        Datum::TimestampTz(_) | Datum::Timestamp(_) | Datum::Date(_) => 3,
        Datum::Numeric(_) => 7,
        // Its own rank: a uuid compares with a uuid and with nothing else.
        Datum::Uuid(_) => 9,
        Datum::Interval { .. } => 10,
        // Its own rank, because it is its own family: a `time` compares with a `time` and with
        // nothing else, so this rank exists to give the cross-type order a total answer rather
        // than to describe an operator a real server has.
        Datum::Time(_) => 8,
        Datum::Text(_)
        | Datum::Citext(_)
        | Datum::Ltree(_)
        | Datum::Hstore(_)
        | Datum::TsVector(_)
        | Datum::TsQuery(_) => 4,
        // Its own rank in the cross-type total order, above every scalar's text.
        Datum::Range { .. } => 21,
        Datum::Bytea(_) => 5,
        Datum::Null => 6,
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// PostgreSQL's `int2in`, one width below [`parse_int4`] and the same shape.
///
/// The name in the error is `smallint`, measured: `invalid input syntax for type smallint: "x"`
/// and `value "32768" is out of range for type smallint`.
fn parse_int2(text: &str) -> Result<i16> {
    let wide = parse_int8(text).map_err(|error| match error {
        SqlError::InvalidTextRepresentation { .. } => SqlError::InvalidTextRepresentation {
            ty: ColumnType::Int2.name(),
            value: text.to_owned(),
        },
        SqlError::IntegerOutOfRange { .. } => SqlError::IntegerOutOfRange {
            ty: ColumnType::Int2.name(),
            value: text.to_owned(),
        },
        other => other,
    })?;
    i16::try_from(wide).map_err(|_| SqlError::IntegerOutOfRange {
        ty: ColumnType::Int2.name(),
        value: text.to_owned(),
    })
}

/// PostgreSQL's `int4in`, which is `pg_strtoint32_safe` — the same lexer as the 64-bit one with a
/// narrower accumulator, so `0x` literals and `_` separators are taken here too.
///
/// It is **not** `parse_int8` with a range check afterwards, and the difference is the message: a
/// real server says `value "2147483648" is out of range for type integer`, naming `integer`, where
/// running the wide parser first and narrowing would name `bigint` for a value that is a perfectly
/// good `bigint`. Measured on 19beta1.
fn parse_int4(text: &str) -> Result<i32> {
    let wide = parse_int8(text).map_err(|error| match error {
        SqlError::InvalidTextRepresentation { .. } => SqlError::InvalidTextRepresentation {
            ty: ColumnType::Int4.name(),
            value: text.to_owned(),
        },
        SqlError::IntegerOutOfRange { .. } => SqlError::IntegerOutOfRange {
            ty: ColumnType::Int4.name(),
            value: text.to_owned(),
        },
        other => other,
    })?;
    i32::try_from(wide).map_err(|_| SqlError::IntegerOutOfRange {
        ty: ColumnType::Int4.name(),
        value: text.to_owned(),
    })
}

/// PostgreSQL's `pg_strtoint64_safe`, which is more than a decimal parser: it takes the
/// non-decimal literals the lexer gained in PostgreSQL 16 and the digit separators with them.
fn parse_int8(text: &str) -> Result<i64> {
    let bad = || SqlError::InvalidTextRepresentation {
        ty: ColumnType::Int8.name(),
        value: text.to_owned(),
    };
    let overflow = || SqlError::IntegerOutOfRange {
        ty: ColumnType::Int8.name(),
        value: text.to_owned(),
    };

    let body = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match body.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, body.strip_prefix('+').unwrap_or(body)),
    };
    let (radix, digits) = match digits.as_bytes() {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', b'o' | b'O', rest @ ..] => (8, rest),
        [b'0', b'b' | b'B', rest @ ..] => (2, rest),
        rest => (10, rest),
    };
    if digits.is_empty() {
        return Err(bad());
    }

    // Accumulated as a negative number so that i64::MIN, which has no positive counterpart, needs
    // no special case.
    let mut value: i64 = 0;
    let mut last_was_digit = false;
    for &byte in digits {
        if byte == b'_' {
            // A separator is only a separator between two digits: `_1` and `1_` are errors.
            if !last_was_digit {
                return Err(bad());
            }
            last_was_digit = false;
            continue;
        }
        let digit = (byte as char).to_digit(radix).ok_or_else(bad)?;
        value = value
            .checked_mul(i64::from(radix))
            .and_then(|v| v.checked_sub(i64::from(digit)))
            .ok_or_else(overflow)?;
        last_was_digit = true;
    }
    if !last_was_digit {
        return Err(bad());
    }

    if negative {
        Ok(value)
    } else {
        value.checked_neg().ok_or_else(overflow)
    }
}

/// PostgreSQL's `parse_bool_with_len`: case-insensitive, whitespace-trimmed, and satisfied by any
/// prefix that can only be one word. `o` is the one that cannot, because `on` and `off` both start
/// with it.
/// Two ranges, ordered the way PostgreSQL orders them.
///
/// **Not their canonical text's order**, which is what this used to be and is wrong twice:
/// `empty` sorts *first* on a real server and last as text, and `[10,21)` sorts *after* `[2,4)`
/// where the text puts it first. Measured — `ORDER BY float_range` over `range_test.rb`'s own
/// fixtures is `empty`, `(,)`, `[-Infinity,Infinity]`, `[0.5,0.7)`, `[0.5,0.7]`, `[0.5,)`.
///
/// Four rules, in this order, and each of the last three has a side that is *absent*:
///
/// 1. `empty` is below every non-empty range.
/// 2. The lower bound, where **absent is unbounded below** and sorts first — `(,)` before
///    `[-Infinity,Infinity]`, because `-Infinity` is a `float8` *value* and not an absent bound.
/// 3. On an equal lower bound, **inclusive first**: `[0.5,` before `(0.5,`.
/// 4. Then the upper bound, where **absent is unbounded above** and sorts *last*, and on an equal
///    one **exclusive first**: `[0.5,0.7)` before `[0.5,0.7]` before `[0.5,)`.
///
/// A text that will not parse falls back to comparing the text, so the function stays total: the
/// only way to get one is a value this node did not write.
fn range_cmp(
    left_subtype: ColumnType,
    left: &str,
    right_subtype: ColumnType,
    right: &str,
) -> Ordering {
    let (Ok(a), Ok(b)) = (
        range::from_text(left_subtype, left),
        range::from_text(right_subtype, right),
    ) else {
        return left.as_bytes().cmp(right.as_bytes());
    };
    match (a.empty, b.empty) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    let lower = match (&a.lower, &b.lower) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => x.pg_cmp(y),
    };
    // `true` before `false`, which `bool`'s own order has backwards.
    let lower = lower.then_with(|| b.lower_inc.cmp(&a.lower_inc));
    let upper = match (&a.upper, &b.upper) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.pg_cmp(y),
    };
    lower
        .then(upper)
        .then_with(|| a.upper_inc.cmp(&b.upper_inc))
}

fn parse_bool(text: &str) -> Result<bool> {
    let body = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let lower = body.to_ascii_lowercase();
    let prefix_of = |word: &str| !lower.is_empty() && word.starts_with(lower.as_str());
    match lower.as_bytes().first() {
        Some(b't') if prefix_of("true") => Ok(true),
        Some(b'f') if prefix_of("false") => Ok(false),
        Some(b'y') if prefix_of("yes") => Ok(true),
        Some(b'n') if prefix_of("no") => Ok(false),
        Some(b'o') if lower.len() >= 2 && prefix_of("on") => Ok(true),
        Some(b'o') if lower.len() >= 2 && prefix_of("off") => Ok(false),
        Some(b'1') if lower.len() == 1 => Ok(true),
        Some(b'0') if lower.len() == 1 => Ok(false),
        _ => Err(SqlError::InvalidTextRepresentation {
            ty: ColumnType::Bool.name(),
            value: text.to_owned(),
        }),
    }
}

/// PostgreSQL's `byteain`: the hex format when the text starts with a lowercase `\x`, and the
/// legacy escape format otherwise.
fn parse_bytea(text: &str) -> Result<Vec<u8>> {
    match text.strip_prefix("\\x") {
        Some(hex) => parse_bytea_hex(hex),
        None => parse_bytea_escape(text),
    }
}

fn parse_bytea_hex(hex: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut high: Option<u8> = None;
    for character in hex.chars() {
        // Whitespace between digits is ignored, which is what makes `\x de ad` legal.
        if character.is_ascii_whitespace() {
            continue;
        }
        let nibble = character
            .to_digit(16)
            .ok_or(SqlError::InvalidHexDigit(character))?;
        #[allow(clippy::cast_possible_truncation, reason = "a hex digit is four bits")]
        let nibble = nibble as u8;
        match high.take() {
            None => high = Some(nibble),
            Some(first) => out.push((first << 4) | nibble),
        }
    }
    if high.is_some() {
        return Err(SqlError::OddHexDigits);
    }
    Ok(out)
}

fn parse_bytea_escape(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            // Exactly three octal digits, and no more than a byte's worth. `\1` and `\777` are
            // both errors, which a capture confirmed against a guess that either might work.
            Some(_) => {
                let digits = bytes
                    .get(i + 1..i + 4)
                    .ok_or(SqlError::InvalidByteaFormat)?;
                let mut value: u32 = 0;
                for &digit in digits {
                    let digit = (digit as char)
                        .to_digit(8)
                        .ok_or(SqlError::InvalidByteaFormat)?;
                    value = value * 8 + digit;
                }
                out.push(u8::try_from(value).map_err(|_| SqlError::InvalidByteaFormat)?);
                i += 4;
            }
            None => return Err(SqlError::InvalidByteaFormat),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{ColumnType, Datum, MAX_MICROS, MIN_MICROS, NEG_INFINITY, POS_INFINITY};
    use super::{PgDatum, PgType};
    use crate::sqlstate;
    use std::cmp::Ordering;

    /// These OIDs go on the wire in `RowDescription`. A client picks its decoder from them, so a
    /// wrong one is not a wrong label -- it is a wrong parse of every value in the column.
    #[test]
    fn the_oids_are_postgresqls_own() {
        assert_eq!(ColumnType::Bool.oid(), 16);
        assert_eq!(ColumnType::Bytea.oid(), 17);
        assert_eq!(ColumnType::Int8.oid(), 20);
        assert_eq!(ColumnType::Text.oid(), 25);
        assert_eq!(ColumnType::Double.oid(), 701);
        assert_eq!(ColumnType::TimestampTz.oid(), 1184);
        // Tier 1 (ADR 0033), each measured off `pg_type` rather than remembered.
        assert_eq!(ColumnType::Int4.oid(), 23);
        assert_eq!(ColumnType::Varchar.oid(), 1043);
        assert_eq!(ColumnType::Timestamp.oid(), 1114);
        assert_eq!(ColumnType::Real.oid(), 700);
        assert_eq!(ColumnType::Bpchar.oid(), 1042);
        // Tier 2's first pair (ADR 0042).
        assert_eq!(ColumnType::Json.oid(), 114);
        assert_eq!(ColumnType::Jsonb.oid(), 3802);
        assert_eq!(ColumnType::Numeric.oid(), 1700);
        // **Not PostgreSQL's own, and deliberately so.** An extension's types are allocated when
        // it is installed, so hstore's oid is above 16384 and differs per database on a real
        // server; a client reads it by `typname`, which is what `ActiveRecord` does. Asserted here
        // so that the exception is on the record rather than looking like an oversight.
        assert_eq!(ColumnType::Hstore.oid(), 16400);
        assert_eq!(ColumnType::HstoreArray.oid(), 16401);
        assert_eq!(ColumnType::Citext.oid(), 16402);
        // The range types are PostgreSQL's own and fixed, unlike an extension's.
        assert_eq!(ColumnType::TsRange.oid(), 3908);
        assert_eq!(ColumnType::TstzRange.oid(), 3910);
        assert_eq!(ColumnType::Int4Range.oid(), 3904);
        for ty in ColumnType::ALL {
            assert_eq!(
                ty.type_len() == -1,
                matches!(
                    ty,
                    ColumnType::Text
                        | ColumnType::TsVector
                        | ColumnType::TsQuery
                        | ColumnType::Varchar
                        | ColumnType::Bpchar
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        | ColumnType::Xml
                        | ColumnType::Ltree
                        | ColumnType::LQuery
                        | ColumnType::Hstore
                        | ColumnType::HstoreArray
                        | ColumnType::TsVectorArray
                        | ColumnType::TsQueryArray
                        | ColumnType::Citext
                        | ColumnType::TsRange
                        | ColumnType::TstzRange
                        | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                        | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                        | ColumnType::Inet | ColumnType::Cidr | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray | ColumnType::Bit | ColumnType::VarBit | ColumnType::BitArray | ColumnType::VarBitArray | ColumnType::Path | ColumnType::Polygon
                        | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::CitextArray
                        | ColumnType::XmlArray
                        | ColumnType::LtreeArray
                        | ColumnType::Bytea
                        // Variable width for the same reason as a string: the digits a value
                        // carries are the value, and `numeric(10,2)` bounds them in the typmod,
                        // not in the type.
                        | ColumnType::Numeric
                        // However many elements it has, which is the definition of a varlena.
                        | ColumnType::Int8Array
                        | ColumnType::Int4Array
        | ColumnType::Int2Array
                        | ColumnType::NumericArray
                        | ColumnType::TextArray
                ),
                "{ty:?} reports the wrong width"
            );
        }
        // And the fixed widths are the type's own, not "eight because it fits": a client sizes a
        // binary column from this, so an `int4` claiming eight is a wrong parse of every value.
        assert_eq!(ColumnType::Int4.type_len(), 4);
        assert_eq!(ColumnType::Timestamp.type_len(), 8);
    }

    /// PostgreSQL complains about `bigint`, not about `int8`. The DDL spelling and the message
    /// spelling are different words for the six types and a parity test compares the message.
    #[test]
    fn the_names_are_the_ones_that_appear_in_messages() {
        assert_eq!(ColumnType::Int8.name(), "bigint");
        assert_eq!(ColumnType::Double.name(), "double precision");
        assert_eq!(ColumnType::TimestampTz.name(), "timestamp with time zone");
    }

    /// NULL on the wire is a length of -1, not an empty string, and the two must not collapse.
    #[test]
    fn null_prints_as_no_text_at_all_and_an_empty_string_prints_as_one() {
        assert_eq!(Datum::Null.to_text(), None);
        assert_eq!(Datum::Text(String::new()).to_text(), Some(String::new()));
        assert_eq!(Datum::Null.column_type(), None);
        assert!(Datum::Null.fits(ColumnType::Int8) && Datum::Null.fits(ColumnType::Text));
        assert!(!Datum::Int8(1).fits(ColumnType::Text));
    }

    /// Three rules PostgreSQL confirmed and IEEE contradicts: one NaN, above everything; `-0.0`
    /// tied with `0.0`; and NULL last. `crate::row` encodes index keys to reproduce this order,
    /// so getting it wrong here would be a wrong answer to `ORDER BY`, not just a wrong sort.
    #[test]
    fn values_sort_the_way_postgresql_sorts_them() {
        let ordered = [
            Datum::Double(f64::NEG_INFINITY),
            Datum::Double(-1.0),
            Datum::Double(-0.0),
            Datum::Double(0.0),
            Datum::Double(1.0),
            Datum::Double(f64::INFINITY),
            Datum::Double(f64::NAN),
            Datum::Null,
        ];
        for (index, left) in ordered.iter().enumerate() {
            for right in &ordered[index + 1..] {
                assert_ne!(
                    left.pg_cmp(right),
                    Ordering::Greater,
                    "{left:?} must not sort after {right:?}"
                );
            }
        }
        assert_eq!(
            Datum::Double(-0.0).pg_cmp(&Datum::Double(0.0)),
            Ordering::Equal,
            "PostgreSQL says -0.0 = 0.0"
        );
        assert_eq!(
            Datum::Double(f64::NAN).pg_cmp(&Datum::Double(f64::NAN)),
            Ordering::Equal,
            "PostgreSQL says NaN = NaN"
        );
        assert_eq!(
            Datum::Null.pg_cmp(&Datum::Null),
            Ordering::Equal,
            "two NULLs tie in the sort even though NULL = NULL is unknown"
        );
    }

    /// Equality here is about bytes surviving a round trip, which is the opposite question from
    /// the one `pg_cmp` answers. Both are needed and neither may stand in for the other.
    #[test]
    fn equality_is_bitwise_where_the_sort_order_is_not() {
        assert_ne!(Datum::Double(-0.0), Datum::Double(0.0));
        assert_eq!(Datum::Double(f64::NAN), Datum::Double(f64::NAN));
    }

    /// The two ends of the type's range, which a real PostgreSQL 19 confirmed by accepting the
    /// first of each pair and refusing the second. `MIN_MICROS` is the start of Julian day 0.
    #[test]
    fn the_range_ends_where_postgresqls_does() {
        assert_eq!(MIN_MICROS, -2_451_545 * 86_400 * 1_000_000);
        assert_eq!(
            Datum::TimestampTz(MIN_MICROS).to_text().as_deref(),
            Some("4714-11-24 00:00:00+00 BC")
        );
        assert_eq!(
            Datum::TimestampTz(MAX_MICROS).to_text().as_deref(),
            Some("294276-12-31 23:59:59.999999+00")
        );
        assert!(MIN_MICROS > NEG_INFINITY && MAX_MICROS < POS_INFINITY);
    }

    /// The sentinels are values, not overflow: `infinity` must not be confusable with the largest
    /// instant, or a comparison against it would be wrong at exactly one point.
    #[test]
    fn the_infinities_print_as_words() {
        assert_eq!(
            Datum::TimestampTz(POS_INFINITY).to_text().as_deref(),
            Some("infinity")
        );
        assert_eq!(
            Datum::TimestampTz(NEG_INFINITY).to_text().as_deref(),
            Some("-infinity")
        );
    }

    /// An instant one microsecond past the end is out of range rather than wrapped.
    #[test]
    fn an_instant_past_the_end_is_refused_and_not_wrapped() {
        let error = Datum::from_text(ColumnType::TimestampTz, "294277-01-01 00:00:00+00")
            .expect_err("past the end");
        assert_eq!(error.sqlstate(), sqlstate::DATETIME_FIELD_OVERFLOW);
    }
}

#[cfg(test)]
mod binary_tests {
    use super::PgDatum;
    use super::{ColumnType, Datum, MIN_MICROS, NEG_INFINITY, POS_INFINITY};

    /// The bytes a real PostgreSQL 19 wrote for these values, taken from
    /// `COPY ... TO STDOUT (FORMAT binary)` — the same `typsend` functions the protocol uses.
    #[test]
    fn the_binary_formats_are_the_ones_postgresql_sends() {
        let cases: &[(Datum, &[u8])] = &[
            (Datum::Int8(1), &[0, 0, 0, 0, 0, 0, 0, 1]),
            (Datum::Int8(-1), &[0xff; 8]),
            (Datum::Bool(true), &[1]),
            (Datum::Bool(false), &[0]),
            (Datum::Text("ab".into()), b"ab"),
            (Datum::Bytea(vec![0xde, 0xad]), &[0xde, 0xad]),
            (Datum::TimestampTz(0), &[0; 8]),
            (
                Datum::TimestampTz(762_525_296_100_000),
                &[0x00, 0x02, 0xb5, 0x83, 0x41, 0x68, 0x02, 0xa0],
            ),
            (
                Datum::Double(1.5),
                &[0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            ),
            (
                Datum::TimestampTz(POS_INFINITY),
                &[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                value.to_binary().as_deref(),
                Some(*expected),
                "{value:?} does not match what PostgreSQL sent"
            );
        }
    }

    /// Every value survives the binary round trip, sentinels included — which is the point of
    /// storing PostgreSQL's own representation rather than a translated one.
    #[test]
    fn binary_round_trips_for_every_type() {
        let cases = [
            (ColumnType::Int8, Datum::Int8(i64::MIN)),
            (ColumnType::Text, Datum::Text("héllo 🌊".into())),
            (ColumnType::Bool, Datum::Bool(true)),
            (ColumnType::Bytea, Datum::Bytea(vec![0, 0xff, 0x7f])),
            (ColumnType::TimestampTz, Datum::TimestampTz(MIN_MICROS)),
            (ColumnType::TimestampTz, Datum::TimestampTz(NEG_INFINITY)),
            (ColumnType::Double, Datum::Double(f64::NAN)),
            (ColumnType::Double, Datum::Double(-0.0)),
        ];
        for (ty, value) in cases {
            let bytes = value.to_binary().expect("not NULL");
            assert_eq!(Datum::from_binary(ty, &bytes).unwrap(), value, "{ty:?}");
        }
        assert_eq!(
            Datum::Null.to_binary(),
            None,
            "NULL is a -1 length, not bytes"
        );
    }

    /// A wrong length is a protocol violation, not a partial read: guessing would turn a client's
    /// bug into a wrong number.
    #[test]
    fn a_binary_value_of_the_wrong_length_is_refused() {
        assert!(Datum::from_binary(ColumnType::Int8, &[0; 4]).is_err());
        assert!(Datum::from_binary(ColumnType::Bool, &[2]).is_err());
        assert!(Datum::from_binary(ColumnType::Double, &[]).is_err());
        // A variable-length type takes whatever it is given.
        assert!(Datum::from_binary(ColumnType::Bytea, &[]).is_ok());
    }

    /// **Ranges sort by their bounds, not by their text**, and the fixture is the capture: this is
    /// `ORDER BY float_range` over `range_test.rb`'s own rows on a real server.
    ///
    /// Sorted as text — which is what this used to do — `empty` lands last instead of first,
    /// because `e` is above `[` and `(`. The second pair below is the case text order gets wrong
    /// even without an `empty` in sight: `[10,21)` precedes `[2,4)` as characters and follows it
    /// as a range.
    #[test]
    fn a_range_sorts_by_its_bounds_and_not_by_its_text() {
        use std::cmp::Ordering;

        let float = |text: &str| Datum::Range {
            subtype: Box::new(ColumnType::Double),
            text: text.to_owned(),
        };
        let order = [
            "empty",
            "(,)",
            "[-Infinity,Infinity]",
            "[0.5,0.7)",
            "[0.5,0.7]",
            "[0.5,)",
        ];
        for pair in order.windows(2) {
            assert_eq!(
                float(pair[0]).pg_cmp(&float(pair[1])),
                Ordering::Less,
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
        // The same value twice is equal, which is what keeps `DISTINCT` and `=` agreeing with the
        // canonical text they are still compared by.
        assert_eq!(
            float("[0.5,0.7]").pg_cmp(&float("[0.5,0.7]")),
            Ordering::Equal
        );

        // And the digits, where the text is wrong without any `empty` involved.
        let ints = |text: &str| Datum::Range {
            subtype: Box::new(ColumnType::Int8),
            text: text.to_owned(),
        };
        assert_eq!(ints("[2,4)").pg_cmp(&ints("[10,21)")), Ordering::Less);
        assert_eq!(
            "[10,21)".cmp("[2,4)"),
            Ordering::Less,
            "the text really does disagree, which is why this test exists"
        );
    }
}
