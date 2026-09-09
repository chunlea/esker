//! **What two SQL nodes do when they lock each other's rows** — the measurement behind
//! `debts-v1.1` #4, and nothing else.
//!
//! `backend::locks`'s own note is the state of the world: *"Node-local, which is every deadlock two
//! sessions of one `esker-sql` process can make. A cycle across nodes needs a graph both can see —
//! PD's job, and a named follow-on."* This file is that follow-on's first half: it writes down what
//! happens today, so a design can be argued against an answer rather than against a guess.
//!
//! **Two backends, not two processes**, and they are the same thing for this question:
//! `StoreBackend::new` builds its own `RowLocks` (`backend/store.rs`), so two backends over one
//! cluster are two independent wait-for graphs over one store — which is exactly what two
//! `esker-sql` processes are. What a second OS process would add is a second pgwire socket, and the
//! lock tables are what this measures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::sync::Arc;
use std::time::Instant;

use cluster::{Cluster, Session, TENANT};
use esker_sql::exec::Executor;

/// A session on a **second** node: its own client, its own `StoreBackend`, and therefore its own
/// lock table — which is the whole of what a second `esker-sql` process is for this question.
///
/// **No schema lease source**, exactly as `Cluster`'s own backend is built. Attaching a `PdLease`
/// with no placement driver to refresh it makes the node read-only within seconds — `25006 this
/// node's schema lease has expired` — and a node that cannot write cannot take part in a deadlock.
/// That is how the first version of this measurement measured nothing.
fn on_a_second_node(cluster: &Cluster) -> Session {
    let backend: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::StoreBackend::new(
            cluster.another_client(),
            Arc::clone(&cluster.oracle),
        ));
    Session {
        executor: Executor::new(
            backend,
            Arc::clone(&cluster.catalog),
            TENANT,
            esker_sql::session::register(),
        ),
    }
}

fn said(error: Option<esker_sql::SqlError>) -> String {
    error.map_or_else(
        || "ok".to_owned(),
        |error| format!("{error} [{}]", error.sqlstate()),
    )
}

/// **The control: two sessions of *one* node, crossing their locks.**
///
/// Two threads, and it has to be two: a cycle needs a *second* waiter, and a single thread that
/// interleaves the statements can only ever have one — the first `UPDATE` blocks and the second
/// never runs. The first version of this hung for that reason, which is worth writing down because
/// the cross-node measurement below is single-threaded and correct to be: there, nothing blocks.
#[test]
fn one_node_answers_a_crossed_lock_with_a_deadlock() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 1), (2, 2)").unwrap();

    let (a_says, hears_a) = std::sync::mpsc::channel();
    let (b_says, hears_b) = std::sync::mpsc::channel();
    let here = &cluster;
    let answers = std::thread::scope(|scope| {
        let far = scope.spawn(move || {
            let mut b = here.session();
            b.run("BEGIN").unwrap();
            b.run("SET lock_timeout = '20s'").unwrap();
            b.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE").unwrap();
            b_says.send("B holds row 2").unwrap();
            hears_a
                .recv_timeout(std::time::Duration::from_secs(20))
                .unwrap();
            let answer = b.run("UPDATE lk SET n = 9 WHERE id = 1").err();
            let _ = b.run("ROLLBACK");
            answer
        });

        let mut a = cluster.session();
        a.run("BEGIN").unwrap();
        a.run("SET lock_timeout = '20s'").unwrap();
        a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
        a_says.send("A holds row 1").unwrap();
        hears_b
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        let mine = a.run("UPDATE lk SET n = 9 WHERE id = 2").err();
        let _ = a.run("ROLLBACK");
        let theirs = far.join().unwrap();
        [mine, theirs]
            .into_iter()
            .flatten()
            .map(|error| format!("{error} [{}]", error.sqlstate()))
            .collect::<Vec<String>>()
    });
    assert_eq!(
        answers,
        vec!["deadlock detected [40P01]".to_string()],
        "one node's crossed lock is one `40P01` and one survivor"
    );
}

/// **The measurement: the same sequence across two nodes**, each with its own lock table.
///
/// What it found, and it is larger than the debt's name:
///
/// ```text
/// A crossed-write:  ok in 2.1 ms      B crossed-write:  ok in 1.5 ms
/// A commit:         ok                B commit:         ok
/// rows afterwards:  (1, 9), (2, 9)    -- both writes committed
/// ```
///
/// **Nothing waited and nothing was detected, because nothing was locked.** A's `FOR UPDATE` on
/// row 1 is a row in A's own table and invisible to B, so B takes row 1 without pausing.
/// PostgreSQL, given this sequence, deadlocks and kills one. So the gap is not an undetected cycle
/// — there is no cycle, because there is no wait — it is that `SELECT … FOR UPDATE` is not a
/// cluster-wide lock. Percolator's first-committer-wins is not the backstop either: it fires on two
/// transactions writing the **same** key, and here they write different ones.
///
/// Asserted only where the assertion is about today's behaviour rather than tomorrow's design:
/// that both writes are accepted, and that the answer differs from the one-node control above.
/// `docs/adr/0088-a-row-lock-across-nodes.md` is the draft this measurement is for.
#[test]
fn two_nodes_crossing_a_lock_are_measured() {
    let cluster = Cluster::start();
    let mut a = cluster.session();
    a.run("CREATE TABLE lk (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    a.run("INSERT INTO lk VALUES (1, 1), (2, 2)").unwrap();
    let mut b = on_a_second_node(&cluster);

    a.run("BEGIN").unwrap();
    b.run("BEGIN").unwrap();
    a.run("SET lock_timeout = '5s'").unwrap();
    b.run("SET lock_timeout = '5s'").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    b.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE").unwrap();

    let at = Instant::now();
    let a_second = a.run("UPDATE lk SET n = 9 WHERE id = 2").err();
    let a_wrote_in = at.elapsed();
    let at = Instant::now();
    let b_second = b.run("UPDATE lk SET n = 9 WHERE id = 1").err();
    let b_wrote_in = at.elapsed();

    let at = Instant::now();
    let a_commit = a.run("COMMIT").err();
    let a_committed_in = at.elapsed();
    let at = Instant::now();
    let b_commit = b.run("COMMIT").err();
    let b_committed_in = at.elapsed();

    println!("--- two nodes, each with its own wait-for graph ---");
    let a_said = said(a_second);
    println!("A crossed-write: {a_said} in {a_wrote_in:?}");
    let b_said = said(b_second);
    println!("B crossed-write: {b_said} in {b_wrote_in:?}");
    println!("A commit:        {} in {a_committed_in:?}", said(a_commit));
    println!("B commit:        {} in {b_committed_in:?}", said(b_commit));
    let mut after = cluster.session();
    let rows = after.rows("SELECT id, n FROM lk ORDER BY id");
    println!("rows afterwards: {rows:?}");
    // The two assertions this file will keep whatever is decided: neither write was refused, and
    // both landed. A design that makes `FOR UPDATE` exclude across nodes turns both of these red,
    // which is the point of writing them down now.
    assert_eq!(a_said, "ok", "A's crossed write was refused");
    assert_eq!(b_said, "ok", "B's crossed write was refused");
    assert_eq!(
        rows,
        vec![
            vec![Some("1".to_owned()), Some("9".to_owned())],
            vec![Some("2".to_owned()), Some("9".to_owned())],
        ],
        "both transactions committed, which one node's control forbids"
    );
    println!(
        "pg_locks on A: {:?}",
        a.rows("SELECT locktype, granted FROM pg_locks")
    );
}
