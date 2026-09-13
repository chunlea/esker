//! Running a PL/pgSQL block inside the statement that reached it —
//! [ADR 0113](../../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md).
//!
//! **One statement, one transaction.** A `DO` is one statement to the session, and every SQL
//! statement its body holds runs through [`Executor::run_recording`] with that statement's own
//! transaction and its own [`Written`]: a write lands in the same buffer, a catalog write marks the
//! transaction exactly as top-level DDL does, and an error anywhere is the `DO`'s error — undone by
//! the implicit savepoint the statement already runs under. There is no second transaction and no
//! write that outlives a failure.
//!
//! **A variable reaches SQL as a value, never as text.** Each fragment is parsed and lowered on its
//! own, and every column reference that names a variable — `max_value`, `r.constraint_check` — is
//! replaced in the lowered tree by a typed literal before the statement is bound. Printing the value
//! into the SQL and reading it back is the round trip that loses a type; PostgreSQL binds a variable
//! as a parameter for the same reason.
//!
//! **What PostgreSQL refuses while a body runs is refused in its words**, each one measured:
//! `22004 query string argument of EXECUTE is null`, `42601 query has no destination for result
//! data`, `42702 column reference "id" is ambiguous`, `55000 record "r" is not assigned yet`.

use super::query::OutputColumn;
use super::{Executor, Written, bind, cancel};
use crate::backend::Txn;
use crate::error::{Result, Severity, SqlError};
use crate::parse::StatementClass;
use crate::pgwire::session::{Outcome, Params};
use crate::plan::{Expr, Literal, Statement as Plan};
use crate::plpgsql::{Block, Context, RaiseLevel, Statement, Target, VariableType};
use crate::value::{ColumnType, Datum, PgDatum as _};

/// How many PL/pgSQL bodies may run inside one another before the statement is refused.
///
/// PostgreSQL's bound is `max_stack_depth`, and past it the answer is `54001 stack depth limit
/// exceeded` — the same one [`SqlError::StatementTooComplex`] carries. A body that `EXECUTE`s a
/// `DO` that `EXECUTE`s a `DO` is a stack of this executor's own frames, several per level, and
/// this is sized so that the stack an executor thread has is never the thing that stops it.
const MAX_DEPTH: usize = 16;

/// A value a record variable holds: one row, its fields named.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub(super) fields: Vec<Field>,
}

/// One field of a [`Row`].
#[derive(Debug, Clone)]
pub(super) struct Field {
    /// The column's name, folded.
    pub(super) name: String,
    /// What the value physically is.
    pub(super) ty: ColumnType,
    /// The user-defined type it was declared as, when a `ColumnType` cannot say — an enum's
    /// ordinal is an `int2` in the row.
    pub(super) user: Option<crate::catalog::TypeDef>,
    /// The value.
    pub(super) value: Datum,
}

impl Row {
    /// A query's row, under the names and types its projection declared.
    fn of(columns: &[OutputColumn], values: Vec<Datum>) -> Row {
        Row {
            fields: columns
                .iter()
                .zip(values)
                .map(|(column, value)| Field {
                    name: column.name.clone(),
                    ty: column.ty,
                    user: column.user_type.clone(),
                    value,
                })
                .collect(),
        }
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|field| field.name == name)
    }
}

/// What a variable holds.
#[derive(Debug, Clone)]
enum Value {
    /// A value of an SQL type, `NULL` until something is assigned.
    Scalar { ty: ColumnType, value: Datum },
    /// A row, or nothing yet.
    Record(Option<Row>),
}

/// The variables a running body can see.
#[derive(Debug, Default)]
pub(super) struct Frame {
    variables: Vec<(String, Value)>,
}

/// What a column reference in a fragment turned out to be.
enum Resolved {
    /// A column, and none of this frame's business.
    Column,
    /// A variable, and the literal that stands for it.
    Value(Expr),
    /// A variable that cannot be read, in PostgreSQL's words.
    Refused(SqlError),
}

impl Frame {
    fn get(&self, name: &str) -> Option<&Value> {
        self.variables
            .iter()
            .rev()
            .find(|(declared, _)| declared == name)
            .map(|(_, value)| value)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.variables
            .iter_mut()
            .rev()
            .find(|(declared, _)| declared == name)
            .map(|(_, value)| value)
    }

    /// A column reference `table.name`, or a bare `name`, read against the variables.
    fn resolve(&self, table: Option<&str>, name: &str) -> Resolved {
        match table {
            None => match self.get(name) {
                Some(Value::Scalar { ty, value }) => Resolved::Value(literal(value, *ty, None)),
                Some(Value::Record(_)) => {
                    Resolved::Refused(unsupported("a whole record used as a value"))
                }
                None => Resolved::Column,
            },
            Some(record) => match self.get(record) {
                Some(Value::Record(Some(row))) => match row.position(name) {
                    Some(at) => {
                        let field = &row.fields[at];
                        Resolved::Value(literal(&field.value, field.ty, field.user.as_ref()))
                    }
                    None => Resolved::Refused(SqlError::RecordHasNoField {
                        record: record.to_owned(),
                        field: name.to_owned(),
                    }),
                },
                Some(Value::Record(None)) => {
                    Resolved::Refused(SqlError::RecordNotAssigned(record.to_owned()))
                }
                // `t.id` with no record called `t` is a column of `t`.
                Some(Value::Scalar { .. }) | None => Resolved::Column,
            },
        }
    }
}

/// The literal a variable's value stands for in a fragment: typed, so that `COALESCE(max_value, 0)`
/// over a `NULL` integer is still an integer.
fn literal(value: &Datum, ty: ColumnType, user: Option<&crate::catalog::TypeDef>) -> Expr {
    if matches!(value, Datum::Null) {
        return Expr::Literal(Literal::TypedNull(ty));
    }
    let literal = Expr::Literal(Literal::Typed {
        value: Box::new(value.clone()),
        user: user.map(|def| Box::new(def.clone())),
    });
    // The cast carries a type the value's representation cannot — a `json` is a `Datum::Text` —
    // and only where the two differ, as the binder does for a bound parameter.
    if user.is_some() || value.column_type() == Some(ty) {
        literal
    } else {
        Expr::Cast {
            operand: Box::new(literal),
            to: ty,
            typmod: crate::value::NO_TYPMOD,
        }
    }
}

/// `0A000` naming a construct PostgreSQL runs and the subset does not.
fn unsupported(construct: &str) -> SqlError {
    SqlError::unsupported(format!("PL/pgSQL {construct}"))
}

/// Whether a statement is one the session itself handles — transaction control and prepared
/// statements — which no body may run.
fn is_session_control(class: &StatementClass) -> bool {
    matches!(
        class,
        StatementClass::Begin
            | StatementClass::Commit
            | StatementClass::Rollback
            | StatementClass::Savepoint(_)
            | StatementClass::RollbackTo(_)
            | StatementClass::Release(_)
            | StatementClass::Prepare { .. }
            | StatementClass::Execute { .. }
            | StatementClass::ExplainExecute { .. }
            | StatementClass::Deallocate(_)
            | StatementClass::DiscardAll
            | StatementClass::SetConstraints { .. }
    )
}

/// The first column of the first row, or `NULL` when there is none — PostgreSQL's reading of a
/// query whose answer goes into one variable.
fn first_value(rows: Vec<Vec<Datum>>) -> Datum {
    rows.into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or(Datum::Null)
}

/// What running a statement of a body did to the rest of it.
enum Flow {
    /// Go on to the next statement.
    Next,
    /// `RETURN`, with the expression it named.
    Return,
}

impl Executor {
    /// `DO $$ … $$`: the body is read whole, then run inside this statement.
    ///
    /// **Read before anything runs**, as PostgreSQL compiles a block before running it — so a
    /// construct outside the subset, or a malformed body, refuses the statement with nothing of it
    /// done.
    pub(super) fn run_do(
        &mut self,
        txn: &mut dyn Txn,
        body: &str,
        written: &mut Written,
    ) -> Result<Outcome> {
        let block = crate::plpgsql::parse(body, Context::Do)?;
        self.within_a_body(|executor| {
            let mut frame = executor.declare(txn, &block)?;
            executor.run_statements(txn, &mut frame, &block.statements, written)?;
            Ok(())
        })?;
        Ok(Outcome::done("DO"))
    }

    /// Runs `run` one body deeper, and refuses the statement past [`MAX_DEPTH`].
    fn within_a_body<T>(&mut self, run: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.plpgsql_depth >= MAX_DEPTH {
            return Err(SqlError::StatementTooComplex);
        }
        self.plpgsql_depth += 1;
        let result = run(self);
        self.plpgsql_depth -= 1;
        result
    }

    /// The frame a block starts with: every declared variable, `NULL`, with its type resolved.
    ///
    /// **Resolved before the first statement runs**, which is where PostgreSQL resolves one too: a
    /// name that is no type is `42704 type "nosuchtype" does not exist` with nothing of the body
    /// done.
    fn declare(&mut self, txn: &mut dyn Txn, block: &Block) -> Result<Frame> {
        let mut frame = Frame::default();
        for declaration in &block.declarations {
            let value = match &declaration.ty {
                VariableType::Record => Value::Record(None),
                VariableType::Sql(type_name) => {
                    // **A name that is no type is PostgreSQL's `42704`**, and a cast of `NULL` does
                    // not say so here — it answers an untyped `NULL` for a name it does not know
                    // (measured: `DECLARE n nosuchtype` ran). So the name is read as a `regtype`
                    // first, whose input function is the one that refuses it in those words.
                    self.query(
                        txn,
                        &frame,
                        &format!("SELECT '{}'::regtype", type_name.replace('\'', "''")),
                    )?;
                    let (columns, _) =
                        self.query(txn, &frame, &format!("SELECT CAST(NULL AS {type_name})"))?;
                    let Some(column) = columns.into_iter().next() else {
                        return Err(SqlError::Internal(format!(
                            "the type {type_name} resolved to no column"
                        )));
                    };
                    // An enum's value is its label's ordinal, and a variable here carries only a
                    // storage type — so one would read back as a number.
                    if column.user_type.is_some() {
                        return Err(unsupported("variables of a user-defined type"));
                    }
                    Value::Scalar {
                        ty: column.ty,
                        value: Datum::Null,
                    }
                }
            };
            frame.variables.push((declaration.name.clone(), value));
        }
        Ok(frame)
    }

    fn run_statements(
        &mut self,
        txn: &mut dyn Txn,
        frame: &mut Frame,
        statements: &[Statement],
        written: &mut Written,
    ) -> Result<Flow> {
        for statement in statements {
            cancel::check()?;
            if let Flow::Return = self.run_statement(txn, frame, statement, written)? {
                return Ok(Flow::Return);
            }
        }
        Ok(Flow::Next)
    }

    fn run_statement(
        &mut self,
        txn: &mut dyn Txn,
        frame: &mut Frame,
        statement: &Statement,
        written: &mut Written,
    ) -> Result<Flow> {
        match statement {
            Statement::Null => {}
            Statement::Assign { target, expression } => {
                let (columns, rows) = self.query(txn, frame, &format!("SELECT {expression}"))?;
                if columns.len() != 1 {
                    return Err(SqlError::AssignmentSourceColumns(columns.len()));
                }
                self.store(frame, target, first_value(rows))?;
            }
            Statement::If { condition, then } => {
                if self.condition(txn, frame, condition)? {
                    return self.run_statements(txn, frame, then, written);
                }
            }
            Statement::Raise { level, message } => {
                let severity = match level {
                    RaiseLevel::Exception => {
                        return Err(SqlError::RaisedException(message.clone()));
                    }
                    RaiseLevel::Notice => Severity::Notice,
                    RaiseLevel::Warning => Severity::Warning,
                };
                self.notice(SqlError::Raised {
                    message: message.clone(),
                    severity,
                });
            }
            Statement::Reraise => return Err(SqlError::RaiseWithoutActiveHandler),
            Statement::SelectInto { query, target } => {
                let (_, rows) = self.query(txn, frame, query)?;
                self.store(frame, target, first_value(rows))?;
            }
            Statement::ForQuery {
                record,
                query,
                body,
            } => {
                let (columns, rows) = self.query(txn, frame, query)?;
                for row in rows {
                    cancel::check()?;
                    if let Some(Value::Record(slot)) = frame.get_mut(record) {
                        *slot = Some(Row::of(&columns, row));
                    }
                    if let Flow::Return = self.run_statements(txn, frame, body, written)? {
                        return Ok(Flow::Return);
                    }
                }
            }
            Statement::Execute { command } => self.execute_dynamic(txn, frame, command, written)?,
            Statement::Return { .. } => return Ok(Flow::Return),
            Statement::Sql { text } => self.run_sql(txn, frame, text, written)?,
        }
        Ok(Flow::Next)
    }

    /// One fragment, parsed, lowered, with the frame's variables put in, and bound.
    fn prepare(&mut self, txn: &mut dyn Txn, frame: &Frame, sql: &str) -> Result<Plan> {
        let parsed = crate::parse::parse_statements(sql)?;
        let [parsed] = parsed.as_slice() else {
            return Err(SqlError::Internal(format!(
                "a PL/pgSQL fragment held {} statements",
                parsed.len()
            )));
        };
        if is_session_control(parsed.class()) {
            return Err(unsupported("transaction control"));
        }
        let mut statement = parsed.lower()?;
        self.substitute(&*txn, &mut statement, frame)?;
        self.bound(&*txn, statement, &Params::NONE)
    }

    /// Replaces every column reference that names a variable with the variable's value.
    ///
    /// **A name that is both a variable and a column of the statement's relation is PostgreSQL's
    /// `42702`**, the refusal its default `plpgsql.variable_conflict = error` makes — measured, with
    /// the `DETAIL` that says which two things the name could be. It is asked of the relations the
    /// statement names, and only when a variable was put in at all.
    fn substitute(&self, txn: &dyn Txn, statement: &mut Plan, frame: &Frame) -> Result<()> {
        if frame.variables.is_empty() {
            return Ok(());
        }
        let mut failure = None;
        let mut bare = Vec::new();
        bind::walk_mut(statement, &mut |expr| {
            let Expr::Column { table, name } = expr else {
                return;
            };
            match frame.resolve(table.as_deref(), name) {
                Resolved::Column => {}
                Resolved::Value(value) => {
                    if table.is_none() {
                        bare.push(name.clone());
                    }
                    *expr = value;
                }
                Resolved::Refused(error) => {
                    failure.get_or_insert(error);
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        if !bare.is_empty() {
            let tables = self.tables_for(txn, statement)?;
            if let Some(name) = bare
                .into_iter()
                .find(|name| tables.iter().any(|table| table.column(name).is_some()))
            {
                return Err(SqlError::PlpgsqlAmbiguousColumn(name));
            }
        }
        Ok(())
    }

    /// A query's projection and rows, as values rather than as the text a client is sent.
    fn query(
        &mut self,
        txn: &mut dyn Txn,
        frame: &Frame,
        sql: &str,
    ) -> Result<(Vec<OutputColumn>, Vec<Vec<Datum>>)> {
        let Plan::Select(select) = self.prepare(txn, frame, sql)? else {
            return Err(unsupported("a query that is not a SELECT"));
        };
        let (planned, rows) = self.planned_rows(txn, &select)?;
        let width = planned.columns.len();
        let rows = rows
            .into_iter()
            .map(|mut row| {
                // The junk columns a locking clause reads its keys from are the query's own.
                row.truncate(width);
                row
            })
            .collect();
        Ok((planned.columns, rows))
    }

    /// `IF`'s reading of a condition: `NULL` is false, and a value that is not a boolean is read as
    /// one the way PostgreSQL reads it — by its text, so `IF 1 THEN` is true.
    fn condition(&mut self, txn: &mut dyn Txn, frame: &Frame, condition: &str) -> Result<bool> {
        let (_, rows) = self.query(txn, frame, &format!("SELECT {condition}"))?;
        match self.convert(first_value(rows), ColumnType::Bool)? {
            Datum::Bool(value) => Ok(value),
            _ => Ok(false),
        }
    }

    /// A value, converted to `ty` the way PostgreSQL's PL/pgSQL assigns one: an assignment cast
    /// where there is one, and otherwise through the value's text — which is how `n = '7'` stores
    /// `7` in an integer and `n = 'abc'` answers `22P02 invalid input syntax for type integer`.
    fn convert(&self, value: Datum, ty: ColumnType) -> Result<Datum> {
        let rendering = self.rendering();
        if matches!(value, Datum::Null) || value.column_type() == Some(ty) {
            return Ok(value);
        }
        match crate::value::assignment_cast(value.clone(), ty, rendering) {
            Ok(converted) => Ok(converted),
            Err(error) => match crate::value::to_text_under(&value, rendering) {
                Some(text) => Datum::from_text(ty, &text),
                None => Err(error),
            },
        }
    }

    /// Stores `value` into `target`, converted to the target's type.
    fn store(&self, frame: &mut Frame, target: &Target, value: Datum) -> Result<()> {
        match target {
            Target::Variable(name) => {
                let Some(Value::Scalar { ty, .. }) = frame.get(name) else {
                    return Err(SqlError::Internal(format!(
                        "PL/pgSQL target {name} is not a scalar variable"
                    )));
                };
                let converted = self.convert(value, *ty)?;
                if let Some(Value::Scalar { value: slot, .. }) = frame.get_mut(name) {
                    *slot = converted;
                }
                Ok(())
            }
            Target::Field { record, field } => {
                let row = match frame.get_mut(record) {
                    Some(Value::Record(Some(row))) => row,
                    Some(Value::Record(None)) => {
                        return Err(SqlError::RecordNotAssigned(record.clone()));
                    }
                    _ => {
                        return Err(SqlError::Internal(format!(
                            "PL/pgSQL target {record} is not a record"
                        )));
                    }
                };
                let Some(at) = row.position(field) else {
                    return Err(SqlError::RecordHasNoField {
                        record: record.clone(),
                        field: field.clone(),
                    });
                };
                if row.fields[at].user.is_some() {
                    return Err(unsupported("assignment to a field of a user-defined type"));
                }
                let ty = row.fields[at].ty;
                let converted = self.convert(value, ty)?;
                row.fields[at].value = converted;
                Ok(())
            }
        }
    }

    /// `EXECUTE <expression>`: the expression's value, run as one or more statements with no
    /// variables in scope — PostgreSQL's dynamic SQL sees none — and their rows discarded.
    fn execute_dynamic(
        &mut self,
        txn: &mut dyn Txn,
        frame: &Frame,
        command: &str,
        written: &mut Written,
    ) -> Result<()> {
        let (_, rows) = self.query(txn, frame, &format!("SELECT {command}"))?;
        let text = match first_value(rows) {
            Datum::Null => return Err(SqlError::ExecuteQueryIsNull),
            value => crate::value::to_text_under(&value, self.rendering()).ok_or_else(|| {
                SqlError::Internal("an EXECUTE command that has no text".to_owned())
            })?,
        };
        for parsed in crate::parse::parse_statements(&text)? {
            if is_session_control(parsed.class()) {
                return Err(SqlError::ExecuteOfTransactionCommands);
            }
            let statement = self.bound(&*txn, parsed.lower()?, &Params::NONE)?;
            if let Plan::Session(_) = statement {
                return Err(unsupported("SET"));
            }
            self.run_recording(txn, &statement, written)?;
        }
        Ok(())
    }

    /// An SQL statement of a body, with its variables put in.
    ///
    /// **Rows with nowhere to go are PostgreSQL's `42601`**: a `SELECT` without `INTO` is refused
    /// before it runs, with the `HINT` that names `PERFORM`, and a `RETURNING` without one after —
    /// without it, measured.
    fn run_sql(
        &mut self,
        txn: &mut dyn Txn,
        frame: &Frame,
        text: &str,
        written: &mut Written,
    ) -> Result<()> {
        let statement = self.prepare(txn, frame, text)?;
        match &statement {
            Plan::Select(_) => return Err(SqlError::QueryHasNoDestination { select: true }),
            Plan::Session(_) => return Err(unsupported("SET")),
            _ => {}
        }
        if let Outcome::Rows { .. } = self.run_recording(txn, &statement, written)? {
            return Err(SqlError::QueryHasNoDestination { select: false });
        }
        Ok(())
    }
}
