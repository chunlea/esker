//! `pg_trgm` and the **operator class** surface, against PostgreSQL 19beta1.
//!
//! Run 70's extension row and `schema_test.rb`'s two opclass tests. The suite uses none of
//! `pg_trgm`'s *functions*: what it needs is `CREATE INDEX … USING gin(position gin_trgm_ops)`
//! accepted and the class readable back by the schema dumper — and `text_pattern_ops`, the sibling
//! test one method up, needs the same machinery without the extension.
//!
//! [ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md):
//! the access method and the class are **recorded**, the index underneath is the ordinary ordered
//! one, and nothing claims a trigram search is accelerated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus installs its own extension and makes its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // The standing catalog trade: `opcname`, `amname` and `typname` are `name` on a real
        // server and `text` here. It is listed for its **rows** below, not for this.
        "SELECT 'r', o.opcname, o.opcdefault, am.amname, t.typname FROM pg_opclass o JOIN pg_am \
         am ON am.oid = o.opcmethod JOIN pg_type t ON t.oid = o.opcintype WHERE o.opcname IN \
         ('gin_trgm_ops','gist_trgm_ops','text_pattern_ops','varchar_pattern_ops') ORDER BY \
         o.opcname, am.amname",
        "SELECT 'r', amname, amtype FROM pg_am WHERE amname IN ('btree','gin','gist') ORDER BY \
         amname",
        "SELECT 'r', extname, extversion FROM pg_extension WHERE extname = 'pg_trgm'",
        "SELECT 'r', name, default_version FROM pg_available_extensions WHERE name = 'pg_trgm'",
        "SELECT 'r', c.relname, am.amname FROM pg_class c JOIN pg_am am ON am.oid = c.relam \
         WHERE c.relname LIKE 'trains_%' ORDER BY 1",
    ],
    answers: &[
        // **Four rows here and six there**, and the two that are missing are the `hash` halves of
        // `text_pattern_ops` and `varchar_pattern_ops`. `USING hash` is refused
        // ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)),
        // so a class for it would be a row nothing could name — the rule `pg_am` states one view
        // over: a class nothing can be declared with is a claim rather than a report. The four
        // that are here agree column for column, `opcdefault = f` included.
        (
            "SELECT 'r', o.opcname, o.opcdefault, am.amname, t.typname FROM pg_opclass o JOIN pg_am am ON am.oid = o.opcmethod JOIN pg_type t ON t.oid = o.opcintype WHERE o.opcname IN ('gin_trgm_ops','gist_trgm_ops','text_pattern_ops','varchar_pattern_ops') ORDER BY o.opcname, am.amname",
            "the hash halves are absent because USING hash is refused",
        ),
        // **`position` is a keyword and a real server quotes it.** Pre-existing and declared where
        // it lives (`catalog::pg_index::quote_identifier`): PostgreSQL quotes every non-unreserved
        // keyword and this node carries no such list, so `"position"` comes back bare. The
        // *operator class* is what these two statements are here for and it is byte-identical —
        // `gin_trgm_ops` after the column, `text_pattern_ops` after the second of two. Closing it
        // means carrying `pg_get_keywords()`'s categories, which is a unit of its own and would
        // move every index definition in every corpus.
        (
            "SELECT 'r', pg_get_indexdef('trains_position'::regclass)",
            "a keyword column name is quoted there and bare here — quote_identifier's declared gap",
        ),
        (
            "SELECT 'r', pg_get_indexdef('trains_np'::regclass)",
            "a keyword column name is quoted there and bare here — quote_identifier's declared gap",
        ),
        // **`indclass` is an `oidvector` and this node has no such type**, so the statement that
        // unnests it needs a `LATERAL … WITH ORDINALITY` FROM item this node does not have either.
        // Two gaps in one line, neither of them the operator class's: `pg_get_indexdef` above
        // carries the same information and agrees, and it is what `ActiveRecord`'s schema dumper
        // actually reads.
        (
            "SELECT 'r', i.indexrelid::regclass::text, o.opcname FROM pg_index i JOIN LATERAL unnest(i.indclass::oid[]) WITH ORDINALITY AS u(cls, n) ON true JOIN pg_opclass o ON o.oid = u.cls WHERE i.indrelid = 'trains'::regclass ORDER BY 1, u.n",
            "an oidvector and a LATERAL WITH ORDINALITY, neither of them the operator class",
        ),
    ],
};

#[test]
fn every_pg_trgm_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_pg_trgm.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
