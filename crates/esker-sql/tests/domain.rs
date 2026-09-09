//! `CREATE DOMAIN` / `DROP DOMAIN` — run 70's row, 8 tests over two files.
//!
//! **A domain is a name and a constraint over a base type**
//! ([ADR 0065](../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)), so
//! it is a fourth `TypeKind` beside the range, the composite and the enum, and everything
//! downstream — the record, `pg_type`, the drop, the dependency edge — is the path all four take.
//!
//! `domain_test.rb` asks the question that decides the shape: a `custom_money` column over
//! `numeric(8,2)` must report `column.type` **`:decimal`** and `column.sql_type`
//! **`"custom_money"`** at once. The value is the base type's and the name is the domain's, and a
//! node that answered one of those for both would pass half the file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own domain and table.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The declared types this entry named all agree now** — `name` (ADR 0084) and `"char"`
    // (ADR 0095) — and it stays because the same statement is an `answers` divergence too: the
    // harness reads a `types` entry only once the *rows* agree, so this one is not read at all
    // and will be deleted with the answer it shadows.
    types: &[
        "SELECT 'r', n.nspname, t.typname, t.typtype FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = 'text' ORDER BY n.nspname",
        "SELECT 'r', format_type(a.atttypid, a.atttypmod), t.typtype FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid WHERE a.attrelid = 'dm_shadow'::regclass AND a.attname = 'c'",
    ],
    answers: &[
        (
            "SELECT 'r', n.nspname, t.typname, t.typtype FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = 'text' ORDER BY n.nspname",
            "**The domain's row agrees; the built-in's schema does not.** `dm_s|text|d` is \
             identical, which is the half this unit is about — a domain is reported in the schema \
             it was created in. The other row is `pg_catalog|text|b` there and `public|text|b` \
             here, which is the standing difference in the **schema model** rather than anything \
             about domains: this node has one schema for its built-in types and a real server puts \
             them in `pg_catalog`. `pg_type`'s `typnamespace` column already carries that note.",
            "pg19_domain.txt:38",
        ),
        (
            "SELECT 'r', format_type(a.atttypid, a.atttypmod), t.typtype FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid WHERE a.attrelid = 'dm_shadow'::regclass AND a.attname = 'c'",
            "**A domain does not shadow a built-in type's name.** With `search_path = dm_s, \
             pg_catalog` and a domain `dm_s.text`, a column declared `text` is the *domain* on a \
             real server (`typtype` `d`) and the built-in here (`b`). The name is resolved as a \
             type before the catalog is consulted at all — `crate::exec::ddl::resolve_user_type` \
             is reached only for a name lowering could not read — so shadowing needs type \
             resolution to walk the `search_path` ahead of the built-in vocabulary, which is a \
             unit of its own and not one `schema_test.rb` needs: measured, that file raises this \
             shape **zero** times now and `format_type` agrees on the spelling either way.",
            "pg19_domain.txt:41",
        ),
    ],
};

#[test]
fn every_domain_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_domain.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
