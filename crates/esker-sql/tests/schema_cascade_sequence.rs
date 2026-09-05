//! **A sequence in a dropped schema must go with it.**
//!
//! `schema_test.rb`'s `setup` ends with a bare `CREATE SEQUENCE test_schema.…` and its `teardown`
//! is `drop_schema … if_exists: true`, which is `DROP SCHEMA "test_schema" CASCADE`. Run 89 shows
//! 51 tests in that class failing on
//! `relation "test_schema.unmatched_primary_key_default_value_seq" already exists` — 0 at the head
//! and 51 in the wake, which is the shape of one object surviving a drop and colliding with every
//! later `setup`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The two statements the suite repeats, run twice — which is what a second test in the class is.
#[test]
fn a_schema_can_be_created_dropped_and_created_again_with_its_sequence() {
    let mut node = parity::Node::new(&[]);
    for round in 1..=2 {
        node.run("CREATE SCHEMA test_schema")
            .unwrap_or_else(|e| panic!("round {round}: CREATE SCHEMA: {e}"));
        node.run("CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq")
            .unwrap_or_else(|e| panic!("round {round}: CREATE SEQUENCE: {e}"));
        node.run("DROP SCHEMA test_schema CASCADE")
            .unwrap_or_else(|e| panic!("round {round}: DROP SCHEMA: {e}"));
    }
}

/// The same with the table that depends on the sequence, which is the rest of what `setup` builds.
#[test]
fn a_schema_with_a_sequence_backed_default_drops_and_rebuilds() {
    let mut node = parity::Node::new(&[]);
    for round in 1..=2 {
        node.run("CREATE SCHEMA test_schema").unwrap();
        node.run("CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq")
            .unwrap();
        node.run(
            "CREATE TABLE test_schema.table_with_unmatched_sequence_for_pk (id integer NOT NULL \
             DEFAULT nextval('test_schema.unmatched_primary_key_default_value_seq'::regclass), \
             CONSTRAINT unmatched_pkey PRIMARY KEY (id))",
        )
        .unwrap_or_else(|e| panic!("round {round}: CREATE TABLE: {e}"));
        node.run("DROP SCHEMA test_schema CASCADE")
            .unwrap_or_else(|e| panic!("round {round}: DROP SCHEMA: {e}"));
    }
}

/// **A view cannot be stranded the same way**, because it cannot get into a non-public schema at
/// all: `CREATE VIEW test_schema.v` is `0A000 the qualified name test_schema.v is not supported`.
/// A separate gap, checked here so the cascade fix is known to cover what can actually be in a
/// schema rather than assumed to.
#[test]
fn a_qualified_view_cannot_be_created_so_cannot_be_stranded() {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema"]);
    node.run("CREATE TABLE test_schema.t (id int8)").unwrap();
    assert_eq!(
        node.answer("CREATE VIEW test_schema.v AS SELECT id FROM test_schema.t")
            .to_string(),
        "!0A000 the qualified name test_schema.v is not supported"
    );
}

/// `schema_test.rb`'s `setup`, verbatim, run **twice** with its `teardown` between — which is what
/// two tests in that class are.
///
/// This is the assertion the run-89 report asked for on both sides: the head test's setup
/// completes, and the next test's `CREATE` succeeds. It is the whole `setup` rather than the one
/// statement that collided, because a setup is only as good as its last line and this one builds
/// two schemas, six indexes and two sequence-backed tables before it gets there.
#[test]
fn the_whole_setup_runs_twice_with_the_teardown_between() {
    const COLUMNS: &str = "id integer,name character varying(50),email character varying(50),\
                           description character varying(100),name_vector tsvector,\
                           moment timestamp without time zone default now()";
    let mut node = parity::Node::new(&[]);
    for round in 1..=2 {
        for sql in [
            format!("CREATE SCHEMA test_schema CREATE TABLE things ({COLUMNS})"),
            format!("CREATE TABLE test_schema.\"things.table\" ({COLUMNS})"),
            format!("CREATE TABLE test_schema.\"Things\" ({COLUMNS})"),
            format!("CREATE SCHEMA test_schema2 CREATE TABLE things ({COLUMNS})"),
            "CREATE INDEX a_index_things_on_name ON test_schema.things  USING btree (name)".into(),
            "CREATE INDEX a_index_things_on_name ON test_schema2.things  USING btree (name)".into(),
            "CREATE INDEX b_index_things_on_different_columns_in_each_schema ON test_schema.things  USING btree (email)".into(),
            "CREATE INDEX b_index_things_on_different_columns_in_each_schema ON test_schema2.things  USING btree (moment)".into(),
            "CREATE INDEX c_index_full_text_search ON test_schema.things  USING gin ((to_tsvector('english', coalesce(things.name, ''))))".into(),
            "CREATE INDEX c_index_full_text_search ON test_schema2.things  USING gin ((to_tsvector('english', coalesce(things.name, ''))))".into(),
            "CREATE INDEX d_index_things_on_description_desc ON test_schema.things  USING btree (description DESC)".into(),
            "CREATE INDEX d_index_things_on_description_desc ON test_schema2.things  USING btree (description DESC)".into(),
            "CREATE INDEX e_index_things_on_name_vector ON test_schema.things  USING gin (name_vector)".into(),
            "CREATE INDEX e_index_things_on_name_vector ON test_schema2.things  USING gin (name_vector)".into(),
            "CREATE TABLE test_schema.table_with_pk (id serial primary key)".into(),
            "CREATE TABLE test_schema2.table_with_pk (id serial primary key)".into(),
            "CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq".into(),
            "CREATE TABLE test_schema.table_with_unmatched_sequence_for_pk (id integer NOT NULL \
             DEFAULT nextval('test_schema.unmatched_primary_key_default_value_seq'::regclass), \
             CONSTRAINT unmatched_pkey PRIMARY KEY (id))".into(),
        ] {
            node.run(&sql)
                .unwrap_or_else(|e| panic!("round {round}: {sql}\n  {e}"));
        }
        // The teardown, in its order: `drop_schema … if_exists: true`, twice.
        node.run("DROP SCHEMA test_schema2 CASCADE").unwrap();
        node.run("DROP SCHEMA test_schema CASCADE").unwrap();
    }
}

/// **A setup that dies partway must still be tearable-down**, which is the other half of the
/// report: one test's setup failed and every later test in the class collided with what it left.
///
/// So the sequence is created, the next statement is made to fail, and the teardown then has to
/// remove the schema *and* the sequence — after which a full second setup succeeds. Before the
/// cascade fix this stranded the sequence exactly as the suite did.
#[test]
fn a_setup_that_fails_partway_still_tears_down() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE SCHEMA test_schema").unwrap();
    node.run("CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq")
        .unwrap();
    // Whatever goes wrong next, the sequence is already there. A duplicate is a stand-in for it.
    assert!(
        node.run("CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq")
            .is_err()
    );
    node.run("DROP SCHEMA test_schema CASCADE").unwrap();

    node.run("CREATE SCHEMA test_schema").unwrap();
    node.run("CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq")
        .expect("the next test's setup must not collide with the failed one's leftovers");
}
