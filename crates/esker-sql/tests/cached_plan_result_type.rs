//! **A prepared statement whose result type changed underneath it must refuse, not answer.**
//!
//! `ActiveRecord` maps `0A000 cached plan must not change result type` to
//! `PreparedStatementCacheExpired`, deallocates the statement and retries — so a node that
//! silently answers with the *new* shape hands the adapter rows its column list does not match,
//! and two suite tests that assert the error see nothing raised at all (run 102's triage).
//!
//! Measured on PostgreSQL 19, `VERBOSITY verbose`, one session:
//!
//! ```text
//! PREPARE r1 AS SELECT * FROM t;  EXECUTE r1;      -> rows
//! ALTER TABLE t ADD COLUMN c int; EXECUTE r1;      0A000: cached plan must not change result type
//! ALTER TABLE t RENAME COLUMN b TO bb; EXECUTE r1; 0A000: the same -- a name is part of the type
//! ALTER TABLE t ALTER COLUMN a TYPE bigint;        0A000: the same
//! PREPARE p2 AS SELECT a FROM t; ADD COLUMN c;     -> rows, unchanged: the column it names did not
//! PREPARE p3 AS SELECT b FROM t; DROP COLUMN b;    42703: column "b" does not exist
//! PREPARE d1 AS SELECT a FROM t; DROP TABLE t;     42P01: relation "t" does not exist
//! SELECT * FROM t \parse w1; ADD COLUMN c; \bind_named w1  0A000: the protocol door, the same
//! ```
//!
//! Four facts in that: **a rename counts**, so the comparison is over names as well as types; the
//! error does **not** heal itself, both `EXECUTE`s after the `ALTER` raise it and only a re-`PREPARE`
//! clears it; a statement that re-analyses *badly* reports the analysis error instead, which falls
//! out of doing the comparison second; and the extended protocol behaves exactly as SQL `PREPARE`
//! does, which is the door `ActiveRecord` is on.

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
            node: parity::Node::new(&[
                "CREATE TABLE t (a int, b int)",
                "INSERT INTO t VALUES (1, 2)",
            ]),
            session: Session::new(),
        }
    }

    fn send(&mut self, message: &Frontend) -> String {
        let mut out = Vec::new();
        self.session
            .handle(message, &mut self.node.executor, &mut out);
        read(&out)
    }

    /// The refusal, or the rows joined, or the empty string for a command.
    fn ask(&mut self, sql: &str) -> String {
        self.send(&Frontend::Query(sql.to_owned()))
    }

    /// Names a statement over the protocol, which is the door no capture can hold.
    fn parse(&mut self, name: &str, sql: &str) -> String {
        self.send(&Frontend::Parse {
            statement: name.to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        })
    }

    /// Asks a statement's shape, which is what gives it a baseline to be compared against.
    fn describe(&mut self, name: &str) -> String {
        self.send(&Frontend::Describe {
            target: Target::Statement,
            name: name.to_owned(),
        })
    }

    /// `Bind` then `Execute` with no parameters: one run of a named statement.
    fn run(&mut self, name: &str) -> String {
        let bound = self.send(&Frontend::Bind {
            portal: name.to_owned(),
            statement: name.to_owned(),
            param_formats: Vec::new(),
            params: Vec::new(),
            result_formats: Vec::new(),
        });
        if bound.starts_with("ERROR") {
            return bound;
        }
        self.send(&Frontend::Execute {
            portal: name.to_owned(),
            max_rows: 0,
        })
    }
}

/// One reply: the `ErrorResponse`'s sentence, or the rows.
///
/// The `M` field and nothing else — that sentence is what `ActiveRecord` matches on, and
/// comparing the whole frame would be comparing the severity and PostgreSQL's source line too.
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
        return format!("ERROR {message}");
    }
    rows(out).join("|")
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

#[test]
fn a_new_column_under_a_starred_statement_is_a_changed_result_type() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE r1 AS SELECT * FROM t"), "");
    assert_eq!(client.ask("EXECUTE r1"), "1\t2");
    assert_eq!(client.ask("ALTER TABLE t ADD COLUMN c int"), "");
    assert_eq!(
        client.ask("EXECUTE r1"),
        "ERROR cached plan must not change result type"
    );
    // It does not heal: the statement stays broken until it is prepared again, which is what the
    // adapter's `PreparedStatementCacheExpired` handler does.
    assert_eq!(
        client.ask("EXECUTE r1"),
        "ERROR cached plan must not change result type"
    );
    assert_eq!(client.ask("DEALLOCATE r1"), "");
    assert_eq!(client.ask("PREPARE r1 AS SELECT * FROM t"), "");
    assert_eq!(client.ask("EXECUTE r1"), "1\t2\t\\N");
}

#[test]
fn a_rename_is_a_changed_result_type_because_a_name_is_part_of_it() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE r1 AS SELECT * FROM t"), "");
    assert_eq!(client.ask("EXECUTE r1"), "1\t2");
    assert_eq!(client.ask("ALTER TABLE t RENAME COLUMN b TO bb"), "");
    assert_eq!(
        client.ask("EXECUTE r1"),
        "ERROR cached plan must not change result type"
    );
}

#[test]
fn a_widened_column_is_a_changed_result_type() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE r1 AS SELECT a FROM t"), "");
    assert_eq!(client.ask("EXECUTE r1"), "1");
    assert_eq!(client.ask("ALTER TABLE t ALTER COLUMN a TYPE bigint"), "");
    assert_eq!(
        client.ask("EXECUTE r1"),
        "ERROR cached plan must not change result type"
    );
}

#[test]
fn a_column_the_statement_does_not_name_may_be_added_freely() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE p2 AS SELECT a FROM t"), "");
    assert_eq!(client.ask("ALTER TABLE t ADD COLUMN c int"), "");
    // Nothing about `SELECT a` changed, so nothing is refused. This is the half a comparison over
    // the *table* rather than the statement's own row type would get wrong.
    assert_eq!(client.ask("EXECUTE p2"), "1");
}

#[test]
fn a_statement_that_no_longer_analyses_reports_that_and_not_the_cached_plan() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE p3 AS SELECT b FROM t"), "");
    assert_eq!(client.ask("ALTER TABLE t DROP COLUMN b"), "");
    // `42703`, not `0A000`: the comparison is second, because there is nothing to compare when the
    // statement cannot be analysed at all.
    assert_eq!(
        client.ask("EXECUTE p3"),
        "ERROR column \"b\" does not exist"
    );
}

#[test]
fn a_widened_type_modifier_is_a_changed_result_type() {
    let mut client = Client::new();
    assert_eq!(client.ask("CREATE TABLE v (a varchar(10))"), "");
    assert_eq!(client.ask("INSERT INTO v VALUES ('x')"), "");
    assert_eq!(client.ask("PREPARE tm AS SELECT a FROM v"), "");
    assert_eq!(client.ask("EXECUTE tm"), "x");
    assert_eq!(
        client.ask("ALTER TABLE v ALTER COLUMN a TYPE varchar(20)"),
        ""
    );
    // Measured: the type OID did not move and PostgreSQL still refuses. A comparison over OIDs
    // alone would let this one through, which is why the modifier is in it.
    assert_eq!(
        client.ask("EXECUTE tm"),
        "ERROR cached plan must not change result type"
    );
}

/// **The protocol door, which is the one `ActiveRecord` is on.**
///
/// A `Parse`d statement, described once and then bound and executed either side of an `ALTER` —
/// the shape a pooled adapter produces when a migration runs under it. Measured the same way on
/// PostgreSQL 19 with `\parse` and `\bind_named`, and it is `0A000` there exactly as `EXECUTE` is.
#[test]
fn a_statement_the_protocol_named_is_revalidated_too() {
    let mut client = Client::new();
    assert_eq!(client.parse("w1", "SELECT * FROM t"), "");
    client.describe("w1");
    assert_eq!(client.run("w1"), "1\t2");
    assert_eq!(client.ask("ALTER TABLE t ADD COLUMN c int"), "");
    assert_eq!(
        client.run("w1"),
        "ERROR cached plan must not change result type"
    );
}

/// A `Parse` nobody described has no shape to compare, and is let through rather than guessed at.
///
/// **The honest gap, written down.** PostgreSQL analyses at `Parse` and refuses here too; this
/// node analyses when something asks, so a statement nobody asked about has no baseline. Every
/// driver that reads rows sends the `Describe` first, which is why the gap is reachable only by a
/// client that binds a `SELECT` without ever asking its shape. Closing it means analysing every
/// `Parse`, which moves when errors are raised — a behaviour change, and not this unit's.
#[test]
fn a_statement_nobody_described_has_no_baseline_to_compare() {
    let mut client = Client::new();
    assert_eq!(client.parse("w2", "SELECT * FROM t"), "");
    assert_eq!(client.run("w2"), "1\t2");
    assert_eq!(client.ask("ALTER TABLE t ADD COLUMN c int"), "");
    assert_eq!(client.run("w2"), "1\t2\t\\N");
}

#[test]
fn a_dropped_table_under_a_prepared_statement_is_reported_as_a_missing_relation() {
    let mut client = Client::new();
    assert_eq!(client.ask("PREPARE d1 AS SELECT a FROM t"), "");
    assert_eq!(client.ask("DROP TABLE t"), "");
    assert_eq!(
        client.ask("EXECUTE d1"),
        "ERROR relation \"t\" does not exist"
    );
}
