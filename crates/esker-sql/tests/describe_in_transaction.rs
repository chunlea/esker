//! `Describe` reads the session's own transaction, not a snapshot beside it.
//!
//! Run 53's `relation "…" does not exist` — 63 tests over 10 files — and a **wire-protocol
//! violation** underneath it, and they are one cause. `Describe` opened a transaction of its own,
//! so a statement prepared over a table the session had created and not yet committed was
//! described against a snapshot without it.
//!
//! `ActiveRecord`'s test cases create their tables inside a transaction — `bytea_test.rb`'s setup
//! is `@connection.transaction { @connection.create_table("bytea_data_type") { … } }`, and every
//! fixture-loading case wraps the whole test — so this is not a corner. Both halves were measured
//! against a real node before this file was written:
//!
//! ```text
//! BEGIN; CREATE TABLE probe_u (…);
//! SELECT count(*) FROM probe_u \bind      -> ERROR: relation "probe_u" does not exist
//! INSERT INTO probe_v (n) VALUES ($1) RETURNING id \bind 'x'
//!                                         -> server sent data ("D" message) without prior
//!                                            row description ("T" message)
//! ```
//!
//! The second is the worse of the two: `Describe` could not see the table, so it answered
//! `NoData`; `Execute` ran in the session's transaction, could see it, and sent rows. A client
//! that was told there would be none gets a `DataRow` it has no `RowDescription` for and drops
//! the connection — which is how one prepared `INSERT … RETURNING` takes a whole file with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// **A statement prepared over a table the open transaction created describes.**
#[test]
fn describe_sees_what_the_open_transaction_has_created() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE TABLE probe_u (id bigserial primary key, n text)")
        .unwrap();

    let parsed = parse_statements("SELECT count(*) FROM probe_u").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(
        described
            .fields
            .expect("a SELECT returns rows")
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>(),
        vec!["count"]
    );
    node.run("ROLLBACK").unwrap();
}

/// **`Describe` and `Execute` must agree about whether there are rows**, which is a protocol
/// obligation rather than a nicety: a `NoData` followed by a `DataRow` is a stream a client cannot
/// read, and `pg` drops the connection on it.
#[test]
fn describe_and_execute_agree_about_returning_in_an_open_transaction() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE TABLE probe_v (id bigserial primary key, n text)")
        .unwrap();

    let parsed = parse_statements("INSERT INTO probe_v (n) VALUES ($1) RETURNING id").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described
        .fields
        .expect("a RETURNING makes the statement row-returning");
    assert_eq!(
        fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        vec!["id"]
    );
    node.run("ROLLBACK").unwrap();
}

/// The same for a table the transaction **altered** rather than created: a column added and not
/// yet committed is a column the shape has to carry.
#[test]
fn describe_sees_a_column_the_open_transaction_added() {
    let mut node = parity::Node::new(&["CREATE TABLE probe_w (id bigint)"]);
    node.run("BEGIN").unwrap();
    node.run("ALTER TABLE probe_w ADD COLUMN extra text")
        .unwrap();

    let parsed = parse_statements("SELECT * FROM probe_w").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(
        described
            .fields
            .expect("a SELECT returns rows")
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>(),
        vec!["id", "extra"]
    );
    node.run("ROLLBACK").unwrap();
}

/// And with no transaction open it still describes, which is the path every other test here uses.
#[test]
fn describe_outside_a_transaction_is_unchanged() {
    let mut node = parity::Node::new(&["CREATE TABLE probe_x (id bigint, n text)"]);
    let parsed = parse_statements("SELECT n FROM probe_x WHERE id = $1").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(described.parameters.len(), 1);
    assert_eq!(
        described
            .fields
            .expect("a SELECT returns rows")
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>(),
        vec!["n"]
    );
}
