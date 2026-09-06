//! `DECLARE`, `FETCH`, `MOVE` and `CLOSE` — a query read once, and a position walked over it.
//!
//! **On the executor rather than on the protocol session**, and the reason is the lifetime: a
//! cursor without `WITH HOLD` dies with the transaction that declared it, and the executor is
//! what owns the transaction. It is also what both callers of a statement go through — a
//! connection and the corpus harness — so there is no dispatch to keep in step.
//!
//! **The rows are read at `DECLARE`.** A real server streams them and takes its snapshot then;
//! this node materialises every result already, so reading them at `DECLARE` is the same snapshot
//! and the same errors at the same statement. What it costs is the memory a `SELECT` of the same
//! query would have cost anyway.

use crate::error::{Result, SqlError};
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::Outcome;
use crate::plan::{CursorDirection, CursorStatement};

use super::{Executor, Txn};

/// A declared cursor: the rows it read, and where in them it is sitting.
#[derive(Debug)]
pub(super) struct Open {
    /// The row shape, sent again with every `FETCH` that returns rows.
    fields: Vec<FieldDescription>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    /// **Where the cursor is, counting from one.** `0` is before the first row and `rows.len() + 1`
    /// is after the last; anything between is *on* that row. PostgreSQL's model exactly, and the
    /// reason it has to be a position rather than an index: `FETCH FORWARD 0` re-reads the row the
    /// cursor is on, which a "next index to read" cannot express.
    at: i64,
}

/// One cursor statement.
pub(super) fn run(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    statement: &CursorStatement,
) -> Result<Outcome> {
    match statement {
        CursorStatement::Declare { name, query } => {
            if executor.cursors.contains_key(name) {
                return Err(SqlError::DuplicateCursor(name.clone()));
            }
            // The query runs here, so a `DECLARE` over a table that is not there fails at the
            // `DECLARE` — which is where PostgreSQL fails it too.
            let (fields, rows) = match executor.select(txn, query)? {
                Outcome::Rows { fields, rows, .. } => (fields, rows),
                // `plan::Statement::Select` always produces rows; a command tag here would mean
                // the lowering had handed us something that is not a query.
                Outcome::Done { tag } => {
                    return Err(SqlError::Internal(format!(
                        "DECLARE CURSOR ran a statement that answered {tag}"
                    )));
                }
            };
            executor.cursors.insert(
                name.clone(),
                Open {
                    fields,
                    rows,
                    at: 0,
                },
            );
            Ok(Outcome::done("DECLARE CURSOR"))
        }
        CursorStatement::Fetch {
            name,
            direction,
            only_move,
        } => {
            let open = executor
                .cursors
                .get_mut(name)
                .ok_or_else(|| SqlError::UndefinedCursor(name.clone()))?;
            let rows = open.step(*direction);
            let count = rows.len();
            if *only_move {
                return Ok(Outcome::done(format!("MOVE {count}")));
            }
            Ok(Outcome::Rows {
                fields: open.fields.clone(),
                rows,
                tag: format!("FETCH {count}"),
            })
        }
        CursorStatement::Close(None) => {
            executor.cursors.clear();
            Ok(Outcome::done("CLOSE CURSOR ALL"))
        }
        CursorStatement::Close(Some(name)) => {
            executor
                .cursors
                .remove(name)
                .ok_or_else(|| SqlError::UndefinedCursor(name.clone()))?;
            Ok(Outcome::done("CLOSE CURSOR"))
        }
    }
}

impl Open {
    /// Moves the cursor and hands back the rows it passed over.
    ///
    /// Every line of the measured ladder falls out of the position model; the three that would not
    /// fall out of an index model are worth naming:
    ///
    /// * **Backward returns rows in the order it read them**, so `FETCH BACKWARD 2` from row 4 is
    ///   `3, 2` and not `2, 3`.
    /// * **Zero re-reads.** `FETCH FORWARD 0` and `MOVE 0` answer a count of one and leave the
    ///   cursor where it is — they are how a client asks "what row am I on".
    /// * **`MOVE BACKWARD ALL` counts what it passed**, so it is `MOVE 4` over four rows rather
    ///   than `MOVE 1` for the one hop to the start.
    fn step(&mut self, direction: CursorDirection) -> Vec<Vec<Option<Vec<u8>>>> {
        let last = i64::try_from(self.rows.len()).unwrap_or(i64::MAX);
        match direction {
            CursorDirection::Relative(0) => self.row_at(self.at).into_iter().collect(),
            CursorDirection::Relative(step) if step > 0 => {
                let from = self.at + 1;
                self.at = (self.at + step).min(last + 1);
                self.collect(from..=self.at, false)
            }
            CursorDirection::Relative(step) => {
                let from = self.at - 1;
                self.at = (self.at + step).max(0);
                self.collect(self.at..=from, true)
            }
            // A row number, counted from the end when it is negative: `-1` is the last row, which
            // is what makes `LAST` this and not a direction of its own.
            CursorDirection::Absolute(number) => {
                self.at = if number < 0 {
                    (last + 1 + number).max(0)
                } else {
                    number.min(last + 1)
                };
                self.row_at(self.at).into_iter().collect()
            }
            CursorDirection::All(true) => {
                let from = self.at + 1;
                self.at = last + 1;
                self.collect(from..=last, false)
            }
            CursorDirection::All(false) => {
                let from = self.at - 1;
                self.at = 0;
                self.collect(1..=from, true)
            }
        }
    }

    /// The rows in a one-based inclusive range, reversed when the cursor walked backwards.
    fn collect(
        &self,
        range: std::ops::RangeInclusive<i64>,
        backwards: bool,
    ) -> Vec<Vec<Option<Vec<u8>>>> {
        let mut rows: Vec<_> = range.filter_map(|at| self.row_at(at)).collect();
        if backwards {
            rows.reverse();
        }
        rows
    }

    /// The row the cursor would be on at `at`, or nothing when that is past either end.
    fn row_at(&self, at: i64) -> Option<Vec<Option<Vec<u8>>>> {
        let index = usize::try_from(at.checked_sub(1)?).ok()?;
        self.rows.get(index).cloned()
    }
}
