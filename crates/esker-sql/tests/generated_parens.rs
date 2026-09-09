//! **The parentheses `pg_get_expr` adds**, for a generated column and for a default alike.
//!
//! `virtual_column_test#test_schema_dumping` (run 105) failed on exactly one pair. The catalog was
//! identical on both servers and the dump was otherwise complete — every virtual column,
//! `stored: true`, and the nested casts `length((name)::text)` and `upper((name)::text)` as
//! PostgreSQL prints them — but the node wrote `as: "column1 + 1"` where the suite matches
//! `as: "\(column1 \+ 1\)"`.
//!
//! The rule is [`esker_sql::catalog::ExprShape`]'s, which was measured for an *index* expression
//! and asked in only that one place. `pg_attrdef.adbin` holds one expression and
//! `pg_get_expr(adbin, adrelid)` prints it for both kinds of column, so the shape decides both:
//! an operator takes one pair, a call and a value take none. It is answered where the text is
//! taken (`parse::lower`), because by then `'x)'::text` and `f(x)` are the same characters at the
//! ends.
//!
//! Same family as run 102's check-constraint parenthesisation, in the opposite direction: there
//! the node added a pair PostgreSQL omits.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`column_default` is a domain, not `text`.** `information_schema.columns.column_default`
    // is `character_data` on a real server — a domain over `character varying` — and this node
    // declares `text` for it. The rows agree to the character; only the declared type differs, and
    // it is the same family as `sql_identifier` and `name` (b4's type unit). The two rows below
    // that also disagree about the *value* carry the same type difference inside their own reason.
    types: &[],
    answers: &[
        (
            "SELECT 'r', pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1gp'::regclass AND a.attname = 'g_nested'",
            "**The nested pairs, which need a deparser this node does not have.** PostgreSQL \
         parenthesises *every* operator node and not only the top one: `(c1 * 2 + 3)` comes back \
         `((c1 * 2) + 3)` and `(c1 > 0 AND c2 > 0)` comes back `((c1 > 0) AND (c2 > 0))`. This \
         crate stores text and normalises it where the text is taken, so only the outermost pair \
         is knowable there; the rest means storing the tree and printing it back, which changes \
         the index key list too and is its own unit. The top-level pair is the one the suite \
         reads, and it agrees.",
            "pg19_generated_parens.txt:55",
        ),
        (
            "SELECT 'r', pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1gp'::regclass AND a.attname = 'g_and'",
            "**The nested pairs, which need a deparser this node does not have.** PostgreSQL \
         parenthesises *every* operator node and not only the top one: `(c1 * 2 + 3)` comes back \
         `((c1 * 2) + 3)` and `(c1 > 0 AND c2 > 0)` comes back `((c1 > 0) AND (c2 > 0))`. This \
         crate stores text and normalises it where the text is taken, so only the outermost pair \
         is knowable there; the rest means storing the tree and printing it back, which changes \
         the index key list too and is its own unit. The top-level pair is the one the suite \
         reads, and it agrees.",
            "pg19_generated_parens.txt:64",
        ),
        (
            "SELECT 'r', pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1dp'::regclass AND a.attname = 'd_func'",
            "**A literal's coercion inside a call.** `upper('a')` comes back `upper('a'::text)`: the \
         deparser prints the *coerced* argument, and a text function's unknown literal is coerced. \
         The same rule over a **column** already agrees here — `upper((name)::text)`, which is \
         what `virtual_column_test` reads — because a generated column's scalar call is stored \
         deparsed. A default is not, and giving it that treatment is the same unit as the nested \
         pairs above.",
            "pg19_generated_parens.txt:68",
        ),
        (
            "SELECT 'r', pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1dp'::regclass AND a.attname = 'd_nested'",
            "**The nested pairs, which need a deparser this node does not have.** PostgreSQL \
         parenthesises *every* operator node and not only the top one: `(c1 * 2 + 3)` comes back \
         `((c1 * 2) + 3)` and `(c1 > 0 AND c2 > 0)` comes back `((c1 > 0) AND (c2 > 0))`. This \
         crate stores text and normalises it where the text is taken, so only the outermost pair \
         is knowable there; the rest means storing the tree and printing it back, which changes \
         the index key list too and is its own unit. The top-level pair is the one the suite \
         reads, and it agrees.",
            "pg19_generated_parens.txt:70",
        ),
        (
            "SELECT 'r', column_default FROM information_schema.columns WHERE table_name = 'g1dp' AND column_name = 'd_func'",
            "**A literal's coercion inside a call.** `upper('a')` comes back `upper('a'::text)`: the \
         deparser prints the *coerced* argument, and a text function's unknown literal is coerced. \
         The same rule over a **column** already agrees here — `upper((name)::text)`, which is \
         what `virtual_column_test` reads — because a generated column's scalar call is stored \
         deparsed. A default is not, and giving it that treatment is the same unit as the nested \
         pairs above.",
            "pg19_generated_parens.txt:75",
        ),
        (
            "SELECT 'r', column_default FROM information_schema.columns WHERE table_name = 'g1dp' AND column_name = 'd_nested'",
            "**The nested pairs, which need a deparser this node does not have.** PostgreSQL \
         parenthesises *every* operator node and not only the top one: `(c1 * 2 + 3)` comes back \
         `((c1 * 2) + 3)` and `(c1 > 0 AND c2 > 0)` comes back `((c1 > 0) AND (c2 > 0))`. This \
         crate stores text and normalises it where the text is taken, so only the outermost pair \
         is knowable there; the rest means storing the tree and printing it back, which changes \
         the index key list too and is its own unit. The top-level pair is the one the suite \
         reads, and it agrees.",
            "pg19_generated_parens.txt:77",
        ),
    ],
};

#[test]
fn every_printed_expression_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_generated_parens.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 35, "the corpus shrank: {checked} statements");
}
