//! **`ALTER TABLE … ADD COLUMN … DEFAULT gen_random_uuid()` gives every stored row its own value.**
//!
//! Every other `ADD COLUMN` here is instant: a column with no default pads to NULL, and one with a
//! *constant* default stores that constant as the column's missing value and pads with it, which
//! is PostgreSQL 11's `attmissingval`. A **volatile** default cannot be one constant, so it was
//! refused — `ALTER TABLE ... ADD COLUMN ... DEFAULT gen_random_uuid(), which would rewrite every
//! row is not supported` — on the honest argument that accepting it and padding NULL would be a
//! wrong answer rather than a gap.
//!
//! PostgreSQL rewrites the table. Measured on 19beta1, over three rows:
//!
//! ```text
//! ALTER TABLE g1_backfill ADD COLUMN thingy uuid NOT NULL DEFAULT gen_random_uuid();
//!   rows 3 | distinct_uuids 3 | non_null 3        -- one value each, all different
//!   the_default  gen_random_uuid()                -- and the default is still an expression
//! ```
//!
//! `uuid_test.rb` sends exactly this, twice: `test_uuid_column_default` with `gen_random_uuid()`
//! and `test_change_column_default` with `uuid_generate_v1()`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1_backfill (id bigserial primary key, name text)",
    "INSERT INTO g1_backfill (name) VALUES ('a'), ('b'), ('c')",
];

/// The rows that predate the column each get their own value, and none is NULL.
#[test]
fn every_stored_row_gets_its_own_value() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_backfill ADD COLUMN thingy uuid NOT NULL DEFAULT gen_random_uuid()")
        .expect("PostgreSQL rewrites the table for this");
    assert_eq!(
        node.rows("SELECT count(*), count(DISTINCT thingy), count(thingy) FROM g1_backfill"),
        [["3".to_owned(), "3".to_owned(), "3".to_owned()]],
        "three rows, three different uuids, none null"
    );
}

/// The column keeps an **expression** default, not the value one row happened to get.
#[test]
fn the_default_is_still_the_expression() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_backfill ADD COLUMN thingy uuid NOT NULL DEFAULT gen_random_uuid()")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef d \
             JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             WHERE a.attname = 'thingy' AND d.adrelid = 'g1_backfill'::regclass"
        ),
        [["gen_random_uuid()".to_owned()]]
    );
    // And a row written afterwards draws again rather than repeating one of the three.
    node.run("INSERT INTO g1_backfill (name) VALUES ('d')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*), count(DISTINCT thingy) FROM g1_backfill"),
        [["4".to_owned(), "4".to_owned()]]
    );
}

/// `uuid_generate_v1()`, which is `test_change_column_default`'s spelling and comes from
/// `uuid-ossp` rather than from core.
#[test]
fn an_extension_function_backfills_the_same_way() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_backfill ADD COLUMN thingy uuid DEFAULT uuid_generate_v1()")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(DISTINCT thingy), count(thingy) FROM g1_backfill"),
        [["3".to_owned(), "3".to_owned()]]
    );
}

/// **A later `SET DEFAULT` rewrites nothing** — measured: the three values stay exactly what the
/// backfill gave them, and only the recorded expression changes.
#[test]
fn changing_the_default_afterwards_leaves_the_rows_alone() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_backfill ADD COLUMN thingy uuid DEFAULT gen_random_uuid()")
        .unwrap();
    let before = node.rows("SELECT thingy FROM g1_backfill ORDER BY id");
    node.run("ALTER TABLE g1_backfill ALTER COLUMN thingy SET DEFAULT uuid_generate_v1()")
        .unwrap();
    assert_eq!(
        node.rows("SELECT thingy FROM g1_backfill ORDER BY id"),
        before,
        "the rows keep the values the backfill gave them"
    );
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef d \
             JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             WHERE a.attname = 'thingy' AND d.adrelid = 'g1_backfill'::regclass"
        ),
        [["uuid_generate_v1()".to_owned()]]
    );
}

/// **The control**: a constant default still takes the cheap road. Every row reads the same value
/// and it comes from the column's missing value, not from a rewrite — which is what
/// `atthasmissing` is for, and the property that must survive this change.
#[test]
fn a_constant_default_still_pads_rather_than_rewrites() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_backfill ADD COLUMN n integer NOT NULL DEFAULT 7")
        .unwrap();
    assert_eq!(
        node.rows("SELECT DISTINCT n FROM g1_backfill"),
        [["7".to_owned()]]
    );
    // The value lives on the *column*, not in the rows — which is observable without
    // `atthasmissing`: change the default and the rows that predate the column keep the old one,
    // while a row written afterwards takes the new. Measured on 19beta1: `7, 7, 9`.
    node.run("ALTER TABLE g1_backfill ALTER COLUMN n SET DEFAULT 9")
        .unwrap();
    node.run("INSERT INTO g1_backfill (name) VALUES ('d')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name, n FROM g1_backfill ORDER BY name"),
        [
            ["a".to_owned(), "7".to_owned()],
            ["b".to_owned(), "7".to_owned()],
            ["c".to_owned(), "7".to_owned()],
            ["d".to_owned(), "9".to_owned()],
        ]
    );
}

/// A table with **no** rows takes the same statement and has nothing to fill, which is the case
/// most of the suite sends.
#[test]
fn an_empty_table_is_unaffected() {
    let mut node = parity::Node::new(&["CREATE TABLE g1_empty (id bigserial primary key)"]);
    node.run("ALTER TABLE g1_empty ADD COLUMN thingy uuid NOT NULL DEFAULT gen_random_uuid()")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM g1_empty"),
        [["0".to_owned()]]
    );
    node.run("INSERT INTO g1_empty DEFAULT VALUES").unwrap();
    assert_eq!(
        node.rows("SELECT count(thingy) FROM g1_empty"),
        [["1".to_owned()]]
    );
}
