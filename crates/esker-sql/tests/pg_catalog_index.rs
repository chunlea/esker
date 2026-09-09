//! Contract C3 for `pg_index` and `pg_get_indexdef` — phase 13 unit 2.
//!
//! `ActiveRecord`'s `indexes()` and `index_name_exists?`, and the half of `primary_keys()` this
//! node can reach. The corpus builds three tables — one with a single-column key and two indexes,
//! one with a two-column key, one with no key at all — and puts the same statements to both
//! servers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `indexrelid` and `indrelid` are `oid`s on a real server and `bigint` here. `indkey` is an
    // `int2vector` on both now, and the two lines that read only it left this list; the ones
    // below still name a relation. `indnatts` is an `int2` on both.
    // Every *value* is identical — the harness only reaches this list when the rows agree — and
    // `indkey`'s characters are the ones `ActiveRecord` splits on.
    types: &[],
    // **Empty.** The one entry read `pg_typeof(indkey), …` and its reason — "`pg_typeof` is a
    // function this node does not have" — had been false for a long time; what was left was the
    // function reading the datum, and it answers `int2vector` since ADR 0093.
    answers: &[],
};

#[test]
fn every_index_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `indisvalid` is the index's **schema state**, and an index mid-build says so.
///
/// The column a client checks before trusting an index — `ActiveRecord`'s `indexes()` selects it —
/// and the one place ADR 0020's states are visible to a client at all. An index that is not
/// `Public` is one no reader may use, so reporting `t` for it would be a wrong answer in the one
/// place a client asked the right question. Not in the corpus: `CREATE INDEX CONCURRENTLY` leaves
/// a real server's index valid a moment later, and what is pinned here is this node's own rule.
#[test]
fn a_half_built_index_is_not_valid() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE iv (id int8 PRIMARY KEY, a int4)",
        "INSERT INTO iv VALUES (1, 10), (2, 20)",
    ]);

    // Staged rather than driven, because a build the statement finishes has nothing half-built
    // to report: `wait` is the boot value and PostgreSQL's contract (`tests/invalid_index.rs`),
    // and `stage` is what leaves an index between states for this to read.
    node.run("SET esker.concurrent_index_build = 'stage'")
        .unwrap();
    node.run("CREATE INDEX CONCURRENTLY iv_a_idx ON iv (a)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT i.relname, x.indisvalid FROM pg_index x JOIN pg_class i \
             ON i.oid = x.indexrelid WHERE i.relname = 'iv_a_idx'"
        ),
        vec![vec!["iv_a_idx", "f"]],
        "an index in delete-only is not one a reader may use"
    );

    // The primary key, beside it, is valid — and is in `pg_index` at all, which is the fact
    // `primary_keys()` and every `ORDER BY` over a dumped schema depend on.
    assert_eq!(
        node.rows(
            "SELECT i.relname, x.indisprimary, x.indisvalid FROM pg_index x JOIN pg_class i \
             ON i.oid = x.indexrelid WHERE i.relname = 'iv_pkey'"
        ),
        vec![vec!["iv_pkey", "t", "t"]]
    );
}

/// `pg_get_indexdef` over an oid that is not an index answers **NULL**, not an error.
///
/// Measured, and it is the case a caller is most likely to hit: `pg_get_indexdef('ia'::regclass)`
/// hands it a *table*. A node that raised would turn a schema dump over a mixed list of oids into
/// a failure rather than a NULL the client already handles.
#[test]
fn an_oid_that_is_not_an_index_is_null() {
    let mut node = parity::Node::new(&["CREATE TABLE ni (id int8 PRIMARY KEY, a int4)"]);
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('ni'::regclass), pg_get_indexdef(999999)"),
        vec![vec!["\\N", "\\N"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('ni_pkey'::regclass)"),
        vec![vec![
            "CREATE UNIQUE INDEX ni_pkey ON public.ni USING btree (id)"
        ]]
    );
}
