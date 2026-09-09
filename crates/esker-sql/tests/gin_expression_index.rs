//! **A JSON accessor is immutable, so an index may be built over one.**
//!
//! `PostgresqlInvertibleMigrationTest#test_migrate_revert_add_index_with_expression` runs
//!
//! ```ruby
//! add_index :settings, "(data->'foo')", using: :gin, name: "index_settings_data_foo"
//! ```
//!
//! and this node refused it: `functions in index expression must be marked IMMUTABLE`. The check
//! is right to exist — a `CREATE INDEX` over `gen_random_uuid()` would build keys nothing could
//! ever read back — and its list was one family short.
//!
//! # Measured on 19beta1, through `pg_operator` rather than guessed from the family
//!
//! ```text
//! json_object_field        i      jsonb_object_field       i
//! json_object_field_text   i      jsonb_object_field_text  i
//! jsonb_extract_path       i      jsonb_array_element      i
//! jsonb_concat             i
//! -> ->> ||  over jsonb    i      (every operator that resolves to those)
//! concat                   s      <- the reason the family cannot be assumed
//! ```
//!
//! `concat` sits two lines from these in the same match and is `STABLE`, so a guess from "string
//! and JSON helpers are surely immutable" would have gone the wrong way for it — which is why the
//! comment beside the change cites `pg_operator` and not a recollection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The migration's own statement, plus the two accessors beside it.
#[test]
fn an_index_may_be_built_over_a_json_accessor() {
    let mut node = parity::Node::new(&["CREATE TABLE g1gin (data jsonb, doc json)"]);
    node.run("CREATE INDEX index_settings_data_foo ON g1gin USING gin ((data->'foo'))")
        .unwrap();
    node.run("CREATE INDEX g1gin_text ON g1gin ((data->>'foo'))")
        .unwrap();
    node.run("CREATE INDEX g1gin_json ON g1gin ((doc->'foo'))")
        .unwrap();
    assert_eq!(
        node.rows("SELECT indexname FROM pg_indexes WHERE tablename = 'g1gin' ORDER BY 1"),
        [
            ["g1gin_json".to_owned()],
            ["g1gin_text".to_owned()],
            ["index_settings_data_foo".to_owned()],
        ]
    );
}

/// **And the check still refuses what it is for.** A volatile function in an index expression is
/// still `42P17`, which is the half a widened list must not take with it.
#[test]
fn a_volatile_function_is_still_refused() {
    let mut node = parity::Node::new(&["CREATE TABLE g1gin_v (a text)"]);
    assert!(
        node.answer("CREATE INDEX g1gin_bad ON g1gin_v ((gen_random_uuid()::text)))")
            .to_string()
            .starts_with('!'),
        "a uuid function in an index expression must still be refused"
    );
}

/// The index the migration builds is droppable, which is what `migrate(:down)` does with it.
#[test]
fn the_index_drops_again() {
    let mut node = parity::Node::new(&["CREATE TABLE g1gin_d (data jsonb)"]);
    node.run("CREATE INDEX g1gin_d_ix ON g1gin_d USING gin ((data->'foo'))")
        .unwrap();
    node.run("DROP INDEX g1gin_d_ix").unwrap();
    assert!(
        node.rows("SELECT indexname FROM pg_indexes WHERE tablename = 'g1gin_d'")
            .is_empty()
    );
}
