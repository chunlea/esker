//! **Which pairs `||` concatenates** — wire v3 families **F3a** and **F3b**, one `match` and
//! therefore one unit.
//!
//! `exec::query`'s `concat_type` was a chain of "any operand of type X" arms ending in a `text`
//! fallback, and `exec::cursor`'s `CatalogFunc::Concat` rendered every operand as text and joined
//! them. So **everything concatenated**, and the two families are that one fact seen from its two
//! sides: `json || json` answered `text` where 19beta1 has no operator at all (F3a), and
//! `bytea || bytea` refused where 19beta1 has one (F3b).
//!
//! The arm chain's own comments count four previous fixes, each adding an arm for one more operand
//! type. This is the table instead, and the table is measured:
//! `tests/captures/pg19_concat_operator.txt` carries all **100** distinct type spellings of the
//! wire v3 probe list asked three ways, because `||` is not one operator —
//! `pg_operator` has `text || text`, `anynonarray || text`, `text || anynonarray`,
//! `anyarray || anyarray`, `anyarray || anyelement`, `anyelement || anyarray` and a handful of
//! same-type ones. A probe that asks only `x || x` sees a third of it, which is what the census's
//! 22 rows were: the real surface is **105 diverging shape-rows of 300**.
//!
//! Read off the measurement:
//!
//! ```text
//! x || x    every array -> itself · char/varchar/citext/name -> text · bit -> BIT VARYING
//!           bytea, tsvector, tsquery, jsonb, hstore, ltree -> itself
//!           int2vector -> smallint[] · oidvector -> oid[] · 39 others -> 42883
//!           "char" -> 42725 operator is not unique, a third sqlstate
//!
//! x || text every non-array scalar -> text · ltree -> ltree · string arrays -> the array
//!           every other array -> 42883 · "char" -> 42725
//! ```
//!
//! **`bit || bit` is `bit varying`, not `bit`**, and **`citext || citext` is `text`, not `citext`**
//! — two widenings an arm written by hand gets wrong, and the reason the table is a table.
//!
//! **Two units, and this file is the first.** It decides *which pairs have a `||` and what type it
//! answers*; `tests/concat_values.rs` decides *what the value is*. Six rows were pinned here at
//! today's answer while the second was outstanding — `bit`, `bytea` and `tsquery` beside
//! themselves, `hstore || text` both ways, `text || tsvector` — and every one is now asserted like
//! the rest, because that unit landed. The two vectors are still pinned: they are **F6**.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `(type, a literal of it, x || x, x || text, text || x)` — measured on 19beta1, every spelling
/// the wire v3 probe list carries. An entry starting with `!` is a refusal, with the whole
/// sentence PostgreSQL sends including its `DETAIL` and `HINT`.
#[rustfmt::skip]
static CONCAT: [(&str, &str, &str, &str, &str); 100] = [
    ("\"char\"", "'x'::\"char\"", "!42725 operator is not unique: \"char\" || \"char\" DETAIL: Could not choose a best candidate operator. HINT: You might need to add explicit type casts.", "!42725 operator is not unique: \"char\" || text DETAIL: Could not choose a best candidate operator. HINT: You might need to add explicit type casts.", "!42725 operator is not unique: text || \"char\" DETAIL: Could not choose a best candidate operator. HINT: You might need to add explicit type casts."),
    ("\"char\"[]", "'{\"x\"}'::\"char\"[]", "\"char\"[]", "!42883 operator does not exist: \"char\"[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || \"char\"[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("bigint", "'1'::bigint", "!42883 operator does not exist: bigint || bigint DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("bigint[]", "'{\"1\"}'::bigint[]", "bigint[]", "!42883 operator does not exist: bigint[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || bigint[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("bit", "'1'::bit", "bit varying", "text", "text"),
    ("bit[]", "'{\"1\"}'::bit[]", "bit[]", "!42883 operator does not exist: bit[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || bit[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("boolean", "'t'::boolean", "!42883 operator does not exist: boolean || boolean DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("boolean[]", "'{\"t\"}'::boolean[]", "boolean[]", "!42883 operator does not exist: boolean[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || boolean[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("box", "'(1,1),(0,0)'::box", "!42883 operator does not exist: box || box DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("box[]", "'{\"(1,1),(0,0)\"}'::box[]", "box[]", "!42883 operator does not exist: box[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || box[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("bytea", "'\\x41'::bytea", "bytea", "text", "text"),
    ("bytea[]", "'{\"\\\\x41\"}'::bytea[]", "bytea[]", "!42883 operator does not exist: bytea[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || bytea[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("character", "'x'::character", "text", "text", "text"),
    ("character varying", "'x'::character varying", "text", "text", "text"),
    ("character varying[]", "'{\"x\"}'::character varying[]", "character varying[]", "character varying[]", "text[]"),
    ("character[]", "'{\"x\"}'::character[]", "character[]", "character[]", "text[]"),
    ("cidr", "'127.0.0.0/24'::cidr", "!42883 operator does not exist: cidr || cidr DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("cidr[]", "'{\"127.0.0.0/24\"}'::cidr[]", "cidr[]", "!42883 operator does not exist: cidr[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || cidr[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("circle", "'<(0,0),1>'::circle", "!42883 operator does not exist: circle || circle DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("circle[]", "'{\"<(0,0),1>\"}'::circle[]", "circle[]", "!42883 operator does not exist: circle[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || circle[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("citext", "'x'::citext", "text", "text", "text"),
    ("citext[]", "'{\"x\"}'::citext[]", "citext[]", "text[]", "text[]"),
    ("date", "'2020-01-01'::date", "!42883 operator does not exist: date || date DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("date[]", "'{\"2020-01-01\"}'::date[]", "date[]", "!42883 operator does not exist: date[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || date[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("daterange", "'[2020-01-01,2020-01-02)'::daterange", "!42883 operator does not exist: daterange || daterange DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("daterange[]", "'{\"[2020-01-01,2020-01-02)\"}'::daterange[]", "daterange[]", "!42883 operator does not exist: daterange[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || daterange[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("double precision", "'1.5'::double precision", "!42883 operator does not exist: double precision || double precision DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("double precision[]", "'{\"1.5\"}'::double precision[]", "double precision[]", "!42883 operator does not exist: double precision[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || double precision[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("hstore", "'a=>1'::hstore", "hstore", "text", "text"),
    ("hstore[]", "'{\"a=>1\"}'::hstore[]", "hstore[]", "!42883 operator does not exist: hstore[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || hstore[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("inet", "'127.0.0.1'::inet", "!42883 operator does not exist: inet || inet DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("inet[]", "'{\"127.0.0.1\"}'::inet[]", "inet[]", "!42883 operator does not exist: inet[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || inet[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("int2vector", "'1 2'::int2vector", "smallint[]", "!42883 operator does not exist: int2vector || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || int2vector DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("int4range", "'[1,3)'::int4range", "!42883 operator does not exist: int4range || int4range DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("int4range[]", "'{\"[1,3)\"}'::int4range[]", "int4range[]", "!42883 operator does not exist: int4range[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || int4range[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("int8range", "'[1,3)'::int8range", "!42883 operator does not exist: int8range || int8range DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("int8range[]", "'{\"[1,3)\"}'::int8range[]", "int8range[]", "!42883 operator does not exist: int8range[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || int8range[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("integer", "'1'::integer", "!42883 operator does not exist: integer || integer DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("integer[]", "'{\"1\"}'::integer[]", "integer[]", "!42883 operator does not exist: integer[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || integer[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("interval", "'1 day'::interval", "!42883 operator does not exist: interval || interval DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("interval[]", "'{\"1 day\"}'::interval[]", "interval[]", "!42883 operator does not exist: interval[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || interval[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("json", "'{\"a\":1}'::json", "!42883 operator does not exist: json || json DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("json[]", "'{\"{\\\"a\\\":1}\"}'::json[]", "json[]", "!42883 operator does not exist: json[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || json[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("jsonb", "'{\"a\":1}'::jsonb", "jsonb", "text", "text"),
    ("jsonb[]", "'{\"{\\\"a\\\":1}\"}'::jsonb[]", "jsonb[]", "!42883 operator does not exist: jsonb[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || jsonb[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("line", "'{1,-1,0}'::line", "!42883 operator does not exist: line || line DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("line[]", "'{\"{1,-1,0}\"}'::line[]", "line[]", "!42883 operator does not exist: line[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || line[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("lquery", "'a.*'::lquery", "!42883 operator does not exist: lquery || lquery DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("lseg", "'[(0,0),(1,1)]'::lseg", "!42883 operator does not exist: lseg || lseg DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("lseg[]", "'{\"[(0,0),(1,1)]\"}'::lseg[]", "lseg[]", "!42883 operator does not exist: lseg[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || lseg[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("ltree", "'a.b'::ltree", "ltree", "ltree", "ltree"),
    ("ltree[]", "'{\"a.b\"}'::ltree[]", "ltree[]", "!42883 operator does not exist: ltree[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || ltree[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("macaddr", "'08:00:2b:01:02:03'::macaddr", "!42883 operator does not exist: macaddr || macaddr DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("macaddr[]", "'{\"08:00:2b:01:02:03\"}'::macaddr[]", "macaddr[]", "!42883 operator does not exist: macaddr[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || macaddr[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("money", "'12.34'::money", "!42883 operator does not exist: money || money DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("money[]", "'{\"12.34\"}'::money[]", "money[]", "!42883 operator does not exist: money[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || money[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("name", "'x'::name", "text", "text", "text"),
    ("name[]", "'{\"x\"}'::name[]", "name[]", "name[]", "text[]"),
    ("numeric", "'1.5'::numeric", "!42883 operator does not exist: numeric || numeric DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("numeric[]", "'{\"1.5\"}'::numeric[]", "numeric[]", "!42883 operator does not exist: numeric[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || numeric[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("numrange", "'[1,3)'::numrange", "!42883 operator does not exist: numrange || numrange DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("numrange[]", "'{\"[1,3)\"}'::numrange[]", "numrange[]", "!42883 operator does not exist: numrange[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || numrange[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("oid", "'1'::oid", "!42883 operator does not exist: oid || oid DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("oid[]", "'{\"1\"}'::oid[]", "oid[]", "!42883 operator does not exist: oid[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || oid[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("oidvector", "'1 2'::oidvector", "oid[]", "!42883 operator does not exist: oidvector || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || oidvector DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("path", "'((0,0),(1,1))'::path", "!42883 operator does not exist: path || path DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("path[]", "'{\"((0,0),(1,1))\"}'::path[]", "path[]", "!42883 operator does not exist: path[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || path[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("point", "'(1,1)'::point", "!42883 operator does not exist: point || point DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("point[]", "'{\"(1,1)\"}'::point[]", "point[]", "!42883 operator does not exist: point[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || point[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("polygon", "'((0,0),(1,1),(1,0))'::polygon", "!42883 operator does not exist: polygon || polygon DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("polygon[]", "'{\"((0,0),(1,1),(1,0))\"}'::polygon[]", "polygon[]", "!42883 operator does not exist: polygon[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || polygon[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("real", "'1.5'::real", "!42883 operator does not exist: real || real DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("real[]", "'{\"1.5\"}'::real[]", "real[]", "!42883 operator does not exist: real[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || real[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("regclass", "'pg_class'::regclass", "!42883 operator does not exist: regclass || regclass DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("regclass[]", "'{\"pg_class\"}'::regclass[]", "regclass[]", "!42883 operator does not exist: regclass[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || regclass[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("regproc", "'int4in'::regproc", "!42883 operator does not exist: regproc || regproc DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("regproc[]", "'{\"int4in\"}'::regproc[]", "regproc[]", "!42883 operator does not exist: regproc[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || regproc[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("regtype", "'int4'::regtype", "!42883 operator does not exist: regtype || regtype DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("regtype[]", "'{\"int4\"}'::regtype[]", "regtype[]", "!42883 operator does not exist: regtype[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || regtype[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("smallint", "'1'::smallint", "!42883 operator does not exist: smallint || smallint DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("smallint[]", "'{\"1\"}'::smallint[]", "smallint[]", "!42883 operator does not exist: smallint[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || smallint[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("text", "NULL::text", "text", "text", "text"),
    ("text[]", "'{\"x\"}'::text[]", "text[]", "text[]", "text[]"),
    ("time", "'12:34:56'::time", "!42883 operator does not exist: time without time zone || time without time zone DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("timestamp", "'2020-01-01 00:00:00'::timestamp", "!42883 operator does not exist: timestamp without time zone || timestamp without time zone DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("timestamp with time zone", "'2020-01-01 00:00:00+00'::timestamp with time zone", "!42883 operator does not exist: timestamp with time zone || timestamp with time zone DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("timestamp with time zone[]", "'{\"2020-01-01 00:00:00+00\"}'::timestamp with time zone[]", "timestamp with time zone[]", "!42883 operator does not exist: timestamp with time zone[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || timestamp with time zone[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("tsquery", "'a & b'::tsquery", "tsquery", "text", "text"),
    ("tsquery[]", "'{\"a & b\"}'::tsquery[]", "tsquery[]", "!42883 operator does not exist: tsquery[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || tsquery[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("tsrange", "'[2020-01-01,2020-01-02)'::tsrange", "!42883 operator does not exist: tsrange || tsrange DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("tsrange[]", "'{\"[2020-01-01,2020-01-02)\"}'::tsrange[]", "tsrange[]", "!42883 operator does not exist: tsrange[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || tsrange[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("tstzrange", "'[2020-01-01,2020-01-02)'::tstzrange", "!42883 operator does not exist: tstzrange || tstzrange DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("tstzrange[]", "'{\"[2020-01-01,2020-01-02)\"}'::tstzrange[]", "tstzrange[]", "!42883 operator does not exist: tstzrange[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || tstzrange[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("tsvector", "'a b'::tsvector", "tsvector", "text", "text"),
    ("tsvector[]", "'{\"a b\"}'::tsvector[]", "tsvector[]", "!42883 operator does not exist: tsvector[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || tsvector[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("uuid", "'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid", "!42883 operator does not exist: uuid || uuid DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("uuid[]", "'{\"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\"}'::uuid[]", "uuid[]", "!42883 operator does not exist: uuid[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || uuid[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
    ("void", "NULL::void", "!42883 operator does not exist: void || void DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("xml", "'<a/>'::xml", "!42883 operator does not exist: xml || xml DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "text", "text"),
    ("xml[]", "'{\"<a/>\"}'::xml[]", "xml[]", "!42883 operator does not exist: xml[] || text DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts.", "!42883 operator does not exist: text || xml[] DETAIL: No operator of that name accepts the given argument types. HINT: You might need to add explicit type casts."),
];

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE EXTENSION IF NOT EXISTS hstore",
        "CREATE EXTENSION IF NOT EXISTS ltree",
        "CREATE EXTENSION IF NOT EXISTS citext",
    ])
}

/// **Two spellings this unit does not close**, pinned so they are not mistaken for coverage.
///
/// **[ADR 0107](../../../docs/adr/0107-a-borrowed-representation-needs-somewhere-to-carry-its-identity.md)
/// is the decision this waits on**, and its step two is what turns this test red.
///
/// `int2vector || int2vector` is `smallint[]` on 19beta1 and `oidvector || oidvector` is `oid[]` —
/// PostgreSQL's two vectors really are arrays. Here they are `Datum::Text`, so answering the array
/// type would mean *parsing* the value and not only declaring a type: that is family **F6**, the
/// borrowed representation, and it is a different unit.
const BORROWED: [&str; 2] = ["int2vector", "oidvector"];

/// Which of the three probes a row is about: `x || x`, `x || text`, `text || x`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Both,
    Right,
    Left,
}

impl Shape {
    /// The statement this shape asks of one type's literal.
    fn sql(self, lit: &str, typeof_it: bool) -> String {
        let expr = match self {
            Shape::Both => format!("{lit} || {lit}"),
            Shape::Right => format!("{lit} || 'a'::text"),
            Shape::Left => format!("'a'::text || {lit}"),
        };
        if typeof_it {
            format!("SELECT pg_typeof({expr})")
        } else {
            format!("SELECT {expr}")
        }
    }
}

/// **Every pair 19beta1 concatenates, and the type it answers.**
#[test]
fn the_pairs_postgresql_concatenates() {
    let mut node = node();
    for (ty, lit, of_self, of_right, of_left) in CONCAT {
        for (expected, shape) in [
            (of_self, Shape::Both),
            (of_right, Shape::Right),
            (of_left, Shape::Left),
        ] {
            // The refusals are the other test's and the two vectors are F6's, pinned in a test
            // of their own rather than skipped in silence. **The six rows that were skipped here
            // for the evaluator are not skipped any more** — `tests/concat_values.rs` built their
            // values, so `bit`, `bytea`, `tsquery`, `hstore || text` and `text || tsvector` are
            // asserted below like every other pair.
            if expected.starts_with('!') || BORROWED.contains(&ty) {
                continue;
            }
            let sql = shape.sql(lit, true);
            assert_eq!(
                node.rows(&sql),
                vec![vec![expected]],
                "{sql} is {expected} on 19beta1"
            );
        }
    }
}

/// **Every pair it has no operator for**, in its own sentence.
///
/// Two sqlstates, and they are two different answers: `42883` is "no candidate matched", `42725`
/// is "two did and neither is preferred". `"char"` is the only spelling here that gets the second,
/// on all three shapes.
#[test]
fn the_pairs_postgresql_has_no_operator_for() {
    let mut node = node();
    for (_, lit, of_self, of_right, of_left) in CONCAT {
        for (expected, shape) in [
            (of_self, Shape::Both),
            (of_right, Shape::Right),
            (of_left, Shape::Left),
        ] {
            if !expected.starts_with('!') {
                continue;
            }
            let sql = shape.sql(lit, false);
            assert_eq!(node.answer(&sql).to_string(), expected, "{sql}");
        }
    }
}

/// **The two vectors, pinned at what this node answers today** — not at what 19beta1 answers,
/// because closing them is F6's unit and an `#[ignore]`d test that would pass is worse than none.
#[test]
fn the_two_vectors_are_still_text() {
    let mut node = node();
    for (ty, pg) in [("int2vector", "smallint[]"), ("oidvector", "oid[]")] {
        let lit = if ty == "int2vector" {
            "'1 2'::int2vector"
        } else {
            "'1 2'::oidvector"
        };
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({lit} || {lit})")),
            vec![vec!["text"]],
            "19beta1 answers {pg} here; this node borrows text's representation for {ty} and \
             cannot until F6 lands"
        );
    }
}
