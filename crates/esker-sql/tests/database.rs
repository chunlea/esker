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
        vec![
            vec![SERVING.to_owned()],
            vec!["template0".to_owned()],
            vec!["template1".to_owned()],
        ],
        "the database being served, and the two every cluster is born with"
    );

    node.run("CREATE DATABASE arunit2").unwrap();
    assert_eq!(
        node.rows("SELECT datname FROM pg_database ORDER BY datname"),
        vec![
            vec!["arunit2".to_owned()],
            vec![SERVING.to_owned()],
            vec!["template0".to_owned()],
            vec!["template1".to_owned()],
        ]
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
        vec![vec!["4".to_owned()]],
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
        vec![
            vec!["arunit3".to_owned()],
            vec![SERVING.to_owned()],
            vec!["template0".to_owned()],
            vec!["template1".to_owned()],
        ]
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

/// **`rake db:create`'s statement**, which is the one this whole option list exists for:
/// `CREATE DATABASE "x" ENCODING = 'utf8'`. It was a `42601` — a syntax error about a statement a
/// real server runs — until the list was cut out of the source before the parse.
#[test]
fn the_statement_rake_db_create_sends_is_an_answer() {
    let mut node = parity::Node::new(&[]);

    node.run("CREATE DATABASE \"arunit\" ENCODING = 'utf8'")
        .unwrap();
    // Every spelling PostgreSQL takes for the same encoding, and the `WITH` and no-`=` forms.
    node.run("CREATE DATABASE a2 ENCODING 'UTF8'").unwrap();
    node.run("CREATE DATABASE a3 WITH ENCODING = 'unicode'")
        .unwrap();
    node.run("CREATE DATABASE a4 ENCODING = 'UTF-8' LC_COLLATE = 'C' LC_CTYPE = 'C'")
        .unwrap();
    node.run("CREATE DATABASE a5 TEMPLATE = template0 LOCALE = 'C' TABLESPACE = pg_default STRATEGY = 'wal_log'")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_database"),
        vec![vec!["8".to_owned()]],
        "five created, the one being served, and the two templates"
    );
    // **The values are not recorded, because there is nothing to record**: this cluster has one
    // encoding and one collation, so every database has the same three and `pg_database` says so.
    assert_eq!(
        node.rows(
            "SELECT DISTINCT pg_encoding_to_char(encoding), datcollate, datctype FROM pg_database"
        ),
        vec![vec!["UTF8".to_owned(), "C".to_owned(), "C".to_owned()]]
    );
}

/// **A value this cluster cannot provide is refused by the value, in PostgreSQL's own words where
/// PostgreSQL has them.**
///
/// The line these draw is not "is the option implemented" but "could a client tell that it was
/// not": an encoding or a collation this node does not have is a refusal, an option naming a
/// facility it has none of gets PostgreSQL's `42704` for a name that is not there, and an option
/// PostgreSQL itself does not have gets PostgreSQL's own `42601` — which is a **syntax** error
/// there and not a feature refusal.
#[test]
fn a_value_this_cluster_cannot_provide_is_refused_the_way_postgresql_refuses_one() {
    let mut node = parity::Node::new(&[]);

    for (statement, state, message) in [
        (
            "CREATE DATABASE d ENCODING = 'nosuchencoding'",
            sqlstate::UNDEFINED_OBJECT,
            "nosuchencoding is not a valid encoding name",
        ),
        (
            "CREATE DATABASE d OWNER = alice",
            sqlstate::UNDEFINED_OBJECT,
            "role \"alice\" does not exist",
        ),
        (
            "CREATE DATABASE d TABLESPACE = fast",
            sqlstate::UNDEFINED_OBJECT,
            "tablespace \"fast\" does not exist",
        ),
        (
            "CREATE DATABASE d STRATEGY = 'nosuch'",
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid create database strategy \"nosuch\"",
        ),
        (
            "CREATE DATABASE d TEMPLATE = nosuchdb",
            sqlstate::INVALID_CATALOG_NAME,
            "template database \"nosuchdb\" does not exist",
        ),
        (
            "CREATE DATABASE d NOSUCHOPTION = 1",
            sqlstate::SYNTAX_ERROR,
            "option \"nosuchoption\" not recognized",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), state, "{statement}: {error}");
        assert_eq!(error.to_string(), message, "{statement}");
    }
    assert_eq!(
        node.run("CREATE DATABASE d STRATEGY = 'nosuch'")
            .unwrap_err()
            .hint()
            .as_deref(),
        Some("Valid strategies are \"wal_log\" and \"file_copy\".")
    );

    // **An encoding PostgreSQL has and this node does not is a different failure from one nobody
    // has**, which is why the encoding names are a list here rather than a single comparison.
    for statement in [
        "CREATE DATABASE d ENCODING = 'LATIN1'",
        "CREATE DATABASE d LC_COLLATE = 'en_US.utf8'",
        "CREATE DATABASE d LOCALE = 'en_US.utf8'",
        // Each of these is a promise a client can check — it would connect past the limit,
        // connect to a database declared closed, or copy from something that is not a template.
        "CREATE DATABASE d CONNECTION LIMIT 5",
        "CREATE DATABASE d ALLOW_CONNECTIONS = false",
        "CREATE DATABASE d IS_TEMPLATE = true",
        "CREATE DATABASE d LOCALE_PROVIDER = 'icu'",
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

    // The defaults of the three that are refused **are** answers: they promise nothing.
    node.run("CREATE DATABASE d1 CONNECTION LIMIT -1").unwrap();
    node.run("CREATE DATABASE d2 ALLOW_CONNECTIONS = true")
        .unwrap();
    node.run("CREATE DATABASE d3 IS_TEMPLATE = false").unwrap();
}

/// **A template must be empty, because this node creates an empty database.**
///
/// An empty copy of an empty template is exact; of a full one it is a wrong answer wearing a
/// success, so it is refused by name. `template0` and `template1` are seeded precisely so that the
/// spelling everybody writes — and the one PostgreSQL's own `HINT` points at — names something.
#[test]
fn a_template_is_copied_only_when_there_is_nothing_to_copy() {
    let mut node = parity::Node::new(&[]);

    node.run("CREATE DATABASE fromtpl TEMPLATE = template1")
        .unwrap();
    node.run("CREATE TABLE t (id bigserial primary key)")
        .unwrap();

    let error = node
        .run(&format!("CREATE DATABASE copied TEMPLATE = {SERVING}"))
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("which is not empty"), "{error}");

    // **A template is there rather than missing**, so it is its own class and `IF EXISTS` does not
    // cover it — the same distinction the open database draws.
    for statement in [
        "DROP DATABASE template0",
        "DROP DATABASE IF EXISTS template1",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE, "{statement}");
        assert_eq!(error.to_string(), "cannot drop a template database");
    }
}

/// **Two databases on one cluster are two tenants, and the isolation is the key encoding rather
/// than a check.**
///
/// This is the whole of what run 46's seventh row needs: `schema.rb` creates `dogs` with four
/// association columns on `arunit` and a bare `dogs` on `arunit2`, and against one namespace the
/// second `create_table … force: true` drops and recreates the first. Here it does not, because
/// `'t' ++ tenant ++ table_id` puts them in different key ranges.
#[test]
fn two_databases_hold_two_tables_of_one_name() {
    use std::sync::Arc;

    let backend: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::MemoryBackend::new());
    let catalog = Arc::new(esker_sql::catalog::Catalog::new());

    let mut arunit = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, SERVING, &[]);
    arunit.run("CREATE DATABASE arunit2").unwrap();
    let id: u64 = arunit.rows("SELECT oid FROM pg_database WHERE datname = 'arunit2'")[0][0]
        .parse()
        .unwrap();
    let mut arunit2 = parity::Node::on(backend, catalog, id, "arunit2", &[]);

    arunit
        .run("CREATE TABLE dogs (id bigserial primary key, trainer_id integer, alias varchar)")
        .unwrap();
    // The last line of `schema.rb`, on the other connection. On a node with one namespace this is
    // a `DROP TABLE` of the four-column `dogs` above; here it is a table of its own.
    arunit2.run("DROP TABLE IF EXISTS dogs").unwrap();
    arunit2
        .run("CREATE TABLE dogs (id bigserial primary key)")
        .unwrap();

    assert_eq!(
        arunit.rows(
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'dogs'::regclass AND attnum > 0"
        ),
        vec![vec!["3".to_owned()]],
        "the fixture's columns are still there — the failure the 103 tests report is this number \
         going to 1"
    );
    assert_eq!(
        arunit2.rows(
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'dogs'::regclass AND attnum > 0"
        ),
        vec![vec!["1".to_owned()]]
    );

    // Rows do not cross either, and neither does a name: each session sees its own `dogs`.
    arunit
        .run("INSERT INTO dogs (trainer_id, alias) VALUES (1, 'rex')")
        .unwrap();
    assert_eq!(
        arunit.rows("SELECT count(*) FROM dogs"),
        vec![vec!["1".to_owned()]]
    );
    assert_eq!(
        arunit2.rows("SELECT count(*) FROM dogs"),
        vec![vec!["0".to_owned()]]
    );

    // **`current_database()` is the session's, not the server's.** A constant here would report
    // `esker` to the second session, which is a wrong answer rather than a missing feature.
    assert_eq!(
        arunit.rows("SELECT current_database()"),
        vec![vec![SERVING.to_owned()]]
    );
    assert_eq!(
        arunit2.rows("SELECT current_database()"),
        vec![vec!["arunit2".to_owned()]]
    );
    // And `pg_database` is the cluster's, so both sessions see both — which is what makes
    // `WHERE datname = current_database()` pick out the right row on each.
    for node in [&mut arunit, &mut arunit2] {
        assert_eq!(
            node.rows("SELECT count(*) FROM pg_database"),
            vec![vec!["4".to_owned()]]
        );
    }
    assert_eq!(
        arunit2.rows("SELECT datname FROM pg_database WHERE datname = current_database()"),
        vec![vec!["arunit2".to_owned()]]
    );
}

/// **A database another session is connected to cannot be dropped** — and the sentence is not the
/// one for the session's own database.
///
/// Two refusals share `55006` and differ in their message, so a test that asserted only the
/// SQLSTATE could not tell a node that implements the first from one that implements both. Measured
/// on 19beta1 (`esker-coord/s2-h102b.out`, `s2-h102c-plural.out`): with one other session attached
/// the answer is `database "…" is being accessed by other users` with
/// `DETAIL: There is 1 other session using the database.`, and with two it is
/// `There are 2 other sessions using the database.` — a plural form chosen by the count. This node
/// deleted the database under the other session and said nothing.
#[test]
fn a_database_another_session_is_on_cannot_be_dropped() {
    use std::sync::Arc;

    let backend: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::MemoryBackend::new());
    let catalog = Arc::new(esker_sql::catalog::Catalog::new());

    let mut arunit = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, SERVING, &[]);
    arunit.run("CREATE DATABASE arunit2").unwrap();
    let id: u64 = arunit.rows("SELECT oid FROM pg_database WHERE datname = 'arunit2'")[0][0]
        .parse()
        .unwrap();

    // **Bound to a name, not to `_`**: the other session has to be alive across the `DROP`, which
    // is the whole statement under test. A `_` binding would drop it on the spot and the test would
    // pass against a node that never learned to count.
    let other = parity::Node::on(
        Arc::clone(&backend),
        Arc::clone(&catalog),
        id,
        "arunit2",
        &[],
    );

    let error = arunit.run("DROP DATABASE arunit2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::OBJECT_IN_USE);
    assert_eq!(
        error.to_string(),
        "database \"arunit2\" is being accessed by other users"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("There is 1 other session using the database.")
    );

    // **And the database is still there.** A refusal that deleted it anyway would satisfy all three
    // assertions above, which is the mechanism this test would otherwise not be testing.
    assert_eq!(
        arunit.rows("SELECT count(*) FROM pg_database WHERE datname = 'arunit2'")[0][0],
        "1"
    );

    // The plural is a second form and not the same sentence with a different number, so it is
    // pinned rather than assumed.
    let third = parity::Node::on(
        Arc::clone(&backend),
        Arc::clone(&catalog),
        id,
        "arunit2",
        &[],
    );
    let error = arunit.run("DROP DATABASE arunit2").unwrap_err();
    assert_eq!(
        error.detail().as_deref(),
        Some("There are 2 other sessions using the database.")
    );

    // With every other session gone the drop is allowed, which is what says the refusal is about
    // the sessions and not about the database.
    drop(other);
    drop(third);
    arunit.run("DROP DATABASE arunit2").unwrap();
}

/// **`pg_stat_activity` names each session's own database** — `debts-v1.1.md` #111.
///
/// The name and the id were resolved once, from the tenant of the session running the query, and
/// stamped onto every row, so a node with sessions on two databases showed one name twice and one
/// `datid` twice. The query an operator uses to find who is holding a database — the same one the
/// 19beta1 capture for #102 used to prove its second session was attached
/// (`esker-coord/s2-h102b.out`) — therefore answered every session or none, depending on who asked.
#[test]
fn pg_stat_activity_names_each_sessions_own_database() {
    use std::sync::Arc;

    let backend: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::MemoryBackend::new());
    let catalog = Arc::new(esker_sql::catalog::Catalog::new());

    let mut arunit = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, SERVING, &[]);
    arunit.run("CREATE DATABASE arunit2").unwrap();
    let id: u64 = arunit.rows("SELECT oid FROM pg_database WHERE datname = 'arunit2'")[0][0]
        .parse()
        .unwrap();
    let _other = parity::Node::on(
        Arc::clone(&backend),
        Arc::clone(&catalog),
        id,
        "arunit2",
        &[],
    );

    // **Asked from `arunit`, which is not on `arunit2`** — that is the whole point: before the fix
    // this counted 0, because every row carried the asking session's own database.
    assert_eq!(
        arunit.rows("SELECT count(*) FROM pg_stat_activity WHERE datname = 'arunit2'")[0][0],
        "1"
    );

    // And the asking session's own database is not doubled by the other one, which is the same
    // defect seen from the other side: this counted 2.
    assert_eq!(
        arunit.rows(&format!(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = '{SERVING}'"
        ))[0][0],
        "1"
    );
}
