//! The `name` type, against PostgreSQL 19beta1.
//!
//! `datatype_test.rb#test_name_column_type` sends `CREATE TABLE ex(data name)`, and the two
//! `compatibility_test.rb` cases reach the same type through `information_schema.tables.table_name`
//! — the `sql_identifier` domain over `name`.
//!
//! It is not "a short `text`". Three things separate them and each is a test below:
//!
//! * **`typlen` is 64 and positive.** `name` is the one string type in PostgreSQL that is fixed
//!   width rather than a varlena, and it takes no typmod at all.
//! * **It truncates at 63** — one of the 64 bytes is the terminator — by *bytes*, and never
//!   through the middle of a character.
//! * **Its collation is C**, so a `name` column sorts in byte order: every capital before every
//!   lower-case letter, which is not what the same values do in a `text` column.
//!
//! Measured in `tests/captures/pg19_name_type.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The corpus's one table.
const CORPUS_FIXTURE: &[&str] = &[
    "CREATE TABLE b4_nm (id int8, data name)",
    "INSERT INTO b4_nm VALUES (1,'plain'),(2,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'),(3,''),(4,'MiXeD'),(5,'apple'),\
     (6,'Apple')",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The rows agree; what differs is what the catalog says its own columns are — and most of
    // that has since been closed.** `pg_type.typname`, `pg_attribute.attname` and
    // `information_schema.columns.column_name` are declared `name` here now (ADR 0084),
    // `typtype`/`typcategory`/`typdelim` are `"char"` (ADR 0095), and `pg_typeof` answers a
    // `regtype` on both sides (ADR 0093). What is left in these two statements is `pg_type.oid`
    // and `typcollation` answered as a `bigint` where a real server says `oid`, and `atttypid`
    // the same — the catalog-oid family, its own unit on the type-surface queue.
    types: &[
        "SELECT typname, oid, typtype, typlen, typcategory, typdelim, typcollation FROM pg_type \
         WHERE typname = 'name'",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.atttypid, a.atttypmod FROM \
         pg_attribute a WHERE a.attrelid = 'b4_nm'::regclass AND a.attnum > 0 ORDER BY a.attnum",
    ],
    answers: &[],
};

#[test]
fn every_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_name_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 24,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The statement the suite sends.**
#[test]
fn a_column_may_be_declared_name() {
    let mut node = parity::Node::new(&["CREATE TABLE ex (id int8, data name)"]);
    node.run("INSERT INTO ex VALUES (1, 'hello')").unwrap();
    assert_eq!(
        node.rows("SELECT data FROM ex"),
        vec![vec!["hello".to_owned()]]
    );
    let outcome = node.run("SELECT data FROM ex").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    // 19 is `name`, and the width is **64 and positive**: the one fixed-width string type.
    assert_eq!(fields[0].type_oid, 19);
    assert_eq!(fields[0].type_size, 64);
    assert_eq!(fields[0].type_modifier, -1);
}

/// **63, not 64**, and by bytes — but never through the middle of a character.
#[test]
fn it_truncates_at_sixty_three_bytes_on_a_character_boundary() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT length('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::name), octet_length('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::name)"),
        vec![vec!["63", "63"]]
    );
    assert_eq!(
        node.rows("SELECT 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::name = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::name"),
        vec![vec!["t"]]
    );
    // **Two bytes each, so the 32nd would cross 63 and is dropped whole**: 31 characters, 62
    // octets. A truncation that counted bytes and stopped would leave half a character here.
    assert_eq!(
        node.rows("SELECT length('éééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé'::name), octet_length('éééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé'::name)"),
        vec![vec!["31", "62"]]
    );
    // Three bytes each, and 21 of them is exactly 63.
    assert_eq!(
        node.rows("SELECT length('ああああああああああああああああああああああああああああああ'::name), octet_length('ああああああああああああああああああああああああああああああ'::name)"),
        vec![vec!["21", "63"]]
    );
    // And a value written into a column is truncated on the way in, not on the way out.
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows("SELECT length(data) FROM b4_nm WHERE id = 2"),
        vec![vec!["63"]]
    );
}

/// **A `name` column sorts in byte order**, which is what its C collation means.
#[test]
fn a_name_column_sorts_in_byte_order() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows("SELECT id FROM b4_nm ORDER BY data, id"),
        vec![
            vec!["3"],
            vec!["6"],
            vec!["4"],
            vec!["2"],
            vec!["5"],
            vec!["1"]
        ],
        "the empty string sorts first and every capital before every lower-case letter"
    );
}

/// Everything it does is text's, and everything it produces is `text`.
#[test]
fn it_compares_with_text_and_produces_text() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'plain'::name = 'plain'::text, 'plain'::name LIKE 'pl%'"),
        vec![vec!["t", "t"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof('x'::name || 'y'), pg_typeof(upper('a'::name))"),
        vec![vec!["text", "text"]]
    );
    assert_eq!(node.rows("SELECT length('abc'::name)"), vec![vec!["3"]]);
    // Spaces are kept — it is not `bpchar` — and the empty string is a legal value.
    assert_eq!(
        node.rows("SELECT ('  x  '::name)::text, length('  x  '::name)"),
        vec![vec!["  x  ", "5"]]
    );
    assert_eq!(
        node.rows("SELECT (''::name)::text, length(''::name)"),
        vec![vec!["", "0"]]
    );
}

/// **It takes no typmod at all**, which is its own refusal and not a range error.
#[test]
fn it_takes_no_type_modifier() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("SELECT 'x'::name(10)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert_eq!(
        error.to_string(),
        "type modifier is not allowed for type \"name\""
    );
}

/// The catalog describes it the way a client reads it.
#[test]
fn the_catalog_describes_the_column() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    assert_eq!(
        node.rows(
            // **`attlen` is not asserted here and it is not an oversight**: `pg_attribute` on
            // this node has no such column, which is a catalog gap and g1's ground, not this
            // type's. The width a client actually reads is the `RowDescription`'s, asserted in
            // `a_column_may_be_declared_name` above.
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.atttypid, a.atttypmod \
             FROM pg_attribute a WHERE a.attrelid = 'b4_nm'::regclass AND a.attnum > 0 \
             AND a.attname = 'data'"
        ),
        vec![vec!["data", "name", "19", "-1"]]
    );
    assert_eq!(
        node.rows(
            "SELECT data_type, character_maximum_length FROM information_schema.columns \
             WHERE table_name = 'b4_nm' AND column_name = 'data'"
        ),
        vec![vec!["name", "\\N"]]
    );
    assert_eq!(
        node.rows("SELECT typname, typlen, typcategory FROM pg_type WHERE typname = 'name'"),
        vec![vec!["name", "64", "S"]]
    );
}
