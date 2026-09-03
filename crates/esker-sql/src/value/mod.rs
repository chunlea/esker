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
pub mod date;
/// PostgreSQL's character-set names, for the two errors `convert_to` tells apart.
pub mod encoding;
pub(crate) mod float;
pub mod interval;
pub(crate) mod json;
pub mod numeric;
pub mod oid;
/// Random bytes from the OS, and the version-4 UUID built from them.
pub mod random;
pub mod range;
pub mod regex;
/// Arithmetic over the date and time types: which pairs have an operator, and what it yields.
pub mod temporal;
pub mod time;
mod timestamp;
pub mod uuid;
/// Arrays as the catalog holds them: text, read by the operators (`vector::Array`).
pub mod vector;

use std::cmp::Ordering;

use crate::error::{Result, SqlError};

pub use esker_keys::value::{ColumnType, Datum, f64_of_sort_bits, sort_bits_of_f64};
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

/// A type as `format_type` writes it, with its typmod: what an error message and `\gdesc` say.
#[must_use]
pub fn format_type(ty: ColumnType, typmod: i32) -> String {
    match (ty, typmod) {
        // **`bpchar` is the one type whose bare name is not its parameterised one.** Measured:
        // `format_type(1042, -1)` is `bpchar` and `format_type(1042, 7)` is `character(3)`, where
        // `varchar` is `character varying` either way. It is what `min(c)` reports, since an
        // aggregate carries no typmod.
        (ColumnType::Bpchar, NO_TYPMOD) => "bpchar".to_owned(),
        (_, NO_TYPMOD) => ty.name().to_owned(),
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
        // `numeric(10,2)`, and `numeric(11,-2)` — the scale is signed and prints signed.
        (ColumnType::Numeric, _) => numeric::format_typmod(typmod),
        _ => ty.name().to_owned(),
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
    let lowered = spelled.trim().to_ascii_lowercase();
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
        return Ok(type_by_name(&element)?.map(Named::Array));
    }
    type_by_name(&lowered).map(|found| found.map(Named::Scalar))
}

/// The type a name means, ignoring arrays. See [`named_type`] for the whole answer.
pub fn type_by_name(spelled: &str) -> Result<Option<ColumnType>> {
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
    let Some(ty) = resolve_type_name(&bare) else {
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

/// A type name, which may name an **array** of a type this node has.
///
/// Arrays are not storable here — there is no `ColumnType` for one — but their *names* resolve,
/// because `ActiveRecord` asks `pg_type` for them and statement 766 of `schema.rb` stops on
/// `'decimal[]'::regtype`. Resolving the name is not claiming the type: nothing can create a
/// column of one, and `Named::Array` exists only so that the two things a `regtype` answers —
/// the OID and the printed name — can be produced for it.
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
        // There is no array of an array: an array type is a constructor over a *scalar* here, so
        // asking for one has no answer and `0` is `InvalidOid`, which is what a real server's
        // `typarray` holds for a type that has no array.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::NumericArray
        | ColumnType::TextArray => 0,
        ColumnType::Bool => 1000,
        ColumnType::Bytea => 1001,
        ColumnType::Int8 => 1016,
        ColumnType::Int2 => 1005,
        ColumnType::Int4 => 1007,
        ColumnType::Text => 1009,
        ColumnType::Oid => 1028,
        ColumnType::Json => 199,
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
fn resolve_type_name(name: &str) -> Option<ColumnType> {
    ColumnType::ALL
        .into_iter()
        .find(|ty| ty.name() == name || crate::catalog::pg_catalog::typname(*ty) == name)
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
        | ColumnType::Numeric => true,
        ColumnType::Int8
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
        | ColumnType::Date
        // `numeric(10,2)[]` is a real declaration on a real server and this node does not read
        // one; the array types take no typmod here, and a declaration that carries one is
        // refused where it is parsed rather than silently dropped.
        | ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::NumericArray
        | ColumnType::TextArray => false,
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
    fn oid(self) -> u32 {
        // **An array type's OID is its element's `typarray`**, which this crate already knows —
        // `array_oid` is where `_int4` is 1007 and `_int8` is 1016. Derived rather than repeated,
        // so the two can never disagree.
        if let Some(element) = esker_keys::array::ArrayValue::element_of(self) {
            return array_oid(element);
        }
        match self {
            ColumnType::Bool => 16,
            ColumnType::Bytea => 17,
            ColumnType::Int8 => 20,
            ColumnType::Int2 => 21,
            ColumnType::Int4 => 23,
            ColumnType::Text => 25,
            ColumnType::Varchar => 1043,
            ColumnType::Bpchar => 1042,
            ColumnType::Json => 114,
            ColumnType::Jsonb => 3802,
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
            ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::NumericArray
            | ColumnType::TextArray => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            // What an error message calls it: the element's name with `[]`, which is how
            // PostgreSQL words `cannot cast type integer[] to uuid` — not the internal `_int4`
            // that `pg_type.typname` holds.
            ColumnType::Int8Array => "bigint[]",
            ColumnType::Int4Array => "integer[]",
            ColumnType::NumericArray => "numeric[]",
            ColumnType::TextArray => "text[]",
            ColumnType::Int8 => "bigint",
            ColumnType::Int4 => "integer",
            ColumnType::Int2 => "smallint",
            ColumnType::Text => "text",
            ColumnType::Varchar => "character varying",
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
        }
    }

    fn type_len(self) -> i16 {
        match self {
            ColumnType::Bool => 1,
            // Four bytes, unsigned, which is the whole of what makes it not an `int4`.
            ColumnType::Int4 | ColumnType::Real | ColumnType::Date | ColumnType::Oid => 4,
            ColumnType::Int2 => 2,
            // Sixteen fixed bytes, which is what `pg_type.typlen` says.
            // Sixteen fixed bytes each: a uuid is one value, an interval is three fields.
            ColumnType::Uuid | ColumnType::Interval => 16,
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Time
            | ColumnType::Double => 8,
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Numeric
            | ColumnType::Bytea
            // However many elements it has, which is the definition of a varlena.
            | ColumnType::Int8Array
            | ColumnType::Int4Array
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
            Datum::Array(value) => array::to_text(value),
            Datum::Int8(v) => v.to_string(),
            Datum::Int4(v) => v.to_string(),
            Datum::Int2(v) => v.to_string(),
            Datum::Text(v) => v.clone(),
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

    fn from_text(ty: ColumnType, text: &str) -> Result<Datum> {
        Ok(match ty {
            // The literal's *shape* is read here and each element by its own type's input
            // function, which is what makes `'{1,x}'::int[]` `int4`'s error and `'{a,,b}'` the
            // array's (`crate::value::array`).
            ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::NumericArray
            | ColumnType::TextArray => {
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
            // `json` keeps the text exactly as sent, once it is known to be a document; `jsonb`
            // keeps the canonical form it prints as. ADR 0042 is why the two differ here and
            // nowhere else in this function.
            ColumnType::Json => {
                json::validate(text)?;
                Datum::Text(text.to_owned())
            }
            ColumnType::Jsonb => Datum::Text(json::canonicalise(text)?),
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
            Datum::Null | Datum::Numeric(_) | Datum::Array(_) => return None,
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
            Datum::Text(v) => v.as_bytes().to_vec(),
            Datum::Bytea(v) => v.clone(),
        })
    }

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
            // The mirror of `to_binary`: `array_recv`'s shape has never been read here, so a
            // client that sends one is told so rather than given a value built from a guess.
            ColumnType::Int8Array
            | ColumnType::Int4Array
            | ColumnType::NumericArray
            | ColumnType::TextArray => {
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
            ColumnType::Json | ColumnType::Jsonb => {
                return Err(SqlError::unsupported(
                    "a json or jsonb parameter in the binary format",
                ));
            }
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                Datum::Text(String::from_utf8(bytes.to_vec()).map_err(|error| {
                    let at = error.utf8_error().valid_up_to();
                    SqlError::InvalidByteSequence(error.as_bytes().get(at).copied().unwrap_or(0))
                })?)
            }
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
            (Datum::Int8(a), Datum::Int8(b))
            | (Datum::TimestampTz(a), Datum::TimestampTz(b))
            // Plain integer order, and only against another `time`: this type compares with
            // nothing else, so there is no promotion arm to write beside it.
            | (Datum::Time(a), Datum::Time(b))
            | (Datum::Timestamp(a), Datum::Timestamp(b)) => a.cmp(b),
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
            (Datum::Oid(a), Datum::Oid(b)) => a.cmp(b),
            (Datum::Oid(a), Datum::Int8(b)) => i64::from(*a).cmp(b),
            (Datum::Int8(a), Datum::Oid(b)) => a.cmp(&i64::from(*b)),
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
            (Datum::Text(a), Datum::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
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
        // Above every scalar, which only decides the order between two values of *different*
        // types — a comparison SQL does not have and this crate's total order still needs.
        Datum::Array(_) => 20,
        Datum::Bool(_) => 0,
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
        Datum::Text(_) => 4,
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
        for ty in ColumnType::ALL {
            assert_eq!(
                ty.type_len() == -1,
                matches!(
                    ty,
                    ColumnType::Text
                        | ColumnType::Varchar
                        | ColumnType::Bpchar
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        | ColumnType::Bytea
                        // Variable width for the same reason as a string: the digits a value
                        // carries are the value, and `numeric(10,2)` bounds them in the typmod,
                        // not in the type.
                        | ColumnType::Numeric
                        // However many elements it has, which is the definition of a varlena.
                        | ColumnType::Int8Array
                        | ColumnType::Int4Array
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
}
