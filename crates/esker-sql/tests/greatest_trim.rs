//! `GREATEST`/`LEAST` and `TRIM`'s three forms, against PostgreSQL 19beta1.
//!
//! Two of `insert_all_test.rb`'s errors are the first — `ON CONFLICT ("id") DO UPDATE SET
//! status = GREATEST(books.status, 1)` was `0A000 the function GREATEST is not supported` — and
//! `TRIM(title)` is the other. Everything asserted here is measured in
//! `tests/captures/pg19_greatest_trim.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &["CREATE TABLE books (id int8 PRIMARY KEY, status int4, title text)"];

#[test]
fn greatest_and_least_pick_the_extreme() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT GREATEST(1, 2, 3)"), vec![vec!["3"]]);
    assert_eq!(node.rows("SELECT LEAST(1, 2, 3)"), vec![vec!["1"]]);
    assert_eq!(node.rows("SELECT greatest(1, 2)"), vec![vec!["2"]]);
    // One argument is legal and answers itself.
    assert_eq!(node.rows("SELECT GREATEST(1)"), vec![vec!["1"]]);
    assert_eq!(node.rows("SELECT GREATEST('a', 'b')"), vec![vec!["b"]]);
    assert_eq!(
        node.rows("SELECT GREATEST('2020-01-01'::date, '2021-01-01'::date)"),
        vec![vec!["2021-01-01"]]
    );
}

/// **Not strict**, which is the rule reasoning gets wrong: a NULL is skipped, not propagated.
#[test]
fn a_null_argument_is_skipped_and_not_propagated() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT GREATEST(1, NULL, 3)"), vec![vec!["3"]]);
    assert_eq!(node.rows("SELECT LEAST(NULL, 2)"), vec![vec!["2"]]);
    // And NULL only when every argument is one.
    assert_eq!(
        node.rows("SELECT GREATEST(NULL::int4, NULL::int4) IS NULL"),
        vec![vec!["t"]]
    );
}

/// The declared type is the arguments' common type, which is the ordinary promotion.
#[test]
fn the_type_is_the_arguments_common_one() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT pg_typeof(GREATEST(1::int2, 2::int8))"),
        vec![vec!["bigint"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(GREATEST(1::int4, 2.5::float8))"),
        vec![vec!["double precision"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(GREATEST('a'::text, 'b'::text))"),
        vec![vec!["text"]]
    );
}

/// The shape `insert_all_test.rb` sends.
#[test]
fn greatest_works_in_an_upsert_over_a_column() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO books (id, status) VALUES (1, 5)")
        .unwrap();
    node.run(
        "INSERT INTO books (id, status) VALUES (1, 0) ON CONFLICT (id) DO UPDATE SET \
         status = GREATEST(books.status, 1)",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT status FROM books"), vec![vec!["5"]]);

    node.run("UPDATE books SET status = 0 WHERE id = 1")
        .unwrap();
    node.run(
        "INSERT INTO books (id, status) VALUES (1, 0) ON CONFLICT (id) DO UPDATE SET \
         status = GREATEST(books.status, 1)",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT status FROM books"), vec![vec!["1"]]);
}

/// **Zero arguments is refused, and by a different code than a real server's.**
///
/// `GREATEST()` is `42601 syntax error at or near ")"` on 19beta1, because its *grammar* requires
/// an argument. Here the statement parses and the arity is checked afterwards, so it is `42883` —
/// a refusal either way, and the divergence is which one. Asserted as what this node does, with
/// PostgreSQL's beside it, so that the day the two agree this test says so rather than passing
/// quietly.
#[test]
fn no_arguments_at_all_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node.run("SELECT GREATEST()").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::UNDEFINED_FUNCTION,
        "PostgreSQL says 42601 here: its grammar requires an argument where this node parses the \
         call and checks the arity afterwards"
    );
}

#[test]
fn trim_has_three_forms_and_a_default() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT TRIM('  x  ')"), vec![vec!["x"]]);
    // The keyword with its characters, which is the shape that reaches the lowering.
    assert_eq!(
        node.rows("SELECT TRIM(BOTH ' ' FROM '  x  ')"),
        vec![vec!["x"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(LEADING ' ' FROM '  x  ')"),
        vec![vec!["x  "]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(TRAILING ' ' FROM '  x  ')"),
        vec![vec!["  x"]]
    );
}

/// **A side with no characters at all is `sqlparser`'s gap, and it is named as one.**
///
/// `TRIM(BOTH FROM '  x  ')` is `x` on 19beta1 — the whitespace default with the side written out
/// — and `sqlparser` 0.62 will not parse it: it expects the characters before `FROM` and reports
/// `Expected: ), found: '  x  '`. The lowering handles the form the moment the parser produces it
/// (`trim_what` is already an `Option`), so nothing here has to change when it does.
///
/// Not worked around — rewriting the text before parsing is how a keyword ends up in a refusal
/// table taking the blame for a gap somewhere else — and not a `42601` either, because a statement
/// PostgreSQL accepts is never a syntax error here (`tests/syntax_corpus.rs`): the refusal table
/// has a row for exactly this shape, `TRIM(BOTH FROM ...)`, so the answer is an `0A000` naming
/// what is missing. Nothing in the suite writes this form — `insert_all_test.rb` sends
/// `TRIM(title)`.
#[test]
fn a_side_with_no_characters_is_the_parsers_gap() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node.run("SELECT TRIM(BOTH FROM '  x  ')").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::FEATURE_NOT_SUPPORTED,
        "PostgreSQL answers `x` here, and this node names the gap: {error}"
    );
    assert!(
        error.to_string().contains("TRIM(BOTH FROM ...)"),
        "the refusal names the shape and not just a keyword: {error}"
    );
}

/// **The characters are a set, not a prefix** — the row that would have been got wrong.
#[test]
fn the_characters_are_a_set_and_not_a_prefix() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT TRIM(BOTH 'ab' FROM 'abcba')"),
        vec![vec!["c"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(BOTH 'x' FROM 'xxhixx')"),
        vec![vec!["hi"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(LEADING 'x' FROM 'xxhixx')"),
        vec![vec!["hixx"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(TRAILING 'x' FROM 'xxhixx')"),
        vec![vec!["xxhi"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM('x' FROM 'xxhixx')"),
        vec![vec!["hi"]]
    );
}

#[test]
fn the_function_spellings_are_the_same_three() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT btrim('  x  ')"), vec![vec!["x"]]);
    assert_eq!(node.rows("SELECT btrim('xxhixx', 'x')"), vec![vec!["hi"]]);
    assert_eq!(node.rows("SELECT ltrim('xxhi', 'x')"), vec![vec!["hi"]]);
    assert_eq!(node.rows("SELECT rtrim('hixx', 'x')"), vec![vec!["hi"]]);
    assert_eq!(
        node.rows("SELECT pg_typeof(TRIM('  x  '))"),
        vec![vec!["text"]]
    );
    assert_eq!(node.rows("SELECT TRIM(NULL) IS NULL"), vec![vec!["t"]]);
    // A varchar in, a text out.
    assert_eq!(
        node.rows("SELECT pg_typeof(TRIM('x'::varchar))"),
        vec![vec!["text"]]
    );
    assert_eq!(
        node.rows("SELECT TRIM(title) FROM books"),
        Vec::<Vec<String>>::new()
    );
}

/// **A bare `trim` strips spaces, and only spaces.** `btrim(text)` removes "a space by default",
/// and a real server means exactly that: `length(btrim(E'\t x \n'))` is 5 there, measured, where
/// a whitespace class would answer 1. Written with the control characters themselves, because the
/// `E'…'` spelling is a literal this node still refuses by name.
#[test]
fn a_bare_trim_strips_spaces_only() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT length(btrim('\t x \n')), length(ltrim('\tx')), length(rtrim('x\n'))"),
        vec![vec!["5", "2", "2"]]
    );
}

/// **The value carries the declared type.** `greatest(3::int4, 2::int8)` is described as `bigint`
/// and used to carry the `int4` it picked; `pg_typeof` reads the value and said `integer`.
/// Measured: `bigint`, `3`, `numeric`, `1.5`.
#[test]
fn the_value_carries_the_declared_type() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(greatest(3::int4, 2::int8)), greatest(3::int4, 2::int8), \
             pg_typeof(least(1.5::numeric, 2::int4)), least(1.5::numeric, 2::int4)"
        ),
        vec![vec!["bigint", "3", "numeric", "1.5"]]
    );
}
