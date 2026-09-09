//! `VALUES` as a **relation**, against PostgreSQL 19beta1.
//!
//! A table made of constant rows, in the two places one can stand: `(VALUES …) AS t(a,b)` where a
//! table goes, and `VALUES …` on its own as a statement. It is what `ARRAY(VALUES (1),(2))` needs
//! and what `array_agg` over a literal list is written with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is constant.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One entry, and it was twenty-six.** Twenty-five of them said that a bare integer constant
    // was an `int8` here where a real server's is an `int4`, so every column a `VALUES` list built
    // from one was `bigint`; listing them one by one rather than dropping the type column is what
    // made the day the ladder gained its `int4` rung (ADR 0085) a *failing* test rather than a
    // silent improvement, and all twenty-five went at once. What is left is `pg_typeof`'s own
    // `regtype`/`text` trade (ADR 0077), whose two answers are now right.
    types: &[
        "SELECT 'r', pg_typeof(x), pg_typeof(y) FROM (VALUES (1,'a'),(2,'b')) AS t(x,y) LIMIT 1",
    ],
    answers: &[
        // The standing constant-width divergence, showing through the one function that reports a
        // type as a value: a bare integer constant is `int8` here and `int4` on a real server, so
        // a `VALUES` column built from one is `bigint`. `pg_typeof` itself answers `text` rather
        // than `regtype` here, which is why the declared types differ too.
        // The same divergence for the other constant: a decimal constant is `double precision`
        // here and `numeric` there, which `Literal::Decimal` already chooses everywhere else.
        (
            "SELECT 'r', pg_typeof(column1) FROM (VALUES (1.5)) AS t LIMIT 1",
            "a decimal constant is double precision here and numeric there",
            "UNMEASURED",
        ),
        // The rule is right and the type in the message is the constant-width divergence: the
        // second row is read as the first row's type and fails to parse as it, which is the whole
        // point of the line.
        // **Not a `VALUES` gap.** A comma-separated `FROM` list is refused for every relation in
        // this crate; the `CROSS JOIN` spelling of this same statement is the line above it and it
        // answers.
    ],
};

#[test]
fn every_values_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_values_relation.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
