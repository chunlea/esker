//! `bigint_array` — the table `postgresql_specific_schema.rb` declares — end to end.
//!
//! `tests/array.rs` is the study of what arrays are; this is the statement that was blocking, and
//! it is here as its own file so that a regression in it is unmistakable: `ActiveRecord` cannot
//! load a single suite file until this table can be created, filled and described.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_attribute.attname` is a `name` on a real server and `text` here, whose values are
    // identical — the trade every `pg_catalog` column makes. **Every row in this corpus agrees**,
    // this one included; what differs is the OID a client is told for one column of one catalog
    // query, and `format_type`'s own answer beside it is right.
    types: &[],
    answers: &[],
};

#[test]
fn the_rails_array_table_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_column.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}
