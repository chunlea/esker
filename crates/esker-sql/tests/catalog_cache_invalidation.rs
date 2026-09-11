//! **A cache that is filled and never invalidated looks exactly like a cache that works** — until
//! somebody else runs DDL.
//!
//! `docs/plans/debt-49-catalog-cache.md` gave `catalog::Cache` six more maps
//! (`schemas`, `schema_lists`, `views`, `types`, `sequences`, `relations`) beside the two it had.
//! Every one of them is a separate way to answer a question with a state that has moved, and the
//! window is one statement wide: session A fills a map, session B commits a `CREATE`, and A's
//! **next** statement must see it.
//!
//! # One test per map, and not one merged test
//!
//! A single case that made a table, a schema, a type and a view and then asked one question would
//! pass on **any one** of the maps being invalidated — the version moves once and the whole cache
//! is dropped together, so a test that reads one map is a test of the drop and not of that map's
//! membership in it. What can go wrong per map is that a map is filled somewhere the version is
//! *not* consulted; that is a property of one map, so it takes one test each.
//!
//! *The counterfactual*: let the cache's version move **without dropping what it holds**, which is
//! a cache that is filled and never invalidated:
//!
//! ```text
//!          if version > cache.version {
//! -            *cache = Cache { version, ..Cache::default() };
//! +            cache.version = version;
//!          }
//! ```
//!
//! **7 of the 9 below go red**, one per map. The two that stay green are the two about a
//! transaction and its *own* uncommitted DDL — those are behind a different door
//! (`Executor::catalog_view`'s `catalog_written` branch), and the last of them names its own
//! counterfactual, which does turn it red. A suite where only one moves is a suite testing one map.
//!
//! **The first counterfactual written here was a bad one and is worth recording**: replacing the
//! condition with `if false` made every test pass, because it leaves `cache.version` *behind* the
//! view's — so `usable_at` answers false and the cache is simply switched **off**. A counterfactual
//! that disables the thing under test proves nothing; this one has to make it *wrong*.
//!
//! # Two sessions, one node
//!
//! Every case is *two* sessions sharing one `Backend` and one `Arc<Catalog>`, because that is what
//! the cache is: per node, shared by every session. A single session would pass on the pin being
//! dropped at its own transaction's end and would say nothing about the shared map.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Two sessions on one store and one cache, with `a` warmed by the statements in `warm`.
fn two_sessions(fixture: &[&str], warm: &[&str]) -> (parity::Node, parity::Node) {
    let store: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    let mut a = parity::Node::on(
        Arc::clone(&store),
        Arc::clone(&catalog),
        1,
        "esker",
        fixture,
    );
    let b = parity::Node::on(store, catalog, 1, "esker", &[]);
    for statement in warm {
        a.run(statement)
            .unwrap_or_else(|error| panic!("the warm-up did not run: {statement}\n{error}"));
    }
    (a, b)
}

/// `Cache::relations` and `Cache::names`: a table B creates is a relation A's next statement lists.
#[test]
fn the_first_statement_after_a_create_table_sees_the_new_table() {
    let (mut a, mut b) = two_sessions(
        &["CREATE TABLE warm (id bigint primary key)"],
        &["SELECT count(*) FROM pg_class"],
    );
    b.run("CREATE TABLE fresh (id bigint primary key)").unwrap();
    assert_eq!(
        a.rows("SELECT relname FROM pg_class WHERE relname = 'fresh'"),
        [["fresh"]],
        "A read `pg_class` before B created the table, and its cached snapshot of the tenant's \
         relations was never dropped"
    );
}

/// `Cache::tables`, through `Cache::relations`: a column B adds is a column A's next statement has.
#[test]
fn the_first_statement_after_an_add_column_sees_the_new_column() {
    let (mut a, mut b) = two_sessions(
        &["CREATE TABLE t (id bigint primary key)"],
        &["SELECT attname FROM pg_attribute WHERE attrelid = 't'::regclass"],
    );
    b.run("ALTER TABLE t ADD COLUMN added text").unwrap();
    assert!(
        a.rows("SELECT attname FROM pg_attribute WHERE attrelid = 't'::regclass")
            .iter()
            .any(|row| row[0] == "added"),
        "A hydrated `t` before B added the column"
    );
}

/// `Cache::schemas`: the map `schema_exists` answers from, which is what a `search_path` entry
/// asks about once per name resolved.
///
/// **The path is what reads it**: an entry naming a schema that does not exist is dropped from the
/// resolved path, so a schema created by somebody else has to appear in A's next `current_schemas`.
#[test]
fn the_first_statement_after_a_create_schema_sees_the_new_schema() {
    let (mut a, mut b) = two_sessions(&[], &["SET search_path TO fresh_schema, public"]);
    assert_eq!(
        a.rows("SELECT current_schemas(false)::text"),
        [["{public}"]],
        "before it exists the entry is dropped, which is what fills the `false` entry"
    );
    b.run("CREATE SCHEMA fresh_schema").unwrap();
    assert_eq!(
        a.rows("SELECT current_schemas(false)::text"),
        [["{fresh_schema,public}"]],
        "A cached `schema_exists(\"fresh_schema\") = false` and B created it"
    );
}

/// `Cache::schema_lists`: the whole list, which `pg_namespace` reads.
#[test]
fn the_first_statement_after_a_create_schema_lists_it_in_pg_namespace() {
    let (mut a, mut b) = two_sessions(&[], &["SELECT nspname FROM pg_namespace"]);
    b.run("CREATE SCHEMA listed").unwrap();
    assert_eq!(
        a.rows("SELECT nspname FROM pg_namespace WHERE nspname = 'listed'"),
        [["listed"]],
        "A read the tenant's schemas before B created one"
    );
}

/// `Cache::types`: the tenant's user-defined types, which `pg_type` and every `::mood` cast read.
#[test]
fn the_first_statement_after_a_create_type_sees_the_new_type() {
    let (mut a, mut b) = two_sessions(&[], &["SELECT typname FROM pg_type WHERE typname = 'mood'"]);
    b.run("CREATE TYPE mood AS ENUM ('sad', 'ok')").unwrap();
    assert_eq!(
        a.rows("SELECT typname FROM pg_type WHERE typname = 'mood'"),
        [["mood"]],
        "A read the tenant's types before B declared one"
    );
}

/// `Cache::views`: the tenant's views, which `pg_views` reads and which `FROM v` resolves through.
#[test]
fn the_first_statement_after_a_create_view_sees_the_new_view() {
    let (mut a, mut b) = two_sessions(
        &["CREATE TABLE t (id bigint primary key)"],
        &["SELECT viewname FROM pg_views"],
    );
    b.run("CREATE VIEW v AS SELECT id FROM t").unwrap();
    assert_eq!(
        a.rows("SELECT viewname FROM pg_views WHERE viewname = 'v'"),
        [["v"]],
        "A read the tenant's views before B created one"
    );
    assert_eq!(
        a.rows("SELECT count(*) FROM v"),
        [["0"]],
        "and the name resolves, which is the other reader of the same map"
    );
}

/// `Cache::sequences`: one table's sequences, attached to its `TableDef` by `catalog::hydrate`.
///
/// **`ALTER SEQUENCE … RESTART` is not enough** — that writes the counter, not the record — so the
/// DDL here is the one that changes what the map holds: a new `bigserial` column, whose sequence
/// the table's definition has to carry for `pg_sequence` to report it.
#[test]
fn the_first_statement_after_a_new_sequence_sees_it() {
    let (mut a, mut b) = two_sessions(
        &["CREATE TABLE t (id bigint primary key)"],
        &["SELECT count(*) FROM pg_sequence"],
    );
    b.run("ALTER TABLE t ADD COLUMN counted bigserial").unwrap();
    assert_eq!(
        a.rows("SELECT relname FROM pg_class WHERE relkind = 'S'"),
        [["t_counted_seq"]],
        "A hydrated `t` — sequences and all — before B gave it one"
    );
}

/// **A rolled-back transaction's types must not be in the map the next one reads.**
///
/// The shape `rolled_back_ddl_cache.rs` found and named, asked of the maps
/// `docs/plans/debt-49-catalog-cache.md` added: a version is not a transaction identity, so a
/// transaction that fills the cache and then rolls back leaves its state at a version some *other*
/// transaction will later arrive at legitimately. `Executor::catalog_view` is what stops it — a
/// transaction that has written the catalog gets a view that neither reads nor fills the cache —
/// and this asks that the new maps are behind the same door.
///
/// *The counterfactual*: `if self.catalog_written` → `if false` in `Executor::catalog_view`, which
/// hands a DDL transaction the shared cache. **Red**, with `rolled` in `pg_type`: A's rolled-back
/// type published at version 2, and B arrived at version 2 by its own `CREATE SCHEMA`.
#[test]
fn a_rolled_back_type_is_not_in_the_next_transactions_catalog() {
    let (mut a, mut b) = two_sessions(&[], &[]);
    a.run("BEGIN").unwrap();
    a.run("CREATE TYPE rolled AS ENUM ('x')").unwrap();
    assert_eq!(
        a.rows("SELECT typname FROM pg_type WHERE typname = 'rolled'"),
        [["rolled"]],
        "inside the transaction it is there, which is the half that must keep working"
    );
    a.run("ROLLBACK").unwrap();
    // B now reaches the version A's rolled-back transaction filled the cache at, honestly.
    b.run("CREATE SCHEMA arrives_at_the_same_version").unwrap();
    assert!(
        b.rows("SELECT typname FROM pg_type WHERE typname = 'rolled'")
            .is_empty(),
        "B was handed a type that no transaction ever committed"
    );
    assert!(
        a.rows("SELECT typname FROM pg_type WHERE typname = 'rolled'")
            .is_empty(),
        "and so was A, whose own transaction rolled it back"
    );
}

/// A transaction that has **written** the catalog reads its own uncommitted DDL, and the new maps
/// are bypassed exactly as `names` and `tables` are.
///
/// This is the other half of the same rule: the cache must not answer such a transaction, and it
/// must not be *filled* by one either — `rolled_back_ddl_cache.rs` is what happens when it is.
#[test]
fn a_ddl_transaction_reads_its_own_catalog_writes() {
    let (mut a, mut b) = two_sessions(&[], &["SELECT count(*) FROM pg_class"]);
    a.run("BEGIN").unwrap();
    a.run("CREATE SCHEMA mine").unwrap();
    a.run("CREATE TYPE mine.grade AS ENUM ('a', 'b')").unwrap();
    a.run("CREATE TABLE mine.t (id bigint primary key)")
        .unwrap();
    a.run("CREATE VIEW mv AS SELECT id FROM mine.t").unwrap();
    assert_eq!(
        a.rows("SELECT nspname FROM pg_namespace WHERE nspname = 'mine'"),
        [["mine"]],
        "its own schema"
    );
    assert_eq!(
        a.rows("SELECT typname FROM pg_type WHERE typname = 'grade'"),
        [["grade"]],
        "its own type"
    );
    assert_eq!(
        a.rows("SELECT viewname FROM pg_views WHERE viewname = 'mv'"),
        [["mv"]],
        "its own view"
    );
    assert_eq!(
        a.rows("SELECT relname FROM pg_class WHERE relname = 't'"),
        [["t"]],
        "its own table"
    );
    // And **none of it is published**: B shares the cache and must see nothing until A commits.
    assert!(
        b.rows("SELECT nspname FROM pg_namespace WHERE nspname = 'mine'")
            .is_empty(),
        "B saw an uncommitted schema, which means the uncached view filled the shared map"
    );
    a.run("ROLLBACK").unwrap();
    assert!(
        a.rows("SELECT nspname FROM pg_namespace WHERE nspname = 'mine'")
            .is_empty(),
        "and after the rollback it is gone from A too"
    );
}
