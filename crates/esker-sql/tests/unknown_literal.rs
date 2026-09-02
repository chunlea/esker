//! Contract C3 for PostgreSQL's `unknown`: what types a quoted string that no column types.
//!
//! `SELECT 1 = '1'` answered `f` here and `t` on a real server. It is a **wrong answer and not a
//! refusal** — the outcome this crate is built to avoid — and it was found by the `IN` corpus
//! rather than by a failure, because `IN` shares `reconcile` with `=` and inherited it exactly.
//! `docs/plans/phase-9-rails.md` §6 recorded it and said it wanted a unit of its own, which this
//! is.
//!
//! The corpus is 123 statements put to a real PostgreSQL 19beta1 in one session and replayed the
//! same way against one node. Its header holds the rule; the three cases are worth repeating
//! because only the first is obvious:
//!
//! 1. one side `unknown` and the other typed — the `unknown` is read by that type's own input
//!    function, so `1 = '01'` is `t` and `1 = 'x'` is `22P02` rather than `false`;
//! 2. **both** sides `unknown` — both are `text`, so `'1' = '01'` is `f` where `1 = '01'` is `t`;
//! 3. an explicit type is not `unknown` — `1 = '1'::text` is `42883` where `1 = '1'` is `t`.
//!
//! And the trap the capture found, which is `IN`: **the list is typed as a whole**, so
//! `'01' IN ('1', 1)` is `t` — the `1` gives every `unknown` in the expression its type, the
//! operand included. A left-to-right pairwise reconcile answers `f`, which is a wrong answer and
//! not a refusal.
//!
//! **Before this unit 78 of the 123 disagreed; after it, 31 do, and every one is declared below.**

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same table the corpus builds, for the assertions a corpus cannot carry.
const FIXTURE: &[&str] = &[
    "CREATE TABLE unk (id int8 PRIMARY KEY, n text, d float8, b bool)",
    "INSERT INTO unk VALUES (1, 'one', 1.5, true)",
];

/// What this node answers differently, and why. **Not one of them is about `unknown`** — the
/// resolution rule itself agrees with a real server on all 123 statements. They are four gaps that
/// were already there, and the corpus is the first thing to put a number on each.
///
/// * **`int4`** — 11 of them. A bare integer constant is `integer` on a real server and `int8`
///   here, so an out-of-`int4` string raises `22003` there and compares here, and every input
///   error names `bigint` where PostgreSQL names `integer`. It closes when `int4` exists, which is
///   the type-surface unit `docs/plans/phase-9-rails.md` §2 blocks on `esker-keys`.
/// * **`numeric`** — 5. A decimal constant is `numeric` there and `double precision` here.
/// * **a cast** — 14. `::` is `0A000` naming itself, and was before this unit.
/// * **the `C` collation** — 1, and `docs/plans/phase-6a.md` §6 already declares it.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // --- `int4`: the type a bare integer constant is ------------------------------------
        (
            "SELECT 1 = '2147483648'",
            "**the `int4` divergence, in the direction that answers rather than raises.** \
             `pg_typeof(1)` is `integer` on a real server, so the `unknown` is read by `int4in` \
             and 2147483648 is `22003 value \"2147483648\" is out of range for type integer`. \
             This node's bare constant is `int8`, so the string reads and the comparison answers \
             `f`. Nothing here is wrong about `unknown` — it is the width of the constant it \
             resolves against, and it closes when `int4` exists.",
        ),
        (
            "SELECT 1 = '9223372036854775808'",
            "the same, one width further out: both raise `22003`, and PostgreSQL names `integer` \
             where this node names `bigint`.",
        ),
        (
            "SELECT 1 = 'x'",
            "the same `22P02` for the same input, naming `bigint` where a real server names \
             `integer` — the constant on the left is `int4` there and `int8` here. Five \
             statements below differ in exactly this word and nothing else.",
        ),
        (
            "SELECT 1 = ''",
            "the `int4` naming, as above. The empty string is not a zero on either server.",
        ),
        (
            "SELECT 'x' = 1",
            "the `int4` naming, with the sides swapped.",
        ),
        (
            "SELECT 1 = '1.5'",
            "the `int4` naming. `int4in` and `int8in` both refuse a fractional part; only the \
             word differs.",
        ),
        (
            "SELECT 1 = ' '",
            "the `int4` naming. Whitespace is trimmed by both input functions and what is left \
             is empty.",
        ),
        (
            "SELECT 1 = '1e0'",
            "the `int4` naming. Neither integer input function takes an exponent — that is \
             `numeric`'s syntax, and `1.5 = '1.5e0'` above shows it accepted where the type has \
             it.",
        ),
        (
            "SELECT 1 IN ('x')",
            "the `int4` naming, reached through the list rather than through `=`. That it is the \
             same message is the point: `IN` types its list by the same rule and inherits the \
             same divergence, exactly as it inherited the bug.",
        ),
        (
            "SELECT 'a' IN ('a', 1)",
            "the `int4` naming, in the case where the **operand** is the `unknown` the list \
             types. The rule under test is working — `'a'` is being read as an integer because \
             the `1` is in the list — and only the width of that integer differs.",
        ),
        (
            "SELECT 1 IN (NULL, 'x')",
            "the `int4` naming. What this line is really pinning is that the coercion runs at \
             **plan time**: both servers raise rather than letting the NULL decide the answer.",
        ),
        // --- `numeric`: the type a decimal constant is ----------------------------------------
        (
            "SELECT 0.1 = '0.1000000000000000000001'",
            "**the `numeric` divergence, and this is where `double precision` stops reproducing \
             it.** A real server reads the string as `numeric` and keeps all 22 digits, so the \
             two differ and it answers `f`; this node reads it as `double precision`, which has \
             about 17, so both are 0.1 and it answers `t`. Every decimal in the block above \
             agrees, because `double` reproduces `numeric` for every value it can hold — ADR \
             0031's rule, and its backlog.",
        ),
        (
            "SELECT 1.5 = 'x'",
            "the same divergence in a message: `numeric` there, `double precision` here.",
        ),
        (
            "SELECT 1 = 1.0",
            "**neither side is `unknown`, so this is not the rule under test**: it is an \
             `integer` beside a `numeric`, which a real server promotes to `numeric` and answers \
             `t`. This node has no promotion between `int8` and `float8` and answers `f`. \
             Promoting to `double` would fix this line and break the one below it in the corpus \
             — `9007199254740993 = 9007199254740992.0`, which is `f` on both servers today and \
             would become `t` — so it is recorded rather than fixed, with its counterexample \
             beside it. Same `numeric` question, same backlog.",
        ),
        (
            "SELECT id FROM unk WHERE id = 1.0",
            "the same promotion, against a column: `0A000` naming the assignment. A refusal \
             rather than a wrong answer, and it was here before this unit.",
        ),
        (
            "SELECT id FROM unk WHERE id = 1.5",
            "the same `0A000`. A real server answers no rows, having compared 1 against 1.5 as \
             `numeric`.",
        ),
        // --- a cast of a *number*, which now runs ----------------------------------------------
        //
        // Twelve entries stood here and have gone. A cast of a **string** literal runs since the
        // json unit, which needed `'{"a":1}'::jsonb`; a cast of a **numeric** literal runs since
        // the numeric unit, which needed `1.5::numeric` and taught `cast_literal_text` to read a
        // number and a leading sign. Both times the harness failed until the entries were
        // deleted, which is the point of listing a divergence rather than describing one.
        //
        // What is left is the pair below, and it is not about casting a number at all.
        (
            "SELECT 1 = '1'::text",
            "**a cast of a number is `0A000` naming itself.** These lines are the corpus's \
             evidence for case 3 — that an explicit type is *not* `unknown`, so `1 = '1'::text` \
             is `42883` where `1 = '1'` is `t`. This node cannot express the distinction on the \
             numeric side and so cannot get it wrong there: the quoted string is `unknown`.",
        ),
        ("SELECT '1'::text = 1", "a cast, as above."),
        // --- the collation, already declared ---------------------------------------------------
        (
            "SELECT 'B' < 'a'",
            "**the `C` collation**, which `docs/plans/phase-6a.md` §6 declares: `text` sorts by \
             bytes here, because a locale-aware collation needs ICU or a platform C library and \
             this project compiles neither. The capture container is `en_US.utf8`, where `B` \
             sorts after `a`. It arrives here because case 2 makes two `unknown`s `text` — which \
             is the rule working, and then the comparison is the one already written down.",
        ),
    ],
};

#[test]
fn every_unknown_literal_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_unknown.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 120,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The bug the plan recorded, on its own, in both the shapes it was reported in.
///
/// It is worth a test outside the corpus because it is the statement `docs/plans/phase-9-rails.md`
/// §6 and `docs/bench/rails-scoreboard.md` both name: a reader following either to here should
/// find the assertion rather than have to search 123 lines for it.
#[test]
fn an_untyped_literal_takes_the_other_operand_s_type() {
    let mut node = parity::Node::new(FIXTURE);

    // The statement §6 recorded. It was `f`.
    assert_eq!(node.rows("SELECT 1 = '1'"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT '1' = 1"), vec![vec!["t"]]);
    // And the one `tests/in_list.rs` inherited from it.
    assert_eq!(node.rows("SELECT 1 IN ('1')"), vec![vec!["t"]]);

    // Read by the type's *input function*, not by a string match: `'01'` and `' 1'` are the same
    // integer and a different string.
    assert_eq!(node.rows("SELECT 1 = '01'"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 = ' 1'"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT true = 'yes'"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1.5 = '1.50'"), vec![vec!["t"]]);

    // A string that will not read is that input function's error and never a `false`. The one
    // outcome a `WHERE` cannot tell from "no match".
    let error = node.run("SELECT 1 = 'x'").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        esker_sql::sqlstate::INVALID_TEXT_REPRESENTATION
    );
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type bigint: \"x\""
    );

    // Case 2, which is what says the rule is `unknown` resolution and not "compare as text":
    // with nothing to type them against, two strings are two strings.
    assert_eq!(node.rows("SELECT '1' = '1'"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT '1' = '01'"), vec![vec!["f"]]);
    assert_eq!(node.rows("SELECT '1' = ' 1'"), vec![vec!["f"]]);
}

/// The trap: an `IN` list is typed as a **whole**, so the operand is typed by the list.
///
/// This is the case a pairwise left-to-right `reconcile` gets wrong, and it gets it wrong by
/// returning `f` — a wrong answer, and in a `WHERE` an indistinguishable one. The pair of
/// statements below is the whole of the difference: the same operand and the same first item, with
/// a typed item added at the end.
#[test]
fn an_in_list_is_typed_as_a_whole_and_types_its_operand() {
    let mut node = parity::Node::new(FIXTURE);

    // Nothing in the expression has a type, so it is text against text: `'01'` is not `'1'`.
    assert_eq!(node.rows("SELECT '01' IN ('1')"), vec![vec!["f"]]);
    // One integer anywhere in the list types *everything*, operand included: `1 IN (1, 1)`.
    assert_eq!(node.rows("SELECT '01' IN ('1', 1)"), vec![vec!["t"]]);
    // And the typed item may come after the unknown it types.
    assert_eq!(node.rows("SELECT '1' IN (1, '01')"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT ' 1' IN (1)"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT ' 1' IN ('1')"), vec![vec!["f"]]);
    assert_eq!(node.rows("SELECT '01' NOT IN ('1', 1)"), vec![vec!["f"]]);

    // A column is a type like any other, and it was already reaching the list before this unit.
    assert_eq!(
        node.rows("SELECT id FROM unk WHERE id IN ('1')"),
        vec![vec!["1"]]
    );

    // The three-valued rule is untouched by any of it: a match still wins outright, and a NULL
    // with no match is still unknown.
    assert_eq!(node.rows("SELECT 1 IN ('1', NULL)"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 IN (NULL, '1')"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 IN ('2', NULL)"), vec![vec!["\\N"]]);

    // The coercion is at plan time, so a list that cannot be typed raises even where a NULL
    // could have answered it.
    assert_eq!(
        node.run("SELECT 1 IN (NULL, 'x')").unwrap_err().sqlstate(),
        esker_sql::sqlstate::INVALID_TEXT_REPRESENTATION
    );

    // Two typed operands that no `=` covers are still `42883`, which is what the pairwise rule
    // after the common type is for.
    let error = node
        .run("SELECT id FROM unk WHERE n IN ('one', 1)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(error.to_string(), "operator does not exist: text = integer");
}
