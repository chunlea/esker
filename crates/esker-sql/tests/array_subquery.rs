//! `ARRAY( SELECT … )` — the array **subquery** constructor, against PostgreSQL 19beta1.
//!
//! Boot statement 32, the last of `ActiveRecord`'s 36, and **not** `ARRAY[…]`. The bracket
//! constructor landed and boot stayed at 35/36, because this is a different grammar production:
//! its argument is a query, its elements are that query's rows, and its order is the query's
//! `ORDER BY`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_typeof` answers a `regtype` on a real server and `text` here — the same trade
    // `'x'::regtype` makes, and the same characters either way. The row agrees.
    types: &["SELECT 'r', ARRAY(SELECT 'a'::text), pg_typeof(ARRAY(SELECT 'a'::text))"],
    answers: &[
        (
            "SELECT 'r', ARRAY(SELECT 1), pg_typeof(ARRAY(SELECT 1))",
            "**The rows agree and the constant's width does not.** `ARRAY(SELECT 1)` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here (`tests/unknown_literal.rs`); `generate_series`' column follows its arguments, so it inherits the same difference. `pg_typeof` reports `regtype` there and `text` here, which is the trade `'x'::regtype` already makes. Every value is identical.",
        ),
        (
            "SELECT 'r', ARRAY(SELECT generate_series(1,3)), pg_typeof(ARRAY(SELECT generate_series(1,3)))",
            "**The rows agree and the constant's width does not.** `ARRAY(SELECT 1)` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here (`tests/unknown_literal.rs`); `generate_series`' column follows its arguments, so it inherits the same difference. `pg_typeof` reports `regtype` there and `text` here, which is the trade `'x'::regtype` already makes. Every value is identical.",
        ),
        (
            "SELECT 'r', ARRAY(SELECT 1 WHERE false), pg_typeof(ARRAY(SELECT 1 WHERE false))",
            "**The rows agree and the constant's width does not.** `ARRAY(SELECT 1)` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here (`tests/unknown_literal.rs`); `generate_series`' column follows its arguments, so it inherits the same difference. `pg_typeof` reports `regtype` there and `text` here, which is the trade `'x'::regtype` already makes. Every value is identical.",
        ),
        (
            "SELECT 'r', ARRAY(SELECT NULL::int4), ARRAY(SELECT x FROM (VALUES (1),(NULL),(3)) AS t(x))",
            "**`VALUES` as a query** — a relation made of constant rows — which this node does not have in either spelling: as a derived table (`(VALUES …) AS t(x)`) or as the argument of this constructor. It is a feature of its own and not an array one, and it is the **last blocker on this corpus**: the capture runs inside one transaction, so the statements after these two are swallowed by the abort rather than checked. What they cover is asserted directly in this file instead, so nothing here rests on a statement that did not run.",
        ),
    ],
};

#[test]
fn every_array_subquery_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_subquery.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Boot statement 32 itself**, over a table with two indexes — the last of `ActiveRecord`'s
/// thirty-six.
///
/// Written as assertions rather than replayed from the capture, and the reason is worth stating:
/// the capture runs in one transaction, and two statements in the middle of it need things this
/// node does not have — a `VALUES` relation, and `COMMENT ON`. Either aborts the block, and
/// everything after is swallowed rather than checked, statement 32 included. So the values below
/// are the capture's, read off it and asserted directly, and the one column that cannot match is
/// named: `obj_description` is NULL here because there is nowhere to put a comment, which is the
/// divergence `tests/pg_catalog_description.rs` already declares.
#[test]
fn boot_statement_32_answers_the_way_postgresql_19_does() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE \"Quoted\" (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX quoted_a_b_idx ON \"Quoted\" (a, b DESC)",
        "CREATE UNIQUE INDEX quoted_b_idx ON \"Quoted\" (b)",
    ]);

    // The statement as `ActiveRecord` sends it, and the row it reads out of it. `columns` is the
    // array this whole unit is for: one element per index column, **in index order**, which is
    // what the inner `ORDER BY k` decides.
    let rows = node.rows(
        "SELECT distinct i.relname, d.indisunique, d.indkey, pg_get_indexdef(d.indexrelid), \
         pg_catalog.obj_description(i.oid, 'pg_class') AS comment, d.indisvalid, ARRAY( SELECT \
         pg_get_indexdef(d.indexrelid, k + 1, true) FROM generate_subscripts(d.indkey, 1) AS k \
         ORDER BY k ) AS columns FROM pg_class t INNER JOIN pg_index d ON t.oid = d.indrelid \
         INNER JOIN pg_class i ON d.indexrelid = i.oid WHERE i.relname = 'quoted_a_b_idx'",
    );
    assert_eq!(
        rows,
        vec![vec![
            "quoted_a_b_idx".to_owned(),
            "f".to_owned(),
            // An `int2vector`, printed space-separated — and **subscripted from zero**, which is
            // why the statement writes `k + 1`.
            "2 3".to_owned(),
            "CREATE INDEX quoted_a_b_idx ON public.\"Quoted\" USING btree (a, b DESC)".to_owned(),
            // `the index comment` on a real server; there is no `COMMENT ON` here to put one.
            "\\N".to_owned(),
            "t".to_owned(),
            "{a,b}".to_owned(),
        ]]
    );

    // **It correlates**, which is what statement 32 does with `d.indexrelid` and `d.indkey`: the
    // subquery is re-run per outer row and its array grows down them. The capture's values.
    for statement in [
        "INSERT INTO \"Quoted\" VALUES (1,10,'x')",
        "INSERT INTO \"Quoted\" VALUES (2,20,'y')",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT id, ARRAY(SELECT q2.id FROM \"Quoted\" q2 WHERE q2.id <= q1.id ORDER BY \
             q2.id) FROM \"Quoted\" q1 ORDER BY id"
        ),
        vec![
            vec!["1".to_owned(), "{1}".to_owned()],
            vec!["2".to_owned(), "{1,2}".to_owned()],
        ]
    );

    // **The contrast this constructor exists for**, over the same empty input: `array_agg` is
    // NULL and `ARRAY(SELECT …)` is the empty array. An implementation that rewrote one into the
    // other would return NULL where Rails expects `{}`.
    assert_eq!(
        node.rows(
            "SELECT ARRAY(SELECT id FROM \"Quoted\" WHERE a > 99), array_agg(id) FROM \"Quoted\" \
             WHERE a > 99"
        ),
        vec![vec!["{}".to_owned(), "\\N".to_owned()]]
    );

    // And the array it builds is an ordinary array: `= ANY` reads it, and so do the array
    // functions, which is what says it is a value and not a special form.
    assert_eq!(
        node.rows(
            "SELECT 1 = ANY(ARRAY(SELECT id FROM \"Quoted\")), 99 = ANY(ARRAY(SELECT id FROM \
             \"Quoted\")), array_length(ARRAY(SELECT id FROM \"Quoted\"), 1), \
             cardinality(ARRAY(SELECT id FROM \"Quoted\"))"
        ),
        vec![vec![
            "t".to_owned(),
            "f".to_owned(),
            "2".to_owned(),
            "2".to_owned()
        ]]
    );
}
