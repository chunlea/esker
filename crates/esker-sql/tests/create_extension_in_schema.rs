//! **`CREATE EXTENSION … SCHEMA <name>` puts the extension in that schema, and `pg_extension`
//! says so.**
//!
//! Three tests send it. `tests/create_extension_schema.rs` already made the bare spelling parse
//! (contract C1); the clause itself was still `0A000 CREATE EXTENSION ... SCHEMA is not supported`
//! and `pg_extension.extnamespace` was a constant.
//!
//! ```text
//! PostgresqlExtensionMigrationTest#test_enable_extension_migration_with_schema
//!     enable_extension "other_schema.hstore"   ->  CREATE EXTENSION IF NOT EXISTS "hstore"
//!                                                    SCHEMA other_schema
//! PostgreSQLAdapterTest#test_extensions_omits_current_schema_name
//!     CREATE EXTENSION hstore SCHEMA customschema, then extensions includes "customschema.hstore"
//! PostgreSQLAdapterTest#test_extensions_includes_non_current_schema_name
//!     CREATE EXTENSION hstore,                  then extensions includes "hstore"
//! ```
//!
//! `ActiveRecord#extensions` is one query and one line of Ruby, and the Ruby is half the contract:
//!
//! ```sql
//! SELECT pg_extension.extname, n.nspname AS schema
//!   FROM pg_extension JOIN pg_namespace n ON pg_extension.extnamespace = n.oid
//! ```
//!
//! ```ruby
//! schema = nil if schema == current_schema
//! [schema, name].compact.join(".")
//! ```
//!
//! So the schema is **dropped when it is the current one** and prefixed otherwise, which is why
//! the two adapter tests are a pair: one asserts the qualified form, the other the bare form, and
//! an implementation that hardcodes either passes one and fails the other.
//!
//! # Measured on 19beta1
//!
//! ```text
//! CREATE EXTENSION hstore SCHEMA g1cs           pg_extension: hstore | g1cs     -> "g1cs.hstore"
//! CREATE EXTENSION hstore                       pg_extension: hstore | public   -> "hstore"
//! CREATE EXTENSION hstore SCHEMA nosuchschema   3F000 schema "nosuchschema" does not exist
//! CREATE EXTENSION hstore        (installed)    42710 extension "hstore" already exists
//! CREATE EXTENSION IF NOT EXISTS hstore SCHEMA g1cs   (installed)
//!                                               NOTICE 42710 extension "hstore" already exists,
//!                                               skipping — and the schema is NOT changed
//! ```
//!
//! **`plpgsql` is the one that is not in `public`.** On a real server its `extnamespace` is
//! `pg_catalog` and its `extrelocatable` is `f`, alone among the seven this build offers:
//!
//! ```text
//! extname   | extnamespace | extrelocatable
//! plpgsql   |           11 | f                 <- pg_catalog on a real server
//! hstore    |         2200 | t                 <- public on a real server
//! ```
//!
//! so `ActiveRecord#extensions` on a real server lists `pg_catalog.plpgsql` and a bare `hstore`.
//! Reporting `plpgsql` in `public` would put it in the list as a bare `plpgsql`, which is the
//! wrong string for the one extension every database has.
//!
//! **The oid *numbers* are this node's own and are deliberately not PostgreSQL's** — measured
//! here, `public` is 11 and `pg_catalog` is 12, where a real server has 2200 and 11. Nothing the
//! suite sends compares a namespace oid to a literal; every reader joins, `ActiveRecord`'s
//! included. So the assertions below are written against the **names** the join produces, and a
//! test that pinned the numbers would be pinning an internal id.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &["CREATE SCHEMA g1cs"];

/// `ActiveRecord#extensions`, both halves: the query verbatim and the Ruby that post-processes it.
fn extensions(node: &mut parity::Node) -> Vec<String> {
    let current = node.rows("SELECT current_schema()")[0][0].clone();
    node.rows(
        "SELECT pg_extension.extname, n.nspname AS schema \
         FROM pg_extension JOIN pg_namespace n ON pg_extension.extnamespace = n.oid \
         ORDER BY 1",
    )
    .into_iter()
    .map(|row| {
        if row[1] == current {
            row[0].clone()
        } else {
            format!("{}.{}", row[1], row[0])
        }
    })
    .collect()
}

/// **The pair, in one test, because an implementation can only pass both by reading the schema.**
#[test]
fn the_schema_is_prefixed_unless_it_is_the_current_one() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE EXTENSION hstore SCHEMA g1cs").unwrap();
    assert!(
        extensions(&mut node).contains(&"g1cs.hstore".to_owned()),
        "{:?}",
        extensions(&mut node)
    );

    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE EXTENSION hstore").unwrap();
    assert!(
        extensions(&mut node).contains(&"hstore".to_owned()),
        "{:?}",
        extensions(&mut node)
    );
}

/// The `IF NOT EXISTS` spelling `enable_extension "other_schema.hstore"` sends.
#[test]
fn if_not_exists_with_a_schema_installs_it_there() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE EXTENSION IF NOT EXISTS \"hstore\" SCHEMA g1cs")
        .unwrap();
    assert!(
        extensions(&mut node).contains(&"g1cs.hstore".to_owned()),
        "{:?}",
        extensions(&mut node)
    );
    // `extension_enabled?` reads a **different view**, and it must agree.
    assert_eq!(
        node.rows(
            "SELECT installed_version IS NOT NULL FROM pg_available_extensions \
             WHERE name = 'hstore'"
        ),
        [["t".to_owned()]]
    );
}

/// A schema that is not there is `3F000`, not a `42704` and not a silent install into `public`.
#[test]
fn a_schema_that_does_not_exist_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE EXTENSION hstore SCHEMA nosuchschema")
            .to_string(),
        "!3F000 schema \"nosuchschema\" does not exist"
    );
    assert!(
        extensions(&mut node).is_empty()
            || !extensions(&mut node).iter().any(|e| e.contains("hstore")),
        "nothing was installed: {:?}",
        extensions(&mut node)
    );
}

/// Installing twice is `42710`; with `IF NOT EXISTS` it is a notice and **the schema does not
/// move** — measured, and the half an implementation that re-installs would get wrong.
#[test]
fn a_second_install_does_not_move_it() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE EXTENSION hstore").unwrap();
    assert_eq!(
        node.answer("CREATE EXTENSION hstore").to_string(),
        "!42710 extension \"hstore\" already exists"
    );
    node.run("CREATE EXTENSION IF NOT EXISTS hstore SCHEMA g1cs")
        .unwrap();
    assert!(
        extensions(&mut node).contains(&"hstore".to_owned()),
        "still in public, not moved to g1cs: {:?}",
        extensions(&mut node)
    );
}

/// **`plpgsql` lives in `pg_catalog`**, which is what makes it the one entry that is never bare in
/// `ActiveRecord`'s list.
#[test]
fn plpgsql_is_in_pg_catalog() {
    let mut node = parity::Node::new(FIXTURE);
    assert!(
        extensions(&mut node).contains(&"pg_catalog.plpgsql".to_owned()),
        "{:?}",
        extensions(&mut node)
    );
}
