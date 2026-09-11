//! **`varbit` is the other spelling of `bit varying`**, and it reached nothing.
//!
//! Group 6 of r1's wire-108 baseline: six probes, every one of them
//! `0A000 the type VARBIT is not supported`. The type was here all along — `ColumnType::VarBit`,
//! oid 1562, `_varbit` 1563 beside it, a column of one already declarable as `bit varying(4)` —
//! and `sqlparser` files the two spellings as two `DataType` variants, `BitVarying` for the two
//! words and `VarBit` for the one. `parse::lower::lower_type` read the first and not the second.
//!
//! One missing variant, and everything behind it: the cast, the array, `array_agg`, `unnest`, a
//! subscript, a column declaration, an index over one. The tell that it was a *spelling* and not
//! a type is that `format_type('varbit'::regtype, 5)` answered `bit varying(5)` the whole time —
//! the catalog knew the name, and only the parser's door was shut.
//!
//! **What the corpus then found is the larger half of this file.** A statement that used to be
//! refused by name cannot be wrong, and six of them were: `'101'::bit(3)::int` read the digits as
//! a decimal number and answered `101` where a real server says `5`, `substring` over a bit string
//! answered NULL, and `'101'::varbit::int` answered `101` for a pair that has no cast at all. The
//! rules are now `pg_cast`'s own eight rows and two's complement, both measured.
//!
//! Measured in `tests/captures/pg19_varbit.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
///
/// **Two families, and neither is about the spelling.** The first is the bit-string *operators and
/// functions* — `||`, the four bitwise ones, the shifts and `position`; `length`, `octet_length`
/// and `bit_length` have since been built — which this crate has built for no element type and
/// which `tests/bit_string.rs`
/// has named as "their own unit" since the bit types arrived. They are here because the spelling
/// made them reachable, not because they are new; every one is a refusal where a real server
/// answers, which is the honest shape of a gap. The citations are this file's capture rather than
/// `UNMEASURED`, which is the half this unit does close.
///
/// The second is the **assignment length rule**, which is the cast rule's other half:
/// `value::fit_to_typmod` serves both the cast and the row write, and what it holds is the *cast's*
/// answer — pad on the right, truncate in silence — because a refusal where a real server pads
/// would be the worse of the two. `value::bit::fit_to_column` is the assignment's answer, written
/// and not yet wired to a caller; wiring it is a seam question (which of the two asks) rather than
/// a bit-string question, and the two string types already have that seam in
/// `value::truncate_to_typmod`.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **The three `length`/`octet_length` rows are gone**, closed by the scalar-overload
        // table: `length(varbit)` counts **bits** and `octet_length(varbit)` counts bytes — two of
        // the eight `pg_proc` rows over four counting names, and they do not agree with each other
        // (`tests/captures/pg19_length_overloads.txt`). The empty bit string was kept as the value
        // that would let a wrong implementation look right, and it is now asserted rather than
        // declared. **`bit_length` is gone too**, closed the same way: its three `pg_proc` rows
        // are `(bit)`, `(bytea)` and `(text)`, and over a bit string it is `length`'s answer while
        // over a string it is eight times the octet count — which is the half that makes it a
        // function rather than an alias.
        // **The four `||` entries that stood here are gone**, closed by wire v3 family F3b's
        // second unit: `bit`, `bytea` and `tsquery` each have a same-type `||` on a real server
        // and this crate now builds the value. The note they carried is worth keeping — `||` is
        // the one bit-string operator that keeps `bit varying`, where the four bitwise ones and
        // the shifts all answer a plain `bit` — and `varbit || bit(2)` is what says the answer is
        // `bit varying` rather than the wider of the two.
        (
            "SELECT '101'::varbit & '110'::varbit",
            "**The four bitwise operators over bit strings are not built.** They are built over the integers (`tests/bitwise.rs`) and share the spelling, which is what makes this a dispatch this crate does not have rather than an operator it has not heard of. Two measured rules ride on it: the result is a plain `bit` even over two `bit varying` operands, and operands of different lengths are `22026 cannot AND bit strings of different sizes` rather than a padded answer.",
            "pg19_varbit.txt:94",
        ),
        (
            "SELECT '101'::varbit | '110'::varbit",
            "The same gap, the OR of the family.",
            "pg19_varbit.txt:95",
        ),
        (
            "SELECT '101'::varbit # '110'::varbit",
            "The same gap, the XOR — and `#` is a spelling this crate has nowhere else, so it is not even parsed as an operator over anything.",
            "pg19_varbit.txt:96",
        ),
        (
            "SELECT ~ '101'::varbit",
            "**Unary bitwise NOT needs its own expression variant** and has none — the same declared gap `tests/bitwise.rs` records for the integers, reached one type over, which is why its refusal is `0A000` about the *expression* rather than `42883` about an operator.",
            "pg19_varbit.txt:97",
        ),
        (
            "SELECT '101'::varbit << 1",
            "**A bit string's shift is not the integers'.** It keeps the width and drops the bits that fall off — `'101'::varbit << 1` is `010`, three bits — where an integer's shift wraps modulo the type's width. Same spelling, different rule, and this crate has only the integer one.",
            "pg19_varbit.txt:98",
        ),
        (
            "SELECT position('11'::varbit in '10110'::varbit)",
            "`position(x in y)` is not built for any type: the refusal names the whole expression. Over a bit string it counts from 1 and answers 0 for no match, which is the string rule applied to bits.",
            "pg19_varbit.txt:100",
        ),
        // **The three assignment rows are gone**, closed by `debts-v1.1.md` #36 with the unit
        // this file opened. They said one function held the *cast's* answer for both callers, and
        // that turned out to be half the story: the string types were wired the opposite way
        // round, so `'abcdef'::varchar(3)` was a `22001` where a real server truncates. One seam,
        // four types, and each had been wired to whichever caller it was written for
        // (`tests/typmod_seam.rs`).
    ],
};

#[test]
fn every_varbit_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_varbit.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 90,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The six probes**, described the way the wire sweep reads them.
#[test]
fn the_one_word_spelling_reaches_the_same_type_as_the_two() {
    let mut node = parity::Node::new(&[]);
    for (statement, oid) in [
        ("SELECT '101'::varbit", 1562),
        ("SELECT '101'::bit varying", 1562),
        ("SELECT '101'::varbit(5)", 1562),
        ("SELECT '{101,11}'::varbit[]", 1563),
        ("SELECT ARRAY['101'::varbit]", 1563),
        (
            "SELECT array_agg(c) FROM (SELECT '101'::varbit AS c) s",
            1563,
        ),
        ("SELECT unnest('{101,11}'::varbit[])", 1562),
    ] {
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        assert_eq!(fields[0].type_oid, oid, "{statement}");
    }
    // **And a column of one, which is the shape a suite writes.** `varbit` and `bit varying` in
    // one `CREATE TABLE`, because a node that took only one of them would still pass every line
    // above.
    let mut node = parity::Node::new(&[
        "CREATE TABLE vb (b varbit, b5 varbit(5), bv bit varying(4), a varbit[])",
    ]);
    assert_eq!(
        node.rows(
            "SELECT attname, atttypid, atttypmod FROM pg_attribute \
             WHERE attrelid = 'vb'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        vec![
            vec!["b", "1562", "-1"],
            vec!["b5", "1562", "5"],
            vec!["bv", "1562", "4"],
            vec!["a", "1563", "-1"],
        ]
    );
}

/// **A cast truncates and an assignment refuses**, which is one rule with two callers.
///
/// This test asserted the second half was missing, and it was the reason `debts-v1.1.md` #36 was
/// written: one function held the cast's answer for both callers, so five bits went into a
/// `bit varying(3)` column and came back three. Closing it found the string types wired the
/// opposite way round — the whole seam is `tests/typmod_seam.rs` now, and this keeps the bit half
/// beside the type it belongs to.
#[test]
fn a_cast_truncates_where_an_assignment_refuses() {
    let mut node = parity::Node::new(&["CREATE TABLE vb (b varbit(3))"]);
    assert_eq!(node.rows("SELECT '10101'::varbit(3)"), vec![vec!["101"]]);
    let error = node.run("INSERT INTO vb VALUES ('10101')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::STRING_DATA_RIGHT_TRUNCATION);
    assert_eq!(
        error.to_string(),
        "bit string too long for type bit varying(3)"
    );
    assert_eq!(node.rows("SELECT count(*) FROM vb"), vec![vec!["0"]]);
}

/// **The input function reads a base prefix**, which is not the literal syntax beside it.
///
/// `X'ff'` is a *token* the parser reads; `'xff'::varbit` is a *string* an input function reads.
/// The two doors look alike, and the tell that this one was shut is which character the refusal
/// blames: a real server says `"y"` for `'xyz'::varbit`, because after the `x` the rest is hex.
#[test]
fn the_input_function_reads_b_and_x_prefixes() {
    let mut node = parity::Node::new(&[]);
    for (statement, answer) in [
        ("SELECT 'xff'::varbit", "11111111"),
        ("SELECT 'x1A'::varbit", "00011010"),
        ("SELECT 'X0f'::varbit", "00001111"),
        ("SELECT 'b101'::varbit", "101"),
        // A prefix on its own is the empty bit string, not an error.
        ("SELECT 'x'::varbit", ""),
        ("SELECT 'b'::varbit", ""),
        // The literal door still answers what it always did.
        ("SELECT X'F'::bit(4)", "1111"),
        ("SELECT B'1010'::bit(2)", "10"),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
    }
    let error = node.run("SELECT 'xyz'::varbit").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "\"y\" is not a valid hexadecimal digit",
        "after an `x` the rest is hex, so the refusal names the second character"
    );
}

/// **A bit string and an integer convert; they do not round-trip through the digits.**
///
/// Two's complement, and the two directions are not each other's mirror: writing an integer into
/// `n` bits **sign-extends** when `n` is wider, and reading `n` bits back **zero-extends** into
/// the target. `5::int4::bit(40)` is zero-padded and `(-1)::int4::bit(40)` is forty ones, while
/// `'1'*40::bit(40)::int8` is a positive number because forty bits leave an `int8`'s sign bit
/// clear. Measured, all four.
#[test]
fn a_bit_string_and_an_integer_convert_in_twos_complement() {
    let mut node = parity::Node::new(&[]);
    for (statement, answer) in [
        ("SELECT 5::int4::bit(4)", "0101"),
        // Shorter than the source keeps the low bits: 300 modulo 256.
        ("SELECT 300::int4::bit(8)", "00101100"),
        ("SELECT 5::int4::bit(2)", "01"),
        // A bare `bit` is `bit(1)`, everywhere a type is written.
        ("SELECT 5::int4::bit", "1"),
        ("SELECT (-5)::int4::bit(8)", "11111011"),
        ("SELECT (-1)::int4::bit(32)", &"1".repeat(32)),
        // Wider than the source sign-extends, which is where zero-padding would be wrong.
        ("SELECT (-1)::int4::bit(40)", &"1".repeat(40)),
        (
            "SELECT 5::int4::bit(40)",
            "0000000000000000000000000000000000000101",
        ),
        ("SELECT '101'::bit(3)::int4", "5"),
        ("SELECT '1111'::bit(4)::int4", "15"),
        ("SELECT '101'::bit(3)::bigint", "5"),
        // The bits fill the target's whole width, sign bit included.
        (
            "SELECT '11111111111111111111111111111111'::bit(32)::int4",
            "-1",
        ),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
    }
}

/// **The pairs that do not exist**, which is the other half of the same census.
///
/// `pg_cast` has eight rows touching a bit string and they are the whole of it: `bit` converts
/// with `int4` and `int8`, a `bit varying` with no number at all, and no bit string with `int2`,
/// `numeric` or `boolean`. Every one of these used to answer — the fold read the digits as a
/// decimal number, so `'101'::bit(3)::int2` was `101` — which is a wrong answer rather than a
/// missing feature, and it was invisible because the *right* answer for the pair next door
/// (`::int4`) was wrong in the same way.
#[test]
fn the_pairs_pg_cast_does_not_hold_are_refused() {
    let mut node = parity::Node::new(&[]);
    for (statement, message) in [
        ("SELECT 5::int2::bit(4)", "cannot cast type smallint to bit"),
        (
            "SELECT 5::numeric::bit(4)",
            "cannot cast type numeric to bit",
        ),
        (
            "SELECT 5::int4::varbit",
            "cannot cast type integer to bit varying",
        ),
        (
            "SELECT '101'::bit(3)::int2",
            "cannot cast type bit to smallint",
        ),
        (
            "SELECT '101'::bit(3)::numeric",
            "cannot cast type bit to numeric",
        ),
        (
            "SELECT '101'::bit(3)::bool",
            "cannot cast type bit to boolean",
        ),
        (
            "SELECT '101'::varbit::bigint",
            "cannot cast type bit varying to bigint",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::CANNOT_COERCE, "{statement}");
        assert_eq!(error.to_string(), message, "{statement}");
    }
    // **The permission is one table read by one function**, and this is what says so: the same
    // pair through an expression rather than a folded literal answers the same way. It did not —
    // `exec::query` asked `pg_cast` and the fold in `parse::lower` did not, which is two readers
    // of one fact and the shape three earlier defects had.
    let mut node = parity::Node::new(&["CREATE TABLE vb (b bit(3))"]);
    node.run("INSERT INTO vb VALUES ('101')").unwrap();
    assert_eq!(node.rows("SELECT b::int4 FROM vb"), vec![vec!["5"]]);
    let error = node.run("SELECT b::int2 FROM vb").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::CANNOT_COERCE);
}

/// **`min` and `max` do not exist over a bit string**, and this node agrees.
///
/// Worth its own test because being *more* permissive is the easy mistake here — the wire baseline
/// has four probes of exactly that shape over `tsvector` and `tsquery` — and because the message
/// is the one a real server gives, naming the printed type rather than the spelling written.
#[test]
fn a_bit_string_has_no_extreme() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "SELECT min(c) FROM (VALUES ('101'::varbit)) s(c)",
        "SELECT max(c) FROM (VALUES ('101'::varbit)) s(c)",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::UNDEFINED_FUNCTION,
            "{statement}"
        );
        assert!(
            error.to_string().contains("(bit varying)"),
            "the refusal names the printed type, not the spelling: {error}"
        );
    }
    // `count` is not an extreme and answers over anything.
    assert_eq!(
        node.rows("SELECT count(c) FROM (SELECT '101'::varbit AS c) s"),
        vec![vec!["1"]]
    );
}

/// **`substring` over a bit string is a bit string**, not NULL and not text.
#[test]
fn substring_over_a_bit_string_answers_one() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT substring('10110'::varbit from 2 for 3)"),
        vec![vec!["011"]]
    );
    assert_eq!(
        node.rows("SELECT substring('10110'::varbit from 2)"),
        vec![vec!["0110"]]
    );
    // A `from` below one is not an offset: the count is measured from it and the take starts at 1.
    assert_eq!(
        node.rows("SELECT substring('10110'::varbit from 0 for 3)"),
        vec![vec!["10"]]
    );
    // **A plain `bit`, whichever of the two it was given** — measured, both.
    let outcome = node
        .run("SELECT substring('10110'::varbit from 2 for 3)")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 1560);
    // And a text argument still answers text, which is what says this is a dispatch and not a
    // replacement.
    assert_eq!(
        node.rows("SELECT substring('abcde' from 2 for 3)"),
        vec![vec!["bcd"]]
    );
}
