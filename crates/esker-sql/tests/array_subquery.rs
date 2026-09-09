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
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        "SELECT 'r', ARRAY(SELECT 1)::int8[], pg_typeof(ARRAY(SELECT 1)::int8[])",
        // These two moved up from `answers` with the literal ladder's `int4` rung (ADR 0085):
        // `ARRAY(SELECT 1)` is an `integer[]` on both now, and `pg_typeof`'s own answer is all
        // that is left of a reason that used to be about the constant's width.
        "SELECT 'r', ARRAY(SELECT 1), pg_typeof(ARRAY(SELECT 1))",
        "SELECT 'r', ARRAY(SELECT 1 WHERE false), pg_typeof(ARRAY(SELECT 1 WHERE false))",
        // Surfaced with the two below it when the runtime cast stopped aborting this file. The
        // rows agree; what differs is the standing integer-width trade — a small constant is
        // `integer` on a real server and `bigint` here — seen through `ARRAY(VALUES …)`.
        "SELECT 'r', ARRAY(SELECT 'a'::text), pg_typeof(ARRAY(SELECT 'a'::text))",
        // The three below are reached for the first time now that a bare `VALUES` list runs; they
        // were swallowed by the aborted block before. Their rows are right and two facts show in
        // the declared types: a bare integer constant is `int8` here and `int4` there, so an array
        // of them is `bigint[]`; and `array_agg` declares `text` here whatever it collects, which
        // is the aggregate result typing and a unit of its own.
        "SELECT 'r', ARRAY(SELECT x FROM generate_series(1,3) AS x ORDER BY x DESC)",
        "SELECT 'r', ARRAY(SELECT x FROM generate_series(1,5) AS x ORDER BY x LIMIT 2)",
        "SELECT 'r', ARRAY(SELECT x FROM generate_series(1,3) AS x), array_agg(x ORDER BY x) FROM generate_series(1,3) AS x",
    ],
    answers: &[
        // **Both of these were swallowed until the runtime cast landed.** The line above them,
        // `ARRAY(SELECT 1)::int8[]`, was `0A000 a cast to INT8[]`, and its abort took the rest of
        // the file with it — which is exactly what the harness's third ratchet rule exists to
        // catch, and what closing one refusal surfaces. Neither is about casts; both are about
        // `ARRAY(subquery)` and belong to whoever takes that back up.
        (
            "SELECT 'r', ARRAY(SELECT ARRAY(SELECT 1))",
            "a nested `ARRAY(subquery)` flattens on a real server -- `{{1}}` of type `integer[]`, \
             not an array of arrays -- and is an array of the inner array's *text* here",
            "pg19_array_subquery.txt:75",
        ),
        (
            "SELECT ARRAY(SELECT)",
            "a subquery with no column at all is `42601` on a real server and `0A000` naming \
             `ARRAY` here: the refusal fires before the subquery's shape is looked at",
            "pg19_array_subquery.txt:80",
        ),
        // Reached for the first time now that a bare `VALUES` list runs: this statement used to be
        // one of the eight the aborted block swallowed. The refusal is the crate's standing one
        // for a **per-row** cast — a cast of anything but a constant has only `text` as a target —
        // and not something about arrays: `ARRAY(SELECT 1)` itself answers on the line above.
        (
            "SELECT 'r', ARRAY(SELECT generate_series(1,3)), pg_typeof(ARRAY(SELECT generate_series(1,3)))",
            "**The rows agree and the constant's width does not.** `ARRAY(SELECT 1)` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here (`tests/unknown_literal.rs`); `generate_series`' column follows its arguments, so it inherits the same difference. `pg_typeof` reports `regtype` there and `text` here, which is the trade `'x'::regtype` already makes. Every value is identical.",
            "pg19_array_subquery.txt:64",
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
