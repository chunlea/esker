//! An enum column read back through the funnel `ActiveRecord` reads it through.
//!
//! `enum_test.rb`'s two remaining failures are both `Expected: "sad" Actual: nil` — a value that
//! is **gone**, with no error anywhere:
//!
//! ```text
//! test_enum_mapping        [:62]    INSERT INTO postgresql_enums VALUES (1, 'sad');  ->  nil
//! test_works_with_activerecord_enum [:198]                                           ->  nil
//! ```
//!
//! The triage's first guess was the unknown-OID story that `tests/regclass.rs` closed. **It is
//! not, and the shape of the failure says so before any measurement**: an OID a client does not
//! know makes it treat the value as a *String*, so `current_mood` would be `"sad"` and the test
//! would pass. `nil` is a value that arrived as NULL.
//!
//! ADR 0050 is why this can happen at all: an enum is an `int2` ordinal in the row and a label on
//! the wire, so every read is a lookup, and a lookup that misses has to answer *something*.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `enum_test.rb`'s own setup, statement for statement.
const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TABLE postgresql_enums (id bigserial PRIMARY KEY, current_mood mood)",
];

/// The read the test makes, and the one that answers `nil`.
#[test]
fn an_enum_column_reads_back_its_label() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO postgresql_enums VALUES (1, 'sad')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT current_mood FROM postgresql_enums"),
        vec![vec!["sad"]]
    );
    // The whole row, which is what `PostgresqlEnum.first` reads.
    assert_eq!(
        node.rows("SELECT id, current_mood FROM postgresql_enums ORDER BY id LIMIT 1"),
        vec![vec!["1", "sad"]]
    );
}

/// **The statement `ActiveRecord` actually sends**, which is not the one anybody writes by hand.
///
/// `PostgresqlEnum.first` is a wildcard, a qualified `ORDER BY` and a bound `LIMIT`:
/// `SELECT "postgresql_enums".* FROM "postgresql_enums" ORDER BY "postgresql_enums"."id" ASC
/// LIMIT $1`. A named column list reads the label back correctly, so if this one does not, the
/// difference is the shape and not the type — and a `*` expansion is exactly where the labels a
/// lookup needs could fail to be attached (ADR 0050).
#[test]
fn the_wildcard_read_activerecord_sends_gets_the_label_too() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO postgresql_enums VALUES (1, 'sad')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT * FROM postgresql_enums"),
        vec![vec!["1", "sad"]]
    );
    assert_eq!(
        node.rows(
            "SELECT \"postgresql_enums\".* FROM \"postgresql_enums\" ORDER BY \
             \"postgresql_enums\".\"id\" ASC LIMIT 1"
        ),
        vec![vec!["1", "sad"]]
    );
}

/// **The wire, not the value**: what a client is told the column is.
#[test]
fn an_enum_column_is_described_as_its_own_type() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO postgresql_enums VALUES (1, 'sad')")
        .unwrap();

    let outcome = node
        .run("SELECT current_mood FROM postgresql_enums")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("a SELECT answered no rows at all");
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "current_mood");
    // **The wire and the catalog must name the same oid.** `ActiveRecord` registers its enum
    // decoder under the oid `pg_type` gave it and then looks the *wire's* oid up in that map; two
    // different numbers means the lookup misses however well-formed each one is on its own.
    let catalog = node.rows("SELECT oid FROM pg_type WHERE typname = 'mood'");
    assert_eq!(catalog.len(), 1, "pg_type has no row for the enum");
    assert_eq!(
        fields[0].type_oid.to_string(),
        catalog[0][0],
        "the RowDescription and pg_type disagree about the enum's oid"
    );
    // And it is a *user* oid on a real server — the first is 16384, and a client that filters by
    // that boundary is filtering out anything below it.
    assert!(
        fields[0].type_oid > 16_383,
        "an enum column was described as oid {}, which is below the first user oid",
        fields[0].type_oid
    );
    // Measured on 19beta1: `pg_type.typlen` for an enum is **4**, not -1
    // (`tests/captures/pg19_unknown_oid.txt`).
    assert_eq!(fields[0].type_size, 4);
}

/// The same read after the statement that `test_enum_mapping` runs next.
#[test]
fn an_enum_survives_an_update_and_a_reload() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO postgresql_enums VALUES (1, 'sad')")
        .unwrap();
    node.run("UPDATE postgresql_enums SET current_mood = 'happy' WHERE id = 1")
        .unwrap();
    assert_eq!(
        node.rows("SELECT current_mood FROM postgresql_enums"),
        vec![vec!["happy"]]
    );
}
