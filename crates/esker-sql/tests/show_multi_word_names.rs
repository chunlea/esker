//! `SHOW TIME ZONE` — **the three parameter names PostgreSQL spells with spaces.**
//!
//! `sqlparser` hands `SHOW` a `Vec<Ident>`, and this node joined them with a dot because that is
//! how a namespaced GUC arrives: `SHOW esker.read_as_of` is two idents for the same reason
//! `SHOW TIME ZONE` is. Joining is right for the first and wrong for the second, and the AST
//! cannot tell them apart — so `SHOW TIME ZONE` was
//! `42704 unrecognized configuration parameter "TIME.ZONE"`, which is
//! `postgresql_adapter_prevent_writes_test.rb:63` in the suite.
//!
//! Measured on PostgreSQL 19, and the closed set is the point:
//!
//! ```text
//! SHOW TIME ZONE                     -> column TimeZone              | Etc/UTC
//! SHOW TIMEZONE                      -> column TimeZone              | Etc/UTC
//! SHOW TRANSACTION ISOLATION LEVEL   -> column transaction_isolation | read committed
//! SHOW SESSION AUTHORIZATION         -> column session_authorization | esker
//! SHOW TRANSACTION READ ONLY         42601: syntax error at or near "READ"
//! SHOW NOSUCH THING                  42601: syntax error at or near "THING"
//! ```
//!
//! **There is no general two-word rule** — that is what the last two lines are for. PostgreSQL's
//! grammar has exactly three multi-word productions here and everything else is a syntax error, so
//! recognising the three by their words is copying the grammar rather than guessing a pattern.
//!
//! The column is named by PostgreSQL's own spelling and not the user's, which is why one-word
//! `SHOW TIMEZONE` answers a column called `TimeZone` too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::session::Outcome;

/// The column name and the value, which is the whole of what a `SHOW` answers.
fn shown(node: &mut parity::Node, sql: &str) -> String {
    match node.run(sql) {
        Ok(Outcome::Rows { fields, rows, .. }) => {
            let name = fields.first().map_or("?", |field| field.name.as_str());
            let value = rows
                .first()
                .and_then(|row| row.first())
                .and_then(|cell| cell.as_ref())
                .map_or_else(
                    || "\\N".to_owned(),
                    |bytes| String::from_utf8_lossy(bytes).into_owned(),
                );
            format!("{name}|{value}")
        }
        Ok(_) => "not rows".to_owned(),
        Err(error) => format!("!{error}"),
    }
}

#[test]
fn show_time_zone_is_the_timezone_parameter() {
    let mut node = parity::Node::new(&[]);
    // `UTC` where the oracle's container says `Etc/UTC`: the same instant under another name, and
    // the standing boot-value divergence `tests/discard_all.rs` already declares.
    assert_eq!(shown(&mut node, "SHOW TIME ZONE"), "TimeZone|UTC");
}

#[test]
fn the_one_word_spelling_answers_the_same_column() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(shown(&mut node, "SHOW TIMEZONE"), "TimeZone|UTC");
    assert_eq!(shown(&mut node, "SHOW timezone"), "TimeZone|UTC");
}

#[test]
fn show_transaction_isolation_level_is_the_parameter() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        shown(&mut node, "SHOW TRANSACTION ISOLATION LEVEL"),
        "transaction_isolation|read committed"
    );
    assert_eq!(
        shown(&mut node, "SHOW transaction_isolation"),
        "transaction_isolation|read committed"
    );
}

#[test]
fn show_session_authorization_is_the_role_the_session_is_acting_as() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        shown(&mut node, "SHOW SESSION AUTHORIZATION"),
        "session_authorization|esker"
    );
    assert_eq!(
        shown(&mut node, "SHOW session_authorization"),
        "session_authorization|esker"
    );
}

#[test]
fn a_namespaced_parameter_is_still_read_as_a_dotted_name() {
    let mut node = parity::Node::new(&[]);
    // The case the dot-join exists for, and the reason the three above are recognised by their
    // words rather than by "two idents means spaces": both arrive as two idents.
    assert_eq!(
        shown(&mut node, "SHOW esker.nosuchthing"),
        "!unrecognized configuration parameter \"esker.nosuchthing\""
    );
}
