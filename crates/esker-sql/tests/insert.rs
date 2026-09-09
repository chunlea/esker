//! `INSERT`, and the two ways a duplicate arrives.
//!
//! The second of those is what `docs/plans/phase-6a.md` §5 spent a ruling on, and it is the reason
//! `backend::tests::a_concurrent_duplicate_loses_at_commit` was written before there was an
//! executor to lose the race: a transaction that reads a unique index key as absent, writes it,
//! and *then* loses the commit has inserted a duplicate from the user's point of view, and telling
//! it `40001 serialization_failure` would be telling it to retry something that will never succeed.
//!
//! [`a_concurrent_duplicate_is_reported_as_a_duplicate`] is that scenario against the real
//! executor. It is the test the plan named, and it is the one that would have failed if the
//! `40001` had been left as it came back from the store.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::row;
use esker_sql::sqlstate;
use esker_sql::value::PgDatum;
use esker_sql::value::{ColumnType, Datum};

/// A node, and as many sessions against it as a test needs.
struct Node {
    backend: Arc<MemoryBackend>,
    catalog: Arc<Catalog>,
}

impl Node {
    fn new() -> Self {
        Node {
            backend: Arc::new(MemoryBackend::new()),
            catalog: Arc::new(Catalog::new()),
        }
    }

    fn session(&self) -> Executor {
        Executor::new(
            Arc::clone(&self.backend) as Arc<dyn Backend>,
            Arc::clone(&self.catalog),
            1,
            esker_sql::session::register(),
        )
    }
}

fn run(executor: &mut Executor, sql: &str) -> esker_sql::Result<Outcome> {
    let mut last = Outcome::done("");
    for parsed in parse_statements(sql)? {
        last = executor.execute(&parsed, &Params::NONE)?;
    }
    Ok(last)
}

/// The row a table's primary key points at, decoded.
fn stored(node: &Node, table: &str, key: &[Datum]) -> Option<Vec<Datum>> {
    let txn = node.backend.begin().unwrap();
    let view = node.catalog.view(&*txn, 1).unwrap();
    let table = view.table(table).unwrap()?;
    let bytes = txn.get(&row::row_key(1, table.id, key).unwrap()).unwrap()?;
    // `None`: this helper asserts what is **on disk**, so it must not resolve anything on the way
    // out — a `regclass` column stores its number and no name (`debts-v1.1.md` #35), and reading
    // it back resolved here would hide exactly that.
    Some(row::decode_row(&table.row_schema(), &bytes, None).unwrap())
}

#[test]
fn a_row_lands_in_the_key_space_with_every_column_in_it() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, ok bool, blob bytea,
                         seen timestamptz, ratio double precision)",
    )
    .unwrap();

    let outcome = run(
        &mut session,
        "INSERT INTO t VALUES (1, 'ada', true, '\\xdead', '2024-02-29 12:34:56.1+00', 1.5)",
    )
    .unwrap();
    assert_eq!(outcome, Outcome::done("INSERT 0 1"));

    assert_eq!(
        stored(&node, "t", &[Datum::Int8(1)]).unwrap(),
        [
            Datum::Int8(1),
            Datum::Text("ada".into()),
            Datum::Bool(true),
            Datum::Bytea(vec![0xde, 0xad]),
            // The literal went through the same input function the value corpus checks.
            Datum::from_text(ColumnType::TimestampTz, "2024-02-29 12:34:56.1+00").unwrap(),
            Datum::Double(1.5),
        ]
    );
}

/// The tag carries a count, and the leading zero is the row OID PostgreSQL stopped assigning in
/// 8.1 and still reports.
#[test]
fn the_command_tag_counts_the_rows_and_keeps_the_legacy_oid() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    assert_eq!(
        run(&mut session, "INSERT INTO t VALUES (1), (2), (3)").unwrap(),
        Outcome::done("INSERT 0 3")
    );
    assert_eq!(
        run(&mut session, "INSERT INTO t VALUES (4)").unwrap(),
        Outcome::done("INSERT 0 1")
    );
}

/// Columns the statement did not name are NULL, including when the `VALUES` tuple is simply
/// shorter than the table.
#[test]
fn unnamed_columns_are_null() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, a text, b text)",
    )
    .unwrap();
    run(&mut session, "INSERT INTO t (id, b) VALUES (1, 'x')").unwrap();
    run(&mut session, "INSERT INTO t VALUES (2)").unwrap();

    assert_eq!(
        stored(&node, "t", &[Datum::Int8(1)]).unwrap(),
        [Datum::Int8(1), Datum::Null, Datum::Text("x".into())]
    );
    assert_eq!(
        stored(&node, "t", &[Datum::Int8(2)]).unwrap(),
        [Datum::Int8(2), Datum::Null, Datum::Null]
    );
}

/// The assignment casts a real PostgreSQL performs, and the ones it refuses. Two of these read
/// oddly and both were measured: `true` in a `text` column stores the word, not the character the
/// output function writes, and `1.5` stores the digits as written.
#[test]
fn a_literal_becomes_what_its_column_says_it_is() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, s text, d double precision)",
    )
    .unwrap();
    run(
        &mut session,
        "INSERT INTO t VALUES (1, 42, 1), (2, true, 2.5), (3, 1.5, '3.5'), (4, '5', 4)",
    )
    .unwrap();

    let text = |id: i64| match &stored(&node, "t", &[Datum::Int8(id)]).unwrap()[1] {
        Datum::Text(value) => value.clone(),
        other => panic!("not text: {other:?}"),
    };
    assert_eq!(text(1), "42", "an integer's own text");
    assert_eq!(
        text(2),
        "true",
        "the cast, not the `t` the output function writes"
    );
    assert_eq!(
        text(3),
        "1.5",
        "the digits as written, which is numeric's text"
    );
    assert_eq!(text(4), "5");

    let double = |id: i64| match stored(&node, "t", &[Datum::Int8(id)]).unwrap()[2] {
        Datum::Double(value) => value,
        ref other => panic!("not a double: {other:?}"),
    };
    assert!((double(1) - 1.0).abs() < f64::EPSILON, "an integer widens");
    assert!((double(2) - 2.5).abs() < f64::EPSILON);
    assert!(
        (double(3) - 3.5).abs() < f64::EPSILON,
        "a string literal is read"
    );
}

/// A conversion PostgreSQL would do and this crate will not: `numeric`'s rounding is half away
/// from zero and a `float8`'s is half to even, and there is no `numeric` here to be sure with. So
/// it is named, not guessed.
#[test]
fn a_decimal_literal_in_an_integer_column_is_refused_by_name() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut session, "INSERT INTO t VALUES (2.5)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("numeric literal 2.5"), "{error}");
}

#[test]
fn a_value_that_cannot_be_assigned_is_42804_with_postgresqls_hint() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, ts timestamptz)",
    )
    .unwrap();
    let error = run(&mut session, "INSERT INTO t VALUES (1, 1)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        error.to_string(),
        "column \"ts\" is of type timestamp with time zone but expression is of type integer"
    );
    assert_eq!(
        error.hint().as_deref(),
        Some("You will need to rewrite or cast the expression.")
    );
}

#[test]
fn a_null_in_a_not_null_column_is_23502_and_names_the_relation() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text NOT NULL)",
    )
    .unwrap();
    let error = run(&mut session, "INSERT INTO t (id) VALUES (1)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NOT_NULL_VIOLATION);
    assert_eq!(
        error.to_string(),
        "null value in column \"name\" of relation \"t\" violates not-null constraint"
    );

    // A primary key column is NOT NULL whether or not it said so.
    let error = run(&mut session, "INSERT INTO t (name) VALUES ('x')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NOT_NULL_VIOLATION);
    assert!(error.to_string().contains("column \"id\""), "{error}");
}

#[test]
fn a_column_the_table_does_not_have_names_the_relation_too() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut session, "INSERT INTO t (nope) VALUES (1)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    assert_eq!(
        error.to_string(),
        "column \"nope\" of relation \"t\" does not exist"
    );
}

/// PostgreSQL calls this a *syntax* error, before it has looked at any of the values.
#[test]
fn more_values_than_columns_is_a_syntax_error() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut session, "INSERT INTO t VALUES (1, 2)").unwrap_err();

    assert_eq!(error.sqlstate(), sqlstate::SYNTAX_ERROR);
    assert_eq!(
        error.to_string(),
        "INSERT has more expressions than target columns",
        "PostgreSQL says exactly this, with no `syntax error:` in front of it"
    );
}

/// The first of the two ways a duplicate arrives: it is already committed, so the read inside the
/// transaction finds it.
#[test]
fn a_committed_duplicate_is_23505_naming_the_constraint() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, email text UNIQUE)",
    )
    .unwrap();
    run(&mut session, "INSERT INTO t VALUES (1, 'a@b')").unwrap();

    let error = run(&mut session, "INSERT INTO t VALUES (1, 'other')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"t_pkey\""
    );

    let error = run(&mut session, "INSERT INTO t VALUES (2, 'a@b')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"t_email_key\""
    );
}

/// The second way, and the one the plan's ruling is about. Two sessions, two transactions, neither
/// able to see the other: both read the index key as absent, both write it, one commits. The loser
/// gets `40001` from the store, and what the *user* did was insert a duplicate — so `23505`, naming
/// the constraint, is the only answer that tells them something true.
///
/// This is `a_concurrent_duplicate_loses_at_commit` from `backend.rs`, run against the executor
/// that has to translate it.
#[test]
fn a_concurrent_duplicate_is_reported_as_a_duplicate() {
    let node = Node::new();
    let mut setup = node.session();
    run(
        &mut setup,
        "CREATE TABLE t (id int8 PRIMARY KEY, email text UNIQUE)",
    )
    .unwrap();

    let (mut left, mut right) = (node.session(), node.session());
    left.begin(false).unwrap();
    right.begin(false).unwrap();

    // Both read the index key at their own snapshot; neither sees anything.
    run(&mut left, "INSERT INTO t VALUES (1, 'a@b')").unwrap();
    run(&mut right, "INSERT INTO t VALUES (2, 'a@b')").unwrap();

    left.commit().expect("the first to commit wins");
    let loser = right.commit().expect_err("the second must lose the race");
    assert_eq!(
        loser.sqlstate(),
        sqlstate::UNIQUE_VIOLATION,
        "a lost race on a unique index key is a duplicate, not a retryable conflict"
    );
    assert_eq!(
        loser.to_string(),
        "duplicate key value violates unique constraint \"t_email_key\""
    );
}

/// The same race on the *primary key*, which has no index behind it -- the row itself is the entry.
#[test]
fn a_concurrent_duplicate_primary_key_is_also_a_duplicate() {
    let node = Node::new();
    let mut setup = node.session();
    run(&mut setup, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();

    let (mut left, mut right) = (node.session(), node.session());
    left.begin(false).unwrap();
    right.begin(false).unwrap();
    run(&mut left, "INSERT INTO t VALUES (1)").unwrap();

    // **The second insert waits now** (ADR 0057), and one thread cannot hold a row and wait for
    // it — so the wait is bounded and the answer is a real server's for a waiter that runs out of
    // `lock_timeout`. The *lesson* of this test is the one below it: a concurrent duplicate is a
    // `23505` naming the key, not the `40001` of an ordinary race, and it is asserted where the
    // race still happens — after the winner has committed.
    run(&mut right, "SET lock_timeout = '100ms'").unwrap();
    let waited = run(&mut right, "INSERT INTO t VALUES (1)").unwrap_err();
    assert_eq!(waited.sqlstate(), "55P03", "{waited}");
    right.rollback().unwrap();

    left.commit().unwrap();

    let mut after = node.session();
    let loser = run(&mut after, "INSERT INTO t VALUES (1)").unwrap_err();
    assert_eq!(loser.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert!(loser.to_string().contains("t_pkey"), "{loser}");
}

/// A lost race on a key that is *not* a unique index entry stays `40001`, and stays retryable.
/// Turning every conflict into `23505` would mislabel an ordinary race as a constraint violation,
/// which is the mistake `backend.rs` refuses to make one layer down.
///
/// Two `CREATE TABLE`s of *different* names race here: they collide on the catalog version
/// counter, which is a key nobody has a uniqueness constraint on, so the loser is told to retry.
#[test]
fn a_lost_race_on_a_key_no_constraint_covers_is_still_40001() {
    let node = Node::new();
    let (mut left, mut right) = (node.session(), node.session());
    left.begin(false).unwrap();
    right.begin(false).unwrap();
    run(&mut left, "CREATE TABLE a (id int8 PRIMARY KEY)").unwrap();
    run(&mut right, "CREATE TABLE b (id int8 PRIMARY KEY)").unwrap();

    left.commit().unwrap();
    let loser = right.commit().unwrap_err();
    assert_eq!(
        loser.sqlstate(),
        sqlstate::SERIALIZATION_FAILURE,
        "no unique index entry was written, so this really is a race and really is retryable"
    );
}

/// PostgreSQL admits any number of NULLs in a `UNIQUE` column, so those rows must not collide.
#[test]
fn many_nulls_fit_in_a_unique_column() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, email text UNIQUE)",
    )
    .unwrap();
    run(
        &mut session,
        "INSERT INTO t VALUES (1, NULL), (2, NULL), (3, NULL)",
    )
    .unwrap();
    assert!(stored(&node, "t", &[Datum::Int8(3)]).is_some());
}

/// A failed row leaves nothing behind, including the rows before it in the same statement: one
/// statement is one transaction when there is no block open.
#[test]
fn a_multi_row_insert_is_all_or_nothing() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text NOT NULL)",
    )
    .unwrap();
    run(&mut session, "INSERT INTO t VALUES (1, 'a'), (2, NULL)").unwrap_err();
    assert!(
        stored(&node, "t", &[Datum::Int8(1)]).is_none(),
        "row 1 went with row 2"
    );
}

/// The `DETAIL` field is the part of a `23505` a user reads to find out *what* collided. It is
/// rendered exactly as PostgreSQL renders it, which is to say with no quoting at all: a text value
/// containing `, y)` really does come back with unbalanced parentheses.
#[test]
fn a_unique_violation_carries_postgresqls_own_detail() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE det (a int8, b text, PRIMARY KEY (a, b))",
    )
    .unwrap();
    run(&mut session, "INSERT INTO det VALUES (1, 'x, y)')").unwrap();

    let error = run(&mut session, "INSERT INTO det VALUES (1, 'x, y)')").unwrap_err();
    assert_eq!(
        error.detail().as_deref(),
        Some("Key (a, b)=(1, x, y)) already exists.")
    );
}

/// And the same for a `NOT NULL` violation: the whole offending row, `null` for a NULL, nothing
/// quoted.
#[test]
fn a_not_null_violation_carries_the_failing_row() {
    let node = Node::new();
    let mut session = node.session();
    run(
        &mut session,
        "CREATE TABLE t (id int8 PRIMARY KEY, s text NOT NULL)",
    )
    .unwrap();
    let error = run(&mut session, "INSERT INTO t (id) VALUES (1)").unwrap_err();
    assert_eq!(
        error.detail().as_deref(),
        Some("Failing row contains (1, null).")
    );
}

/// An integer literal too large for `bigint` is three words, where the same overflow through the
/// type's input function is a longer message. Two paths, two messages.
#[test]
fn an_integer_literal_that_is_too_large_says_bigint_out_of_range() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut session, "INSERT INTO t VALUES (9223372036854775808)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
    assert_eq!(error.to_string(), "bigint out of range");

    // Through the input function instead, the message is PostgreSQL's longer one.
    let error = run(&mut session, "INSERT INTO t VALUES ('9223372036854775808')").unwrap_err();
    assert_eq!(
        error.to_string(),
        "value \"9223372036854775808\" is out of range for type bigint"
    );
}

/// A `$1` in a simple query is `42P02`: the protocol has no way to carry a parameter, so there
/// really is no parameter $1.
#[test]
fn a_parameter_in_a_simple_query_is_42p02() {
    let node = Node::new();
    let mut session = node.session();
    run(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut session, "INSERT INTO t VALUES ($1)").unwrap_err();
    assert_eq!(error.sqlstate(), "42P02");
    assert_eq!(error.to_string(), "there is no parameter $1");
}
