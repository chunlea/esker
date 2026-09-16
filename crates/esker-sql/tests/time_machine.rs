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
            esker_sql::session::register(),
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
                    self.executor.begin(parsed.begins_read_only())?;
                    if let Some(level) = parsed.begins_isolation() {
                        self.executor.set_isolation(level)?;
                    }
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
                esker_sql::session::register(),
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

/// A `SET` naming a parameter this node does not have is `42704`, the same as `SHOW` and `RESET`
/// of the same name — **and the price is `work_mem`, which a real server does have.**
///
/// This is the declared divergence the `SET`-parameters unit chose, and it replaced a `0A000`.
/// The old answer had the better argument for this one statement: refusing by name never claims a
/// real parameter is absent. What decided it the other way is that `SET` was the only one of the
/// three entry points saying so — `SHOW work_mem` and `RESET work_mem` have answered `42704` since
/// phase 8 — and the capture settles the commoner shape: `SET nosuchparameter` is `42704` on a
/// real server (`tests/corpus/pg19_set_parameters.txt`). Telling the two apart needs PostgreSQL's
/// whole GUC table, which this node does not carry, so the three answers now agree with each other
/// and are wrong together about `work_mem` rather than disagreeing about whether it exists.
#[test]
fn another_set_is_42704_naming_the_parameter() {
    let mut node = Node::new();
    for written in ["SET work_mem = '4MB'", "SHOW work_mem", "RESET work_mem"] {
        let error = node.fails(written);
        assert_eq!(
            error.sqlstate(),
            sqlstate::UNDEFINED_OBJECT,
            "for {written}"
        );
        assert_eq!(
            error.to_string(),
            "unrecognized configuration parameter \"work_mem\"",
            "for {written}"
        );
    }
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

// --- The checkpoint verbs --------------------------------------------------------------------

/// PostgreSQL's own verb, and it means here what it means there: export what *this* transaction
/// sees, so that a second session importing the token reads exactly that state.
#[test]
fn pg_export_snapshot_hands_out_a_token_that_reads_back() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    let exported = node.rows("SELECT pg_export_snapshot()")[0][0]
        .clone()
        .unwrap();
    node.run("COMMIT").unwrap();

    // The token names the snapshot the exporting transaction was reading, not a fresh one.
    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{exported}'"))
        .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    node.run("ROLLBACK").unwrap();
}

/// A checkpoint is a **name and a number**, and it costs one record: no snapshot, no copy, no
/// flush. What proves it is that the name outlives the session that took it.
#[test]
fn a_checkpoint_outlives_the_session_that_took_it() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    node.run("SELECT esker_checkpoint('nightly')").unwrap();
    node.run("COMMIT").unwrap();

    // A different session, which never saw the name being taken.
    let mut other = node.session();
    other.run("BEGIN").unwrap();
    other.run("SET TRANSACTION SNAPSHOT 'nightly'").unwrap();
    assert_eq!(other.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    other.run("ROLLBACK").unwrap();
}

/// The name is a **string literal**, and PostgreSQL does not case-fold those. A checkpoint called
/// `Nightly` is not the same one as `nightly`, which is the behaviour a user who quoted it expects.
#[test]
fn a_checkpoint_name_keeps_the_case_it_was_written_in() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);
    node.run("SELECT esker_checkpoint('Nightly')").unwrap();

    assert_eq!(
        node.rows("SELECT * FROM esker_checkpoints()")
            .into_iter()
            .map(|row| row[0].clone().unwrap())
            .collect::<Vec<_>>(),
        ["Nightly"]
    );
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.fails("SET TRANSACTION SNAPSHOT 'nightly'").sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
}

/// A name you cannot list is a name you cannot use, so listing carries the two things a user
/// needs: the token to paste into `SET TRANSACTION SNAPSHOT`, and the instant to compare against
/// the window.
#[test]
fn checkpoints_list_with_their_tokens_and_their_instants() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    node.run("SELECT esker_checkpoint('a')").unwrap();
    node.run("COMMIT").unwrap();
    node.run("SELECT esker_checkpoint('b')").unwrap();

    let listed = node.rows("SELECT * FROM esker_checkpoints()");
    assert_eq!(listed.len(), 2, "in name order: {listed:?}");
    assert_eq!(listed[0][0], Some("a".to_owned()));
    assert_eq!(listed[0][1], Some(token(before)));
    assert_eq!(listed[1][0], Some("b".to_owned()));
    // The instant is what a user compares against the window, so it must be the checkpoint's own.
    assert_eq!(listed[0][2], Some(esker_sql::time_machine::render(before)));
}

/// Dropping forgets the name; the versions it named are retention's business and are untouched.
/// A name that was not there answers `f` rather than raising — a client cleaning up after itself
/// should not have to look first.
#[test]
fn dropping_a_checkpoint_forgets_the_name_and_nothing_else() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);

    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    node.run("SELECT esker_checkpoint('nightly')").unwrap();
    node.run("COMMIT").unwrap();

    assert_eq!(
        node.rows("SELECT esker_drop_checkpoint('nightly')"),
        [[Some("t".to_owned())]]
    );
    assert_eq!(
        node.rows("SELECT esker_drop_checkpoint('nightly')"),
        [[Some("f".to_owned())]],
        "dropping what is not there is not an error"
    );
    assert!(node.rows("SELECT * FROM esker_checkpoints()").is_empty());

    // The data the name pointed at is still there, reachable by the token it no longer needs.
    node.run("BEGIN").unwrap();
    node.run(&format!("SET TRANSACTION SNAPSHOT '{}'", token(before)))
        .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    node.run("ROLLBACK").unwrap();

    // And the name is `42704`, which is what a real server answers for an id it does not hold.
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.fails("SET TRANSACTION SNAPSHOT 'nightly'").sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
}

/// A name shaped like an exported token would shadow the token it looks like — importing it would
/// read the timestamp out of the string and never reach the record.
#[test]
fn a_checkpoint_may_not_be_named_like_a_token() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);
    let error = node.fails("SELECT esker_checkpoint('esker-0000000000000001')");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
}

/// **A checkpoint of the past is the point of the feature**, so it must not be refused as a write.
///
/// The record is written in a present-time transaction of its own (`crate::exec::verbs`), because
/// the transaction doing the reading is read-only by construction and could never commit one. The
/// value it stores is that transaction's snapshot, which is what makes the name mean the moment
/// the user was looking at.
#[test]
fn a_checkpoint_can_be_taken_of_the_past_a_session_is_reading() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    let as_of = esker_sql::time_machine::render(before);
    node.run(&format!("SET esker.read_as_of = '{as_of}'"))
        .unwrap();

    // The session's snapshot is the *instant*, whose logical bits are zero by design, so that is
    // what both the export and the checkpoint name.
    let exported = node.rows("SELECT pg_export_snapshot()")[0][0]
        .clone()
        .unwrap();
    node.run("SELECT esker_checkpoint('before-the-incident')")
        .unwrap();
    node.run("RESET esker.read_as_of").unwrap();

    let listed = node.rows("SELECT * FROM esker_checkpoints()");
    assert_eq!(listed[0][0], Some("before-the-incident".to_owned()));
    assert_eq!(listed[0][1], Some(exported), "the name means what was read");
    assert_eq!(listed[0][2], Some(as_of));

    // And it reads back as the past it named.
    node.run("BEGIN").unwrap();
    node.run("SET TRANSACTION SNAPSHOT 'before-the-incident'")
        .unwrap();
    assert_eq!(node.rows("SELECT note FROM t"), [[Some("one".to_owned())]]);
    node.run("ROLLBACK").unwrap();
}

// --- The spellings that are not ours, and the redirect they get ------------------------------

/// The whole argument for the shape this feature took. Each of these is `42601` on a real
/// PostgreSQL 19 *and* here, so the code is parity and nothing is invented — but a bare syntax
/// error cannot say that the feature exists under another name, and the `HINT` can.
#[test]
fn an_invented_spelling_gets_postgresqls_code_and_a_redirect() {
    for sql in [
        "SELECT * FROM t AS OF SYSTEM TIME '-1h'",
        "SELECT * FROM t FOR SYSTEM_TIME AS OF '2026-08-30 14:00:00+00'",
    ] {
        let error = parse_statements(sql).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR, "{sql}");
        assert_eq!(
            error.hint().as_deref(),
            Some(
                "Esker reads the past with SET esker.read_as_of = '<timestamp>' or an interval \
                 such as '-1h'. See docs/adr/0021-time-machine.md."
            ),
            "{sql}"
        );
    }

    let error = parse_statements("SELECT * FROM t AS OF CHECKPOINT 'n'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert!(
        error.hint().unwrap().contains("SET TRANSACTION SNAPSHOT"),
        "{error:?}"
    );
}

/// `CHECKPOINT` stays PostgreSQL's, refused by name as it always was. ADR 0021 is explicit that
/// the word must not be taken — PostgreSQL owns it for forcing a WAL checkpoint — so what changes
/// is the hint, not the answer.
#[test]
fn checkpoint_stays_postgresqls_word_and_gains_a_redirect() {
    for sql in ["CHECKPOINT", "CHECKPOINT nightly"] {
        let error = parse_statements(sql).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED, "{sql}");
        assert_eq!(error.to_string(), "CHECKPOINT is not supported", "{sql}");
        assert!(
            error.hint().unwrap().contains("esker_checkpoint"),
            "{sql}: {error:?}"
        );
    }
}

/// A verb buried in an expression is not honoured half-way: it falls through to the ordinary path
/// and is `0A000` naming the expression, rather than exporting a snapshot and then failing to use
/// it.
#[test]
fn a_verb_inside_an_expression_is_refused_rather_than_half_run() {
    let mut node = Node::new();
    a_row_with_a_past(&mut node);
    let before = node.rows("SELECT * FROM esker_checkpoints()").len();
    assert_eq!(
        node.fails("SELECT esker_checkpoint('n') || 'x'").sqlstate(),
        sqlstate::FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        node.rows("SELECT * FROM esker_checkpoints()").len(),
        before,
        "nothing was written"
    );
}

// --- DIFF -------------------------------------------------------------------------------------

/// The four things a diff can say, and the fourth is that it says nothing: two scans, one merge,
/// and an unchanged key costs a comparison and produces no row.
#[test]
fn a_diff_reports_an_insert_an_update_a_delete_and_nothing_for_an_unchanged_key() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'same'), (2, 'old'), (3, 'gone')")
        .unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("UPDATE t SET note = 'new' WHERE id = 2").unwrap();
    node.run("DELETE FROM t WHERE id = 3").unwrap();
    node.run("INSERT INTO t VALUES (4, 'fresh')").unwrap();

    let rows = node.rows("SELECT * FROM esker_diff('t', 'before')");
    assert_eq!(
        rows,
        [
            vec![
                Some("update".to_owned()),
                Some("(2)".to_owned()),
                Some("(2, old)".to_owned()),
                Some("(2, new)".to_owned()),
            ],
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
        ],
        "row 1 was never touched and is not a row here"
    );
}

/// Two named snapshots, rather than one against the present. The three-argument form.
#[test]
fn a_diff_between_two_checkpoints_ignores_what_happened_after_the_second() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'one')").unwrap();
    node.run("SELECT esker_checkpoint('a')").unwrap();
    node.run("INSERT INTO t VALUES (2, 'two')").unwrap();
    node.run("SELECT esker_checkpoint('b')").unwrap();
    node.run("INSERT INTO t VALUES (3, 'three')").unwrap();

    let rows = node.rows("SELECT * FROM esker_diff('t', 'a', 'b')");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][0], Some("insert".to_owned()));
    assert_eq!(rows[0][1], Some("(2)".to_owned()));

    // And the same pair the other way round is the same change, read as its undo.
    let back = node.rows("SELECT * FROM esker_diff('t', 'b', 'a')");
    assert_eq!(back.len(), 1, "{back:?}");
    assert_eq!(back[0][0], Some("delete".to_owned()));
}

/// **It is not a changelog**, and this is the test that says so: a key written and written back
/// is invisible, and five updates look like one. Saying it in a doc comment is not enough — the
/// other reading is the one "diff" invites.
#[test]
fn a_diff_compares_two_states_and_is_not_a_changelog() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'a'), (2, 'x')").unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    // Written and written back: three versions of key 1 exist, and the diff sees none of them.
    node.run("UPDATE t SET note = 'b' WHERE id = 1").unwrap();
    node.run("UPDATE t SET note = 'a' WHERE id = 1").unwrap();
    // Five updates to key 2, which the diff reports as one.
    for note in ["p", "q", "r", "s", "z"] {
        node.run(&format!("UPDATE t SET note = '{note}' WHERE id = 2"))
            .unwrap();
    }

    let rows = node.rows("SELECT * FROM esker_diff('t', 'before')");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][1], Some("(2)".to_owned()));
    assert_eq!(rows[0][2], Some("(2, x)".to_owned()));
    assert_eq!(rows[0][3], Some("(2, z)".to_owned()), "the last, not each");
}

/// Each side is rendered with **its own snapshot's schema**, which falls out of giving each side
/// its own transaction. Using one schema for both would print a row that never existed.
#[test]
fn a_diff_across_an_add_column_renders_each_side_with_its_own_schema() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'one')").unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("ALTER TABLE t ADD COLUMN extra text").unwrap();
    node.run("UPDATE t SET extra = 'added' WHERE id = 1")
        .unwrap();

    let rows = node.rows("SELECT * FROM esker_diff('t', 'before')");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][2], Some("(1, one)".to_owned()), "two columns then");
    assert_eq!(
        rows[0][3],
        Some("(1, one, added)".to_owned()),
        "three columns now"
    );
}

/// A diff is two reads, so a snapshot outside the window is refused the way any read is, and a
/// name that is not there is `42704`.
#[test]
fn a_diff_refuses_a_snapshot_it_could_not_read_at() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    assert_eq!(
        node.fails("SELECT * FROM esker_diff('t', 'nope')")
            .sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
    assert_eq!(
        node.fails("SELECT * FROM esker_diff('t', '')").sqlstate(),
        sqlstate::INVALID_PARAMETER_VALUE
    );
}

/// A table that did not exist at the older snapshot is `42P01` from the side that cannot see it —
/// which is the honest answer. A diff against a moment before the table existed is not an empty
/// diff, it is a question about a table that was not there.
#[test]
fn a_diff_of_a_table_that_did_not_exist_yet_is_42p01() {
    let mut node = Node::new();
    node.run("CREATE TABLE anchor (id int8 PRIMARY KEY)")
        .unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();
    node.run("CREATE TABLE later (id int8 PRIMARY KEY)")
        .unwrap();

    let error = node.fails("SELECT * FROM esker_diff('later', 'before')");
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}

/// **`EXCEPT` answers now, and it still is not a diff.**
///
/// This test asserted `0A000 EXCEPT is not supported` and was named for it
/// (`except_is_still_refused_by_name`), on the reasoning that "implementing general set operations
/// to reach a two-table diff would be a larger feature refused in a smaller disguise". #105
/// implemented them for their own sake rather than to reach a diff, so the refusal it pinned is
/// gone — and the name and that paragraph went with the assertion, because a dead claim survives
/// in both.
///
/// **The half that mattered survives**: `esker_diff` is still the one ADR 0021 describes — two
/// scans over **one** table's row range, and a merge — and a set operation is not a substitute for
/// it. So this asserts only that the operator is no longer refused; what its rows are is
/// `tests/set_operator_rows.rs`'s question and not this file's.
#[test]
fn except_answers_and_is_still_not_a_diff() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    node.run("INSERT INTO t VALUES (1), (2)").unwrap();
    node.run("SELECT id FROM t EXCEPT SELECT id FROM t")
        .expect("EXCEPT is implemented since #105 and no longer refuses");
}

/// `ADD COLUMN` rewrites no row (ADR 0019), so a row nobody touched has the **same bytes** on both
/// sides and is not a change — even though it decodes to one more column. That is the right
/// answer and it is not the obvious one: the column is new, the row is not.
#[test]
fn add_column_alone_is_not_a_change_because_it_rewrites_no_row() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'one')").unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();
    node.run("ALTER TABLE t ADD COLUMN extra text").unwrap();

    assert!(
        node.rows("SELECT * FROM esker_diff('t', 'before')")
            .is_empty(),
        "no row was rewritten, so no row changed"
    );
}

// --- FLASHBACK ---------------------------------------------------------------------------------
//
// ADR 0021 Decision 3's fourth verb, and the one whose *design* is the point rather than its
// mechanism. A flashback writes the difference **forwards**, at a fresh commit_ts, and touches no
// version that already exists — so the state it undid is still there, still readable, and the undo
// is itself undoable. A flashback that mutated history would be the one operation in this system
// that destroys evidence, and it would destroy it exactly when somebody is working out what
// happened.

/// The three shapes of change, put back: a row deleted comes back, a row inserted goes away, and a
/// row updated returns to what it was. Rows nobody touched are not written at all.
#[test]
fn a_flashback_restores_deletes_removes_inserts_and_reverts_updates() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'same'), (2, 'old'), (3, 'gone')")
        .unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("UPDATE t SET note = 'new' WHERE id = 2").unwrap();
    node.run("DELETE FROM t WHERE id = 3").unwrap();
    node.run("INSERT INTO t VALUES (4, 'fresh')").unwrap();

    assert_eq!(
        node.rows("SELECT esker_flashback('t', 'before')"),
        [[Some("3".to_owned())]],
        "three rows moved, and row 1 was not one of them"
    );
    assert_eq!(
        node.rows("SELECT id, note FROM t ORDER BY id"),
        [
            [Some("1".to_owned()), Some("same".to_owned())],
            [Some("2".to_owned()), Some("old".to_owned())],
            [Some("3".to_owned()), Some("gone".to_owned())],
        ]
    );
}

/// **The undo is undoable**, which is the whole argument for compensating writes. The state before
/// the flashback is still readable `AS OF` an instant before it, and flashing back to *that* puts
/// it right back — no version was ever destroyed.
#[test]
fn a_flashback_is_itself_undoable_because_it_wrote_nothing_away() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, note text)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 'original')").unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("UPDATE t SET note = 'wrong' WHERE id = 1")
        .unwrap();
    // The mistake, named — the state a flashback is about to walk away from.
    node.run("SELECT esker_checkpoint('mistake')").unwrap();

    node.run("SELECT esker_flashback('t', 'before')").unwrap();
    assert_eq!(
        node.rows("SELECT note FROM t"),
        [[Some("original".to_owned())]]
    );

    // The state it undid is still there, which is what "writes forwards" buys.
    node.run("BEGIN").unwrap();
    node.run("SET TRANSACTION SNAPSHOT 'mistake'").unwrap();
    assert_eq!(
        node.rows("SELECT note FROM t"),
        [[Some("wrong".to_owned())]]
    );
    node.run("ROLLBACK").unwrap();

    // And so the flashback can be flashed back.
    node.run("SELECT esker_flashback('t', 'mistake')").unwrap();
    assert_eq!(
        node.rows("SELECT note FROM t"),
        [[Some("wrong".to_owned())]]
    );
}

/// Indexes are maintained by the path an `UPDATE` uses, not by a shortcut — so an index is correct
/// after a flashback, and a flashback that would violate a `UNIQUE` fails like any other write.
#[test]
fn a_flashback_maintains_every_index_through_the_ordinary_write_path() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, k int8 UNIQUE)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("UPDATE t SET k = 30 WHERE id = 1").unwrap();
    node.run("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM t WHERE k = 10"),
        Vec::<Vec<Option<String>>>::new()
    );

    node.run("SELECT esker_flashback('t', 'before')").unwrap();

    // Every lookup goes through the unique index, and every one of them is right.
    assert_eq!(
        node.rows("SELECT id FROM t WHERE k = 10"),
        [[Some("1".to_owned())]]
    );
    assert_eq!(
        node.rows("SELECT id FROM t WHERE k = 20"),
        [[Some("2".to_owned())]],
        "the deleted row's index entry came back with it"
    );
    assert!(
        node.rows("SELECT id FROM t WHERE k = 30").is_empty(),
        "and the entry it moved away from is gone"
    );
}

/// A table larger than one batch is completely put back, and the cursor is what makes that true —
/// the same requirement, and the same shape, as the index backfill's.
#[test]
fn a_flashback_larger_than_one_batch_finishes() {
    let rows = 300;
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, n int8)")
        .unwrap();
    let values: Vec<String> = (1..=rows).map(|id| format!("({id}, {id})")).collect();
    node.run(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    node.run("SELECT esker_checkpoint('before')").unwrap();

    node.run("DELETE FROM t").unwrap();
    assert!(node.rows("SELECT id FROM t").is_empty());

    assert_eq!(
        node.rows("SELECT esker_flashback('t', 'before')"),
        [[Some(rows.to_string())]]
    );
    assert_eq!(node.rows("SELECT id FROM t").len(), rows);
}

/// A snapshot that is not there is `42704`, like every other read at a name that does not exist —
/// the same namespace `SET TRANSACTION SNAPSHOT` and `esker_diff` read, so anything you can look at
/// you can go back to.
#[test]
fn a_flashback_to_a_snapshot_that_is_not_there_is_42704() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    assert_eq!(
        node.fails("SELECT esker_flashback('t', 'nope')").sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
    assert_eq!(
        node.fails("SELECT esker_flashback('nosuch', 'nope')")
            .sqlstate(),
        sqlstate::UNDEFINED_OBJECT
    );
}

/// A flashback is a write, so at a past snapshot it is `25006` like any other. Writing the present
/// from a transaction that may not write is exactly what the rule forbids.
#[test]
fn a_flashback_is_refused_while_reading_the_past() {
    let mut node = Node::new();
    let before = a_row_with_a_past(&mut node);
    node.run(&format!(
        "SET esker.read_as_of = '{}'",
        esker_sql::time_machine::render(before)
    ))
    .unwrap();
    let error = node.fails(&format!("SELECT esker_flashback('t', '{}')", token(before)));
    assert_eq!(error.sqlstate(), sqlstate::READ_ONLY_SQL_TRANSACTION);
    assert_eq!(
        error.to_string(),
        "cannot execute esker_flashback in a read-only transaction"
    );
}

/// `FLASHBACK TABLE` is Oracle's spelling and neither `sqlparser` nor PostgreSQL 19 reads it, so it
/// is `42601` on both — and carries a `HINT` naming the verb that works, like the other three
/// spellings this node redirects rather than invents.
#[test]
fn the_flashback_table_spelling_gets_a_redirect() {
    let error =
        parse_statements("FLASHBACK TABLE t TO TIMESTAMP '2026-08-30 14:00:00+00'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert!(
        error.hint().unwrap().contains("esker_flashback"),
        "{error:?}"
    );
}
