//! `interval`, against PostgreSQL 19beta1 — tier 2, and what every `time` arithmetic answers.
//!
//! # The output style is the gap this type creates
//!
//! There are **four**, and `ActiveRecord` asks for the one that is not the default:
//! `SET intervalstyle = iso_8601` is its **third** boot statement, so `P1D` and not `1 day` is
//! what Rails expects to read back. `crate::parameter` already accepts the setting and reports
//! it, and says it is inert — which was true while there was no interval type. There is one now.
//!
//! This node prints the `postgres` style always. Closing it means threading the session's
//! setting into value formatting, which no type has needed before and which is not this unit's:
//! the three readings under a non-default style are trimmed from the corpus, with the reason
//! written in its header, because all four are the *same statement text* and the harness keys a
//! divergence by that text — one of the four agrees, so the key would have to be both listed and
//! not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
/// One of `DIVERGENCES`' reasons: the typmod is a **bitmask** and this node drops it.
const TYPMOD: &str = "**`interval`'s typmod is a field mask, not a number** — `interval day` is \
     589823 and `interval day to hour` is 67698687, fields in the high bits and precision in the \
     low — and it does more than name a width: it says which fields the value *keeps*, so \
     `'1 2'` means one day and two hours **because** the column is `day to hour`. This node \
     parses the syntax and drops the mask, so such a column stores every field and a literal \
     that only has meaning under one is refused — which is exactly what a real server does with \
     the same literal and no typmod: `'1 2'::interval` is `22007` there too. Everything reading \
     that table follows, because the rows were never inserted.";

/// The engine has no arithmetic operator at all.
const ARITHMETIC: &str = "`plan::BinaryOp` is Eq/NotEq/Lt/LtEq/Gt/GtEq/And/Or, and every one of \
     its uses assumes a comparison producing a boolean — so `+`, `-`, `*` and `/` over any pair \
     are `0A000` naming the operator. This is the type those operators mostly *answer with*, \
     which is why the whole family is here rather than in `tests/time.rs`: landing interval \
     removes the reason arithmetic could not be modelled, and not the arithmetic.";

/// Functions this node has for no type.
const FUNCTIONS: &str = "A function this node does not implement for any type, named rather than \
     answered: `justify_days`/`justify_hours`/`justify_interval` are the three that do the \
     carrying this type deliberately does not, `age` builds an interval from two instants, \
     `extract` and `greatest`/`least` are general, and `pg_typeof` reads the catalog.";

/// The `bpchar` explicit-cast truncation, recorded a third time.
const BPCHAR: &str = "An explicit cast to `character(n)` truncates on a real server and raises \
     `22001` here — `tests/time.rs` and `tests/uuid.rs` record the same thing. The `varchar` \
     half diverges only in its declared type, `text` for `character varying`.";

/// A bare integer literal is an `int8` here.
const LITERAL: &str = "Both refuse with `42883` and name a different integer: PostgreSQL says \
     `integer` because a bare `1` is an `int4` there, and this node says `bigint`. A divergence \
     of the literal, not of this type.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `typname` is a `name`, `typinput` a `regproc` and `typcategory` a `"char"` on a real
        // server; all three are `text` here with identical characters, and `typlen` agrees
        // exactly. The trade every `pg_catalog` column makes.
        "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname = \
         'interval'",
        // The value is right — `interval(3)` trims where `numeric(p,s)` pads, so `1.5 seconds`
        // is `00:00:01.5` — and the declared type drops the precision, because a cast carries
        // no typmod here for any parameterised type (`tests/numeric.rs`, `tests/time.rs`).
        "SELECT '1.5 seconds'::interval(3)",
    ],
    answers: &[
        (
            "SELECT pg_typeof('1 day'::interval), format_type(1186, -1)",
            TYPMOD,
        ),
        (
            "SELECT attname, atttypmod, format_type(atttypid, atttypmod) FROM \
             pg_attribute WHERE attrelid = 'iv'::regclass AND attnum > 0 ORDER BY attnum",
            TYPMOD,
        ),
        ("SELECT format_type(1186, 3)", TYPMOD),
        ("SELECT format_type(1186, 32767)", TYPMOD),
        (
            "INSERT INTO iv VALUES (1, '1 year 2 mons 3 days 04:05:06', '1.5 seconds', \
             '1 day', '1 2', '1-2')",
            TYPMOD,
        ),
        (
            "INSERT INTO iv VALUES (2, '1 mon', '00:00:00', '2 days', '0 0', '0-0')",
            TYPMOD,
        ),
        (
            "INSERT INTO iv VALUES (3, '30 days', '00:00:00', '3 days', '0 0', '0-0')",
            TYPMOD,
        ),
        (
            "INSERT INTO iv VALUES (4, '-1 day', '00:00:00', '-1 days', '0 0', '0-0')",
            TYPMOD,
        ),
        ("SELECT id, a, b, c, d, e FROM iv ORDER BY id", TYPMOD),
        ("SELECT id, a FROM iv ORDER BY a", TYPMOD),
        ("SELECT id, a FROM iv ORDER BY a DESC", TYPMOD),
        ("SELECT id FROM iv WHERE a = '1 mon' ORDER BY id", TYPMOD),
        ("SELECT id FROM iv WHERE a > '1 day' ORDER BY id", TYPMOD),
        ("SELECT count(*), count(a), min(a), max(a) FROM iv", TYPMOD),
        ("SELECT sum(a) FROM iv", TYPMOD),
        ("SELECT avg(a) FROM iv", TYPMOD),
        (
            "SELECT INTERVAL '1 day', INTERVAL '1' DAY, INTERVAL '1 2' DAY TO HOUR",
            TYPMOD,
        ),
        (
            "SELECT '1 second'::interval(0), '1.5 seconds'::interval(0)",
            TYPMOD,
        ),
        ("SELECT '1 year 1 month'::interval::interval year", TYPMOD),
        ("SELECT 'interval(9)'::regtype::oid", TYPMOD),
        (
            "SELECT justify_days('35 days'::interval), justify_hours('27 \
             hours'::interval), justify_interval('1 mon 33 days 27 hours'::interval)",
            FUNCTIONS,
        ),
        (
            "SELECT age('2021-03-01'::timestamp, '2021-01-01'::timestamp)",
            FUNCTIONS,
        ),
        (
            "SELECT extract(day FROM '1 year 2 mons 3 days'::interval), extract(epoch \
             FROM '1 day'::interval)",
            FUNCTIONS,
        ),
        (
            "SELECT '1 day'::interval::varchar, '1 day'::interval::char(3)",
            BPCHAR,
        ),
        ("SELECT '1 day'::interval::time", ARITHMETIC),
        ("SELECT '1 day'::interval = 1", LITERAL),
        (
            "SELECT greatest('1 day'::interval, '2 days'::interval), least('1 \
             day'::interval, '2 days'::interval)",
            FUNCTIONS,
        ),
    ],
};

#[test]
fn every_interval_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_interval.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}
