//! **The deparse census: expression shape × reader.**
//!
//! Two days of reds had one thing in common — they were **wrong and green**. The node printed a
//! stored expression differently from a real server and nothing in this repository said so,
//! because each reader had only ever been measured on the shapes that happened to reach it: a
//! `CASE` through a `CHECK`, a `mod` through an index, a reserved word through two of five
//! printers. Each was found by the Rails suite, one per run.
//!
//! This file is the cross product instead — 86 expression shapes and 10 constants against the
//! eight readers each admits, 221 probes — so that the next parenthesis rule is learned here.
//!
//! ```text
//! shapes   binary chains (AND/OR mixed and nested) · operator-spelled functions both ways ·
//!          CASE searched and simple, comparisons and functions in every position · cast chains
//!          and negative constants · array constructor, subscript, slice, concatenation ·
//!          nested string functions · NULL tests · BETWEEN · LIKE/ILIKE · IS DISTINCT FROM ·
//!          IN · COALESCE/GREATEST/NULLIF
//! readers  pg_get_constraintdef plain and pretty · pg_get_indexdef · pg_get_expr(indexprs) ·
//!          pg_get_expr(indpred) · pg_get_expr(adbin) for a generated column and for a DEFAULT ·
//!          pg_get_viewdef
//! ```
//!
//! Five probes record a **refusal** rather than a print. Three: PostgreSQL will not take
//! `(ARRAY[...])::text` in a generated column, because the array-to-text cast reads settings. Two
//! more: the simple `CASE`'s `WHEN` is a value compared with `=`, so `CASE t WHEN 1` is
//! `42883 operator does not exist: text = integer` and `CASE a WHEN 'x'` is `22P02`. A reader that
//! refuses is as much a fact as one that prints, so the refusal is the expected answer and the
//! savepoint around it is what keeps the other 217 comparable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Thirty-four statements in four groups, by mechanism.** The census opened with
    // sixty-five in eight; groups B, H, A, E and F are closed and deleted as blocks, and group I is what closing A
    // left behind — which is the point of grouping them: a group that shrinks by one was two
    // mechanisms wearing one name.
    //
    // **Twice now a group has shrunk by less than its size, and both times that was the tell.** E
    // gave up two of four and C seven of fourteen: the rest were `pg_get_viewdef` rows over
    // expressions with nothing of the group's subject in them, diverging for G's reason and listed
    // there now. C's remainder is a *feature* gap rather than a printing one and keeps the letter
    // under a name that says so. Each group's reason
    // is written once and attached to every member: what is worth reading is the mechanism, not
    // forty-six sentences.
    answers: &[
        // ---- I-midchain: 3 statements ----
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k4m'",
            "**Group I -- a pair the user wrote in the middle of a chain is not printed back.** `parse::lower::balance` folds `a AND b AND c AND d` *pairwise*, into a tree `log2(n)` deep, so `(a AND b) AND (c AND d)` and the same four terms unparenthesised lower to the identical tree and there is nothing left to tell them apart -- the deparser prints the flat form, which is right for the shape people write and one pair short for the shape they rarely do. PostgreSQL keeps it because its `BoolExpr` is n-ary and the parser only merges the *left* spine. The two readers disagree in opposite directions and only one of them can be served: the **pretty** form agrees for both shapes (a real server flattens it there too), which is the form `ActiveRecord` reads. **And the obvious closure costs more than the divergence.** Folding the chain left instead -- which is what would preserve the written grouping, since PostgreSQL merges only the left spine -- makes an `n`-term chain `n` levels deep where `balance` makes it `log2(n)`, and `parse::MAX_PLAN_DEPTH` is **42 levels in debug and 204 in release**: a 43-term `OR` chain would become `54001` where it runs today, on ordinary generated SQL, and no test would catch it -- `tests/lowering_depth.rs` pins 16 terms and 100,000 and nothing between. The closure that does not cost that is an **n-ary boolean node**, PostgreSQL's own `BoolExpr`: one level per chain rather than `log2(n)`, the written grouping preserved exactly the way a real server preserves it, and the printer trivial. That is a plan-variant change across every walker and evaluator, so it is a unit of its own and not a line in a deparse group -- recorded here so that whoever opens this row starts from the measurement rather than from the left fold.",
            "pg19_deparse_census.txt:690",
        ),
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'ky4'",
            "**Group I -- a pair the user wrote in the middle of a chain is not printed back.** `parse::lower::balance` folds `a AND b AND c AND d` *pairwise*, into a tree `log2(n)` deep, so `(a AND b) AND (c AND d)` and the same four terms unparenthesised lower to the identical tree and there is nothing left to tell them apart -- the deparser prints the flat form, which is right for the shape people write and one pair short for the shape they rarely do. PostgreSQL keeps it because its `BoolExpr` is n-ary and the parser only merges the *left* spine. The two readers disagree in opposite directions and only one of them can be served: the **pretty** form agrees for both shapes (a real server flattens it there too), which is the form `ActiveRecord` reads. **And the obvious closure costs more than the divergence.** Folding the chain left instead -- which is what would preserve the written grouping, since PostgreSQL merges only the left spine -- makes an `n`-term chain `n` levels deep where `balance` makes it `log2(n)`, and `parse::MAX_PLAN_DEPTH` is **42 levels in debug and 204 in release**: a 43-term `OR` chain would become `54001` where it runs today, on ordinary generated SQL, and no test would catch it -- `tests/lowering_depth.rs` pins 16 terms and 100,000 and nothing between. The closure that does not cost that is an **n-ary boolean node**, PostgreSQL's own `BoolExpr`: one level per chain rather than `log2(n)`, the written grouping preserved exactly the way a real server preserves it, and the printer trivial. That is a plan-variant change across every walker and evaluator, so it is a unit of its own and not a line in a deparse group -- recorded here so that whoever opens this row starts from the measurement rather than from the left fold.",
            "pg19_deparse_census.txt:774",
        ),
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k4r'",
            "**Group I -- a pair the user wrote in the middle of a chain is not printed back.** `parse::lower::balance` folds `a AND b AND c AND d` *pairwise*, into a tree `log2(n)` deep, so `(a AND b) AND (c AND d)` and the same four terms unparenthesised lower to the identical tree and there is nothing left to tell them apart -- the deparser prints the flat form, which is right for the shape people write and one pair short for the shape they rarely do. PostgreSQL keeps it because its `BoolExpr` is n-ary and the parser only merges the *left* spine. The two readers disagree in opposite directions and only one of them can be served: the **pretty** form agrees for both shapes (a real server flattens it there too), which is the form `ActiveRecord` reads. **And the obvious closure costs more than the divergence.** Folding the chain left instead -- which is what would preserve the written grouping, since PostgreSQL merges only the left spine -- makes an `n`-term chain `n` levels deep where `balance` makes it `log2(n)`, and `parse::MAX_PLAN_DEPTH` is **42 levels in debug and 204 in release**: a 43-term `OR` chain would become `54001` where it runs today, on ordinary generated SQL, and no test would catch it -- `tests/lowering_depth.rs` pins 16 terms and 100,000 and nothing between. The closure that does not cost that is an **n-ary boolean node**, PostgreSQL's own `BoolExpr`: one level per chain rather than `log2(n)`, the written grouping preserved exactly the way a real server preserves it, and the printer trivial. That is a plan-variant change across every walker and evaluator, so it is a unit of its own and not a line in a deparse group -- recorded here so that whoever opens this row starts from the measurement rather than from the left fold.",
            "pg19_deparse_census.txt:698",
        ),
        // ---- C-slice: 4 statements ----
        (
            "CREATE INDEX ix_arr_slice ON cen ((arr[1:2]))",
            "**Group C -- an array slice is `0A000` here and a working index key there.** `arr[1:2]` is refused by name (`an array slice is not supported`), so the four readers that would print it are never reached: measured, PostgreSQL takes `CREATE INDEX ON cen ((arr[1:2]))` and prints it back `USING btree ((arr[1:2]))`, stores `arr[1:2]` in `indexprs`, and refuses only the *generated column* over `(arr[1:2])::text` -- for the immutability reason, not the slice. A feature gap and not a printing one, which is what is left of a group that was four mechanisms wearing one word: the pair on a constructor and a subscript, the immutability refusal, the viewdef rows that were never C's, and this.",
            "pg19_deparse_census.txt:376",
        ),
        (
            "CREATE INDEX ix_arr_slice ON cen ((arr[1:2]))",
            "**Group C -- an array slice is `0A000` here and a working index key there.** `arr[1:2]` is refused by name (`an array slice is not supported`), so the four readers that would print it are never reached: measured, PostgreSQL takes `CREATE INDEX ON cen ((arr[1:2]))` and prints it back `USING btree ((arr[1:2]))`, stores `arr[1:2]` in `indexprs`, and refuses only the *generated column* over `(arr[1:2])::text` -- for the immutability reason, not the slice. A feature gap and not a printing one, which is what is left of a group that was four mechanisms wearing one word: the pair on a constructor and a subscript, the immutability refusal, the viewdef rows that were never C's, and this.",
            "pg19_deparse_census.txt:382",
        ),
        (
            "ALTER TABLE cen ADD COLUMN g_arr_slice text GENERATED ALWAYS AS ((arr[1:2])::text) STORED",
            "**Group C -- an array slice is `0A000` here and a working index key there.** `arr[1:2]` is refused by name (`an array slice is not supported`), so the four readers that would print it are never reached: measured, PostgreSQL takes `CREATE INDEX ON cen ((arr[1:2]))` and prints it back `USING btree ((arr[1:2]))`, stores `arr[1:2]` in `indexprs`, and refuses only the *generated column* over `(arr[1:2])::text` -- for the immutability reason, not the slice. A feature gap and not a printing one, which is what is left of a group that was four mechanisms wearing one word: the pair on a constructor and a subscript, the immutability refusal, the viewdef rows that were never C's, and this.",
            "pg19_deparse_census.txt:388",
        ),
        (
            "CREATE VIEW v_arr_slice AS SELECT arr[1:2] AS x FROM cen",
            "**Group C -- an array slice is `0A000` here and a working index key there.** `arr[1:2]` is refused by name (`an array slice is not supported`), so the four readers that would print it are never reached: measured, PostgreSQL takes `CREATE INDEX ON cen ((arr[1:2]))` and prints it back `USING btree ((arr[1:2]))`, stores `arr[1:2]` in `indexprs`, and refuses only the *generated column* over `(arr[1:2])::text` -- for the immutability reason, not the slice. A feature gap and not a printing one, which is what is left of a group that was four mechanisms wearing one word: the pair on a constructor and a subscript, the immutability refusal, the viewdef rows that were never C's, and this.",
            "pg19_deparse_census.txt:391",
        ),
        // ---- D-default: 4 statements ----
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a fold at plan time destroys a node PostgreSQL keeps and prints.** `lower_cast` folds a literal under a cast into a `Literal::Typed` datum, and `ToText` over a constant folds too, so `((1)::bigint)::text` is one `Datum::Text` by the time anything can print it: the `bigint` is not recoverable, and the `DEFAULT` falls back to the text the user wrote -- rendered by `sqlparser`'s `Display`, which is where the upper-cased `::TEXT` in this node's answer comes from. **Measured with a one-off build that bypassed `reads_back`**: three of these four are unchanged by it, so they never reach the fixpoint guard -- `reprinted_by_pg_get_expr` refuses a `Typed(Text)` top node, which is what the fold leaves. The fourth does change, to `('{1,2}'::integer[])::text`, so the array constant prints as a literal where a real server prints the constructor it kept. **Not a printing rule and not one commit**: ADR 0086 is this fold, `debts-v1.1.md` #30 already carved out the *lossy* case (rounding keeps its `Cast` node because it is not invertible), and closing this means the lossless case keeps its node too -- every literal cast in every statement, through typing, evaluation and the wire. Left declared with its measurement rather than half-closed, and it belongs beside #30 rather than in a deparse group.",
            "pg19_deparse_census.txt:641",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:645",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:649",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:653",
        ),
        // ---- G-viewdef: 23 statements ----
        (
            "SELECT pg_get_viewdef('v_case_simple'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:218",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_ctor'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:357",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_sub'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:373",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_cat'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:409",
        ),
        (
            "SELECT pg_get_viewdef('v_str_nest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:425",
        ),
        (
            "SELECT pg_get_viewdef('v_str_cat'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:441",
        ),
        (
            "SELECT pg_get_viewdef('v_fn_mod'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:122",
        ),
        (
            "SELECT pg_get_viewdef('v_op_mod'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:138",
        ),
        (
            "SELECT pg_get_viewdef('v_fn_abs'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:154",
        ),
        (
            "SELECT pg_get_viewdef('v_op_plus'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:170",
        ),
        (
            "SELECT pg_get_viewdef('v_op_nest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:186",
        ),
        (
            "SELECT pg_get_viewdef('v_case_search'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:202",
        ),
        (
            "SELECT pg_get_viewdef('v_case_fn'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:234",
        ),
        (
            "SELECT pg_get_viewdef('v_case_nest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:250",
        ),
        (
            "SELECT pg_get_viewdef('v_case_null'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:266",
        ),
        (
            "SELECT pg_get_viewdef('v_cast_chain'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:294",
        ),
        (
            "SELECT pg_get_viewdef('v_cast_text'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:310",
        ),
        (
            "SELECT pg_get_viewdef('v_neg_cast'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:326",
        ),
        (
            "SELECT pg_get_viewdef('v_neg_bare'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:342",
        ),
        (
            "SELECT pg_get_viewdef('v_str_len'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:457",
        ),
        (
            "SELECT pg_get_viewdef('v_coalesce'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:605",
        ),
        (
            "SELECT pg_get_viewdef('v_greatest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:621",
        ),
        (
            "SELECT pg_get_viewdef('v_nullif'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Twenty-three of the census's 34 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:637",
        ),
    ],
};

#[test]
fn every_shape_prints_the_way_postgresql_19_prints_it() {
    let checked = parity::replay(
        include_str!("corpus/pg19_deparse_census.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 170,
        "only {checked} statements ran; the census is not being read"
    );
}
