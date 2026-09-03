//! `CREATE DATABASE` and `DROP DATABASE` — the statements behind [ADR 0052](../../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md).
//!
//! The corpus (`tests/two_database_dogs.rs`) covers the one shape a rolled-back capture can reach:
//! `CREATE DATABASE` inside a transaction block is `25001` on both servers. Everything a *working*
//! `CREATE DATABASE` does happens outside one, which is exactly what no capture in this phase can
//! contain — so it is pinned here instead, statement by statement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The database every session here is already connected to.
const SERVING: &str = "esker";

/// A cluster nobody has told about databases reports the one it is serving, and creating a second
/// **adds** to it rather than replacing it.
///
/// The seed is the half worth pinning: without it the row for the database the session is using
/// would vanish the moment somebody created a different one.
#[test]
fn creating_a_database_leaves_the_one_the_session_is_using() {
    let mut node = parity::Node::new(&[]);

    assert_eq!(
        node.rows("SELECT datname FROM pg_database ORDER BY datname"),
        vec![vec![SERVING.to_owned()]]
    );

    node.run("CREATE DATABASE arunit2").unwrap();
    assert_eq!(
        node.rows("SELECT datname FROM pg_database ORDER BY datname"),
        vec![vec!["arunit2".to_owned()], vec![SERVING.to_owned()]]
    );

    // **The oid is the tenant id**, so the two are one number and `datname = current_database()`
    // matches by construction rather than by two constants being kept equal.
    assert_eq!(
        node.rows("SELECT oid FROM pg_database WHERE datname = current_database()"),
        vec![vec!["1".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT oid FROM pg_database WHERE datname = 'arunit2'"),
        vec![vec!["16384".to_owned()]],
        "a database a user created starts at PostgreSQL's own boundary"
    );
}

/// A name the cluster has is `42P04`, and `IF NOT EXISTS` turns it into a notice and a success.
#[test]
fn a_second_database_of_one_name_is_refused_unless_the_clause_says_otherwise() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE DATABASE arunit2").unwrap();

    let error = node.run("CREATE DATABASE arunit2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_DATABASE);
    assert_eq!(error.to_string(), "database \"arunit2\" already exists");

    node.run("CREATE DATABASE IF NOT EXISTS arunit2").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_database"),
        vec![vec!["2".to_owned()]],
        "the clause is a success and not a second row"
    );

    // The database the session is serving is in the directory the same way, so it collides too —
    // which is what says the seed is a row rather than a special case.
    let error = node.run(&format!("CREATE DATABASE {SERVING}")).unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_DATABASE);
}

/// **`DROP DATABASE` cannot name the one the session is connected to**, and `IF EXISTS` does not
/// change that: the database is there rather than missing, which is a different failure.
#[test]
fn the_open_database_cannot_be_dropped_with_or_without_if_exists() {
    let mut node = parity::Node::new(&[]);

    for statement in [
        format!("DROP DATABASE {SERVING}"),
        format!("DROP DATABASE IF EXISTS {SERVING}"),
    ] {
        let error = node.run(&statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::OBJECT_IN_USE, "{statement}");
        assert_eq!(error.to_string(), "cannot drop the currently open database");
    }
}

/// A name nothing has is `3D000`, and `IF EXISTS` is a notice and a success.
#[test]
fn dropping_a_database_that_is_not_there_says_which_class_of_wrong_it_is() {
    let mut node = parity::Node::new(&[]);

    let error = node.run("DROP DATABASE nosuchdatabase").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_CATALOG_NAME);
    assert_eq!(
        error.to_string(),
        "database \"nosuchdatabase\" does not exist"
    );

    node.run("DROP DATABASE IF EXISTS nosuchdatabase").unwrap();
}

/// A database created and dropped leaves the directory as it was.
#[test]
fn a_dropped_database_leaves_the_directory_it_was_in() {
    let mut node = parity::Node::new(&[]);

    node.run("CREATE DATABASE arunit2").unwrap();
    node.run("CREATE DATABASE arunit3").unwrap();
    node.run("DROP DATABASE arunit2").unwrap();

    assert_eq!(
        node.rows("SELECT datname FROM pg_database ORDER BY datname"),
        vec![vec!["arunit3".to_owned()], vec![SERVING.to_owned()]]
    );

    // **Ids are not reused**, so the name coming back does not bring the old tenant's rows with
    // it — which is what makes dropping the directory row and sweeping the tenant two halves of
    // one statement rather than one of them being optional.
    node.run("CREATE DATABASE arunit2").unwrap();
    assert_eq!(
        node.rows("SELECT oid FROM pg_database WHERE datname = 'arunit2'"),
        vec![vec!["16386".to_owned()]]
    );
}

/// **Neither statement may be part of a transaction block**, which is PostgreSQL's `25001` and is
/// measured in `tests/corpus/pg19_two_database_dogs.txt` for the `CREATE`.
///
/// The reason is the same on both servers: a database is state outside every transaction, so a
/// block that could roll one back would be a block that could roll back half a schema change.
#[test]
fn neither_statement_may_be_part_of_a_transaction_block() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();

    for (statement, named) in [
        ("CREATE DATABASE arunit2", "CREATE DATABASE"),
        ("DROP DATABASE arunit2", "DROP DATABASE"),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "{statement}"
        );
        assert_eq!(
            error.to_string(),
            format!("{named} cannot run inside a transaction block")
        );
        node.run("ROLLBACK").unwrap();
        node.run("BEGIN").unwrap();
    }
    node.run("ROLLBACK").unwrap();
}

/// **The option list is refused by name**, which is contract C2 — and it is a refusal rather than
/// a feature because `sqlparser` 0.62.0's `CREATE DATABASE` grammar has no room for one, so
/// without these rows every option would be a `42601` about a statement a real server runs.
///
/// `rake db:create` sends `ENCODING`, which is why it is the row that matters most.
#[test]
fn every_option_postgresql_takes_is_refused_by_name_and_never_as_syntax() {
    let mut node = parity::Node::new(&[]);

    for statement in [
        "CREATE DATABASE d WITH OWNER alice ENCODING 'UTF8'",
        "CREATE DATABASE d ENCODING = 'utf8'",
        "CREATE DATABASE d TEMPLATE template0",
        "CREATE DATABASE d LC_COLLATE 'C' LC_CTYPE 'C'",
        "CREATE DATABASE d CONNECTION LIMIT 5",
        "CREATE DATABASE d TABLESPACE fast",
        "DROP DATABASE IF EXISTS d WITH (FORCE)",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement}: {error}"
        );
        assert!(
            error.to_string().ends_with(" is not supported"),
            "{statement}: {error}"
        );
    }

    // And the bare form is not caught by any of them, which is the half a refusal table gets
    // wrong by being too eager.
    node.run("CREATE DATABASE d").unwrap();
    node.run("CREATE DATABASE IF NOT EXISTS d").unwrap();
    node.run("DROP DATABASE d").unwrap();
}
