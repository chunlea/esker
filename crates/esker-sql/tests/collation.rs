//! **`COLLATE "C"` and `COLLATE "POSIX"`**, which name the ordering this node has.
//!
//! Run 89's `COLLATE "C" is not supported` — 6 tests, `collation_test.rb` (5) and
//! `unsafe_raw_sql_test.rb` (1). The decision is
//! [ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md): both names
//! mean byte order, a memcomparable key already sorts by its bytes, and every other collation name
//! is refused rather than accepted and ignored — because ignoring one would return rows in an
//! order the client did not ask for, which ADR 0031 ranks above any gap.
//!
//! Measured in `captures/pg19_collation.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// `ActiveRecord`'s own read-back: the two columns of interest, joined the way
/// `postgresql_adapter.rb`'s `column_definitions` joins them, verbatim.
const READ_BACK: &str = "SELECT a.attname, c.collname FROM pg_attribute a \
     LEFT JOIN pg_type t ON a.atttypid = t.oid \
     LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation \
     WHERE a.attrelid = 'pc'::regclass AND a.attnum > 0 ORDER BY a.attnum";

/// And the **whole** statement, every column of it, because a join that works on its own can still
/// be the one thing a nine-column projection cannot plan — and this is the statement the adapter
/// sends before it can describe any table at all (`postgresql_adapter.rb:1058`).
const COLUMN_DEFINITIONS: &str = "SELECT a.attname, format_type(a.atttypid, a.atttypmod), \
     pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod, \
     c.collname, col_description(a.attrelid, a.attnum) AS comment, \
     attidentity AS identity, attgenerated as attgenerated \
     FROM pg_attribute a \
     LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
     LEFT JOIN pg_type t ON a.atttypid = t.oid \
     LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation \
     WHERE a.attrelid = 'pc'::regclass \
     AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum";

#[test]
fn a_declared_collation_is_stored_and_read_back() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run(
            "CREATE TABLE pc (id int8, string_c varchar COLLATE \"C\", \
             text_posix text COLLATE \"POSIX\", plain text)",
        )
        .unwrap();

    // **The two that named one are reported and the two that did not are NULL** — which is the
    // whole of `attcollation <> typcollation`, and the reason `typcollation` had to move off zero
    // at the same time: with the type saying 0 and the column saying 100, a plain `text` column
    // reported the collation `default`, which is the wrong half of the same comparison.
    assert_eq!(
        rendered(&mut session, READ_BACK),
        [
            ["id", ""],
            ["string_c", "C"],
            ["text_posix", "POSIX"],
            ["plain", ""],
        ],
        "a column reports the collation it named, and none if it named none"
    );

    // The seventh column of the real statement is `collname`, and it must say the same thing.
    assert_eq!(
        session
            .rows(COLUMN_DEFINITIONS)
            .into_iter()
            .map(|row| [row[0].clone(), row[6].clone()])
            .collect::<Vec<_>>(),
        [
            [Some("id".to_owned()), None],
            [Some("string_c".to_owned()), Some("C".to_owned())],
            [Some("text_posix".to_owned()), Some("POSIX".to_owned())],
            [Some("plain".to_owned()), None],
        ],
        "and says it inside the statement the adapter actually sends"
    );
}

/// The same read-back for a table in a schema that is not `public`, because a bare name is what
/// `public` stores and a schema-qualified one is `schema ++ NUL ++ name` (ADR 0071) — so a
/// catalog lookup that forgot the strip passes here and only here.
#[test]
fn a_declared_collation_survives_a_schema_qualified_name() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session.run("CREATE SCHEMA app").unwrap();
    session
        .run("CREATE TABLE app.pc (id int8, string_c varchar COLLATE \"C\", plain text)")
        .unwrap();

    assert_eq!(
        rendered(&mut session, &READ_BACK.replace("'pc'", "'app.pc'")),
        [["id", ""], ["string_c", "C"], ["plain", ""]],
        "the collation read-back must not depend on the table living in `public`"
    );
}

/// `collation_test.rb`'s other two: the clause on `ALTER TABLE`, added to a column and changed on
/// one. Both read back through the same `attcollation <> typcollation`.
#[test]
fn alter_table_carries_a_collation_too() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE pc (id int8, plain text)")
        .unwrap();
    session
        .run("ALTER TABLE pc ADD COLUMN title varchar COLLATE \"C\"")
        .unwrap();
    session
        .run("ALTER TABLE pc ALTER COLUMN plain TYPE text COLLATE \"POSIX\"")
        .unwrap();

    assert_eq!(
        rendered(&mut session, READ_BACK),
        [["id", ""], ["plain", "POSIX"], ["title", "C"]],
        "a collation added or changed by ALTER is read back like a declared one"
    );
}

/// `COLLATE` on a type with no ordering to override is `42804` and not `42704`: the name exists,
/// the type still cannot have one. Measured for `integer` and `uuid` — `CREATE TABLE badx (a uuid
/// COLLATE "C")` and `SELECT 1 COLLATE "C"` are the same sentence with a different noun.
#[test]
fn a_type_with_no_collation_refuses_one_that_exists() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    for (sql, ty) in [
        ("CREATE TABLE bad (a uuid COLLATE \"C\")", "uuid"),
        ("CREATE TABLE bad (a int COLLATE \"POSIX\")", "integer"),
        ("SELECT 1 COLLATE \"C\"", "integer"),
    ] {
        let error = session.run(sql).expect_err("a non-collatable type refuses");
        assert_eq!(error.sqlstate(), "42804", "{sql}: {error}");
        assert_eq!(
            error.to_string(),
            format!("collations are not supported by type {ty}")
        );
    }

    // **The name is checked before the type**, which is the order a real server checks them in:
    // `42704`, not `42804`, even though `integer` could not have taken it either.
    let error = session
        .run("CREATE TABLE bad (a int COLLATE \"en_US.UTF-8\")")
        .expect_err("an unknown name on a non-collatable type is still the name's error");
    assert_eq!(error.sqlstate(), "42704", "{error}");
}

/// `ORDER BY title COLLATE "C" DESC`, which is `unsafe_raw_sql_test.rb`'s one statement — and it
/// must give the *same* rows as the plain ordering, because byte order is what this node has.
#[test]
fn collate_in_an_order_by_is_the_ordering_the_node_already_gives() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE p (author_id int8, title text)")
        .unwrap();
    session
        .run("INSERT INTO p VALUES (1,'b'),(1,'A'),(2,'a'),(2,'B')")
        .unwrap();

    assert_eq!(
        rendered(
            &mut session,
            "SELECT title FROM p ORDER BY author_id, title COLLATE \"C\" DESC"
        ),
        // `b,A,a,B` and not `b,A,B,a`: within `author_id = 2` the DESC key puts `a` (0x61) above
        // `B` (0x42), which is the whole difference between byte order and a locale's. Measured
        // on the oracle after this assertion was written the other way round.
        [["b"], ["A"], ["a"], ["B"]],
        "the clause names the order the rows were already in"
    );
    assert_eq!(
        rendered(
            &mut session,
            "SELECT title FROM p ORDER BY title COLLATE \"POSIX\""
        ),
        [["A"], ["B"], ["a"], ["b"]],
        "and POSIX is the same order under the other name"
    );
    let error = session
        .run("SELECT title FROM p ORDER BY title COLLATE \"en_US.UTF-8\"")
        .expect_err("a locale collation is a different order and is refused");
    assert_eq!(error.sqlstate(), "42704", "{error}");
}

/// `rows` with `None` rendered as the empty string, which no collation name can be.
fn rendered(session: &mut cluster::Session, sql: &str) -> Vec<Vec<String>> {
    session
        .rows(sql)
        .into_iter()
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .collect()
}

#[test]
fn a_collation_this_node_does_not_have_is_refused() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    for name in ["en_US.UTF-8", "nope", "und-x-icu"] {
        let error = session
            .run(&format!("CREATE TABLE bad (a text COLLATE \"{name}\")"))
            .expect_err("a collation this node does not have must be refused");
        assert_eq!(
            error.sqlstate(),
            "42704",
            "{name}: {error} is not the sqlstate a real server gives"
        );
        assert_eq!(
            error.to_string(),
            format!("collation \"{name}\" for encoding \"UTF8\" does not exist"),
            "and the sentence is PostgreSQL's own"
        );
    }
}
