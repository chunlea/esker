//! `pg_prepared_statements` — **what this session has named**, through either door.
//!
//! Eight columns, measured from `\d pg_prepared_statements` on PostgreSQL 19 rather than copied
//! from documentation, and the values measured in one session beside them:
//!
//! ```text
//! PREPARE h1_probe_p1 AS SELECT $1::int + 1;
//! SELECT $1::int + 1 AS v \parse h1_wire1          -- a Parse, not a PREPARE
//! INSERT INTO h1_t VALUES ($1) \parse h1_wire3
//! PREPARE h1_ins AS INSERT INTO h1_t VALUES ($1);
//!
//!  name     | statement                                       | parameter_types | result_types | from_sql
//!  h1_ins   | PREPARE h1_ins AS INSERT INTO h1_t VALUES ($1); | {integer}       |              | t
//!  h1_wire1 | SELECT $1::int + 1 AS v                         | {integer}       | {integer}    | f
//!  h1_wire3 | INSERT INTO h1_t VALUES ($1)                    | {integer}       |              | f
//! ```
//!
//! Three things that reading are worth more than the column list:
//!
//! * **`statement` is not the same string for the two doors.** A SQL `PREPARE` reports the whole
//!   `PREPARE …` statement, semicolon and line breaks included, exactly as the client sent it; a
//!   `Parse` reports only the query string it carried.
//! * **The two type columns spell "nothing" differently.** A statement that takes no parameters
//!   has `parameter_types = {}`; one that returns no rows has `result_types` **NULL**. The
//!   `INSERT` rows above have both at once.
//! * **`DEALLOCATE ALL` clears both kinds**, which is what makes them one store rather than two:
//!   `h1_ins` and `h1_wire3` went together.
//!
//! What the corpus (`tests/discard_all.rs`) covers is the SQL door and the counts either side of
//! every `DISCARD`. What is here is the column values, the protocol door — no capture can hold a
//! `Parse`, because `psql` sends `Query` — and the one property a single session cannot show: a
//! second session's statements are not in this one's view.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

/// A session over its own store, driven the way a client drives one.
///
/// **Through `Session` and not `parity::Node`**: the statement store lives on the session, so a
/// test that drove the executor directly would watch a `PREPARE` fall through to
/// `0A000 PREPARE is not supported` however complete the implementation is (the mistake
/// `tests/prepared_sql.rs`'s header records).
struct Client {
    node: parity::Node,
    session: Session,
}

impl Client {
    fn new() -> Self {
        Client {
            node: parity::Node::new(&[
                "CREATE TABLE t (id bigint PRIMARY KEY, n int)",
                "INSERT INTO t VALUES (1, 7)",
            ]),
            session: Session::new(),
        }
    }

    fn send(&mut self, message: &Frontend) -> String {
        let mut out = Vec::new();
        self.session
            .handle(message, &mut self.node.executor, &mut out);
        answer(&out)
    }

    /// One simple query: its rows joined, or the refusal.
    fn ask(&mut self, sql: &str) -> String {
        self.send(&Frontend::Query(sql.to_owned()))
    }

    /// The view, one row per line, with `\N` for a NULL — the corpus's own spelling.
    fn view(&mut self) -> String {
        self.ask(
            "SELECT name, statement, parameter_types, result_types, from_sql, generic_plans, \
             custom_plans, prepare_time FROM pg_prepared_statements ORDER BY name",
        )
    }
}

/// The rows a reply carries, tab-joined per row and newline-joined between them.
fn answer(out: &[u8]) -> String {
    let text = String::from_utf8_lossy(out);
    if let Some(at) = text.find("ERROR") {
        let message: String = text[at..].chars().take(140).collect();
        return message
            .split('\u{0}')
            .filter(|part| !part.is_empty())
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ");
    }
    rows(out).join("\n")
}

/// Every `DataRow`'s columns, tab-joined, with `\N` for a NULL.
fn rows(out: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 5 <= out.len() {
        let len = u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
        if out[at] == b'D' {
            let body = &out[at + 5..at + 1 + len];
            let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let mut cursor = 2;
            let mut columns = Vec::with_capacity(count);
            for _ in 0..count {
                let size = i32::from_be_bytes([
                    body[cursor],
                    body[cursor + 1],
                    body[cursor + 2],
                    body[cursor + 3],
                ]);
                cursor += 4;
                if size < 0 {
                    columns.push("\\N".to_owned());
                    continue;
                }
                let size = usize::try_from(size).unwrap();
                columns.push(String::from_utf8_lossy(&body[cursor..cursor + size]).into_owned());
                cursor += size;
            }
            found.push(columns.join("\t"));
        }
        at += 1 + len;
    }
    found
}

/// Names a statement over the protocol, which is the door no capture can hold.
fn parse(client: &mut Client, name: &str, sql: &str) -> String {
    client.send(&Frontend::Parse {
        statement: name.to_owned(),
        sql: sql.to_owned(),
        param_types: Vec::new(),
    })
}

#[test]
fn a_sql_prepare_is_reported_with_the_whole_statement_that_made_it() {
    let mut client = Client::new();
    assert_eq!(
        client.ask("PREPARE p1 AS SELECT n FROM t WHERE id = $1"),
        ""
    );
    // The `statement` column is the text the client sent, verbatim: the `PREPARE` and all. That is
    // what a real server reports and it is why a client can tell the two doors apart by reading it.
    // `prepare_time` is NULL here where a real server has a timestamp — no wall clock is read for
    // anything a client can order by, which is why every timestamp in `pg_stat_activity` and
    // `pg_locks.waitstart` is NULL too.
    assert_eq!(
        client.view(),
        "p1\tPREPARE p1 AS SELECT n FROM t WHERE id = $1\t{bigint}\t{integer}\tt\t0\t0\t\\N"
    );
}

#[test]
fn a_statement_that_returns_no_rows_has_null_result_types_and_not_an_empty_array() {
    let mut client = Client::new();
    assert_eq!(
        client.ask("PREPARE ins AS INSERT INTO t VALUES ($1, 2)"),
        ""
    );
    // Measured on PostgreSQL 19: `{integer}` beside a NULL, in one row. The two columns mean
    // different things by "nothing" and a client reading `result_types IS NULL` is asking whether
    // the statement returns rows at all.
    assert_eq!(
        client.view(),
        "ins\tPREPARE ins AS INSERT INTO t VALUES ($1, 2)\t{bigint}\t\\N\tt\t0\t0\t\\N"
    );
}

#[test]
fn a_statement_that_takes_no_parameters_has_an_empty_array_and_not_a_null() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE none AS SELECT 1"), "");
    assert!(
        client.view().contains("\t{}\t"),
        "parameter_types must be an empty array: {}",
        client.view()
    );
}

#[test]
fn a_statement_the_protocol_named_is_not_from_sql_and_reports_only_its_query() {
    let mut client = Client::new();
    assert_eq!(
        parse(&mut client, "w1", "SELECT n FROM t WHERE id = $1"),
        ""
    );
    // A `Describe` is what resolves a `Parse`'s types on this node — it is not analysed before
    // something asks — so the row is read after one, which is what every driver sends anyway.
    client.send(&Frontend::Describe {
        target: Target::Statement,
        name: "w1".to_owned(),
    });
    assert_eq!(
        client.view(),
        "w1\tSELECT n FROM t WHERE id = $1\t{bigint}\t{integer}\tf\t0\t0\t\\N"
    );
}

#[test]
fn the_two_doors_are_one_store_and_deallocate_all_clears_both() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE p1 AS SELECT 1"), "");
    assert_eq!(parse(&mut client, "w1", "SELECT 2"), "");
    assert_eq!(
        client.ask("SELECT count(*) FROM pg_prepared_statements"),
        "2"
    );
    assert_eq!(client.ask("DEALLOCATE ALL"), "");
    assert_eq!(
        client.ask("SELECT count(*) FROM pg_prepared_statements"),
        "0"
    );
}

#[test]
fn discard_all_clears_the_view_and_discard_plans_leaves_it_alone() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE p1 AS SELECT 1"), "");
    assert_eq!(client.ask("DISCARD PLANS"), "");
    // `DISCARD PLANS` throws away cached plans and keeps the statements — measured, and the
    // corpus replays the whole ladder of them (`tests/discard_all.rs`).
    assert_eq!(
        client.ask("SELECT count(*) FROM pg_prepared_statements"),
        "1"
    );
    assert_eq!(client.ask("DISCARD ALL"), "");
    assert_eq!(
        client.ask("SELECT count(*) FROM pg_prepared_statements"),
        "0"
    );
}

#[test]
fn a_second_session_s_statements_are_not_in_this_one_s_view() {
    // **The property a single session cannot show, and the one that separates this view from
    // `pg_stat_activity` beside it.** That one is cluster-wide on a real server and reports every
    // backend; this one is per-backend and reports the asking session only. Two sessions on one
    // store is the shape that would catch a view reading a process-wide registry.
    let mut first = Client::new();
    let mut second = Client::new();
    assert_eq!(first.ask("PREPARE mine AS SELECT 1"), "");
    assert_eq!(second.ask("PREPARE yours AS SELECT 2"), "");
    assert_eq!(
        first.ask("SELECT name FROM pg_prepared_statements ORDER BY name"),
        "mine"
    );
    assert_eq!(
        second.ask("SELECT name FROM pg_prepared_statements ORDER BY name"),
        "yours"
    );
}

#[test]
fn the_view_is_empty_on_a_session_that_has_prepared_nothing() {
    let mut client = Client::new();
    assert_eq!(
        client.ask("SELECT count(*) FROM pg_prepared_statements"),
        "0"
    );
}
