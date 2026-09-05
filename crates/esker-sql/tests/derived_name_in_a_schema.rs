//! **A derived name is derived from the bare identifier, and re-qualified.**
//!
//! A stored relation name is bare in `public` and `schema ++ NUL ++ name` anywhere else
//! ([ADR 0071](../../../docs/adr/0071-a-relation-name-is-keyed-by-its-schema.md)), so a name path
//! that mishandles the schema is **invisible in `public`** — the qualified form and the bare form
//! are the same string there. That is why this file exists and why every case in it is in a schema
//! of its own.
//!
//! What it guards: `plan::make_object_name` spends a 63-byte budget, and PostgreSQL spends that
//! budget on the **identifier**, never on the schema in front of it. Handing it a qualified name
//! made the schema and its separator eat into the table's share, so a long table in a schema
//! derived a shorter name than a real server does — and a long enough one would have truncated
//! away the separator itself and landed the index in `public`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// `NAMEDATALEN - 1`, which is what a derived name must still fit in.
const LIMIT: usize = 63;

#[test]
fn a_derived_name_in_a_schema_is_the_one_postgresql_derives() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session.run("CREATE SCHEMA s").unwrap();

    // A short name first: the derived index and sequence live in the table's schema and are named
    // exactly as they would be in `public`.
    session
        .run("CREATE TABLE s.things (id bigserial PRIMARY KEY, a int8)")
        .unwrap();
    session.run("CREATE INDEX ON s.things (a)").unwrap();
    assert_eq!(
        session.rows(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 's' ORDER BY c.relname"
        ),
        [
            ["things".to_owned()],
            ["things_a_idx".to_owned()],
            ["things_id_seq".to_owned()],
            ["things_pkey".to_owned()],
        ]
        .map(|row| row.map(Some).to_vec()),
        "a derived name in a schema is the same name it would have in public"
    );

    // **The long case, which is where the schema was being charged to the identifier.** The table
    // is 60 characters, so `<table>_a_idx` is 66 and must truncate to 63 — and the 60 characters
    // are all PostgreSQL counts, whatever schema they are in.
    let long = "t".repeat(60);
    session
        .run(&format!("CREATE TABLE s.{long} (id int8, a int8)"))
        .unwrap();
    session
        .run(&format!("CREATE INDEX ON s.{long} (a)"))
        .unwrap();
    let names = session.rows(
        "SELECT c.relname, length(c.relname) FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 's' AND c.relkind = 'i' ORDER BY length(c.relname) DESC LIMIT 1",
    );
    let longest = names
        .first()
        .and_then(|row| row.first())
        .cloned()
        .flatten()
        .expect("an index was created");
    assert_eq!(
        longest.len(),
        LIMIT,
        "the derived name spends the whole budget on the identifier: {longest}"
    );
    assert!(
        longest.ends_with("_a_idx"),
        "and the label survives truncation: {longest}"
    );
    assert!(
        longest.starts_with(&"t".repeat(50)),
        "the table's own characters are what gave way, not the schema: {longest}"
    );
}
