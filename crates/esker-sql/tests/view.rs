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
        // **The same three facts about three more statements**, comparable only since the write
        // through a view stopped aborting the block: `character varying(3)` for the
        // `information_schema` flags, and `name` / `"char"` for the `pg_catalog` ones. The values
        // agree in every case; it is the declared type that differs.
        "SELECT 'r', is_updatable, is_insertable_into FROM information_schema.views WHERE table_name = 'ebooks_distinct'",
        "SELECT 'r', relname, relkind FROM pg_class WHERE relname = 'ebooks_mat'",
        "SELECT 'r', matviewname FROM pg_matviews WHERE matviewname = 'ebooks_mat'",
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
        // **Writing through a view is no longer a divergence** — the entry that stood here said
        // "its own unit", and this is that unit. What follows is what closing it *surfaced*: five
        // statements the aborted transaction had been swallowing, none of them about writing
        // through a view, and each recorded as a gap rather than a choice.
        (
            "SELECT 'r', pg_get_viewdef('ebooks_plain'::regclass, true)",
            "The **formatting** divergence two entries above, reached through a third spelling: \
             PostgreSQL prints a view's body through its own renderer, one column per line. Only \
             comparable at all now that the insert before it stopped aborting the block.",
            "pg19_view.txt:72",
        ),
        // **Both `CREATE OR REPLACE VIEW` entries are gone**, because the rule they recorded as a
        // gap is implemented: a replacement may append columns and may not rename or drop one,
        // `42P16` either way. Rule 2 would fail this file for leaving them.
        (
            "DROP TABLE books",
            "Two differences in one dependency message, both in the `DETAIL` and neither about \
             views being writable. PostgreSQL **quotes** a dependent's name when it needs quoting \
             — `view \"ebooks'\"` against this node's `view ebooks'` — and where several views \
             depend on the table it names a different one of them. Both were hidden behind the \
             aborted block until now.",
            "pg19_view.txt:86",
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
