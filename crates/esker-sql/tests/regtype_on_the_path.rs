//! **`'<name>'::regtype` resolves an unqualified type through the `search_path`.**
//!
//! `enum_test#test_enum_type_scoped_to_schemas` creates an enum inside `test_schema`, builds a
//! table with a column of it, and asserts the table exists. It aborted on a statement in between:
//!
//! ```text
//! SELECT 'mood_in_other_schema'::regtype::oid   ->  42704 type "…" does not exist
//! ```
//!
//! **A regression of my own**, and the shape is worth keeping: the day a user-defined type started
//! taking a schema, every lookup that searched for the *bare* stored name stopped finding one that
//! lived in a schema. `Executor::stored_type_name` was taught the path for a column's declared
//! type; `qualified_user_type`, which is what a `::regtype` goes through, was not — so a name the
//! session could see in every other statement was `42704` in this one.
//!
//! The rule is a relation's: a qualified spelling finds the schema it names, an unqualified one
//! walks the path, and the bare name is tried last because that is what `public` stores.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema"]);
    node.run("SET search_path TO test_schema, public").unwrap();
    node.run("CREATE TYPE \"mood_in_other_schema\" AS ENUM ('sad','ok','happy')")
        .unwrap();
    node
}

/// The statement the test aborted on.
#[test]
fn an_unqualified_regtype_finds_a_type_on_the_path() {
    let mut node = node();
    let oid = node.rows("SELECT 'mood_in_other_schema'::regtype::oid");
    assert_eq!(oid.len(), 1, "{oid:?}");
    assert!(
        oid[0][0].parse::<u64>().is_ok(),
        "an oid, not {:?}",
        oid[0][0]
    );
}

/// **The rest of the test's sequence**, which is what says the abort is the whole of it: the table
/// is created and is there.
#[test]
fn the_table_of_that_type_is_created_and_found() {
    let mut node = node();
    node.run(
        "CREATE TABLE \"postgresql_enums_in_other_schema\" (\"id\" bigserial primary key, \
         \"current_mood\" \"mood_in_other_schema\" DEFAULT 'happy' NOT NULL)",
    )
    .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = ANY (current_schemas(false)) \
             AND c.relname = 'postgresql_enums_in_other_schema'"
        ),
        [["postgresql_enums_in_other_schema"]]
    );
    // The default computes, which is the half a name that resolved to the wrong type would fail.
    node.run("INSERT INTO postgresql_enums_in_other_schema DEFAULT VALUES")
        .unwrap();
    assert_eq!(
        node.rows("SELECT current_mood FROM postgresql_enums_in_other_schema"),
        [["happy"]]
    );
}

/// A **qualified** spelling still names its own schema, and a type off the path is still not found
/// unqualified — the two halves the walk must not swallow.
#[test]
fn a_qualified_spelling_and_one_off_the_path_are_unchanged() {
    let mut node = node();
    node.run("CREATE SCHEMA g1rt_off").unwrap();
    node.run("CREATE TYPE g1rt_off.hidden AS ENUM ('h')")
        .unwrap();
    assert!(
        node.rows("SELECT 'g1rt_off.hidden'::regtype::oid")[0][0]
            .parse::<u64>()
            .is_ok()
    );
    assert_eq!(
        node.answer("SELECT 'hidden'::regtype::oid").to_string(),
        "!42704 type \"hidden\" does not exist"
    );
}
