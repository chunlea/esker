//! Which typmods a rendered default carries, against PostgreSQL 19beta1.
//!
//! `pg_get_expr` deparses a stored default by printing the value and casting it to a type. **The
//! type is written bare for everything except `interval`**, which carries its precision —
//! measured over every typmod-bearing type at once in `tests/captures/pg19_typmod_default.txt`,
//! because a rule read off one row is a rule about that row.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE tmd (a interval(3) DEFAULT '3 years', b interval DEFAULT '3 years', \
     c numeric(6,2) DEFAULT 1.5, e varchar(3) DEFAULT 'ab', \
     g timestamp(1) DEFAULT '2020-01-01 00:00:00.55', i time(2) DEFAULT '01:02:03.456')",
];

fn default_of(node: &mut parity::Node, column: &str) -> String {
    let rows = node.rows(&format!(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a \
         ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = 'tmd'::regclass \
         AND a.attname = '{column}'"
    ));
    rows.first()
        .and_then(|row| row.first())
        .cloned()
        .unwrap_or_default()
}

/// **The one that carries it.**
#[test]
fn an_interval_default_carries_its_precision() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(default_of(&mut node, "a"), "'3 years'::interval(3)");
    // And one with no precision does not grow one.
    assert_eq!(default_of(&mut node, "b"), "'3 years'::interval");
}

/// **And the four that do not**, which is what makes the rule a rule rather than a row.
#[test]
fn every_other_typmod_bearing_type_writes_the_bare_name() {
    let mut node = parity::Node::new(FIXTURE);
    // A numeric constant prints unquoted and takes no cast at all.
    assert_eq!(default_of(&mut node, "c"), "1.5");
    assert_eq!(default_of(&mut node, "e"), "'ab'::character varying");
    assert_eq!(
        default_of(&mut node, "g"),
        "'2020-01-01 00:00:00.55'::timestamp without time zone"
    );
    assert_eq!(
        default_of(&mut node, "i"),
        "'01:02:03.456'::time without time zone"
    );
}
