//! `money` as a column type and a value, against PostgreSQL 19beta1.
//!
//! Run 59's tier-3 row: 12 tests in `adapters/postgresql/money_test.rb`, which declares two money
//! columns, defaults one of them, and reads the default back out of the schema dumper.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade, six times: `typname` and `udt_name` are `name` on a real
    // server, `typcategory` and `typinput` are `"char"` and `regproc`, `information_schema`'s
    // three are its own domains, and `pg_typeof` answers a `regtype`. All `text` here, all
    // comparing identically. **Every value agrees**, and the values are the whole point of these
    // six: `money` is `typlen` 8 with category `N` and input `cash_in`, its precision and scale
    // are both NULL, its default reads back `'$150.55'::money`, `money / money` really is a
    // `double precision`, `money::numeric` is `567.89` with no symbol, and `money[]` exists.
    types: &[
        "SELECT 'r', typname, typlen, typcategory, typinput FROM pg_type WHERE typname IN \
         ('money','_money') ORDER BY typname",
        "SELECT 'r', column_name, data_type, udt_name, numeric_precision, numeric_scale, \
         column_default FROM information_schema.columns WHERE table_name = 'm' ORDER BY \
         ordinal_position",
        "SELECT 'r', '150.55'::money::text, pg_typeof('150.55'::money)",
        "SELECT 'r', '6.00'::money / '2.00'::money, pg_typeof('6.00'::money / '2.00'::money)",
        "SELECT 'r', '567.89'::money::numeric, pg_typeof('567.89'::money::numeric)",
        "SELECT 'r', '{1.00,2.00}'::money[], pg_typeof('{1.00,2.00}'::money[])",
    ],
    answers: &[
        // **`||` is not built here for any type**, which is the standing gap
        // `tests/integer_plus_text.rs` already declares three times over — not a money question.
        // It is in the corpus because it is the one place a money is *not* an integer: `money ||
        // text` is `text` on a real server and goes through the output function, symbol and all.
        (
            "SELECT 'r', '1.00'::money || 'x'",
            "|| is not built for any type here",
        ),
        // **Three refusals that agree except for the type named in them**, and all three name a
        // standing trade rather than anything about `money`: an integer literal is an `int8` here
        // and an `integer` there (the constant-width trade ADR 0033 records), and a decimal
        // literal beside a non-numeric operand resolves to `double precision` here where a real
        // server keeps it `numeric`. The `42883`, its DETAIL and its HINT agree in every case, and
        // so does the fact that the operator does not exist.
        (
            "SELECT 'r', '1.00'::money + 1",
            "an integer literal is a bigint here, so the refusal names bigint",
        ),
        (
            "SELECT 'r', '1.00'::money = 1.00",
            "a decimal literal is a double precision here, so the refusal names it",
        ),
        (
            "SELECT 'r', 6 / '2.00'::money",
            "an integer literal is a bigint here, so the refusal names bigint",
        ),
    ],
};

#[test]
fn every_money_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_money.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
