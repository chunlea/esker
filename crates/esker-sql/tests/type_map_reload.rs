//! The OIDs a `RowDescription` carries, and the client behaviour they drive.
//!
//! Three `postgresql_adapter_test.rb` tests and two `enum_test.rb` tests are one mechanism.
//! `ActiveRecord` keeps a map from type OID to a decoder; when a `RowDescription` carries an OID
//! the map does not hold, it **reloads the map from `pg_type`**, warns once, and remembers that the
//! OID is unknown so it does not reload again. Every one of those steps is driven by the OID the
//! server sends, so the tests are assertions about this node's `RowDescription` and nothing else:
//!
//! ```text
//! test_only_reload_type_map_once_for_every_unrecognized_type
//!     select 'pg_catalog.pg_class'::regclass   2 queries, then 1, then 2 for NULL::anyarray
//! test_only_warn_on_first_encounter_of_unrecognized_oid
//!     unknown OID 2205: failed to recognize type of 'regclass'. It will be treated as String.
//! test_reload_type_map_for_newly_defined_types
//!     SELECT 'good'::feeling  ->  OID::Enum, in one query and no reload
//! ```
//!
//! **The count is the assertion.** A client that already knows the OID does not reload, so
//! describing `NULL::anyarray` as `text` makes the second block one query where the suite wants
//! two — the failure is a *missing* round trip, which is not a shape a wrong answer usually takes.
//!
//! **One gap is deliberate**: `pg_type` has no row for `anyarray`. Its rows are derived from
//! `ColumnType::ALL` so that a real type cannot be forgotten, and a pseudo-type has no
//! `ColumnType` — writing the row by hand would mean twenty positional fields describing a type
//! whose value layer does not exist. Nothing the suite does needs it: a reload that finds nothing
//! *is* an unknown OID, which is the behaviour under test.
//!
//! Measured in `tests/captures/pg19_unknown_oid.txt` and `tests/captures/pg19_recursive_cte.txt`'s
//! sibling probe, recorded in the commit that added this file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The OIDs of the two types the suite uses to provoke a reload, and one it must not.
#[test]
fn an_unknown_type_is_described_by_its_own_oid() {
    let mut node = parity::Node::new(&[]);
    for (statement, oid, what) in [
        // 2205 is `regclass`. The warning the suite matches quotes the *name* this oid has in
        // `pg_type`, so the catalog has to answer for it too.
        ("SELECT 'pg_catalog.pg_class'::regclass", 2205, "regclass"),
        // 2277 is `anyarray`, a pseudo-type: no value of it is ever stored, and the only thing a
        // client does with it is fail to recognise it. That is precisely what the test wants.
        ("SELECT NULL::anyarray", 2277, "anyarray"),
    ] {
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement} answered no rows");
        };
        assert_eq!(
            fields[0].type_oid, oid,
            "{statement} was not described as {what}"
        );
    }
}

/// **A value of a user-defined enum carries the enum's own OID**, and the catalog describes it as
/// an enum — the two halves `OID::Enum` needs.
#[test]
fn an_enum_value_carries_the_enums_oid_and_the_catalog_says_it_is_an_enum() {
    let mut node = parity::Node::new(&["CREATE TYPE feeling AS ENUM ('good', 'bad', 'sad')"]);

    let oid: i64 = node.rows("SELECT 'feeling'::regtype::oid")[0][0]
        .parse()
        .expect("the enum has no oid");
    // **Outside PostgreSQL's built-in range**, which is the whole of why this matters: a user type
    // numbered 23 is `int4` to every client, and `Type::Integer.cast('sad')` is `nil`. That was
    // the real cause of the two `enum_test.rb` failures, and it is not the OID-reload story the
    // triage read into them.
    assert!(oid >= 16_384, "the enum took a built-in oid: {oid}");

    let outcome = node.run("SELECT 'good'::feeling").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, rows, .. } = outcome else {
        panic!("the enum cast answered no rows");
    };
    assert_eq!(
        i64::from(fields[0].type_oid),
        oid,
        "the value was not described by its own type's oid"
    );
    // The client must never see the ordinal (ADR 0050).
    assert_eq!(rows[0][0].as_deref(), Some(b"good".as_slice()));

    // `pg_type` is where the client looks the oid up, and `typtype = 'e'` is what makes
    // `ActiveRecord` build an `OID::Enum` rather than warn.
    assert_eq!(
        node.rows(&format!(
            "SELECT typname, typtype FROM pg_type WHERE oid = {oid}"
        )),
        vec![vec!["feeling".to_owned(), "e".to_owned()]]
    );
}

/// The two `enum_test.rb` failures, which are the same mechanism read through a column.
#[test]
fn an_enum_column_reads_back_the_label_it_was_given() {
    let mut node = parity::Node::new(&[
        "CREATE TYPE mood AS ENUM ('sad', 'okay', 'happy')",
        "CREATE TABLE feelings (id int8, current_mood mood)",
        "INSERT INTO feelings VALUES (1, 'sad'), (2, 'okay')",
    ]);
    assert_eq!(
        node.rows("SELECT current_mood FROM feelings ORDER BY id"),
        vec![vec!["sad"], vec!["okay"]]
    );
    let outcome = node
        .run("SELECT current_mood FROM feelings ORDER BY id")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    let oid: i64 = node.rows("SELECT 'mood'::regtype::oid")[0][0]
        .parse()
        .unwrap();
    assert_eq!(i64::from(fields[0].type_oid), oid);
}

/// **Only NULL may be a pseudo-type**, and the two ways of writing a value get two codes.
#[test]
fn a_value_of_a_pseudo_type_is_refused_in_two_ways() {
    let mut node = parity::Node::new(&[]);

    // A typed operand has no cast to offer.
    let error = node.run("SELECT 1::anyarray").unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::CANNOT_COERCE);
    assert_eq!(error.to_string(), "cannot cast type integer to anyarray");

    // An unadorned literal is `unknown`, which every type takes as *input*, so the refusal moves
    // to the target and says what a pseudo-type is.
    let error = node.run("SELECT '{1,2}'::anyarray").unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(error.to_string(), "cannot accept a value of type anyarray");
}

/// **The `Describe` a prepared statement gets must say the same type the simple protocol does.**
///
/// `enum · test_enum_mapping` reads an enum column back through `PREPARE`/`EXECUTE`. The value on
/// the wire was right — `"sad"` — and the `RowDescription` said **21**, which is `int2`: the
/// enum's *storage* type leaking out where its own OID belongs. `ActiveRecord` decodes by that
/// OID, so it parsed the label as an integer and got `0`, then `nil`.
///
/// Measured, run 105, one fixture and three protocol paths on both servers: PostgreSQL answers its
/// enum's OID on all three, and this node answered its own enum OID on the simple path and `21` on
/// the two extended ones.
///
/// **A test that reads `Outcome::Rows` cannot see this** — that is the simple path, and it was
/// already right. This asks the executor for the `Describe`, which is the only thing a client that
/// prepares ever sees.
#[test]
fn a_describe_names_the_enum_the_way_the_simple_protocol_does() {
    let mut node = parity::Node::new(&[
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
        "CREATE TABLE feelings (id bigint primary key, current_mood mood)",
        "INSERT INTO feelings VALUES (1, 'sad')",
    ]);

    // What the simple path says, which is the answer both must give.
    let outcome = node.run("SELECT current_mood FROM feelings").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("the read answered no rows");
    };
    let simple = fields[0].type_oid;
    assert_ne!(
        simple, 21,
        "the enum's own oid, not `int2` — this is the assertion the whole test rests on"
    );

    for statement in [
        "SELECT current_mood FROM feelings",
        "SELECT * FROM feelings",
        "SELECT current_mood FROM feelings WHERE id = 1",
    ] {
        let described = node.describe(statement).unwrap();
        let fields = described
            .fields
            .unwrap_or_else(|| panic!("{statement} was described as returning nothing"));
        let mood = fields
            .iter()
            .find(|field| field.name == "current_mood")
            .unwrap_or_else(|| panic!("{statement} has no `current_mood` column"));
        assert_eq!(
            mood.type_oid, simple,
            "`{statement}` was described as {} through the extended protocol and as {simple} \
             through the simple one",
            mood.type_oid
        );
    }
}
