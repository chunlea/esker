//! A column of a **composite type**, end to end — run 97's
//! `a column of the composite type full_address is not supported`, 4 tests in
//! `adapters/postgresql/composite_test.rb`.
//!
//! The plan is `docs/plans/composite-type.md` and the measurement is
//! `captures/pg19_composite.txt`. `tests/composite_text.rs` holds the I/O functions on their own;
//! this is the column, the `ROW(…)` constructor and the catalog, which is what the four tests
//! touch. Field access — `(address).city` — is deliberately not built: the Ruby side splits the
//! string, so nothing in the suite asks for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// The schema both `composite_test.rb` classes share.
fn schema(session: &mut cluster::Session) {
    session
        .run("CREATE TYPE full_address AS (city VARCHAR(90), street VARCHAR(90))")
        .unwrap();
    session
        .run("CREATE TABLE postgresql_composites (id int8, address full_address)")
        .unwrap();
}

#[test]
fn the_four_shapes_composite_test_sends() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    // `test_composite_mapping`'s own statement, verbatim.
    session
        .run("INSERT INTO postgresql_composites VALUES (1, ROW('Paris', 'Champs-Élysées'))")
        .unwrap();
    assert_eq!(
        session.rows("SELECT address FROM postgresql_composites"),
        [[Some("(Paris,Champs-Élysées)".to_owned())]],
        "nothing here needs quoting, so nothing is quoted"
    );

    // The second half of the same test: the model assigns composite **text** and saves.
    session
        .run("UPDATE postgresql_composites SET address = '(Paris,Rue Basse)' WHERE id = 1")
        .unwrap();
    assert_eq!(
        session.rows("SELECT address FROM postgresql_composites"),
        [[Some("(Paris,\"Rue Basse\")".to_owned())]],
        "the value is re-rendered canonically, so a space comes back quoted"
    );

    // `test_column`: the adapter reads the type's own name, which is what makes it treat the
    // column as an unknown OID and hand the string through.
    assert_eq!(
        session.rows(
            "SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'postgresql_composites'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        [
            vec![Some("id".to_owned()), Some("bigint".to_owned())],
            vec![Some("address".to_owned()), Some("full_address".to_owned())],
        ],
        "the column reports the composite's name, not the text it is stored as"
    );
    assert_eq!(
        session.rows("SELECT pg_typeof(address) FROM postgresql_composites"),
        [[Some("full_address".to_owned())]]
    );
    assert_eq!(
        session.rows("SELECT typname, typtype FROM pg_type WHERE typname = 'full_address'"),
        [[Some("full_address".to_owned()), Some("c".to_owned())]],
        "typtype `c` is how a client tells a composite from every other kind"
    );
}

/// The custom-OID class's shape: it serialises to `"(#{city},#{street})"` and reads the halves
/// back out of the string, so what it sends is a **text literal** and never a `ROW(…)`.
#[test]
fn a_text_literal_is_the_other_way_the_suite_writes_one() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);
    session
        .run("INSERT INTO postgresql_composites VALUES (1, '(Paris,Rue Basse)')")
        .unwrap();
    assert_eq!(
        session.rows("SELECT address FROM postgresql_composites"),
        [[Some("(Paris,\"Rue Basse\")".to_owned())]]
    );
}

/// A record the type has no room for is refused, and with PostgreSQL's own sentence — the arity is
/// checked where the type's fields are known, which is the write path.
#[test]
fn a_record_of_the_wrong_width_is_refused() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);
    for literal in ["'(a,b,c)'", "'()'", "'(a'"] {
        let error = session
            .run(&format!(
                "INSERT INTO postgresql_composites VALUES (1, {literal})"
            ))
            .expect_err("a malformed record literal must be refused");
        assert_eq!(error.sqlstate(), "22P02", "{literal}: {error}");
        assert!(
            error.to_string().starts_with("malformed record literal:"),
            "{error}"
        );
    }
}

/// The standing rule: any name-resolution path is tested outside `public`, because `public` stores
/// a bare name and every other schema stores `schema ++ NUL ++ name` (ADR 0071).
#[test]
fn a_composite_column_works_in_a_schema_that_is_not_public() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session.run("CREATE SCHEMA app").unwrap();
    session
        .run("CREATE TYPE app.addr AS (city VARCHAR(90), street VARCHAR(90))")
        .unwrap();
    session
        .run("CREATE TABLE app.places (id int8, where_at app.addr)")
        .unwrap();
    session
        .run("INSERT INTO app.places VALUES (1, ROW('Paris', 'Rue Basse'))")
        .unwrap();
    assert_eq!(
        session.rows("SELECT where_at FROM app.places"),
        [[Some("(Paris,\"Rue Basse\")".to_owned())]],
        "the type resolves by its qualified name and the value still canonicalises"
    );
}

/// **No index over one in this unit**, and the refusal is deliberate rather than a gap nobody
/// noticed: PostgreSQL orders a record field by field and this node's key would be the canonical
/// text, which parts company as soon as a field needs quoting. An index that returns rows in an
/// order a real server does not is worse than one that cannot be built.
#[test]
fn a_composite_column_is_not_an_index_key() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);
    let error = session
        .run("CREATE INDEX ix ON postgresql_composites (address)")
        .expect_err("an index over a composite must be refused");
    assert_eq!(error.sqlstate(), "0A000", "{error}");
    assert!(
        error.to_string().contains("full_address"),
        "{error} must name the type the user wrote"
    );
}
