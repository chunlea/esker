//! SQL-level `PREPARE` / `EXECUTE` / `DEALLOCATE`, which are not the protocol's named statements.
//!
//! They share one store — `pgwire::Session`'s — because that is what PostgreSQL does: `DISCARD ALL`
//! clears both, and `pg_prepared_statements` reports both with `from_sql` telling them apart.
//!
//! Measured on PostgreSQL 19beta1 in one rolled-back session, with `VERBOSITY verbose`:
//!
//! ```text
//! PREPARE p AS SELECT n FROM t WHERE id = $1;   -> PREPARE
//! PREPARE p (int8) AS …;                        -> PREPARE      (a declared type list)
//! EXECUTE p(1);                                 -> the statement's own rows and tag
//! EXECUTE nope(1);      26000: prepared statement "nope" does not exist
//! PREPARE p AS SELECT 2; 42P05: prepared statement "p" already exists
//! EXECUTE p(1, 2);      42601: wrong number of parameters for prepared statement "p"
//!                       DETAIL:  Expected 1 parameters but got 2.
//! EXECUTE p;            the same sentence, with "but got 0."
//! DEALLOCATE nope;      26000: prepared statement "nope" does not exist
//! DEALLOCATE p;         -> DEALLOCATE
//! DEALLOCATE ALL;       -> DEALLOCATE ALL      (its own tag)
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::Frontend;
use esker_sql::pgwire::session::Session;

/// One simple query **through a `Session`**, which is where the statement store lives.
///
/// `parity::Node::run` drives the `Executor` directly and never enters `Session::run_one`, so a
/// test written against it sees `PREPARE` fall through to the executor and answer
/// `0A000 PREPARE is not supported` however complete the implementation is. That is the same
/// mistake as v50's: a test on a path the feature does not live on.
struct Client {
    node: parity::Node,
    session: Session,
}

impl Client {
    fn new() -> Self {
        Client {
            node: node(),
            session: Session::new(),
        }
    }

    /// The refusal, or the rows joined by tabs, or the empty string for a command.
    fn ask(&mut self, sql: &str) -> String {
        let mut out = Vec::new();
        self.session.handle(
            &Frontend::Query(sql.to_owned()),
            &mut self.node.executor,
            &mut out,
        );
        let text = String::from_utf8_lossy(&out);
        if let Some(at) = text.find("ERROR") {
            let message: String = text[at..].chars().take(120).collect();
            return message
                .split('\u{0}')
                .filter(|part| !part.is_empty())
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
        }
        rows(&out).join("|")
    }
}

/// Every `DataRow`'s columns, tab-joined.
fn rows(out: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 5 <= out.len() {
        let len = u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
        if out[at] == b'D' {
            let body = &out[at + 5..at + 1 + len];
            let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let mut cursor = 2;
            let mut columns = Vec::new();
            for _ in 0..count {
                let width = i32::from_be_bytes([
                    body[cursor],
                    body[cursor + 1],
                    body[cursor + 2],
                    body[cursor + 3],
                ]);
                cursor += 4;
                if width < 0 {
                    columns.push(String::new());
                } else {
                    let width = usize::try_from(width).unwrap();
                    columns
                        .push(String::from_utf8_lossy(&body[cursor..cursor + width]).into_owned());
                    cursor += width;
                }
            }
            found.push(columns.join("\t"));
        }
        at += 1 + len;
    }
    found
}

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE h1p (id bigint primary key, n bigint)",
        "INSERT INTO h1p VALUES (1, 10), (2, 20)",
    ])
}

/// **A prepared statement runs, and its arguments are typed like a `Bind`'s.**
#[test]
fn a_prepared_statement_runs_with_its_arguments() {
    let mut c = Client::new();
    assert_eq!(c.ask("PREPARE h1_p AS SELECT n FROM h1p WHERE id = $1"), "");
    assert_eq!(
        c.ask("EXECUTE h1_p(2)"),
        "20",
        "the argument is bound to $1 and typed from the column beside it"
    );
    // Twice, because running a prepared statement does not consume it.
    assert_eq!(c.ask("EXECUTE h1_p(1)"), "10");
}

/// **A declared type list parses**, which is the only claim the corpus makes about it:
/// `pg19.sql:428` is the one parameterised declaration in the whole capture.
#[test]
fn a_declared_type_list_is_accepted() {
    let mut c = Client::new();
    assert_eq!(
        c.ask("PREPARE h1_t (int8) AS SELECT n FROM h1p WHERE id = $1"),
        ""
    );
    assert_eq!(c.ask("EXECUTE h1_t(1)"), "10");
}

/// **A negative literal is a literal**, and `sqlparser` reads it as a unary minus over a number.
///
/// `EXECUTE cc(-2, 1)` is in the corpus twice; matching `Expr::Value` alone refuses it.
#[test]
fn a_negative_argument_is_an_argument() {
    let mut c = Client::new();
    assert_eq!(
        c.ask("PREPARE h1_neg AS UPDATE h1p SET n = n + $1 WHERE id = $2"),
        ""
    );
    assert_eq!(c.ask("EXECUTE h1_neg(-2, 1)"), "");
    assert_eq!(c.ask("SELECT n FROM h1p WHERE id = 1"), "8");
}

/// A statement that **writes** runs through `EXECUTE` too — the counter-cache `UPDATE`
/// `integer_plus_text`'s capture prepares.
#[test]
fn a_prepared_update_writes() {
    let mut c = Client::new();
    assert_eq!(
        c.ask("PREPARE h1_up AS UPDATE h1p SET n = $1 WHERE id = $2"),
        ""
    );
    assert_eq!(c.ask("EXECUTE h1_up(99, 2)"), "");
    assert_eq!(c.ask("SELECT n FROM h1p WHERE id = 2"), "99");
}

/// The five refusals, each with PostgreSQL's own code and sentence.
#[test]
fn the_refusals_are_postgresql_s() {
    let mut c = Client::new();
    assert_eq!(c.ask("PREPARE h1_r AS SELECT n FROM h1p WHERE id = $1"), "");

    for (sql, wanted) in [
        (
            "EXECUTE h1_missing(1)",
            r#"prepared statement "h1_missing" does not exist"#,
        ),
        (
            "DEALLOCATE h1_missing",
            r#"prepared statement "h1_missing" does not exist"#,
        ),
        (
            "PREPARE h1_r AS SELECT 2",
            r#"prepared statement "h1_r" already exists"#,
        ),
        (
            "EXECUTE h1_r(1, 2)",
            r#"wrong number of parameters for prepared statement "h1_r""#,
        ),
        ("EXECUTE h1_r", "Expected 1 parameters but got 0."),
    ] {
        let answered = c.ask(sql);
        assert!(answered.contains(wanted), "{sql} -> {answered}");
    }
    // And the codes, measured: 26000, 42P05, 42601.
    assert!(c.ask("EXECUTE h1_missing(1)").contains("26000"));
    assert!(c.ask("PREPARE h1_r AS SELECT 2").contains("42P05"));
    assert!(c.ask("EXECUTE h1_r(1, 2)").contains("42601"));
    assert!(
        c.ask("EXECUTE h1_r(1, 2)")
            .contains("Expected 1 parameters but got 2."),
        "the DETAIL is PostgreSQL's own"
    );
}

/// **`DEALLOCATE ALL` takes the rest**, and each name goes on its own.
#[test]
fn deallocate_drops_one_or_all() {
    let mut c = Client::new();
    assert_eq!(c.ask("PREPARE h1_a AS SELECT 1"), "");
    assert_eq!(c.ask("PREPARE h1_b AS SELECT 2"), "");

    assert_eq!(c.ask("DEALLOCATE h1_a"), "");
    assert!(
        c.ask("EXECUTE h1_a").contains("does not exist"),
        "the one that was dropped is gone"
    );
    assert_eq!(c.ask("EXECUTE h1_b"), "2");

    assert_eq!(c.ask("DEALLOCATE ALL"), "");
    assert!(
        c.ask("EXECUTE h1_b").contains("does not exist"),
        "ALL takes the rest"
    );
}

/// **`PREPARE TRANSACTION` is two-phase commit and stays refused.**
///
/// The trap: it shares a leading keyword with `PREPARE` and nothing else. The guard is the
/// `UNSUPPORTED` word table, which refuses it before `sqlparser` produces an AST at all — so it is
/// not a case in the classifier and cannot be broken by one.
#[test]
fn prepare_transaction_is_a_different_statement_and_still_refuses() {
    let mut c = Client::new();
    let answered = c.ask("PREPARE TRANSACTION 'gid'");
    assert!(
        answered.contains("PREPARE TRANSACTION is not supported"),
        "two-phase commit is not this feature: {answered}"
    );
}

/// **A body this node cannot run fails at `PREPARE`**, as on a real server, rather than being
/// stored and failing at the first `EXECUTE`.
#[test]
fn a_body_that_cannot_be_prepared_says_so_at_prepare_time() {
    let mut c = Client::new();
    let answered = c.ask("PREPARE h1_bad AS SELECT * FROM nonexistent_table_h1");
    assert!(
        answered.contains("42P01"),
        "the body is resolved when it is prepared: {answered}"
    );
}

/// **One store, two doors: `DISCARD ALL` clears a SQL-level statement too.**
///
/// `discard_all.rs` already declares that the protocol's named statements are cleared, and asserts
/// it directly "because no corpus statement can reach them". Now one can.
#[test]
fn discard_all_clears_a_sql_prepared_statement() {
    let mut c = Client::new();
    assert_eq!(c.ask("PREPARE h1_d AS SELECT 1"), "");
    assert_eq!(c.ask("EXECUTE h1_d"), "1");
    assert_eq!(c.ask("DISCARD ALL"), "");
    assert!(
        c.ask("EXECUTE h1_d").contains("does not exist"),
        "the statement store DISCARD ALL clears is the one PREPARE writes to"
    );
}
