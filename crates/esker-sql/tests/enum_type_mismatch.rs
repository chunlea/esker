//! **A foreign enum is named by its own name** — the sentence half of `debts-v1.1.md` #57.
//!
//! Two enums have no operator between them and no assignment cast either, so a comparison is
//! `42883` and a write is `42804` — and both messages *name the other type*. Measured on 19beta1
//! in a `BEGIN … ROLLBACK` with `mood` = `('sad','ok','happy')` and `other_mood` = `('sad','ok')`:
//!
//! ```text
//! SELECT m = 'sad'::other_mood FROM t     42883 operator does not exist: mood = other_mood
//! SELECT m = n FROM t                     42883 operator does not exist: mood = other_mood
//! SELECT m < n FROM t                     42883 operator does not exist: mood < other_mood
//! INSERT INTO t VALUES (2,'sad'::other_mood, …)
//!                                         42804 column "m" is of type mood but expression is of
//!                                               type other_mood
//! PREPARE p(text) AS SELECT m = $1::other_mood FROM t     the same 42883, at PREPARE
//! ```
//!
//! This node said **`mood = smallint`** and **`expression is of type smallint`**, because a
//! resolved enum cast carried its type as an oid (`plan::Literal::Typed::user`, `feb0ca27`) and an
//! oid is not a name: `ColumnType` is a closed enum of storage types, and the only thing that
//! could spell `other_mood` was a catalog the row evaluator does not have. Carrying the
//! `catalog::TypeDef` instead of its number is what closes it — the same slot
//! `plan::SelectItem::user_type` has always held, one position over.
//!
//! **And `m = n` was not a message at all.** Two enum *columns* both resolve to `int2` ordinals,
//! and `reconcile_enum` let a pair of them through to the ordinary path — so comparing a `mood`
//! with an `other_mood` answered `f` where a real server refuses. A wrong answer, found while
//! measuring the sentence.
//!
//! **What must not move**, measured beside the rest: `m = 'sad'::text` is `mood = text`, `m = 1`
//! is `mood = integer`, and `m = 'sad'::mood` answers — an enum compares with *itself* and with
//! an `unknown` literal, and with nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TYPE other_mood AS ENUM ('sad', 'ok')",
    "CREATE TABLE t (id bigint primary key, m mood, n other_mood)",
    "INSERT INTO t VALUES (1, 'ok', 'sad')",
];

/// One statement with one parameter through `Parse`/`Describe`/`Bind`/`Execute`, answering the
/// first refusal as `!SQLSTATE message` or the rows it produced.
fn bound(sql: &str, value: &str) -> Result<Vec<String>, String> {
    let mut node = parity::Node::new(FIXTURE);
    let mut session = Session::new();
    let mut rows = Vec::new();
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
        let text = String::from_utf8_lossy(&out);
        let parts: Vec<&str> = text.split('\u{0}').collect();
        if parts
            .iter()
            .any(|part| *part == "SERROR" || *part == "VERROR")
        {
            let code = parts
                .iter()
                .find(|part| part.starts_with('C') && part.len() == 6)
                .map_or("?????", |part| &part[1..]);
            let message = parts
                .iter()
                .find(|part| part.starts_with('M'))
                .map_or("", |part| &part[1..]);
            return Err(format!("!{code} {message}"));
        }
        rows.push(text.into_owned());
    }
    Ok(rows)
}

/// One statement's answer with the `DETAIL` and `HINT` cut off — they are the same two sentences
/// for every one of these and what is being asserted is the type names.
fn said(node: &mut parity::Node, sql: &str) -> String {
    let answer = node.answer(sql).to_string();
    answer
        .split_once(" DETAIL:")
        .or_else(|| answer.split_once(" HINT:"))
        .map_or(answer.clone(), |(head, _)| head.to_owned())
}

/// The same, for the refusal a bound statement answers.
fn said_bound(sql: &str, value: &str) -> String {
    match bound(sql, value) {
        Err(refusal) => refusal
            .split_once(" DETAIL:")
            .or_else(|| refusal.split_once(" HINT:"))
            .map_or(refusal.clone(), |(head, _)| head.to_owned()),
        Ok(rows) => format!("answered {} frames", rows.len()),
    }
}

/// **A comparison with a different enum names it**, in both modes.
#[test]
fn a_comparison_with_another_enum_names_that_enum() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m = 'sad'::other_mood FROM t"),
        "!42883 operator does not exist: mood = other_mood",
        "a literal cast to the other enum"
    );
    assert_eq!(
        said(&mut node, "SELECT 'sad'::other_mood = m FROM t"),
        "!42883 operator does not exist: other_mood = mood",
        "and the sides are reported the way they were written"
    );
    assert_eq!(
        said_bound("SELECT m = $1::other_mood FROM t", "sad"),
        "!42883 operator does not exist: mood = other_mood",
        "the same statement as a driver sends it"
    );
}

/// **Two enum columns of different enums do not compare at all** — this answered a boolean.
#[test]
fn two_different_enum_columns_do_not_compare() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m = n FROM t"),
        "!42883 operator does not exist: mood = other_mood",
        "an ordinal is not an identity: both sides are int2 and they are not the same type"
    );
    assert_eq!(
        said(&mut node, "SELECT m < n FROM t"),
        "!42883 operator does not exist: mood < other_mood",
        "and the operator in the message is the one that was written"
    );
    // The same enum on both sides still compares, which is what the ordinal storage is for.
    assert_eq!(
        node.rows("SELECT m = m FROM t"),
        vec![vec!["t"]],
        "one enum compares with itself"
    );
}

/// **A write of a different enum names it too**, which is the half a reader does not predict from
/// the comparison.
#[test]
fn a_write_of_another_enum_names_that_enum() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(
            &mut node,
            "INSERT INTO t VALUES (2, 'sad'::other_mood, 'sad')"
        ),
        "!42804 column \"m\" is of type mood but expression is of type other_mood"
    );
    assert_eq!(
        said(&mut node, "UPDATE t SET m = 'sad'::other_mood"),
        "!42804 column \"m\" is of type mood but expression is of type other_mood"
    );
    assert_eq!(
        said_bound("INSERT INTO t VALUES (2, $1::other_mood, 'sad')", "sad"),
        "!42804 column \"m\" is of type mood but expression is of type other_mood",
        "the same write as a driver sends it"
    );
}

/// **What must not move.** Three spellings, three resolutions, all measured on 19beta1.
#[test]
fn the_other_refusals_still_name_what_they_always_named() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m = 'sad'::text FROM t"),
        "!42883 operator does not exist: mood = text",
        "a text is not an unknown"
    );
    assert_eq!(
        said(&mut node, "SELECT m = 1 FROM t"),
        "!42883 operator does not exist: mood = integer",
        "and an integer is the integer it is, not the enum's storage"
    );
    assert_eq!(
        node.rows("SELECT m = 'ok'::mood FROM t"),
        vec![vec!["t"]],
        "this enum's own cast still compares"
    );
    assert_eq!(
        node.rows("SELECT m = 'ok' FROM t"),
        vec![vec!["t"]],
        "and an unquoted literal is still coerced"
    );
    assert_eq!(
        said(&mut node, "INSERT INTO t VALUES (2, 1, 'sad')"),
        // **`bigint` where 19beta1 says `integer`, and that is a different divergence.** An
        // unsuffixed integer literal is an `int4` there and an `int8` here until a column says
        // otherwise (`plan::Literal::Integer`), and the *comparison* above says `integer` because
        // `expr_type` reads the literal's width from its value. Pinned as this node answers it so
        // that the enum half of the sentence is what this test is about; the width is not.
        "!42804 column \"m\" is of type mood but expression is of type bigint",
        "a bare integer into an enum column is still the integer's own refusal"
    );
}
