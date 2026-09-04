//! `GENERATED ALWAYS AS (…) VIRTUAL` — PostgreSQL 18's other kind of generated column.
//!
//! **Run 64: 9 tests over 1 file**, and that row exists only because the previous unit corrected
//! it: those tests were ranked under `CREATE UNLOGGED is not supported` while unlogged tables
//! worked fine, and the refusal was naming the wrong feature.
//!
//! **The two kinds are indistinguishable from SQL except in one catalog column.** Same results,
//! same recompute on `UPDATE`, same `is_generated = ALWAYS`, same `generation_expression`, same
//! refusal of a non-DEFAULT value; `pg_attribute.attgenerated` is `s` for stored and `v` for
//! virtual, and that is the whole of it. So "virtual" is a statement about *storage*, not about
//! answers — which is why this node computes and stores the value, reports `v`, and is right about
//! every query. The divergence is where the bytes live, and nothing in SQL can see it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `column_name` is a `name` there and `is_generated` a `character varying(3)`; both are `text`
    // here, with identical characters — the standing `information_schema` trade.
    types: &[
        "SELECT column_name, is_generated, generation_expression, column_default FROM information_schema.columns WHERE table_name = 'vg' ORDER BY ordinal_position;",
    ],
    // **A rendering divergence, and the same family as `pg_get_viewdef`'s.** PostgreSQL prints
    // `generation_expression` through its own deparser, which lower-cases the function name:
    // `upper(name)` where this node stores and returns `UPPER(name)`, the text as written. The
    // expression *is* the same expression and evaluates identically — every value in this corpus
    // agrees — and what differs is the spelling a schema dumper would echo.
    answers: &[(
        "SELECT column_name, is_generated, generation_expression, column_default FROM information_schema.columns WHERE table_name = 'vg' ORDER BY ordinal_position;",
        "`UPPER(name)` against `upper(name)`: the stored text against PostgreSQL's deparse of its \
         own parse tree. Same expression, same values; reproducing the deparser is `ruleutils.c` \
         and is declared rather than attempted, exactly as for `pg_get_viewdef`.",
    )],
};

#[test]
fn every_virtual_generated_column_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_virtual_generated_column.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The keyword-less form is virtual**, and that is worth its own test because it changed.
///
/// Before PostgreSQL 18 the word `STORED` was required and there was no other kind; 18 added
/// `VIRTUAL` and made it the default. A rule read from memory rather than from the oracle would
/// have `GENERATED ALWAYS AS (expr)` mean *stored*, which is backwards — so it is measured here
/// and in the capture.
#[test]
fn the_keyword_less_form_is_virtual() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE k (a int, s int GENERATED ALWAYS AS (a + 1) STORED, v int GENERATED ALWAYS \
         AS (a + 2) VIRTUAL, d int GENERATED ALWAYS AS (a + 3))",
    )
    .unwrap();
    assert_eq!(
        node.rows(
            "SELECT attname, attgenerated FROM pg_attribute WHERE attrelid = 'k'::regclass AND \
             attnum > 0 ORDER BY attnum"
        ),
        [["a", ""], ["s", "s"], ["v", "v"], ["d", "v"]],
        "the bare form is virtual, and the two written kinds report the letter written"
    );
    // And all three compute, which is the point: `virtual` is about storage, not answers.
    node.run("INSERT INTO k (a) VALUES (10)").unwrap();
    assert_eq!(
        node.rows("SELECT a, s, v, d FROM k"),
        [["10", "11", "12", "13"]]
    );
    // A change to the source recomputes every one of them — the case the old refusal worried
    // about, and the reason storing a virtual column is not a wrong answer.
    node.run("UPDATE k SET a = 20").unwrap();
    assert_eq!(
        node.rows("SELECT a, s, v, d FROM k"),
        [["20", "21", "22", "23"]]
    );
}

/// A column a generated column reads cannot be dropped — the third dependency edge.
///
/// The other two are a view on a table and a view on a view (`tests/view_debts.rs`); this one
/// lives **inside a single table**, which makes it the cheapest to find and the easiest to miss.
#[test]
fn a_column_a_generated_column_reads_is_a_dependency() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g (id bigint PRIMARY KEY, a int, spare int, b int GENERATED ALWAYS AS (a + \
         1) VIRTUAL)",
    ]);
    // A column nothing computes from goes.
    node.run("ALTER TABLE g DROP COLUMN spare").unwrap();

    let error = node.run("ALTER TABLE g DROP COLUMN a").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.detail().as_deref(),
        Some("column b of table g depends on column a of table g")
    );
}
