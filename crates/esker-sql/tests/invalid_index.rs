//! **`CREATE INDEX CONCURRENTLY` answers its client when the build is finished**, and a build that
//! fails leaves the invalid index behind.
//!
//! `postgresql_adapter_test#test_invalid_index`:
//!
//! ```ruby
//! @connection.exec_query("INSERT INTO ex (number) VALUES (1), (1)")
//! error = assert_raises(ActiveRecord::RecordNotUnique) do
//!   @connection.add_index(:ex, :number, unique: true, algorithm: :concurrently, name: :invalid_index)
//! end
//! assert_match(/could not create unique index/, error.message)
//! assert     @connection.index_exists?(:ex, :number, name: :invalid_index)
//! assert_not @connection.index_exists?(:ex, :number, name: :invalid_index, valid: true)
//! assert     @connection.index_exists?(:ex, :number, name: :invalid_index, valid: false)
//! ```
//!
//! Three facts in one statement, and this node had none of them: the statement returned as soon as
//! the job record existed, so nothing was raised, nothing was built, and the index sat at `absent`
//! for ever because the fake backend publishes no step interval and so runs no re-driver.
//!
//! # Measured on 19beta1 first
//!
//! ```text
//! BEGIN; CREATE UNIQUE INDEX CONCURRENTLY … ;   25001 CREATE INDEX CONCURRENTLY cannot run
//!                                                     inside a transaction block
//! CREATE UNIQUE INDEX CONCURRENTLY g1ci_invalid ON g1ci (number);
//!                                               23505 could not create unique index "g1ci_invalid"
//!                                               DETAIL:  Key (number)=(1) is duplicated.
//! SELECT indisvalid, indisready FROM pg_index … ;      f | f
//! CREATE UNIQUE INDEX [CONCURRENTLY] g1ci_invalid … ;  42P07 relation "g1ci_invalid" already exists
//! REINDEX INDEX g1ci_invalid;                          23505, the build tried again
//! DROP INDEX g1ci_invalid;                             DROP INDEX, and the name is free
//! ```
//!
//! Only the second line was wrong here, and one of the others was right by accident: `indisvalid`
//! was `f` because the index had never been built at all, not because a build had failed.
//! `indisready` is a column this node's `pg_index` does not have (`42703`) and `REINDEX` is not
//! implemented (`0A000`, register G05) — neither is reached by the Rails test, and both are named
//! here so the next reader does not have to measure them again.
//!
//! # The wait is this node's, and it is what `esker.concurrent_index_build` is for
//!
//! PostgreSQL's concurrent build is two waits for concurrent transactions; this node's is four
//! states with a step interval between the transitions (ADR 0020), and on a cluster with a
//! placement driver that interval is `lease + lock TTL`. Holding a client for three of those is
//! exactly what PostgreSQL does hold it for — the change is not finished until it is finished —
//! so `wait` is the boot value. `stage` is the answer this node used to give, kept because the
//! staged machine's own tests drive it by hand: `tests/schema_change.rs`, `tests/redrive.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `with_example_table`'s own DDL, and the two rows the test inserts.
const FIXTURE: &[&str] = &[
    "CREATE TABLE ex (id serial primary key, number integer, data character varying(255))",
    "INSERT INTO ex (number) VALUES (1), (1)",
];

/// Rails' `index_exists?`: `indexes(:ex)` filtered by name, and its `valid:` reads `indisvalid`.
const RAILS_INDEXES: &str = "SELECT i.relname, d.indisunique, d.indisvalid \
     FROM pg_class t \
     INNER JOIN pg_index d ON t.oid = d.indrelid \
     INNER JOIN pg_class i ON d.indexrelid = i.oid \
     WHERE i.relkind IN ('i', 'I') AND d.indisprimary = 'f' AND t.relname = 'ex' \
     ORDER BY i.relname";

/// The statement raises the build's own error, with PostgreSQL's sentence and `DETAIL`.
#[test]
fn a_concurrent_unique_build_over_duplicates_raises() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)")
            .to_string(),
        "!23505 could not create unique index \"invalid_index\" \
         DETAIL: Key (number)=(1) is duplicated."
    );
}

/// And the invalid index is **left in the catalog**: `indisvalid = f`, and the name is taken.
#[test]
fn the_index_a_failed_build_leaves_behind_is_invalid_and_keeps_its_name() {
    let mut node = parity::Node::new(FIXTURE);
    node.answer("CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)");

    assert_eq!(
        node.rows(RAILS_INDEXES),
        [["invalid_index".to_owned(), "t".to_owned(), "f".to_owned()]],
        "Rails reads the index, and reads it as not valid"
    );
    // The name is held by something that is not readable, which is `42P07` either way round.
    for again in [
        "CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)",
        "CREATE UNIQUE INDEX invalid_index ON ex (number)",
    ] {
        assert_eq!(
            node.answer(again).to_string(),
            "!42P07 relation \"invalid_index\" already exists",
            "{again}"
        );
    }
}

/// `DROP INDEX` clears it, entries and name together.
#[test]
fn dropping_the_invalid_index_frees_its_name() {
    let mut node = parity::Node::new(FIXTURE);
    node.answer("CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)");
    node.run("DROP INDEX invalid_index").unwrap();
    assert_eq!(node.rows(RAILS_INDEXES), Vec::<Vec<String>>::new());
    // Not unique, so this one can be built: the name is free and the table is unchanged.
    node.run("CREATE INDEX invalid_index ON ex (number)")
        .unwrap();
}

/// **A build that succeeds is readable the moment the statement answers**, with no job left over.
#[test]
fn a_concurrent_build_is_finished_when_the_statement_answers() {
    // Seven hundred rows, so the backfill is three batches and a bit (`exec::BATCH_ROWS` is 256):
    // a build that answered before its last batch would leave a readable index missing rows.
    let rows = (1..=700)
        .map(|n| format!("({n})"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut node = parity::Node::new(&[
        "CREATE TABLE ok (id serial primary key, number integer)",
        &format!("INSERT INTO ok (number) VALUES {rows}"),
    ]);
    node.run("CREATE UNIQUE INDEX CONCURRENTLY ok_number ON ok (number)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT x.indisvalid FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
             WHERE i.relname = 'ok_number'"
        ),
        [["t".to_owned()]],
        "public, so the planner may use it"
    );
    assert!(
        node.rows("SELECT * FROM esker_schema_jobs()").is_empty(),
        "and the job is forgotten"
    );
    // Three batches' worth of rows, all indexed: a lookup answers from the index.
    assert_eq!(
        node.rows("SELECT number FROM ok WHERE number = 700"),
        [["700".to_owned()]]
    );
    assert_eq!(
        node.answer("INSERT INTO ok (number) VALUES (700)")
            .to_string(),
        "!23505 duplicate key value violates unique constraint \"ok_number\" \
         DETAIL: Key (number)=(700) already exists."
    );
}

/// Inside a transaction block it is still refused, which is what makes driving it here safe.
#[test]
fn inside_a_transaction_block_it_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.answer("CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)")
            .to_string(),
        "!25001 CREATE INDEX CONCURRENTLY cannot run inside a transaction block"
    );
    node.run("ROLLBACK").unwrap();
    assert_eq!(node.rows(RAILS_INDEXES), Vec::<Vec<String>>::new());
}

/// `stage` is the other half of the setting: the statement returns as soon as the job exists.
#[test]
fn staged_is_the_node_that_answers_before_the_build() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("SET esker.concurrent_index_build = 'stage'")
        .unwrap();
    node.run("CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number)")
        .unwrap();
    let jobs = node.rows("SELECT * FROM esker_schema_jobs()");
    assert_eq!(jobs[0][1], "adding");
    assert_eq!(jobs[0][3], "absent");
    // The duplicate is still the change's failure, and it is the driver that is told.
    node.run("SELECT esker_schema_step('invalid_index')")
        .unwrap();
    node.run("SELECT esker_schema_step('invalid_index')")
        .unwrap();
    assert_eq!(
        node.answer("SELECT esker_schema_step('invalid_index')")
            .to_string(),
        "!23505 could not create unique index \"invalid_index\" \
         DETAIL: Key (number)=(1) is duplicated."
    );
}
