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

/// **The same sequence across two nodes, and it must end the way one node's ends.**
///
/// This test replaced the measurement that came before it, and the measurement's own words are why:
/// *"A design that makes `FOR UPDATE` exclude across nodes turns both of these red, which is the
/// point of writing them down now."* What it recorded — both crossed writes accepted, both
/// transactions committed, rows `(1, 9)` and `(2, 9)`, `pg_locks` empty — is in
/// [ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md) and in this file's module docs,
/// which is where a measurement belongs once it has been acted on.
///
/// **Two threads, like the control**, and for the same reason: once the lock is real, one of the
/// two blocks, and a single thread that interleaves the statements has only one waiter — the first
/// crossed write never returns. That is a hang, not a measurement.
///
/// The assertion is the outcome and not the site. Which statement tells the victim depends on
/// whether it learns at the lock or at its commit, and both are honest answers to "one of you has
/// to die"; what is not negotiable is that **exactly one transaction commits** and the other is
/// told `40P01`.
#[test]
fn two_nodes_crossing_a_lock_leave_one_victim() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 1), (2, 2)").unwrap();

    let (a_says, hears_a) = std::sync::mpsc::channel();
    let (b_says, hears_b) = std::sync::mpsc::channel();
    let here = &cluster;
    let (mine, theirs) = std::thread::scope(|scope| {
        let far = scope.spawn(move || {
            let mut b = on_a_second_node(here);
            let out = one_half(&mut b, 2, 1, &b_says, &hears_a);
            println!("B (second node): {out}");
            out
        });
        let mut a = cluster.session();
        let mine = one_half(&mut a, 1, 2, &a_says, &hears_b);
        println!("A (first node):  {mine}");
        (mine, far.join().unwrap())
    });

    let mut after = cluster.session();
    let rows = after.rows("SELECT id, n FROM lk ORDER BY id");
    println!("rows afterwards: {rows:?}");

    let committed = [&mine, &theirs].into_iter().filter(|o| *o == "ok").count();
    assert_eq!(
        committed, 1,
        "exactly one of the two commits: A said {mine}, B said {theirs}"
    );
    let victim = [&mine, &theirs]
        .into_iter()
        .find(|outcome| *outcome != "ok")
        .expect("the other one is the victim");
    assert!(
        victim.contains("[40P01]"),
        "the victim is told it deadlocked, not something else: {victim}"
    );
    // One row moved and one did not, which is what one committed transaction looks like. Which
    // row it is depends on which transaction survived, so the assertion is the shape.
    let moved = rows
        .iter()
        .filter(|row| row[1] == Some("9".to_owned()))
        .count();
    assert_eq!(
        moved, 1,
        "exactly one transaction's write is visible: {rows:?}"
    );
}

/// One session's whole part of the crossed sequence, mirrored: lock `mine`, wait for the other
/// side to have locked theirs, then write `theirs` and commit. Answers `ok`, or the error that
/// stopped it — whichever statement that was.
fn one_half(
    node: &mut Session,
    mine: i64,
    theirs: i64,
    says: &std::sync::mpsc::Sender<&'static str>,
    hears: &std::sync::mpsc::Receiver<&'static str>,
) -> String {
    node.run("BEGIN").unwrap();
    node.run("SET lock_timeout = '20s'").unwrap();
    node.run(&format!("SELECT n FROM lk WHERE id = {mine} FOR UPDATE"))
        .unwrap();
    says.send("locked my row").unwrap();
    hears
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the other session never locked its row");

    if let Some(error) = node
        .run(&format!("UPDATE lk SET n = 9 WHERE id = {theirs}"))
        .err()
    {
        let _ = node.run("ROLLBACK");
        return format!("at the crossed write: {}", said(Some(error)));
    }
    if let Some(error) = node.run("COMMIT").err() {
        let _ = node.run("ROLLBACK");
        return format!("at COMMIT: {}", said(Some(error)));
    }
    "ok".to_owned()
}

/// **A lock that committed did not change the row, so it refuses nobody**
/// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md), the rule this file's build
/// reversed).
///
/// `SELECT … FOR UPDATE` leaves a `Kind::Lock` record in the `write` column family when its
/// transaction commits, and that record used to count as a commit in the prewrite conflict check.
/// The rule the check exists for is first-committer-wins — *somebody wrote a version of this key
/// after my snapshot, so the value I computed from it is stale* — and a lock record is the
/// transaction saying it held the key and wrote **nothing** to it. Nothing moved, so nothing is
/// stale.
///
/// **`REPEATABLE READ` is what makes this askable at all.** Under `READ COMMITTED` the writer takes
/// a fresh snapshot per statement (ADR 0057), which would be *after* the lock committed, and the
/// question never arises. The older transaction here keeps one snapshot for its whole life, so the
/// lock record is unambiguously above it.
///
/// **This test passes under the old rule too, and that is recorded rather than hidden.** The
/// counterfactual — the `Kind::Lock` skip taken back out of `newest_write_after` — leaves it green,
/// because the SQL write path asks `changed_since_statement` before it asks to prewrite and that
/// question does not reach the conflict check on this shape. What discriminates the two rules is
/// `esker-txn`'s `a_lock_kind_record_is_neither_a_version_nor_a_conflict` at the layer the decision
/// lives at, and `two_nodes_crossing_a_lock_leave_one_victim` where the old rule's damage reaches a
/// client. This one says what a user meets; it is not the proof, and calling a green a proof is the
/// mistake this comment exists to stop.
#[test]
fn a_committed_lock_record_does_not_refuse_an_older_writer() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 1)").unwrap();

    // The older transaction, and its snapshot is taken here and kept.
    let mut older = cluster.session();
    older.run("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(
        older.rows("SELECT n FROM lk WHERE id = 1"),
        vec![vec![Some("1".to_owned())]]
    );

    // A locking read that takes the row, holds it, and commits — leaving the record and no value.
    let mut holder = cluster.session();
    holder.run("BEGIN").unwrap();
    holder
        .run("SELECT n FROM lk WHERE id = 1 FOR UPDATE")
        .unwrap();
    holder.run("COMMIT").unwrap();

    // And now the older transaction writes the row it read. Its snapshot is behind a commit that
    // changed nothing, which is not a reason to refuse it.
    older
        .run("UPDATE lk SET n = 5 WHERE id = 1")
        .expect("a lock that wrote nothing must not make an older writer stale");
    older
        .run("COMMIT")
        .expect("and it must not refuse the commit either");

    let mut after = cluster.session();
    assert_eq!(
        after.rows("SELECT n FROM lk WHERE id = 1"),
        vec![vec![Some("5".to_owned())]]
    );
}

/// **The other half: while the lock is held, a write from another node may not pass it.**
///
/// The face above says a lock record refuses nobody *after* it commits; this says the lock itself
/// excludes *during*, which is the whole point of (a') and the thing measured missing. The writer
/// is the **younger** of the two, so wound-wait sends it to wait rather than to kill — and the
/// holder never lets go, so waiting is all it can do until its budget runs out.
///
/// It is a second node deliberately: the writer's own node-local table would stop it on the first
/// node without the store being involved at all, and the store is what is on trial.
#[test]
fn a_write_may_not_pass_a_lock_another_node_holds() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 1)").unwrap();

    // The holder, older, and it never releases inside this test.
    let mut holder = cluster.session();
    holder.run("BEGIN").unwrap();
    holder
        .run("SELECT n FROM lk WHERE id = 1 FOR UPDATE")
        .unwrap();

    // A younger writer on a second node. Its `UPDATE` is buffered — the write path takes the
    // node's own table and Percolator's prewrite is what excludes the other node — so the wait is
    // at the commit, which is where the lock is met.
    let mut writer = on_a_second_node(&cluster);
    writer.run("BEGIN").unwrap();
    writer.run("UPDATE lk SET n = 9 WHERE id = 1").unwrap();
    let refused = writer
        .run("COMMIT")
        .expect_err("a write passed a lock another node was holding");
    assert_eq!(
        refused.sqlstate(),
        esker_sql::sqlstate::SERIALIZATION_FAILURE,
        "the writer waited the holder out and then gave up, which is `40001`: {refused}"
    );
    let _ = writer.run("ROLLBACK");

    // The holder still has the row it was holding, and nothing was written over it.
    holder.run("ROLLBACK").unwrap();
    let mut after = cluster.session();
    assert_eq!(
        after.rows("SELECT n FROM lk WHERE id = 1"),
        vec![vec![Some("1".to_owned())]]
    );
}
