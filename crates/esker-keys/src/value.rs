//! The six shapes a stored tuple can hold, and nothing about what they mean in SQL.
//!
//! This is the **storage layer's shared type vocabulary**. It lives here, beside the codecs that
//! write it, because [`crate::row`] and [`crate::codec`] both need to name a value and neither
//! may depend on `esker-sql`, which sits above the store.
//!
//! # What is deliberately not here
//!
//! Every PostgreSQL-ism stays in `esker-sql`, behind an extension trait: type OIDs, the text and
//! binary I/O formats, and [`Datum`]'s PostgreSQL *ordering*. Those are contract C3's surface and
//! they are measured against a running server; a crate that owns byte layout has no business
//! carrying them. [ADR 0029](../../../docs/adr/0030-the-row-codec-moves-down.md).
//!
//! # One comparison stays and one leaves, and they disagree
//!
//! [`Datum`]'s [`PartialEq`] is here and is **bitwise for doubles**, so `-0.0` is not `0.0` and one
//! `NaN` payload is not another. `pg_cmp` is in `esker-sql` and says the opposite: `NaN` equals
//! itself and sorts above every other float, which is what PostgreSQL does and what a `WHERE x > 5`
//! has to agree with.
//!
//! That is not a contradiction, it is the seam. Equality here answers *did these bytes survive the
//! round trip*, which is a storage question and must be exact. `pg_cmp` answers *how does a user's
//! query order these*, which is a SQL question and must match a real server. Putting them in one
//! crate is what would let somebody use the wrong one.

/// The 64 bits whose big-endian order is PostgreSQL's order over floats, for an index key.
///
/// Lives beside the type rather than in [`crate::row`] because which floats are *the same value*
/// is a fact about the type, not about the key encoding that has to respect it. The ordering it
/// produces is PostgreSQL's — `NaN` above `Infinity` — which is why `esker-sql`'s `pg_cmp` and
/// this function agree even though [`Datum`]'s `PartialEq` does not.
#[must_use]
pub fn sort_bits_of_f64(value: f64) -> u64 {
    let canonical = if value.is_nan() {
        f64::NAN
    } else if value == 0.0 {
        0.0
    } else {
        value
    };
    let bits = canonical.to_bits();
    if bits & SIGN == 0 { bits | SIGN } else { !bits }
}

/// The same for an `f32`, and 32 bits rather than 64.
///
/// **Not** `sort_bits_of_f64(value.into())`: widening an `f32` is exact, so that would sort
/// correctly — and it would write eight bytes where the type is four, which is the same lie about
/// a width that [`ColumnType::Real`] exists to avoid.
#[must_use]
pub fn sort_bits_of_f32(value: f32) -> u32 {
    const SIGN32: u32 = 1 << 31;
    let canonical = if value.is_nan() {
        f32::NAN
    } else if value == 0.0 {
        0.0
    } else {
        value
    };
    let bits = canonical.to_bits();
    if bits & SIGN32 == 0 {
        bits | SIGN32
    } else {
        !bits
    }
}

/// The inverse of [`sort_bits_of_f32`], up to the canonicalisation it performs.
#[must_use]
pub fn f32_of_sort_bits(bits: u32) -> f32 {
    const SIGN32: u32 = 1 << 31;
    f32::from_bits(if bits & SIGN32 == 0 {
        !bits
    } else {
        bits & !SIGN32
    })
}

/// The inverse of [`sort_bits_of_f64`], up to the canonicalisation it performs.
#[must_use]
pub fn f64_of_sort_bits(bits: u64) -> f64 {
    f64::from_bits(if bits & SIGN == 0 {
        !bits
    } else {
        bits & !SIGN
    })
}

/// One of the six types phase 6a executes (`docs/plans/phase-6a.md` §3).
///
/// The type of a stored value. What each one means to a client — its OID, its text format —
/// is `esker-sql`'s, on an extension trait over this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnType {
    /// 64-bit integer. PostgreSQL calls it `bigint` in messages and `int8` in DDL.
    Int8,
    /// 16-bit integer. PostgreSQL calls it `smallint` in messages and `int2` in DDL. A distinct
    /// type for the same reason `int4` is: two bytes, and `22003` past its own range.
    Int2,
    /// 32-bit integer. PostgreSQL calls it `integer` in messages and `int4` in DDL.
    ///
    /// A **distinct type and not an alias for [`ColumnType::Int8`]** ([ADR
    /// 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)): a client asking what a column is
    /// gets `int4`'s OID, and a value between 2^31 and 2^63 is `22003` here as it is on a real
    /// server rather than being quietly accepted.
    Int4,
    /// Variable-length UTF-8 string.
    Text,
    /// PostgreSQL's `character varying`. **The same representation as [`ColumnType::Text`] and a
    /// different type**, which is PostgreSQL's own model: `text`, `varchar` and `bpchar` are one
    /// varlena told apart by OID, not by bytes. So a `varchar` column's rows are byte-identical to
    /// a `text` column's and this is not a format change even in principle
    /// ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Varchar,
    /// PostgreSQL's `character(n)`, whose internal name is `bpchar` — "blank-padded char".
    ///
    /// The same representation as [`ColumnType::Text`] again, and a third type telling itself
    /// apart by OID. What makes it different is not the bytes but **what is in them**: a value is
    /// padded to the column's length on the way in, so two equal values are equal byte strings and
    /// a plain byte comparison *is* PostgreSQL's blank-insensitive one. That is what lets an index
    /// key hold a `character(n)` without breaking "equal values encode identically", and it is why
    /// this type could not arrive before the typmod did — there is nowhere to pad to without an
    /// `n` ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Bpchar,
    /// PostgreSQL's `json`: a **validated string**, stored exactly as it was sent. Whitespace,
    /// key order and duplicate keys all survive, because that is all `json` is
    /// ([ADR 0042](../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)).
    Json,
    /// PostgreSQL's `jsonb`: a value, stored as the **canonical text** it prints as — keys
    /// reordered by length then bytes, duplicates dropped with the last winning, and a space after
    /// every colon and comma.
    ///
    /// Its equality is **not** its byte equality: `1.0` and `1.00` print differently and compare
    /// equal, because a jsonb number is a `numeric`. That is why a `jsonb` column cannot be a key
    /// here, and it is the one thing ADR 0042 turns on.
    Jsonb,
    /// The `hstore` extension's type: a map of text to nullable text, stored as the **canonical
    /// text** it prints as — every key and value quoted, pairs joined with `, `, a NULL value the
    /// bare word `NULL`, and the pairs ordered by the key's **length first and its bytes second**.
    ///
    /// The same road `jsonb` takes, and for the same reason: the canonical form is a function of
    /// the content, so two hstores are equal exactly when their texts are, and equality, ordering,
    /// grouping and an index over the column are the text machinery's. `crate::value`'s codecs
    /// treat it as a `Text` throughout; `esker_sql::value::hstore` is what canonicalises one.
    ///
    /// **Unlike `jsonb` it is a key**: its equality *is* its byte equality, because there is no
    /// number inside it to print two ways.
    ///
    /// Its oid is a real oid here. On a real server the extension is allocated one at install
    /// time and it differs per database, which is why `ActiveRecord` looks it up by `typname` and
    /// why nothing may hard-code it — a fixed one on this side is invisible to a client that
    /// reads it the way the adapter does.
    Hstore,
    /// PostgreSQL's `tsrange`: a range of `timestamp without time zone`.
    ///
    /// Stored as the canonical text `crate::value::range` renders, the road `hstore` takes and for
    /// the same reason — the canonical form is a function of the content, so equality, ordering
    /// and grouping are the text's. **Not canonicalised** the way `int4range` is: a timestamp has
    /// no successor, so `'[a,b]'` keeps its brackets.
    TsRange,
    /// PostgreSQL's `tstzrange`: the same, over `timestamp with time zone`.
    TstzRange,
    /// PostgreSQL's `int4range`. **Canonicalised**, because an integer has a successor:
    /// `'[1,10]'` is stored and printed `[1,11)`.
    Int4Range,
    /// `tsrange[]`, which `range_test.rb` declares as `t.tsrange :ts_ranges, array: true`.
    TsRangeArray,
    /// The `citext` extension's type: text whose **comparison folds case**.
    ///
    /// Stored exactly as it was written — `'Cased Text'` comes back `Cased Text` — and compared
    /// through [`Datum::Citext`], which folds. That split is the whole type: the value is the
    /// user's spelling and the *comparison* is the folded one, which is why it needs a `Datum` of
    /// its own where `hstore` needed none. ADR 0042 named this shape in advance — a type may share
    /// another's representation only if it shares its comparison, and citext shares neither
    /// equality nor order with `text`.
    Citext,
    /// `hstore[]`, which `hstore_test.rb` declares as `t.hstore "payload", array: true`.
    HstoreArray,
    /// Two-valued, with no third state but NULL.
    Bool,
    /// Variable-length byte string.
    Bytea,
    /// An instant, stored as microseconds from 2000-01-01 UTC.
    TimestampTz,
    /// PostgreSQL's `timestamp` **without** time zone: the same eight bytes as
    /// [`ColumnType::TimestampTz`] and a different type. It does no zone conversion, so what goes
    /// in is what comes out, and it prints with no offset
    /// ([ADR 0033](../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    Timestamp,
    /// IEEE-754 binary64.
    Double,
    /// IEEE-754 binary32; PostgreSQL's `real`. A distinct type because its **text** differs — the
    /// shortest digits that round-trip at 32 bits — and because a value a `double` holds is
    /// `22003` here at both ends of the range.
    Real,
    /// PostgreSQL's `numeric` / `decimal`: an **arbitrary-precision decimal whose scale is part
    /// of the value**.
    ///
    /// `1.0`, `1.00` and `1.000` are three values of this type, all equal and all printed as
    /// written — which is the property ADR 0031 has been refusing `avg(int8)` over since unit 0,
    /// and the reason this type is tier 2's hard half. Variable-length, like a `text`.
    Numeric,
    /// PostgreSQL's `date`: a **day**, stored as a signed count of days from 2000-01-01 — the same
    /// four bytes and the same epoch a real server uses.
    ///
    /// Tier 2's first type, and a distinct one rather than a [`ColumnType::Timestamp`] rounded
    /// down: a `date` has no time in it at all, prints without one, and its arithmetic answers
    /// different types from a timestamp's (`date - date` is an `integer`).
    Date,
    /// PostgreSQL's `time` **without** time zone: a time of day, stored as microseconds since
    /// midnight — eight bytes, PostgreSQL's own representation.
    ///
    /// **Its range is closed at both ends**: `00:00:00` through `24:00:00` inclusive, so a value
    /// naming a twenty-fifth hour is storable and `86_400_000_000` is a legal number here. That is
    /// PostgreSQL's rule, not a rounding artefact, and it is why nothing may assume a time is
    /// strictly less than a day.
    Time,
    /// PostgreSQL's `uuid`: **sixteen fixed bytes**, and nothing about them is a number.
    ///
    /// Its order is its bytes' order, which is what lets an index key hold one unchanged. Its
    /// *text* is far more permissive on the way in than on the way out — five spellings read as
    /// one value — but that is `esker-sql`'s business; here it is sixteen bytes.
    Uuid,
    /// PostgreSQL's `interval`: **three independent fields** — months, days and microseconds —
    /// in sixteen bytes.
    ///
    /// Almost everything surprising about the type follows from their independence. Storage keeps
    /// what was written (`1 mon 1 day` stays that, `400 days` never becomes a year), while
    /// *comparison* converts a month to 30 days and a day to 24 hours — so two intervals can be
    /// equal and print differently. Signs are per field: `1 day -12:00:00` is a real value.
    Interval,
    /// `bigint[]`. **An array is a constructor over one element type**, not a scalar of its own,
    /// and the four here are the ones `ActiveRecord`'s schemas declare. A recursive
    /// `Array(Box<ColumnType>)` would say that better and would cost a `Box` at every one of the
    /// hundreds of places this `Copy` type is passed by value
    /// ([ADR 0047](../../docs/adr/0047-an-array-is-a-column-type-over-one-element-type.md)).
    Int8Array,
    /// `integer[]`.
    Int4Array,
    /// `smallint[]`. **The catalog's own array type**: `pg_constraint.conkey` and `confkey` are
    /// `smallint[]` on a real server, and an element of one is compared with `pg_attribute.attnum`
    /// in every schema dump `ActiveRecord` writes. It is not `pg_index.indkey`, which looks the
    /// same and is an `int2vector` — a different type, printed `1 2` and subscripted from zero.
    Int2Array,
    /// `numeric[]`.
    NumericArray,
    /// `text[]`.
    TextArray,
    /// PostgreSQL's `oid`: a **four-byte unsigned** integer that prints as a plain number.
    ///
    /// Unsigned is the whole of what makes it not an `int4`: `(-1)::oid` is `4294967295` — it
    /// wraps rather than refusing — and `4294967296` is `22003`. It is the type every catalog
    /// identifier really has.
    Oid,
    /// `boolean[]`. **The sixteen below are not sixteen features.** Every base type on a real
    /// server has an array type, and `pg_type.typarray` points at it; a `typarray` naming a row
    /// that is not there is worse than a zero, because a client walks the link in both directions
    /// and finds half of it. `ActiveRecord` registers an array decoder **by the element type's
    /// `typarray`**, so a dangling one leaves the column looking scalar and quoting a Ruby array
    /// for it raises `TypeError: can't quote Array` client-side, before a statement is sent.
    BoolArray,
    /// `bytea[]`.
    ByteaArray,
    /// `character[]`.
    BpcharArray,
    /// `character varying[]`.
    VarcharArray,
    /// `date[]`.
    DateArray,
    /// `time without time zone[]`.
    TimeArray,
    /// `timestamp without time zone[]`.
    TimestampArray,
    /// `timestamp with time zone[]`.
    TimestampTzArray,
    /// `interval[]`.
    IntervalArray,
    /// `real[]`.
    RealArray,
    /// `double precision[]`.
    DoubleArray,
    /// `uuid[]`.
    UuidArray,
    /// `json[]`.
    JsonArray,
    /// `jsonb[]`.
    JsonbArray,
    /// `oid[]`.
    OidArray,
    /// `citext[]`.
    CitextArray,
}

impl ColumnType {
    /// Every type, for tests that must not silently skip one.
    pub const ALL: [ColumnType; 48] = [
        ColumnType::Int8,
        ColumnType::Int4,
        ColumnType::Int2,
        ColumnType::Text,
        ColumnType::Varchar,
        ColumnType::Bpchar,
        ColumnType::Json,
        ColumnType::Jsonb,
        ColumnType::Hstore,
        ColumnType::Citext,
        ColumnType::TsRange,
        ColumnType::TstzRange,
        ColumnType::Int4Range,
        ColumnType::TsRangeArray,
        ColumnType::HstoreArray,
        ColumnType::Bool,
        ColumnType::Bytea,
        ColumnType::TimestampTz,
        ColumnType::Timestamp,
        ColumnType::Double,
        ColumnType::Real,
        ColumnType::Date,
        ColumnType::Numeric,
        ColumnType::Time,
        ColumnType::Uuid,
        ColumnType::Interval,
        ColumnType::Oid,
        ColumnType::Int8Array,
        ColumnType::Int4Array,
        ColumnType::Int2Array,
        ColumnType::NumericArray,
        ColumnType::TextArray,
        ColumnType::BoolArray,
        ColumnType::ByteaArray,
        ColumnType::BpcharArray,
        ColumnType::VarcharArray,
        ColumnType::DateArray,
        ColumnType::TimeArray,
        ColumnType::TimestampArray,
        ColumnType::TimestampTzArray,
        ColumnType::IntervalArray,
        ColumnType::RealArray,
        ColumnType::DoubleArray,
        ColumnType::UuidArray,
        ColumnType::JsonArray,
        ColumnType::JsonbArray,
        ColumnType::OidArray,
        ColumnType::CitextArray,
    ];
}

/// One column's value, or its absence.
///
/// `PartialEq` compares floats by their **bits**, not by IEEE equality, so `NaN` equals itself and
/// `-0.0` does not equal `0.0`. That is the right question for an encoding module — "did this
/// value survive the round trip" — and the wrong one for SQL, where PostgreSQL says both the
/// opposite things. SQL's comparison is `esker_sql::value::PgDatum::pg_cmp`, in the crate that
/// owns what a value *means*, precisely so neither can be mistaken for the other.
#[derive(Debug, Clone)]
pub enum Datum {
    /// SQL NULL, of whatever the column's type is.
    Null,
    /// [`ColumnType::Int8`].
    Int8(i64),
    /// [`ColumnType::Int4`]. Four bytes on disk, and four bytes is the point: the width is what
    /// makes it a different type from an `int8` that happens to hold a small number.
    Int4(i32),
    /// [`ColumnType::Int2`]. Two bytes, for the same reason.
    Int2(i16),
    /// [`ColumnType::Text`]. Always valid UTF-8: the server encoding is UTF8, and bytes that are
    /// not are refused on the way in the way PostgreSQL refuses them.
    Text(String),
    /// [`ColumnType::Hstore`]: the map, as its canonical text.
    ///
    /// A variant of its own for the reason [`Datum::Citext`] is one, arrived at the same way: a
    /// cast of a constant folds at plan time, and a folded `'a=>b'::hstore` that came out as a
    /// `Text` had lost the only thing that said it was an hstore — so `pg_typeof` over it answered
    /// `text` and `||` could not tell which concatenation it was. Its **comparison is `text`'s**,
    /// unlike citext's, because the canonical form is a function of the content.
    Hstore(String),
    /// [`ColumnType::TsRange`] and its siblings: the range, as its canonical text.
    ///
    /// The subtype rides along because a folded constant would otherwise lose it — the lesson
    /// [`Datum::Hstore`] records: `'…'::tsrange` folds at plan time, and a `Text` coming out of
    /// that fold has nothing left saying which range it is, so `pg_typeof` and the operators
    /// cannot tell a `tsrange` from a `tstzrange`.
    ///
    /// Its comparison is the **text's**, which is right because the text is canonical: two ranges
    /// print the same exactly when they are the same range. The subtype is not compared — a
    /// `tsrange` and an `int4range` are different types and no operator puts them together.
    Range {
        /// What the bounds are: `ColumnType::Timestamp` for a `tsrange`, and so on.
        subtype: Box<ColumnType>,
        /// The canonical text, which is what `crate::value::range` renders.
        text: String,
    },
    /// [`ColumnType::Citext`]: the text **as it was written**, compared **folded**.
    ///
    /// A variant of its own rather than a `Text` under a different column type, because the
    /// difference is in the *comparison* and a comparison sees only the values — which is exactly
    /// what ADR 0042 said would eventually be needed and why it left `jsonb` refused instead of
    /// guessing. `'Cased Text'` is stored and returned with its capitals; `=`, `pg_cmp`, `DISTINCT`
    /// and a unique index all fold it.
    ///
    /// **`PartialEq` stays bitwise** — it is what the round-trip tests assert and it is not SQL
    /// equality for any type here (`esker_sql::value::PgDatum::pg_cmp` disagrees with it for
    /// floats too, and is named in text rather than linked for the reason the module doc gives:
    /// it is in the crate above this one).
    Citext(String),
    /// [`ColumnType::Bool`].
    Bool(bool),
    /// [`ColumnType::Bytea`].
    Bytea(Vec<u8>),
    /// [`ColumnType::TimestampTz`], in microseconds from 2000-01-01 00:00:00 UTC — PostgreSQL's
    /// own epoch and its own representation, including the infinities `esker-sql` names
    /// `POS_INFINITY` and `NEG_INFINITY` — to this crate they are ordinary `i64` sentinels.
    TimestampTz(i64),
    /// [`ColumnType::Double`].
    Double(f64),
    /// [`ColumnType::Real`].
    Real(f32),
    /// [`ColumnType::Timestamp`], in microseconds from 2000-01-01 — the same representation as
    /// [`Datum::TimestampTz`], and a separate variant because the two print differently and a
    /// value has to know which it is.
    Timestamp(i64),
    /// [`ColumnType::Date`], in days from 2000-01-01. PostgreSQL's own representation, including
    /// the infinities, which to this crate are ordinary `i32` sentinels.
    Date(i32),
    /// [`ColumnType::Numeric`]. Its scale is part of it (`crate::numeric`).
    Numeric(crate::numeric::Numeric),
    /// [`ColumnType::Time`], in microseconds since midnight. `86_400_000_000` — `24:00:00` — is a
    /// value and not an overflow.
    Time(i64),
    /// [`ColumnType::Uuid`], as the sixteen bytes it is.
    Uuid([u8; 16]),
    /// [`ColumnType::Oid`], as the unsigned it is.
    Oid(u32),
    /// One of the four array types: its elements, their shape, and where they are subscripted
    /// from (`crate::array`).
    ///
    /// **An element that is NULL is not a NULL array.** `'{NULL}'::int[] IS NULL` is false, and
    /// the two states are told apart here by `Datum::Null` against a `None` inside the value.
    Array(crate::array::ArrayValue),
    /// [`ColumnType::Interval`]: months, days and microseconds, each with its own sign.
    Interval {
        /// Whole months. Years are twelve of these; nothing else carries into them.
        months: i32,
        /// Whole days, which never carry into months.
        days: i32,
        /// The time of day part, which never carries into days.
        micros: i64,
    },
}

impl PartialEq for Datum {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Datum::Null, Datum::Null) => true,
            (Datum::Int8(a), Datum::Int8(b)) | (Datum::TimestampTz(a), Datum::TimestampTz(b)) => {
                a == b
            }
            (Datum::Int4(a), Datum::Int4(b)) | (Datum::Date(a), Datum::Date(b)) => a == b,
            // Representation equality, not value equality: `1.0` and `1.00` are different values
            // of this type and this asks whether a round trip preserved one. `pg_cmp` is where
            // the number is compared.
            (Datum::Numeric(a), Datum::Numeric(b)) => a == b,
            (Datum::Int2(a), Datum::Int2(b)) => a == b,
            (Datum::Timestamp(a), Datum::Timestamp(b)) | (Datum::Time(a), Datum::Time(b)) => a == b,
            (Datum::Uuid(a), Datum::Uuid(b)) => a == b,
            (Datum::Oid(a), Datum::Oid(b)) => a == b,
            // Representation equality, element by element: two arrays that print the same are
            // the same row. What `1.0` and `1.00` are to a `numeric`, `{1.0}` and `{1.00}` are
            // to a `numeric[]`, and `pg_cmp` is again where the *values* are compared.
            (Datum::Array(a), Datum::Array(b)) => a == b,
            // Representation equality, not value equality: `1 mon` and `30 days` are equal
            // *values* and different rows. `pg_cmp` is where the number of them is compared.
            (
                Datum::Interval {
                    months: left_months,
                    days: left_days,
                    micros: left_micros,
                },
                Datum::Interval {
                    months: right_months,
                    days: right_days,
                    micros: right_micros,
                },
            ) => {
                left_months == right_months
                    && left_days == right_days
                    && left_micros == right_micros
            }
            // **Representation equality, and for citext that is the *unfolded* spelling** — this
            // asks whether a round trip preserved the value, and `'Cased Text'` and `'CASED TEXT'`
            // are different rows however they compare. `pg_cmp` is where the folding is, exactly
            // as it is where `1.0` and `1.00` are compared as numbers.
            //
            // **A missing arm here is not a compile error**, because this impl is written pair by
            // pair and falls through to `false`: citext arrived without one and was never equal to
            // itself, which `any_row_survives_encode_and_decode` caught and nothing else would
            // have. Adding a `Datum` variant means adding a line here.
            (Datum::Text(a), Datum::Text(b))
            | (Datum::Citext(a), Datum::Citext(b))
            | (Datum::Hstore(a), Datum::Hstore(b)) => a == b,
            // The canonical text and the subtype together: two ranges are one row when they print
            // the same *and* are the same type.
            (
                Datum::Range {
                    subtype: a,
                    text: at,
                },
                Datum::Range {
                    subtype: b,
                    text: bt,
                },
            ) => a == b && at == bt,
            (Datum::Bool(a), Datum::Bool(b)) => a == b,
            (Datum::Bytea(a), Datum::Bytea(b)) => a == b,
            // Bitwise, so a round-trip test cannot pass by turning -0.0 into 0.0 or one NaN
            // payload into another.
            (Datum::Double(a), Datum::Double(b)) => a.to_bits() == b.to_bits(),
            (Datum::Real(a), Datum::Real(b)) => a.to_bits() == b.to_bits(),
            _ => false,
        }
    }
}

impl Eq for Datum {}

impl Datum {
    /// The type this value belongs to, or `None` for NULL, which belongs to all of them.
    #[must_use]
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Datum::Null => return None,
            Datum::Int8(_) => ColumnType::Int8,
            Datum::Citext(_) => ColumnType::Citext,
            Datum::Hstore(_) => ColumnType::Hstore,
            Datum::Range { subtype, .. } => match **subtype {
                ColumnType::TimestampTz => ColumnType::TstzRange,
                ColumnType::Int4 | ColumnType::Int8 => ColumnType::Int4Range,
                _ => ColumnType::TsRange,
            },
            Datum::Int4(_) => ColumnType::Int4,
            Datum::Date(_) => ColumnType::Date,
            Datum::Time(_) => ColumnType::Time,
            Datum::Uuid(_) => ColumnType::Uuid,
            Datum::Interval { .. } => ColumnType::Interval,
            Datum::Oid(_) => ColumnType::Oid,
            Datum::Numeric(_) => ColumnType::Numeric,
            // The array's own element type decides which of the four it is, so a value always
            // knows what it is without being told.
            Datum::Array(value) => {
                return crate::array::ArrayValue::array_of(value.element);
            }
            Datum::Int2(_) => ColumnType::Int2,
            Datum::Real(_) => ColumnType::Real,
            Datum::Timestamp(_) => ColumnType::Timestamp,
            Datum::Text(_) => ColumnType::Text,
            Datum::Bool(_) => ColumnType::Bool,
            Datum::Bytea(_) => ColumnType::Bytea,
            Datum::TimestampTz(_) => ColumnType::TimestampTz,
            Datum::Double(_) => ColumnType::Double,
        })
    }

    /// Whether this value fits a column of `ty`. NULL fits every type.
    #[must_use]
    pub fn fits(&self, ty: ColumnType) -> bool {
        // NULL fits every column.
        let Some(actual) = self.column_type() else {
            return true;
        };
        if actual == ty || one_representation(actual, ty) {
            return true;
        }
        // **The same question one level up.** An array fits an array column exactly when its
        // element fits that column's element: `ARRAY['one','two']` is a `text[]` and a
        // `character varying(255)[]` column takes it, for the reason a bare `'one'` goes into a
        // `varchar` one. It is not a wider rule than the scalar one — `int4[]` still does not fit
        // an `int8[]` column, because an `int4` does not fit an `int8` one.
        match (
            crate::array::ArrayValue::element_of(actual),
            crate::array::ArrayValue::element_of(ty),
        ) {
            (Some(held), Some(wanted)) => held == wanted || one_representation(held, wanted),
            _ => false,
        }
    }
}

/// Whether a value of `held` is already a value of `wanted`, with no conversion at all.
///
/// `text` fits `varchar` and `bpchar` as well as `text`, because they are **one representation and
/// three types**. [`Datum`] has no `Varchar` or `Bpchar` variant, since there would be nothing in
/// one that a `Text` does not already hold — what differs is the column's declared type, which
/// comes from the schema and not from the value. A `bpchar`'s padding is part of its *value*: it
/// is applied before the datum is built, not carried beside it.
fn one_representation(held: ColumnType, wanted: ColumnType) -> bool {
    matches!(
        (held, wanted),
        (
            ColumnType::Text,
            ColumnType::Varchar | ColumnType::Bpchar | ColumnType::Json | ColumnType::Jsonb
        )
    )
}

/// The sign bit of an IEEE-754 binary64.
const SIGN: u64 = 1 << 63;
