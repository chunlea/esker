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
use esker_sql::value::PgType;

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
    first.executor.begin(false).unwrap();
    second.executor.begin(false).unwrap();
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

    session.executor.begin(false).unwrap();
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
                bound: true,
            },
        )
        .unwrap();
    let Outcome::Rows { rows, .. } = outcome else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"ann".to_vec())]]);
}

/// A historical read against a **real** three-store cluster, through the client's `begin_at`.
///
/// This test was written against a stub that refused by name, and it is the one that changed when
/// `TxnClient::begin_at` landed — which is what it was for. What it asserts now is the whole of
/// ADR 0021 Decision 1 end to end: the snapshot is a number, the read at it sees the old row, the
/// present still sees the new one, and a write at it is refused.
///
/// A **token**, not an instant, and the reason is worth writing down: this cluster's oracle is a
/// `CountingOracle`, whose timestamps are a counter rather than `physical_ms << 18`. The physical
/// half of every timestamp here is therefore zero, every version sits inside the first millisecond
/// of 1970, and no instant a user could name would distinguish two of them. A token carries the
/// timestamp directly and needs no clock at all, which is what makes it the shape that works
/// before PD's real TSO is wired in — `src/bin/esker-sql.rs` still builds a `CountingOracle`, and
/// `esker_pd::tso` is the real one.
#[test]
fn a_historical_read_against_real_stores_sees_the_old_row() {
    let cluster = Cluster::start();
    let mut session = cluster.session();

    session
        .run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    session.run("INSERT INTO t VALUES (1, 'one')").unwrap();

    // A snapshot of the present, named. `pg_export_snapshot()` is PostgreSQL's own verb for this
    // and costs one record: no copy, no flush.
    let before = session.rows("SELECT pg_export_snapshot()")[0][0]
        .clone()
        .unwrap();

    session
        .run("UPDATE t SET note = 'two' WHERE id = 1")
        .unwrap();
    assert_eq!(
        session.rows("SELECT note FROM t"),
        [[Some("two".to_owned())]]
    );

    session.run("BEGIN").unwrap();
    session
        .run(&format!("SET TRANSACTION SNAPSHOT '{before}'"))
        .unwrap();
    assert_eq!(
        session.rows("SELECT note FROM t"),
        [[Some("one".to_owned())]],
        "the past, out of three real stores"
    );

    // A write at that snapshot is refused before anything is planned, naming the command the way
    // PostgreSQL names it.
    let error = session
        .run("UPDATE t SET note = 'three' WHERE id = 1")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::READ_ONLY_SQL_TRANSACTION);
    assert_eq!(
        error.to_string(),
        "cannot execute UPDATE in a read-only transaction"
    );
    session.run("ROLLBACK").unwrap();

    assert_eq!(
        session.rows("SELECT note FROM t"),
        [[Some("two".to_owned())]],
        "and the present is where the block left it"
    );
}

/// The time machine's two verbs against three real stores: a checkpoint, and a diff across it.
///
/// The fake proves the merge; this proves the wiring, which is the question a fake cannot answer —
/// two read-only transactions at two timestamps, each scanning a range that spans the stores that
/// hold it, through Percolator.
#[test]
fn a_diff_across_a_checkpoint_runs_against_real_stores() {
    let cluster = Cluster::start();
    let mut session = cluster.session();

    session
        .run("CREATE TABLE d (id int8 PRIMARY KEY, note text)")
        .unwrap();
    session
        .run("INSERT INTO d VALUES (1, 'same'), (2, 'old'), (3, 'gone')")
        .unwrap();
    session.run("SELECT esker_checkpoint('d0')").unwrap();

    session
        .run("UPDATE d SET note = 'new' WHERE id = 2")
        .unwrap();
    session.run("DELETE FROM d WHERE id = 3").unwrap();
    session.run("INSERT INTO d VALUES (4, 'fresh')").unwrap();

    let mut rows = session.rows("SELECT * FROM esker_diff('d', 'd0')");
    rows.sort();
    assert_eq!(
        rows,
        [
            vec![
                Some("delete".to_owned()),
                Some("(3)".to_owned()),
                Some("(3, gone)".to_owned()),
                None,
            ],
            vec![
                Some("insert".to_owned()),
                Some("(4)".to_owned()),
                None,
                Some("(4, fresh)".to_owned()),
            ],
            vec![
                Some("update".to_owned()),
                Some("(2)".to_owned()),
                Some("(2, old)".to_owned()),
                Some("(2, new)".to_owned()),
            ],
        ],
        "key 1 was never touched and is not a row"
    );

    // A checkpoint taken on one session is a name any session on the cluster can use.
    let mut other = cluster.session();
    other.run("BEGIN").unwrap();
    other.run("SET TRANSACTION SNAPSHOT 'd0'").unwrap();
    assert_eq!(
        other.rows("SELECT note FROM d WHERE id = 3"),
        [[Some("gone".to_owned())]],
        "the row this session deleted, out of three real stores"
    );
    other.run("ROLLBACK").unwrap();
}

/// A re-created table's `bigserial` starts at **1**, on the connection that dropped the old one.
///
/// `range_test.rb` is what asks: every one of its 46 tests drops `postgresql_ranges`, re-creates
/// it, loads fixtures with **explicit** ids 101-105, and one of them then `create!`s a row and
/// reads `PostgresqlRange.first` — `ORDER BY id ASC LIMIT 1`. On a real server the new row's id is
/// 1, below every fixture, so `.first` is the row the test just wrote. Run 72's capture shows this
/// node handing out **705** there, so `.first` returned fixture 101 and the test read a value it
/// had not written. It passes alone and fails in the file, which is the shape of a counter that
/// remembers something across the drop.
///
/// **Explicitly on the real backend and on one session**, because that is where the two things
/// that could remember live: a sequence's stored counter is a key in the catalog region, and the
/// reserved block ([`esker_sql::catalog::SEQUENCE_BATCH`]) is in the `Executor` that a connection
/// owns. `MemoryBackend` answers 1 for this shape, so a green in-process test would have proved
/// nothing about either.
#[test]
fn a_re_created_serial_starts_at_one_on_the_session_that_dropped_it() {
    let cluster = Cluster::start();
    let mut session = cluster.session();

    // Five rounds, because the capture's ids step by whole blocks of 32 rather than by one: a
    // counter that survives one drop survives all of them, and a single round would only catch a
    // remembered *value* and not a remembered *block*.
    for round in 1..=5 {
        // The capture's own DDL, `UNLOGGED` and the two `ALTER`s included, because the question is
        // what a re-created table's sequence remembers and every one of those touches the catalog.
        session
            .run(
                "CREATE UNLOGGED TABLE pr (id bigserial PRIMARY KEY, note text, \
                 int4_range int4range, int8_range int8range)",
            )
            .unwrap();
        session.run("ALTER TABLE pr ADD ts_range tsrange").unwrap();
        // The fixtures, with their ids written out — which is what leaves a real server's sequence
        // untouched and is why its next value is 1.
        for id in 101..=105 {
            session
                .run(&format!(
                    "INSERT INTO pr (id, note) VALUES ({id}, 'fixture')"
                ))
                .unwrap();
        }
        session
            .run("INSERT INTO pr (note) VALUES ('created')")
            .unwrap();
        assert_eq!(
            session.rows("SELECT id, note FROM pr ORDER BY id ASC LIMIT 1"),
            [[Some("1".to_owned()), Some("created".to_owned())]],
            "round {round}: the created row is the first row, as it is on a real server"
        );
        session.run("DROP TABLE pr").unwrap();
        // **The reserved block goes with the sequence.** A session takes
        // `catalog::SEQUENCE_BATCH` values at a time and serves them from memory, keyed by the
        // sequence's id; nothing was forgetting them, so a connection that created and dropped
        // tables collected one entry per drop for its whole life — which is what the Rails
        // harness's connection is. It cannot hand out a wrong value, because
        // `catalog::allocate_id` never reuses an id and a re-created sequence is a different
        // sequence, so this is a leak and the round above is what proves the values are right.
        assert_eq!(
            session.executor.held_sequence_blocks(),
            0,
            "round {round}: the dropped table's sequence block is still held"
        );
    }
}

/// The same question with a **connection pool**, which is what `ActiveRecord` has.
///
/// A session reserves [`esker_sql::catalog::SEQUENCE_BATCH`] values and serves them from memory,
/// so two connections on one sequence take two blocks and the second one's first value is 33 —
/// the declared `CACHE 32` divergence, and PostgreSQL with `CACHE 32` would do the same. What must
/// **not** survive is the block outliving the sequence: after the table is dropped and re-created,
/// every connection has to see a sequence that starts at 1 again.
#[test]
fn a_pool_of_connections_sees_a_re_created_sequence_start_over() {
    let cluster = Cluster::start();
    let mut ddl = cluster.session();
    let mut writers: Vec<_> = (0..3).map(|_| cluster.session()).collect();

    for round in 1..=3 {
        ddl.run("CREATE TABLE pp (id bigserial PRIMARY KEY, note text)")
            .unwrap();
        for id in 101..=105 {
            ddl.run(&format!(
                "INSERT INTO pp (id, note) VALUES ({id}, 'fixture')"
            ))
            .unwrap();
        }
        // Every writer takes its own block, so the ids differ; what they share is that all of them
        // are **below the fixtures**, which is what `.first` reads.
        for (at, writer) in writers.iter_mut().enumerate() {
            writer
                .run(&format!("INSERT INTO pp (note) VALUES ('w{at}')"))
                .unwrap();
        }
        let first = ddl.rows("SELECT id FROM pp ORDER BY id ASC LIMIT 1");
        assert_eq!(
            first,
            [[Some("1".to_owned())]],
            "round {round}: the lowest id is a created row's, not a fixture's"
        );
        let created = ddl.rows("SELECT count(*) FROM pp WHERE id < 101");
        assert_eq!(
            created,
            [[Some("3".to_owned())]],
            "round {round}: all three writers' rows are below the fixtures"
        );
        ddl.run("DROP TABLE pp").unwrap();
    }
}

/// Consecutive `bigserial` ids are **consecutive**, on a real cluster.
///
/// r1's run-82 probe against the live node: five inserts into a table with one `bigserial`
/// column answered `1, 33, 65, 97, 129`, with `last_value` at the *end* of each block —
/// **every statement was allocating a fresh block of 32 and using only its first value**, so 31
/// of every 32 were discarded. PostgreSQL gives `1, 2, 3, 4, 5`.
///
/// It is what makes `range_test.rb#test_infinity_values` order-dependent: the test reads
/// `PostgresqlRange.first` and the fixtures hold 101-105, so a created row sorts before them
/// while the blocks are small and after them once they have run past 105 — the flip-flop the
/// board recorded across runs 73, 74, 75 and 77.
///
/// `SEQUENCE_BATCH` is a *declared* divergence about the gaps a session leaves; handing out one
/// value per block is not that. The block exists so that a batch is reserved once and served from
/// memory, and this asserts it is served.
#[test]
fn consecutive_serial_ids_are_consecutive() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE ser (id bigserial PRIMARY KEY, v int8)")
        .unwrap();
    for value in 1..=5 {
        session
            .run(&format!("INSERT INTO ser (v) VALUES ({value})"))
            .unwrap();
    }
    assert_eq!(
        session.rows("SELECT id FROM ser ORDER BY id"),
        (1..=5)
            .map(|id| vec![Some(id.to_string())])
            .collect::<Vec<_>>(),
        "five inserts on one connection are five consecutive ids"
    );

    // And across connections: the second one takes its own block, so its ids are not the first
    // one's — but each connection's own run is still consecutive, which is what a reserved block
    // means. That the two blocks differ is `SEQUENCE_BATCH`'s declared gap and not this.
    let mut other = cluster.session();
    for value in 6..=8 {
        other
            .run(&format!("INSERT INTO ser (v) VALUES ({value})"))
            .unwrap();
    }
    let theirs: Vec<i64> = other
        .rows("SELECT id FROM ser WHERE v > 5 ORDER BY id")
        .into_iter()
        .map(|row| row[0].as_deref().unwrap_or_default().parse().unwrap_or(0))
        .collect();
    assert_eq!(theirs.len(), 3);
    assert_eq!(
        theirs[1] - theirs[0],
        1,
        "the second connection's own ids are consecutive too: {theirs:?}"
    );
    assert_eq!(theirs[2] - theirs[1], 1, "{theirs:?}");
}
