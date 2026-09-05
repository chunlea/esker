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
    // **One fact, twenty-five times.** A bare integer constant is `int8` here and `int4` on a real
    // server, so every column a `VALUES` list builds from one is `bigint` where PostgreSQL says
    // `integer` — and an array of them is `bigint[]`. The rows agree everywhere; only the declared
    // type differs. Listing them one by one rather than dropping the type column is what makes the
    // day this is fixed a *failing* test rather than a silent improvement.
    types: &[
        "SELECT 'r', * FROM (VALUES (1),(2),(3)) AS t(x) ORDER BY x",
        "SELECT 'r', a, b FROM (VALUES (1,'x'),(2,'y')) AS t(a,b) ORDER BY a",
        "SELECT 'r', column1, column2 FROM (VALUES (1,'x'),(2,'y')) AS t ORDER BY column1",
        "SELECT 'r', * FROM (VALUES (1),(NULL),(3)) AS t(x) ORDER BY x",
        "SELECT 'r', array_agg(x ORDER BY x) FROM (VALUES (1),(2)) AS t(x)",
        "SELECT 'r', ARRAY(SELECT x FROM (VALUES (1),(NULL),(3)) AS t(x))",
        "SELECT 'r', ARRAY(VALUES (1),(2))",
        "SELECT 'r', x FROM (VALUES (1),(2)) AS t(x) WHERE x > 1",
        "SELECT 'r', * FROM (VALUES (1,2),(3,4)) AS t(a,b) ORDER BY a DESC",
        "SELECT 'r', v.x FROM (VALUES (1)) AS v(x)",
        "VALUES (1),(2),(3)",
        "VALUES (1,'a'),(2,'b')",
        "VALUES (1)",
        "VALUES (1),(2) ORDER BY column1 DESC",
        "VALUES (1),(2),(3) LIMIT 2",
        "VALUES (1),(2),(3) OFFSET 1",
        "VALUES (1),(NULL)",
        "SELECT 'r', * FROM (VALUES (1,2)) AS t(a)",
        "SELECT 'r', * FROM (VALUES (1)) t(x)",
        "SELECT 'r', * FROM (VALUES (1))",
        "SELECT 'r', 1 FROM (VALUES (1),(2)) AS t(x) CROSS JOIN (VALUES (3)) AS u(y)",
        "SELECT 'r', t.x, u.y FROM (VALUES (1),(2)) AS t(x) JOIN (VALUES (2)) AS u(y) ON t.x = u.y",
        "SELECT 'r', x FROM (VALUES (3),(1),(2)) AS t(x)",
        "SELECT 'r', * FROM (VALUES (1),(2)) AS t(x) WHERE x = 2",
        "VALUES (1),(2) ORDER BY 1 DESC",
    ],
    answers: &[
        // The standing constant-width divergence, showing through the one function that reports a
        // type as a value: a bare integer constant is `int8` here and `int4` on a real server, so
        // a `VALUES` column built from one is `bigint`. `pg_typeof` itself answers `text` rather
        // than `regtype` here, which is why the declared types differ too.
        (
            "SELECT 'r', pg_typeof(x), pg_typeof(y) FROM (VALUES (1,'a'),(2,'b')) AS t(x,y) LIMIT 1",
            "a bare integer constant is int8 here and int4 there, and pg_typeof answers text",
            "UNMEASURED",
        ),
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
        (
            "VALUES (1),('a')",
            "the type named in the message is bigint here and integer there",
            "UNMEASURED",
        ),
        // **Not a `VALUES` gap.** A comma-separated `FROM` list is refused for every relation in
        // this crate; the `CROSS JOIN` spelling of this same statement is the line above it and it
        // answers.
        (
            "SELECT 'r', 1 FROM (VALUES (1),(2)) AS t(x), (VALUES (3)) AS u(y)",
            "a comma-separated FROM list is refused for every relation, not only for VALUES",
            "UNMEASURED",
        ),
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
