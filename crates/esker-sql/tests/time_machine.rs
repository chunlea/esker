//! Reading the past, end to end: `SET esker.read_as_of`, `SET TRANSACTION SNAPSHOT`, and the
//! refusals that bound them.
//!
//! These drive the real [`Executor`] over the in-memory store, which is a real time machine —
//! every version is filed under its `commit_ts` and a read at `T` is the newest at or before it,
//! the same sentence that is true of the store below (`docs/plans/phase-6d.md` §3). So the whole
//! feature is exercised here before `TxnClient::begin_at` exists.
//!
//! # Where PostgreSQL parity ends, and what stands in for it
//!
//! Half of this feature **is** PostgreSQL and is asserted against a real PostgreSQL 19beta1: the
//! custom GUC, `SET TRANSACTION SNAPSHOT`'s five preconditions and their precedence order, the
//! `22023`/`42704` split between a malformed snapshot id and an absent one, and `25006` for a
//! write in a read-only transaction. Every one of those was captured off the server rather than
//! recalled, and `tests/corpus/pg19_time_machine.txt` holds the capture.
//!
//! The other half has **no oracle**: PostgreSQL 19 has no time travel, so what a read at a past
//! timestamp *returns* cannot be checked against it. The reference for those semantics is
//! `CockroachDB`'s `AS OF SYSTEM TIME` — the rounding rule, the read-only rule, the retention bound
//! — cited in `docs/adr/0021-time-machine.md` and deliberately not copied as syntax.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;

const TENANT: u64 = 1;

struct Node {
    backend: Arc<MemoryBackend>,
    catalog: Arc<Catalog>,
    executor: Executor,
}

impl Node {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new());
        let catalog = Arc::new(Catalog::new());
        let executor = Executor::new(
            Arc::clone(&backend) as Arc<dyn Backend>,
            Arc::clone(&catalog),
            TENANT,
        );
        Node {
            backend,
            catalog,
            executor,
        }
    }

    /// Runs every statement in the string, stopping at the first failure, as a session would.
    ///
    /// Transaction control goes to the executor's own `begin`/`commit`/`rollback` rather than to
    /// `execute`, because that is where `crate::pgwire::session` sends it: `BEGIN` moves the
    /// status a client sees in every `ReadyForQuery`, so it belongs to the session and reaches the
    /// executor as a call and not as a statement. A harness that sent it through `execute` would
    /// get `0A000 BEGIN is not supported` and prove nothing about a block.
    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = match parsed.class() {
                StatementClass::Begin => {
                    self.executor.begin()?;
                    Outcome::done("BEGIN")
                }
                StatementClass::Commit => {
                    self.executor.commit()?;
                    Outcome::done("COMMIT")
                }
                StatementClass::Rollback => {
                    self.executor.rollback()?;
                    Outcome::done("ROLLBACK")
                }
                _ => self.executor.execute(&parsed, &Params::NONE)?,
            };
        }
        Ok(last)
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.map(|bytes| String::from_utf8(bytes).unwrap()))
                        .collect()
                })
                .collect(),
            other @ Outcome::Done { .. } => panic!("expected rows, got {other:?}"),
        }
    }

    fn fails(&mut self, sql: &str) -> esker_sql::SqlError {
        self.run(sql).expect_err("expected a refusal")
    }

    fn notices(&mut self) -> Vec<String> {
        self.executor
            .take_notices()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A second session on the same node: its own executor, the same store and catalog cache.
    fn session(&self) -> Node {
        Node {
            backend: Arc::clone(&self.backend),
            catalog: Arc::clone(&self.catalog),
            executor: Executor::new(
                Arc::clone(&self.backend) as Arc<dyn Backend>,
                Arc::clone(&self.catalog),
                TENANT,
            ),
        }
    }

    /// The store's current timestamp, which the fake's clock stands in for.
    fn now(&self) -> u64 {
        self.backend.now().unwrap()
    }
}

/// A table with one row, and a timestamp at which that row still said `one`.
///
/// The pattern every test here needs: write, remember when, overwrite. Two details are not
/// incidental, and getting either wrong makes the test pass on data no cluster produces.
///
/// **The three moments are in three different milliseconds.** A read timestamp built from an
/// instant has its logical bits zeroed, which is millisecond resolution by design
/// (`crate::time_machine`) and the correct rounding for "as of 14:00". So a version committed
/// *inside* the millisecond a user names is not visible at it — which means the writes have to be
/// in an earlier millisecond than the instant that is supposed to see them, exactly as they would
/// be on a cluster where the two happened seconds apart.
///
/// **The moment comes from the store's clock**, never from a wall clock, which is `CLAUDE.md`
/// invariant 6 kept even in a test.
fn a_row_with_a_past(node: &mut Node) -> u64 {
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'one')").unwrap();

    node.backend.advance_ms(1);
    let before = node.now();

    node.backend.advance_ms(1);
    node.run("UPDATE t SET note = 'two' WHERE id = 1").unwrap();
    before
}

/// The token an export would hand out, built here because unit 1 has no export verb yet.
fn token(ts: u64) -> String {
    format!("esker-{ts:016x}")
}

// --- The GUC ------------------------------------------------------------------------------

/// PostgreSQL accepts a namespaced custom GUC, stores it and hands the **text** back to `SHOW` —
/// not what the server made of it. Captured from 19beta1.
#[test]
fn setting_the_guc_is_silent_and_show_hands_back_the_text() {
    let mut node = Node::new();
    assert_eq!(
        node.run("SET esker.read_as_of = '-1h'").unwrap(),
        Outcome::done("SET")
    );
    assert_eq!(
        node.rows("SHOW esker.read_as_of"),
        [[Some("-1h".to_owned())]]
    );
}

/// After `RESET`, a real server answers `SHOW` with one row holding the **empty string** rather
/// than an error. The asymmetry with a never-set parameter is PostgreSQL's and was measured.
#[test]
fn reset_empties_the_guc_rather_than_removing_it() {
    let mut node = Node::new();
    node.run("SET esker.read_as_of = '-1h'").unwrap();
    node.run("RESET esker.read_as_of").unwrap();
    assert_eq!(node.rows("SHOW esker.read_as_of"), [[Some(String::new())]]);
    node.run("SET esker.read_as_of = '-1h'").unwrap();
    node.run("SET esker.read_as_of = DEFAULT").unwrap();
    assert_eq!(node.rows("SHOW esker.read_as_of"), [[Some(String::new())]]);
}

/// A parameter this node does not have is `42704`, which is what a real server answers for
/// `SET nonamespace_thing` and for a `SHOW` of anything it was never told about.
#[test]
fn another_parameter_is_42704_and_not_a_feature_gap() {
    let mut node = Node::new();
    let error = node.fails("SHOW esker.nothing_ever_set");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_OBJECT);
    assert_eq!(
        error.to_string(),
        "unrecognized configuration parameter \"esker.nothing_ever_set\""
    );
    assert_eq!(
        node.fails("RESET some.other_thing").sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
}

/// A `SET` this node does not execute keeps contract C2's answer, and names the **parameter** —
/// "SET is not supported" tells somebody who set `search_path` nothing about which line to remove.
#[test]
fn another_set_is_0a000_naming_the_parameter() {
    let mut node = Node::new();
    let error = node.fails("SET search_path = 'public'");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(error.to_string(), "SET search_path is not supported");
}

/// The value grammar's failure is PostgreSQL's own condition for a `SET` it cannot read, with
/// PostgreSQL's own sentence.
#[test]
fn an_unreadable_value_is_22023_with_postgresqls_sentence() {
    let mut node = Node::new();
    let error = node.fails("SET esker.read_as_of = 'yesterday'");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        error.to_string(),
        "invalid value for parameter \"esker.read_as_of\": \"yesterday\""
    );
}

// --- The read ------------------------------------------------------------------------------

#[test]
fn the_guc_moves_a_whole_session_into_the_past_and_reset_brings_it_back() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    assert_eq!(node.rows("SELECT note FROM t"), [[Some("two".to_owned())]]);
    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    node.run("RESET esker.read_as_of").unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("two".to_owned())]]);
}

/// A snapshot token carries its timestamp directly, so `SET TRANSACTION SNAPSHOT` needs no lookup
/// — which is the half of ADR 0021's checkpoint design that is free.
#[test]
fn a_snapshot_token_reads_the_past_inside_a_block() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    node.run("COMMIT").unwrap();
    // An imported snapshot is imported into *that* transaction and ends with it.
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("two".to_owned())]]);
}

/// One session in the past must not move another. The GUC is per connection, like every GUC.
#[test]
fn the_past_is_one_sessions_and_not_the_nodes() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    let mut other = node.session();

    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    assert_eq!(other.rows("SELECT note FROM t"), [[Some("two".to_owned())]]);
}

// --- The three refusals ---------------------------------------------------------------------

/// Every write, by its own name. PostgreSQL names the command so that one statement out of a block
/// can be identified, and this is the rule ADR 0021 calls correctness rather than policy: a commit
/// at `commit_ts > start_ts` against an old snapshot is a lost update Percolator cannot catch.
#[test]
fn every_write_at_a_past_snapshot_is_25006_naming_the_command() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();

    for (sql, command) in [
        ("INSERT INTO t VALUES (2, 'x')", "INSERT"),
        ("UPDATE t SET note = 'x' WHERE id = 1", "UPDATE"),
        ("DELETE FROM t WHERE id = 1", "DELETE"),
        ("CREATE TABLE u (a int8)", "CREATE TABLE"),
        ("DROP TABLE t", "DROP TABLE"),
        ("CREATE INDEX i ON t (note)", "CREATE INDEX"),
        ("ALTER TABLE t ADD COLUMN c text", "ALTER TABLE"),
    ] {
        let error = node.fails(sql);
        assert_eq!(
            error.sqlstate(),
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            "{sql}"
        );
        assert_eq!(
            error.to_string(),
            format!("cannot execute {command} in a read-only transaction"),
            "{sql}"
        );
    }

    // The store is untouched by every one of those, which is the part a SQLSTATE cannot assert.
    node.run("RESET esker.read_as_of").unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("two".to_owned())]]);
}

/// `EXPLAIN` runs nothing, so it is allowed at a past snapshot even for a statement that writes —
/// which is how a user reading the past can still ask what a write *would* have done.
#[test]
fn explain_of_a_write_is_allowed_in_the_past_because_it_runs_nothing() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();
    assert!(
        !node
            .rows("EXPLAIN INSERT INTO t VALUES (2, 'x')")
            .is_empty()
    );
}

/// A timestamp that has not happened names an instant a read would see a prefix of and call
/// complete. Refused, and the message names the window rather than repeating the input.
#[test]
fn a_timestamp_in_the_future_is_refused_naming_the_window() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);
    let error = node.fails("SET esker.read_as_of = '2999-01-01 00:00:00+00'");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert!(
        error.to_string().starts_with(
            "2999-01-01 00:00:00+00 is outside the valid range for parameter \
             \"esker.read_as_of\" ("
        ),
        "{error}"
    );
}

/// Below the window a read cannot be answered correctly, and answering it approximately is worse
/// than refusing (`docs/txn-spec.md` §7). The useful reply says how far back they *can* ask.
#[test]
fn a_timestamp_below_the_window_is_refused_naming_the_window() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);
    let error = node.fails("SET esker.read_as_of = '2000-01-02 00:00:00+00'");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    let message = error.to_string();
    assert!(message.contains("is outside the valid range"), "{message}");
    assert!(
        message.contains(" .. "),
        "the refusal must name both ends of the window: {message}"
    );
}

// --- `SET TRANSACTION SNAPSHOT`'s preconditions, in PostgreSQL's own order -------------------

/// Outside a block a real server sends **both**: a `25P01` warning that the statement is out of
/// place, and then `0A000` failing it for the second reason. A client shown only one of the two
/// would be told half of what PostgreSQL says.
#[test]
fn a_snapshot_outside_a_block_warns_and_then_fails_the_way_postgresql_does() {
    let mut node = Node::new();
    let error = node.fails("SET TRANSACTION SNAPSHOT 'esker-0000000000000001'");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "a snapshot-importing transaction must have isolation level SERIALIZABLE or REPEATABLE READ"
    );
    assert_eq!(
        node.notices(),
        ["SET TRANSACTION can only be used in transaction blocks"]
    );
}

/// The rule that matters most, and the one an invented syntax would have had to discover the hard
/// way: a `start_ts` cannot change under a transaction that has already read at it.
#[test]
fn a_snapshot_after_a_query_in_the_block_is_25001() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    node.run("BEGIN").unwrap();
    node.run("SELECT note FROM t").unwrap();
    let error = node.fails(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)));
    assert_eq!(error.sqlstate(), sqlstate::ACTIVE_SQL_TRANSACTION);
    assert_eq!(
        error.to_string(),
        "SET TRANSACTION SNAPSHOT must be called before any query"
    );
}

/// The two refusals a bad identifier gets, and they are **different**: what cannot be an
/// identifier at all is `22023`, and what is well formed and not there is `42704`. Captured,
/// because nobody would invent two codes for one apparent condition.
#[test]
fn a_bad_snapshot_identifier_is_22023_and_a_missing_one_is_42704() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    let error = node.fails("SET TRANSACTION SNAPSHOT ''");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(error.to_string(), "invalid snapshot identifier: \"\"");
    node.run("ROLLBACK").unwrap();

    node.run("BEGIN").unwrap();
    // PostgreSQL's own id shape: well formed, and belonging to another server.
    let error = node.fails("SET TRANSACTION SNAPSHOT '00000003-0000001B-1'");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_OBJECT);
    assert_eq!(
        error.to_string(),
        "snapshot \"00000003-0000001B-1\" does not exist"
    );
}

// --- `SET LOCAL` -----------------------------------------------------------------------------

/// `SET LOCAL` is scoped to the transaction, and PostgreSQL undoes it whichever way the block
/// ends. The `ROLLBACK` case is the one that is easy to miss and the one a failed block takes.
#[test]
fn set_local_is_undone_by_commit_and_by_rollback() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    let as_of = esker_sql::time_machine::render(before);

    for ending in ["COMMIT", "ROLLBACK"] {
        node.run("BEGIN").unwrap();
        node.run(&format!("SET LOCAL esker.read_as_of = '{as_of}'"))
            .unwrap();
        assert_eq!(
            node.rows("SELECT note FROM t"),
            [[Some("one".to_owned())]],
            "{ending}"
        );
        node.run(ending).unwrap();
        assert_eq!(
            node.rows("SELECT note FROM t"),
            [[Some("two".to_owned())]],
            "{ending}"
        );
        assert_eq!(node.rows("SHOW esker.read_as_of"), [[Some(String::new())]]);
    }
}

/// A plain `SET` inside a block is *not* local and survives the block, which is the difference
/// `LOCAL` exists to express.
#[test]
fn a_plain_set_inside_a_block_outlives_it() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();
    node.run("COMMIT").unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
}

// --- The retention DDL -----------------------------------------------------------------------

/// The window is retention (ADR 0021 Decision 2), so the DDL that sets it lands with the read
/// rather than ahead of it. The records were already built and golden-tested; this is the surface.
#[test]
fn alter_table_set_retention_writes_the_record_and_default_clears_it() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let table_id = node
        .catalog
        .view(&*node.backend.begin().unwrap(), TENANT)
        .unwrap()
        .table("t")
        .unwrap()
        .unwrap()
        .id;

    let retention = |node: &Node| {
        let txn = node.backend.begin().unwrap();
        esker_sql::catalog::table_retention(&*txn, TENANT, table_id).unwrap()
    };

    assert_eq!(retention(&node), None, "no override until one is set");

    node.run("ALTER TABLE t SET (retention = '7d')").unwrap();
    assert_eq!(retention(&node), Some(7 * 24 * 60 * 60 * 1000));

    node.run("ALTER TABLE t SET (retention = '90m')").unwrap();
    assert_eq!(retention(&node), Some(90 * 60 * 1000));

    // A bare number is milliseconds, the unit the record stores.
    node.run("ALTER TABLE t SET (retention = 604800000)")
        .unwrap();
    assert_eq!(retention(&node), Some(604_800_000));

    node.run("ALTER TABLE t SET (retention = 'forever')")
        .unwrap();
    assert_eq!(
        retention(&node),
        Some(esker_sql::catalog::RETENTION_FOREVER)
    );

    // `DEFAULT` deletes the override rather than storing a zero: an absent key and a key holding
    // zero are different things to the collector.
    node.run("ALTER TABLE t SET (retention = DEFAULT)").unwrap();
    assert_eq!(retention(&node), None);
}

/// Setting a retention deliberately does **not** bump the catalog version (ADR 0021 Decision 4).
/// Bumping would make every node in the cluster discard its table cache to learn a number none of
/// them uses.
#[test]
fn setting_a_retention_does_not_bump_the_schema_version() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let version = |node: &Node| {
        node.catalog
            .view(&*node.backend.begin().unwrap(), TENANT)
            .unwrap()
            .table("t")
            .unwrap()
            .unwrap()
            .schema_version
    };
    let before = version(&node);
    node.run("ALTER TABLE t SET (retention = '7d')").unwrap();
    assert_eq!(version(&node), before);
}

/// Every other storage parameter is contract C2's `0A000` **naming the parameter**, so a user who
/// wrote `fillfactor` is told about `fillfactor`.
#[test]
fn another_storage_parameter_is_0a000_naming_it() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = node.fails("ALTER TABLE t SET (fillfactor = 70)");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "the storage parameter fillfactor is not supported"
    );
}

/// A retention value the grammar cannot read is `22023`, named for the parameter rather than for
/// the GUC that shares its grammar.
#[test]
fn an_unreadable_retention_is_22023() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = node.fails("ALTER TABLE t SET (retention = 'a week')");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        error.to_string(),
        "invalid value for parameter \"retention\": \"a week\""
    );
}
