//! `numeric`, against PostgreSQL 19beta1 — tier 2's hard half, statements 389/390/474.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // One, and **the rows agree** — this is the declared type only. Two things used to be behind
    // it and both are gone.
    //
    // **The corpus format was the first, and it was not this node.** A types field was split on
    // every comma and `numeric(10,2)` contains one, so PostgreSQL's three types arrived as five
    // and a row declaring them could not agree whatever this node answered — six statements sat
    // here for that reason. `parity_harness`'s `declared_types` now splits at parenthesis depth
    // zero, which is decidable without any escape, and `SELECT id, n, d FROM nm ORDER BY id`
    // agreed the moment it did (`docs/plans/debts-v1.1.md` #20).
    //
    // **The second is gone too, and it was smaller than its row said.** A cast's result used to
    // carry no typmod — `1.0::numeric(10,3)` described itself as bare `numeric` — because a
    // modifier travelled only with a plain column reference. Register row #28 sized that as a
    // type-surface change; it was one rule in `exec::query::typmod_of` plus one clause in the
    // fold, which was already keeping a `Cast` node "when the value cannot speak for itself" and
    // needed to keep one when the value cannot speak for its *modifier* either. Six statements
    // left this list at once (`tests/corpus/pg19_expression_typmod.txt`).
    types: &[
        // **All four answers agree now**, and it took two units: `pg_typeof` answers them at all
        // (it was `0A000` naming itself), and the first of the four — a bare `1.5` — is a
        // `numeric` here as it is there. What is left is `pg_typeof`'s own `regtype`/`text` trade
        // (ADR 0077).
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname = 'numeric'",
    ],
    // Twenty-one of eighty, and **every one is a missing function or a refused feature** — not one is
    // a value where PostgreSQL answers something else. Everything the type *is* agrees: the scale
    // is kept and printed, rounding is half away from zero, `NaN` equals itself and sorts above
    // every number, the two `22003`s say which bound broke, a negative scale works, and the
    // typmod is the `((p << 16) | s) + 4` a real server encodes.
    answers: &[
        (
            "SELECT 1.5::numeric(-1,0)",
            "Both refuse and the code differs, because they refuse in different places: PostgreSQL \
             parses the negative precision and rejects it at the bounds check (22023), where \
             sqlparser's grammar takes an unsigned precision and stops at the minus (42601). A \
             negative *scale* parses on both — `numeric(10,-2)` is a real type and works here — \
             so this is the precision's grammar, not the bounds rule, which is implemented and \
             proved by `numeric(1001,0)` two lines above",
            "pg19_numeric.txt:97",
        ),
        (
            "SELECT (10::numeric ^ 100)::text",
            "Arithmetic. The line's point — that a `numeric` grows to a hundred digits without a width to overflow — is the storage half, and that is built: the corpus stores and prints exact values of any length.",
            "pg19_numeric.txt:87",
        ),
        (
            "SELECT round(1.245, 2), trunc(1.999, 2), ceil(1.1), floor(1.9)",
            "Four functions, each `0A000` naming itself. **The rounding rule they measure is implemented** — `round(1.245, 2)` is `1.25` and so is `1.245::numeric(10,2)`, which this corpus does prove: one rule for the cast, the assignment and the function.",
            "pg19_numeric.txt:113",
        ),
        (
            "SELECT round(2.5), round(3.5), round(-2.5)",
            "`round` is `0A000` naming itself. Half away from zero, which is the same rule `2.5::numeric::int` takes here and answers `3` for.",
            "pg19_numeric.txt:114",
        ),
        (
            "SELECT scale(1.500), scale(1.5::numeric(10,3))",
            "`scale` is `0A000` naming itself. The value it asks about is right — the corpus proves `1.500` keeps three digits and a `numeric(10,3)` forces three — there is no function to read it back with.",
            "pg19_numeric.txt:115",
        ),
        (
            "SELECT (2147483647::numeric + 1)::int4",
            "Arithmetic. This crate has **no arithmetic operators at all** for any type, so the `22003` PostgreSQL raises after adding is never reached. The cast itself is built: a `numeric` past `int4` **is** `22003 integer out of range` here, reached by a literal instead.",
            "pg19_numeric.txt:124",
        ),
        (
            "SELECT numeric_send(1.5::numeric)",
            "`numeric_send` is `0A000` naming itself, and so is the binary format underneath it — a `numeric`'s wire form is a four-`i16` header plus base-10000 digit groups that nothing here has ever sent or read. The capture records the bytes (`\\x000200000000000100011388`) so the day that path is built it has an oracle.",
            "pg19_numeric.txt:125",
        ),
        (
            "SELECT to_char(1.5::numeric, 'FM999.00'), to_char(1234.5::numeric, '9,999.99')",
            "`to_char` is `0A000` naming itself — a whole format-picture language, and nothing `ActiveRecord` sends.",
            "pg19_numeric.txt:126",
        ),
        (
            "SELECT 2::numeric ^ 10, 2::numeric ^ 0.5",
            "**`^` over two exact values has a scale rule of its own** — `2 ^ 10` is \
             `1024.0000000000000` and `10 ^ 100` is a bare integer, so the rule is about the \
             result's weight and not the operands'. That is `numeric_power` in `numeric.c`, a \
             different function from the `select_div_scale` this node implements for division, \
             and it needs a capture round of its own. Every other `numeric` operator answers now \
             (`tests/numeric_arithmetic.rs`), and `^` over the floats does too.",
            "pg19_numeric.txt:111",
        ),
    ],
};

#[test]
fn every_numeric_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_numeric.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The scale is part of the value: three spellings of one number, all kept and all equal.
///
/// This is the whole type in one test, and the property ADR 0031 has been refusing `avg(int8)`
/// over since unit 0. An implementation that normalised trailing zeros passes every comparison
/// here and fails the first assertion.
#[test]
fn three_spellings_of_one_number_are_kept_and_are_equal() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nm (id int8 PRIMARY KEY, n numeric)",
        "INSERT INTO nm VALUES (1, 1.0)",
        "INSERT INTO nm VALUES (2, 1.00)",
        "INSERT INTO nm VALUES (3, 1.000)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT n FROM nm ORDER BY id"),
        [["1.0"], ["1.00"], ["1.000"]],
        "each row keeps the scale it was written with"
    );
    // And all three are one number.
    assert_eq!(
        node.rows("SELECT id FROM nm WHERE n = 1.0 ORDER BY id"),
        [["1"], ["2"], ["3"]]
    );
}

/// **A unique index normalises where the row does not**, which is the same fact from the other
/// side: two spellings of one number are one key.
#[test]
fn a_unique_index_sees_one_number_where_the_row_keeps_three() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nm (id int8 PRIMARY KEY, n numeric)",
        "CREATE UNIQUE INDEX nm_n ON nm (n)",
        "INSERT INTO nm VALUES (1, 1.0)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("INSERT INTO nm VALUES (2, 1.000)").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        "23505",
        "1.0 and 1.000 are one value and must be one key"
    );
    // A different number is a different key, however it is spelled.
    node.run("INSERT INTO nm VALUES (2, 1.0000000001)").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM nm WHERE n = 1.0"),
        [["1"]],
        "and the index still finds it by value"
    );
}

/// Rounding is half **away from zero**, for the cast and for the column alike.
///
/// `1.245` is the discriminating case: half-to-even would give `1.24`. And the same value written
/// into a `numeric(10,2)` column rounds identically, which is what "one rule" means.
#[test]
fn rounding_is_half_away_from_zero_everywhere() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 1.235::numeric(10,2), 1.245::numeric(10,2), 1.255::numeric(10,2)"),
        [["1.24", "1.25", "1.26"]]
    );
    assert_eq!(
        node.rows("SELECT (-1.245)::numeric(10,2), 2.5::numeric::int, (-2.5)::numeric::int"),
        [["-1.25", "3", "-3"]]
    );
    for statement in [
        "CREATE TABLE nm (id int8 PRIMARY KEY, d numeric(10,2))",
        "INSERT INTO nm VALUES (1, 1.245)",
        "INSERT INTO nm VALUES (2, 1.0)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT d FROM nm ORDER BY id"),
        [["1.25"], ["1.00"]],
        "the column's declared scale rounds and pads by the same rule the cast uses"
    );
}

/// `NaN` is a value, it equals itself, and it is the **largest** one.
///
/// PostgreSQL's deliberate departure from IEEE 754, and `float8` makes the identical one: a total
/// order is what lets a `NaN` be indexed and sorted at all. Measured both ways rather than
/// reasoned about — the IEEE rule says a `NaN` is unequal to itself, and believing that here cost
/// this test a wrong expectation before the oracle corrected it.
#[test]
fn nan_equals_itself_and_sorts_above_everything() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nm (id int8 PRIMARY KEY, n numeric)",
        "INSERT INTO nm VALUES (1, 1.5)",
        "INSERT INTO nm VALUES (2, 'NaN')",
        "INSERT INTO nm VALUES (3, 'Infinity')",
        "INSERT INTO nm VALUES (4, '-Infinity')",
    ] {
        node.run(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    assert_eq!(
        node.rows("SELECT id FROM nm ORDER BY n"),
        [["4"], ["1"], ["3"], ["2"]],
        "-Infinity < finite < Infinity < NaN"
    );
    assert_eq!(node.rows("SELECT id FROM nm WHERE n = 'NaN'"), [["2"]]);
    // Not a contrast: the float agrees, because PostgreSQL gave both types the same total order.
    assert_eq!(
        node.rows("SELECT 'NaN'::numeric = 'NaN'::numeric, 'NaN'::float8 = 'NaN'::float8"),
        [["t", "t"]]
    );
    // And in both, a `NaN` is above the infinity.
    assert_eq!(
        node.rows(
            "SELECT 'NaN'::numeric > 'Infinity'::numeric, 'NaN'::float8 > 'Infinity'::float8"
        ),
        [["t", "t"]]
    );
}

/// The two `22003`s, and the one special that fits every typmod.
#[test]
fn an_infinity_fits_no_typmod_and_a_nan_fits_them_all() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 'NaN'::numeric(10,2)"), [["NaN"]]);
    for (sql, detail) in [
        (
            "SELECT 'Infinity'::numeric(10,2)",
            "A field with precision 10, scale 2 cannot hold an infinite value.",
        ),
        (
            "SELECT 12345678901::numeric(10,2)",
            "A field with precision 10, scale 2 must round to an absolute value less than 10^8.",
        ),
        (
            // Every digit is fractional, so nothing at all fits in front of the point — and
            // PostgreSQL writes that bound as `1` rather than as `10^0`.
            "SELECT 1.5::numeric(1000,1000)",
            "A field with precision 1000, scale 1000 must round to an absolute value less than 1.",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "22003", "for {sql}");
        assert_eq!(error.detail().as_deref(), Some(detail), "for {sql}");
    }
}

/// A **negative** scale is a real type, and it multiplies.
#[test]
fn a_negative_scale_multiplies() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 12345.6789::numeric(10,-2)"),
        [["12300"]],
        "stored as three digits at scale -2, and printed with the zeros it stands for"
    );
    assert_eq!(
        node.rows("SELECT format_type(1700, 655366), format_type(1700, 786434)"),
        [["numeric(10,2)", "numeric(11,-2)"]],
        "the typmod is ((p << 16) | s) + 4, and the scale in it is signed"
    );
}
