//! **A user-defined type takes a schema when it is created, and resolves through the
//! `search_path` when it is named.**
//!
//! `enum_test.rb#test_schema_dump_scoped_to_schemas` dumps an enum created inside `test_schema`
//! and finds it filed under `public`:
//!
//! ```text
//! dump contains    create_enum "public.mood_in_test_schema", ["sad", "ok", "happy"]
//! expected to find create_enum "test_schema.mood_in_test_schema", ["sad", "ok", "happy"]
//! ```
//!
//! The dump query is not the defect — `pg_type.typnamespace` already reports the schema a type is
//! actually in. The **create** is: `lower_create_type` qualifies a two-part name and leaves a
//! one-part name bare, so an unqualified `CREATE TYPE` under a `search_path` landed in `public`
//! whatever the path said, and `ActiveRecord`'s dumper — which sets the path to one schema at a
//! time and re-prefixes with the schema it is dumping — wrote `public.` in front of it.
//!
//! Qualifying the create is only half. Every lookup went through `catalog::type_by_name`, an
//! **exact key lookup with no path walk**, so a type created in `test_schema` would then be
//! invisible to the very next statement in that test — `t.column :current_mood,
//! :mood_in_test_schema`. The two halves are one unit.
//!
//! # Measured on 19beta1, `search_path = g1ts_a, public`
//!
//! ```text
//! CREATE TYPE g1ts_mood AS ENUM (…)          -> typnamespace g1ts_a      (the first schema)
//! CREATE DOMAIN g1ts_money AS numeric(8,2)   -> typnamespace g1ts_a      (the same rule)
//! CREATE TABLE t (m g1ts_mood)               -> resolves through the path
//! the same name in public and in g1ts_a      -> the column takes g1ts_a's  (labels from_a)
//! DROP TYPE g1ts_shadow                      -> drops g1ts_a's; public's survives
//! ALTER TYPE g1ts_mood RENAME TO g1ts_mood2  -> stays in g1ts_a
//! a type in a schema NOT on the path         -> 42704 type "…" does not exist, unqualified
//!                                            -> found when written g1ts_b.g1ts_hidden
//! CREATE TYPE g1ts_a.g1ts_mood  (installed)  -> 42710 type "g1ts_mood" already exists
//!                                               — the **bare** name, though the create was not
//! CREATE TYPE nosuchschema.x                 -> 3F000 schema "nosuchschema" does not exist
//! ```
//!
//! Two of those a guess would get wrong: the duplicate message quotes the bare name even when the
//! statement wrote a schema, and `RENAME TO` does **not** move a type between schemas — that is
//! what `SET SCHEMA` is for, and it is the same rule a table's rename follows
//! (`tests/rename_in_schema.rs`).
//!
//! # What is deliberately not here
//!
//! An unqualified name that resolves to no type is still `0A000 the type … is not supported`
//! rather than PostgreSQL's `42704 type "…" does not exist`. This node cannot tell "a type
//! PostgreSQL has that is not implemented here" from "a type nobody declared", and the first of
//! those is contract C2's `0A000`. Changing it needs the built-in type names enumerated, which is
//! a unit of its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Two schemas, one of them on the path, and `public` behind it — the shape `enum_test.rb` makes.
const FIXTURE: &[&str] = &[
    "CREATE SCHEMA g1ts_a",
    "CREATE SCHEMA g1ts_b",
    "SET search_path TO g1ts_a, public",
];

/// One statement with `public` as the creation schema, then the path back as the fixture left it.
///
/// **Why not simply write `CREATE TYPE public.x`.** A stored name is `schema ++ NUL ++ name` and
/// `catalog::qualify` collapses `public` to the bare name (`catalog::PUBLIC_SCHEMA`), so by the
/// time a plan reaches the executor an explicitly written `public.x` and a bare `x` are the same
/// string — and the executor, which is where the creation schema is decided, cannot tell a name
/// that *said* `public` from one that said nothing. `CREATE TABLE public.t` under
/// `SET search_path TO s` therefore lands in `s` on this node and in `public` on a real server.
/// That is a **separate open defect**, older than this unit and shared with `ddl::qualified_create`;
/// closing it means carrying "a schema was written" from the lowering, for relations and types
/// together. It is not worked around here — the fixture just does not depend on it.
fn in_public(node: &mut parity::Node, sql: &str) {
    node.run("SET search_path TO public").unwrap();
    node.run(sql).unwrap();
    node.run("SET search_path TO g1ts_a, public").unwrap();
}

/// `typname | nspname` for every type this file makes, the join `ActiveRecord` writes.
fn types(node: &mut parity::Node) -> Vec<String> {
    node.rows(
        "SELECT t.typname, n.nspname FROM pg_type t \
         JOIN pg_namespace n ON t.typnamespace = n.oid \
         WHERE t.typname LIKE 'g1ts%' ORDER BY 1, 2",
    )
    .into_iter()
    .map(|row| row.join("|"))
    .collect()
}

/// **The create takes the first schema of the path** — an enum and a domain, because both go
/// through the same lowering and only the domain's half was ever written down (ADR 0065).
#[test]
fn an_unqualified_create_lands_in_the_first_schema_of_the_path() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TYPE g1ts_mood AS ENUM ('sad','ok','happy')")
        .unwrap();
    node.run("CREATE DOMAIN g1ts_money AS numeric(8,2)")
        .unwrap();
    assert_eq!(
        types(&mut node),
        [
            "g1ts_money|g1ts_a".to_owned(),
            "g1ts_mood|g1ts_a".to_owned()
        ]
    );
}

/// And the very next statement has to find it, which is the half that makes this one unit.
#[test]
fn a_column_resolves_its_type_through_the_path() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TYPE g1ts_mood AS ENUM ('sad','ok','happy')")
        .unwrap();
    node.run("CREATE TABLE g1ts_t (id int8 PRIMARY KEY, mood g1ts_mood)")
        .unwrap();
    node.run("INSERT INTO g1ts_t VALUES (1, 'ok')").unwrap();
    assert_eq!(node.rows("SELECT mood FROM g1ts_t"), [["ok"]]);
}

/// **The first schema on the path wins**, which is the rule a shadowed name needs and the one an
/// exact lookup cannot express.
#[test]
fn the_first_schema_on_the_path_wins() {
    let mut node = parity::Node::new(FIXTURE);
    in_public(&mut node, "CREATE TYPE g1ts_shadow AS ENUM ('from_public')");
    node.run("CREATE TYPE g1ts_a.g1ts_shadow AS ENUM ('from_a')")
        .unwrap();
    node.run("CREATE TABLE g1ts_s (id int8 PRIMARY KEY, c g1ts_shadow)")
        .unwrap();
    node.run("INSERT INTO g1ts_s VALUES (1, 'from_a')").unwrap();
    assert_eq!(node.rows("SELECT c FROM g1ts_s"), [["from_a"]]);
}

/// A type in a schema that is **not** on the path is reached only when the name says so.
#[test]
fn a_type_off_the_path_needs_its_schema_written() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TYPE g1ts_b.g1ts_hidden AS ENUM ('h')")
        .unwrap();
    assert!(
        node.answer("CREATE TABLE g1ts_h (c g1ts_hidden)")
            .to_string()
            .starts_with('!'),
        "an unqualified name must not reach a schema off the path"
    );
    node.run("CREATE TABLE g1ts_h2 (id int8 PRIMARY KEY, c g1ts_b.g1ts_hidden)")
        .unwrap();
}

/// `DROP TYPE` takes the one the path finds, and leaves `public`'s alone.
#[test]
fn a_drop_takes_the_one_the_path_finds() {
    let mut node = parity::Node::new(FIXTURE);
    in_public(&mut node, "CREATE TYPE g1ts_shadow AS ENUM ('from_public')");
    node.run("CREATE TYPE g1ts_a.g1ts_shadow AS ENUM ('from_a')")
        .unwrap();
    node.run("DROP TYPE g1ts_shadow").unwrap();
    assert_eq!(types(&mut node), ["g1ts_shadow|public".to_owned()]);
}

/// `RENAME TO` keeps it where it is — the rule `SET SCHEMA` exists for, and the one a table's
/// rename already follows.
#[test]
fn a_rename_keeps_the_type_in_its_own_schema() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TYPE g1ts_mood AS ENUM ('sad','ok')")
        .unwrap();
    node.run("ALTER TYPE g1ts_mood RENAME TO g1ts_mood2")
        .unwrap();
    assert_eq!(types(&mut node), ["g1ts_mood2|g1ts_a".to_owned()]);
}

/// **The duplicate names the bare type**, even when the statement wrote a schema. Measured.
#[test]
fn a_second_create_is_42710_naming_the_bare_type() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TYPE g1ts_mood AS ENUM ('sad')").unwrap();
    assert_eq!(
        node.answer("CREATE TYPE g1ts_a.g1ts_mood AS ENUM ('x')")
            .to_string(),
        "!42710 type \"g1ts_mood\" already exists"
    );
}

/// A schema that is not there is `3F000`, the same class `CREATE TABLE` gives — not a type that
/// quietly lands in `public`.
#[test]
fn a_missing_schema_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE TYPE nosuchschema.g1ts_x AS ENUM ('x')")
            .to_string(),
        "!3F000 schema \"nosuchschema\" does not exist"
    );
    assert!(types(&mut node).is_empty(), "{:?}", types(&mut node));
}

/// **`ActiveRecord#enum_types`, the query and the schema loop**: the dumper sets the path to one
/// schema at a time, so each schema reports its own enums and the dumper prefixes them itself.
/// This is the assertion `test_schema_dump_scoped_to_schemas` makes.
#[test]
fn the_dumper_sees_each_enum_under_the_schema_it_was_created_in() {
    let mut node = parity::Node::new(FIXTURE);
    in_public(&mut node, "CREATE TYPE g1ts_public_mood AS ENUM ('sad')");
    node.run("CREATE TYPE g1ts_mood AS ENUM ('sad','ok','happy')")
        .unwrap();

    let enums = |node: &mut parity::Node| -> Vec<String> {
        node.rows(
            "SELECT type.typname, n.nspname FROM pg_enum AS enum \
             JOIN pg_type AS type ON (type.oid = enum.enumtypid) \
             JOIN pg_namespace n ON type.typnamespace = n.oid \
             WHERE n.nspname = ANY (current_schemas(false)) AND type.typname LIKE 'g1ts%' \
             GROUP BY type.oid, n.nspname, type.typname ORDER BY 1",
        )
        .into_iter()
        .map(|row| row.join("|"))
        .collect()
    };

    node.run("SET search_path TO g1ts_a").unwrap();
    assert_eq!(enums(&mut node), ["g1ts_mood|g1ts_a".to_owned()]);
    node.run("SET search_path TO public").unwrap();
    assert_eq!(enums(&mut node), ["g1ts_public_mood|public".to_owned()]);
}
