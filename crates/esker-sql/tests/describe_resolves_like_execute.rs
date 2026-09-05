//! **`Describe` must resolve a name exactly as `Execute` will**, or the client is told about a
//! different statement from the one that runs.
//!
//! One defect, two symptoms, both from `schema_authorization_test.rb` — where the table lives in
//! the session user's own schema and `search_path` is `'$user',public`. `Executor::tables_for`
//! looked names up in the catalog *directly* while every execution path resolves them along the
//! search path, so under `SET SESSION AUTHORIZATION` the describe pass simply did not find the
//! table, and each caller of that empty list failed in its own way.
//!
//! `schema_authorization_test.rb`'s `test_sequence_schema_caching` died with
//! `PG::UnableToSend: server sent data ("D" message) without prior row description ("T" message)`
//! — the only occurrence in a 426-file pass, and worse than one failed test: libpq stops parsing
//! the stream, so the connection is lost rather than the statement. r1 saw the same shape around
//! run 60.
//!
//! The rule libpq enforces is a property of the **bytes**, not of any one statement, so that is
//! what is asserted here: every `D` in a response has a `T` before it. Written this way the test
//! does not depend on guessing which statement desynchronises — it fails on whichever one does.
//!
//! The second symptom is `test_auth_with_bind`'s `operator does not exist: integer = text`. The
//! same empty list is what `bind::infer` types parameters against, so `WHERE id = $1` had no
//! column to type `$1` from and fell back to text. It is one fix, and both tests are here so that
//! a future change which re-separates the two paths fails on whichever symptom it produces
//! first.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

/// The message tags in a response, in order.
///
/// Every backend message is `tag ++ len:u32 ++ body`, and the length counts itself. A stream that
/// does not decode cleanly is itself the defect this file is about, so a short tail panics rather
/// than being skipped.
fn tags(out: &[u8]) -> String {
    let mut tags = String::new();
    let mut at = 0;
    while at < out.len() {
        assert!(at + 5 <= out.len(), "a truncated message at byte {at}");
        let len = u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
        tags.push(char::from(out[at]));
        at += 1 + len;
    }
    assert_eq!(at, out.len(), "a message ran past the end of the buffer");
    tags
}

/// Panics unless every `D` in `stream` has a `T` (or a `t`-less `D` is impossible) before it.
///
/// `ReadyForQuery` (`Z`) ends a batch and clears the expectation, which is what a client does too.
fn assert_no_data_before_description(stream: &str, what: &str) {
    let mut described = false;
    for tag in stream.chars() {
        match tag {
            'T' => described = true,
            'Z' => described = false,
            'D' => assert!(
                described,
                "a DataRow with no RowDescription before it, in {what}: {stream}"
            ),
            _ => {}
        }
    }
}

const USER: &str = "rails_pg_schema_user1";

/// The statement sequence of `test_sequence_schema_caching`, over the extended protocol.
///
/// `SchemaThing` has a `serial` primary key, so both saves are `INSERT … RETURNING "id"` — a
/// command that *does* return a row, which is the pairing that can go wrong: a `Describe` that
/// answers `NoData` and an `Execute` that then produces one.
#[test]
fn an_insert_returning_describes_its_row_before_sending_it() {
    let mut node = parity::Node::new(&[
        &format!("CREATE USER {USER}"),
        &format!("CREATE SCHEMA AUTHORIZATION {USER}"),
        "SET search_path TO '$user',public",
        &format!("SET SESSION AUTHORIZATION {USER}"),
        "CREATE TABLE schema_things (id serial primary key, name character varying(50))",
        &format!("INSERT INTO schema_things (name) VALUES ('{USER}')"),
        "SET SESSION AUTHORIZATION DEFAULT",
    ]);
    let mut session = Session::new();

    let mut run = |message: Frontend, node: &mut parity::Node| {
        let mut out = Vec::new();
        session.handle(&message, &mut node.executor, &mut out);
        tags(&out)
    };

    // The loop body of the test: become the user, then save twice.
    let mut stream = String::new();
    stream.push_str(&run(
        Frontend::Query(format!("SET SESSION AUTHORIZATION {USER}")),
        &mut node,
    ));

    for (sql, params) in [
        (
            r#"INSERT INTO schema_things (name) VALUES ($1) RETURNING "id""#,
            vec![Some(b"TEST1".to_vec())],
        ),
        (
            r#"INSERT INTO schema_things (id, name) VALUES ($1, $2) RETURNING "id""#,
            vec![Some(b"5".to_vec()), Some(b"TEST2".to_vec())],
        ),
    ] {
        stream.push_str(&run(
            Frontend::Parse {
                statement: "s1".to_owned(),
                sql: sql.to_owned(),
                param_types: Vec::new(),
            },
            &mut node,
        ));
        stream.push_str(&run(
            Frontend::Describe {
                target: Target::Statement,
                name: "s1".to_owned(),
            },
            &mut node,
        ));
        stream.push_str(&run(
            Frontend::Bind {
                portal: String::new(),
                statement: "s1".to_owned(),
                param_formats: Vec::new(),
                params,
                result_formats: Vec::new(),
            },
            &mut node,
        ));
        stream.push_str(&run(
            Frontend::Describe {
                target: Target::Portal,
                name: String::new(),
            },
            &mut node,
        ));
        stream.push_str(&run(
            Frontend::Execute {
                portal: String::new(),
                max_rows: 0,
            },
            &mut node,
        ));
        stream.push_str(&run(Frontend::Sync, &mut node));
        stream.push_str(&run(
            Frontend::Close {
                target: Target::Statement,
                name: "s1".to_owned(),
            },
            &mut node,
        ));
    }

    assert_no_data_before_description(&stream, "test_sequence_schema_caching");
}

/// **A `$1` is typed from the column it is compared with, in whatever schema that column lives.**
///
/// `test_auth_with_bind`, which is `select_value("SELECT name FROM schema_things WHERE id = $1")`
/// with an integer bind under `SET SESSION AUTHORIZATION`. The parameter's type is inferred from
/// the other operand, so an unresolvable table left `$1` as text and the comparison became
/// `operator does not exist: integer = text` — a message about the *statement* for what was really
/// a describe that could not see the table.
///
/// Asserted through the wire, not through the executor, because the inference happens in the
/// `Parse`/`Describe` pass and the `ParameterDescription` is where a client reads it.
#[test]
fn a_bind_parameter_is_typed_from_a_column_in_the_session_s_own_schema() {
    let mut node = parity::Node::new(&[
        &format!("CREATE USER {USER}"),
        &format!("CREATE SCHEMA AUTHORIZATION {USER}"),
        "SET search_path TO '$user',public",
        &format!("SET SESSION AUTHORIZATION {USER}"),
        "CREATE TABLE schema_things (id serial primary key, name character varying(50))",
        &format!("INSERT INTO schema_things (name) VALUES ('{USER}')"),
    ]);
    let mut session = Session::new();
    let mut out = Vec::new();
    for message in [
        Frontend::Parse {
            statement: "b1".to_owned(),
            sql: "SELECT name FROM schema_things WHERE id = $1".to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Describe {
            target: Target::Statement,
            name: "b1".to_owned(),
        },
        Frontend::Bind {
            portal: String::new(),
            statement: "b1".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(b"1".to_vec())],
            result_formats: Vec::new(),
        },
        Frontend::Execute {
            portal: String::new(),
            max_rows: 0,
        },
        Frontend::Sync,
    ] {
        session.handle(&message, &mut node.executor, &mut out);
    }

    let stream = tags(&out);
    assert!(
        !stream.contains('E'),
        "no statement here should fail: {stream}"
    );
    assert_no_data_before_description(&stream, "test_auth_with_bind");
    // `1 t T 2 D C Z` — parsed, parameters described, rows described, bound, one row, complete.
    assert_eq!(stream, "1tT2DCZ", "the whole exchange: {stream}");
}
