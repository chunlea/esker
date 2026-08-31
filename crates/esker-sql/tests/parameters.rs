//! `$1`, and what its bytes mean.
//!
//! A `Bind` carries values and format codes and no types at all, so the type comes from *where*
//! the parameter appears — the column it is inserted into, the column it is compared against. That
//! inference is also what `Describe` reports, which is the third obligation
//! `docs/plans/phase-6a.md` §10a left for unit 6: until now `ParameterDescription` echoed back what
//! the client had declared, which tells a client that declared nothing exactly nothing.
//!
//! The fallback is `text`, measured rather than assumed: `PREPARE p AS SELECT $1` on a real
//! PostgreSQL 19 reports `{text}`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;
use esker_sql::value::{ColumnType, Datum};

struct Node {
    executor: Executor,
}

impl Node {
    fn loaded() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Node {
            executor: Executor::new(backend, Arc::new(Catalog::new()), 1),
        };
        node.plain("CREATE TABLE t (id int8 PRIMARY KEY, name text, at timestamptz, ok bool)")
            .unwrap();
        node
    }

    fn plain(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        self.bound(sql, &[], &[])
    }

    fn bound(
        &mut self,
        sql: &str,
        values: &[Option<Vec<u8>>],
        formats: &[i16],
    ) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(
                &parsed,
                &Params {
                    values,
                    formats,
                    declared: &[],
                },
            )?;
        }
        Ok(last)
    }

    fn describe(&mut self, sql: &str, declared: &[u32]) -> esker_sql::Result<Vec<u32>> {
        let parsed = parse_statements(sql)?;
        Ok(self.executor.describe(&parsed[0], declared)?.parameters)
    }

    fn column(&mut self, sql: &str) -> Vec<String> {
        match self.plain(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row[0].clone().map_or_else(
                        || "NULL".to_owned(),
                        |bytes| String::from_utf8(bytes).unwrap(),
                    )
                })
                .collect(),
            Outcome::Done { tag } => panic!("not rows: {tag}"),
        }
    }
}

/// A text-format parameter. `Option` because the wire spells NULL as a length of -1, and every
/// caller here is building a slot in that shape.
#[allow(clippy::unnecessary_wraps, reason = "the shape a Bind parameter has")]
fn text(value: &str) -> Option<Vec<u8>> {
    Some(value.as_bytes().to_vec())
}

#[test]
fn a_text_parameter_is_read_as_the_column_it_is_going_into() {
    let mut node = Node::loaded();
    node.bound(
        "INSERT INTO t VALUES ($1, $2, $3, $4)",
        &[
            text("1"),
            text("ada"),
            text("2024-02-29 12:34:56.1+00"),
            text("t"),
        ],
        &[],
    )
    .unwrap();
    assert_eq!(node.column("SELECT name FROM t WHERE id = 1"), ["ada"]);
    assert_eq!(
        node.column("SELECT at FROM t WHERE id = 1"),
        ["2024-02-29 12:34:56.1+00"],
        "read by the timestamptz input function, not stored as characters"
    );
    assert_eq!(node.column("SELECT ok FROM t WHERE id = 1"), ["t"]);
}

/// The binary formats are PostgreSQL's own, captured from `COPY ... (FORMAT binary)`. A
/// `timestamptz` is the sharpest case: it goes onto the wire exactly as it is stored.
#[test]
fn a_binary_parameter_is_read_the_way_postgresql_sends_it() {
    let mut node = Node::loaded();
    let at = Datum::from_text(ColumnType::TimestampTz, "2024-02-29 12:34:56.1+00").unwrap();
    node.bound(
        "INSERT INTO t VALUES ($1, $2, $3, $4)",
        &[
            Datum::Int8(7).to_binary(),
            Datum::Text("bin".into()).to_binary(),
            at.to_binary(),
            Datum::Bool(false).to_binary(),
        ],
        // One format code for all of them, which is the protocol's shorthand.
        &[1],
    )
    .unwrap();
    assert_eq!(node.column("SELECT name FROM t WHERE id = 7"), ["bin"]);
    assert_eq!(
        node.column("SELECT at FROM t WHERE id = 7"),
        ["2024-02-29 12:34:56.1+00"]
    );
    assert_eq!(node.column("SELECT ok FROM t WHERE id = 7"), ["f"]);
}

/// One format code per parameter, mixed. Getting the three-way rule wrong reads a binary value as
/// text or the reverse, and both produce garbage rather than an error.
#[test]
fn text_and_binary_parameters_mix_in_one_bind() {
    let mut node = Node::loaded();
    node.bound(
        "INSERT INTO t (id, name) VALUES ($1, $2)",
        &[Datum::Int8(3).to_binary(), text("mixed")],
        &[1, 0],
    )
    .unwrap();
    assert_eq!(node.column("SELECT name FROM t WHERE id = 3"), ["mixed"]);
}

#[test]
fn a_null_parameter_is_a_null_and_not_an_empty_string() {
    let mut node = Node::loaded();
    node.bound(
        "INSERT INTO t (id, name) VALUES ($1, $2)",
        &[text("1"), None],
        &[],
    )
    .unwrap();
    assert_eq!(node.column("SELECT name FROM t WHERE id = 1"), ["NULL"]);
    assert_eq!(node.column("SELECT id FROM t WHERE name IS NULL"), ["1"]);
}

#[test]
fn a_parameter_in_a_where_clause_takes_the_columns_type() {
    let mut node = Node::loaded();
    node.plain("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')")
        .unwrap();

    let Outcome::Rows { rows, .. } = node
        .bound("SELECT name FROM t WHERE id = $1", &[text("2")], &[])
        .unwrap()
    else {
        panic!("not rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some(&b"b"[..]));
}

/// The obligation from §10a: `Describe` reports what the statement needs, not what the client
/// happened to declare.
#[test]
fn describe_infers_the_parameter_types() {
    let mut node = Node::loaded();
    assert_eq!(
        node.describe("INSERT INTO t (id, name) VALUES ($1, $2)", &[])
            .unwrap(),
        [ColumnType::Int8.oid(), ColumnType::Text.oid()]
    );
    assert_eq!(
        node.describe("SELECT * FROM t WHERE id = $1", &[]).unwrap(),
        [ColumnType::Int8.oid()]
    );
    assert_eq!(
        node.describe("SELECT * FROM t WHERE at = $1 AND ok = $2", &[])
            .unwrap(),
        [ColumnType::TimestampTz.oid(), ColumnType::Bool.oid()]
    );
    assert_eq!(
        node.describe("UPDATE t SET name = $1 WHERE id = $2", &[])
            .unwrap(),
        [ColumnType::Text.oid(), ColumnType::Int8.oid()]
    );
    assert_eq!(
        node.describe("SELECT id FROM t LIMIT $1", &[]).unwrap(),
        [ColumnType::Int8.oid()],
        "a LIMIT is a count whatever else is going on"
    );
    assert_eq!(
        node.describe("SELECT $1", &[]).unwrap(),
        [ColumnType::Text.oid()],
        "nothing types it, and PostgreSQL's fallback is text"
    );
}

/// A type the client declared wins: it is the one that knows what bytes it is sending.
#[test]
fn a_declared_type_beats_the_inferred_one() {
    let mut node = Node::loaded();
    assert_eq!(
        node.describe("SELECT $1", &[ColumnType::Int8.oid()])
            .unwrap(),
        [ColumnType::Int8.oid()],
        "nothing types it, so the declaration is all there is"
    );
    // And a declaration that contradicts the context is still honoured -- and then the comparison
    // it implies is the one PostgreSQL refuses, for the same reason and with the same code.
    assert_eq!(
        node.describe("SELECT * FROM t WHERE id = $1", &[ColumnType::Text.oid()])
            .unwrap_err()
            .sqlstate(),
        sqlstate::UNDEFINED_FUNCTION
    );
    // A zero means "you decide", which is what a client that declares nothing sends.
    assert_eq!(
        node.describe("SELECT * FROM t WHERE id = $1", &[0])
            .unwrap(),
        [ColumnType::Int8.oid()]
    );
}

/// `Describe` also answers the row shape, which is what a driver builds its decoder from.
#[test]
fn describe_answers_the_row_shape_too() {
    let mut node = Node::loaded();
    let parsed = parse_statements("SELECT id, name FROM t WHERE id = $1").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described.fields.expect("a SELECT returns rows");
    assert_eq!(
        fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        ["id", "name"]
    );
    assert_eq!(fields[0].type_oid, ColumnType::Int8.oid());

    let parsed = parse_statements("INSERT INTO t (id) VALUES (1)").unwrap();
    assert_eq!(
        node.executor.describe(&parsed[0], &[]).unwrap().fields,
        None,
        "a statement that returns no rows is NoData"
    );
}

#[test]
fn a_parameter_with_nothing_bound_to_it_is_42p02() {
    let mut node = Node::loaded();
    let error = node
        .bound("INSERT INTO t (id) VALUES ($1)", &[], &[])
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_PARAMETER);
    assert_eq!(error.to_string(), "there is no parameter $1");
}

/// A value that will not read as its type keeps the input function's own error, which says what is
/// actually wrong with it.
#[test]
fn a_parameter_that_will_not_read_keeps_its_own_error() {
    let mut node = Node::loaded();
    let error = node
        .bound(
            "INSERT INTO t (id) VALUES ($1)",
            &[text("not a number")],
            &[],
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type bigint: \"not a number\""
    );
}

/// A binary value of the wrong length is a client bug, and guessing at what it meant would turn it
/// into a wrong number.
#[test]
fn a_binary_parameter_of_the_wrong_length_is_a_protocol_violation() {
    let mut node = Node::loaded();
    let error = node
        .bound("INSERT INTO t (id) VALUES ($1)", &[Some(vec![0, 1])], &[1])
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::PROTOCOL_VIOLATION);
}

/// Describing a statement over a **join** resolves against both tables, and a prepared join has to
/// work at all.
///
/// This is where the join unit was broken and nothing said so: `Describe` planned with the inner
/// table missing, so every prepared `SELECT` over a join came back `42P01 relation "c" does not
/// exist` — naming a table that plainly did exist. A driver that prepares its statements, which is
/// most of them, would have failed on the first join it sent, while `psql`'s simple query protocol
/// worked fine.
#[test]
fn a_join_can_be_prepared_and_its_parameters_come_from_either_table() {
    let mut node = Node::loaded();
    node.plain("CREATE TABLE c (id int8 PRIMARY KEY, email text UNIQUE, at2 timestamptz)")
        .unwrap();
    node.plain("CREATE TABLE o (id int8 PRIMARY KEY, cid int8, tag text)")
        .unwrap();

    // A parameter compared against a column of the *inner* table takes that column's type.
    assert_eq!(
        node.describe(
            "SELECT o.id FROM o JOIN c ON o.cid = c.id WHERE c.email = $1",
            &[]
        )
        .unwrap(),
        [ColumnType::Text.oid()]
    );
    // And one against the outer table's.
    assert_eq!(
        node.describe(
            "SELECT o.id FROM o JOIN c ON o.cid = c.id WHERE o.id = $1",
            &[]
        )
        .unwrap(),
        [ColumnType::Int8.oid()]
    );
    // Two, one from each side, in `$n` order rather than in the order they appear.
    assert_eq!(
        node.describe(
            "SELECT o.id FROM o JOIN c ON o.cid = c.id WHERE c.at2 = $2 AND o.id = $1",
            &[],
        )
        .unwrap(),
        [ColumnType::Int8.oid(), ColumnType::TimestampTz.oid()]
    );

    // A bare name both tables have is refused at *describe* time, not left to execution --
    // which is what a real server does with the same statement under `PREPARE`, because Describe
    // plans the statement and planning is where the ambiguity is found.
    let error = node
        .describe(
            "SELECT o.cid FROM o JOIN c ON o.cid = c.id WHERE id = $1",
            &[],
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::AMBIGUOUS_COLUMN);

    // And the whole thing runs bound, which is what a driver actually does.
    node.plain("INSERT INTO c VALUES (1,'a@x',NULL)").unwrap();
    node.plain("INSERT INTO o VALUES (10,1,'t')").unwrap();
    let outcome = node
        .bound(
            "SELECT o.id FROM o JOIN c ON o.cid = c.id WHERE c.email = $1",
            &[Some(b"a@x".to_vec())],
            &[],
        )
        .unwrap();
    let Outcome::Rows { rows, .. } = outcome else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"10".to_vec())]]);
}
