//! **The deparse census: expression shape × reader.**
//!
//! Two days of reds had one thing in common — they were **wrong and green**. The node printed a
//! stored expression differently from a real server and nothing in this repository said so,
//! because each reader had only ever been measured on the shapes that happened to reach it: a
//! `CASE` through a `CHECK`, a `mod` through an index, a reserved word through two of five
//! printers. Each was found by the Rails suite, one per run.
//!
//! This file is the cross product instead — 42 expression shapes and 10 constants against the
//! eight readers each admits, 160 probes — so that the next parenthesis rule is learned here.
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
//! Three probes record a **refusal** rather than a print: PostgreSQL will not take
//! `(ARRAY[...])::text` in a generated column, because the array-to-text cast reads settings. A
//! reader that refuses is as much a fact as one that prints, so the refusal is the expected answer
//! and the savepoint around it is what keeps the other 157 comparable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Sixty-five statements in eight groups, by mechanism.** Each group's reason is written once
    // and attached to every member: what is worth reading is the mechanism, not sixty-five
    // sentences. Closing a group deletes its entries as a block, which is the point of grouping
    // them — a group that shrinks by one was two mechanisms wearing one name.
    answers: &[
        // ---- A-chain: 6 statements ----
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k_chain_mix'",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:62",
        ),
        (
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index WHERE indexrelid = 'ix_chain_mix'::regclass",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:70",
        ),
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k_chain_nest'",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:74",
        ),
        (
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'k_chain_nest'",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:78",
        ),
        (
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index WHERE indexrelid = 'ix_chain_nest'::regclass",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:82",
        ),
        (
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'k_chain_not'",
            "**Group A -- a boolean chain's grouping is re-derived from flat text and cannot express nesting.** PostgreSQL prints the tree, so precedence shows: `a > 0 AND b > 0 OR flag` is `(((a > 0) AND (b > 0)) OR flag)` and `(a > 0 OR b > 0) AND flag` is `(((a > 0) OR (b > 0)) AND flag)`. This node stores one flat string and `catalog::parenthesised_operands` re-parenthesises it at read time by splitting on the top-level keyword, which has no way to say which operands bind first -- and doubles a pair the deparser already wrote. Closing it means the *writer* emitting the grouping, which is what a tree has and a splitter never will.",
            "pg19_deparse_census.txt:90",
        ),
        // ---- B-between: 9 statements ----
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k_between'",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:493",
        ),
        (
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'k_between'",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:497",
        ),
        (
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index WHERE indexrelid = 'ix_between'::regclass",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:501",
        ),
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'k_not_between'",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:505",
        ),
        (
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'k_not_between'",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:509",
        ),
        (
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index WHERE indexrelid = 'ix_not_between'::regclass",
            "**Group B -- PostgreSQL rewrites three shapes and this node prints them as written.** `a BETWEEN 1 AND 10` is `((a >= 1) AND (a <= 10))` there, `a NOT BETWEEN 1 AND 10` is `((a < 1) OR (a > 10))`, and `a IS NOT DISTINCT FROM b` is `(NOT (a IS DISTINCT FROM b))`. These are not parenthesis differences: the node in the tree is different, because the parser expands them. **And the `AND` inside a `BETWEEN` is not a chain separator** -- the splitter took it for one and produced `CHECK (((a BETWEEN 1) AND (10)))`, which no longer parses. Same defect as the `CASE` one closed in `3b688020`, one keyword over.",
            "pg19_deparse_census.txt:513",
        ),
        (
            "SELECT pg_get_indexdef('ix_arr_ctor'::regclass)",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:354",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_arr_ctor'::regclass",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:358",
        ),
        (
            "ALTER TABLE cen ADD COLUMN g_arr_ctor text GENERATED ALWAYS AS ((ARRAY[a, b])::text) STORED",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:361",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_ctor'::regclass)",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:365",
        ),
        (
            "SELECT pg_get_indexdef('ix_arr_sub'::regclass)",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:369",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_arr_sub'::regclass",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:373",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_sub'::regclass)",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:381",
        ),
        (
            "CREATE INDEX ix_arr_slice ON cen ((arr[1:2]))",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:384",
        ),
        (
            "CREATE INDEX ix_arr_slice ON cen ((arr[1:2]))",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:384",
        ),
        (
            "ALTER TABLE cen ADD COLUMN g_arr_slice text GENERATED ALWAYS AS ((arr[1:2])::text) STORED",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:396",
        ),
        (
            "CREATE VIEW v_arr_slice AS SELECT arr[1:2] AS x FROM cen",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:399",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_arr_cat'::regclass",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:410",
        ),
        (
            "ALTER TABLE cen ADD COLUMN g_arr_cat text GENERATED ALWAYS AS ((arr || ARRAY[a])::text) STORED",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:413",
        ),
        (
            "SELECT pg_get_viewdef('v_arr_cat'::regclass)",
            "**Group C -- arrays, and three separate things behind one word.** A subscript takes one pair and this node writes two (`((arr[1]))` against `(arr[1])`); an array **slice** is `0A000` here; and a generated column over `(ARRAY[...])::text` is *accepted* here and **refused** by PostgreSQL -- `generation expression is not immutable`, because the array-to-text cast reads settings. The last is the interesting direction: this node is more permissive than the server it copies, which no corpus had asked about.",
            "pg19_deparse_census.txt:417",
        ),
        // ---- D-default: 4 statements ----
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:649",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:649",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:649",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cend'::regclass AND a.attname='t'",
            "**Group D -- a `DEFAULT` keeps the text it was written with, type name and all.** `(1)::bigint` comes back `(1)::BIGINT`, the parser's own rendering upper-cased, and `-1` comes back `(-1)` where a real server prints `('-1'::integer)`. The deparser has the rule (`numeric_constant`, `debts-v1.1.md` #24 closed it) and the `DEFAULT` path does not reach it for these shapes: `reprinted_by_pg_get_expr` is an allow-list and a cast over a constant is not on it.",
            "pg19_deparse_census.txt:649",
        ),
        // ---- E-nocast: 4 statements ----
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cen'::regclass AND a.attname='g_str_nest'",
            "**Group E -- a no-op cast is elided by PostgreSQL and kept here.** `GENERATED ALWAYS AS ((upper(btrim(t)))::text)` is stored as `upper(btrim(t))` there: the expression is already `text`, so the coercion never becomes a node. This node keeps `(upper(btrim(t)))::text`. One rule -- a cast to the type the operand already has is not a node -- and it belongs where the cast is resolved, not where it is printed.",
            "pg19_deparse_census.txt:429",
        ),
        (
            "SELECT pg_get_viewdef('v_str_nest'::regclass)",
            "**Group E -- a no-op cast is elided by PostgreSQL and kept here.** `GENERATED ALWAYS AS ((upper(btrim(t)))::text)` is stored as `upper(btrim(t))` there: the expression is already `text`, so the coercion never becomes a node. This node keeps `(upper(btrim(t)))::text`. One rule -- a cast to the type the operand already has is not a node -- and it belongs where the cast is resolved, not where it is printed.",
            "pg19_deparse_census.txt:433",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cen'::regclass AND a.attname='g_str_cat'",
            "**Group E -- a no-op cast is elided by PostgreSQL and kept here.** `GENERATED ALWAYS AS ((upper(btrim(t)))::text)` is stored as `upper(btrim(t))` there: the expression is already `text`, so the coercion never becomes a node. This node keeps `(upper(btrim(t)))::text`. One rule -- a cast to the type the operand already has is not a node -- and it belongs where the cast is resolved, not where it is printed.",
            "pg19_deparse_census.txt:445",
        ),
        (
            "SELECT pg_get_viewdef('v_str_cat'::regclass)",
            "**Group E -- a no-op cast is elided by PostgreSQL and kept here.** `GENERATED ALWAYS AS ((upper(btrim(t)))::text)` is stored as `upper(btrim(t))` there: the expression is already `text`, so the coercion never becomes a node. This node keeps `(upper(btrim(t)))::text`. One rule -- a cast to the type the operand already has is not a node -- and it belongs where the cast is resolved, not where it is printed.",
            "pg19_deparse_census.txt:449",
        ),
        // ---- F-case: 4 statements ----
        (
            "CREATE INDEX ix_case_simple ON cen ((CASE a WHEN 1 THEN b ELSE 0 END))",
            "**Group F -- the simple `CASE`.** `CASE a WHEN 1 THEN b ELSE 0 END` is `0A000 CASE <expression> WHEN ..., the simple form is not supported`. A feature gap rather than a printing one, named here because the census is where a reader will look for it. PostgreSQL prints it as the searched form it is equivalent to, so implementing it as one would close the printing half for free.",
            "pg19_deparse_census.txt:205",
        ),
        (
            "CREATE INDEX ix_case_simple ON cen ((CASE a WHEN 1 THEN b ELSE 0 END))",
            "**Group F -- the simple `CASE`.** `CASE a WHEN 1 THEN b ELSE 0 END` is `0A000 CASE <expression> WHEN ..., the simple form is not supported`. A feature gap rather than a printing one, named here because the census is where a reader will look for it. PostgreSQL prints it as the searched form it is equivalent to, so implementing it as one would close the printing half for free.",
            "pg19_deparse_census.txt:205",
        ),
        (
            "ALTER TABLE cen ADD COLUMN g_case_simple text GENERATED ALWAYS AS ((CASE a WHEN 1 THEN b ELSE 0 END)::text) STORED",
            "**Group F -- the simple `CASE`.** `CASE a WHEN 1 THEN b ELSE 0 END` is `0A000 CASE <expression> WHEN ..., the simple form is not supported`. A feature gap rather than a printing one, named here because the census is where a reader will look for it. PostgreSQL prints it as the searched form it is equivalent to, so implementing it as one would close the printing half for free.",
            "pg19_deparse_census.txt:217",
        ),
        (
            "CREATE VIEW v_case_simple AS SELECT CASE a WHEN 1 THEN b ELSE 0 END AS x FROM cen",
            "**Group F -- the simple `CASE`.** `CASE a WHEN 1 THEN b ELSE 0 END` is `0A000 CASE <expression> WHEN ..., the simple form is not supported`. A feature gap rather than a printing one, named here because the census is where a reader will look for it. PostgreSQL prints it as the searched form it is equivalent to, so implementing it as one would close the printing half for free.",
            "pg19_deparse_census.txt:223",
        ),
        // ---- G-viewdef: 17 statements ----
        (
            "SELECT pg_get_viewdef('v_fn_mod'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:122",
        ),
        (
            "SELECT pg_get_viewdef('v_op_mod'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:138",
        ),
        (
            "SELECT pg_get_viewdef('v_fn_abs'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:154",
        ),
        (
            "SELECT pg_get_viewdef('v_op_plus'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:170",
        ),
        (
            "SELECT pg_get_viewdef('v_op_nest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:186",
        ),
        (
            "SELECT pg_get_viewdef('v_case_search'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:202",
        ),
        (
            "SELECT pg_get_viewdef('v_case_fn'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:242",
        ),
        (
            "SELECT pg_get_viewdef('v_case_nest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:258",
        ),
        (
            "SELECT pg_get_viewdef('v_case_null'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:274",
        ),
        (
            "SELECT pg_get_viewdef('v_cast_chain'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:302",
        ),
        (
            "SELECT pg_get_viewdef('v_cast_text'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:318",
        ),
        (
            "SELECT pg_get_viewdef('v_neg_cast'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:334",
        ),
        (
            "SELECT pg_get_viewdef('v_neg_bare'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:350",
        ),
        (
            "SELECT pg_get_viewdef('v_str_len'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:465",
        ),
        (
            "SELECT pg_get_viewdef('v_coalesce'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:613",
        ),
        (
            "SELECT pg_get_viewdef('v_greatest'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:629",
        ),
        (
            "SELECT pg_get_viewdef('v_nullif'::regclass)",
            "**Group G -- `pg_get_viewdef` returns the stored text where a real server reconstructs the query.** The standing divergence recorded on `plan::CatalogFunc::PgGetViewdef` and in `tests/view_debts.rs`: re-cased, re-qualified, re-indented, every expression parenthesised and a semicolon on the end is `ruleutils.c` rather than a function. Seventeen of the census's 65 are this one divergence seen once per shape, which is what a cross product does to a standing difference -- they are listed rather than folded so that the *shapes* stay countable.",
            "pg19_deparse_census.txt:645",
        ),
        // ---- H-case-layout: 7 statements ----
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_case_search'::regclass",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:194",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_case_fn'::regclass",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:234",
        ),
        (
            "SELECT pg_get_indexdef('ix_case_nest'::regclass)",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:246",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_case_nest'::regclass",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:250",
        ),
        (
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='cen'::regclass AND a.attname='g_case_nest'",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:254",
        ),
        (
            "SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 'ix_case_null'::regclass",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:266",
        ),
        (
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index WHERE indexrelid = 'ix_case_bool'::regclass",
            "**Group H -- a `CASE`'s layout, and the leading newline belongs to the reader.** Three facts, measured: PostgreSQL's `pg_get_expr(indexprs)` gives `CASE\\n    WHEN ...` with **no** leading newline while the `CHECK` printer adds one (`CHECK (\\nCASE`), so the newline this node stores in the expression is the printer's and not the expression's; a **nested** `CASE` goes on its own line after `THEN` and is indented four further spaces, where this node inlines it at the same depth; and `pg_get_expr(indpred)` gives a bare `CASE ... END` where this node wraps it in a pair. All three are the same mistake in opposite directions -- the layout is split between the writer and the reader, and this node put all of it in the writer.",
            "pg19_deparse_census.txt:286",
        ),
        (
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'k_notdistinct'",
            "**Group H's shape, reached by group B's fix.** Now that `IS NOT DISTINCT FROM` prints as the negation PostgreSQL holds, the *pretty* form has a pair to elide and this node has none of that machinery: `CHECK (NOT a IS DISTINCT FROM b)` there against `CHECK (NOT (a IS DISTINCT FROM b))` here. It is **not** \"strip the pair after NOT\" — the census's `chain_not` row has PostgreSQL *keeping* it, `NOT (a > 0 AND flag)`, because there the operand is a chain and needs it. Which pairs are redundant is a precedence question, which is exactly what group A says a flat string cannot answer; this row closes with A and H.",
            "pg19_deparse_census.txt:569",
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
        checked > 150,
        "only {checked} statements ran; the census is not being read"
    );
}
