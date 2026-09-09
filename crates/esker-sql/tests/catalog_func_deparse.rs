//! **How a call to a catalog function is printed**, and the six readers that print one.
//!
//! `docs/plans/debts-v1.1.md` #25. `exec::ddl::deparse`'s `CatalogFunc` arm was a `name(...)`
//! placeholder that `exec::ddl::reads_back` refused to store, so every stored expression over a
//! catalog function kept its **written** text — right in the sense that it re-parsed and computed
//! the right value, and wrong in every character PostgreSQL would have printed. This file is the
//! measurement that landed first and the assertion that the arm now prints what was measured:
//! **29 declared divergences when the corpus landed, 3 after**.
//!
//! The corpus header states the rule beside the rows that measure it. In short: four functions are
//! grammar productions and print upper-cased (`COALESCE`, `GREATEST`, `LEAST`, `NULLIF`), a fifth
//! prints as the production or as a **quoted** call depending on how it was written
//! (`SUBSTRING(t FROM 1 FOR 2)` against `"substring"(t, 1, 2)`), everything else prints its own
//! lower-case name, a literal argument carries the coercion its *parameter* took — which is not
//! always `text` (`setweight(tv, 'A'::"char")`, `to_tsvector('english'::regconfig, t)`) — and for
//! the four productions the coercion is to the call's **common type** instead.
//!
//! The three that remain are not rules this node has yet to measure. Two name a **type it does not
//! have** — `regconfig` for `to_tsvector`'s configuration and PostgreSQL's one-byte `"char"` for
//! `setweight`'s weight — so there is no `ColumnType` to thread down for the cast, and inventing
//! the text would put a coercion in the catalog that nothing here can re-parse. The third is
//! `substring`'s two spellings, of which this node keeps one. A fourth reader, a partial index's
//! **predicate**, is deliberately not routed through the deparser; the entry says what it costs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', a.attname, replace(pg_get_expr(d.adbin, d.adrelid), '|', '!') FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1cf'::regclass AND a.attname = 'f_substring_f'",
            "**`substring` has two spellings and this node keeps one.** PostgreSQL prints the keyword form as the production -- `substring(t from 1 for 2)` is `SUBSTRING(t FROM 1 FOR 2)` -- and the call form as a *quoted call*, `\"substring\"(t, 1, 2)`, because `substring` is a reserved word. `parse::lower` reads both into `CatalogFunc::Substring` (the flag the parse tree carries separates `substr` from `substring`, not the call form from the keyword one), so this node prints the production for both and the call spelling differs. The keyword form is the half that reads back: this node's parser has no quoted function names, so `\"substring\"(t, 1, 2)` would be stored and then fail to re-parse -- which is why the production is what prints. Closing it means keeping which spelling was written, one bit in the plan.",
            "pg19_catalog_func_deparse.txt:147",
        ),
        (
            "SELECT 'r', a.attname, replace(pg_get_expr(d.adbin, d.adrelid), '|', '!') FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1cf'::regclass AND a.attname = 'f_tsvector'",
            "**No `regconfig` type, so the argument shows no coercion.** PostgreSQL prints `to_tsvector('english'::regconfig, t)` and this node keeps `to_tsvector('english', t)` -- the value and the answer are identical, and there is no `ColumnType` to thread down for the cast. `exec::ddl::catalog_parameter_types` leaves the function out for exactly that reason and says so: printing an invented coercion would be worse than keeping the written text, because it might read back and then the catalog would carry a cast this node made up. `regconfig` is a one-value type here (`ADR 0076`'s shape, `english` and nothing else), so the type surface is where this closes.",
            "pg19_catalog_func_deparse.txt:159",
        ),
        (
            "SELECT 'r', a.attname, replace(pg_get_expr(d.adbin, d.adrelid), '|', '!') FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1cf'::regclass AND a.attname = 'f_setweight'",
            "**No `\"char\"` type, so the argument shows no coercion.** PostgreSQL prints `setweight(tv, 'A'::\"char\")` -- the quoted one-byte type, which is *not* `char(n)` -- and this node keeps `setweight(tv, 'A')`. Same shape as `to_tsvector`'s `regconfig` row and the same reason: `exec::ddl::catalog_parameter_types` has no `ColumnType` whose name prints `\"char\"`, and inventing the text would put a cast in the catalog that nothing here can re-parse. The node's `char` keyword maps to `bpchar` (`value::COLUMN_TYPE_NAMES`), which is a different type with a different oid.",
            "pg19_catalog_func_deparse.txt:170",
        ),
        (
            "SELECT 'r', replace(pg_get_expr(x.indpred, x.indrelid), '|', '!') FROM pg_index x WHERE x.indexrelid = 'g1cf_ix3'::regclass",
            "**A partial index's predicate is stored as written, so its literals show no coercion.** `(btrim(t, 'x'::text) = 'y'::text)` there, `(btrim(t, 'x') = 'y')` here. It is the sixth reader of the deparse rule and the one this unit did **not** route through it, after measuring what it costs: `pg_get_expr(indpred)` re-parenthesises a boolean chain's operands at read time (`catalog::parenthesised_operands`), so deparsing the chain adds a pair the reader then doubles -- `(((n > 0)) AND flag)`, caught by `tests/index_deparse.rs` -- and the `ON CONFLICT` arbiter matches an index by its predicate **text**, quotes included (`exec::dml::same_predicate`), so a deparsed predicate stops matching the `WHERE \"b\" IS NOT NULL` `ActiveRecord` writes and the statement becomes `42P10`. Both measured by making the change and reverting it; `docs/plans/debts-v1.1.md` has the row. The `CHECK` beside it *is* routed, because its reader has only the first of those two problems and a chain can be skipped.",
            "pg19_catalog_func_deparse.txt:200",
        ),
    ],
};

#[test]
fn every_printed_catalog_call_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_func_deparse.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 30, "the corpus shrank: {checked} statements");
}
