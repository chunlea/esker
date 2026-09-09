//! `EXPLAIN [(options)] EXECUTE name(args)` — **explaining a statement the session holds.**
//!
//! `connection_test.rb`'s `test_statement_key_is_logged` names a statement with `PQprepare`, reads
//! the name out of the log payload, and then asks
//! `EXPLAIN (FORMAT JSON) EXECUTE <that name>(1)`. The node answered
//! `0A000 EXECUTE is not supported`: `EXPLAIN (FORMAT JSON) SELECT 1` worked, SQL `EXECUTE` worked,
//! and only the two together did not — the lowering of an `EXPLAIN` lowers the statement inside it,
//! and the statement inside this one lives in the session's store where the executor cannot see it.
//!
//! Measured on PostgreSQL 19 through the `pg` gem, in one `BEGIN … ROLLBACK` with a savepoint
//! around each probe so a refusal did not swallow the ones after it:
//!
//! ```text
//! EXPLAIN EXECUTE p1(1)                  ["QUERY PLAN"]  Result  (cost=0.00..0.01 rows=1 width=4)
//! EXPLAIN (FORMAT JSON) EXECUTE p1(1)    ["QUERY PLAN"]  [ { "Plan": { "Node Type": "Result", …
//! EXPLAIN (FORMAT JSON) EXECUTE a1(1)    ["QUERY PLAN"]  the same — `a1` came from PQprepare
//! EXPLAIN (ANALYZE) EXECUTE p1(1)        ["QUERY PLAN"]  … (actual time=…)
//! EXPLAIN EXECUTE p1                     42601: wrong number of parameters for prepared statement "p1"
//! EXPLAIN (FORMAT JSON) EXECUTE nosuch(1) 26000: prepared statement "nosuch" does not exist
//! ```
//!
//! **Every refusal is the one a bare `EXECUTE` gives**, which is why the resolution is the same
//! function: the name is folded the same way, looked up in the same store, and checked against the
//! same argument count. Two lookups would be two chances to fold a quoted name differently.
//!
//! And `a1` is the point of the third line: the statement was named by the **protocol**, and SQL
//! `EXPLAIN … EXECUTE` finds it. One store, two doors, now at a third statement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

struct Client {
    node: parity::Node,
    session: Session,
}

impl Client {
    fn new() -> Self {
        Client {
            node: parity::Node::new(&["CREATE TABLE t (id int)", "INSERT INTO t VALUES (1)"]),
            session: Session::new(),
        }
    }

    fn send(&mut self, message: &Frontend) -> String {
        let mut out = Vec::new();
        self.session
            .handle(message, &mut self.node.executor, &mut out);
        read(&out)
    }

    fn ask(&mut self, sql: &str) -> String {
        self.send(&Frontend::Query(sql.to_owned()))
    }

    /// The column names of the last reply, from its `RowDescription`.
    fn columns(&mut self, sql: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.session.handle(
            &Frontend::Query(sql.to_owned()),
            &mut self.node.executor,
            &mut out,
        );
        let mut names = Vec::new();
        let mut at = 0;
        while at + 5 <= out.len() {
            let len =
                u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
            if out[at] == b'T' {
                let body = &out[at + 5..at + 1 + len];
                let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let mut cursor = 2;
                for _ in 0..count {
                    let end = body[cursor..].iter().position(|byte| *byte == 0).unwrap() + cursor;
                    names.push(String::from_utf8_lossy(&body[cursor..end]).into_owned());
                    cursor = end + 1 + 18;
                }
            }
            at += 1 + len;
        }
        names
    }
}

/// The rows joined, or the refusal's own sentence.
fn read(out: &[u8]) -> String {
    let text = String::from_utf8_lossy(out);
    let parts: Vec<&str> = text.split('\u{0}').collect();
    if parts
        .iter()
        .any(|part| *part == "SERROR" || *part == "VERROR")
    {
        let message = parts
            .iter()
            .find(|part| part.starts_with('M'))
            .map_or("", |part| &part[1..]);
        return format!("!{message}");
    }
    rows(out).join("\n")
}

fn rows(out: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 5 <= out.len() {
        let len = u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
        if out[at] == b'D' {
            let body = &out[at + 5..at + 1 + len];
            let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let mut cursor = 2;
            for _ in 0..count {
                let size = i32::from_be_bytes([
                    body[cursor],
                    body[cursor + 1],
                    body[cursor + 2],
                    body[cursor + 3],
                ]);
                cursor += 4;
                if size < 0 {
                    found.push("\\N".to_owned());
                    continue;
                }
                let size = usize::try_from(size).unwrap();
                found.push(String::from_utf8_lossy(&body[cursor..cursor + size]).into_owned());
                cursor += size;
            }
        }
        at += 1 + len;
    }
    found
}

#[test]
fn a_sql_prepared_statement_can_be_explained() {
    let mut client = Client::new();
    assert_eq!(
        client.ask("PREPARE p1 AS SELECT id FROM t WHERE id = $1"),
        ""
    );
    assert_eq!(client.columns("EXPLAIN EXECUTE p1(1)"), vec!["QUERY PLAN"]);
    let plan = client.ask("EXPLAIN EXECUTE p1(1)");
    assert!(!plan.is_empty() && !plan.starts_with('!'), "{plan}");
}

/// **The statement the suite sends**, and the door it came in by: `a1` was named by a `Parse`
/// message, not by SQL, and the SQL `EXPLAIN … EXECUTE` finds it anyway.
#[test]
fn a_statement_the_protocol_named_can_be_explained_in_json() {
    let mut client = Client::new();
    assert_eq!(
        client.send(&Frontend::Parse {
            statement: "a1".to_owned(),
            sql: "SELECT $1::integer".to_owned(),
            param_types: Vec::new(),
        }),
        ""
    );
    client.send(&Frontend::Describe {
        target: Target::Statement,
        name: "a1".to_owned(),
    });
    assert_eq!(
        client.columns("EXPLAIN (FORMAT JSON) EXECUTE a1(1)"),
        vec!["QUERY PLAN"]
    );
    let plan = client.ask("EXPLAIN (FORMAT JSON) EXECUTE a1(1)");
    // What `res.column_types["QUERY PLAN"].deserialize` is handed, and the test asserts its length
    // is above zero — so it has to be a JSON array with something in it.
    assert!(plan.starts_with('['), "not a JSON document: {plan}");
    assert!(plan.len() > 2, "an empty JSON document: {plan}");
}

/// Every refusal is `EXECUTE`'s own, which is what one shared resolution buys.
#[test]
fn the_refusals_are_the_ones_execute_gives() {
    let mut client = Client::new();
    assert_eq!(
        client.ask("PREPARE p1 AS SELECT id FROM t WHERE id = $1"),
        ""
    );
    assert_eq!(
        client.ask("EXPLAIN (FORMAT JSON) EXECUTE nosuch(1)"),
        "!prepared statement \"nosuch\" does not exist"
    );
    assert_eq!(
        client.ask("EXPLAIN EXECUTE p1"),
        "!wrong number of parameters for prepared statement \"p1\""
    );
}

/// A quoted name is one name in both statements, which is only true while one function folds it.
#[test]
fn a_quoted_name_is_found_by_explain_too() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE \"Q\" AS SELECT 1"), "");
    assert_eq!(client.columns("EXPLAIN EXECUTE \"Q\""), vec!["QUERY PLAN"]);
    // Unquoted folds to lower case and names nothing, exactly as it does for `EXECUTE`.
    assert_eq!(
        client.ask("EXPLAIN EXECUTE Q"),
        "!prepared statement \"q\" does not exist"
    );
}
