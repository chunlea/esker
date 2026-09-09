//! **What `pg_get_indexdef` and `pg_get_expr` print: the implicit cast, and the pair a bare column
//! does not take.**
//!
//! Two Rails assertions, one renderer family.
//!
//! ```text
//! schema_dumper_test#test_schema_dump_expression_indices
//!   /CASE.+lower\(\(name\)::text\).+END\) DESC"/i     -- the cast has to be shown
//! postgresql_adapter_test#test_partial_index_on_column_named_like_keyword
//!   index.where == "\"primary\""                       -- quoted, and NOT wrapped
//! ```
//!
//! # Measured on 19beta1
//!
//! **The cast.** A text function shows the conversion its argument took, and only a column takes a
//! visible one:
//!
//! ```text
//! (lower(name))    over character varying   lower((name)::text)
//! (length(v))      over character varying   length((v)::text)
//! (upper(c))       over character(3)        upper((c)::text)
//! (octet_length(v))over character varying   octet_length((v)::text)
//! (lower(t))       over text                lower(t)
//! (abs(n))         over integer             abs(n)          -- the one that does not take text
//! ```
//!
//! **The parentheses.** PostgreSQL's boolean deparser wraps everything except a bare column
//! reference — measured through a partial index's predicate and an exclusion constraint's alike:
//!
//! ```text
//! WHERE "primary"        -> "primary"       WHERE n > 0           -> (n > 0)
//! WHERE flag             -> flag            WHERE NOT flag        -> (NOT flag)
//! WHERE (flag)           -> flag            WHERE v IS NOT NULL   -> (v IS NOT NULL)
//! WHERE n > 0 AND flag   -> ((n > 0) AND flag)   -- the boolean operand takes none either
//! ```
//!
//! The user's own parentheses come off and the server's go on; the bare column is the one shape
//! that gets none, at the top and inside a chain.
//!
//! # What is deparsed, and what is still stored as written
//!
//! A `CASE` and a scalar call are deparsed; everything else keeps the user's text. Both are shapes
//! where what went in cannot be what comes out — a `CASE`'s implicit `ELSE` is filled in with the
//! resolved type, and a text function's argument shows the cast it took — and both are shapes the
//! deparser prints faithfully. Everything else agrees once the outer parentheses are normalised
//! and is left alone rather than passed through a deparser that still has a placeholder in it.
//!
//! This file first said `CREATE INDEX ON t ((lower(v)))` was a standing divergence, printing
//! `lower(v)` where a real server prints `lower((v)::text)`. It is not one any more: the scalar
//! call joined the `CASE`, and the four shapes are asserted below.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **The expression index `schema_dumper_test` reads**, byte for byte as 19beta1 prints it —
/// newlines and indentation included, which reach the dump as `\n` escapes inside a Ruby string
/// and are what the test's `.` matches.
#[test]
fn a_case_in_an_index_key_shows_the_casts_postgresql_shows() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_companies (id bigserial primary key, name character varying, \
         rating integer)",
        "CREATE INDEX g1b_expression_index ON g1b_companies \
         ((CASE WHEN rating > 0 THEN lower(name) END) DESC)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('g1b_expression_index'::regclass, 1, false)"),
        [["(\nCASE\n    WHEN (rating > 0) THEN lower((name)::text)\n    ELSE NULL::text\nEND)"]]
    );
    // And the whole definition, which is what the dumper's `t.index` line is built from.
    assert_eq!(
        node.rows("SELECT indexdef FROM pg_indexes WHERE indexname = 'g1b_expression_index'"),
        [[
            "CREATE INDEX g1b_expression_index ON public.g1b_companies USING btree ((\nCASE\n    \
             WHEN (rating > 0) THEN lower((name)::text)\n    ELSE NULL::text\nEND) DESC)"
        ]]
    );
}

/// **A scalar call as the whole index key** shows its cast too, which this file used to record as
/// a divergence and no longer does.
#[test]
fn a_scalar_index_key_shows_its_cast() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_s (v character varying, c character(3), t text, n integer)",
        "CREATE INDEX g1b_s1 ON g1b_s ((length(v)))",
        "CREATE INDEX g1b_s2 ON g1b_s ((upper(c)))",
        "CREATE INDEX g1b_s3 ON g1b_s ((abs(n)))",
        "CREATE INDEX g1b_s4 ON g1b_s ((lower(t)))",
    ]);
    assert_eq!(
        node.rows(
            "SELECT c.relname, pg_get_indexdef(i.indexrelid, 1, false) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname LIKE 'g1b_s_' ORDER BY 1"
        ),
        [
            ["g1b_s1".to_owned(), "length((v)::text)".to_owned()],
            ["g1b_s2".to_owned(), "upper((c)::text)".to_owned()],
            // The one scalar function that does not take text keeps its argument bare.
            ["g1b_s3".to_owned(), "abs(n)".to_owned()],
            ["g1b_s4".to_owned(), "lower(t)".to_owned()],
        ]
    );
}

/// **A generated column is normalised the same way**, once, where the table is known:
/// `GENERATED ALWAYS AS (UPPER(name))` reads back `upper((name)::text)` — lower-cased and cast,
/// which `virtual_column_test#test_schema_dumping` asserts — and still computes.
#[test]
fn a_generated_column_reads_back_deparsed_and_still_computes() {
    let mut node = parity::Node::new(&["CREATE TABLE g1b_v (name character varying, t text, \
         upper_name character varying GENERATED ALWAYS AS (UPPER(name)) STORED, \
         plain_upper text GENERATED ALWAYS AS (upper(t)) STORED)"]);
    assert_eq!(
        node.rows(
            "SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
             JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             WHERE d.adrelid = 'g1b_v'::regclass ORDER BY 1"
        ),
        [
            ["plain_upper".to_owned(), "upper(t)".to_owned()],
            ["upper_name".to_owned(), "upper((name)::text)".to_owned()],
        ]
    );
    // **The normalised text is still the expression**, which is what a rewrite of stored SQL has
    // to prove: the column computes from it on the next insert.
    node.run("INSERT INTO g1b_v (name, t) VALUES ('rails', 'x')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT upper_name, plain_upper FROM g1b_v"),
        [["RAILS".to_owned(), "X".to_owned()]]
    );
}

/// **A bare column predicate is quoted and unwrapped** — the keyword-named column
/// `postgresql_adapter_test` asserts, which needs both halves at once.
#[test]
fn a_bare_column_predicate_is_quoted_and_takes_no_parentheses() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_ex (id bigserial primary key, number integer, \"primary\" boolean)",
        "CREATE INDEX g1b_partial ON g1b_ex (id) WHERE \"primary\"",
    ]);
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'g1b_partial'"
        ),
        [["\"primary\""]]
    );
    assert_eq!(
        node.rows("SELECT indexdef FROM pg_indexes WHERE indexname = 'g1b_partial'"),
        [["CREATE INDEX g1b_partial ON public.g1b_ex USING btree (id) WHERE \"primary\""]]
    );
}

/// Every other predicate shape keeps its pair, including each operand of a chain — except the
/// boolean column inside one.
#[test]
fn every_other_predicate_shape_keeps_its_parentheses() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_p (id int8, flag boolean, v character varying, n integer)",
        "CREATE INDEX g1b_p1 ON g1b_p (id) WHERE flag",
        "CREATE INDEX g1b_p2 ON g1b_p (id) WHERE (flag)",
        "CREATE INDEX g1b_p3 ON g1b_p (id) WHERE n > 0",
        "CREATE INDEX g1b_p4 ON g1b_p (id) WHERE n > 0 AND flag",
        "CREATE INDEX g1b_p5 ON g1b_p (id) WHERE v IS NOT NULL",
    ]);
    assert_eq!(
        node.rows(
            "SELECT c.relname, pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname LIKE 'g1b_p_' ORDER BY 1"
        ),
        [
            ["g1b_p1".to_owned(), "flag".to_owned()],
            // The user's own pair comes off and none goes back on.
            ["g1b_p2".to_owned(), "flag".to_owned()],
            ["g1b_p3".to_owned(), "(n > 0)".to_owned()],
            ["g1b_p4".to_owned(), "((n > 0) AND flag)".to_owned()],
            ["g1b_p5".to_owned(), "(v IS NOT NULL)".to_owned()],
        ]
    );
}

/// The same rule inside an exclusion constraint, which shares the deparser and was measured with
/// it: `EXCLUDE … WHERE (((n > 0) AND flag))`, three pairs and none around the column.
#[test]
fn an_exclusion_predicate_follows_the_same_rule() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_e (r int4range, flag boolean, n integer)",
        "ALTER TABLE g1b_e ADD CONSTRAINT g1b_x EXCLUDE USING gist (r WITH &&) \
         WHERE (n > 0 AND flag)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'g1b_x'"),
        [["EXCLUDE USING gist (r WITH &&) WHERE (((n > 0) AND flag))"]]
    );
}

/// **A predicate is deparsed at the statement that writes it, and `ON CONFLICT` still finds it.**
///
/// `docs/plans/debts-v1.1.md` #29: `pg_get_expr(indpred)` prints what is stored, so a predicate
/// goes through the deparser like every other stored expression — the sixth reader. Two things had
/// to be true at once, and they are what this test is:
///
/// 1. **The predicate really is deparsed.** `btrim(t, 'x') = 'y'` is stored as
///    `btrim(t, 'x'::text) = 'y'::text`, showing the coercions a real server shows. Measured in
///    `tests/corpus/pg19_catalog_func_deparse.txt` and `pg19_negative_constant.txt`, whose
///    `indpred` rows this closed.
/// 2. **`ON CONFLICT` still infers the index.** The arbiter matches a partial index by its
///    predicate *text* (`exec::dml::same_predicate`), and once the stored text is deparsed it is
///    no longer the text a client wrote — `WHERE "b" IS NOT NULL` against `b IS NOT NULL`. The
///    first attempt at this unit turned a working statement into `42P10` for exactly that reason,
///    so the arbiter now puts what the statement wrote through the same printer first.
///
/// The second assertion is the one that would pass on the wrong mechanism: it also passes if
/// nothing is deparsed at all. That is what the first is for, and why they are one test.
#[test]
fn a_deparsed_predicate_is_still_the_one_on_conflict_names() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1b_oc (a int8, b int8, t text)",
        // Written without the coercions, and with the quoting `ActiveRecord` writes.
        "CREATE UNIQUE INDEX g1b_oc_u ON g1b_oc (a) WHERE btrim(t, 'x') = 'y'",
        "CREATE UNIQUE INDEX g1b_oc_q ON g1b_oc (b) WHERE b IS NOT NULL",
    ]);
    // 1. the coercions are stored, which is what the sixth reader means
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i JOIN pg_class c \
             ON c.oid = i.indexrelid WHERE c.relname = 'g1b_oc_u'"
        ),
        [["(btrim(t, 'x'::text) = 'y'::text)"]]
    );
    // 2. and the statement that repeats the predicate as *written* still infers the index
    for sql in [
        "INSERT INTO g1b_oc (a, t) VALUES (1, 'y') ON CONFLICT (a) WHERE btrim(t, 'x') = 'y' \
         DO NOTHING",
        "INSERT INTO g1b_oc (b) VALUES (2) ON CONFLICT (\"b\") WHERE \"b\" IS NOT NULL DO NOTHING",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
}
