//! `pg_locks`: who holds a row lock on this node, and who is waiting for one.
//!
//! Built for a specific failure. Ten passes of the Rails suite have lost a file to a request the
//! node never answered, and at the moment of the hang the server could not be asked who else was
//! connected or what they held: `pg_stat_activity` answers only the asking session, and `pg_locks`
//! did not exist. The harness's forensics had to ask the operating system instead.
//!
//! The shape is PostgreSQL 19's, measured with one session holding `SELECT … FOR UPDATE` and a
//! second blocked behind it: the holder is a `tuple` row with `granted = true`, and **the waiter is
//! a `transactionid` row with `granted = false`** naming the transaction it waits for. Reading the
//! two together is how a stuck session's counterpart is found.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::{Pair, edge, reached};

/// **An idle node holds nothing, and says so in sixteen columns.**
///
/// The empty answer matters as much as the full one: it is what the harness reads when it wants to
/// know that nothing is held, and a view that could not be selected from at all — which is what
/// `pg_locks` was — cannot say that.
#[test]
fn an_idle_node_holds_no_locks_and_the_view_has_postgresql_s_columns() {
    let mut node = parity::Node::new(&["CREATE TABLE lk (id bigint primary key, n text)"]);
    assert!(node.rows("SELECT * FROM pg_locks").is_empty());
    // The columns a client selects by name, in PostgreSQL's order.
    assert!(
        node.run(
            "SELECT locktype, database, relation, page, tuple, virtualxid, transactionid, \
             classid, objid, objsubid, virtualtransaction, pid, mode, granted, fastpath, \
             waitstart FROM pg_locks"
        )
        .is_ok()
    );
    // And it is a relation like any other: `pg_catalog.pg_locks` resolves, and so does the bare
    // name, which is what a schema dump and a `::regclass` both need.
    assert!(node.rows("SELECT count(*) FROM pg_catalog.pg_locks")[0][0] == "0");
}

/// **A held row is one `tuple` row, and a blocked writer is one `transactionid` row that is not
/// granted.**
///
/// The assertion this view exists for: with A holding and B blocked, a *third* session can see both
/// of them — which is the question the harness could not ask when a file hung.
#[test]
fn a_held_row_and_its_waiter_are_both_visible_to_a_third_session() {
    let pair = Pair::new(&[
        "CREATE TABLE lk (id bigint primary key, n text)",
        "INSERT INTO lk VALUES (1, 'a')",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to write");
        b.run("UPDATE lk SET n = 'b' WHERE id = 1")
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(300));

    let mut watcher = pair.session();
    let rows = watcher.rows(
        "SELECT locktype, granted, mode, relation IS NOT NULL, virtualtransaction \
         FROM pg_locks ORDER BY locktype",
    );
    assert_eq!(rows.len(), 2, "one holder and one waiter: {rows:?}");
    assert_eq!(
        rows[0][..4].to_vec(),
        vec!["transactionid", "f", "ShareLock", "f"],
        "the waiter is not granted and names a transaction rather than a relation"
    );
    assert_eq!(
        rows[1][..4].to_vec(),
        vec!["tuple", "t", "ExclusiveLock", "t"],
        "the holder is granted and names the relation"
    );
    assert_ne!(
        rows[0][4], rows[1][4],
        "the two are different transactions, which is the whole point of the column"
    );

    a.run("COMMIT").unwrap();
    waiter.join().unwrap().unwrap();
    // And when nobody holds anything, it is empty again — a lock that outlived its session would
    // show here, which is the regression this view makes visible rather than fatal.
    assert!(watcher.rows("SELECT * FROM pg_locks").is_empty());
}
