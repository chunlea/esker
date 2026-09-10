//! `interval`, against PostgreSQL 19beta1 — tier 2, and what every `time` arithmetic answers.
//!
//! # The output style was the gap this type created, and it is closed
//!
//! There are **four**, and `ActiveRecord` asks for the one that is not the default:
//! `SET intervalstyle = iso_8601` is its **third** boot statement, so `P1D` and not `1 day` is
//! what Rails expects to read back. The note here used to say that `crate::parameter` accepted the
//! setting, reported it and did nothing with it — "which was true while there was no interval
//! type. There is one now." — and that closing it meant threading a session setting into value
//! formatting, which no type had needed before.
//!
//! It is threaded (`tests/interval_style.rs`), and what the gap cost while it stood is the reason
//! to record rather than the plumbing: `OID::Interval#cast_value` **rescues a parse failure by
//! returning `nil`**, so two `interval_test.rb` tests read no value at all and nothing anywhere
//! said why. A wrong dialect is not a wrong answer here, it is a missing one.
//!
//! The corpus's style tail is back with it — all four readings, all agreeing.

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

/// Functions this node has for no type.
const FUNCTIONS: &str = "A function this node does not implement for any type, named rather than \
     answered: `justify_days`/`justify_hours`/`justify_interval` are the three that do the \
     carrying this type deliberately does not, `age` builds an interval from two instants, \
     `extract` is general, and `pg_typeof` reads the catalog. **`greatest`/`least` left this \
     list**: they are built, and answer an interval like any other ordered type.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty, and three units emptied it.** The `interval(0)` entry left with g1's typmod
    // propagation (#28) — a cast carries its modifier now — and the `pg_type` row left when the
    // last of its five columns stopped diverging: `typname` is a `name` (ADR 0084), `typcategory`
    // a `"char"` (ADR 0095), `typinput` a `regproc` (ADR 0098) and `oid` an `oid` (ADR 0097).
    // `typlen` always agreed. The row itself never changed a character.
    types: &[],
    answers: &[
        (
            "SELECT attname, atttypmod, format_type(atttypid, atttypmod) FROM \
             pg_attribute WHERE attrelid = 'iv'::regclass AND attnum > 0 ORDER BY attnum",
            TYPMOD,
            "pg19_interval.txt:61",
        ),
        (
            "SELECT format_type(1186, 3)",
            TYPMOD,
            "pg19_format_type.txt:62",
        ),
        (
            "SELECT format_type(1186, 32767)",
            TYPMOD,
            "pg19_interval.txt:63",
        ),
        (
            "INSERT INTO iv VALUES (1, '1 year 2 mons 3 days 04:05:06', '1.5 seconds', \
             '1 day', '1 2', '1-2')",
            TYPMOD,
            "pg19_interval.txt:64",
        ),
        (
            "INSERT INTO iv VALUES (2, '1 mon', '00:00:00', '2 days', '0 0', '0-0')",
            TYPMOD,
            "pg19_interval.txt:65",
        ),
        (
            "INSERT INTO iv VALUES (3, '30 days', '00:00:00', '3 days', '0 0', '0-0')",
            TYPMOD,
            "pg19_interval.txt:66",
        ),
        (
            "INSERT INTO iv VALUES (4, '-1 day', '00:00:00', '-1 days', '0 0', '0-0')",
            TYPMOD,
            "pg19_interval.txt:67",
        ),
        (
            "SELECT id, a, b, c, d, e FROM iv ORDER BY id",
            TYPMOD,
            "pg19_interval.txt:69",
        ),
        (
            "SELECT id, a FROM iv ORDER BY a",
            TYPMOD,
            "pg19_interval.txt:70",
        ),
        (
            "SELECT id, a FROM iv ORDER BY a DESC",
            TYPMOD,
            "pg19_interval.txt:71",
        ),
        (
            "SELECT id FROM iv WHERE a = '1 mon' ORDER BY id",
            TYPMOD,
            "pg19_interval.txt:73",
        ),
        (
            "SELECT id FROM iv WHERE a > '1 day' ORDER BY id",
            TYPMOD,
            "pg19_interval.txt:74",
        ),
        (
            "SELECT count(*), count(a), min(a), max(a) FROM iv",
            TYPMOD,
            "pg19_interval.txt:75",
        ),
        ("SELECT sum(a) FROM iv", TYPMOD, "pg19_interval.txt:76"),
        ("SELECT avg(a) FROM iv", TYPMOD, "pg19_interval.txt:77"),
        (
            "SELECT INTERVAL '1 day', INTERVAL '1' DAY, INTERVAL '1 2' DAY TO HOUR",
            TYPMOD,
            "pg19_interval.txt:82",
        ),
        (
            "SELECT '1 year 1 month'::interval::interval year",
            TYPMOD,
            "pg19_interval.txt:98",
        ),
        (
            "SELECT justify_days('35 days'::interval), justify_hours('27 \
             hours'::interval), justify_interval('1 mon 33 days 27 hours'::interval)",
            FUNCTIONS,
            "pg19_interval.txt:112",
        ),
        (
            "SELECT age('2021-03-01'::timestamp, '2021-01-01'::timestamp)",
            FUNCTIONS,
            "pg19_interval.txt:116",
        ),
        (
            "SELECT extract(day FROM '1 year 2 mons 3 days'::interval), extract(epoch \
             FROM '1 day'::interval)",
            FUNCTIONS,
            "pg19_interval.txt:117",
        ),
        // `::char(n)` truncating on a cast and raising only on an assignment was declared
        // here and is closed: `debts-v1.1.md` #36 gave the cast and the row write their own
        // sides of one seam (`tests/typmod_seam.rs`). It had nothing to do with this type,
        // which is what every copy of it said — five entries across four files, one cause.
        // **`'1 day'::interval::time` was the last entry here and it is closed**, which took the
        // `ARITHMETIC` reason with it — no entry uses it any more, so the constant is gone too.
        // What that reason said is worth keeping: `plan::BinaryOp` is Eq/NotEq/Lt/LtEq/Gt/GtEq/
        // And/Or and every one of its uses assumes a comparison producing a boolean, so `+`, `-`,
        // `*` and `/` over any pair were `0A000` naming the operator — and this is the type those
        // operators mostly *answer with*, which is why the whole family sat here rather than in
        // `tests/time.rs`. The arithmetic landed one entry at a time; this last one was never
        // arithmetic at all. `interval -> time` keeps the clock part, drops the calendar and wraps
        // modulo a day, and it went through the text — `time`'s input function reading `1 day` —
        // until `value::convert_without_text` was given the pair (`debts-v1.1.md` #43). A missing
        // *conversion* had been sitting in an arithmetic entry because an `interval` was on one
        // side of it.
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
