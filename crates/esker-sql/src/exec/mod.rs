//! The thing that runs statements, and the transaction each one runs in.
//!
//! [`Executor`] implements [`crate::pgwire::session::Execute`], which is the seam between the
//! protocol and everything below it. One executor per connection; the store and the catalog cache
//! behind it are shared by all of them.
//!
//! # Every statement is in a transaction, whether the client said so or not
//!
//! A statement inside a `BEGIN` block runs in the transaction that block opened. A statement
//! outside one gets its own, committed if it succeeded and rolled back if it did not — PostgreSQL's
//! autocommit, and the reason a single `INSERT` is atomic without anybody asking.
//!
//! # A lost race on a unique index is a duplicate, not a race
//!
//! `docs/plans/phase-6a.md` §5 rules that uniqueness composes out of a read and an ordinary write,
//! and it leaves the executor two obligations. The first is easy: read the index key in the
//! transaction and raise `23505` if it is there. The second is this module's, and it is the harder
//! half — when `commit` comes back `40001`, the transaction lost a write-write race, and if the
//! key it lost was a unique index entry then what the *user* did was insert a duplicate.
//!
//! Reporting that needs to know *which* constraint, and the transaction that could have told us is
//! gone. So the executor records every unique index key it wrote, and on a `40001` it opens a
//! fresh transaction and looks: the keys that are now present are the ones it collided with, and
//! the first of those names the constraint. A key that is absent means nobody took it and the
//! conflict really was an ordinary row-level race, which stays `40001` and stays retryable.

mod bind;
mod cursor;
mod ddl;
mod dml;
pub(crate) mod query;
mod verbs;

use std::sync::Arc;

use crate::backend::{Backend, Txn};
use crate::catalog::Catalog;
use crate::error::{Result, SqlError};
use crate::parse::Parsed;
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::{Described, Execute, Outcome, Params};
use crate::plan::Statement;
use crate::time_machine;
use crate::value::ColumnType;

/// Runs statements for one connection.
#[derive(Debug)]
pub struct Executor {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    pub(crate) tenant: u64,
    /// The transaction an explicit `BEGIN` opened. `None` means the next statement gets its own.
    open: Option<Box<dyn Txn>>,
    /// What the open transaction has written that changes how a failed commit reads. It
    /// accumulates across the whole block, because the commit that fails is the block's and not
    /// any one statement's — the first version of this recorded per statement and lost the
    /// translation entirely for a client that used `BEGIN`.
    written: Written,
    /// Notices produced by the statement that just ran, waiting for the session to send them.
    notices: Vec<SqlError>,
    /// Row ids reserved for this session but not yet handed out: `table_id -> (next, end)`.
    /// See [`Executor::next_row_id`].
    row_ids: std::collections::BTreeMap<u64, (u64, u64)>,
    /// Whether the open transaction has run DDL. From then on its catalog lookups read through
    /// rather than from the shared cache: they answer with its own uncommitted definitions, which
    /// must not reach the other sessions on this node (`crate::catalog`).
    catalog_written: bool,
    /// The snapshot this session reads at, when it is not reading the present.
    read_as_of: Option<ReadAsOf>,
    /// Whether the open transaction has run a statement.
    ///
    /// `SET TRANSACTION SNAPSHOT` may only be called before any query in the block, which is
    /// PostgreSQL's rule and exactly the right one: a `start_ts` cannot change under a transaction
    /// that has already read at it.
    open_used: bool,
    /// `BEGIN READ ONLY`: every write in this block is `25006`, as PostgreSQL does it.
    ///
    /// Separate from the transaction's own read-only-ness, which comes from reading the past: a
    /// block may be read-only because the user asked, because the snapshot is historical, or both.
    block_read_only: bool,
}

/// What a session was told to read at, and what it was told in.
#[derive(Debug, Clone)]
struct ReadAsOf {
    /// The resolved snapshot.
    start_ts: u64,
    /// What the user wrote, for `SHOW` to hand back. PostgreSQL echoes the text of a `SET`, not
    /// what the server made of it, and a user who wrote `-1h` is better served by `-1h` than by
    /// the instant it became.
    text: String,
    /// Retention as it was when this was set, so the window can be recomputed against a `now`
    /// that has moved on without a round trip per statement (`crate::time_machine::Window`).
    retention_ms: u64,
    /// `SET LOCAL`: undone when the transaction ends, whichever way it ends.
    local: bool,
}

impl Executor {
    /// An executor over a store and a shared catalog cache.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, catalog: Arc<Catalog>, tenant: u64) -> Self {
        Executor {
            backend,
            catalog,
            tenant,
            open: None,
            written: Written::default(),
            notices: Vec::new(),
            row_ids: std::collections::BTreeMap::new(),
            catalog_written: false,
            read_as_of: None,
            open_used: false,
            block_read_only: false,
        }
    }

    /// Opens a transaction at whatever snapshot this session reads at.
    ///
    /// The one place `begin` and `begin_at` are chosen between, so that no statement can be
    /// written in a way that reads the present by accident. The window is checked **here** rather
    /// than only where it was set: retention moves the floor forward under a session that set
    /// `esker.read_as_of` an hour ago, and a snapshot that has fallen out of the window must stop
    /// answering rather than answer approximately.
    fn open_txn(&self) -> Result<Box<dyn Txn>> {
        let Some(as_of) = &self.read_as_of else {
            return self.backend.begin();
        };
        let now = self.backend.now()?;
        time_machine::Window::new(now, as_of.retention_ms).admits(as_of.start_ts)?;
        self.backend.begin_at(as_of.start_ts)
    }

    /// Runs `statement` in the open transaction, or in one of its own that is committed on success
    /// and rolled back on failure.
    fn in_a_transaction(&mut self, statement: Statement, params: &Params<'_>) -> Result<Outcome> {
        if let Some(mut txn) = self.open.take() {
            let mut written = std::mem::take(&mut self.written);
            let outcome = self
                .bound(&*txn, statement, params)
                .and_then(|statement| self.run_recording(&mut *txn, &statement, &mut written));
            self.open = Some(txn);
            self.written = written;
            return outcome;
        }

        let mut txn = self.open_txn()?;
        self.catalog_written = false;
        let mut written = Written::default();
        let bound = match self.bound(&*txn, statement, params) {
            Ok(bound) => bound,
            Err(error) => {
                let _ = txn.rollback();
                return Err(error);
            }
        };
        let outcome = match self.run_recording(&mut *txn, &bound, &mut written) {
            Ok(outcome) => match txn.commit() {
                Ok(_) => Ok(outcome),
                Err(error) => Err(self.explain_conflict(error, &written)),
            },
            Err(error) => {
                // The rollback's own failure is not what the client asked about; the statement's
                // error is. Reporting the second would hide the first.
                let _ = txn.rollback();
                Err(error)
            }
        };
        self.catalog_written = false;
        outcome
    }

    fn run_recording(
        &mut self,
        txn: &mut dyn Txn,
        statement: &Statement,
        written: &mut Written,
    ) -> Result<Outcome> {
        // A write at a past snapshot is refused *here*, before anything is planned. It cannot be
        // caught later: `Txn::put` is buffered and returns nothing, so a write in a read-only
        // transaction would be dropped in silence and the statement would report success.
        if (txn.is_read_only() || self.block_read_only)
            && let Some(command) = statement.write_command()
        {
            return Err(SqlError::ReadOnlyTransaction(command));
        }
        // Before the statement rather than after it: a DDL statement that fails part-way has
        // still written, and the reads it makes on the way are its own uncommitted catalog.
        self.catalog_written |= statement.writes_catalog();
        match statement {
            Statement::CreateTable(create) => ddl::create_table(self, txn, create),
            Statement::DropTable(drop) => ddl::drop_table(self, txn, drop),
            Statement::CreateIndex(create) => ddl::create_index(self, txn, create),
            Statement::DropIndex(drop) => ddl::drop_index(self, txn, drop),
            Statement::AlterTable(alter) => ddl::alter_table(self, txn, alter),
            Statement::Insert(insert) => dml::insert(self, txn, insert, written),
            Statement::Select(select) => self.select(txn, select),
            Statement::Update(update) => dml::update(self, txn, update, written),
            Statement::Delete(delete) => dml::delete(self, txn, delete),
            Statement::Explain(inner) => self.explain(txn, inner),
            Statement::TimeMachine(verb) => verbs::run(self, txn, verb),
            // Handled before a transaction is opened; `execute` never routes one here.
            Statement::Session(_) => Err(SqlError::Internal(
                "a session statement reached the transaction path".into(),
            )),
        }
    }

    /// `SET`, `SHOW` and `RESET`, which run outside any transaction the client opened.
    ///
    /// Outside, because one of them *replaces* that transaction: `SET TRANSACTION SNAPSHOT` moves
    /// the block's `start_ts`, and a block that had already read at the old one cannot have it
    /// changed underneath — which is exactly the rule PostgreSQL enforces with `25001`.
    fn session_statement(&mut self, statement: &crate::plan::SessionStatement) -> Result<Outcome> {
        use crate::plan::SessionStatement;

        match statement {
            SessionStatement::SetReadAsOf { value, local } => {
                self.set_read_as_of(value.as_deref(), *local)?;
                Ok(Outcome::done("SET"))
            }
            SessionStatement::ShowReadAsOf => Ok(Outcome::Rows {
                fields: vec![FieldDescription::computed(
                    time_machine::READ_AS_OF,
                    ColumnType::Text,
                )],
                // Unset reads back as the empty string rather than as an error, which is what a
                // real server does for a custom GUC that has been set and then reset. It diverges
                // from PostgreSQL only for a parameter that was *never* set, where a real server
                // answers `42704` because it has never heard of it and this node always has.
                rows: vec![vec![Some(
                    self.read_as_of
                        .as_ref()
                        .map(|as_of| as_of.text.clone())
                        .unwrap_or_default()
                        .into_bytes(),
                )]],
                tag: "SHOW".to_owned(),
            }),
            SessionStatement::SetSnapshot(id) => {
                self.set_snapshot(id)?;
                Ok(Outcome::done("SET"))
            }
        }
    }

    /// `SET esker.read_as_of = '...'`, resolved once and checked against the window.
    fn set_read_as_of(&mut self, value: Option<&str>, local: bool) -> Result<()> {
        let Some(text) = value else {
            return self.move_to(None);
        };
        let now = self.backend.now()?;
        let start_ts = time_machine::resolve(text, now)?;
        // One transaction to read the retention the window is computed from. It is a rare
        // statement and a single point read; every *later* statement recomputes the window from
        // the number cached here and a fresh `now`, so the check costs nothing per query.
        let retention_ms = self.cluster_retention()?;
        time_machine::Window::new(now, retention_ms).admits(start_ts)?;
        self.move_to(Some(ReadAsOf {
            start_ts,
            text: text.to_owned(),
            retention_ms,
            local,
        }))
    }

    /// `SET TRANSACTION SNAPSHOT '<id>'`, with PostgreSQL's preconditions in PostgreSQL's order.
    ///
    /// Every one of them was captured off a real server (`docs/plans/phase-6d.md` §2), and every
    /// one is a rule this feature wants anyway. The isolation-level check is the exception that
    /// proves it: Percolator gives snapshot isolation, which is PostgreSQL's `REPEATABLE READ`, so
    /// inside a block the precondition holds by construction and is never raised.
    fn set_snapshot(&mut self, id: &str) -> Result<()> {
        if self.open.is_none() {
            // Both, in this order. PostgreSQL warns that the statement is out of place and *then*
            // fails it for the second reason, and a client that only saw one of the two would be
            // told half of what a real server says.
            self.notice(SqlError::SetTransactionOutsideBlock);
            return Err(SqlError::SnapshotIsolationRequired);
        }
        if self.open_used {
            return Err(SqlError::SnapshotAfterQuery);
        }

        let start_ts = match time_machine::parse_snapshot_id(id)? {
            // A token carries its timestamp, so this path does no lookup at all — which is the
            // half of ADR 0021's checkpoint design that costs nothing.
            time_machine::SnapshotId::Timestamp(start_ts) => start_ts,
            // A name is looked up **at the present**, however far back it points: the record says
            // what the name means now, and a checkpoint that has been dropped is `42704` exactly
            // as an id a real server does not hold is.
            time_machine::SnapshotId::Checkpoint(name) => self.checkpoint_at(&name)?,
        };
        let now = self.backend.now()?;
        let retention_ms = self.cluster_retention()?;
        time_machine::Window::new(now, retention_ms).admits(start_ts)?;
        self.move_to(Some(ReadAsOf {
            start_ts,
            text: id.to_owned(),
            retention_ms,
            // A snapshot is imported into *this* transaction, so it ends with it.
            local: true,
        }))
    }

    /// Moves the session to a snapshot, reopening the block's transaction there.
    ///
    /// **The setting is applied only if the move succeeds**, which is why the new value is passed
    /// in rather than assigned by the caller. A `SET` that failed must leave the session where it
    /// was: applying it anyway would poison every later statement with a snapshot the node had
    /// already refused, and the user would have been told the `SET` did not work.
    ///
    /// Reopening is only reachable before the block has read anything — every caller checks — so
    /// the transaction being discarded has done nothing and rolling it back loses no work. That is
    /// what makes the eager `BEGIN` in [`Execute::begin`] compatible with changing the snapshot
    /// afterwards, and it is why PostgreSQL's "before any query" rule is what makes this safe
    /// rather than merely tidy.
    fn move_to(&mut self, as_of: Option<ReadAsOf>) -> Result<()> {
        let previous = std::mem::replace(&mut self.read_as_of, as_of);
        let Some(txn) = self.open.take() else {
            return Ok(());
        };
        let _ = txn.rollback();
        match self.open_txn() {
            Ok(reopened) => {
                self.open = Some(reopened);
                Ok(())
            }
            Err(error) => {
                // The block is over either way — its transaction is gone and the session will see
                // the error and mark it failed — but the *setting* must not survive a refusal.
                self.read_as_of = previous;
                Err(error)
            }
        }
    }

    /// Runs `write` in a present-time transaction of its own, committed on success.
    ///
    /// Always at the **present**, never at the session's snapshot, and that is the point: the two
    /// callers are the checkpoint verbs, and the moment a user most wants to name is one they are
    /// already reading — a transaction that is read-only by construction and could never commit a
    /// record. The value written is that transaction's `start_ts`; the write is this one.
    ///
    /// The same shape as [`Executor::next_row_id`], which needs its own transaction for a
    /// different structural reason, and the same trade: what it writes is outside the surrounding
    /// block's atomicity, so a `ROLLBACK` does not take it back.
    fn in_its_own_transaction(&self, write: impl FnOnce(&mut dyn Txn) -> Result<()>) -> Result<()> {
        let mut txn = self.backend.begin()?;
        if let Err(error) = write(&mut *txn) {
            let _ = txn.rollback();
            return Err(error);
        }
        txn.commit()?;
        Ok(())
    }

    /// A snapshot id — a token or a checkpoint name — as the timestamp it means.
    ///
    /// The **same namespace** `SET TRANSACTION SNAPSHOT` reads, so anything a session can read at
    /// is something a diff can compare against. Sharing it is the point: two grammars for one idea
    /// would mean a checkpoint you could read at and not diff.
    pub(crate) fn snapshot_named(&self, id: &str) -> Result<u64> {
        match time_machine::parse_snapshot_id(id)? {
            time_machine::SnapshotId::Timestamp(start_ts) => Ok(start_ts),
            time_machine::SnapshotId::Checkpoint(name) => self.checkpoint_at(&name),
        }
    }

    /// A read-only transaction at a timestamp, with the window checked as any other read is.
    ///
    /// Not routed through [`Executor::open_txn`], which answers with the *session's* snapshot: a
    /// diff names its own two, and neither of them is the one the session is sitting at.
    pub(crate) fn read_at(&self, start_ts: u64) -> Result<Box<dyn Txn>> {
        let now = self.backend.now()?;
        time_machine::Window::new(now, self.cluster_retention()?).admits(start_ts)?;
        self.backend.begin_at(start_ts)
    }

    /// An ordinary transaction at the present, for the side of a diff that is "now".
    pub(crate) fn plain_read(&self) -> Result<Box<dyn Txn>> {
        self.backend.begin()
    }

    /// The timestamp a checkpoint names, or `42704`.
    ///
    /// Read in a transaction of its own **at the present**, and not at the snapshot the session is
    /// moving to: a name means what the catalog says it means now. Looking it up in the past would
    /// make a checkpoint invisible to the very read it was taken for.
    fn checkpoint_at(&self, name: &str) -> Result<u64> {
        let txn = self.backend.begin()?;
        let found = crate::catalog::checkpoint_at(&*txn, self.tenant, name);
        let _ = txn.rollback();
        found?.ok_or_else(|| SqlError::SnapshotDoesNotExist(name.to_owned()))
    }

    /// The cluster's default retention, which is the travel window
    /// (`docs/adr/0021-time-machine.md` Decision 2).
    ///
    /// Read in a transaction of its own, at the present: the record says how far back the
    /// collector has *not yet swept*, which is a fact about now and not about the snapshot being
    /// asked for.
    fn cluster_retention(&self) -> Result<u64> {
        let txn = self.backend.begin()?;
        let retention = crate::catalog::default_retention(&*txn);
        let _ = txn.rollback();
        retention
    }

    /// What a `COMMIT` or a `ROLLBACK` undoes besides the writes.
    ///
    /// `SET LOCAL` is scoped to the transaction and PostgreSQL undoes it whichever way the block
    /// ends — including a `ROLLBACK`, which is the case that is easy to miss and the one a failed
    /// block takes. An imported snapshot is local by the same rule: it was imported into *this*
    /// transaction.
    fn end_of_block(&mut self) {
        self.open_used = false;
        self.block_read_only = false;
        if self.read_as_of.as_ref().is_some_and(|as_of| as_of.local) {
            self.read_as_of = None;
        }
    }

    /// `SELECT`: plan it, then pull every row through.
    fn select(&mut self, txn: &mut dyn Txn, select: &crate::plan::Select) -> Result<Outcome> {
        let planned = self.plan_select(txn, select)?;
        let mut cursor = cursor::Cursor::open(txn, self.tenant, &planned.node)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next()? {
            rows.push(
                row.iter()
                    .map(|value| value.to_text().map(String::into_bytes))
                    .collect(),
            );
        }
        let fields = planned
            .columns
            .iter()
            .map(|(name, ty)| FieldDescription::computed(name.clone(), *ty))
            .collect();
        let tag = format!("SELECT {}", rows.len());
        Ok(Outcome::Rows { fields, rows, tag })
    }

    /// Resolves the tables a `SELECT` names and plans against them.
    fn plan_select(&self, txn: &dyn Txn, select: &crate::plan::Select) -> Result<query::Planned> {
        let table = match &select.from {
            Some(name) => Some(self.require_table(txn, name)?),
            None => None,
        };
        let inner = match &select.join {
            Some(join) => Some(self.require_table(txn, &join.table)?),
            None => None,
        };
        query::plan(select, self.tenant, table.as_deref(), inner.as_deref())
    }

    /// `EXPLAIN`: the plan, as rows, and nothing run.
    fn explain(&self, txn: &dyn Txn, statement: &Statement) -> Result<Outcome> {
        // A `SELECT`'s plan is the whole point of `EXPLAIN`, and building it needs the catalog.
        let lines = match statement {
            Statement::Select(select) => {
                let planned = self.plan_select(txn, select)?;
                planned.node.explain(&planned.table, &planned.column_names)
            }
            other => explain_lines(other),
        };
        Ok(Self::explain_rows(lines))
    }

    fn explain_rows(lines: Vec<String>) -> Outcome {
        Outcome::Rows {
            fields: vec![FieldDescription::computed("QUERY PLAN", ColumnType::Text)],
            rows: lines
                .into_iter()
                .map(|line| vec![Some(line.into_bytes())])
                .collect(),
            tag: "EXPLAIN".to_owned(),
        }
    }

    /// Turns a `40001` from `commit` into the `23505` it is, when the key that lost was a unique
    /// index entry.
    ///
    /// The store now **names the key that lost** (`docs/txn-spec.md` §6.1), so the common case is
    /// a lookup in what this transaction wrote and costs nothing. The second look is what is left
    /// of the original design, for a refusal that named no key: `Commit` and `Rollback` do not
    /// answer per key, and this layer will not invent one.
    fn explain_conflict(&self, error: SqlError, written: &Written) -> SqlError {
        let SqlError::SerializationFailure { key, .. } = &error else {
            return error;
        };
        if written.unique_keys.is_empty() {
            return error;
        }

        if let Some(lost) = key {
            return match written.unique_keys.iter().find(|it| &it.key == lost) {
                Some(unique) => SqlError::UniqueViolation {
                    constraint: unique.constraint.clone(),
                    key: Some(unique.detail.clone()),
                },
                // A key this transaction wrote that is not one of its unique index entries: an
                // ordinary row-level race, and still retryable.
                None => error,
            };
        }

        // No key named. Open a transaction and look: the entries that are now present are the
        // ones this transaction collided with, and the first of those names the constraint.
        let Ok(txn) = self.backend.begin() else {
            return error;
        };
        for unique in &written.unique_keys {
            if matches!(txn.get(&unique.key), Ok(Some(_))) {
                return SqlError::UniqueViolation {
                    constraint: unique.constraint.clone(),
                    key: Some(unique.detail.clone()),
                };
            }
        }
        error
    }

    /// The next internal row id for a table that has no primary key of its own.
    ///
    /// Reserved a batch at a time **in a transaction of its own**, which is the point rather than
    /// an optimisation: the counter is one key per table, so bumping it inside the statement's
    /// transaction would make two concurrent inserts into the same table conflict on it and one of
    /// them always lose. A session takes [`catalog::ROW_ID_BATCH`] ids, commits that, and hands
    /// them out from memory (`crate::catalog::allocate_row_ids`, which carries the argument and
    /// the consequence — gaps, exactly as a PostgreSQL sequence leaves them).
    fn next_row_id(&mut self, table_id: u64) -> Result<i64> {
        if let Some((next, end)) = self.row_ids.get_mut(&table_id)
            && *next < *end
        {
            let id = *next;
            *next += 1;
            return Ok(i64::try_from(id).unwrap_or(i64::MAX));
        }

        let mut txn = self.backend.begin()?;
        let first = match crate::catalog::allocate_row_ids(
            &mut *txn,
            self.tenant,
            table_id,
            crate::catalog::ROW_ID_BATCH,
        ) {
            Ok(first) => first,
            Err(error) => {
                let _ = txn.rollback();
                return Err(error);
            }
        };
        txn.commit()?;
        self.row_ids
            .insert(table_id, (first + 1, first + crate::catalog::ROW_ID_BATCH));
        Ok(i64::try_from(first).unwrap_or(i64::MAX))
    }

    /// Adds a notice for the session to send before this statement's `CommandComplete`.
    fn notice(&mut self, notice: SqlError) {
        self.notices.push(notice);
    }

    /// Reads every `$n` in a statement as the type its context gives it, leaving a statement with
    /// no parameters left in it.
    fn bound(
        &self,
        txn: &dyn Txn,
        mut statement: Statement,
        params: &Params<'_>,
    ) -> Result<Statement> {
        if params.values.is_empty() && !bind::has_parameters(&statement) {
            return Ok(statement);
        }
        let tables = self.tables_for(txn, &statement)?;
        let types = bind::infer(&statement, &tables, params.declared);
        bind::substitute(&mut statement, params, &types)?;
        Ok(statement)
    }

    /// The tables a statement is about, in the order their columns appear in a row.
    ///
    /// A join has two, and both are needed wherever names are resolved — which is `Describe` as
    /// much as it is `Execute`. The first version of this returned one, and a prepared `SELECT`
    /// over a join could not be described at all: every driver that prepares its statements, which
    /// is most of them, would have failed on the first join it sent.
    fn tables_for(
        &self,
        txn: &dyn Txn,
        statement: &Statement,
    ) -> Result<Vec<Arc<crate::catalog::TableDef>>> {
        let view = self.catalog_view(txn)?;
        // A name that is not there is not this function's error to raise: the statement will
        // reach it and report it with the message that statement uses.
        bind::table_names(statement)
            .into_iter()
            .filter_map(|name| view.table(name).transpose())
            .collect()
    }

    /// This transaction's view of the catalog, pinned to one version.
    fn catalog_view<'a>(&'a self, txn: &'a dyn Txn) -> Result<crate::catalog::View<'a>> {
        if self.catalog_written {
            return self.catalog.view_uncached(txn, self.tenant);
        }
        self.catalog.view(txn, self.tenant)
    }

    /// A table by name, or `42P01`.
    fn require_table(&self, txn: &dyn Txn, name: &str) -> Result<Arc<crate::catalog::TableDef>> {
        self.catalog_view(txn)?.require_table(name)
    }

    /// A table by id. A name that resolved to an id whose record is missing is corruption, not a
    /// missing table: the two keys are written by one transaction.
    fn table_by_id(&self, txn: &dyn Txn, table_id: u64) -> Result<Arc<crate::catalog::TableDef>> {
        self.catalog_view(txn)?
            .table_by_id(table_id)?
            .ok_or_else(|| {
                SqlError::DataCorrupted(format!(
                    "a name points at table {table_id}, which is not there"
                ))
            })
    }
}

/// Keys read from the store in one round trip, by everything that walks a whole range.
///
/// The same number [`cursor`] uses, and for the same reason: a range has to be read a page at a
/// time or a scan of a large table is a large table in memory.
pub(crate) const SCAN_CHUNK: u32 = 1024;

/// Walks `[start, end)` a page at a time, handing each page to `page`.
///
/// **A whole range cannot be asked for in one call**, and the reason is a seam rather than a
/// preference. [`Txn::scan`]'s `limit` of 0 means "no limit" to this crate's trait and a *page* to
/// the real `TxnClient`, which turns 0 into its protocol default and then caps it
/// (`docs/plans/phase-6a.md` §10a). A caller that asked for everything and got a page would get no
/// error and no clue: a `DROP TABLE` that left rows, or — the one that returns wrong answers — a
/// `CREATE INDEX` whose index is missing every row past the first page, so that a query *using* it
/// answers with fewer rows than the same query without it.
///
/// The loop stops on an **empty** read rather than on a short one. A short page is not evidence
/// that a range is finished: the store may cap a scan below what was asked for, and it answers for
/// one region at a time. That costs one extra round trip at the end of every walk, which is the
/// right price for a termination rule that does not depend on a limit anybody can configure.
pub(crate) fn for_each_page(
    txn: &mut dyn Txn,
    start: &[u8],
    end: &[u8],
    mut page: impl FnMut(&mut dyn Txn, &[(bytes::Bytes, bytes::Bytes)]) -> Result<()>,
) -> Result<()> {
    let mut next = start.to_vec();
    loop {
        let read = txn.scan(&next, end, SCAN_CHUNK)?;
        let Some((last, _)) = read.last() else {
            return Ok(());
        };
        next = query::successor(last);
        page(txn, &read)?;
    }
}

/// What a transaction wrote that changes how a failed commit should be reported.
#[derive(Debug, Default)]
pub(crate) struct Written {
    /// Unique index entries, with what to say if one of them turns out to have been taken.
    pub(crate) unique_keys: Vec<Unique>,
}

/// One unique index entry this transaction wrote, and the error it becomes if it lost the race.
///
/// The message is built *here*, while the row's values are still to hand — after the commit fails
/// there is no transaction left to ask what they were.
#[derive(Debug)]
pub(crate) struct Unique {
    /// The key, so a second look can see whether somebody took it.
    pub(crate) key: Vec<u8>,
    /// The constraint's name.
    pub(crate) constraint: String,
    /// `Key (id)=(1)`, for the `DETAIL` field.
    pub(crate) detail: String,
}

/// The `EXPLAIN` output for a statement. One line per plan node, indented by depth, which is the
/// shape `psql` renders and users read.
fn explain_lines(statement: &Statement) -> Vec<String> {
    match statement {
        Statement::CreateTable(create) => vec![format!("Create Table on {}", create.name)],
        Statement::DropTable(drop) => vec![format!("Drop Table on {}", drop.names.join(", "))],
        Statement::CreateIndex(create) => vec![format!("Create Index on {}", create.table)],
        Statement::DropIndex(drop) => vec![format!("Drop Index on {}", drop.names.join(", "))],
        Statement::AlterTable(alter) => vec![format!("Alter Table on {}", alter.name)],
        // A `SELECT`'s plan is the interesting one, and it needs the catalog to be built, so
        // `EXPLAIN SELECT` is handled where the catalog is in reach rather than here.
        Statement::Select(_) => vec!["Select".to_owned()],
        Statement::Update(update) => vec![format!("Update on {}", update.table)],
        Statement::Delete(delete) => vec![format!("Delete on {}", delete.table)],
        Statement::Insert(insert) => vec![format!(
            "Insert on {} ({} row{})",
            insert.table,
            insert.rows.len(),
            if insert.rows.len() == 1 { "" } else { "s" }
        )],
        // `EXPLAIN EXPLAIN ...` is not something PostgreSQL's grammar admits, so this is
        // unreachable through the parser and is written as a value rather than a panic anyway.
        Statement::Explain(_) => vec!["Explain".to_owned()],
        // `EXPLAIN SET ...` is not PostgreSQL's grammar either, and a session statement has no
        // plan to print: it touches no table and reads no row.
        Statement::Session(session) => vec![session.tag().to_owned()],
        // A checkpoint verb has no access path to choose: it is one key, by name.
        Statement::TimeMachine(_) => vec!["Time Machine".to_owned()],
    }
}

impl Execute for Executor {
    fn execute(&mut self, parsed: &Parsed, params: &Params<'_>) -> Result<Outcome> {
        let statement = parsed.lower()?;
        if let Statement::Session(session) = &statement {
            return self.session_statement(session);
        }
        // After the session statements, because `SET TRANSACTION SNAPSHOT` is the one thing a
        // block may run before it counts as having read anything.
        self.open_used = true;
        self.in_a_transaction(statement, params)
    }

    fn describe(&mut self, parsed: &Parsed, declared: &[u32]) -> Result<Described> {
        let statement = parsed.lower()?;
        // Describing takes a transaction of its own, because typing the parameters needs the
        // catalog and the catalog is data like any other. It writes nothing, so it costs a
        // snapshot and no conflict — and it takes the *session's* snapshot, so that a statement
        // prepared under `esker.read_as_of` is described against the schema it will run on.
        let txn = self.open_txn()?;
        let tables = self.tables_for(&*txn, &statement)?;
        let types = bind::infer(&statement, &tables, declared);
        let parameters = types.iter().copied().map(ColumnType::oid).collect();

        // Planning needs every expression to have a type, and a `$1` has none until now. Nothing
        // is run, so a placeholder of the right type is all the planner needs to answer the shape.
        let mut statement = statement;
        bind::substitute_placeholders(&mut statement, &types);
        let fields = match &statement {
            Statement::Select(select) => Some(
                query::plan(
                    select,
                    self.tenant,
                    tables.first().map(AsRef::as_ref),
                    tables.get(1).map(AsRef::as_ref),
                )?
                .columns
                .into_iter()
                .map(|(name, ty)| FieldDescription::computed(name, ty))
                .collect(),
            ),
            Statement::Explain(_) => Some(vec![FieldDescription::computed(
                "QUERY PLAN",
                ColumnType::Text,
            )]),
            _ => None,
        };
        let _ = txn.rollback();
        Ok(Described { parameters, fields })
    }

    fn take_notices(&mut self) -> Vec<SqlError> {
        std::mem::take(&mut self.notices)
    }

    fn begin(&mut self, read_only: bool) -> Result<()> {
        // A second `BEGIN` never reaches here: the session answers it with PostgreSQL's warning
        // and leaves the block alone.
        self.block_read_only = read_only;
        self.open = Some(self.open_txn()?);
        self.open_used = false;
        self.written = Written::default();
        self.catalog_written = false;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let written = std::mem::take(&mut self.written);
        self.catalog_written = false;
        self.end_of_block();
        let Some(txn) = self.open.take() else {
            return Ok(());
        };
        match txn.commit() {
            Ok(_) => Ok(()),
            // The same translation the autocommit path does. A block's commit is where a client
            // that wrote several rows finds out it lost, and it deserves the same answer.
            Err(error) => Err(self.explain_conflict(error, &written)),
        }
    }

    fn rollback(&mut self) -> Result<()> {
        self.written = Written::default();
        self.catalog_written = false;
        self.end_of_block();
        let Some(txn) = self.open.take() else {
            return Ok(());
        };
        txn.rollback()
    }
}
