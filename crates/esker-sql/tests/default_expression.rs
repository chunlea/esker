//! A column `DEFAULT` is an arbitrary expression — statement 738's ten defaults.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a ON \
             a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = 'dd'::regclass AND \
             a.attname = 'bdf'",
            "**PostgreSQL prints the casts its parser inserted**, and this node prints the \
             expression as written: `convert_to('A'::text, 'UTF8'::name)` there against \
             `convert_to('A', 'UTF8')` here. A default is stored as a parse tree on a real \
             server, and resolving `convert_to` coerced two unknown-typed literals to the \
             parameter types before the tree was written, so the casts are *in* the tree the \
             deparser walks. Reproducing them needs a function signature catalog to coerce \
             against — the same machinery the arithmetic below needs — and until there is one the \
             text differs while the **value** does not: the row reads `\\x41` either way, which is \
             the line above this one.",
        ),
        (
            "SELECT length(convert_to('hello', 'UTF8'))",
            "`length` is not implemented, for any type — contract C2, and it is in the corpus \
             because it is the statement that would prove `convert_to` counted the bytes rather \
             than the characters. The two disagree only on multi-byte input, and the byte string \
             itself is checked directly in the test below.",
        ),
        (
            "SELECT pg_typeof(random()), pg_typeof(concat('a','b')), pg_typeof(convert_to('A','UTF8')), pg_typeof(CURRENT_DATE)",
            "`pg_typeof` is not implemented — contract C2, and the standing divergence of every \
             corpus that would use it to prove a declared type. The four types it would report \
             are what `PlainFunc::result_type` returns, and the `\\gdesc` column of every other \
             line here checks them the long way.",
        ),
        // **Both arithmetic entries are deleted** (ADR 0031, rule 2). They recorded that this
        // node had no arithmetic operator of any kind, so `random() * 100` and `1 + 1` — two of
        // statement 738's ten defaults — were refused by the operator's name. The arithmetic
        // landed on `main` in the same round as this branch's `random()`, and between them the
        // two lines now answer what a real server answers. Nothing in the default machinery
        // changed to make that true; the operators simply arrived under it.
        (
            "CREATE TABLE bad4 (a int8 DEFAULT nosuchfunc())",
            "`0A000` here against `42883` there, and the difference is contract C2 rather than a \
             mistake: on a real server the function genuinely does not exist, and on this one it \
             is a function nobody has implemented yet. **What matters is that both refuse when \
             the table is created** rather than when the first row is written — the expression is \
             resolved at DDL time here as it is there, which is what stops a table whose every \
             insert would fail.",
        ),
    ],
};

#[test]
fn every_default_expression_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_default_expression.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **PostgreSQL forbids three things in a `DEFAULT`, and this node forbids the same three.**
///
/// Not a volatility test and not a constant test — neither exists on a real server. A column
/// reference, a subquery and a set-returning function, each with its own message.
#[test]
fn the_only_three_refusals_are_postgresqls() {
    let mut node = parity::Node::new(&[]);
    for (statement, message) in [
        (
            "CREATE TABLE bad (a int8, b int8 DEFAULT a)",
            "cannot use column reference in DEFAULT expression",
        ),
        (
            "CREATE TABLE bad (a int8 DEFAULT (SELECT 1))",
            "cannot use subquery in DEFAULT expression",
        ),
        (
            "CREATE TABLE bad (a int8 DEFAULT generate_series(1, 2))",
            "set-returning functions are not allowed in DEFAULT expressions",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {statement}");
        assert_eq!(error.to_string(), message, "for {statement}");
    }
    // An unknown function is the ordinary `42883`, not a fourth rule about defaults.
    // An unknown function is **not** a fourth rule about defaults. A real server answers `42883`
    // because the function genuinely does not exist there; this node answers `0A000` naming it,
    // which is contract C2 — the function is one it has not implemented, not one nobody has. The
    // divergence is declared in the corpus; what matters here is that the refusal happens when the
    // table is created rather than when the first row is written.
    let error = node
        .run("CREATE TABLE bad (a int8 DEFAULT nosuchfunc())")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(error.to_string().contains("nosuchfunc"), "{error}");
}

/// The defaults statement 738 writes that need no arithmetic — nine of its ten.
#[test]
fn statement_738_s_defaults_are_taken() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE dd (id int8 PRIMARY KEY, ror varchar DEFAULT concat('Ruby ', 'on ', \
         'Rails'), md date DEFAULT CURRENT_DATE, mdf date DEFAULT now(), fd date DEFAULT \
         '2004-01-01', mt timestamp(6) DEFAULT CURRENT_TIMESTAMP, ft timestamp(6) DEFAULT \
         '2004-01-01 00:00:00', c1 char(1) DEFAULT 'Y', bd bigint DEFAULT 0::bigint, bdf bytea \
         DEFAULT convert_to('A', 'UTF8'))",
    )
    .unwrap();
    node.run("INSERT INTO dd (id) VALUES (1)").unwrap();
    assert_eq!(
        node.rows("SELECT ror, bd, bdf, c1, md = CURRENT_DATE FROM dd"),
        [["Ruby on Rails", "0", "\\x41", "Y", "t"]]
    );
}

/// A volatile default is **per row**, which is what makes folding it wrong rather than narrow.
#[test]
fn a_volatile_default_differs_per_row() {
    let mut node =
        parity::Node::new(&["CREATE TABLE dd (id int8 PRIMARY KEY, r float8 DEFAULT random())"]);
    for id in 0..8 {
        node.run(&format!("INSERT INTO dd (id) VALUES ({id})"))
            .unwrap();
    }
    assert_eq!(
        node.rows("SELECT count(DISTINCT r), count(*) FROM dd"),
        [["8", "8"]]
    );
}
