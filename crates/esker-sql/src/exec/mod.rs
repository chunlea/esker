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
mod query;

use std::sync::Arc;

use crate::backend::{Backend, Txn};
use crate::catalog::Catalog;
use crate::error::{Result, SqlError};
use crate::parse::Parsed;
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::{Described, Execute, Outcome, Params};
use crate::plan::Statement;
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
        }
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

        let mut txn = self.backend.begin()?;
        let mut written = Written::default();
        let bound = match self.bound(&*txn, statement, params) {
            Ok(bound) => bound,
            Err(error) => {
                let _ = txn.rollback();
                return Err(error);
            }
        };
        match self.run_recording(&mut *txn, &bound, &mut written) {
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
        }
    }

    fn run_recording(
        &mut self,
        txn: &mut dyn Txn,
        statement: &Statement,
        written: &mut Written,
    ) -> Result<Outcome> {
        match statement {
            Statement::CreateTable(create) => ddl::create_table(self, txn, create),
            Statement::DropTable(drop) => ddl::drop_table(self, txn, drop),
            Statement::CreateIndex(create) => ddl::create_index(self, txn, create),
            Statement::DropIndex(drop) => ddl::drop_index(self, txn, drop),
            Statement::Insert(insert) => dml::insert(self, txn, insert, written),
            Statement::Select(select) => self.select(txn, select),
            Statement::Update(update) => dml::update(self, txn, update, written),
            Statement::Delete(delete) => dml::delete(self, txn, delete),
            Statement::Explain(inner) => self.explain(txn, inner),
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

    /// Resolves the table a `SELECT` names and plans against it.
    fn plan_select(&self, txn: &dyn Txn, select: &crate::plan::Select) -> Result<query::Planned> {
        let table = match &select.from {
            Some(name) => Some(self.require_table(txn, name)?),
            None => None,
        };
        query::plan(select, self.tenant, table.as_deref())
    }

    /// `EXPLAIN`: the plan, as rows, and nothing run.
    fn explain(&self, txn: &dyn Txn, statement: &Statement) -> Result<Outcome> {
        // A `SELECT`'s plan is the whole point of `EXPLAIN`, and building it needs the catalog.
        let lines = match statement {
            Statement::Select(select) => {
                let planned = self.plan_select(txn, select)?;
                planned.node.explain(&planned.table)
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
    /// index entry. See the module docs for why this needs a second look at the store.
    fn explain_conflict(&self, error: SqlError, written: &Written) -> SqlError {
        if !matches!(error, SqlError::SerializationFailure(_)) || written.unique_keys.is_empty() {
            return error;
        }
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
        // Nobody took any of them: an ordinary row-level race, and still retryable.
        error
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
        let table = self.table_for(txn, &statement)?;
        let types = bind::infer(&statement, table.as_deref(), params.declared);
        bind::substitute(&mut statement, params, &types)?;
        Ok(statement)
    }

    /// The table a statement is about, when it is about one that exists.
    fn table_for(
        &self,
        txn: &dyn Txn,
        statement: &Statement,
    ) -> Result<Option<Arc<crate::catalog::TableDef>>> {
        match bind::table_name(statement) {
            // A name that is not there is not this function's error to raise: the statement will
            // reach it and report it with the message that statement uses.
            Some(name) => Ok(self.catalog_view(txn)?.table(name)?),
            None => Ok(None),
        }
    }

    /// This transaction's view of the catalog, pinned to one version.
    fn catalog_view<'a>(&'a self, txn: &'a dyn Txn) -> Result<crate::catalog::View<'a>> {
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
    }
}

impl Execute for Executor {
    fn execute(&mut self, parsed: &Parsed, params: &Params<'_>) -> Result<Outcome> {
        let statement = parsed.lower()?;
        self.in_a_transaction(statement, params)
    }

    fn describe(&mut self, parsed: &Parsed, declared: &[u32]) -> Result<Described> {
        let statement = parsed.lower()?;
        // Describing takes a transaction of its own, because typing the parameters needs the
        // catalog and the catalog is data like any other. It writes nothing, so it costs a
        // snapshot and no conflict.
        let txn = self.backend.begin()?;
        let table = self.table_for(&*txn, &statement)?;
        let types = bind::infer(&statement, table.as_deref(), declared);
        let parameters = types.iter().copied().map(ColumnType::oid).collect();

        // Planning needs every expression to have a type, and a `$1` has none until now. Nothing
        // is run, so a placeholder of the right type is all the planner needs to answer the shape.
        let mut statement = statement;
        bind::substitute_placeholders(&mut statement, &types);
        let fields = match &statement {
            Statement::Select(select) => Some(
                query::plan(select, self.tenant, table.as_deref())?
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

    fn begin(&mut self) -> Result<()> {
        // A second `BEGIN` never reaches here: the session answers it with PostgreSQL's warning
        // and leaves the block alone.
        self.open = Some(self.backend.begin()?);
        self.written = Written::default();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let written = std::mem::take(&mut self.written);
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
        let Some(txn) = self.open.take() else {
            return Ok(());
        };
        txn.rollback()
    }
}
