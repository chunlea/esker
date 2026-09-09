//! `bit(n)` and `bit varying(n)`, against PostgreSQL 19beta1.
//!
//! Run 71's `type "…" does not exist` row: 6 tests in `adapters/postgresql/bit_string_test.rb`,
//! which declares `bit(8)`, `bit varying(4)` and a bare one of each in a single `create_table`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // Two trades. The catalog one is down to the oids, answered as `bigint`, and `typinput`,
    // answered as `text` where a real server says `regproc`; `typname`/`udt_name` are `name` here
    // (ADR 0084), `typcategory` is a `"char"` (ADR 0095) and `pg_typeof` a `regtype` on both
    // (ADR 0093). Then the **typmod one**: a typmod travels only with a plain column reference here, so
    // `'101'::bit(3)` is declared `bit(1)` where a real server keeps the 3 through the cast. The
    // *values* agree in every one of these, which is what they are here to say: `bit` is 1560
    // with array 1561 and `varbit` 1562/1563, both category `V`, inputs `bit_in`/`varbit_in`; the
    // column reports `character_maximum_length` 8, 4, **1** for a bare `bit` and NULL for a bare
    // `bit varying`; the defaults read back `'00000011'::"bit"` and `'0011'::bit varying`; and a
    // cast pads on the right (`10100000`) and truncates (`1010`, `101`) exactly as measured.
    types: &[
        "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput FROM pg_type WHERE typname IN ('bit','varbit','_bit','_varbit') ORDER BY typname",
        "SELECT 'r', '101'::bit(3), '101'::bit varying(5)",
        "SELECT 'r', '{101,010}'::bit(3)[], pg_typeof('{101}'::bit(3)[])",
        "SELECT 'r', '101'::bit(8)",
        "SELECT 'r', '101010101'::bit(4)",
        "SELECT 'r', '10101'::bit varying(3)",
        // **The values agree now** — `x'F'` is `1111` and `x'1A'` is `00011010`, four bits a
        // digit — and what is left is the standing one three lines up: a cast's typmod does not
        // reach the declared type, so this says `"bit"` where a real server says `bit(4)`.
        "SELECT 'r', x'F'::bit(4), x'1A'::bit(8)",
        // The same, one spelling over: `'101'::bit` is `1` here and there — the bare keyword is
        // the grammar's `bit(1)` and truncates — and it is only the *declared* `bit(1)` that this
        // node reports as `"bit"`, because a `Literal::Typed` carries a `Datum` and not a typmod.
        "SELECT 'r', '101'::bit, '101'::bit varying",
        // `pg_typeof` is a `regtype` there and `text` here, the standing catalog trade; the
        // values are `bit` in both, which is what these two ask.
        // `column_name`, `data_type` and `column_default` are `information_schema`'s own domains
        // and `text` here. **Every value agrees** — including `'00000011'::"bit"` and
        // `'0011'::"bit"`, which is the quoted spelling a real server prints for a `B'…'` default
        // even on a `bit varying` column, and `bit(1)` for the bare `another_bit`.
    ],
    answers: &[
        // **`integer -> bit` is a cast and not a reading of the digits.** `5::int4::bit(8)` is
        // `00000101` — the number in binary — where this node reads the *text* `5` and finds a
        // character that is not a binary digit. Refused rather than answered, and named here.
        (
            "SELECT 'r', '101'::bit(3)::text, 5::int4::bit(8)",
            "integer -> bit is a conversion, not a reading of the printed digits",
            "UNMEASURED",
        ),
        // The functions and operators over a bit string, which are one unit and none of it is in
        // the suite: `length`/`octet_length`, the bitwise `& | # ~`, and the shifts.
        (
            "SELECT 'r', length('10101'::bit(5)), octet_length('10101'::bit(5))",
            "the bit-string functions are their own unit",
            "UNMEASURED",
        ),
        // The same unit, reached through the literal: the `B'1010'` and the `B''` beside it are
        // right and the `length` is what refuses, which is why the whole statement is here.
        (
            "SELECT 'r', B'1010', length(B'1010'), B''",
            "the bit-string functions are their own unit",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', '101'::bit(3) & '110'::bit(3), '101'::bit(3) | '110'::bit(3)",
            "the bitwise operators are their own unit",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', '101'::bit(3) << 1, '101'::bit(3) >> 1",
            "the shift operators are their own unit",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_bit_string_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_bit_string.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
