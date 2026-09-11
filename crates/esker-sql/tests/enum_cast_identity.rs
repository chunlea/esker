//! **A cast to an enum keeps the enum's identity, everywhere a cast can appear** — `debts-v1.1.md`
//! #57 (b).
//!
//! `'sad'::mood` is carried as a `CatalogFunc::UserCast` with the placeholder type
//! `ColumnType::Int2` until `Executor::bound` folds it against the catalog (ADR 0053) — the row
//! evaluator has no catalog, so the pass is right to exist. What it leaves behind is the defect:
//! a **bare** `Literal::Typed(Datum::Int2(ordinal))`, whose type name survives in exactly one
//! place, `plan::SelectItem::user_type`, and that is a slot the **projection** has and no other
//! position does.
//!
//! So an enum cast outside a target list has no identity, and everything that types two operands
//! against each other refuses it. Measured on this tree:
//!
//! ```text
//! SELECT 'sad'::mood                        16384, 'sad'      right — the projection has a slot
//! WHERE m = 'sad'                           right             — an unquoted literal is coerced
//! WHERE m = 'sad'::mood                     42883 operator does not exist: mood = smallint
//! INSERT INTO t VALUES ('sad'::mood)        42804 column "m" is of type mood but expression is
//!                                                 of type smallint
//! UPDATE t SET m = 'sad'::mood              42804, the same
//! SELECT m FROM t UNION SELECT 'sad'::mood  answers the ordinals under `smallint`
//! ```
//!
//! and every one of them is measured on 19beta1 too, in a `BEGIN … ROLLBACK` with the type made
//! inside it: the comparison answers, the write answers, the union is a **`mood`** whose values
//! are the labels, and `pg_typeof('sad'::mood)` is `mood`.
//!
//! **The one that must not move**: `WHERE m = 'sad'::text` is `42883 operator does not exist:
//! mood = text` on a real server, measured beside the rest. A fix that makes the enum cast work by
//! turning it back into a string would answer that one too, which is why the debt row rejects it.
//!
//! # Both modes
//!
//! Every shape is asked as a literal and as a **bound parameter** — `$1::mood` with the value sent
//! as text, which is what a driver does. The two roads through this node differ (the fold versus
//! the evaluator), and a sweep of the cast matrix found 31 pairs where they disagreed; a type
//! identity that only survives one of them is half a fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok')",
    "CREATE TABLE t (id bigint primary key, m mood)",
    "INSERT INTO t VALUES (1, 'ok')",
];

/// One statement through `Parse`/`Describe`/`Bind`/`Execute` with **no declared type** — the shape
/// a driver sends — answering the rows' first column, or `!SQLSTATE message`.
///
/// A fresh node per call, because these tests write: a shared session would make the second
/// assertion depend on the first one's rows.
fn bound(sql: &str, value: &str) -> Result<Vec<String>, String> {
    let mut node = parity::Node::new(FIXTURE);
    let mut session = Session::new();
    let mut stream = Vec::new();
    for message in [
        Frontend::Parse {
            statement: "s".to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Describe {
            target: Target::Statement,
            name: "s".to_owned(),
        },
        Frontend::Bind {
            portal: "p".to_owned(),
            statement: "s".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        },
        Frontend::Execute {
            portal: "p".to_owned(),
            max_rows: 0,
        },
    ] {
        let mut out = Vec::new();
        session.handle(&message, &mut node.executor, &mut out);
        if let Some(refusal) = refusal_in(&out) {
            return Err(refusal);
        }
        stream.extend(out);
    }
    Ok(first_column(&stream))
}

/// `!SQLSTATE message` if this frame carried an `ErrorResponse`.
fn refusal_in(out: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(out);
    let parts: Vec<&str> = text.split('\u{0}').collect();
    if !parts
        .iter()
        .any(|part| *part == "SERROR" || *part == "VERROR")
    {
        return None;
    }
    let code = parts
        .iter()
        .find(|part| part.starts_with('C') && part.len() == 6)
        .map_or("?????", |part| &part[1..]);
    let message = parts
        .iter()
        .find(|part| part.starts_with('M'))
        .map_or("", |part| &part[1..]);
    Some(format!("!{code} {message}"))
}

/// Every `DataRow`'s first column, read off the backend stream — `tag(1) · length(4) · body`, and
/// the body is a count then `length(4, -1 for NULL) · bytes` per column.
fn first_column(stream: &[u8]) -> Vec<String> {
    let mut rows = Vec::new();
    let mut at = 0;
    while at + 5 <= stream.len() {
        let len = u32::from_be_bytes([
            stream[at + 1],
            stream[at + 2],
            stream[at + 3],
            stream[at + 4],
        ]) as usize;
        if stream[at] == b'D' {
            let body = &stream[at + 5..(at + 1 + len).min(stream.len())];
            let size = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
            rows.push(usize::try_from(size).map_or_else(
                |_| "NULL".to_owned(),
                |size| String::from_utf8_lossy(&body[6..6 + size]).into_owned(),
            ));
        }
        at += 1 + len;
    }
    rows
}

/// The type OID the **simple protocol** declares for a statement's first column.
fn declared(sql: &str, node: &mut parity::Node) -> u32 {
    match node.run(sql).unwrap() {
        esker_sql::pgwire::session::Outcome::Rows { fields, .. } => fields[0].type_oid,
        other @ esker_sql::pgwire::session::Outcome::Done { .. } => {
            panic!("{sql} returned no rows: {other:?}")
        }
    }
}

/// **A comparison against an enum column takes an enum cast.**
///
/// `WHERE m = 'sad'::mood` is `42883 operator does not exist: mood = smallint` here and answers on
/// 19beta1. `reconcile_enum` recognises an enum only as an `Expr::Ordinal` — a **column** — and
/// the cast on the other side has folded to a bare `int2` by the time it is asked.
#[test]
fn a_comparison_takes_an_enum_cast() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT m::text FROM t WHERE m = 'ok'::mood"),
        vec![vec!["ok"]],
        "the row the comparison selects"
    );
    assert_eq!(
        node.rows("SELECT count(*)::text FROM t WHERE m = 'sad'::mood"),
        vec![vec!["0"]],
        "and a label the row does not hold selects nothing"
    );
    // **The distinction that must survive.** A `text` is not an `unknown`, and there is no
    // operator between an enum and a text — measured on 19beta1 beside the rest.
    assert!(
        node.answer("SELECT m FROM t WHERE m = 'sad'::text")
            .to_string()
            .starts_with("!42883 operator does not exist: mood = text"),
        "a text is not an unknown: {}",
        node.answer("SELECT m FROM t WHERE m = 'sad'::text")
    );
    // And the spelling that has always worked: an unquoted literal is `unknown` and is coerced.
    assert_eq!(
        node.rows("SELECT m::text FROM t WHERE m = 'ok'"),
        vec![vec!["ok"]]
    );
}

/// **A write takes an enum cast**, which is the half a reader does not predict from the
/// comparison.
///
/// `INSERT INTO t VALUES (2, 'sad'::mood)` is `42804 column "m" is of type mood but expression is
/// of type smallint`, and so is the `UPDATE`. Both answer on 19beta1.
#[test]
fn a_write_takes_an_enum_cast() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO t VALUES (2, 'sad'::mood)").unwrap();
    node.run("UPDATE t SET m = 'ok'::mood WHERE id = 2")
        .unwrap();
    assert_eq!(
        node.rows("SELECT m::text FROM t WHERE id = 2"),
        vec![vec!["ok"]],
        "the value the write stored, read back through the enum's output function"
    );
}

/// **A set operation over an enum is an enum**, not the ordinal it is stored as.
///
/// `SELECT m FROM t UNION SELECT 'sad'::mood` answers `mood` on 19beta1 with the **labels** as its
/// values; here the arm's cast has no identity, so `query::append` unifies a `mood` column with an
/// `int2` one, the set is declared `smallint`, and the client is sent `1` and `2`.
#[test]
fn a_set_operation_over_an_enum_is_an_enum() {
    let mut node = parity::Node::new(FIXTURE);
    let mut rows = node.rows("SELECT m AS v FROM t UNION SELECT 'sad'::mood");
    rows.sort();
    assert_eq!(
        rows,
        vec![vec!["ok"], vec!["sad"]],
        "the labels, because the set is an enum"
    );
    // **The type the client is told**, read off the simple protocol's own `RowDescription` rather
    // than through `pg_typeof` over a derived table: a derived table loses a column's user type
    // here with or without a set operation — `SELECT pg_typeof(v) FROM (SELECT m AS v FROM t) s`
    // is `smallint` — which is a **separate** gap this unit does not touch and a assertion
    // through one would have been testing.
    //
    // Compared against the same statement's own arm rather than against a number, because an
    // enum's oid is assigned by the catalog and moves with the fixture.
    let enum_oid = declared("SELECT 'sad'::mood AS v", &mut node);
    assert_eq!(
        declared("SELECT m AS v FROM t UNION SELECT 'sad'::mood", &mut node),
        enum_oid,
        "the set is the enum, not the int2 it is stored as"
    );
    assert_eq!(
        declared(
            "SELECT m AS v FROM t UNION ALL SELECT 'sad'::mood",
            &mut node
        ),
        enum_oid,
        "and the same without the deduplication"
    );
}

/// **The same three shapes with the value bound, which is a second defect behind the first.**
///
/// `Executor::bound` resolves a cast to a user type (`resolve_user_cast`) and that is the
/// **execute** path. `Executor::described_in` re-derives the statement's shape for `Describe` and
/// does not run that pass, so at `Describe` time `$1::mood` is still a `CatalogFunc::UserCast`,
/// whose declared type is the placeholder `ColumnType::Int2` — and the comparison is refused
/// before a value is ever bound:
///
/// ```text
/// Parse    SELECT m FROM t WHERE m = $1::mood      ok
/// Describe                                          42883 operator does not exist: mood = smallint
/// ```
///
/// It is wire v3 family **F11's shape one pass over** — the `Describe` path deriving a statement's
/// types by a road that skips something the execute path does — and it is why a driver sees this
/// even for the spellings the simple protocol now answers. `ActiveRecord` binds by default.
///
/// **Not fixed here, and not by guessing**: running `resolve_user_cast` inside `described_in`
/// would meet a *placeholder* where the label goes — `substitute_placeholders` puts an empty
/// `Datum::Text` there — and resolving that is `22P02 invalid input value for enum mood: ""` for a
/// statement that runs perfectly once a value arrives. What it needs is either a describing mode
/// for that pass or `reconcile_enum` reading the unresolved call's own type **name**, which the
/// node still carries as its first argument. One of those is the next unit; the numbers above are
/// the acceptance.
#[test]
#[ignore = "#57(b), second half: the Describe path does not resolve a user cast"]
fn a_bound_parameter_takes_an_enum_cast() {
    assert_eq!(
        bound("SELECT m::text FROM t WHERE m = $1::mood", "ok"),
        Ok(vec!["ok".to_owned()]),
        "a comparison with the value bound"
    );
    assert_eq!(
        bound(
            "INSERT INTO t VALUES (3, $1::mood) RETURNING m::text",
            "sad"
        ),
        Ok(vec!["sad".to_owned()]),
        "a write with the value bound"
    );
    let mut rows = bound(
        "SELECT v::text FROM (SELECT m AS v FROM t UNION SELECT $1::mood) s",
        "sad",
    )
    .expect("the bound spelling answers");
    rows.sort();
    assert_eq!(rows, vec!["ok".to_owned(), "sad".to_owned()]);
}
