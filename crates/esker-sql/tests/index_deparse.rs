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
//! # What is still stored as written, deliberately
//!
//! Only a `CASE` is deparsed here — every other index expression is stored as the user wrote it
//! (`exec::ddl`, and the comment there says why). So `CREATE INDEX ON t ((lower(v)))` still prints
//! `lower(v)` where a real server prints `lower((v)::text)`: the cast reaches an expression the
//! deparser walks, and a stored one keeps its text. Widening that is a unit of its own with corpus
//! consequences, and the Rails assertion above is inside a `CASE`.

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
