//! **A prepared `INSERT` must not consume a sequence value while it is being described.**
//!
//! Run 86's whole-file trace against the real binary: in a single pass with nothing else running,
//! `range_test.rb`'s created row steps by **+32 on every test** — one whole
//! `catalog::SEQUENCE_BATCH` per test — where PostgreSQL gives id 1 at every position. The `33`
//! and `737` seen earlier were that ordinary block step read out of context, not a race.
//!
//! What the file does that every probe so far did not: `ActiveRecord` prepares by default, so it
//! speaks the **extended protocol** — `Parse`/`Describe`/`Bind`/`Execute` — while `psql` probes
//! and the in-process harness send simple queries. `Describe` is answered by
//! `Executor::describe`, which lowers and *plans* the statement to work out its shape. If
//! anything on that path evaluates a column default, a `nextval` is drawn and a whole block is
//! reserved for a statement that has not run.
//!
//! So the assertion is the one PostgreSQL always satisfies: **describing a statement changes
//! nothing**. The first row inserted afterwards is id 1, whatever was described before it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The id the first row of a fresh `bigserial` table gets on a real server, always.
const FIRST: &str = "1";

/// The one column of the one row of a single-value query, as text.
fn only(node: &mut parity::Node, sql: &str) -> String {
    let rows = node.rows(sql);
    rows.first()
        .and_then(|row| row.first())
        .cloned()
        .unwrap_or_else(|| panic!("{sql} answered no value"))
}

#[test]
fn describing_an_insert_does_not_consume_a_sequence_value() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ps (id bigserial primary key, v int8)")
        .unwrap();

    // The two shapes `ActiveRecord` prepares against a table with a serial key: the fixture insert
    // that supplies the id itself, and the `create!` that leaves it to the sequence.
    for sql in [
        "INSERT INTO ps (id, v) VALUES ($1, $2)",
        "INSERT INTO ps (v) VALUES ($1)",
        "INSERT INTO ps (v) VALUES ($1) RETURNING id",
    ] {
        let parsed = parse_statements(sql).unwrap();
        node.executor.describe(&parsed[0], &[]).unwrap();
    }

    // Nothing has been executed, so nothing may have been drawn.
    assert_eq!(
        only(&mut node, "SELECT last_value, is_called FROM ps_id_seq"),
        FIRST,
        "describing a statement drew from the sequence: last_value moved"
    );
    assert_eq!(
        only(&mut node, "SELECT is_called FROM ps_id_seq"),
        "f",
        "describing a statement drew from the sequence: is_called was set"
    );

    // And the first row really is 1.
    assert_eq!(
        only(&mut node, "INSERT INTO ps (v) VALUES (7) RETURNING id"),
        FIRST,
        "the first row of a fresh bigserial table is 1, whatever was described before it"
    );
}

/// The same question one layer out: **describe, then execute, then describe again**, which is what
/// a pooled client does statement after statement.
///
/// If a block is burned per prepare, the ids step by `SEQUENCE_BATCH` — which is exactly the
/// `+32 on every test` the file shows.
#[test]
fn preparing_the_same_insert_again_does_not_skip_a_block() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE pr (id bigserial primary key, v int8)")
        .unwrap();

    let mut ids = Vec::new();
    for round in 0..4 {
        let parsed = parse_statements("INSERT INTO pr (v) VALUES ($1) RETURNING id").unwrap();
        node.executor.describe(&parsed[0], &[]).unwrap();
        ids.push(only(
            &mut node,
            &format!("INSERT INTO pr (v) VALUES ({round}) RETURNING id"),
        ));
    }

    assert_eq!(
        ids,
        ["1", "2", "3", "4"],
        "a prepare between two inserts skipped a block; PostgreSQL numbers them consecutively"
    );
}
