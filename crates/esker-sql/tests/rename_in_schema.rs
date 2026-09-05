//! **`ALTER TABLE … RENAME TO` keeps the table in its own schema**, and its collision check is
//! that schema's, not `public`'s.
//!
//! `SchemaWithDotsTest` is the two-test class that found this. Its head error, tapped off
//! `PG::Connection#exec` by the harness lane rather than inferred from a stack:
//!
//! ```text
//! SENT    ALTER TABLE "posts" RENAME TO "articles"
//!   RAISED PG::DuplicateTable: ERROR: relation "articles" already exists
//! ```
//!
//! `articles` is not residue — it is a suite fixture in `public` (`schema.rb:66`), always present
//! — while `posts` lives in a schema named `my.schema` that the test puts on the `search_path`.
//! So the rename was being resolved against `public` instead of against the renamed table's own
//! schema, and the recorded `PG::InFailedSqlTransaction` in every later test of the file is that
//! one refusal poisoning the session.
//!
//! # The clean two-database comparison, which is what the reproducer asked for before any edit
//!
//! The harness lane marked the mechanism a **candidate** rather than a finding, because the two
//! databases it compared were not in identical states. Redone here on names nothing else in either
//! database owns, `public.g1v54_art` created deliberately so the collision is the suite's:
//!
//! ```text
//! CREATE SCHEMA "g1.v54";
//! CREATE TABLE public.g1v54_art (…);        -- stands in for the suite fixture
//! SET search_path TO "g1.v54";
//! CREATE TABLE g1v54_src (…);
//! ALTER TABLE g1v54_src RENAME TO g1v54_art;
//! ```
//!
//! PostgreSQL 19beta1 **accepts it**, and afterwards holds both:
//!
//! ```text
//! g1.v54 | g1v54_art          r     <- the renamed table, still in its own schema
//! public | g1v54_art          r     <- untouched
//! ```
//!
//! Three more things the same session measured, each of which an implementation can get wrong on
//! its own:
//!
//! * **The collision check is per schema.** A second `RENAME TO g1v54_art` from inside `g1.v54`,
//!   where that name now exists, is `42P07 relation "g1v54_art" already exists`. So the check is
//!   real — it is just asking the wrong schema when it fires against `public`.
//! * **The sequence and the index keep their old names.** After the rename the schema still holds
//!   `g1v54_src_id_seq` and `g1v54_src_pkey`. A table rename renames the table and nothing else.
//! * **The bare name still resolves, and prints bare.** `SELECT count(*) FROM g1v54_art` answers
//!   from the dotted schema, and `'g1v54_art'::regclass::text` is `g1v54_art` rather than
//!   `"g1.v54".g1v54_art`, because the schema is on the `search_path`.
//!
//! # Why a dotted schema name is the case that breaks
//!
//! A relation outside `public` is stored `schema ++ NUL ++ name` (`catalog::split_qualified`), and
//! a dot is an ordinary character in a schema name. Any reader that splits a stored name on `.`
//! instead of on the separator sees `my` and `schema.posts` — and a reader that *builds* a name by
//! joining with `.` produces something no other reader can take apart. `tests/collation.rs` and
//! `tests/bare_relation_name.rs` cover the read side of that grammar; this covers the write side,
//! which is the one that moves a table between schemas when it gets it wrong.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The suite's shape: a fixture in `public` and the working table in a schema whose name has a dot
/// in it, with that schema on the `search_path`.
const FIXTURE: &[&str] = &[
    "CREATE SCHEMA \"g1.v54\"",
    "CREATE TABLE public.g1v54_art (id bigserial primary key, title character varying)",
    "SET search_path TO \"g1.v54\"",
    "CREATE TABLE g1v54_src (id bigserial primary key, title character varying)",
];

/// Every relation either schema holds, as `schema|name|kind`, so a table that moved schemas is
/// visible rather than merely absent.
fn relations(node: &mut parity::Node) -> Vec<String> {
    node.rows(
        "SELECT n.nspname, c.relname, c.relkind FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname LIKE 'g1v54%' ORDER BY 1, 2",
    )
    .into_iter()
    .map(|row| row.join("|"))
    .collect()
}

/// **The comparison, as one assertion.** Before the fix this is the `42P07` the suite meets.
#[test]
fn a_rename_stays_in_the_tables_own_schema() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1v54_src RENAME TO g1v54_art")
        .unwrap();
    assert_eq!(
        relations(&mut node),
        [
            "g1.v54|g1v54_art|r".to_owned(),
            "g1.v54|g1v54_src_id_seq|S".to_owned(),
            "g1.v54|g1v54_src_pkey|i".to_owned(),
            "public|g1v54_art|r".to_owned(),
            "public|g1v54_art_id_seq|S".to_owned(),
            "public|g1v54_art_pkey|i".to_owned(),
        ],
        "the renamed table is in g1.v54; public's same-named one is untouched, and neither the \
         sequence nor the index was renamed with it"
    );
}

/// **The collision check is that schema's own**, which is the half a fix must not lose: the check
/// is right, it was asking the wrong schema.
#[test]
fn a_collision_inside_the_same_schema_is_still_refused() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1v54_src RENAME TO g1v54_art")
        .unwrap();
    node.run("CREATE TABLE g1v54_two (id bigserial primary key)")
        .unwrap();
    assert_eq!(
        node.answer("ALTER TABLE g1v54_two RENAME TO g1v54_art")
            .to_string(),
        "!42P07 relation \"g1v54_art\" already exists"
    );
}

/// The renamed table answers to its bare name from the `search_path`, and `regclass` prints it
/// bare — the two surfaces a client meets right after the rename.
#[test]
fn the_renamed_table_resolves_bare() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1v54_src RENAME TO g1v54_art")
        .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1v54_art"), [["0"]]);
    assert_eq!(
        node.rows("SELECT 'g1v54_art'::regclass::text"),
        [["g1v54_art".to_owned()]]
    );
}

/// The same rename with the table named in full, which must land in the same place — a fix that
/// only repaired the `search_path` path would leave this one wrong.
#[test]
fn a_qualified_rename_lands_in_the_named_schema() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA \"g1.v54\"",
        "CREATE TABLE public.g1v54_art (id bigserial primary key, title character varying)",
        "CREATE TABLE \"g1.v54\".g1v54_src (id bigserial primary key, title character varying)",
    ]);
    node.run("ALTER TABLE \"g1.v54\".g1v54_src RENAME TO g1v54_art")
        .unwrap();
    assert!(
        relations(&mut node).contains(&"g1.v54|g1v54_art|r".to_owned()),
        "{:?}",
        relations(&mut node)
    );
}
