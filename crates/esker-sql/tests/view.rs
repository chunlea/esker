//! `CREATE VIEW` / `DROP VIEW` — run 53's view row, 21 tests in `view_test.rb`.
//!
//! **A view is a stored derived table.** `FROM v` becomes `FROM (<definition>) AS v` before
//! anything plans it, which is the rewrite `crate::plan::cte` already performs for a `WITH` item —
//! the difference is only that the text comes from the catalog. So a view needs no plan node, no
//! access path and no read of its own, and nothing below the expansion can tell one from a
//! sub-select somebody typed.
//!
//! The suite's view is deliberately named `ebooks'`, with an apostrophe, and that is the point of
//! the file: the name has to survive quoting, the catalog and every message that quotes it back.
//! It is the first thing `setup` does, so it gates all 21.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table and views.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`name` and `"char"` there, `text` here** — PostgreSQL's identifier and single-byte
    // types, which compare identically and are the standing choice every catalog view in this
    // crate makes (`tests/coalesce.rs` declares the same fact about the same columns).
    types: &[
        r"SELECT 'r', relname, relkind FROM pg_class WHERE relname = 'ebooks'''",
        r"SELECT 'r', viewname, definition FROM pg_views WHERE viewname = 'ebooks'''",
    ],
    answers: &[
        (
            r"SELECT 'r', viewname, definition FROM pg_views WHERE viewname = 'ebooks'''",
            "**PostgreSQL prints a view's definition through its own renderer**, not the text the \
             user wrote: one column per line, a two-space continuation indent and a leading space. \
             This node stores the statement as `sqlparser` renders it back and hands that over, so \
             the definition agrees and its *formatting* does not. Reproducing `pg_get_viewdef`'s \
             layout is a pretty-printer for the whole expression language — a unit of its own, and \
             one that buys formatting rather than meaning.",
        ),
        (
            r#"SELECT 'r', pg_get_viewdef('"ebooks''"'::regclass, true)"#,
            "The same renderer as a function, refused by name until there is one. The schema \
             dumper is what reads it, and a dumper fed a differently-formatted body would emit a \
             file that round-trips and still does not compare equal — so answering with this \
             node's own text would be worse than not answering.",
        ),
        (
            "REFRESH MATERIALIZED VIEW ebooks_mat",
            "**A materialized view is a different feature.** It holds its own rows — storage \
             rather than a rewrite — and `REFRESH` is the statement that rewrites them; this \
             node's views are stored `SELECT`s expanded where they are read, so there is nothing \
             to refresh. `CREATE`/`DROP MATERIALIZED VIEW` and `pg_matviews` are refused by name \
             beside it, and `view_test.rb:218-222` is the part of the file that needs them.",
        ),
    ],
};

#[test]
fn every_view_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_view.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
