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

mod aggregate;
mod assign;
mod bind;
mod comment;
mod cursor;
mod ddl;
mod deferred;
mod dml;
pub(crate) mod explain;
mod flashback;
mod foreign_key;
mod fragment;
mod index;
mod job;

pub use job::BATCH_ROWS;
pub(crate) mod query;
pub mod redrive;
mod savepoint;
mod subquery;
mod table_function;
mod typedef;
mod values;
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
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// Runs statements for one connection.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent fact about the open transaction — has it written the \
              catalog, has it run a statement, was it opened read-only, did it change a columnar \
              setting — and a flags struct or a bitfield would hide what each one means"
)]
pub struct Executor {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    /// This node's advisory locks, shared with every other session it serves.
    ///
    /// **A private table by default**, which is what a single-session test wants and what keeps
    /// the corpus replay honest; `Executor::sharing_advisory_locks` joins the node's, and the
    /// server's session factory is the one caller (`crate::advisory`).
    locks: Arc<crate::advisory::Locks>,
    /// Who this session is, in that table. Handed out once at construction and never reused, so a
    /// lock released by one session cannot be mistaken for a later one's.
    session: crate::advisory::Session,

    pub(crate) tenant: u64,
    /// The database this session is connected to, which is the tenant above under its name.
    ///
    /// **The name is carried rather than looked up.** `current_database()` runs four times while
    /// `ActiveRecord` connects and the tenant is already decided by then — reading the directory
    /// again per statement would answer the same thing and cost a catalog read to do it.
    pub(crate) database: String,
    /// The transaction an explicit `BEGIN` opened. `None` means the next statement gets its own.
    open: Option<Box<dyn Txn>>,
    /// What the open transaction has written that changes how a failed commit reads. It
    /// accumulates across the whole block, because the commit that fails is the block's and not
    /// any one statement's — the first version of this recorded per statement and lost the
    /// translation entirely for a client that used `BEGIN`.
    written: Written,
    /// The constraint checks this transaction owes, and what it has been told about them
    /// (`crate::exec::deferred`).
    ///
    /// **Behind a `RefCell`, deliberately.** A row is written through `&Executor` — the write path
    /// borrows the catalog out of it while it works — and threading `&mut` down to the one line
    /// that registers a check turned four unrelated signatures mutable and did not stop there.
    /// One session is one thread, so the cell is a borrow discipline rather than a lock.
    constraints: std::cell::RefCell<deferred::Constraints>,
    /// Notices produced by the statement that just ran, waiting for the session to send them.
    ///
    /// What actually leaves is filtered by `client_min_messages`
    /// ([`Executor::take_notices`]), which is the whole reason `ActiveRecord` sets it.
    /// **Interior-mutable, because a notice is a side channel rather than state.** Statement
    /// binding runs behind `&self` and is where `pg_advisory_unlock` decides it has nothing to
    /// release — a `WARNING` PostgreSQL raises and `ActiveRecord` reads past. Threading `&mut`
    /// through the whole bind path to carry one message would have been the tail wagging the dog.
    notices: std::cell::RefCell<Vec<SqlError>>,
    /// Session parameters this session has set, by [`crate::parameter::Parameter::name`]. A
    /// parameter absent here reads back its boot value, which is what makes `RESET` a removal
    /// rather than a second assignment.
    parameters: savepoint::Parameters,
    /// The parameters as they stood when the open block began, or `None` outside one.
    ///
    /// A `SET` is transactional on a real server: `ROLLBACK` puts the old value back and `COMMIT`
    /// keeps the new one (measured, `tests/corpus/pg19_set.txt`). A whole copy, for the reason the
    /// savepoint marks hold one — there are six parameters and a block is not a hot path.
    block_parameters: Option<savepoint::Parameters>,
    /// Row ids reserved for this session but not yet handed out: `table_id -> (next, end)`.
    /// See [`Executor::next_row_id`].
    row_ids: std::collections::BTreeMap<u64, (u64, u64)>,
    /// One session's reserved block per sequence: the next value it will hand out and the first
    /// value past its block. See [`Executor::next_sequence_value`].
    sequences: std::collections::BTreeMap<u64, (i64, i64)>,
    /// The sequences this session has a `currval` for, which is **not** the same set as the ones
    /// it holds a block of.
    ///
    /// They were one map until `DISCARD SEQUENCES` needed to tell them apart: it makes `currval`
    /// *undefined* again — `55000`, the answer a fresh connection gives — while `nextval` carries
    /// on where it was. Clearing the block instead would answer the `currval` line correctly and
    /// then skip a whole batch on the next `nextval`, which is what the capture caught: `2` on a
    /// real server, `33` here.
    currval_defined: std::collections::BTreeSet<u64>,
    /// The sequence this session last took a value from, which is the whole of what `lastval()`
    /// is. `None` until there has been one, and `55000` is what that answers with.
    last_sequence: Option<u64>,
    /// The open block's savepoints and the pre-images they can undo to
    /// (`crate::exec::savepoint`). Empty outside a block, and empty inside one until the first
    /// `SAVEPOINT` — which is what keeps an ordinary transaction paying nothing for this.
    savepoints: savepoint::Savepoints,
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
    /// Where columnar placement is reported, on a node that has a placement driver.
    columnar: Option<Arc<dyn crate::pd::ColumnarReport>>,
    /// Where this node sends fragments, or `None` for a node that cannot ask one.
    ///
    /// `None` is a real configuration and not a broken one — a cluster with no placement driver,
    /// and every in-process test cluster in this crate — so a node without it plans every query on
    /// rows and says so in `EXPLAIN` ([`crate::plan::routing::Reason::NoFragmentService`]).
    fragments: Option<Arc<dyn crate::fragment::FragmentSource>>,
    /// Whether the open transaction has changed a table's columnar setting.
    ///
    /// Set by the `ALTER` and acted on **after the commit**, because what is reported is what the
    /// cluster can now read: a report sent from inside the transaction would name a wish that a
    /// rollback could take back, and PD has no way to hear that it was taken back.
    columnar_changed: bool,
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
    /// The table as an `Arc`, for a deferred check that outlives the statement.
    ///
    /// A check is verified after the statement that registered it has finished, so it cannot
    /// borrow the definition it was written against — and it must not re-read it either, because
    /// an `ALTER` later in the same transaction would move the constraint under it.
    pub(crate) fn table_arc(table: &crate::catalog::TableDef) -> Arc<crate::catalog::TableDef> {
        Arc::new(table.clone())
    }

    /// Whether `name` is deferred in this transaction, given how it was declared.
    pub(crate) fn constraint_is_deferred(&self, name: &str, initially_deferred: bool) -> bool {
        self.constraints.borrow().deferred(name, initially_deferred)
    }

    /// Registers a check to run at `COMMIT`, or at the next `SET CONSTRAINTS … IMMEDIATE`.
    pub(crate) fn defer_check(&self, check: deferred::Check) {
        self.constraints.borrow_mut().push(check);
    }

    /// `SET CONSTRAINTS`, run against a transaction of its own when there is no block.
    ///
    /// **It needs one even outside a block**: the names are checked against the catalog whether or
    /// not there is anything to defer, so `SET CONSTRAINTS nosuch IMMEDIATE` is `42704` on its own
    /// as it is inside a transaction. Measured.
    fn set_constraints_statement(&mut self, names: &[String], deferred: bool) -> Result<Outcome> {
        if let Some(txn) = self.open.take() {
            let outcome = self.set_constraints(names, deferred, &*txn);
            self.open = Some(txn);
            outcome?;
        } else {
            let txn = self.open_txn()?;
            let outcome = self.set_constraints(names, deferred, &*txn);
            // **Outside a block the mode belongs to the implicit transaction**, which ends with
            // this statement — so it is forgotten here. Keeping it made a later `BEGIN` inherit a
            // `SET CONSTRAINTS ALL DEFERRED` from a statement that had already finished, and a
            // constraint that was declared immediate stopped checking at the statement.
            self.constraints.borrow_mut().clear();
            let _ = txn.rollback();
            outcome?;
        }
        Ok(Outcome::done("SET CONSTRAINTS"))
    }

    /// Whether a constraint of this name exists and may be deferred.
    ///
    /// `42704` when nothing has the name — a real server checks that a constraint exists before
    /// it checks whether it is deferrable, and the two errors are different SQLSTATEs.
    ///
    /// `UNIQUE` and `EXCLUDE` answer, which are the two kinds this node can defer. A third
    /// registers here as it registers in `crate::exec::deferred`: one more place to look, in the
    /// same order.
    fn constraint_deferrable(&self, txn: &dyn Txn, name: &str) -> Result<bool> {
        let relations = crate::catalog::pg_relations::Relations::read(txn, self.tenant)?;
        for table in relations.rows().filter_map(|row| relations.table(row)) {
            for index in &table.indexes {
                if index.name == name {
                    return Ok(index.deferrable());
                }
            }
            if table.primary_key_name == name {
                // A primary key is never deferrable here; PostgreSQL's may be, and declaring one
                // that way is refused where it is lowered.
                return Ok(false);
            }
            for check in &table.checks {
                if check.name == name {
                    return Ok(false);
                }
            }
            for exclude in &table.excludes {
                if exclude.name == name {
                    return Ok(exclude.deferrable);
                }
            }
            for key in &table.foreign_keys {
                if key.name == name {
                    // `DEFERRABLE` on a foreign key is recorded and changes nothing yet, so it
                    // cannot be deferred either — and saying so is better than accepting a
                    // `SET CONSTRAINTS` that would not defer it.
                    return Ok(false);
                }
            }
        }
        Err(SqlError::ConstraintDoesNotExist(name.to_owned()))
    }

    /// The deferred checks, then the commit — the pair every transaction ends with.
    ///
    /// A failed check leaves nothing committed: the transaction is rolled back, which is what a
    /// real server does and is visible afterwards as the rows not being there.
    fn checked_and_committed(&mut self, txn: Box<dyn Txn>, written: &Written) -> Result<()> {
        let owed = self.constraints.borrow_mut().take();
        for check in &owed {
            if let Err(error) = check.verify(&*txn, self.tenant) {
                let _ = txn.rollback();
                return Err(error);
            }
        }
        match txn.commit() {
            Ok(_) => Ok(()),
            Err(error) => Err(self.explain_conflict(error, written)),
        }
    }

    /// Runs every check the transaction owes, and forgets them.
    ///
    /// The **first** failure is the answer, which is what a real server reports: one `23505`
    /// naming one constraint, not a list.
    fn run_deferred_checks(&mut self) -> Result<()> {
        if self.constraints.borrow().is_empty() {
            return Ok(());
        }
        let checks = self.constraints.borrow_mut().take();
        let Some(txn) = self.open.as_deref() else {
            return Ok(());
        };
        for check in &checks {
            check.verify(txn, self.tenant)?;
        }
        Ok(())
    }

    /// `SET CONSTRAINTS { ALL | name [, …] } { DEFERRED | IMMEDIATE }`.
    ///
    /// **`IMMEDIATE` runs what is already owed**, at once — which is where the `23505` appears for
    /// a transaction that deferred a violation and then asked for the answer. `ALL` reaches only
    /// the constraints that are deferrable; a plain `UNIQUE` is unaffected by it and refuses a
    /// statement that names it (`42809`). Measured, all three.
    pub(crate) fn set_constraints(
        &mut self,
        names: &[String],
        deferred: bool,
        txn: &dyn Txn,
    ) -> Result<()> {
        if names.is_empty() {
            self.constraints.borrow_mut().set_all(deferred);
            if !deferred {
                let owed = self.constraints.borrow_mut().take();
                for check in &owed {
                    check.verify(txn, self.tenant)?;
                }
            }
            return Ok(());
        }
        for name in names {
            // **Not deferrable is an error and not a no-op**, even for `IMMEDIATE`, which would
            // change nothing: PostgreSQL refuses the statement either way. Measured.
            if !self.constraint_deferrable(txn, name)? {
                return Err(SqlError::ConstraintNotDeferrable(name.clone()));
            }
            self.constraints
                .borrow_mut()
                .set_one(name.clone(), deferred);
        }
        if deferred {
            return Ok(());
        }
        // **The transaction handed in, not `self.open`** — which the caller took out before
        // calling, so reading it here found `None` and skipped every check it was asked to run.
        let owed: Vec<deferred::Check> = names
            .iter()
            .flat_map(|name| self.constraints.borrow_mut().take_named(name))
            .collect();
        for check in &owed {
            check.verify(txn, self.tenant)?;
        }
        Ok(())
    }

    /// An executor over a store and a shared catalog cache.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, catalog: Arc<Catalog>, tenant: u64) -> Self {
        let locks = Arc::new(crate::advisory::Locks::new());
        let session = locks.session();
        Executor {
            backend,
            catalog,
            locks,
            session,
            tenant,
            database: crate::parse::DATABASE_NAME.to_owned(),
            open: None,
            written: Written::default(),
            constraints: std::cell::RefCell::default(),
            notices: std::cell::RefCell::new(Vec::new()),
            parameters: savepoint::Parameters::new(),
            block_parameters: None,
            row_ids: std::collections::BTreeMap::new(),
            sequences: std::collections::BTreeMap::new(),
            currval_defined: std::collections::BTreeSet::new(),
            last_sequence: None,
            savepoints: savepoint::Savepoints::default(),
            catalog_written: false,
            read_as_of: None,
            open_used: false,
            block_read_only: false,
            columnar: None,
            fragments: None,
            columnar_changed: false,
        }
    }

    /// The same executor, reporting columnar placement to `report` after an `ALTER` commits.
    ///
    /// Without one an `ALTER ... SET (columnar_replicas = N)` still writes its durable catalog
    /// record and simply tells nobody — which is a node with no placement driver, and is what
    /// every test cluster in this crate is.
    #[must_use]
    pub fn reporting_columnar_to(mut self, report: Arc<dyn crate::pd::ColumnarReport>) -> Self {
        self.columnar = Some(report);
        self
    }

    /// The same executor, serving a named database.
    ///
    /// The tenant and the name are two halves of one fact and the caller has both — the startup
    /// packet named the database and the directory answered which tenant it is
    /// ([ADR 0052](../../../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md)).
    #[must_use]
    pub fn serving_database(mut self, name: impl Into<String>) -> Self {
        self.database = name.into();
        self
    }

    /// The same executor, able to ask a columnar learner to evaluate a plan fragment.
    ///
    /// Without one the planner still *decides* — and decides rows, naming the reason — so an
    /// `EXPLAIN` on a node with no placement driver says why rather than saying nothing
    /// (ADR 0022 milestone 4).
    #[must_use]
    pub fn asking_fragments_of(mut self, source: Arc<dyn crate::fragment::FragmentSource>) -> Self {
        self.fragments = Some(source);
        self
    }

    /// This session's `esker.engine`.
    fn engine(&self) -> crate::plan::routing::Setting {
        // The parameter is in the table, so `lookup` cannot fail; a session that never set it
        // reads the boot value, which is `auto`.
        crate::parameter::lookup("esker.engine").map_or_else(
            |_| crate::plan::routing::Setting::default(),
            |parameter| crate::plan::routing::Setting::parse(&self.parameter(parameter)),
        )
    }

    /// Marks the open transaction as having changed a table's columnar setting.
    pub(crate) fn columnar_changed(&mut self) {
        self.columnar_changed = true;
    }

    /// Asserts this tenant's columnar wishes to PD, if the transaction that just committed
    /// changed one.
    ///
    /// **The scan is the message** (ADR 0022 Decision 5): what is sent is the whole set read back
    /// out of the catalog, never a delta of what this statement did, so a report is complete by
    /// construction and a lost one costs nothing. Which is also why a failure here is a log line
    /// and not the statement's error: the `ALTER` is committed and durable, and the lease
    /// refresher re-asserts the same content on its next pass.
    fn report_columnar(&mut self) {
        if !std::mem::take(&mut self.columnar_changed) {
            return;
        }
        let Some(report) = self.columnar.as_ref() else {
            return;
        };
        match crate::pd::columnar_wishes(&*self.backend, self.tenant) {
            Ok(wishes) => {
                if let Err(error) = report.report(wishes) {
                    tracing::warn!(
                        %error,
                        "could not report columnar placement to the placement driver; the next \
                         lease refresh re-asserts it"
                    );
                }
            }
            Err(error) => tracing::warn!(
                %error,
                "could not read this tenant's columnar settings after the ALTER committed; the \
                 next lease refresh re-asserts them"
            ),
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
            let mut savepoints = std::mem::take(&mut self.savepoints);
            // With a savepoint open the statement writes through a `Recording`, which takes each
            // key's pre-image on the way past. That is what makes "every write is undoable" a fact
            // about the type the executor was handed rather than a rule every call site follows.
            let outcome = self.bound(&*txn, statement, params).and_then(|statement| {
                if savepoints.recording() {
                    let mut recording = savepoint::Recording::new(&mut *txn, &mut savepoints);
                    let outcome = self.run_recording(&mut recording, &statement, &mut written);
                    // A pre-image that could not be read is reported here, at the first place that
                    // can say anything: `put` and `delete` return nothing by contract.
                    recording.finish().and(outcome)
                } else {
                    self.run_recording(&mut *txn, &statement, &mut written)
                }
            });
            self.open = Some(txn);
            self.written = written;
            self.savepoints = savepoints;
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
            // **A statement outside a block is its own transaction, so its commit is here** — and
            // a deferred check runs at every commit, this one included. Measured: the second of
            // two colliding inserts fails on its own, with the row not written, exactly as an
            // immediate constraint would refuse it; deferral is a property of the *transaction*,
            // and an implicit transaction is one statement long.
            Ok(outcome) => match self.checked_and_committed(txn, &written) {
                Ok(()) => {
                    // After the commit, and only after it.
                    self.report_columnar();
                    Ok(outcome)
                }
                Err(error) => Err(error),
            },
            Err(error) => {
                // The rollback's own failure is not what the client asked about; the statement's
                // error is. Reporting the second would hide the first.
                let _ = txn.rollback();
                Err(error)
            }
        };
        self.catalog_written = false;
        // A statement that did not commit changed nothing PD could act on, whether it was rolled
        // back or refused.
        self.columnar_changed = false;
        outcome
    }

    fn run_recording(
        &mut self,
        txn: &mut dyn Txn,
        statement: &Statement,
        written: &mut Written,
    ) -> Result<Outcome> {
        // Every reason this statement may not write, checked *here*, before anything is planned.
        // It cannot be caught later: `Txn::put` is buffered and returns nothing, so a write that
        // was going to be refused would be dropped in silence and the statement would report
        // success.
        if let Some(command) = statement.write_command() {
            if txn.is_read_only() || self.block_read_only {
                return Err(SqlError::ReadOnlyTransaction(command));
            }
            // **The schema lease, and only for writes.** A node past its lease may be acting on a
            // schema the cluster has moved two states beyond, which is the one thing ADR 0020's
            // states do not make safe. Reads are untouched: a reader's snapshot already agrees
            // with the rows it can see.
            if self.backend.schema_lease_remaining().is_none() {
                return Err(SqlError::SchemaLeaseExpired { command });
            }
        }
        // Before the statement rather than after it: a DDL statement that fails part-way has
        // still written, and the reads it makes on the way are its own uncommitted catalog.
        self.catalog_written |= statement.writes_catalog();
        match statement {
            Statement::CreateTable(create) => ddl::create_table(self, txn, create),
            Statement::CreateExtension(create) => ddl::create_extension(self, txn, create),
            Statement::DropExtension(drop) => ddl::drop_extension(self, txn, drop),
            Statement::CreateSchema(create) => ddl::create_schema(self, txn, create),
            Statement::CreateDatabase(create) => ddl::create_database(self, txn, create),
            Statement::DropDatabase(drop) => ddl::drop_database(self, txn, drop),
            Statement::DropSchema(drop) => ddl::drop_schema(self, txn, drop),
            Statement::AlterSchemaRename(rename) => ddl::alter_schema_rename(self, txn, rename),
            Statement::DropSequence(drop) => ddl::drop_sequence(self, txn, drop),
            Statement::CreateSequence(create) => ddl::create_sequence(self, txn, create),
            Statement::DropFunction(drop) => ddl::drop_function(self, txn, drop),
            Statement::CreateFunction(create) => ddl::create_function(self, txn, create),
            Statement::CreateTrigger(create) => ddl::create_trigger(self, txn, create),
            Statement::DropTrigger(drop) => ddl::drop_trigger(self, txn, drop),
            Statement::DropTable(drop) => ddl::drop_table(self, txn, drop),
            Statement::CreateIndex(create) => ddl::create_index(self, txn, create),
            Statement::DropIndex(drop) => ddl::drop_index(self, txn, drop),
            Statement::Comment(statement) => comment::comment(self, txn, statement),
            Statement::CreateType(create) => typedef::create(self, txn, create),
            Statement::DropType(drop) => typedef::drop(self, txn, drop),
            Statement::AlterTable(alter) => ddl::alter_table(self, txn, alter),
            Statement::Insert(insert) => dml::insert(self, txn, insert, written),
            Statement::Select(select) => self.select(txn, select),
            Statement::Update(update) => dml::update(self, txn, update, written),
            Statement::Delete(delete) => dml::delete(self, txn, delete),
            Statement::Explain(inner, analyze) => self.explain(txn, inner, *analyze),
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
            // **What the session set, not every parameter there is.** A real server's `RESET ALL`
            // leaves alone the ones it may not change; clearing what was set is the same answer
            // and needs no read-only special case.
            SessionStatement::ResetAll => {
                self.parameters.clear();
                Ok(Outcome::done("RESET"))
            }
            // **Each target resets what it names and nothing else**, which the capture measured
            // one at a time: `DISCARD PLANS` leaves the advisory lock and the temp table, and only
            // `ALL` releases a lock. The prepared statements are the session's, not the
            // executor's, and are cleared where they live (`crate::pgwire::session`).
            SessionStatement::Discard(target) => {
                // **The `25001` is raised here** and not with `CREATE DATABASE`'s, because a
                // session statement returns from `execute` before that check is reached — so a
                // rule written beside the others would never fire. Measured: only `ALL` is
                // refused; `PLANS`, `SEQUENCES` and `TEMP` all run inside a block.
                if matches!(target, crate::plan::DiscardTarget::All) && self.open.is_some() {
                    return Err(SqlError::NotInATransactionBlock("DISCARD ALL"));
                }
                if matches!(target, crate::plan::DiscardTarget::All) {
                    self.parameters.clear();
                    self.locks.unlock_all(self.session);
                }
                if matches!(
                    target,
                    crate::plan::DiscardTarget::All | crate::plan::DiscardTarget::Sequences
                ) {
                    // `currval` becomes **undefined** again rather than stale — the next call is
                    // `55000`, the answer a fresh connection gives. **The reserved block stays**:
                    // throwing it away would answer that line correctly and then skip a batch on
                    // the next `nextval`, which is `2` on a real server and was `33` here.
                    self.currval_defined.clear();
                    self.last_sequence = None;
                }
                // `PLANS` caches nothing here and `TEMP` has nothing to drop
                // (`CREATE TEMPORARY TABLE` is a named refusal), so both are honest no-ops for as
                // long as those two facts hold. Each is declared in the corpus rather than left to
                // be assumed.
                Ok(Outcome::done(statement.tag()))
            }
            SessionStatement::SetParameter { name, value } => {
                self.set_parameter(name, value.as_deref())?;
                Ok(Outcome::done("SET"))
            }
            SessionStatement::ShowParameter(name) => {
                let parameter = crate::parameter::lookup(name)?;
                Ok(Outcome::Rows {
                    // Named by PostgreSQL's **own** spelling and not the user's: `SHOW
                    // intervalstyle` answers a column called `IntervalStyle`. Measured.
                    fields: vec![FieldDescription::computed(
                        parameter.reported,
                        ColumnType::Text,
                    )],
                    rows: vec![vec![Some(self.parameter(parameter).into_bytes())]],
                    tag: "SHOW".to_owned(),
                })
            }
        }
    }

    /// `SET <parameter> = <value>`, or `RESET` / `TO DEFAULT`, which are one operation.
    ///
    /// Three checks in PostgreSQL's own order, and the third is this crate's: the parameter must
    /// exist (`42704`), the value must be one it takes (`22023`), and this node must **mean** it
    /// (`crate::parameter::Parameter::honour`). The third is what keeps a `SET` from being
    /// accepted and ignored, which is the failure mode a client cannot see.
    fn set_parameter(&mut self, name: &str, value: Option<&str>) -> Result<()> {
        let parameter = crate::parameter::lookup(name)?;
        let Some(value) = value else {
            if parameter.read_only {
                return Err(SqlError::CannotChangeParameter(parameter.reported));
            }
            self.parameters.remove(parameter.name);
            return Ok(());
        };
        let value = parameter.normalise(value)?;
        parameter.honour(&value)?;
        self.parameters.insert(parameter.name, value);
        Ok(())
    }

    /// What this session reports for a parameter: what it set, or the boot value.
    fn parameter(&self, parameter: &crate::parameter::Parameter) -> String {
        self.parameters
            .get(parameter.name)
            .cloned()
            .unwrap_or_else(|| parameter.boot.to_owned())
    }

    /// Whether `client_min_messages` lets a message of this severity out.
    ///
    /// PostgreSQL's ordering, and the two levels this node actually raises are `NOTICE` and
    /// `WARNING`. `ActiveRecord` sets `warning` at connect precisely to silence the first — a
    /// `DROP TABLE IF EXISTS` for a table that is not there says so, every time, and a framework
    /// running a migration does not want to hear it.
    fn reports(&self, severity: crate::error::Severity) -> bool {
        use crate::error::Severity;

        // Ordered as PostgreSQL orders them; a message is sent when its level is at least the
        // threshold. Everything below `notice` is a level this node never raises.
        const ORDER: &[&str] = &[
            "debug5", "debug4", "debug3", "debug2", "debug1", "log", "notice", "warning", "error",
        ];
        let level = match severity {
            Severity::Notice => "notice",
            Severity::Warning => "warning",
            // An error is not a notice: it goes out through `ErrorResponse`, which no threshold
            // suppresses, and `client_min_messages` has never governed it.
            Severity::Error | Severity::Fatal => return true,
        };
        let threshold = self
            .parameters
            .get("client_min_messages")
            .map_or("notice", String::as_str);
        let rank = |name: &str| ORDER.iter().position(|level| *level == name).unwrap_or(0);
        rank(level) >= rank(threshold)
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
        self.block_parameters = None;
        self.block_read_only = false;
        if self.read_as_of.as_ref().is_some_and(|as_of| as_of.local) {
            self.read_as_of = None;
        }
    }

    /// `SELECT`: plan it, then pull every row through.
    fn select(&mut self, txn: &mut dyn Txn, select: &crate::plan::Select) -> Result<Outcome> {
        // A sequence function is a **write**, not a value of a row, so it runs here — once, in the
        // order the target list names it — and what the planner sees is the number it produced.
        // Running it inside the plan would run it once per row, which is what PostgreSQL does over
        // a `FROM` and is why that shape is refused rather than approximated.
        let resolved = self.resolve_sequence_calls(&*txn, select)?;
        let select = resolved.as_ref();
        let mut planned = self.plan_select(txn, select)?;
        // The fragments, before the cursor: a `Cursor` has no way to make a network call, and
        // running them here is what puts the fallback in the *same* transaction at the *same*
        // snapshot as the plan it replaces.
        if let Some(source) = self.fragments.clone() {
            fragment::resolve(&mut planned.node, &*source, txn.start_ts());
        }
        // And the subqueries, for the same reason and in the same place: a `Cursor` has a row and
        // no transaction, so running them here is what puts their answers in the *same*
        // transaction at the *same* snapshot as the plan that reads them.
        subquery::resolve(&mut planned.node, &*txn, self.tenant)?;
        // And a sequence read, which is the opposite case and in the same place: its value must
        // come from **outside** this transaction, because a sequence is not transactional and a
        // read through the statement's own snapshot would report the sequence as of `BEGIN`.
        self.fill_sequence_reads(&mut planned.node)?;
        let mut cursor = cursor::Cursor::open(txn, self.tenant, &planned.node)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next()? {
            rows.push(
                row.iter()
                    .enumerate()
                    .map(|(at, value)| {
                        // **An enum leaves as its label.** The ordinal is what was ordered,
                        // grouped and indexed by — all of that happened below this line — and the
                        // label is what a client is told, which is the whole shape ADR 0050 chose.
                        match planned.columns.get(at).and_then(|c| c.user_type.as_ref()) {
                            Some(def) => assign::from_enum(value, def).to_text(),
                            None => value.to_text(),
                        }
                        .map(String::into_bytes)
                    })
                    .collect(),
            );
        }
        let fields = planned
            .columns
            .iter()
            .map(|column| match &column.user_type {
                Some(def) => FieldDescription::of_user_type(
                    column.name.clone(),
                    u32::try_from(def.oid).unwrap_or(0),
                ),
                None => FieldDescription::of(column.name.clone(), column.ty, column.typmod),
            })
            .collect();
        let tag = format!("SELECT {}", rows.len());
        Ok(Outcome::Rows { fields, rows, tag })
    }

    /// Runs every sequence function in a target list and puts its value back in its place.
    ///
    /// `Cow`-shaped by hand: a statement with none — every statement but a handful — is handed
    /// back untouched and nothing is cloned.
    ///
    /// **Only with no `FROM`.** `SELECT nextval('s')` is what every client writes and is one call;
    /// `SELECT nextval('s') FROM t` is one call *per row* on a real server, which is a side effect
    /// inside the row loop and a different feature. It is `0A000` naming the function rather than
    /// quietly running once, because a client that got one number where it expected four would
    /// have no way to tell.
    fn resolve_sequence_calls<'s>(
        &mut self,
        txn: &dyn Txn,
        select: &'s crate::plan::Select,
    ) -> Result<SelectRef<'s>> {
        use crate::plan::{Expr, Literal, SelectItem};
        if !select
            .projection
            .iter()
            .any(|item| matches!(item, SelectItem::Expr { expr, .. } if has_sequence_call(expr)))
        {
            // Nothing to do, and nothing anywhere else either: a call outside the target list is
            // refused by the planner, which is where every other expression is checked.
            return Ok(SelectRef::Borrowed(select));
        }
        if select.from.is_some() {
            return Err(SqlError::unsupported(
                "a sequence function in a SELECT with a FROM clause",
            ));
        }
        let mut resolved = select.clone();
        // **Every call, wherever it sits**, and not only a bare one in the target list.
        // `pg_typeof(setval('s', 3, true))` and `nextval('s') + 1` are both ordinary statements on
        // a real server, and a call this pass did not reach used to arrive at the row evaluator,
        // which has no session to draw a value from and answered `XX000`. Two passes rather than
        // one because running a call needs `&mut self` and rewriting the tree needs `&mut expr`:
        // the walks visit in the same order, so the nth call collected is the nth replaced.
        let mut calls = Vec::new();
        for item in &resolved.projection {
            if let SelectItem::Expr { expr, .. } = item {
                bind::descend(expr, &mut |expr: &Expr| {
                    if let Expr::Sequence(call) = expr {
                        calls.push(call.clone());
                    }
                });
            }
        }
        let mut values = Vec::with_capacity(calls.len());
        for call in &calls {
            values.push(self.run_sequence_call(txn, call)?);
        }
        let mut at = 0;
        for item in &mut resolved.projection {
            let SelectItem::Expr { expr, alias } = item else {
                continue;
            };
            // PostgreSQL names the column after the function, so a bare call the user did not
            // alias takes the function's own name. A call *inside* an expression names nothing:
            // the enclosing function does.
            if let Expr::Sequence(call) = expr
                && alias.is_none()
            {
                *alias = Some(call.func.name().to_owned());
            }
            bind::walk_expr_mut(expr, &mut |expr: &mut Expr| {
                if matches!(expr, Expr::Sequence(_)) {
                    if let Some(value) = values.get(at) {
                        *expr = Expr::Literal(Literal::Typed(Box::new(Datum::Int8(*value))));
                    }
                    at += 1;
                }
            });
        }
        Ok(SelectRef::Owned(Box::new(resolved)))
    }

    /// One sequence function, and the write it is.
    fn run_sequence_call(
        &mut self,
        txn: &dyn Txn,
        call: &crate::plan::SequenceCall,
    ) -> Result<i64> {
        use crate::plan::SequenceFunc;
        // `lastval()` names no sequence: it is the last value *this session* got from any of them,
        // and it is `55000` before there has been one.
        let SequenceFunc::LastVal = call.func else {
            let name = call.name.as_deref().unwrap_or_default();
            let sequence = self.require_sequence(txn, name)?;
            return match call.func {
                SequenceFunc::NextVal => self.next_sequence_value(sequence.id),
                SequenceFunc::CurrVal => self
                    .currval_defined
                    .contains(&sequence.id)
                    .then(|| self.sequences.get(&sequence.id).map(|(next, _)| next - 1))
                    .flatten()
                    // **The sequence's own name, not the spelling that reached it**: by here the
                    // name has resolved, and `currval('"public"."s"')` is about the sequence `s`.
                    .ok_or_else(|| {
                        SqlError::SequenceNotYetDefined(Some(
                            crate::catalog::split_qualified(&sequence.name).1.to_owned(),
                        ))
                    }),
                SequenceFunc::SetVal => self.set_sequence(sequence.id, call),
                SequenceFunc::LastVal => unreachable!("handled above"),
            };
        };
        let id = self
            .last_sequence
            .ok_or(SqlError::SequenceNotYetDefined(None))?;
        self.currval_defined
            .contains(&id)
            .then(|| self.sequences.get(&id).map(|(next, _)| next - 1))
            .flatten()
            .ok_or(SqlError::SequenceNotYetDefined(None))
    }

    /// A sequence by name, or the two refusals a real server gives: `42P01` for a name that is
    /// nothing and `42809` for one that is something else.
    ///
    /// **The name arrives inside a string**, exactly as `::regclass`'s does, so a schema in it is a
    /// dot rather than the separator the parser would have produced and either part may be quoted.
    /// `reset_pk_sequence!` sends `setval('"public"."accounts_id_seq"', …)` on every fixture load,
    /// and resolving that as one opaque name is run 50's top regression — 4,873 tests in 93 files.
    ///
    /// A name with no schema in it resolves along the `search_path`, which is what makes
    /// `nextval('s')` find a sequence the session can see and nothing it cannot.
    pub(super) fn require_sequence(
        &self,
        txn: &dyn Txn,
        name: &str,
    ) -> Result<crate::catalog::SequenceDef> {
        let stored = self.resolve_unqualified(txn, &crate::catalog::parse_qualified(name))?;
        // **The name as written, not as it would be stored**: a sequence in `public` is stored
        // bare, so the stored form has forgotten a `public.` the caller typed and PostgreSQL has
        // not — `relation "public.nosuch_seq" does not exist`. Measured.
        let printed = crate::catalog::written_display(name);
        let view = self.catalog_view(txn)?;
        match view.relation(&stored)? {
            // **Read straight from the record, not through the table**: a sequence no column
            // owns is filed under `STANDALONE_SEQUENCE_OWNER`, which has no `TableDef` behind it,
            // and a column may own more than one — neither of which the old lookup, which asked
            // the table for the sequence *filling* a column, could express.
            Some(crate::catalog::Relation::Sequence {
                table_id,
                sequence_id,
            }) => crate::catalog::sequence_by_id(txn, self.tenant, table_id, sequence_id)?
                .ok_or(SqlError::UndefinedTable(printed)),
            // No `HINT`: PostgreSQL sends one only for the `DROP` statements, where there is
            // another verb to point at. `nextval` over a table has nothing to suggest.
            //
            // **The name is bare here where the `42P01` above is qualified**, and that is
            // PostgreSQL's own asymmetry: by this point the name has resolved, so what is being
            // reported is the relation rather than the spelling — `nextval('"public"."cap"')` is
            // `"cap" is not a sequence`. Measured.
            Some(_) => Err(SqlError::WrongObjectType {
                name: crate::catalog::split_qualified(&stored).1.to_owned(),
                expected: "a sequence",
                found: "",
            }),
            None => Err(SqlError::UndefinedTable(printed)),
        }
    }

    /// `setval`: where the sequence resumes from, and what this session's `currval` now answers.
    ///
    /// It **discards this session's cached block**, which is not an optimisation detail: without
    /// it the next `nextval` would keep handing out the numbers reserved before the `setval` and
    /// the statement would have done nothing visible. Other sessions' blocks are not discarded and
    /// cannot be — PostgreSQL's `CACHE n` has the same hole and documents it.
    fn set_sequence(&mut self, sequence_id: u64, call: &crate::plan::SequenceCall) -> Result<i64> {
        let value = call.value.unwrap_or_default();
        if value < 1 {
            return Err(SqlError::SetvalOutOfBounds {
                sequence: call.name.clone().unwrap_or_default(),
                value,
            });
        }
        // `is_called` true means the value has been handed out, so the next one is past it.
        let next = if call.is_called { value + 1 } else { value };
        let mut txn = self.backend.begin()?;
        crate::catalog::set_sequence_value(
            &mut *txn,
            self.tenant,
            sequence_id,
            u64::try_from(next).unwrap_or(u64::MAX),
            call.is_called,
        );
        txn.commit()?;
        // `currval` answers the value that was set, whether or not it was called -- measured.
        self.sequences.insert(sequence_id, (value + 1, value + 1));
        self.last_sequence = Some(sequence_id);
        self.currval_defined.insert(sequence_id);
        Ok(value)
    }

    /// Takes each sequence read's value in a transaction of its own.
    ///
    /// **Not the statement's**, and that is the whole of it: `nextval` and `setval` commit outside
    /// the block that calls them, so a sequence read through the block's snapshot answers the value
    /// as of `BEGIN`. A real server reads the sequence itself, which is what this does.
    fn fill_sequence_reads(&mut self, node: &mut crate::plan::Node) -> Result<()> {
        let mut ids = Vec::new();
        collect_sequence_reads(node, &mut ids);
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.backend.begin()?;
        let mut states = std::collections::BTreeMap::new();
        for id in ids {
            states.insert(id, crate::catalog::sequence_state(&*txn, self.tenant, id)?);
        }
        let _ = txn.rollback();
        fill_sequence_reads_in(node, &states);
        Ok(())
    }

    /// Resolves the tables a `SELECT` names and plans against them.
    fn plan_select(&self, txn: &dyn Txn, select: &crate::plan::Select) -> Result<query::Planned> {
        // Every subquery is planned **before** the statement holding it, because the statement
        // cannot be typed until they are: `WHERE n IN (SELECT a_id FROM b)` is `42883 operator
        // does not exist: text = bigint`, and nothing can say so without knowing the subquery's
        // column type. `Cow`-shaped by hand the way `resolve_sequence_calls` is — a statement with
        // no subquery in it is planned from the caller's own `Select` and nothing is cloned.
        let mut owned;
        let select = if subquery::present(select) {
            owned = select.clone();
            subquery::plan_subqueries(
                &mut owned,
                self.tenant,
                txn,
                &Catalogued { exec: self, txn },
                None,
            )?;
            &owned
        } else {
            select
        };
        let catalogued = Catalogued { exec: self, txn };
        let table = match &select.from {
            // A derived table stands for a relation nothing stores; `relation_of` hands back the
            // synthetic one its sub-select's target list makes, which everything above reads like
            // any other table.
            Some(table) => Some(subquery::relation_of(table, &catalogued)?),
            None => None,
        };
        let inners = select
            .joins
            .iter()
            .map(|join| subquery::relation_of(&join.table, &catalogued))
            .collect::<Result<Vec<_>>>()?;
        let inner_refs: Vec<&crate::catalog::TableDef> = inners.iter().map(AsRef::as_ref).collect();
        let mut planned = query::plan(select, self.tenant, table.as_deref(), &inner_refs)?;
        // **After the row plan, never instead of it.** Routing is a rewrite of a plan that already
        // exists and is already correct, which is what lets a refusal be answered by putting the
        // original back (`crate::exec::fragment`). A join has no outer table to route and is left
        // alone by `consider` in any case.
        // A derived table is never routed: nothing stores the relation, so there is no columnar
        // copy of it, and its id is a reserved one no catalog record names.
        let derived_from = select
            .from
            .as_ref()
            .is_some_and(|from| from.derived.is_some());
        if let Some(table) = table.as_deref()
            && inners.is_empty()
            && !derived_from
        {
            fragment::route(
                txn,
                self.tenant,
                table,
                self.fragments.as_deref(),
                self.engine(),
                &mut planned,
            );
        }
        Ok(planned)
    }

    /// `EXPLAIN`: the plan, as rows, and nothing run.
    fn explain(&self, txn: &dyn Txn, statement: &Statement, analyze: bool) -> Result<Outcome> {
        // A `SELECT`'s plan is the whole point of `EXPLAIN`, and building it needs the catalog.
        let lines = match statement {
            Statement::Select(select) => {
                let mut planned = self.plan_select(txn, select)?;
                if analyze {
                    // **`ANALYZE` runs it**, which is what makes the numbers real. The fragments
                    // go out and the plan is drained: what a routed query costs is a fact about a
                    // run, and a plan that only described one would be reporting an estimate this
                    // node does not have.
                    if let Some(source) = self.fragments.clone() {
                        fragment::resolve(&mut planned.node, &*source, txn.start_ts());
                    }
                    subquery::resolve(&mut planned.node, txn, self.tenant)?;
                    let mut cursor = cursor::Cursor::open(txn, self.tenant, &planned.node)?;
                    while cursor.next()?.is_some() {}
                }
                planned.node.explain(
                    &planned.table,
                    &planned.column_names,
                    planned.engine.as_ref(),
                )
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
        // **Only a key this statement *added* can be a duplicate.** One it removed and put back is
        // an entry the row already owned, so a race on it is the row-level conflict it looks like
        // (`Written::rewritten`, which carries the argument and the wrong answer it fixes).
        let added = |unique: &Unique| !written.rewritten.contains(&unique.key);

        if let Some(lost) = key {
            return match written
                .unique_keys
                .iter()
                .filter(|it| added(it))
                .find(|it| &it.key == lost)
            {
                Some(unique) => SqlError::UniqueViolation {
                    constraint: unique.constraint.clone(),
                    key: Some(unique.detail.clone()),
                },
                // A key this transaction wrote that is not one of its *new* unique index entries:
                // an ordinary row-level race, and still retryable.
                None => error,
            };
        }

        // No key named. Open a transaction and look: the entries that are now present are the
        // ones this transaction collided with, and the first of those names the constraint.
        let Ok(txn) = self.backend.begin() else {
            return error;
        };
        for unique in written.unique_keys.iter().filter(|it| added(it)) {
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

    /// The next value of one sequence.
    ///
    /// The same shape as [`Executor::next_row_id`] and the same trade, for a counter the *user*
    /// can see: a batch is reserved in a transaction of its own and handed out from memory. That
    /// separate transaction is what makes `nextval` **non-transactional**, which is not a
    /// side-effect but the semantics — a rolled-back `INSERT` has still consumed its value, here
    /// and on a real server, measured on both.
    ///
    /// The gaps it leaves are wider than PostgreSQL's default, and that is the declared
    /// divergence on [`crate::catalog::SEQUENCE_BATCH`]: `CACHE n` is a sequence option PostgreSQL
    /// has with exactly this behaviour, and neither server offers gap-freeness.
    fn next_sequence_value(&mut self, sequence_id: u64) -> Result<i64> {
        if let Some((next, end)) = self.sequences.get_mut(&sequence_id)
            && *next < *end
        {
            let value = *next;
            *next += 1;
            self.last_sequence = Some(sequence_id);
            self.currval_defined.insert(sequence_id);
            return Ok(value);
        }

        let mut txn = self.backend.begin()?;
        let first = match crate::catalog::allocate_sequence_values(
            &mut *txn,
            self.tenant,
            sequence_id,
            crate::catalog::SEQUENCE_BATCH,
        ) {
            Ok(first) => first,
            Err(error) => {
                let _ = txn.rollback();
                return Err(error);
            }
        };
        txn.commit()?;
        let batch = i64::try_from(crate::catalog::SEQUENCE_BATCH).unwrap_or(i64::MAX);
        self.sequences
            .insert(sequence_id, (first + 1, first.saturating_add(batch)));
        self.last_sequence = Some(sequence_id);
        self.currval_defined.insert(sequence_id);
        Ok(first)
    }

    /// Adds a notice for the session to send before this statement's `CommandComplete`.
    fn notice(&self, notice: SqlError) {
        self.notices.borrow_mut().push(notice);
    }

    /// Reads every `$n` in a statement as the type its context gives it, and turns every
    /// `'name'::regclass` into the oid it names — leaving a statement with neither left in it.
    fn bound(
        &self,
        txn: &dyn Txn,
        mut statement: Statement,
        params: &Params<'_>,
    ) -> Result<Statement> {
        if !params.values.is_empty() || bind::has_parameters(&statement) {
            let tables = self.tables_for(txn, &statement)?;
            bind::refuse_unmatched_parameters(&statement, params)?;
            let types = bind::infer(&statement, &tables, params.declared);
            bind::substitute(&mut statement, params, &types)?;
        }
        self.resolve_current_schema(txn, &mut statement)?;
        self.resolve_current_database(&mut statement);
        self.resolve_current_setting(&mut statement)?;
        self.resolve_advisory(&mut statement)?;
        self.resolve_regclass(txn, &mut statement)?;
        self.refuse_unavailable_functions(txn, &statement)?;
        Ok(statement)
    }

    /// Joins this node's advisory-lock table instead of the private one `new` made.
    ///
    /// The session factory calls it once per connection (`bin/esker-sql.rs`), which is what makes
    /// two sessions on one node able to block each other — the whole point of the feature, and the
    /// thing a private table per executor cannot do. Written as a builder because that is how the
    /// other three node-wide handles reach an executor.
    #[must_use]
    pub fn sharing_advisory_locks(mut self, locks: Arc<crate::advisory::Locks>) -> Self {
        self.session = locks.session();
        self.locks = locks;
        self
    }

    /// Folds every `current_database()` to the database this session is connected to.
    ///
    /// **Not a constant of the server.** It was one while there was a single database to fold to,
    /// and a constant would now report the default database's name to a client connected to
    /// another — a wrong answer rather than a missing feature, which is the class ADR 0031 ranks
    /// worst. `pg_database.datname` carries the same string, so `WHERE datname =
    /// current_database()` still matches by construction.
    fn resolve_current_database(&self, statement: &mut Statement) {
        use crate::plan::{Expr, Literal};

        if !bind::any(statement, |expr| matches!(expr, Expr::CurrentDatabase)) {
            return;
        }
        let mut resolve = |expr: &mut Expr| {
            if matches!(expr, Expr::CurrentDatabase) {
                *expr = Expr::Literal(Literal::Typed(Box::new(Datum::Text(self.database.clone()))));
            }
        };
        bind::walk_mut(statement, &mut resolve);
    }

    /// Takes or releases every advisory lock the statement names, and folds each call to the
    /// boolean it answered.
    ///
    /// **This is where "once per statement" is decided, and it is the whole reason a non-constant
    /// argument is refused.** A real server evaluates `pg_try_advisory_lock(id)` once per row and
    /// would take a lock per row; folding here would take one. Rather than silently differ, an
    /// argument that is not a constant by this point is `0A000` naming the function — and by this
    /// point a `$1` has already been substituted, so every shape `ActiveRecord` sends
    /// (`postgresql_adapter.rb:474` interpolates the id into the text) is a constant.
    ///
    /// The lock **outlives the statement and the transaction**: it is released by an explicit
    /// unlock or when the session ends, measured, which is why nothing here is staged in the
    /// transaction's write set.
    fn resolve_advisory(&self, statement: &mut Statement) -> Result<()> {
        use crate::plan::{Expr, Literal};

        if !bind::any(statement, |expr| matches!(expr, Expr::Advisory { .. })) {
            return Ok(());
        }
        let mut failed = None;
        let mut resolve = |expr: &mut Expr| {
            let Expr::Advisory { call, args } = expr else {
                return;
            };
            let (call, args) = (*call, std::mem::take(args));
            let answered = match advisory_key(call, &args) {
                Ok(key) => {
                    if call.takes() {
                        self.locks.try_lock(self.session, key, call.mode())
                    } else {
                        let released = self.locks.unlock(self.session, key, call.mode());
                        if !released {
                            // PostgreSQL's own sentence, and a `WARNING` rather than an error: the
                            // caller gets `false` and carries on. `ActiveRecord` reads exactly this
                            // to decide a migration lock was never held.
                            self.notice(SqlError::LockNotHeld(call.mode().name()));
                        }
                        released
                    }
                }
                Err(error) => {
                    failed.get_or_insert(error);
                    false
                }
            };
            *expr = Expr::Literal(Literal::Bool(answered));
        };
        bind::walk_mut(statement, &mut resolve);
        failed.map_or(Ok(()), Err)
    }

    /// Folds every `current_setting(…)` to the value this session reports.
    ///
    /// Once per statement, where [`Executor::resolve_current_schema`] does its own and for the same
    /// reason: the value is the session's and the row evaluator has no handle on one. A parameter
    /// cannot change in the middle of a statement, so folding it once is exact rather than
    /// approximate.
    ///
    /// **`42704` here and not at lowering**, which is where a real server raises it too — and the
    /// two-argument form answers NULL instead, which is the documented escape hatch and the one
    /// shape that must not error.
    fn resolve_current_setting(&self, statement: &mut Statement) -> Result<()> {
        use crate::plan::{Expr, Literal};

        if !bind::any(statement, |expr| {
            matches!(expr, Expr::CurrentSetting { .. })
        }) {
            return Ok(());
        }
        let mut failed = None;
        let mut resolve = |expr: &mut Expr| {
            let Expr::CurrentSetting { name, missing_ok } = expr else {
                return;
            };
            *expr = match crate::parameter::lookup(name) {
                Ok(parameter) => Expr::Literal(Literal::String(self.parameter(parameter))),
                Err(_) if *missing_ok => Expr::Literal(Literal::Null),
                Err(error) => {
                    failed.get_or_insert(error);
                    Expr::Literal(Literal::Null)
                }
            };
        };
        bind::walk_mut(statement, &mut resolve);
        match failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The `search_path` **as resolved**: the entries that name a schema this tenant has, in the
    /// order written, without repeats.
    ///
    /// `SHOW search_path` gives the path as *set* and this gives it as resolved, and they are two
    /// different answers to the same question — both measured. An entry naming no schema is
    /// **dropped**, not refused, which is what makes the default `"$user", public` resolve to
    /// `{public}` on a server that has no schema named for the role.
    pub(crate) fn resolved_search_path(&self, txn: &dyn Txn) -> Result<Vec<String>> {
        let written = self.parameter(crate::parameter::search_path());
        let mut out: Vec<String> = Vec::new();
        for entry in written.split(',') {
            let entry = entry.trim().trim_matches('"');
            // `$user` names a schema after the connected role, and there are no roles here — so it
            // resolves to nothing and is dropped, exactly as a missing schema is.
            if entry.is_empty() || entry == "$user" || out.iter().any(|held| held == entry) {
                continue;
            }
            if crate::catalog::schema_exists(txn, self.tenant, entry)? {
                out.push(entry.to_owned());
            }
        }
        Ok(out)
    }

    /// The stored name an **unqualified** relation name resolves to.
    ///
    /// Each schema on the path in order, and the first that has it wins: with `sp_b, sp_a` a bare
    /// `t` is `sp_b`'s and with `sp_a, sp_b` it is `sp_a`'s — the same query, two answers,
    /// measured. A name that is found nowhere comes back **unchanged**, so the `42P01` quotes the
    /// bare name the user wrote rather than a schema they did not.
    pub(crate) fn resolve_unqualified(&self, txn: &dyn Txn, name: &str) -> Result<String> {
        if name.contains(crate::catalog::SCHEMA_SEPARATOR) {
            return Ok(name.to_owned());
        }
        let view = self.catalog_view(txn)?;
        for schema in self.resolved_search_path(txn)? {
            let candidate = crate::catalog::qualify(&schema, name);
            if view.relation(&candidate)?.is_some() {
                return Ok(candidate);
            }
        }
        Ok(name.to_owned())
    }

    /// The schema a `CREATE` with no qualifier puts its relation in: the **first** entry of the
    /// path that resolves, and `public` when none does.
    ///
    /// Measured: with `sp_a, sp_b` a `CREATE TABLE made_here` lands in `sp_a`, and with
    /// `nosuchschema, sp_b` it still succeeds — the first entry that *resolves* is what it uses.
    pub(crate) fn creation_schema(&self, txn: &dyn Txn) -> Result<String> {
        Ok(self
            .resolved_search_path(txn)?
            .into_iter()
            .next()
            .unwrap_or_else(|| crate::catalog::PUBLIC_SCHEMA.to_owned()))
    }

    /// Replaces every `current_schema()` and `current_schemas(…)` with the session's own.
    ///
    /// **Once per statement**, here rather than in the row evaluator, for the reason `::regclass`
    /// is: the value is the session's and a row has no session. It was folded to `public` where the
    /// statement is lowered while `public` was the only schema there was.
    fn resolve_current_schema(&self, txn: &dyn Txn, statement: &mut Statement) -> Result<()> {
        use crate::plan::{Expr, Literal};

        if !bind::any(statement, |expr| matches!(expr, Expr::CurrentSchema { .. })) {
            return Ok(());
        }
        let path = self.resolved_search_path(txn)?;
        let mut resolve = |expr: &mut Expr| {
            let Expr::CurrentSchema { all } = expr else {
                return;
            };
            *expr = match all {
                // **NULL when nothing resolves**, not `public` and not an error: measured, `SET
                // search_path TO nosuchschema` makes `current_schema()` NULL.
                None => match path.first() {
                    None => Expr::Literal(Literal::Null),
                    Some(first) => Expr::Literal(Literal::String(first.clone())),
                },
                // `current_schemas(true)` prepends `pg_catalog`, and only that one — it is the
                // *implicit* schema the argument names.
                Some(implicit) => {
                    let mut all = Vec::new();
                    if *implicit {
                        all.push(Some("pg_catalog".to_owned()));
                    }
                    all.extend(path.iter().map(|name| Some(name.clone())));
                    Expr::Literal(Literal::Typed(Box::new(Datum::Text(
                        crate::value::vector::Array::write(&all),
                    ))))
                }
            };
        };
        bind::walk_mut(statement, &mut resolve);
        Ok(())
    }

    /// Replaces every `'name'::regclass` with the oid that name has.
    ///
    /// **Once per statement**, here rather than in the row evaluator, for the reason a sequence
    /// call is resolved before the plan: `WHERE a.attrelid = 'companies'::regclass` is one lookup
    /// and the evaluator would make it one lookup per row it filtered — over a catalog view, one
    /// per column of the whole catalog.
    ///
    /// A name nothing answers to is `42P01`, which is what a real server says and is the answer
    /// `ActiveRecord` relies on to tell a missing table from an empty one.
    /// Refuses a UUID function whose extension is not installed.
    ///
    /// **Checked here rather than at lowering**, because availability is transaction state: the
    /// same statement is `42883` before a `CREATE EXTENSION "uuid-ossp"` and a value after it, and
    /// is `42883` again the moment that install rolls back. Measured on PostgreSQL 19, all three.
    ///
    /// `gen_random_uuid` is in core and needs no check; only `uuid_generate_v4` has an extension
    /// behind it.
    fn refuse_unavailable_functions(&self, txn: &dyn Txn, statement: &Statement) -> Result<()> {
        use crate::plan::Expr;

        let mut missing = None;
        bind::for_each_expr(statement, &mut |expr: &Expr| {
            if missing.is_some() {
                return;
            }
            let Expr::Uuid(func) = expr else { return };
            let Some(extension) = func.requires() else {
                return;
            };
            match crate::catalog::pg_catalog::is_installed(txn, self.tenant, extension) {
                Ok(true) => {}
                // The **name** form of the DETAIL, not the argument-types one: there is no
                // function of that name at any arity until the extension is there. Measured.
                Ok(false) => {
                    missing = Some(SqlError::UndefinedFunctionName(format!(
                        "{}()",
                        func.name()
                    )));
                }
                Err(error) => missing = Some(error),
            }
        });
        match missing {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn resolve_regclass(&self, txn: &dyn Txn, statement: &mut Statement) -> Result<()> {
        use crate::plan::{CatalogFunc, Expr, Literal};

        let mut failure = None;
        // One catalog snapshot for the whole statement; see `relation_oid`.
        let mut relations = None;
        let mut resolve = |expr: &mut Expr| {
            let Expr::CatalogFunc(call) = expr else {
                return;
            };
            if call.func != CatalogFunc::RegClass {
                return;
            }
            let Some(Expr::Literal(Literal::String(name))) = call.args.first() else {
                failure.get_or_insert(SqlError::Internal(
                    "a ::regclass whose argument is not a name".to_owned(),
                ));
                return;
            };
            match self.relation_oid(&mut relations, txn, name) {
                Ok(oid) => *expr = Expr::Literal(Literal::Typed(Box::new(Datum::Int8(oid)))),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        };
        bind::walk_mut(statement, &mut resolve);
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The oid of a relation by name, or `42P01`.
    ///
    /// A `pg_catalog` view answers with its own reserved id, which is what makes
    /// `'pg_class'::regclass` a number rather than a refusal — a real server answers there too, and
    /// this node's `pg_class` really does hold `pg_class`'s columns.
    fn relation_oid(
        &self,
        relations: &mut Option<crate::catalog::pg_relations::Relations>,
        txn: &dyn Txn,
        name: &str,
    ) -> Result<i64> {
        if let Some(view) = crate::catalog::pg_catalog::view(name) {
            return Ok(i64::try_from(view.table_def().id).unwrap_or(i64::MAX));
        }
        // **`::regclass` takes a name as a *string***, so a schema in it is a dot rather than the
        // separator the parser would have produced — `'se_idx.t_i_idx'::regclass` is the index in
        // `se_idx`, and looking it up whole would find nothing.
        let stored = crate::catalog::parse_qualified(name);
        // **Read once per statement, not once per literal.** The catalog scan is one pass over the
        // name records plus a point read per relation, so a statement with three `::regclass` casts
        // was three of those — and `ActiveRecord`'s schema dump writes several per statement
        // against a catalog with hundreds of relations. It is a *snapshot*: every literal in one
        // statement resolves against the same catalog, which is what a real server does too, and
        // the statement has not written anything at this point because nothing has run yet.
        let relations = match relations {
            Some(relations) => relations,
            slot => slot.insert(crate::catalog::pg_relations::Relations::read(
                txn,
                self.tenant,
            )?),
        };
        relations
            .by_name(&stored)
            .map(|relation| relation.oid)
            // **The message spells the name the caller wrote**, normalised: the schema goes
            // *inside* the quotes, and a `public.` that the stored form drops is still printed —
            // `relation "public.nosuch_seq" does not exist`.
            .ok_or_else(|| SqlError::UndefinedTable(crate::catalog::written_display(name)))
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
    ///
    /// **An unqualified name is looked for along the `search_path`**, in order, and the first
    /// schema that has it wins ([`Executor::resolve_unqualified`]). A name found nowhere keeps the
    /// spelling the user wrote, so the `42P01` quotes that rather than a schema they did not name.
    fn require_table(&self, txn: &dyn Txn, name: &str) -> Result<Arc<crate::catalog::TableDef>> {
        let resolved = self.resolve_unqualified(txn, name)?;
        self.catalog_view(txn)?.require_table(&resolved)
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

/// The lock a call names, or `0A000` for an argument that is not a constant by binding time.
///
/// Both arities, and the two are **different key spaces** rather than two spellings of one — the
/// capture shows `pg_try_advisory_lock(42)` and `pg_try_advisory_lock(42, 7)` held at once, told
/// apart by `pg_locks.objsubid`.
fn advisory_key(
    call: crate::plan::AdvisoryCall,
    args: &[crate::plan::Expr],
) -> Result<crate::advisory::Key> {
    use crate::plan::{Expr, Literal};

    let integer = |expr: &Expr| -> Option<i64> {
        match expr {
            Expr::Literal(Literal::Integer(value)) => Some(*value),
            Expr::Literal(Literal::Typed(datum)) => match **datum {
                Datum::Int8(value) => Some(value),
                Datum::Int4(value) => Some(i64::from(value)),
                _ => None,
            },
            _ => None,
        }
    };
    let constant = |expr: &Expr| {
        integer(expr).ok_or_else(|| {
            SqlError::unsupported(format!(
                "{}() over a value that is not a constant, which a real server would evaluate \
                 once per row",
                call.name()
            ))
        })
    };
    match args {
        [whole] => Ok(crate::advisory::Key::whole(constant(whole)?)),
        [high, low] => {
            let narrow = |value: i64| {
                i32::try_from(value).map_err(|_| SqlError::IntegerOutOfRange {
                    value: value.to_string(),
                    ty: "integer",
                })
            };
            Ok(crate::advisory::Key::pair(
                narrow(constant(high)?)?,
                narrow(constant(low)?)?,
            ))
        }
        _ => Err(SqlError::UndefinedFunction(format!("{}()", call.name()))),
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
    /// Keys this statement **removed before putting them back** — the entries a rewritten row
    /// already owned.
    ///
    /// **A key a row already had cannot be a duplicate of itself.** An `UPDATE` deletes the old
    /// row's entries and writes the new row's, so an `UPDATE` that leaves a unique column alone
    /// re-puts the same key and records it in `unique_keys` exactly as an `INSERT` of a new key
    /// would. Without this list, two sessions updating one row's *unrelated* column ended with
    /// the loser told `23505 duplicate key value violates unique constraint "t_pkey"` about a
    /// primary key neither of them changed — a wrong answer, not a divergence: PostgreSQL never
    /// says that, and `ActiveRecord` maps it to `RecordNotUnique` where the truth is a retryable
    /// serialization failure.
    ///
    /// A key the statement **moved** — an `UPDATE` that sets a unique column to a value another
    /// row holds — is not in here, so it is still the `23505` it should be.
    pub(crate) rewritten: Vec<Vec<u8>>,
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

/// A `SELECT` that either was left alone or had its sequence calls run.
///
/// `std::borrow::Cow` would need `Select: ToOwned`, which it has by `Clone` but which reads worse
/// here than saying the two cases out loud.
enum SelectRef<'a> {
    Borrowed(&'a crate::plan::Select),
    Owned(Box<crate::plan::Select>),
}

impl SelectRef<'_> {
    fn as_ref(&self) -> &crate::plan::Select {
        match self {
            SelectRef::Borrowed(select) => select,
            SelectRef::Owned(select) => select,
        }
    }
}

/// Whether an expression contains a sequence function anywhere in it.
fn has_sequence_call(expr: &crate::plan::Expr) -> bool {
    let mut found = false;
    bind::descend(expr, &mut |expr| {
        found |= matches!(expr, crate::plan::Expr::Sequence(_));
    });
    found
}

/// Every sequence a plan reads, so their values can be taken in one transaction.
fn collect_sequence_reads(node: &mut crate::plan::Node, into: &mut Vec<u64>) {
    if let crate::plan::Node::SequenceRead { sequence_id, .. } = node {
        into.push(*sequence_id);
    }
    for child in node.children_mut() {
        collect_sequence_reads(child, into);
    }
}

/// Puts those values into the plan.
fn fill_sequence_reads_in(
    node: &mut crate::plan::Node,
    states: &std::collections::BTreeMap<u64, (i64, bool)>,
) {
    if let crate::plan::Node::SequenceRead {
        sequence_id, state, ..
    } = node
    {
        *state = states.get(sequence_id).copied();
    }
    for child in node.children_mut() {
        fill_sequence_reads_in(child, states);
    }
}

/// The `EXPLAIN` output for a statement. One line per plan node, indented by depth, which is the
/// shape `psql` renders and users read.
fn explain_lines(statement: &Statement) -> Vec<String> {
    match statement {
        Statement::CreateTable(create) => vec![format!("Create Table on {}", create.name)],
        Statement::CreateExtension(create) => {
            vec![format!("Create Extension on {}", create.name)]
        }
        Statement::DropExtension(drop) => {
            vec![format!("Drop Extension on {}", drop.name)]
        }
        Statement::CreateSchema(create) => vec![format!("Create Schema on {}", create.name)],
        Statement::CreateDatabase(create) => vec![format!("Create Database on {}", create.name)],
        Statement::DropDatabase(drop) => {
            vec![format!("Drop Database on {}", drop.names.join(", "))]
        }
        Statement::DropSchema(drop) => vec![format!("Drop Schema on {}", drop.names.join(", "))],
        Statement::AlterSchemaRename(rename) => {
            vec![format!("Alter Schema on {}", rename.name)]
        }
        Statement::DropTable(drop) => vec![format!("Drop Table on {}", drop.names.join(", "))],
        Statement::DropSequence(drop) => {
            vec![format!("Drop Sequence on {}", drop.names.join(", "))]
        }
        Statement::CreateSequence(create) => {
            vec![format!("Create Sequence on {}", create.name)]
        }
        Statement::CreateFunction(create) => {
            vec![format!("Create Function on {}", create.name)]
        }
        Statement::CreateTrigger(create) => {
            vec![format!("Create Trigger on {}", create.table)]
        }
        Statement::DropTrigger(drop) => vec![format!("Drop Trigger on {}", drop.table)],
        Statement::DropFunction(drop) => {
            let names: Vec<&str> = drop
                .functions
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            vec![format!("Drop Function on {}", names.join(", "))]
        }
        Statement::CreateIndex(create) => vec![format!("Create Index on {}", create.table)],
        Statement::DropIndex(drop) => vec![format!("Drop Index on {}", drop.names.join(", "))],
        Statement::Comment(statement) => vec![format!("Comment on {}", statement.name)],
        Statement::CreateType(create) => vec![format!("Create Type on {}", create.name)],
        Statement::DropType(drop) => vec![format!("Drop Type on {}", drop.names.join(", "))],
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
        Statement::Explain(..) => vec!["Explain".to_owned()],
        // `EXPLAIN SET ...` is not PostgreSQL's grammar either, and a session statement has no
        // plan to print: it touches no table and reads no row.
        Statement::Session(session) => vec![session.tag().to_owned()],
        // A checkpoint verb has no access path to choose: it is one key, by name.
        Statement::TimeMachine(_) => vec!["Time Machine".to_owned()],
    }
}
/// The columns a write statement's `RETURNING` will describe, or `None` when it has none.
///
/// `tables` is what `describe` already resolved, so this neither opens a transaction nor reads the
/// catalog again; a statement whose table is gone has already failed by the time it gets here.
fn returning_fields(
    returning: Option<&crate::plan::Returning>,
    tables: &[Arc<crate::catalog::TableDef>],
) -> Result<Option<Vec<FieldDescription>>> {
    let (Some(items), Some(table)) = (returning, tables.first()) else {
        return Ok(None);
    };
    let (columns, _) = query::returning_columns(items, table)?;
    Ok(Some(described(columns)))
}

/// The same for an `UPDATE`, whose `RETURNING` may name a `FROM` relation as readily as the row
/// being written — `RETURNING a.id, a.body, p.title`, measured — and whose target may be under an
/// alias that took its name away.
///
/// The `FROM` relations are resolved here rather than read out of `tables` positionally: a name
/// the catalog does not have is dropped from that list, and a scope built by position off a list
/// with a hole in it describes the wrong columns rather than failing.
fn update_returning_fields(
    update: &crate::plan::Update,
    tables: &[Arc<crate::catalog::TableDef>],
    catalogued: &Catalogued<'_>,
) -> Result<Option<Vec<FieldDescription>>> {
    let (Some(items), Some(target)) = (update.returning.as_ref(), tables.first()) else {
        return Ok(None);
    };
    let chain = update.chain();
    let sources = chain
        .iter()
        .map(|join| subquery::relation_of(&join.table, catalogued))
        .collect::<Result<Vec<_>>>()?;
    let name = dml::target_name(update, target);
    let entries = dml::scope_entries(target, &name, &chain, &sources);
    let from = crate::plan::TableRef {
        alias: update.alias.clone(),
        ..crate::plan::TableRef::bare(target.name.clone())
    };
    let (columns, _) =
        query::returning_columns_over(items, Some(from), &chain, &query::Scope::chain(&entries))?;
    Ok(Some(described(columns)))
}

/// A resolved target list as the wire describes it.
fn described(columns: Vec<query::OutputColumn>) -> Vec<FieldDescription> {
    columns
        .into_iter()
        .map(|column| FieldDescription::of(column.name, column.ty, column.typmod))
        .collect()
}

impl Execute for Executor {
    /// Everything this session holds, released — what the end of a connection owes the node.
    ///
    /// A session's locks die with it on a real server and nothing else releases them: they survive
    /// `ROLLBACK`, measured. So this is not tidying, it is the other half of the lifetime.
    fn release_advisory_locks(&self) {
        self.locks.unlock_all(self.session);
    }

    /// What this session set, or the boot value — which is `0`, meaning no limit.
    ///
    /// Read through `Executor::parameter` — a private method, so this is a code span rather than
    /// a link — instead of from the map directly, so that a session
    /// that never set it gets the same answer as one that reset it.
    fn idle_in_transaction_timeout(&self) -> Option<std::time::Duration> {
        let parameter = crate::parameter::lookup("idle_in_transaction_session_timeout").ok()?;
        crate::parameter::duration_ms(&self.parameter(parameter))
            .map(std::time::Duration::from_millis)
    }

    fn execute(&mut self, parsed: &Parsed, params: &Params<'_>) -> Result<Outcome> {
        // **Before lowering**, because the statement the parser was given is a placeholder: what
        // the user wrote is on the class (`crate::parse::StatementClass::SetConstraints`).
        if let crate::parse::StatementClass::SetConstraints { names, deferred } = parsed.class() {
            return self.set_constraints_statement(names, *deferred);
        }
        let statement = parsed.lower()?;
        if let Statement::Session(session) = &statement {
            return self.session_statement(session);
        }
        // PostgreSQL's `25001`, captured: a concurrent change is *many* transactions, so it cannot
        // be part of one, and a block that could roll it back would be a block that could roll back
        // half a schema change.
        //
        // Checked **here** rather than beside the write gate, and the reason is a trap worth
        // naming: `in_a_transaction` *takes* the open transaction out of `self` before it runs the
        // statement, so a check for "am I in a block" further down always reads `None`.
        if self.open.is_some()
            && let Some(named) = statement.refused_in_a_transaction_block()
        {
            return Err(SqlError::NotInATransactionBlock(named));
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
        // **A derived table has no shape until its sub-select is planned**, and the shape is the
        // whole of what a `Describe` answers. `Executor::plan_select` does this before it plans;
        // here it was skipped, so `FROM (SELECT …) AS x` reached the planner as a relation with
        // nothing behind it and came back `XX000 a derived table reached the planner without a
        // shape` — a code that says *this server has a bug* about a statement it runs perfectly
        // well through the simple protocol.
        if let Statement::Select(select) = &mut statement
            && subquery::present(select)
        {
            let catalogued = Catalogued {
                exec: self,
                txn: &*txn,
            };
            subquery::plan_subqueries(select, self.tenant, &*txn, &catalogued, None)?;
        }
        let fields = match &statement {
            // **The relations are resolved here, not read out of `tables` by position.**
            // `tables` comes from `bind::table_names`, which is built for parameter *typing*: a
            // name appearing twice is one name to type against and a name the catalog does not
            // have is nothing to type against, so that list de-duplicates and it drops. Both are
            // right for typing and wrong for a position — a **self-join**'s two entries collapsed
            // into one and left this planning a one-join `SELECT` with nothing to join to, which
            // is `42P01` for a table that is right there. Seventeen of run 51's forty-seven
            // `relation "…" does not exist` were `topics` alone, because `Reply < Topic` makes
            // `Topic.joins(:replies)` a self-join.
            //
            // So it resolves the way `Executor::plan_select` does, off the statement's own `FROM`
            // and joins, which is also the only reading that a derived table cannot shift.
            Statement::Select(select) => {
                let catalogued = Catalogued {
                    exec: self,
                    txn: &*txn,
                };
                let from = match &select.from {
                    Some(table) => Some(subquery::relation_of(table, &catalogued)?),
                    None => None,
                };
                let inners = select
                    .joins
                    .iter()
                    .map(|join| subquery::relation_of(&join.table, &catalogued))
                    .collect::<Result<Vec<_>>>()?;
                let inner_refs: Vec<&crate::catalog::TableDef> =
                    inners.iter().map(AsRef::as_ref).collect();
                Some(
                    query::plan(select, self.tenant, from.as_deref(), &inner_refs)?
                        .columns
                        .into_iter()
                        .map(|column| FieldDescription::of(column.name, column.ty, column.typmod))
                        .collect(),
                )
            }
            Statement::Explain(..) => Some(vec![FieldDescription::computed(
                "QUERY PLAN",
                ColumnType::Text,
            )]),
            // A `RETURNING` makes a write statement row-returning, and a client that prepares one
            // asks for its shape before it binds. Answering `None` here would tell the client
            // there are no columns and then send it some, which is the one thing a `Describe` is
            // for.
            Statement::Insert(insert) => returning_fields(insert.returning.as_ref(), &tables)?,
            Statement::Update(update) => update_returning_fields(
                update,
                &tables,
                &Catalogued {
                    exec: self,
                    txn: &*txn,
                },
            )?,
            Statement::Delete(delete) => returning_fields(delete.returning.as_ref(), &tables)?,
            _ => None,
        };
        let _ = txn.rollback();
        Ok(Described { parameters, fields })
    }

    fn take_notices(&mut self) -> Vec<SqlError> {
        // Filtered here rather than where each notice is raised, because this is the one place
        // every notice this node produces passes through — and a suppressed one must still not be
        // left in the queue for the next statement to emit.
        std::mem::take(self.notices.get_mut())
            .into_iter()
            .filter(|notice| self.reports(notice.severity()))
            .collect()
    }

    fn begin(&mut self, read_only: bool) -> Result<()> {
        // A second `BEGIN` never reaches here: the session answers it with PostgreSQL's warning
        // and leaves the block alone.
        self.savepoints.clear();
        self.block_parameters = Some(self.parameters.clone());
        self.block_read_only = read_only;
        self.open = Some(self.open_txn()?);
        self.open_used = false;
        self.written = Written::default();
        self.catalog_written = false;
        Ok(())
    }

    fn savepoint(&mut self, name: &str) -> Result<()> {
        self.savepoints.savepoint(name, self.parameters.clone());
        Ok(())
    }

    /// The undo runs against the **open transaction**, which is the only place the writes it is
    /// compensating for exist. A `ROLLBACK TO` with no block open never reaches here — the session
    /// answers `25P01` first — so a missing transaction is a bug rather than a user's mistake.
    fn rollback_to(&mut self, name: &str) -> Result<()> {
        let mut txn = self
            .open
            .take()
            .ok_or_else(|| SqlError::Internal("a ROLLBACK TO with no open block".to_owned()))?;
        let result = self.savepoints.rollback_to(name, &mut *txn);
        self.open = Some(txn);
        // The parameters go back with the writes: a `SET` inside the savepoint is undone too.
        // Only on success — a `3B001` rolled nothing back and must change nothing.
        let parameters = result?;
        self.parameters = parameters;
        Ok(())
    }

    fn release(&mut self, name: &str) -> Result<()> {
        self.savepoints.release(name)
    }

    fn commit(&mut self) -> Result<()> {
        // **Before anything else the commit does**, because a check that fails means the
        // transaction does not commit at all. A real server rolls it back and this does too: the
        // rows the block wrote are not there afterwards, measured.
        if let Err(error) = self.run_deferred_checks() {
            let _ = self.rollback();
            return Err(error);
        }
        self.savepoints.clear();
        let written = std::mem::take(&mut self.written);
        self.catalog_written = false;
        self.end_of_block();
        let Some(txn) = self.open.take() else {
            self.columnar_changed = false;
            return Ok(());
        };
        match txn.commit() {
            Ok(_) => {
                // The block's commit is where an `ALTER` inside one becomes visible, so it is
                // where the report belongs.
                self.report_columnar();
                Ok(())
            }
            // The same translation the autocommit path does. A block's commit is where a client
            // that wrote several rows finds out it lost, and it deserves the same answer.
            Err(error) => Err(self.explain_conflict(error, &written)),
        }
    }

    fn rollback(&mut self) -> Result<()> {
        // A check owed by a transaction that is not committing is a check nobody will ever run,
        // and `SET CONSTRAINTS` is undone with everything else the block did.
        self.constraints.borrow_mut().clear();
        // Before `end_of_block`, which drops the snapshot: a `SET` made inside the block goes back
        // with everything else the block did.
        if let Some(parameters) = self.block_parameters.take() {
            self.parameters = parameters;
        }
        self.savepoints.clear();
        self.written = Written::default();
        self.catalog_written = false;
        self.columnar_changed = false;
        self.end_of_block();
        let Some(txn) = self.open.take() else {
            return Ok(());
        };
        txn.rollback()
    }
}

/// The catalog as a subquery's planner sees it: one lookup, in this transaction.
///
/// A borrowed pair rather than a method on [`Executor`], because `crate::exec::subquery` is given
/// exactly what it needs and no way to start a statement of its own — a subquery names tables, and
/// that is the whole of its access to anything outside its own plan.
pub(super) struct Catalogued<'a> {
    pub(super) exec: &'a Executor,
    pub(super) txn: &'a dyn Txn,
}

impl subquery::Tables for Catalogued<'_> {
    fn get(&self, name: &str) -> Result<Arc<crate::catalog::TableDef>> {
        self.exec.require_table(self.txn, name)
    }
}
