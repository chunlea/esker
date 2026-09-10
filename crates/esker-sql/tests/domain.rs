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

/// **The query `ActiveRecord` loads its type map with, verbatim, returning the five
/// `information_schema` domains** — `debts-v1.1.md` #37, family two.
///
/// It runs on **every connection** — 712 occurrences across 164 captured files — and it is the
/// constraint the rest of this row has to meet: **an oid this node sends that does not come back
/// from here has no decoder, and a correct value arrives as a string.** So these rows come first
/// and the wire follows them, never the other way round
/// ([ADR 0103](../../../docs/adr/0103-a-domain-is-a-type-a-client-can-be-sent.md)).
///
/// Measured on 19beta1, 2026-09-10 (`tests/captures/pg19_domain_type.txt`). Three of these are
/// not guessable:
///
/// * **`typinput` is `domain_in`** for all five, not the base's input function;
/// * **the array oid is the domain's minus one**, because `initdb` allocates the array first —
///   where this node's own convention for a user type is `oid + 1`;
/// * **`time_stamp` carries a default**, `CURRENT_TIMESTAMP(2)`, and the other four do not.
#[test]
fn the_type_map_query_returns_the_information_schema_domains() {
    let mut node = parity::Node::new(&[]);
    // **The query verbatim, and then the domains taken out of what it returned.** It asks for
    // three kinds and this node answers six ranges beside the five domains — which is the point of
    // the one query and not a complication: `ActiveRecord` learns every range, enum and domain the
    // server has in a single load. The ranges belong to their own row and are not pinned here.
    let answered = node.rows(
        "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, \
         t.typtype, t.typbasetype \
         FROM pg_type as t LEFT JOIN pg_range as r ON oid = rngtypid \
         WHERE t.typtype IN ('r', 'e', 'd') ORDER BY t.typname",
    );
    let domains: Vec<Vec<String>> = answered
        .into_iter()
        .filter(|row| row.get(6).is_some_and(|typtype| typtype == "d"))
        .collect();
    assert_eq!(
        domains,
        vec![
            row("13356", "cardinal_number", "23"),
            row("13359", "character_data", "1043"),
            row("13361", "sql_identifier", "19"),
            row("13367", "time_stamp", "1184"),
            row("13369", "yes_or_no", "1043"),
        ]
    );
}

/// One expected row of the type-map query: the four columns that vary, and the four that do not.
fn row(oid: &str, name: &str, base: &str) -> Vec<String> {
    vec![
        oid.to_owned(),
        name.to_owned(),
        // `typelem` 0 and `rngsubtype` NULL: a domain over a scalar is not an array and not a
        // range, however its base prints. A NULL comes back from the harness as `\N`, which is the
        // corpus format's own spelling and not an empty string.
        "0".to_owned(),
        ",".to_owned(),
        "domain_in".to_owned(),
        "\\N".to_owned(),
        "d".to_owned(),
        base.to_owned(),
    ]
}

/// **A domain's array is reachable, and is not itself a domain** — which is why the query above
/// does not return it and does not need to.
///
/// Measured: `_sql_identifier` is `typtype = 'b'` with `typelem` pointing at the domain, so
/// `ActiveRecord` finds it through the domain's own row. The array oid is the domain's **minus
/// one**.
#[test]
fn a_domains_array_is_a_base_type_that_points_back_at_it() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT typname, typtype, typelem, typinput FROM pg_type \
             WHERE typname IN ('_sql_identifier', '_time_stamp') ORDER BY typname"
        ),
        vec![
            vec![
                "_sql_identifier".to_owned(),
                "b".to_owned(),
                "13361".to_owned(),
                "array_in".to_owned(),
            ],
            vec![
                "_time_stamp".to_owned(),
                "b".to_owned(),
                "13367".to_owned(),
                "array_in".to_owned(),
            ],
        ]
    );
}
