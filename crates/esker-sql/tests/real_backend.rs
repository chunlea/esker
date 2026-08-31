//! SQL against a **real** three-store cluster, over real sockets, through Percolator.
//!
//! Everything else in this crate proves the executor against `MemoryBackend`. This proves the
//! wiring, which is a different question and the only one a fake cannot answer: whether a
//! statement whose catalog and rows live in *different regions* commits as one transaction,
//! whether a range this crate walks really spans the stores that hold it, and whether a lost race
//! comes back as the error the user caused rather than the one the storage layer saw.
//!
//! `cluster::Cluster` divides the key space at the namespace bytes, so **every statement here is
//! already a cross-region transaction** — see that module for why.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use cluster::Cluster;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;

/// The first end-to-end transaction: DDL and DML through Percolator, with the catalog in one
/// region and the rows in another.
#[test]
fn a_statement_commits_through_percolator_across_regions() {
    let cluster = Cluster::start();
    let mut session = cluster.session();

    // `CREATE TABLE` writes the table record, its name, its `_pkey` name and the version counter
    // -- four keys in the catalog region -- and takes a relation id from a counter there too.
    let outcome = session
        .run("CREATE TABLE accounts (id int8 PRIMARY KEY, email text UNIQUE, balance int8)")
        .unwrap();
    assert_eq!(outcome, Outcome::done("CREATE TABLE"));

    // A second session, over the same stores, sees it. This is the catalog crossing the wire:
    // nothing of that table is in this process.
    let mut other = cluster.session();
    assert_eq!(
        other
            .run("INSERT INTO accounts VALUES (1, 'a@x', 100)")
            .unwrap(),
        Outcome::done("INSERT 0 1")
    );

    // The row and its unique index entry are in the row region; the catalog read that resolved
    // the table was in the catalog region. One transaction, two regions, a primary in one of them.
    assert_eq!(
        session.rows("SELECT id, email, balance FROM accounts"),
        [[
            Some("1".to_owned()),
            Some("a@x".to_owned()),
            Some("100".to_owned())
        ]]
    );
}

/// A read of a whole table walks the region that holds it, and the paging this crate does on top
/// reaches every row rather than the first page.
#[test]
fn a_scan_reads_every_row_a_real_store_holds() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE t (id int8 PRIMARY KEY, n int8)")
        .unwrap();

    // More rows than one page of anything: the client's default scan limit is 1024 and the
    // executor's chunk is the same, so this crosses both.
    let values: Vec<String> = (1..=1500).map(|id| format!("({id},{id})")).collect();
    for chunk in values.chunks(250) {
        session
            .run(&format!("INSERT INTO t VALUES {}", chunk.join(",")))
            .unwrap();
    }

    assert_eq!(session.rows("SELECT id FROM t").len(), 1500);
    assert_eq!(
        session.rows("SELECT id FROM t ORDER BY id DESC LIMIT 1"),
        [[Some("1500".to_owned())]]
    );

    // And `DROP TABLE` takes all of them, which is the paging fix against a real store rather
    // than against a fake told to answer short.
    session.run("DROP TABLE t").unwrap();
    session
        .run("CREATE TABLE t (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    assert_eq!(
        session.rows("SELECT id FROM t").len(),
        0,
        "no rows survived"
    );
}

/// A duplicate that loses the race comes back as the `23505` the user caused, not as the `40001`
/// the storage layer saw — through a real `Prewrite`, which is where the key that lost is named.
#[test]
fn a_lost_race_on_a_unique_index_is_a_duplicate_key() {
    let cluster = Cluster::start();
    let mut first = cluster.session();
    first
        .run("CREATE TABLE u (id int8 PRIMARY KEY, email text UNIQUE)")
        .unwrap();

    let mut second = cluster.session();

    // Both transactions open before either writes, so neither can see the other's row and the
    // read each does finds the index key absent. Exactly one may commit.
    first.executor.begin().unwrap();
    second.executor.begin().unwrap();
    first.run("INSERT INTO u VALUES (1, 'same@x')").unwrap();
    second.run("INSERT INTO u VALUES (2, 'same@x')").unwrap();

    first.executor.commit().unwrap();
    let error = second.executor.commit().unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::UNIQUE_VIOLATION,
        "the loser was told {error}, not that it duplicated a key"
    );
    assert!(error.to_string().contains("u_email_key"), "{error}");

    // And the winner's row is the one that is there.
    assert_eq!(
        first.rows("SELECT id FROM u"),
        [[Some("1".to_owned())]],
        "one row, from the transaction that won"
    );
}

/// A committed duplicate is caught by the read, before anything is written — the other half of the
/// unique-index ruling, over the real store.
#[test]
fn a_committed_duplicate_is_caught_before_anything_is_written() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE u (id int8 PRIMARY KEY, email text UNIQUE)")
        .unwrap();
    session.run("INSERT INTO u VALUES (1, 'a@x')").unwrap();

    let error = session.run("INSERT INTO u VALUES (2, 'a@x')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert_eq!(
        session.rows("SELECT id FROM u"),
        [[Some("1".to_owned())]],
        "the refused insert wrote nothing"
    );
}

/// A transaction that rolls back leaves nothing behind on any store, and the catalog cache does
/// not publish what it abandoned.
#[test]
fn a_rollback_leaves_nothing_on_any_store() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    session.run("INSERT INTO t VALUES (1)").unwrap();

    session.executor.begin().unwrap();
    session.run("INSERT INTO t VALUES (2)").unwrap();
    session
        .run("CREATE TABLE ghost (id int8 PRIMARY KEY)")
        .unwrap();
    session.executor.rollback().unwrap();

    assert_eq!(session.rows("SELECT id FROM t"), [[Some("1".to_owned())]]);
    let error = session.run("SELECT id FROM ghost").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);

    // A different session, whose cache was never warmed by the rolled-back DDL, agrees.
    let mut other = cluster.session();
    let error = other.run("SELECT id FROM ghost").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}

/// The whole supported surface, against the cluster: indexes built over rows that are already
/// there, an `UPDATE` that maintains them, a join, and `ALTER TABLE ADD COLUMN` reading rows
/// written before the column existed.
#[test]
fn the_supported_surface_runs_against_real_stores() {
    let cluster = Cluster::start();
    let mut session = cluster.session();

    session
        .run("CREATE TABLE c (id int8 PRIMARY KEY, name text)")
        .unwrap();
    session
        .run("CREATE TABLE o (id int8 PRIMARY KEY, cid int8, amount int8)")
        .unwrap();
    session
        .run("INSERT INTO c VALUES (1,'ann'),(2,'bob')")
        .unwrap();
    session
        .run("INSERT INTO o VALUES (10,1,100),(11,1,200),(12,2,300)")
        .unwrap();

    // A join whose inner side is a point read, across the wire.
    assert_eq!(
        session.rows("SELECT o.id, c.name FROM o JOIN c ON o.cid = c.id ORDER BY o.id"),
        [
            [Some("10".to_owned()), Some("ann".to_owned())],
            [Some("11".to_owned()), Some("ann".to_owned())],
            [Some("12".to_owned()), Some("bob".to_owned())],
        ]
    );

    // An index built over rows that already exist, then used to find one.
    session
        .run("CREATE UNIQUE INDEX c_name_key ON c (name)")
        .unwrap();
    assert_eq!(
        session.rows("SELECT id FROM c WHERE name = 'bob'"),
        [[Some("2".to_owned())]]
    );

    // An UPDATE that moves an indexed value has to move its entry with it.
    session
        .run("UPDATE c SET name = 'bobby' WHERE id = 2")
        .unwrap();
    assert_eq!(session.rows("SELECT id FROM c WHERE name = 'bob'").len(), 0);
    assert_eq!(
        session.rows("SELECT id FROM c WHERE name = 'bobby'"),
        [[Some("2".to_owned())]]
    );

    // `ALTER TABLE ADD COLUMN` rewrites no row: the rows above were written two columns wide and
    // read back three wide, with the new one NULL.
    session.run("ALTER TABLE c ADD COLUMN note text").unwrap();
    assert_eq!(
        session.rows("SELECT id, note FROM c ORDER BY id"),
        [[Some("1".to_owned()), None], [Some("2".to_owned()), None],]
    );
    session.run("INSERT INTO c VALUES (3,'cid','hi')").unwrap();
    assert_eq!(
        session.rows("SELECT note FROM c WHERE id = 3"),
        [[Some("hi".to_owned())]]
    );

    // A table with no primary key, whose row ids are leased from a counter in the catalog region.
    session.run("CREATE TABLE nk (a int8)").unwrap();
    session.run("INSERT INTO nk VALUES (1),(1),(2)").unwrap();
    assert_eq!(session.rows("SELECT a FROM nk").len(), 3, "duplicates kept");
    assert_eq!(
        session.run("DELETE FROM nk WHERE a = 1").unwrap(),
        Outcome::done("DELETE 2")
    );
}

/// A prepared statement with bound parameters, which is the path every driver but `psql`'s simple
/// query protocol takes.
#[test]
fn a_bound_parameter_round_trips_through_the_cluster() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
        .unwrap();
    session.run("INSERT INTO t VALUES (1,'ann')").unwrap();

    let parsed = esker_sql::parse::parse_statements("SELECT name FROM t WHERE id = $1").unwrap();
    let described = session.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(
        described.parameters,
        [esker_sql::value::ColumnType::Int8.oid()]
    );

    let values = [Some(b"1".to_vec())];
    let outcome = session
        .executor
        .execute(
            &parsed[0],
            &Params {
                values: &values,
                formats: &[],
                declared: &[],
            },
        )
        .unwrap();
    let Outcome::Rows { rows, .. } = outcome else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"ann".to_vec())]]);
}
