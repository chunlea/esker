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
        // `is_updatable` and `is_insertable_into` are `character varying(3)` in the standard and
        // `text` here — the standing `information_schema` trade, with `YES`/`NO` identical.
        "SELECT 'r', table_name, is_updatable FROM information_schema.views WHERE table_name = 'ebooks'''",
        "SELECT 'r', is_updatable, is_insertable_into FROM information_schema.views WHERE table_name = 'ebooks_plain'",
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
            "pg19_view.txt:44",
        ),
        (
            r#"SELECT 'r', pg_get_viewdef('"ebooks''"'::regclass, true)"#,
            "The same renderer as a function. It **answers** now, with the stored text — the same \
             text `pg_views.definition` gives, and the same formatting divergence. The earlier \
             argument here was that answering would be worse than not answering, because a schema \
             dumper fed a differently-formatted body emits a file that round-trips and still does \
             not compare equal. That is true of the *formatting* either way; what refusing added \
             was an error where PostgreSQL has a value, which aborted the block and hid every \
             statement after it. Two of those are now measured and one was a real gap.",
            "pg19_view.txt:45",
        ),
        // **Un-swallowed by the line above.** `information_schema.views` did not exist, so the
        // statement before this one aborted the transaction and this was never compared. It is a
        // pre-existing gap and not a view-formatting one: an `INSERT` through an automatically
        // updatable view is not implemented, so the insert path does not know the name.
        (
            "INSERT INTO ebooks_plain (name, cover, status, format) VALUES ('Written Through', \
             'hard', 0, 'ebook')",
            "`42P01`: writing **through** a view is its own feature. `is_updatable` now answers \
             `YES` for this view, which is the right answer about the *query* — PostgreSQL would \
             accept the insert and this node does not. Its own unit; the read side is complete.",
            "pg19_view.txt:58",
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
