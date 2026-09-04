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
    // Two trades and both are standing. The catalog one — `typname` and `udt_name` are `name`,
    // the oids `oid`, `typcategory` a `"char"`, `typinput` a `regproc`, `pg_typeof` a `regtype` —
    // and the **typmod one**: a typmod travels only with a plain column reference here, so
    // `'101'::bit(3)` is declared `bit(1)` where a real server keeps the 3 through the cast. The
    // *values* agree in every one of these, which is what they are here to say: `bit` is 1560
    // with array 1561 and `varbit` 1562/1563, both category `V`, inputs `bit_in`/`varbit_in`; the
    // column reports `character_maximum_length` 8, 4, **1** for a bare `bit` and NULL for a bare
    // `bit varying`; the defaults read back `'00000011'::"bit"` and `'0011'::bit varying`; and a
    // cast pads on the right (`10100000`) and truncates (`1010`, `101`) exactly as measured.
    types: &[
        "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput FROM pg_type WHERE typname IN ('bit','varbit','_bit','_varbit') ORDER BY typname",
        "SELECT 'r', column_name, data_type, udt_name, character_maximum_length, column_default FROM information_schema.columns WHERE table_name = 'b' ORDER BY ordinal_position",
        "SELECT 'r', '101'::bit(3), '101'::bit varying(5)",
        "SELECT 'r', pg_typeof('101'::bit(3)), pg_typeof('101'::bit varying(5))",
        "SELECT 'r', '{101,010}'::bit(3)[], pg_typeof('{101}'::bit(3)[])",
        "SELECT 'r', '101'::bit(8)",
        "SELECT 'r', '101010101'::bit(4)",
        "SELECT 'r', '10101'::bit varying(3)",
    ],
    answers: &[
        // **The bit *literals* are their own small feature and the suite writes none of them.**
        // `ActiveRecord` sends `'00001010'` as an ordinary string; `B'101'` and `x'F'` are SQL's
        // own spellings and reach this node as a lexical form it has no value for. The hex one
        // is where the two meet: `x'F'` is `1111`, four bits per digit, which is also what
        // `bit_string_test.rb`'s `"0xF"` becomes — client-side, before the statement exists.
        ("SELECT 'r', B'101', B'0'", "a bit literal is its own unit"),
        (
            "SELECT 'r', x'F'::bit(4), x'1A'::bit(8)",
            "a hexadecimal bit literal is its own unit",
        ),
        // **`integer -> bit` is a cast and not a reading of the digits.** `5::int4::bit(8)` is
        // `00000101` — the number in binary — where this node reads the *text* `5` and finds a
        // character that is not a binary digit. Refused rather than answered, and named here.
        (
            "SELECT 'r', '101'::bit(3)::text, 5::int4::bit(8)",
            "integer -> bit is a conversion, not a reading of the printed digits",
        ),
        // The functions and operators over a bit string, which are one unit and none of it is in
        // the suite: `length`/`octet_length`, the bitwise `& | # ~`, and the shifts.
        (
            "SELECT 'r', length('10101'::bit(5)), octet_length('10101'::bit(5))",
            "the bit-string functions are their own unit",
        ),
        (
            "SELECT 'r', '101'::bit(3) & '110'::bit(3), '101'::bit(3) | '110'::bit(3)",
            "the bitwise operators are their own unit",
        ),
        (
            "SELECT 'r', '101'::bit(3) << 1, '101'::bit(3) >> 1",
            "the shift operators are their own unit",
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
