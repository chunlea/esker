//! `$user` means something only inside quotes, and outside them it is a syntax error.
//!
//! `schema_test.rb`'s `test_raise_on_unquoted_schema_name` asserts the difference directly — it
//! sets `search_path` to `"$user,public"` and requires a raise — and this node used to accept it,
//! quietly setting a path whose first entry is a schema nobody has. A wrong answer dressed as a
//! success, which [ADR 0031](../../docs/adr/0031-a-refusal-outranks-a-wrong-answer.md) ranks below
//! a refusal.
//!
//! Measured on PostgreSQL 19beta1, both spellings, in one rolled-back session:
//!
//! ```text
//! esker=# SET search_path = $user,public;
//! ERROR:  syntax error at or near "$"
//! LINE 1: SET search_path = $user,public
//!                           ^
//! esker=# SET search_path = '$user',public;
//! SET
//! esker=# SHOW search_path;
//!  "$user", public
//! ```
//!
//! The cause is that `$` outside a string begins a *parameter*: PostgreSQL's parser never gets as
//! far as the GUC. `sqlparser` hands it over as a placeholder instead, so the refusal is made
//! where the `SET` is lowered.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Both spellings of the assignment, because the parser has two.**
#[test]
fn an_unquoted_user_is_a_syntax_error_at_the_dollar() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "SET search_path = $user,public",
        "SET search_path TO $user, public",
        "SET search_path = $user",
    ] {
        let refused = node.run(sql).expect_err(sql);
        assert_eq!(refused.sqlstate(), "42601", "{sql}: {refused}");
        assert_eq!(
            refused.to_string(),
            "syntax error at or near \"$\"",
            "PostgreSQL's own sentence, with no `syntax error:` prefix of ours"
        );
    }
}

/// **The quoted form still works, and `SHOW` gives the quotes back.**
///
/// The other half of the capture, and the one that keeps this a refusal of the *spelling* rather
/// than of the feature: `ActiveRecord` sends `SET search_path TO "$user", public` on every
/// connection, so a change that refused `$user` everywhere would refuse the normal case.
#[test]
fn the_quoted_user_is_accepted_and_shown_with_its_quotes() {
    let mut node = parity::Node::new(&[]);
    node.run("SET search_path = '$user',public").unwrap();
    assert_eq!(
        node.rows("SHOW search_path"),
        vec![vec!["\"$user\", public".to_owned()]],
        "measured: PostgreSQL answers with the quotes still on the first entry"
    );
    node.run("SET search_path TO \"$user\", public").unwrap();
    assert_eq!(
        node.rows("SHOW search_path"),
        vec![vec!["\"$user\", public".to_owned()]],
        "the double-quoted spelling ActiveRecord actually sends"
    );
}
