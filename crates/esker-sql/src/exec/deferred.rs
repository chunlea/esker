//! Constraint checks a transaction owes: which ones, when they run, and what switches that.
//!
//! A constraint declared `DEFERRABLE INITIALLY DEFERRED` is not checked by the statement that
//! breaks it. It is checked at `COMMIT`, and that is not a delay for its own sake — it is what
//! lets a transaction break the constraint in the middle and repair it before the end. Measured:
//! inserting a duplicate and then deleting the original **commits**, where the same pair of
//! statements against an immediate constraint cannot even be written.
//!
//! # A pending check is a question, not a record of what went wrong
//!
//! That repair case is the whole design. A pending check that remembered *the row that
//! collided* would fail at `COMMIT` even though nothing collides any more; so what is recorded is
//! the **key to re-examine**, and the check is re-run against the state as it stands. There is no
//! "was it violated" stored anywhere, because the answer at the time of the write is not the
//! answer at the end.
//!
//! # Deferrable changes the index's shape, not only when it is read
//!
//! A unique index normally stores its entry **by value** — no primary key in the key — which is
//! what makes a duplicate a collision on one key and the check a single point read. A deferrable
//! one cannot: the two colliding rows have to coexist until the check runs, and by-value keys
//! give them one slot between them. So a deferrable unique index stores suffixed entries, as a
//! non-unique index does, and its uniqueness is enforced by **scanning the value's range** and
//! counting what is there. The shape follows the declaration, which never changes; `SET
//! CONSTRAINTS` changes only *when* the scan happens.
//!
//! # The hook
//!
//! [`Check`] is an enum on purpose. A second kind of deferred constraint adds a variant and an arm
//! in [`Check::verify`], and needs nothing else: the collection, the mode resolution, the drain at
//! `COMMIT` and the drain at `SET CONSTRAINTS … IMMEDIATE` are all written in terms of the enum.
//! `EXCLUDE` is the one coming — an `ExcludeDef { deferrable, deferred }` in the catalog, enforced
//! by scanning for an overlapping row — and it registers by pushing a `Check::Exclude { … }` where
//! the unique path pushes its own, and answering for it in `verify`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::value::Datum;

/// One constraint check a transaction still owes.
#[derive(Debug)]
pub(crate) enum Check {
    /// A `UNIQUE` constraint, at one key value.
    ///
    /// The index is named by id rather than borrowed, because the check outlives the statement
    /// that registered it and the table it belongs to may have been read again since.
    Unique {
        /// The table the index is on, as it stood when the row was written.
        table: Arc<TableDef>,
        /// Which index of it.
        index: u64,
        /// The key values to re-examine.
        values: Vec<Datum>,
    },
    /// An `EXCLUDE` constraint, for one written row.
    ///
    /// **The row, not the key.** The key is an *expression* over the row and the partial `WHERE`
    /// is another, so re-examining means evaluating both again against the table as it stands —
    /// and the row is also what the `23P01` `DETAIL` prints. Named by position in
    /// [`TableDef::excludes`] for the reason the unique check names its index by id: the check
    /// outlives the statement.
    Exclude {
        /// The table as it stood when the row was written.
        table: Arc<TableDef>,
        /// Which of its `excludes`.
        at: usize,
        /// The row that was written.
        row: Vec<Datum>,
    },
    /// A `FOREIGN KEY`, for one child row's key.
    ///
    /// **Both tables are carried.** The check is a lookup in the *parent*, and re-reading the
    /// parent's record at `COMMIT` would need a catalog view this call does not have; the parent's
    /// rows are read from the transaction as it stands, which is what "re-examined" means here —
    /// a parent inserted later in the same transaction satisfies it, which is the case
    /// `INITIALLY DEFERRED` exists for.
    ForeignKey {
        /// The child, as it stood when the row was written.
        table: Arc<TableDef>,
        /// Which of its `foreign_keys`.
        at: usize,
        /// The parent's record, resolved when the check was registered.
        parent: Arc<TableDef>,
        /// The referencing values to look up again.
        values: Vec<Datum>,
    },
}

impl Check {
    /// The constraint's name, which is what a `SET CONSTRAINTS` names and what a violation says.
    pub(crate) fn constraint(&self) -> &str {
        match self {
            Check::Unique { table, index, .. } => table
                .indexes
                .iter()
                .find(|candidate| candidate.id == *index)
                .map_or("", |candidate| candidate.name.as_str()),
            Check::Exclude { table, at, .. } => table
                .excludes
                .get(*at)
                .map_or("", |exclude| exclude.name.as_str()),
            Check::ForeignKey { table, at, .. } => table
                .foreign_keys
                .get(*at)
                .map_or("", |key| key.name.as_str()),
        }
    }

    /// Runs the check against the transaction as it stands now.
    ///
    /// **Re-examined, never replayed**: the key is looked at again, so a duplicate that has since
    /// been deleted is no violation and a row that was fine when written and collides now is one.
    pub(crate) fn verify(&self, txn: &dyn crate::backend::Txn, tenant: u64) -> Result<()> {
        match self {
            Check::Unique {
                table,
                index,
                values,
            } => {
                let Some(index) = table
                    .indexes
                    .iter()
                    .find(|candidate| candidate.id == *index)
                else {
                    // The index was dropped inside this transaction, so there is no constraint
                    // left to break. Not an error: `DROP` is how a user withdraws one.
                    return Ok(());
                };
                let (start, end) =
                    crate::row::index_value_range(tenant, table.id, index.id, values)?;
                // **Two is the whole question.** A third entry changes no answer, and asking for
                // only two keeps a widely duplicated key from costing a scan of all of it.
                if txn.scan(&start, &end, 2)?.len() > 1 {
                    return Err(SqlError::UniqueViolation {
                        constraint: index.name.clone(),
                        key: Some(super::index::render_key(table, &index.keys, values)),
                    });
                }
                Ok(())
            }
            // **Re-scanned, and the written row is in the table now** — so it would overlap
            // itself. `exclusion_conflict` skips the row by primary key, which is what lets one
            // function answer both here and at the statement.
            Check::Exclude { table, at, row } => {
                let Some(exclude) = table.excludes.get(*at) else {
                    // The constraint went with a `DROP` inside this transaction; there is nothing
                    // left to break. The reading the unique arm above takes.
                    return Ok(());
                };
                match super::dml::exclusion_conflict(txn, tenant, table, exclude, row)? {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            Check::ForeignKey {
                table,
                at,
                parent,
                values,
            } => {
                let Some(key) = table.foreign_keys.get(*at) else {
                    // Dropped inside this transaction: the reading the two arms above take.
                    return Ok(());
                };
                if super::foreign_key::parent_row(parent, tenant, txn, key, values)?.is_some() {
                    return Ok(());
                }
                Err(SqlError::ForeignKeyViolation {
                    relation: table.name.clone(),
                    constraint: key.name.clone(),
                    detail: format!(
                        "Key ({})=({}) is not present in table \"{}\".",
                        super::foreign_key::column_names(table, &key.columns),
                        super::index::render_values(values),
                        parent.name
                    ),
                })
            }
        }
    }
}

/// What a transaction has been told about its constraints, and what it owes.
#[derive(Debug, Default)]
pub(crate) struct Constraints {
    /// `SET CONSTRAINTS ALL { DEFERRED | IMMEDIATE }`, or `None` if the transaction has not said.
    all: Option<bool>,
    /// `SET CONSTRAINTS <name> …`, which beats the `ALL` above it.
    by_name: BTreeMap<String, bool>,
    /// The checks owed, in the order they were registered.
    pending: Vec<Check>,
}

impl Constraints {
    /// Whether `name` is deferred right now, given how it was declared.
    ///
    /// A name set explicitly wins over `ALL`, and `ALL` wins over the declaration — which is
    /// PostgreSQL's precedence and the reason both are kept rather than one flattened setting.
    pub(crate) fn deferred(&self, name: &str, initially_deferred: bool) -> bool {
        if let Some(set) = self.by_name.get(name) {
            return *set;
        }
        self.all.unwrap_or(initially_deferred)
    }

    /// Records a check to run later.
    pub(crate) fn push(&mut self, check: Check) {
        self.pending.push(check);
    }

    /// `SET CONSTRAINTS ALL …`.
    ///
    /// **It does not reach a constraint that is not deferrable.** `SET CONSTRAINTS ALL DEFERRED`
    /// followed by a duplicate against a plain `UNIQUE` still fails at the statement — measured —
    /// which falls out of this being consulted only where a deferrable constraint asks.
    pub(crate) fn set_all(&mut self, deferred: bool) {
        self.all = Some(deferred);
        self.by_name.clear();
    }

    /// `SET CONSTRAINTS <name> …`.
    pub(crate) fn set_one(&mut self, name: String, deferred: bool) {
        self.by_name.insert(name, deferred);
    }

    /// The checks owed, taken out of the list.
    ///
    /// Taken rather than borrowed because every caller is ending them: `COMMIT` runs them all and
    /// `SET CONSTRAINTS … IMMEDIATE` runs the ones it names.
    pub(crate) fn take(&mut self) -> Vec<Check> {
        std::mem::take(&mut self.pending)
    }

    /// The checks owed for one constraint, taken out; the rest stay.
    pub(crate) fn take_named(&mut self, name: &str) -> Vec<Check> {
        let (named, rest) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|check| check.constraint() == name);
        self.pending = rest;
        named
    }

    /// Whether anything is owed, which is what lets `COMMIT` skip the whole mechanism.
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Forgets everything: the end of a transaction, either way.
    pub(crate) fn clear(&mut self) {
        self.all = None;
        self.by_name.clear();
        self.pending.clear();
    }
}
