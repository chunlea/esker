//! Contract C3 for `RETURNING`: replay `tests/corpus/pg19_returning.txt` and require our answer.
//!
//! Sixteen statements put to a real PostgreSQL 19beta1 over a fixture this test rebuilds, in
//! order and **statefully** — an `INSERT` in the corpus is a row the next line can see, so the
//! replay does not reorder, skip, or run them independently. That is the point of a corpus rather
//! than a table of assertions: the two things `RETURNING` is easy to get wrong are both about
//! *which* row it sees, and neither shows up in a statement run in isolation.
//!
//! There are no divergences. Every type, every value and both refusals agree, which is what a
//! `RETURNING` built out of the same target-list lowering a `SELECT` uses ought to buy — and the
//! assertion that the list is empty is what keeps it that way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute as _, Outcome};
use esker_sql::value::PgType;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The fixture the corpus was captured over.
const FIXTURE: &[&str] = &[
    "CREATE TABLE r (id int8 PRIMARY KEY, n int8, t text DEFAULT 'd', b bool)",
    "INSERT INTO r VALUES (1,10,'a',true),(2,20,'b',false),(3,NULL,NULL,NULL)",
];

#[test]
fn every_returning_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_returning.txt"),
        FIXTURE,
        &parity::Divergences::default(),
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The tag is the same with a `RETURNING` as without it, because it counts rows written and the
/// result set is a second thing the statement produced rather than a different thing it did.
///
/// Its own test because the corpus cannot hold it: `psql` prints a result set's rows where it
/// would have printed the tag, so the container could not be asked what tag it sent. Every tag
/// below is the one `tests/insert.rs` and `tests/update_delete.rs` already pin for the same
/// statement **without** a `RETURNING` — which is the claim being made, and it is a claim about
/// this node rather than a recollection about that one.
#[test]
fn a_returning_does_not_change_the_command_tag() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, expected) in [
        ("INSERT INTO r (id) VALUES (50) RETURNING id", "INSERT 0 1"),
        (
            "INSERT INTO r (id) VALUES (51), (52) RETURNING id",
            "INSERT 0 2",
        ),
        ("UPDATE r SET n = 1 WHERE id = 50 RETURNING id", "UPDATE 1"),
        ("UPDATE r SET n = 1 WHERE id = 999 RETURNING id", "UPDATE 0"),
        ("DELETE FROM r WHERE id = 50 RETURNING id", "DELETE 1"),
        ("DELETE FROM r WHERE id = 999 RETURNING id", "DELETE 0"),
    ] {
        let Outcome::Rows { tag, .. } = node.run(sql).unwrap() else {
            panic!("{sql} did not return rows");
        };
        assert_eq!(tag, expected, "{sql}");
    }
}

/// A `RETURNING` makes a write statement row-returning, and a client that prepares one asks for
/// its shape before it binds. Answering "no columns" and then sending some is the one thing a
/// `Describe` exists to prevent.
#[test]
fn a_prepared_returning_describes_its_columns() {
    let mut node = parity::Node::new(FIXTURE);
    let parsed = parse_statements("INSERT INTO r (id) VALUES (60) RETURNING id, t")
        .unwrap()
        .remove(0);
    let described = node.executor.describe(&parsed, &[]).unwrap();
    let fields = described.fields.expect("a RETURNING describes its columns");
    assert_eq!(
        fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        ["id", "t"]
    );
    assert_eq!(fields[0].type_oid, esker_sql::value::ColumnType::Int8.oid());
    assert_eq!(fields[1].type_oid, esker_sql::value::ColumnType::Text.oid());

    // And a statement without one still describes as no columns at all.
    let parsed = parse_statements("INSERT INTO r (id) VALUES (61)")
        .unwrap()
        .remove(0);
    assert!(
        node.executor
            .describe(&parsed, &[])
            .unwrap()
            .fields
            .is_none()
    );
}

/// Resolution happens before the first row is written, so a `RETURNING` naming nothing leaves the
/// table as it was. A statement that half-ran and then failed is the shape of bug that is hardest
/// to see, because the error message is about the clause and the damage is in the rows.
#[test]
fn a_returning_that_names_nothing_writes_nothing() {
    let mut node = parity::Node::new(FIXTURE);
    let before = node.rows("SELECT count(*) FROM r");
    node.run("INSERT INTO r (id) VALUES (70), (71) RETURNING nope")
        .unwrap_err();
    assert_eq!(node.rows("SELECT count(*) FROM r"), before);
}
