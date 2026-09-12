//! **A derived table keeps its columns' user-defined types** — the third gap `debts-v1.1.md` #57
//! names and does not own.
//!
//! An enum is stored as its label's **ordinal** and rendered through the catalog on the way out
//! (ADR 0050), so a column's identity is two halves that have to be found together: the oid on
//! `catalog::ColumnDef::user_type` and the labels on `catalog::TableDef::enums`. Every reader that
//! tells a client what a column is — `pg_typeof`, the `RowDescription` oid, the value's own
//! rendering — goes through `Scope::user_type_at`, which looks the one up in the other.
//!
//! A derived table is planned as a **synthetic `TableDef`** (`exec::subquery::plan_derived`), and
//! that def was built with `user_type: None` on every column and an empty `enums` map. So one
//! clause of wrapping loses the type:
//!
//! ```text
//! SELECT pg_typeof(m) FROM t                          mood       — the column itself
//! SELECT pg_typeof(v) FROM (SELECT m AS v FROM t) s    smallint   — one derived table over it
//! SELECT v FROM (SELECT m AS v FROM t) s               2          — and the ordinal reaches the
//!                                                                   client in place of the label
//! ```
//!
//! Measured on 19beta1, in a `BEGIN … ROLLBACK` with the type made inside it: all three wrappers
//! answer **`mood`** and the label.
//!
//! # Three wrappers, one relation
//!
//! `WITH c AS (…)` is rewritten to a derived table by `plan::cte` and a view is rewritten to one by
//! `Executor::expand_views`, so all three arrive at the same builder. They are asked separately
//! anyway, because that is a claim about this tree rather than a property of SQL — a rewrite that
//! stopped going through `plan_derived` would take the type back out through a door no test on the
//! derived table itself watches.
//!
//! # Both modes
//!
//! Each wrapper is asked as a plain statement and through `Parse`/`Bind`/`Execute` with a
//! parameter, because the two roads through this node differ and a sweep of the cast matrix found
//! 31 pairs where they disagreed. What is asserted in the bound mode is the **`RowDescription`
//! oid** beside the value: a silently wrong oid is what a driver decodes by.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TABLE t (id bigint primary key, m mood)",
    "INSERT INTO t VALUES (1, 'happy')",
    "CREATE VIEW vw AS SELECT id, m AS v FROM t",
];

/// One statement with one parameter through `Parse`/`Describe`/`Bind`/`Execute` — the shape a
/// driver sends — answering the `RowDescription` oid of the first column and the rows under it, or
/// `!SQLSTATE message`.
fn bound(sql: &str, value: &str) -> Result<(u32, Vec<String>), String> {
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
    let oid = first_field_oid(&stream).ok_or_else(|| "no RowDescription".to_owned())?;
    Ok((oid, first_column(&stream)))
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

/// Every backend message in a stream, as `(tag, body)` — `tag(1) · length(4) · body`.
fn frames(stream: &[u8]) -> Vec<(u8, &[u8])> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at + 5 <= stream.len() {
        let len = u32::from_be_bytes([
            stream[at + 1],
            stream[at + 2],
            stream[at + 3],
            stream[at + 4],
        ]) as usize;
        frames.push((
            stream[at],
            &stream[at + 5..(at + 1 + len).min(stream.len())],
        ));
        at += 1 + len;
    }
    frames
}

/// Every `DataRow`'s first column. The body is a count then `length(4, -1 for NULL) · bytes` per
/// column.
fn first_column(stream: &[u8]) -> Vec<String> {
    frames(stream)
        .into_iter()
        .filter(|(tag, _)| *tag == b'D')
        .map(|(_, body)| {
            let size = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
            usize::try_from(size).map_or_else(
                |_| "NULL".to_owned(),
                |size| String::from_utf8_lossy(&body[6..6 + size]).into_owned(),
            )
        })
        .collect()
}

/// The type oid of a `RowDescription`'s **first** field: `count(2)`, then per field a NUL-terminated
/// name, `tableoid(4)`, `attnum(2)` and the oid.
fn first_field_oid(stream: &[u8]) -> Option<u32> {
    let (_, body) = frames(stream).into_iter().find(|(tag, _)| *tag == b'T')?;
    let name_end = body[2..].iter().position(|byte| *byte == 0)? + 2;
    let at = name_end + 1 + 4 + 2;
    Some(u32::from_be_bytes([
        body[at],
        body[at + 1],
        body[at + 2],
        body[at + 3],
    ]))
}

/// The type oid the **simple protocol** declares for a statement's first column.
fn declared(sql: &str, node: &mut parity::Node) -> u32 {
    match node.run(sql).unwrap() {
        esker_sql::pgwire::session::Outcome::Rows { fields, .. } => fields[0].type_oid,
        other @ esker_sql::pgwire::session::Outcome::Done { .. } => {
            panic!("{sql} returned no rows: {other:?}")
        }
    }
}

/// The enum's oid, as this fixture's catalog assigned it — read off the **column**, which has
/// always carried it, so no test here holds a number the catalog is free to move.
fn enum_oid() -> u32 {
    let mut node = parity::Node::new(FIXTURE);
    declared("SELECT m FROM t", &mut node)
}

/// What every wrapper must answer, given the two spellings that reach it.
///
/// `plain` is the statement as written and `bound` the same shape with the row chosen by a
/// parameter — one relation, two roads.
fn keeps_the_user_type(plain: &str, parameterised: &str) {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(&format!("SELECT pg_typeof(v) FROM {plain}")),
        vec![vec!["mood"]],
        "the type a client is told, simple protocol: {plain}"
    );
    assert_eq!(
        node.rows(&format!("SELECT v FROM {plain}")),
        vec![vec!["happy"]],
        "the label, not the ordinal it is stored as: {plain}"
    );
    assert_eq!(
        declared(&format!("SELECT v FROM {plain}"), &mut node),
        enum_oid(),
        "the RowDescription oid is the enum's own: {plain}"
    );
    // **The value rule, not just the label.** An unquoted literal is `unknown` and is coerced to
    // the enum; against a `smallint` column it is `22P02 invalid input syntax for type smallint`.
    assert_eq!(
        node.rows(&format!("SELECT v FROM {plain} WHERE v = 'happy'")),
        vec![vec!["happy"]],
        "a comparison against the wrapped column is the enum's: {plain}"
    );
    // **And the bound road.** A driver sends the row it wants as a parameter, and what it decodes
    // the answer by is the oid beside it.
    assert_eq!(
        bound(&format!("SELECT v FROM {parameterised}"), "1"),
        Ok((enum_oid(), vec!["happy".to_owned()])),
        "the same wrapper under the extended protocol: {parameterised}"
    );
}

/// **A derived table keeps its columns' user-defined types.**
///
/// `SELECT pg_typeof(v) FROM (SELECT m AS v FROM t) s` is `mood` on 19beta1 and was `smallint`
/// here, with the ordinal reaching the client in place of the label.
#[test]
fn a_derived_table_keeps_the_columns_user_type() {
    keeps_the_user_type(
        "(SELECT m AS v FROM t) s",
        "(SELECT m AS v FROM t WHERE id = $1) s",
    );
}

/// **A `WITH` query keeps them too**, through `plan::cte`'s rewrite to a derived table.
#[test]
fn a_with_query_keeps_the_columns_user_type() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("WITH c AS (SELECT m AS v FROM t) SELECT pg_typeof(v) FROM c"),
        vec![vec!["mood"]],
        "the type a client is told"
    );
    assert_eq!(
        node.rows("WITH c AS (SELECT m AS v FROM t) SELECT v FROM c"),
        vec![vec!["happy"]],
        "the label, not the ordinal it is stored as"
    );
    assert_eq!(
        declared(
            "WITH c AS (SELECT m AS v FROM t) SELECT v FROM c",
            &mut node
        ),
        enum_oid(),
        "the RowDescription oid is the enum's own"
    );
    assert_eq!(
        bound(
            "WITH c AS (SELECT m AS v FROM t WHERE id = $1) SELECT v FROM c",
            "1"
        ),
        Ok((enum_oid(), vec!["happy".to_owned()])),
        "the same query under the extended protocol"
    );
}

/// **A view keeps them too**, through `Executor::expand_views`' rewrite to a derived table.
#[test]
fn a_view_keeps_the_columns_user_type() {
    keeps_the_user_type("vw", "vw WHERE id = $1");
}
