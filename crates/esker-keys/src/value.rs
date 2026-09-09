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
    /// PostgreSQL's `name`: the type its own catalog is written in, and the one string type that
    /// is **fixed width**.
    ///
    /// `typlen` is 64 and positive where every other string type's is -1, and the value is
    /// truncated to **63** bytes on the way in — the 64th is C's terminator. The truncation is by
    /// bytes and stops on a character boundary, so `repeat('é',64)::name` is 31 characters and 62
    /// octets rather than half of a 32nd (measured, `tests/captures/pg19_name_type.txt`).
    ///
    /// Stored as its text, like [`ColumnType::Varchar`], and telling itself apart by OID — but
    /// unlike `varchar` its **collation is C**, so a column of it sorts in byte order and every
    /// capital precedes every lower-case letter. A memcomparable key is already in byte order
    /// ([ADR 0076](../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)), so that
    /// ordering is the one this crate gives it for free.
    Name,
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
    /// PostgreSQL's `tsvector`: a sorted, deduplicated set of lexemes, each optionally carrying a
    /// list of positions, stored as **the canonical text it prints as**
    /// ([ADR 0066](../../docs/adr/0066-a-tsvector-is-its-canonical-text.md)).
    ///
    /// The road `hstore` takes and for the same reason: the canonical form is a function of the
    /// content, so two tsvectors are equal exactly when their texts are, and equality, ordering,
    /// grouping and an index over the column are the text machinery's. **It is a key**, unlike
    /// `jsonb`, because there is no number inside it to print two ways.
    ///
    /// The canonicalisation is not free and is the whole of the risk: `'a fat cat'::tsvector` is
    /// `'a' 'cat' 'fat'` — sorted and quoted — so a node that stored the user's characters
    /// unchanged would round-trip `full_text_test.rb` and disagree with a real server the first
    /// time a value arrived unsorted. `esker_sql::value::tsvector` is what canonicalises one.
    TsVector,
    /// PostgreSQL's `tsquery`: lexemes joined by `&`, `|`, `!` and `<->`, stored as its canonical
    /// text.
    ///
    /// **A different grammar from [`ColumnType::TsVector`], not a different spelling of it**:
    /// `'a b'` is a two-lexeme tsvector and a *syntax error* as a tsquery, which is measured in
    /// `crates/esker-sql/tests/captures/pg19_tsvector.txt`.
    TsQuery,
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
    /// `daterange`. **Discrete, so it canonicalises**: `[2012-01-02,2012-01-04]` comes back
    /// `[2012-01-02,2012-01-05)`, measured. A date has a successor and a timestamp does not,
    /// which is the whole of why one of these normalises and the next does not.
    DateRange,
    /// `numrange`. **Continuous, so it does not canonicalise**: `[0.1,0.2]` comes back exactly
    /// `[0.1,0.2]`. `rngcanonical` is `-` for it on a real server, beside `tsrange`'s.
    NumRange,
    /// `int8range`. Discrete like `int4range`: `[10,100]` comes back `[10,101)`.
    Int8Range,
    /// A range over `float8`, which is **no PostgreSQL type at all** — it is the representation a
    /// user-defined `CREATE TYPE … AS RANGE (subtype = float8)` gets.
    ///
    /// `range_test.rb`'s own `floatrange` is the reason it exists, and the name here is the
    /// *subtype*, not that type: two different user types over `float8` share this representation
    /// and stay two types, exactly as two enums share `int2` and stay two (ADR 0050). Which one a
    /// column is comes from `esker_sql::catalog::ColumnDef::user_type`, and every name a client
    /// sees comes from the catalog with it.
    ///
    /// **Continuous, so it does not canonicalise**: `[0.5,0.7]` comes back `[0.5,0.7]`, measured
    /// beside `numrange`'s.
    FloatRange,
    /// A range over `varchar`, the other half of the same statement —
    /// `CREATE TYPE stringrange AS RANGE (subtype = varchar)`, which the same `setup` runs.
    ///
    /// **The bounds are quoted text**, which no other range here has needed: `'["ca""t","do\\g")'`
    /// round-trips byte for byte on a real server and the suite asserts exactly that value.
    VarcharRange,
    /// `tstzrange[]`. **`range_test.rb` declares two range arrays, not one** — `ts_ranges` and
    /// `tstz_ranges` — and an array type is built per element type, so three of the four left the
    /// file's 46 tests exactly where they were.
    TstzRangeArray,
    /// `int4range[]`.
    Int4RangeArray,
    /// `daterange[]`.
    DateRangeArray,
    /// `numrange[]`.
    NumRangeArray,
    /// `int8range[]`.
    Int8RangeArray,
    /// `point`: **two `float8`s and no comparison at all.**
    ///
    /// `point = point` and `point < point` are each `42883 operator does not exist` on a real
    /// server — only `~=` (same-as) and `<->` (distance) exist — so a point is **not an index
    /// key**, cannot be `DISTINCT`ed and cannot be grouped. That is ADR 0042's rule at its
    /// sharpest: `json` at least has no equality with *another* type, and a point has none with
    /// itself. `CREATE INDEX` on one is `42704 data type point has no default operator class for
    /// access method "btree"`.
    ///
    /// `typlen` is **16** rather than -1: two eight-byte coordinates and no length header.
    Point,
    /// `point[]`. `geometric_test.rb` declares one (`t.point :array_of_points, array: true`).
    PointArray,
    /// `box[]`, and **the one array type in all of `pg_type` whose delimiter is not a comma**.
    ///
    /// A `box` is written `(x1,y1),(x2,y2)` — commas inside the value — so an array of them
    /// separates its elements with `;` instead: `{(1,1),(0,0);(3,3),(2,2)}` is two boxes.
    /// `type_lookup_test.rb` looks this type up by oid precisely to read that delimiter back
    /// (`tests/array_delimiter.rs`).
    BoxArray,
    /// PostgreSQL's `money`: **a count of cents in an `i64`**, and nothing else.
    ///
    /// `typlen` is 8 and `typstorage` is `p` — plain, not a varlena — so the range is exactly
    /// `i64`'s in hundredths: `92233720368547758.07` is the top and `.08` is `22003`. That is the
    /// whole type, and it is why it is not a `numeric`: a `numeric` has unbounded digits and a
    /// scale that travels with the value, where every `money` has scale 2 and a fixed width.
    ///
    /// **Its comparison is the integer's** and it shares that with nothing: `money = numeric` is
    /// `42883` on a real server, as is `money + 1`. ADR 0042's rule is met by keeping the
    /// representation to itself — `Datum::Money` — rather than by storing cents in an `Int8`,
    /// which would answer `bigint` to `pg_typeof` and admit every integer operator.
    Money,
    /// PostgreSQL's `inet`: an address and a prefix length.
    ///
    /// **`inet` and `cidr` are one representation and two types**, which ADR 0042 allows because
    /// they share a comparison: `'192.168.1.1'::inet = '192.168.1.1'::cidr` is `t` on a real
    /// server. What differs is the input rule and the output — a `cidr` refuses bits to the right
    /// of its mask and always prints its prefix, an `inet` does neither — so the *value* carries
    /// which of the two it is (`Datum::Inet::cidr`), the lesson `Datum::Hstore` records.
    Inet,
    /// PostgreSQL's `cidr`. See [`ColumnType::Inet`], whose representation it shares.
    Cidr,
    /// PostgreSQL's `macaddr`: **six bytes**, which is what `typlen` says, and its own comparison —
    /// no operator relates it to an address.
    MacAddr,
    /// `inet[]`, `cidr[]` and `macaddr[]`. `network_test.rb` declares none of the three; they exist
    /// because a real server pairs each with an array, and a base type whose `typarray` is `0` is
    /// what cost run 53 its 43 `can't quote Array` tests.
    InetArray,
    /// See [`ColumnType::InetArray`].
    CidrArray,
    /// See [`ColumnType::InetArray`].
    MacAddrArray,
    /// PostgreSQL's `bit(n)`: a fixed-length string of ones and zeros.
    ///
    /// **`bit` and `bit varying` are one representation and two types**, which ADR 0042 allows
    /// because they share a comparison — `B'101'::bit varying = B'101'::bit(3)` is `t` on a real
    /// server. What differs is the length rule and the name a client is told, so the value carries
    /// which of the two it is, exactly as `inet` and `cidr` do.
    Bit,
    /// PostgreSQL's `bit varying(n)`. See [`ColumnType::Bit`], whose representation it shares.
    VarBit,
    /// `bit[]` and `bit varying[]`. No suite test declares one; a real server pairs each with an
    /// array, and a base type whose `typarray` is `0` is what cost run 53 its 43 tests.
    BitArray,
    /// See [`ColumnType::BitArray`].
    VarBitArray,
    /// PostgreSQL's `lseg`, `box`, `path`, `polygon`, `circle` and `line`.
    ///
    /// Six types and **one representation** — the canonical text, the road `hstore` and the ranges
    /// take — because the canonical form is a function of the content: two shapes that print the
    /// same are the same shape. None of the six is an index key, for `point`'s reason one family
    /// along: `CREATE INDEX` on an `lseg` is `42704 … has no default operator class` and
    /// `count(DISTINCT)` over one is `42883 could not identify an equality operator`, both
    /// measured, even though the `=` operator itself answers.
    Lseg,
    /// See [`ColumnType::Lseg`]. **Its corners are reordered on the way in**: upper right first.
    Box,
    /// See [`ColumnType::Lseg`]. **Its bracket is data**: `[…]` is open and `(…)` is closed.
    Path,
    /// See [`ColumnType::Lseg`].
    Polygon,
    /// See [`ColumnType::Lseg`].
    Circle,
    /// See [`ColumnType::Lseg`]. `{A,B,C}`, and `A` and `B` may not both be zero.
    Line,
    /// PostgreSQL's `xml`: **`json`'s shape with a different validator**.
    ///
    /// A string stored exactly as it was sent — `'  <a/>  '` keeps its spaces and `'<a></a>'` does
    /// not become `'<a/>'` — once it is known to be well-formed XML *content*, which may be a bare
    /// text run and not only a document. Like `json` it has no equality operator at all, so it is
    /// not an index key, cannot be `DISTINCT`ed and cannot be ordered
    /// ([ADR 0042](../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)).
    Xml,
    /// `xml[]`. `xml_test.rb` declares no array; the type exists because a real server's `xml` has
    /// `typarray = 143`, and a base type whose `typarray` is `0` is what cost run 53 its 43 tests.
    XmlArray,
    /// The `ltree` extension's type: a **path of dot-separated labels**, stored as written.
    ///
    /// A label is one or more of `A-Za-z0-9_-` and any non-ASCII letter; the empty path is a
    /// value and has zero labels. Nothing is normalised, so equality is the text's — and
    /// **ordering is not**: ltree compares the labels in turn, and a shorter path that is a
    /// prefix of a longer one sorts first. See [`Datum::Ltree`] for what that costs the key.
    Ltree,
    /// `ltree[]`. `ltree_test.rb` declares none; the type exists because a real server's `ltree`
    /// has a `typarray`, and a base type whose `typarray` is `0` is what cost run 53 its 43 tests.
    LtreeArray,
    /// The `ltree` extension's `lquery`: a **pattern** over a path, not a path.
    ///
    /// `json`'s shape — a validated string with no comparison of its own — because that is all a
    /// pattern is here: it is written in a statement, matched with `~`, and never stored. No
    /// column in the suite is one, so it has no array type; see [`ColumnType::Ltree`] for the
    /// thing it matches.
    LQuery,
    /// `money[]`. No suite test declares one; the type exists because a real server's `money` has
    /// `typarray = 791`, and a base type whose `typarray` is `0` is what cost run 53 its 43
    /// `can't quote Array` tests.
    MoneyArray,
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
    /// `tsvector[]`. The capture builds one — `ARRAY['a b'::tsvector, 'c'::tsvector]` prints
    /// `{"'a' 'b'",'c'}` — so the element is quoted exactly when the array codec's own rules say
    /// so, which is what makes this a flat variant rather than a special case.
    TsVectorArray,
    /// `tsquery[]`, for the symmetry [ADR 0047](../../docs/adr/0047-an-array-is-a-column-type-over-one-element-type.md)
    /// asks of every element type. Nothing in the suite builds one.
    TsQueryArray,
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
    /// `regtype` — **an oid that prints as a type's name**
    /// ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
    ///
    /// Not a spelling of [`ColumnType::Oid`]: the two share a representation and a comparison, and
    /// differ only in the output function, which is the one thing `ColumnType` is allowed to
    /// distinguish. Measured — `'text'::regtype < 'int4'::regtype` is **false**, because the
    /// comparison is `25 < 23` and not the names; and `'text'::regtype = 25` is true against an
    /// uncast integer.
    RegType,
    /// `regclass`: an oid that prints as a **relation's** name, the type
    /// [ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)'s shape one
    /// letter along.
    ///
    /// The same two measurements decide it as decided `regtype`, and both are in
    /// `tests/captures/pg19_regclass.txt`: `'pg_class'::regclass = 1259` is **t** against an
    /// uncast integer, and `'pg_class'::regclass < 'pg_type'::regclass` is **f** — 1259 < 1247 is
    /// false where the *names* sort the other way, so the ordering is the oid's.
    ///
    /// It exists because a client has to be told: `ActiveRecord` reloads its type map when a
    /// `RowDescription` carries an oid it does not know, and this node was describing
    /// `'x'::regclass` as a `bigint` — an oid it knows — so three of its tests watched nothing
    /// happen (`tests/captures/pg19_unknown_oid.txt`).
    RegClass,
    /// `int2vector` and `oidvector`: `pg_index.indkey` and `pg_proc.proargtypes`.
    ///
    /// **Text's representation, like `json` and `jsonb`** — the value is the numbers space
    /// separated and this node has no vector of its own — and a `ColumnType` of its own because
    /// the *declared* type is what a client reads: `indkey` is an `int2vector` (oid 22) on a real
    /// server and `indclass` an `oidvector` (oid 30), and this node called both `text`.
    ///
    /// Measured (`tests/captures/pg19_indkey.txt`): `indkey` of a two-column index is `1 2`,
    /// `indclass` is `1978 3126`, both `typlen` -1, both `typcategory` `A`, and both subscript
    /// **from zero** where a SQL array subscripts from one.
    Int2Vector,
    /// `oidvector`, `int2vector`'s sibling — see it for the shape they share.
    OidVector,
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
    /// `name[]` — oid 1003, `_name`.
    ///
    /// The type `array_agg(enum.enumlabel)` has, which is how `ActiveRecord` reads an enum's
    /// labels: every identifier column of the catalog is a [`ColumnType::Name`], so an aggregate
    /// over one is an array of them and a client decodes the literal into a list. Told `text`
    /// instead it keeps the string, which is the ten tests run 106 lost.
    NameArray,
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
    /// `regtype[]`, which is what an `ARRAY['text'::regtype]` is.
    RegTypeArray,
}

/// The address family a `Datum::Inet` names: 4 for IPv4, 6 for IPv6, and the **first** byte of an
/// index key so that every IPv4 address sorts below every IPv6 one — PostgreSQL's own order.
pub const INET_V4: u8 = 4;
/// See [`INET_V4`].
pub const INET_V6: u8 = 6;

impl ColumnType {
    /// Every type **that has a `pg_type` row of its own**, for tests that must not silently skip
    /// one — and for the catalog, which derives that view from this list.
    ///
    /// Not quite "every variant": see [`ColumnType::USER_RANGES`] for the two that are
    /// representations of a user-defined type rather than types, and whose `pg_type` row is
    /// written by the `CREATE TYPE` that made them.
    pub const ALL: [ColumnType; 93] = [
        ColumnType::Int8,
        ColumnType::Int4,
        ColumnType::Int2,
        ColumnType::Text,
        ColumnType::Varchar,
        ColumnType::Bpchar,
        ColumnType::Name,
        ColumnType::Json,
        ColumnType::Jsonb,
        ColumnType::Hstore,
        ColumnType::Citext,
        ColumnType::TsRange,
        ColumnType::TstzRange,
        ColumnType::Int4Range,
        ColumnType::TsRangeArray,
        ColumnType::HstoreArray,
        ColumnType::TsVector,
        ColumnType::TsQuery,
        ColumnType::TsVectorArray,
        ColumnType::TsQueryArray,
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
        ColumnType::RegType,
        ColumnType::RegClass,
        ColumnType::Int2Vector,
        ColumnType::OidVector,
        ColumnType::Int8Array,
        ColumnType::Int4Array,
        ColumnType::Int2Array,
        ColumnType::NumericArray,
        ColumnType::TextArray,
        ColumnType::BoolArray,
        ColumnType::ByteaArray,
        ColumnType::BpcharArray,
        ColumnType::VarcharArray,
        ColumnType::NameArray,
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
        ColumnType::RegTypeArray,
        ColumnType::DateRange,
        ColumnType::NumRange,
        ColumnType::Int8Range,
        ColumnType::TstzRangeArray,
        ColumnType::Int4RangeArray,
        ColumnType::DateRangeArray,
        ColumnType::NumRangeArray,
        ColumnType::Int8RangeArray,
        ColumnType::Point,
        ColumnType::PointArray,
        ColumnType::BoxArray,
        ColumnType::Money,
        ColumnType::MoneyArray,
        ColumnType::Inet,
        ColumnType::Cidr,
        ColumnType::MacAddr,
        ColumnType::InetArray,
        ColumnType::CidrArray,
        ColumnType::MacAddrArray,
        ColumnType::Bit,
        ColumnType::VarBit,
        ColumnType::BitArray,
        ColumnType::VarBitArray,
        ColumnType::Lseg,
        ColumnType::Box,
        ColumnType::Path,
        ColumnType::Polygon,
        ColumnType::Circle,
        ColumnType::Line,
        ColumnType::Xml,
        ColumnType::XmlArray,
        ColumnType::Ltree,
        ColumnType::LtreeArray,
        ColumnType::LQuery,
    ];

    /// The range representations a **user-defined** type gets, which are deliberately *not* in
    /// [`ColumnType::ALL`].
    ///
    /// `ALL` is every type that has a `pg_type` row of its own, and everything derived from it
    /// says so: `esker_sql`'s `pg_type` view, `'name'::regtype`, `type_by_oid` for a parameter's
    /// declared oid. A `floatrange` has a `pg_type` row too — written by the `CREATE TYPE` that
    /// made it, with the oid that statement allocated — so putting these two in `ALL` would give
    /// it a *second* row, named after the subtype and with oid `0`, and would make
    /// `'float8range'::regtype` resolve where a real server answers `42704`.
    ///
    /// They are listed here so the codec's round-trip properties can still reach them: a type
    /// nothing generates is a type whose encoding is unchecked, which is how `range_subtype` came
    /// to have two copies that disagreed.
    pub const USER_RANGES: [ColumnType; 2] = [ColumnType::FloatRange, ColumnType::VarcharRange];
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
    /// [`ColumnType::TsVector`]: the lexeme set, as its canonical text.
    ///
    /// A variant of its own for exactly the reason [`Datum::Hstore`] is one, and the capture names
    /// the operator that proves it: `||` over two tsvectors **concatenates and renumbers**, which
    /// is not what `||` over two strings does. A folded `'…'::tsvector` that came out as a `Text`
    /// would have lost the only thing saying which concatenation to run.
    TsVector(String),
    /// [`ColumnType::TsQuery`]: the query, as its canonical text.
    TsQuery(String),
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
    /// [`ColumnType::Inet`] and [`ColumnType::Cidr`]: an address, its prefix length, and which of
    /// the two types the value is.
    ///
    /// The flag is part of the **representation** and not of the comparison: two values that
    /// differ only in it are equal to `esker_sql::value::PgDatum::pg_cmp` — `inet = cidr` is `t` —
    /// and different to `PartialEq`, which asks whether a round trip preserved the value. Without
    /// it a folded `'192.168.1.1'::cidr` constant would print `192.168.1.1`, one `/32` short.
    Inet {
        /// 4 or 6, and the **first** thing compared: every IPv4 address sorts below every IPv6 one.
        family: u8,
        /// The prefix length in bits.
        bits: u8,
        /// Whether the value is a `cidr` rather than an `inet`.
        cidr: bool,
        /// The address, big-endian, an IPv4 in the first four bytes and the rest zero.
        addr: [u8; 16],
    },
    /// [`ColumnType::Bit`] and [`ColumnType::VarBit`]: the ones and zeros, and which type it is.
    ///
    /// The flag is part of the **representation** and not of the comparison, the split
    /// [`Datum::Inet`] already makes: `B'101'::bit varying = B'101'::bit(3)` is `t`, and the two
    /// are different rows to `PartialEq` because that asks whether a round trip preserved the
    /// value.
    Bit {
        /// Whether the value is a `bit varying` rather than a `bit`.
        varying: bool,
        /// The digits, most significant first — `'101'::bit(8)` is `10100000` and not
        /// `00000101`, which is the half of the padding rule a reader gets wrong.
        bits: String,
    },
    /// One of the six geometric shapes, as its **canonical text**, and which of the six it is.
    ///
    /// The kind rides along for the reason [`Datum::Range`]'s subtype does: a folded
    /// `'…'::circle` constant that came out as a `Text` would have nothing left saying it is a
    /// circle, so `pg_typeof` could not tell one shape from another.
    Geometry {
        /// Which shape, as `esker_sql::value::geometric::Kind` spells it — carried here as the
        /// column type it corresponds to, so this crate needs no vocabulary of its own for it.
        kind: Box<ColumnType>,
        /// The canonical text, which is what `esker_sql::value::geometric` renders.
        text: String,
    },
    /// [`ColumnType::MacAddr`]: six bytes, which is the whole type.
    MacAddr([u8; 6]),
    /// [`ColumnType::Money`]: **cents**, as an `i64`.
    ///
    /// A variant of its own rather than an `Int8` under a different column type, for the reason
    /// [`Datum::Citext`] is one: the difference is in what the value *is*, and a comparison sees
    /// only the values. Storing cents in an `Int8` would make `pg_typeof` answer `bigint` and
    /// would let `money + 1` through, which is `42883` on a real server.
    ///
    /// The scale is not stored because it is not a property of the value: every `money` has two
    /// decimal places, which is what makes the whole type an integer.
    Money(i64),
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
    /// [`ColumnType::Ltree`]: a path of dot-separated labels, stored **as written**.
    ///
    /// Its own variant for `Citext`'s reason and not `json`'s: **an ltree's order is not its
    /// text's**. `'a.b' < 'a-b'` is true as an ltree and false as bytes, because a `.` (0x2E) is
    /// above a `-` (0x2D) and ltree compares label by label rather than character by character.
    /// [`crate::row`] encodes the key with the separator lowered below every byte a label can
    /// hold, which reproduces PostgreSQL's order exactly and is reversible — unlike a citext's
    /// fold, so an ltree key decodes back to the value it was written from.
    Ltree(String),
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
    /// [`ColumnType::RegType`]: the oid **and** the name it prints as
    /// ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
    ///
    /// **Compared by the oid alone**, which is measured and is the whole reason this is not a
    /// text: `'text'::regtype < 'int4'::regtype` is false because 25 < 23 is, and
    /// `'text'::regtype = 25` is true against an uncast integer.
    ///
    /// The name rides along because deriving it needs the catalog and this crate must not have one
    /// (invariant 7). It is resolved where the value is produced — the seam
    /// `CatalogFunc::UserRegType` already sits on — so a type a `CREATE TYPE` made prints its own
    /// name. An oid with no type carries its digits, which is what a real server prints for one.
    RegType {
        /// The oid, which is the value.
        oid: u32,
        /// What it prints as.
        name: Box<str>,
    },
    /// [`ColumnType::RegClass`]: the same, for a **relation**.
    ///
    /// The name rides along for the reason a `regtype`'s does — deriving it needs the catalog and
    /// this crate must not have one — and with one extra wrinkle that is measured: the printed
    /// name is **search-path dependent**. `'s1.t'::regclass` prints `s1.t` when `s1` is not in the
    /// path and `t` when it is. Resolving it where the value is produced is what makes that come
    /// out right: the cast is resolved once per statement, and a statement's search path cannot
    /// change under it. An oid naming no relation carries its digits, which is what a real server
    /// prints for one.
    RegClass {
        /// The oid, which is the value — **an `i64` where PostgreSQL's is four bytes**.
        ///
        /// A relation's identity here is `esker-catalog`'s 64-bit id, and truncating it into a
        /// `u32` to match the width of a real server's oid is how a value comes to name a
        /// *different* relation. The declared type is still `regclass` (2205); it is the value
        /// that is wider, and it stays wider because every column it is compared against —
        /// `attrelid`, `indrelid`, `conrelid`, `adrelid` — is a `bigint` here for the same reason.
        oid: i64,
        /// What it prints as, qualified only when the relation is not reachable unqualified.
        name: Box<str>,
    },
    /// One of the four array types: its elements, their shape, and where they are subscripted
    /// from (`crate::array`).
    ///
    /// **An element that is NULL is not a NULL array.** `'{NULL}'::int[] IS NULL` is false, and
    /// the two states are told apart here by `Datum::Null` against a `None` inside the value.
    Array(crate::array::ArrayValue),
    /// A `point`: its two `float8` coordinates, which is exactly what a real server stores in the
    /// sixteen bytes `typlen` reports.
    ///
    /// The coordinates rather than the text, so that a subscript is an arithmetic fact and not a
    /// parse — `p[0]` is a `double precision` on a real server, and zero-based.
    Point {
        /// The first coordinate, `p[0]`.
        x: f64,
        /// The second, `p[1]`.
        y: f64,
    },
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
            // **A money joins the two `i64` clocks**: for all three, representation equality *is*
            // value equality — a count of cents has exactly one spelling, which is not true of
            // the `numeric` two arms up.
            (Datum::Money(a), Datum::Money(b))
            | (Datum::Timestamp(a), Datum::Timestamp(b))
            | (Datum::Time(a), Datum::Time(b))
            // **A `regclass`'s value is an `i64` too**, and it joins them rather than repeating
            // their body: a relation's id is 64 bits here where a real server's oid is four.
            | (Datum::RegClass { oid: a, .. }, Datum::RegClass { oid: b, .. }) => a == b,
            (Datum::Uuid(a), Datum::Uuid(b)) => a == b,
            // **Representation equality, flag included**, which is not the SQL comparison: an
            // `inet` and a `cidr` holding the same address are equal on a real server and are two
            // different rows here. `pg_cmp` is where the value is compared.
            (
                Datum::Inet {
                    family: af,
                    bits: ab,
                    cidr: ac,
                    addr: aa,
                },
                Datum::Inet {
                    family: bf,
                    bits: bb,
                    cidr: bc,
                    addr: ba,
                },
            ) => af == bf && ab == bb && ac == bc && aa == ba,
            (Datum::MacAddr(a), Datum::MacAddr(b)) => a == b,
            // Kind and text, which together are the value: two shapes that print the same and are
            // the same type are the same row.
            (Datum::Geometry { kind: ak, text: at }, Datum::Geometry { kind: bk, text: bt }) => {
                ak == bk && at == bt
            }
            // Representation equality, flag included — see the variant's own note.
            (
                Datum::Bit {
                    varying: av,
                    bits: ab,
                },
                Datum::Bit {
                    varying: bv,
                    bits: bb,
                },
            ) => av == bv && ab == bb,
            // A `regtype` beside the `oid` it is: the oid alone decides, because two spellings of
            // one type are one value and the name is only how it prints —
            // `'int4'::regtype = 'integer'::regtype` is `t` on a real server. **The two are still
            // separate pairs**: this is representation equality, and a row holding an `oid` is not
            // a row holding a `regtype`.
            (Datum::Oid(a), Datum::Oid(b))
            | (Datum::RegType { oid: a, .. }, Datum::RegType { oid: b, .. }) => a == b,
            // Representation equality, element by element: two arrays that print the same are
            // the same row. What `1.0` and `1.00` are to a `numeric`, `{1.0}` and `{1.00}` are
            // to a `numeric[]`, and `pg_cmp` is again where the *values* are compared.
            (Datum::Array(a), Datum::Array(b)) => a == b,
            // **By the bits, like every float in this impl**, so a `point` survives the round
            // trip whatever it holds — `NaN` included. It is not a SQL equality and there is no
            // SQL equality for a point to be confused with: `point = point` is `42883`, which is
            // why `pg_cmp` has no arm for one either.
            (Datum::Point { x: ax, y: ay }, Datum::Point { x: bx, y: by }) => {
                ax.to_bits() == bx.to_bits() && ay.to_bits() == by.to_bits()
            }
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
            // And for an ltree that is the path as written, which is also its SQL equality — the
            // type differs from `text` in its *order*, not in what two equal values are. It
            // arrived without this line and was never equal to itself either; the same property
            // test caught it, one type later.
            | (Datum::Ltree(a), Datum::Ltree(b))
            | (Datum::Hstore(a), Datum::Hstore(b))
            | (Datum::TsVector(a), Datum::TsVector(b))
            | (Datum::TsQuery(a), Datum::TsQuery(b)) => a == b,
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
            Datum::Ltree(_) => ColumnType::Ltree,
            Datum::Point { .. } => ColumnType::Point,
            Datum::Money(_) => ColumnType::Money,
            // The flag is what tells the two apart, which is the whole reason it is carried.
            Datum::Inet { cidr: true, .. } => ColumnType::Cidr,
            Datum::Inet { .. } => ColumnType::Inet,
            Datum::MacAddr(_) => ColumnType::MacAddr,
            Datum::Geometry { kind, .. } => **kind,
            Datum::Bit { varying: true, .. } => ColumnType::VarBit,
            Datum::Bit { .. } => ColumnType::Bit,
            Datum::Hstore(_) => ColumnType::Hstore,
            Datum::TsVector(_) => ColumnType::TsVector,
            Datum::TsQuery(_) => ColumnType::TsQuery,
            // **The inverse of `crate::row::range_subtype`, and it is not total.** `int4range`
            // and `int8range` are both ranges *of* an `int8` here — an `int4` is read as one
            // everywhere in this crate — so a value carrying that subtype could be either, and
            // this answers the first as a representative. Which of the two a value **fits** is a
            // different question and [`Datum::fits`] asks it properly, against the column's own
            // subtype; nothing needs a single answer except a value with no column beside it.
            Datum::Range { subtype, .. } => match **subtype {
                ColumnType::TimestampTz => ColumnType::TstzRange,
                ColumnType::Int4 | ColumnType::Int8 => ColumnType::Int4Range,
                ColumnType::Date => ColumnType::DateRange,
                ColumnType::Numeric => ColumnType::NumRange,
                ColumnType::Double => ColumnType::FloatRange,
                ColumnType::Varchar => ColumnType::VarcharRange,
                _ => ColumnType::TsRange,
            },
            Datum::Int4(_) => ColumnType::Int4,
            Datum::Date(_) => ColumnType::Date,
            Datum::Time(_) => ColumnType::Time,
            Datum::Uuid(_) => ColumnType::Uuid,
            Datum::Interval { .. } => ColumnType::Interval,
            Datum::Oid(_) => ColumnType::Oid,
            Datum::RegType { .. } => ColumnType::RegType,
            Datum::RegClass { .. } => ColumnType::RegClass,
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
        // **A range fits the column whose subtype it carries**, which `column_type` alone cannot
        // decide: `int4range` and `int8range` are both ranges of an `int8` here, so a value's
        // subtype names a *set* of column types and not one. Asked from the column's side, where
        // the answer is single-valued.
        if let Datum::Range { subtype, .. } = self {
            return crate::row::range_subtype(ty) == **subtype
                && crate::array::ArrayValue::element_of(ty).is_none();
        }
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
            ColumnType::Varchar
                // **A `name` is its text too**, and truncated before it ever reaches a row: the
                // 63-byte cut belongs to the cast, where the character boundary is known, so what
                // arrives here is already a value of the type.
                | ColumnType::Name
                | ColumnType::Bpchar
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Xml
                | ColumnType::LQuery
                // **The two catalog vectors are their text**: space-separated numbers, which is
                // what `decode_row` hands back for one and all the column holds.
                | ColumnType::Int2Vector
                | ColumnType::OidVector
        )
        // **A `regtype` fits an `oid` column and the reverse**, which is ADR 0042's rule met in
        // full: the two share a representation *and* a comparison — the oid, in both directions —
        // and differ only in the output function. That is what makes
        // `castsource = 'character varying'::regtype` the comparison a real server makes.
        | (
            ColumnType::RegType,
            ColumnType::Oid
        )
        | (ColumnType::Oid, ColumnType::RegType)
        // A `regclass` beside an `oid`, for the same reason and with the same measurement:
        // `SELECT count(*) > 0 FROM pg_attribute WHERE attrelid = 'pg_class'::regclass` is `t`.
        //
        // **And beside an `int8`, which is temporary and is written down as such.** Every
        // relation-oid column in this node's catalog — `attrelid`, `adrelid`, `conrelid`,
        // `indrelid` — is declared `bigint` where a real server declares `oid`
        // (`tests/captures/pg19_regclass.txt`), and `WHERE attrelid = 'iv'::regclass` is the
        // commonest statement in the catalog corpora. That pair is what keeps those comparing
        // while the cast stops being a `bigint`; it goes when those four columns become `oid`,
        // which is the move ADR 0077 made for `pg_enum.enumtypid` for exactly this reason.
        | (ColumnType::RegClass, ColumnType::Oid | ColumnType::Int8)
        | (ColumnType::Oid | ColumnType::Int8, ColumnType::RegClass)
    )
}

/// The sign bit of an IEEE-754 binary64.
const SIGN: u64 = 1 << 63;
