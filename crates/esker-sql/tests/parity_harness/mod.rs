//! Replaying a captured corpus against this node, and holding the divergences from both sides.
//!
//! Three corpora use this and there will be more: `pg19_aggregate.txt`, `pg19_returning.txt` and
//! `pg19_sequence.txt`, each a file of statements put to a real PostgreSQL 19beta1 with what came
//! back beside them. The shape is one line per statement:
//!
//! ```text
//! statement <tab> types <tab> rows      a result set: `\gdesc`'s types, then the rows
//! statement <tab> <tab> -               a command that returned no result set, only a tag
//! statement <tab> !SQLSTATE message     a refusal; DETAIL and HINT follow the message
//! ```
//!
//! In the rows field ` ; ` separates rows, `|` separates columns, `\N` is SQL NULL, and a lone `-`
//! is no rows at all — which is a different answer from one row of NULL and is what the empty-input
//! rules are about.
//!
//! # The replay is stateful, and that is the point
//!
//! Statements run **in file order against one node**, so an `INSERT` in a corpus is a row the next
//! line sees and a `CREATE TABLE` is a table the rest of the file uses. A harness that ran each
//! line independently would be a table of assertions with extra steps, and would miss every rule
//! that is about what a *previous* statement left behind — which is most of what a sequence is.
//!
//! # Divergences are held from both sides
//!
//! [`Divergences`] carries two lists. `types` is where the **rows agree** and the declared type
//! does not; `answers` is where the answer itself differs. Both are checked in both directions: an
//! unlisted divergence fails the test, and so does a listed one that has started agreeing. Closing
//! a gap cannot be absorbed silently, and neither can opening one.

#![allow(dead_code)]

#[path = "../provenance/mod.rs"]
mod provenance;

#[path = "../trace/mod.rs"]
mod trace;

use std::fmt::Write as _;
use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Described, Execute, Outcome, Session};
use esker_sql::value::PgType;

/// How long a test waits for the other session to reach its edge before calling it wedged. Long
/// enough that a loaded container is not a failure, short enough that a genuine hang is one.
pub(crate) const EDGE: std::time::Duration = std::time::Duration::from_secs(10);

/// Two sessions on one store, for the half of a locking rule that only exists between two of them.
///
/// **Every gate a caller builds on this must be on a transaction's edge** — A's write is buffered,
/// A has committed — and never on "the thread started". Every earlier racy test in this family was
/// the second kind, and what these tests are about is precisely what happens *between* two edges.
pub(crate) struct Pair {
    store: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
}

impl Pair {
    pub(crate) fn new(fixture: &[&str]) -> Self {
        let store: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let catalog = Arc::new(Catalog::new());
        let mut setup = Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        for statement in fixture {
            setup.run(statement).unwrap();
        }
        Pair { store, catalog }
    }

    /// A handle that can open more sessions, from another thread.
    pub(crate) fn sessions(&self) -> Sessions {
        Sessions {
            store: Arc::clone(&self.store),
            catalog: Arc::clone(&self.catalog),
        }
    }

    /// Another session against the same store and catalog.
    pub(crate) fn session(&self) -> Node {
        Node::on(
            Arc::clone(&self.store),
            Arc::clone(&self.catalog),
            1,
            "esker",
            &[],
        )
    }
}

/// A [`Pair`]'s store and catalog, sendable to another thread so it can open sessions of its own.
pub(crate) struct Sessions {
    store: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
}

impl Sessions {
    pub(crate) fn session(&self) -> Node {
        Node::on(
            Arc::clone(&self.store),
            Arc::clone(&self.catalog),
            1,
            "esker",
            &[],
        )
    }
}

/// Waits for the other session to say it has reached an edge, and fails rather than hanging.
pub(crate) fn edge(from: &std::sync::mpsc::Receiver<&'static str>, what: &str) {
    match from.recv_timeout(EDGE) {
        Ok(_) => {}
        Err(error) => panic!("the other session never reached `{what}`: {error}"),
    }
}

pub(crate) fn reached(to: &std::sync::mpsc::Sender<&'static str>, what: &'static str) {
    to.send(what).unwrap();
}

/// The listed divergences that **currently swallow the rest of their corpus**, and may.
///
/// Rule 3 below says a declared divergence may not be a refusal that aborts the transaction: every
/// statement after it comes back `25P02` and is counted as swallowed rather than compared, so the
/// entry that was meant to declare one difference quietly stops the file being checked at all.
/// `pg19_tsrange.txt` was doing that to **84 of its 96 statements**, green, for ten rounds.
///
/// These 34 statements across 19 corpora were found the moment the rule was armed and are written
/// down rather than silenced: each needs a `SAVEPOINT` / `ROLLBACK TO` around the probe in its
/// capture, and each will surface whatever the rest of its file has not been comparing — three
/// divergences appeared in `pg19_tsrange.txt` alone, and three more in `pg19_money.txt`. Every one
/// of these files belongs to a feature lane rather than to the type surface, which is why they are
/// listed here instead of fixed in the commit that found them.
///
/// **This list may only shrink.** A statement not on it that swallows its file fails the test on
/// the spot; one on it that has been guarded is dead weight and should be deleted.
const SWALLOWING_DEBT: &[&str] = &[
    // `tests/alter_column_type.rs`
    "ALTER TABLE \"pg_arrays\" ALTER COLUMN \"snippets\" TYPE text[] USING string_to_array(\"snippets\", ','), ALTER COLUMN \"snippets\" SET DEFAULT '{}';",
    // `tests/alter_index.rs`
    "ALTER INDEX \"ai\" RENAME TO \"ai_renamed\"",
    // `tests/array_subquery.rs`'s entry was `ARRAY(SELECT 1)::int8[]` and is **gone**: the
    // runtime cast made that statement answer, so it stops the file no more. Three statements
    // came out from behind it — one that agrees and two that do not, both now declared with
    // their capture lines. The list may only shrink, and this is what shrinking looks like.
    // `tests/assignment_cast_date.rs`'s entry was `SET TimeZone = 'Pacific/Auckland'` and is
    // **gone**: the zone table landed (ADR 0080), so the `SET` is answered rather than refused and
    // the five statements behind it run. Two of them agreed and were deleted from that file's
    // divergences by rule 2; the ones that did not are declared there with their capture lines.
    // The list may only shrink, and this is what shrinking looks like.
    // `tests/create_schema_elements.rs`
    "CREATE SCHEMA test_schema CREATE TABLE things (id integer,name character varying(50),email character varying(50),description character varying(100),name_vector tsvector,moment timestamp without time zone default now())",
    "CREATE SCHEMA se_multi CREATE TABLE a (i int) CREATE TABLE b (j int) CREATE VIEW v AS SELECT 1 AS one",
    // `tests/do_block.rs`'s entry was this one and is **gone**: `enumtypid` is an `oid` now, so
    // `WHERE enumtypid = 'mood'::regtype` compares instead of being `42883 bigint = regtype`.
    // `tests/drop_extension.rs`
    // **This was `"ltree"` and is one statement later now**: the ltree unit closed that refusal,
    // and the file reaches `postgres_fdw` before it aborts. One fewer statement swallowed, and
    // the entry stays until a foreign-data wrapper is a thing this node has.
    "CREATE EXTENSION IF NOT EXISTS \"postgres_fdw\"",
    "CREATE EXTENSION IF NOT EXISTS \"pgcrypto\" SCHEMA extschema",
    // `tests/include_index.rs`
    "SELECT 'r', pg_get_indexdef('companies_u_include'::regclass)",
    // `tests/lateral.rs`
    "SELECT 'r', s.x FROM lt l, LATERAL (SELECT l.id AS x) s ORDER BY s.x",
    // `tests/partition.rs`
    "SELECT 'r', tableoid::regclass::text, city_id, logdate, peaktemp FROM \"measurements\" ORDER BY city_id",
    "ALTER TABLE \"measurements\" DETACH PARTITION \"measurements_concepcion\"",
    "ALTER TABLE \"measurements\" ATTACH PARTITION \"measurements_concepcion\" FOR VALUES IN (2)",
    // `tests/regex_match.rs`
    "SELECT 'r', E'a\\nb' ~ 'a.b', E'a\\nb' ~ '^a.b$'",
    "SELECT 'r', 'abc' ~ '(?i)ABC'",
    "SELECT 'r', 'aab' ~ '^(a)\\1b$'",
    // `tests/rename_column.rs`
    "SELECT 'r', count(*) FROM rc_view",
    // `tests/set_parameters.rs`
    "SET TIME ZONE 'America/New_York'",
    "SHOW lc_monetary",
    "SHOW lc_monetary",
    "SHOW lc_monetary",
    "SHOW lc_monetary",
    // `tests/set_session.rs`
    "SET LOCAL search_path TO 'ss_two'",
    "SET SESSION AUTHORIZATION esker",
    "SET LOCAL SESSION AUTHORIZATION esker",
    // `tests/trigger_function.rs`
    "SELECT 'r', proname, prokind, prorettype::regtype::text, l.lanname, pronargs, provolatile FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang WHERE proname = 'populate_column'",
    // `tests/update_from.rs`
    "UPDATE vl_comments a SET body = c.body || '!' FROM vl_comments c WHERE c.id = a.id",
    "DELETE FROM vl_comments a USING vl_posts p WHERE p.id = a.vl_post_id AND p.title = 'x'",
    // `tests/values_catalog_function.rs`
    "SELECT 'r', pg_typeof(CURRENT_TIMESTAMP), pg_typeof(now()), pg_typeof(LOCALTIMESTAMP), pg_typeof(CURRENT_DATE), pg_typeof(CURRENT_TIME)",
    "INSERT INTO vl_posts (title, created_at, updated_at) VALUES ('bad', nosuchfunction(), CURRENT_TIMESTAMP)",
    // `tests/view.rs`
    "INSERT INTO ebooks_plain (name, cover, status, format) VALUES ('Written Through', 'hard', 0, 'ebook')",
    "REFRESH MATERIALIZED VIEW ebooks_mat",
    // `tests/view_debts.rs`
    "CREATE VIEW v_union AS SELECT id, a FROM vb UNION SELECT id, c FROM vb2;",
];

/// What one statement answered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// A result set: the declared types, then the rows.
    Rows {
        types: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A command with no result set. The tag is not compared — `psql` prints a result set where it
    /// would have printed one, so the container cannot be asked what tag it sent, and a corpus
    /// that pretended otherwise would be asserting a recollection.
    Done,
    /// A refusal: `SQLSTATE message`, with ` DETAIL: …` and ` HINT: …` when the server sent them.
    Refused(String),
}

impl std::fmt::Display for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Refused(message) => write!(f, "!{message}"),
            Answer::Done => write!(f, "(a command, no result set)"),
            Answer::Rows { types, rows } => write!(
                f,
                "{}\t{}",
                types.join(","),
                if rows.is_empty() {
                    "-".to_owned()
                } else {
                    rows.iter()
                        .map(|row| row.join("|"))
                        .collect::<Vec<_>>()
                        .join(" ; ")
                }
            ),
        }
    }
}

/// The three statements a failed block still accepts, mirroring `crate::pgwire::session`.
fn allowed_in_a_failed_block(class: &StatementClass) -> bool {
    matches!(
        class,
        StatementClass::Commit | StatementClass::Rollback | StatementClass::RollbackTo(_)
    )
}

/// What a corpus is allowed to disagree about, and why.
#[derive(Default)]
pub(crate) struct Divergences {
    /// Statements whose rows agree and whose declared types do not.
    ///
    /// **The standing families, so that a corpus's entry can name one instead of arguing it
    /// again.** Every one is a type this node does not have or has at a different width, and each
    /// is measured; nothing here is about a value, which is what separates this list from
    /// [`Divergences::answers`]:
    ///
    /// | PostgreSQL | here | why |
    /// |---|---|---|
    /// | ~~`regtype`~~ | — | **closed**: `ColumnType::RegType` exists (ADR 0077) and no corpus declares it |
    /// | `oid` | `bigint` | an oid is an `int8` here — the catalog's own oid columns |
    /// | `"char"` | `text` | the one-byte type is not in `ColumnType`: `relkind`, `contype`, `typcategory`, `typdelim` |
    /// | `regproc` | `text` | `typinput`'s type; the names are identical |
    /// | `name[]` | `text` | one row left, an `array_agg` over `pg_enum`; the type exists (ADR 0084) |
    /// | ~~`integer`~~ | — | **closed** by the literal ladder (ADR 0087): `pg_typeof(1)` is `integer` |
    /// | `integer[]` | `bigint[]` | the same, one dimension out |
    /// | `character varying(3)` | `character varying` | `yes_or_no`'s length: a catalog column list carries a type and no typmod |
    ///
    /// **Four closed since the census was written**, and the table keeps their rows struck rather
    /// than deleting them, because a family that is gone is the useful half of a census. Two went
    /// by giving the catalog's own columns the type a real server declares — `name` for every
    /// identifier column, and `character varying` for `information_schema`'s `character_data`;
    /// `regtype` went with ADR 0077's type; and `integer` went with b4's literal ladder
    /// (ADR 0087), which made `pg_typeof(1)` an `integer` here.
    ///
    /// The counts behind "closed" are re-taken by walking every test's `types` list back to its
    /// corpus row, which is cheap and exact: [`replay`] deletes an entry that starts agreeing, so
    /// **a surviving entry is the evidence**. At `docs/plans/debts-v1.1.md`'s last reconciliation
    /// that left `"char"` 68, `oid` 32, `regproc` 12 and `name[]` 1.
    pub(crate) types: &'static [&'static str],
    /// Statements answered differently, each with the reason.
    pub(crate) answers: &'static [provenance::Divergence],
}

/// A node with a fixture loaded, which is what a corpus was captured over.
pub(crate) struct Node {
    /// Public to the tests that need the extended protocol rather than the simple one — a
    /// `Describe` has no place in a corpus, because `psql` never sends one.
    pub(crate) executor: Executor,
    /// Whether an explicit block is open, and whether it has failed. The session's state, mirrored
    /// (see [`Node::run`]).
    in_block: bool,
    failed: bool,
    /// **The real session's statement store**, which is not mirrored and cannot be.
    ///
    /// `PREPARE`, `EXECUTE`, `DEALLOCATE` and the half of `DISCARD ALL` that clears them all act
    /// on state the protocol owns, so a harness that only had an `Executor` answered `0A000` for
    /// statements this node runs. It is the same `Session` a connection has, driven through
    /// [`Session::run_statement`] — one implementation, two callers, rather than a third copy of
    /// the dispatch below.
    session: Session,
    /// This session's registry pid, kept so that [`Drop`] can take it back out.
    pid: u32,
}

/// **A test session leaves the registry when it ends.**
///
/// The registry is one process-wide static shared by every test in a binary, and nothing
/// in-process ever deregistered — only a real connection and the re-driver did — so
/// sessions accumulated across a binary's tests. Every test also builds its own `Catalog`,
/// so tenant ids collide between tests, and a check that counts the sessions on a tenant
/// would have seen one leaked by an earlier test and refused a `DROP DATABASE` that should
/// succeed. The leak was visible before anything read it (`tests/redrive.rs` documents the
/// accumulation); it only became *wrong* when the count grew a reader.
impl Drop for Node {
    fn drop(&mut self) {
        esker_sql::session::deregister(self.pid);
    }
}

impl Node {
    pub(crate) fn new(fixture: &[&str]) -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        Node::on(backend, Arc::new(Catalog::new()), 1, "esker", fixture)
    }

    /// A session on a store something else already has — a **second database** on one cluster,
    /// which is the only way to reach the isolation `esker-sql` gets from the tenant in its keys.
    ///
    /// `tenant` and `database` are the two halves the startup packet decides between them: the
    /// directory answers which tenant a name is, and the executor carries the name so that
    /// `current_database()` reports the one this session asked for.
    pub(crate) fn on(
        backend: Arc<dyn Backend>,
        catalog: Arc<Catalog>,
        tenant: u64,
        database: &str,
        fixture: &[&str],
    ) -> Self {
        // **In `on` rather than in `new`, which delegates here**: the same reason `cluster`
        // installs it in its one shared constructor — `RUST_LOG` should work on the test somebody
        // is already debugging, without an edit to that test first.
        trace::on();
        // Held rather than passed straight in: `deregister` takes the pid, and after the
        // executor owns the identity there is no public way back to it.
        let identity = esker_sql::session::register();
        let pid = identity.pid;
        let mut node = Node {
            executor: Executor::new(backend, catalog, tenant, identity).serving_database(database),
            in_block: false,
            failed: false,
            session: Session::new(),
            pid,
        };
        for statement in fixture {
            node.run(statement)
                .unwrap_or_else(|error| panic!("the fixture did not load: {statement}\n{error}"));
        }
        node
    }

    /// One statement's **`Describe`**, which is the extended protocol's own answer and not the
    /// simple one's.
    ///
    /// The two paths derive the row shape separately, and a client that prepares — which
    /// `ActiveRecord` does by default — only ever sees this one. A test that reads `Outcome::Rows`
    /// is reading the simple path and cannot see a `Describe` that disagrees with it, which is how
    /// an enum column went out as `int2` to every prepared read while `psql` showed it correct.
    pub(crate) fn describe(&mut self, sql: &str) -> esker_sql::Result<Described> {
        let parsed = parse_statements(sql)?;
        let [statement] = parsed.as_slice() else {
            panic!("describe takes one statement: {sql}");
        };
        self.executor.describe(statement, &[])
    }

    /// One statement, through the same dispatch a client's would take.
    ///
    /// Transaction control does not go through `execute` — the session handles it, because it is
    /// what moves the status a client sees — so this mirrors `crate::pgwire::session`'s dispatch,
    /// the way `tests/slt_harness` already does. It mirrors the **aborted-block rule** too, which
    /// is the one thing here that is a rule rather than a call: after an error every statement is
    /// `25P02` until the block ends, except the three that are allowed through.
    ///
    /// That the real session agrees is not assumed: `tests/savepoint.rs` drives `Session` itself
    /// and reads the status out of its `ReadyForQuery`.
    pub(crate) fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            let class = parsed.class().clone();
            if self.failed && !allowed_in_a_failed_block(&class) {
                return Err(esker_sql::SqlError::InFailedTransaction);
            }
            let outcome = match &class {
                StatementClass::Begin => {
                    self.in_block = true;
                    self.executor
                        .begin(parsed.begins_read_only())
                        .and_then(|()| {
                            // `BEGIN ISOLATION LEVEL …` names the level *inside* the block it starts,
                            // so it is applied after `begin` has saved the block's parameters — the
                            // same order `pgwire::session` uses (ADR 0057).
                            match parsed.begins_isolation() {
                                Some(level) => self.executor.set_isolation(level),
                                None => Ok(()),
                            }
                        })?;
                    Ok(Outcome::done("BEGIN"))
                }
                // A `COMMIT` on a **failed** block rolls it back and says so, which is
                // PostgreSQL's own answer and the reason the tag is `ROLLBACK`.
                StatementClass::Commit if self.failed => {
                    self.in_block = false;
                    self.failed = false;
                    self.executor.rollback().map(|()| Outcome::done("ROLLBACK"))
                }
                StatementClass::Commit => {
                    self.in_block = false;
                    self.executor.commit().map(|()| Outcome::done("COMMIT"))
                }
                StatementClass::Rollback => {
                    self.in_block = false;
                    self.failed = false;
                    self.executor.rollback().map(|()| Outcome::done("ROLLBACK"))
                }
                StatementClass::Savepoint(name) => {
                    if self.in_block {
                        self.executor
                            .savepoint(name)
                            .map(|()| Outcome::done("SAVEPOINT"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock("SAVEPOINT"))
                    }
                }
                StatementClass::RollbackTo(name) => {
                    if self.in_block {
                        // The one statement that recovers an aborted block.
                        self.executor
                            .rollback_to(name)
                            .inspect(|()| {
                                self.failed = false;
                            })
                            .map(|()| Outcome::done("ROLLBACK"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock(
                            "ROLLBACK TO SAVEPOINT",
                        ))
                    }
                }
                StatementClass::Release(name) => {
                    if self.in_block {
                        self.executor
                            .release(name)
                            .map(|()| Outcome::done("RELEASE"))
                    } else {
                        Err(esker_sql::SqlError::OutsideTransactionBlock(
                            "RELEASE SAVEPOINT",
                        ))
                    }
                }
                // **Not `executor.execute`**: the statements whose whole effect is on the
                // session's own store go through the session, which is where a client's go.
                _ => self.session.run_statement(&parsed, &mut self.executor),
            };
            match outcome {
                Ok(outcome) => last = outcome,
                Err(error) => {
                    if self.in_block && error.aborts_transaction() {
                        self.failed = true;
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    }

    /// The notices the last statement produced, **after** `client_min_messages` has filtered
    /// them — which is the only place a suppressed notice can be observed, because a corpus
    /// records rows and a notice is not one.
    pub(crate) fn executor_notices(&mut self) -> Vec<esker_sql::error::SqlError> {
        self.executor.take_notices()
    }

    /// The rows a query returns, or a panic naming the refusal — for the assertions a corpus
    /// cannot carry.
    pub(crate) fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self.answer(sql) {
            Answer::Rows { rows, .. } => rows,
            other => panic!("{sql}: {other}"),
        }
    }

    /// One statement, in the corpus's own shape.
    pub(crate) fn answer(&mut self, sql: &str) -> Answer {
        match self.run(sql) {
            Err(error) => {
                let mut message = format!("{} {error}", error.sqlstate());
                if let Some(detail) = error.detail() {
                    let _ = write!(message, " DETAIL: {detail}");
                }
                if let Some(hint) = error.hint() {
                    let _ = write!(message, " HINT: {hint}");
                }
                Answer::Refused(message)
            }
            Ok(Outcome::Done { .. }) => Answer::Done,
            Ok(Outcome::Rows { fields, rows, .. }) => Answer::Rows {
                types: fields
                    .iter()
                    .map(|field| type_name(field.type_oid, field.type_modifier))
                    .collect(),
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|value| {
                                value.map_or_else(
                                    || "\\N".to_owned(),
                                    |bytes| String::from_utf8(bytes).unwrap(),
                                )
                            })
                            .collect()
                    })
                    .collect(),
            },
        }
    }
}

/// What a replay saw: the statements it compared, and the statements it could not.
pub(crate) struct Replay {
    /// Statements compared against the oracle.
    pub checked: usize,
    /// Statements the aborted transaction swallowed, and therefore **nobody compared**.
    ///
    /// Reported so a corpus can say the number out loud. A refusal this node makes and PostgreSQL
    /// does not aborts the transaction, and every line after it comes back `25P02` until the
    /// `ROLLBACK` — so a gap in one feature hides whatever the rest of the file would have said
    /// about the others. That is not hypothetical: `CREATE VIEW`'s arrival un-hid two bugs in
    /// `drop_column` and `rename_column` that had been swallowed exactly this way for weeks.
    pub swallowed: usize,
}

/// Replays a corpus over a fixture and asserts every line, holding the divergences from both
/// sides. Answers how many statements ran, so a caller can assert the file loaded at all.
pub(crate) fn replay(corpus: &str, fixture: &[&str], divergences: &Divergences) -> usize {
    replay_reporting(corpus, fixture, divergences).checked
}

/// [`replay`], answering the swallowed count as well — for a corpus that has one and says so.
#[expect(
    clippy::too_many_lines,
    reason = "one pass over the corpus with the three ratchet rules in it; splitting it would \
              put a rule somewhere other than beside the comparison it is about"
)]
pub(crate) fn replay_reporting(
    corpus: &str,
    fixture: &[&str],
    divergences: &Divergences,
) -> Replay {
    // **Before anything is replayed.** A corpus whose divergences cannot be traced to a
    // measurement is not evidence, so there is no point measuring the node against it.
    provenance::check(divergences.answers);
    let mut node = Node::new(fixture);
    let mut checked = 0;
    let mut mismatched = Vec::new();
    // Statements the aborted transaction swallowed — see the `25P02` arm below.
    let mut cascaded = 0_usize;
    let mut type_mismatched = Vec::new();
    let mut agreed_after_all = Vec::new();
    // **One entry, however many times the statement appears.** A capture can run the same text
    // twice and expect two different answers — an insert under an immediate constraint and the
    // same insert under a deferred one — and the entry that covers the second is not stale merely
    // because the first now agrees. So rule 2 is decided per *entry* after the whole file: an
    // entry fails only when **every** occurrence agreed. `(entry, line, statement)`.
    let mut listed_agreements: Vec<(usize, usize, String)> = Vec::new();
    let mut listed_seen: Vec<usize> = Vec::new();
    // **Rule 4's evidence**: listed answer divergences whose *rows* now agree, so that all that
    // still differs is the declared type. See the assertion at the end of this function.
    let mut misfiled: Vec<String> = Vec::new();
    // The last **listed** divergence this node answered with a refusal, and therefore the
    // candidate for having aborted the transaction. See `swallowers` below.
    let mut last_listed_refusal: Option<(String, String)> = None;
    // Listed divergences that were followed by a cascade — each one is an entry that silently
    // stops the rest of the file being compared at all.
    let mut swallowers: Vec<String> = Vec::new();

    for (line_number, statement, expected) in parse(corpus) {
        let listed = divergences
            .answers
            .iter()
            .position(|(sql, ..)| *sql == statement);
        let actual = node.answer(&statement);
        checked += 1;

        if let Some(entry) = listed {
            listed_seen.push(entry);
            if agrees(&expected, &actual) {
                listed_agreements.push((entry, line_number, statement.clone()));
            }
            // **Rule 4: an answer divergence whose rows have started agreeing.** A listed entry
            // skips the comparison below, so what keeps it "still diverging" is the whole
            // `Answer` — types and rows together. The day the *rows* start agreeing, an entry
            // whose reason is about the answer goes stale behind the declared type, and nothing
            // says so: rule 2 only fires when the type agrees too.
            //
            // It happened. `activerecord_schema_dump` declared that `= ANY(i.indkey)` over an
            // array value and a per-row `t2.oid::regclass::text` were refused `0A000`. Both had
            // started answering — `id`, and no rows, which is what the oracle says — and the
            // entries stood because `attname` and `conname` were `text` here and `name` there.
            // Two closed features recorded as open ones, for as long as one column's type was
            // wrong.
            if let (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
            ) = (&expected, &actual)
                && rows == theirs
                && !types.is_empty()
                && types != ours
            {
                misfiled.push(format!(
                    "line {line_number}: {statement}\n  the rows agree; only the declared type \
                     differs\n  PostgreSQL: {types:?}\n  Esker:      {ours:?}"
                ));
            }
            // **A listed divergence that is a *refusal* can abort the transaction**, and then
            // every statement after it is `25P02` and counted as swallowed — so the entry that
            // was supposed to declare one difference quietly stops the file being compared at
            // all. Remembered here and reported below if a cascade follows.
            last_listed_refusal = matches!(actual, Answer::Refused(_)).then(|| {
                (
                    format!("line {line_number}: {statement}"),
                    statement.clone(),
                )
            });
            continue;
        }

        // **A statement that answered clears the suspicion.** `last_listed_refusal` names the
        // most recent listed refusal *with nothing successful since*: a `ROLLBACK TO SAVEPOINT`
        // that works proves the transaction is usable again, so the refusal before it swallowed
        // nothing. Without this the memory went stale and blamed whichever listed refusal came
        // last, wherever the cascade actually started.
        if !matches!(actual, Answer::Refused(_)) {
            last_listed_refusal = None;
        }
        match (&expected, &actual) {
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
                // An undeclared types column pins nothing: the rows are the whole claim, and a
                // difference in the declared types is neither a divergence nor a disagreement.
            ) if rows == theirs && types.is_empty() && types != ours => {}
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
            ) if rows == theirs && types != ours => {
                if !divergences.types.contains(&statement.as_str()) {
                    type_mismatched.push(format!(
                        "line {line_number}: {statement}\n  PostgreSQL: {types:?}\n  \
                         Esker:      {ours:?}"
                    ));
                }
            }
            _ if actual == expected => {
                if divergences.types.contains(&statement.as_str()) {
                    agreed_after_all.push(format!("line {line_number}: {statement}"));
                }
            }
            // **A cascade, not a disagreement.** A session capture runs inside one `BEGIN`, so the
            // first statement this node refuses that PostgreSQL answers aborts the transaction and
            // every statement after it comes back `25P02` — forty of them in one file. Listing
            // those as forty divergences would bury the one that caused them and would need forty
            // entries to silence; they are counted instead, and the root divergence above is what
            // has to be declared or fixed.
            (_, Answer::Refused(message))
                if message.starts_with("25P02") || message.starts_with("3B001") =>
            {
                // `3B001` is the second-order consequence: the `SAVEPOINT` that would have created
                // it was itself swallowed by the abort, so the `ROLLBACK TO` has nothing to return
                // to. Counted with the rest rather than reported as a divergence of its own.
                cascaded += 1;
                if let Some((culprit, sql)) = last_listed_refusal.take()
                    && !SWALLOWING_DEBT.contains(&sql.as_str())
                {
                    swallowers.push(format!("{culprit}  [cascade at line {line_number}]"));
                }
            }
            _ => mismatched.push(format!(
                "line {line_number}: {statement}\n  PostgreSQL: {expected}\n  Esker:      {actual}"
            )),
        }
    }

    // **The type divergences ride along with the value ones, because this assertion hides them.**
    // It fires before the `type_mismatched` one below, so a corpus with any disagreeing *value*
    // reports **no** type divergences at all — and the reader has no way to know there were any.
    // Thirteen of them sat behind four value rows while `debts-v1.1.md` #28 was being measured,
    // and they only came out when the four were declared by hand, one round later.
    //
    // The ordering stays: a value that differs is the bigger fact and belongs first, and the
    // `{cascaded} more were swallowed` count keeps saying what it said. What changes is that the
    // message no longer stops at the first thing wrong with the file.
    let also_typed = if type_mismatched.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nand {} statement(s) have the right rows and an unlisted type divergence, which \
             this assertion would otherwise hide until the values agree:\n\n{}",
            type_mismatched.len(),
            type_mismatched.join("\n\n")
        )
    };
    assert!(
        mismatched.is_empty(),
        "{} of {checked} statements disagree with PostgreSQL 19 and are not listed as \
         divergences ({cascaded} more were swallowed by the aborted transaction and are not \
         counted):\n\n{}{also_typed}",
        mismatched.len(),
        mismatched.join("\n\n")
    );
    // **Rule 3: a listed divergence may not swallow the file.** `pg19_tsrange.txt` declared
    // `current_setting('DateStyle')` as a difference, this node refuses it, and the refusal
    // aborted the transaction — so 84 of that corpus's 96 statements came back `25P02` and were
    // counted as swallowed rather than compared. The entry was doing its job and the corpus was
    // asserting almost nothing, for ten rounds, with a green test. The fix in the corpus is a
    // `SAVEPOINT` around the probe; the fix here is that nobody has to notice on their own.
    assert!(
        swallowers.is_empty(),
        "{} declared divergence(s) are refusals that aborted the transaction, so every statement \
         after each of them was swallowed rather than compared — guard the probe with a \
         SAVEPOINT / ROLLBACK TO in the capture:\n\n{}",
        swallowers.len(),
        swallowers.join("\n")
    );
    assert!(
        type_mismatched.is_empty(),
        "{} statements have the right rows and an unlisted type divergence:\n\n{}",
        type_mismatched.len(),
        type_mismatched.join("\n\n")
    );
    // **Rule 4: an entry in `answers` whose rows agree belongs in `types`.** Its reason describes
    // an answer that no longer differs, and leaving it there means the corpus records a closed
    // feature as an open one — invisibly, because a listed entry is never compared. Move the
    // statement to the `types` list with a reason about the *type*, or delete it if the type
    // agrees too (rule 2 will say so).
    assert!(
        misfiled.is_empty(),
        "{} listed answer divergence(s) have started agreeing on the rows — move them to \
         `types`, with a reason about the declared type, or delete them:\n\n{}",
        misfiled.len(),
        misfiled.join("\n\n")
    );
    // Rule 2, decided per entry: an entry is stale only when **every** occurrence of its statement
    // agreed. One that still covers a second occurrence stays.
    for (at, (sql, ..)) in divergences.answers.iter().enumerate() {
        let occurrences = listed_seen.iter().filter(|&&seen| seen == at).count();
        let agreements = listed_agreements
            .iter()
            .filter(|(entry, ..)| *entry == at)
            .count();
        if occurrences > 0 && agreements == occurrences {
            let line = listed_agreements
                .iter()
                .find(|(entry, ..)| *entry == at)
                .map_or(0, |(_, line, _)| *line);
            agreed_after_all.push(format!("line {line}: {sql}"));
        }
    }
    assert!(
        agreed_after_all.is_empty(),
        "{} statements are listed as divergences and now agree with PostgreSQL -- delete the \
         entries:\n\n{}",
        agreed_after_all.len(),
        agreed_after_all.join("\n")
    );
    Replay {
        checked,
        swallowed: cascaded,
    }
}

/// Whether a statement's answer **agrees**, by the same reading the comparison below uses.
///
/// **Rule 2's predicate, and it used to be `actual == expected`, which is a stricter reading than
/// the one applied to an unlisted statement.** The comparison for an unlisted statement has an arm
/// saying so in as many words — "an undeclared types column pins nothing: the rows are the whole
/// claim" — and rule 2 did not have it. The consequence is not a false report, it is silence: an
/// entry whose corpus line declares no types could **never** be reported as agreeing, because this
/// node always answers *some* type and `[]` is never equal to it. Two entries in
/// `tests/generated_parens.rs` sat in that state, closed and invisible, and were found by deleting
/// them by hand and watching the file stay green.
///
/// Same shape as rule 4 one screen below: a listed entry is not compared, so whatever decides
/// "still diverging" is the only thing that can ever say "not any more". Both readings have to be
/// the same reading, which is what this function is for — there is now one place to change it.
fn agrees(expected: &Answer, actual: &Answer) -> bool {
    match (expected, actual) {
        (
            Answer::Rows { types, rows },
            Answer::Rows {
                types: _,
                rows: theirs,
            },
        ) if types.is_empty() => rows == theirs,
        _ => expected == actual,
    }
}

/// The name `\gdesc` prints for an OID, which is the name [`PgType`] already knows.
/// A column's type as `\gdesc` writes it, **with its typmod**: `character varying(5)`, not
/// `character varying`.
///
/// The number is what the capture holds, so leaving it out would make every corpus with a
/// `varchar(n)`, a `character(n)` or a `timestamp(p)` in it agree by not looking.
fn type_name(oid: u32, typmod: i32) -> String {
    // **A domain is printed as its base, which is what the capture holds.** `psql` resolves
    // `typbasetype` for display: `\gdesc` of `information_schema.columns.is_nullable` says
    // `character varying(3)` where the `RowDescription` carries 13369 — measured on 19beta1
    // through `pg_prepared_statements.result_types`, which is the **plan's** type and not the
    // wire's — a distinction ADR 0103 got wrong once, and the corpora hold the client's rendering
    // either way, which is all this function needs — and says
    // `information_schema.yes_or_no`. The corpora were captured through the client, so this side
    // has to render the same way or two servers that agree exactly would be reported as
    // disagreeing (ADR 0103, `debts-v1.1.md` #37).
    //
    // The width comes with the base, because the column's typmod is -1 and the domain's is 7.
    // Only the five `information_schema` domains are here; a **user** domain would need the
    // tenant's catalog, which this function has no reader for, and no corpus declares one's type
    // today.
    let (oid, typmod) = esker_sql::catalog::pg_catalog::information_schema_domain_base(oid)
        .map_or((oid, typmod), |(base, width)| (base.oid(), width));
    esker_sql::value::ColumnType::ALL
        .into_iter()
        .find(|ty| ty.oid() == oid)
        .map_or_else(
            || "?".to_owned(),
            |ty| esker_sql::value::format_type(ty, typmod),
        )
}

/// One corpus file, as `(line number, statement, what PostgreSQL answered)`.
/// The directive a corpus file uses to say its **values are escaped**.
///
/// **It must be in the header comment block** — before the first line that is neither blank nor a
/// comment — and not merely somewhere in the file. A corpus that *documents* this format writes
/// the directive as an example (`pg19_corpus_format.txt` does, in its own header prose), and
/// anywhere-in-the-file would make such a file silently start escaping.
///
/// A comment line to every reader that does not know about it, so a file carrying it is still a
/// valid corpus for anything else, and old files are untouched — which matters, because 19 rows
/// across this directory already hold a `\\` or a `\n` inside a value and un-escaping them
/// unconditionally would silently change what they assert. Measured before choosing opt-in.
const ESCAPED_DIRECTIVE: &str = "#!escaped";

/// **The escape contract**, which the capture tool has to write and this reads.
///
/// `docs/plans/debts-v1.1.md` #20: a corpus row is `statement TAB types TAB rows`, rows separated
/// by `" ; "` and cells by `"|"`, and nothing said what happens when a *value* holds one of those.
/// The answer was that it silently became extra cells: `to_tsquery('fat | cat')` renders
/// `'fat' | 'cat'` and parsed as three columns where the node answered two, so a row that read
/// identically was reported as a disagreement and declared as one. Three files declared exactly
/// that, two of them with an `UNMEASURED` provenance.
///
/// In a file that declares [`ESCAPED_DIRECTIVE`], a value's `\`, `|`, `;`, newline, carriage
/// return and tab are written as `\\`, `\|`, `\;`, `\n`, `\r` and `\t`. **An unrecognised
/// escape is kept as written**, so `\N` — this format's NULL marker, and a value that happens to
/// be those two characters — comes back as itself either way.
fn unescape(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut rest = cell;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let mut chars = rest.chars();
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(c @ ('|' | ';' | '\\')) => out.push(c),
            // Kept as written: an escape this contract does not define is not this reader's to
            // interpret, and `\N` is the one that matters.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
        rest = chars.as_str();
    }
    out.push_str(rest);
    out
}

/// Split on `separator`, skipping any occurrence a backslash escapes.
///
/// Byte indices, and the step over an escape moves past one **whole character** so the slices
/// below stay on boundaries — a `\` before a multi-byte character is otherwise a panic.
fn split_unescaped<'a>(field: &'a str, separator: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let bytes = field.as_bytes();
    let (mut start, mut at) = (0, 0);
    while at < field.len() {
        if bytes[at] == b'\\' {
            at += 1;
            at += field[at..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if field[at..].starts_with(separator) {
            out.push(&field[start..at]);
            at += separator.len();
            start = at;
            continue;
        }
        at += 1;
    }
    out.push(&field[start..]);
    out
}

/// The rows of one corpus row's third field, split the way the file says it is written.
///
/// Unescaped files split on every `" ; "` and every `|`, which is what every corpus in this
/// directory did before the contract existed and what all but three of them still do. An escaped
/// file skips a separator a backslash protects and un-escapes each cell.
fn answer_rows(field: &str, escaped: bool) -> Vec<Vec<String>> {
    if !escaped {
        return field
            .split(" ; ")
            .map(|row| row.split('|').map(str::to_owned).collect())
            .collect();
    }
    split_unescaped(field, " ; ")
        .into_iter()
        .map(|row| {
            split_unescaped(row, "|")
                .into_iter()
                .map(unescape)
                .collect()
        })
        .collect()
}

/// **A row whose cells do not match its declared types, in a file that is not escaped.**
///
/// The decidable half of #20's loudness: a row declaring three types and parsing as four cells is
/// malformed, and the reason is almost always a `|` inside a value. It used to read as a
/// disagreement — the report showed two rows of text that looked identical — and cost this lane two
/// rounds before anyone printed the raw `Answer`. Now it says which line, what the counts are, and
/// what to do about it.
///
/// Only for a file without the directive, and only where the types are declared: an escaped file
/// has no ambiguity to catch, and a row whose types field is empty pins no count.
fn refuse_an_ambiguous_row(line: usize, statement: &str, types: &[String], rows: &[Vec<String>]) {
    for row in rows {
        assert!(
            row.len() == types.len(),
            "line {line}: this row declares {} types and parses as {} cells:\n  {statement}\n               {row:?}\nA value holding `|` or `\\n` is split by this format and there is no escape \
             unless the file says `{ESCAPED_DIRECTIVE}` (docs/plans/debts-v1.1.md #20). Re-capture \
             the file with escaping, or the row is asserting something other than what it reads.",
            types.len(),
            row.len()
        );
    }
}

/// The declared types of one corpus row, split on the commas that **separate** them.
///
/// **A type name can hold a comma**, and this used to split on every one: `numeric(10,2)` parsed
/// as `numeric(10` and `2)`, so a row declaring three types was compared as five and could not
/// agree whatever this node answered. `tests/numeric.rs` had six statements listed as declared-type
/// divergences for exactly that reason, under a reason about "one of the standing declared-type
/// families" — the exemption was real and its cause was the format
/// (`docs/plans/debts-v1.1.md` #20).
///
/// **Decidable without an escape**, which is why this half needs no re-capture: a comma inside a
/// type's modifier is inside parentheses, and a comma that separates two types never is. So the
/// split is at parenthesis depth zero. The other two halves of #20 — a `|` inside a *value* and a
/// newline inside one — are not decidable and need the escape [`unescape`] reads.
fn declared_types(field: &str) -> Vec<String> {
    let mut types = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    for (at, byte) in field.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                types.push(field[start..at].to_owned());
                start = at + 1;
            }
            _ => {}
        }
    }
    types.push(field[start..].to_owned());
    types
}

fn parse(corpus: &str) -> Vec<(usize, String, Answer)> {
    // In the header block only: the directive is positional, and the rule is decidable — everything
    // up to the first line that is neither blank nor a comment.
    let escaped = corpus
        .lines()
        .take_while(|line| line.trim_start().starts_with('#') || line.trim().is_empty())
        .any(|line| line.trim() == ESCAPED_DIRECTIVE);
    corpus
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let statement = fields.next().unwrap_or_default().to_owned();
            let second = fields
                .next()
                .unwrap_or_else(|| panic!("line {}: no answer", index + 1));
            let answer = match second.strip_prefix('!') {
                Some(message) => Answer::Refused(message.to_owned()),
                // **An empty types field means the types were not declared, not that there is no
                // result set.** A `CREATE TABLE` leaves it blank because `\gdesc` describes
                // nothing for one — and so does a `SELECT` in a session capture whose `\gdesc`
                // pass ran after its transaction rolled back, which is how such a capture records
                // a query over a table that no longer exists. What decides is the *rows* field:
                // `-` or nothing at all is a command, and anything else is rows whose declared
                // types this line does not pin.
                None if second.is_empty() => match fields.next() {
                    None | Some("-") => Answer::Done,
                    Some(rows) => Answer::Rows {
                        types: Vec::new(),
                        rows: answer_rows(rows, escaped),
                    },
                },
                None => {
                    let types = declared_types(second);
                    let rows = match fields.next().unwrap_or("-") {
                        "-" => Vec::new(),
                        rows => answer_rows(rows, escaped),
                    };
                    if !escaped {
                        refuse_an_ambiguous_row(index + 1, &statement, &types, &rows);
                    }
                    Answer::Rows { types, rows }
                }
            };
            (index + 1, statement, answer)
        })
        .collect()
}
