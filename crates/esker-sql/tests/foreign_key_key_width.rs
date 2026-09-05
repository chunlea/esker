//! **A child key of a different integer width is still the parent's key.**
//!
//! `t.integer :train_id` referencing a `bigserial` primary key is what `ActiveRecord` writes by
//! default, so `Int4` child against `Int8` parent is the ordinary shape of a Rails foreign key,
//! not an edge case.
//!
//! The child's side of the constraint always agreed: an `INSERT` asks by building the parent's row
//! key, and the memcomparable codec widens both to the same bytes. The parent's side compared
//! `Vec<Datum>` with `==`, where `Int4(1) != Int8(1)` — so every parent-side rule was a no-op:
//!
//! ```text
//! DELETE FROM p        succeeded, leaving the child pointing at nothing   -- should be 23503
//! ON DELETE CASCADE    deleted the parent and kept the child
//! ON DELETE SET NULL   deleted the parent and left the child's key set
//! ON UPDATE CASCADE    moved the parent and left the child behind
//! ```
//!
//! All four answers below were measured on 19beta1 with `c.pid integer` and `p.id bigserial`.
//! Found while testing `SchemaForeignKeyTest`, which uses exactly this pairing — the schemas were
//! not what made it fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `p.id` is `bigserial`; `c.pid` is `integer`. The two are one key.
fn fixture(action: &str) -> Vec<String> {
    vec![
        "CREATE TABLE p (id bigserial primary key)".to_owned(),
        format!("CREATE TABLE c (id int primary key, pid integer REFERENCES p(id){action})"),
        "INSERT INTO p VALUES (1), (2)".to_owned(),
        "INSERT INTO c VALUES (10, 1), (11, 2)".to_owned(),
    ]
}

fn node(action: &str) -> parity::Node {
    let fixture = fixture(action);
    let borrowed: Vec<&str> = fixture.iter().map(String::as_str).collect();
    parity::Node::new(&borrowed)
}

/// **The bug in one statement**: `NO ACTION` refuses, rather than succeeding and orphaning a row.
#[test]
fn a_delete_is_refused_across_the_width() {
    let mut node = node("");
    assert_eq!(
        node.answer("DELETE FROM p WHERE id = 1").to_string(),
        "!23503 update or delete on table \"p\" violates foreign key constraint \"c_pid_fkey\" \
         on table \"c\" DETAIL: Key (id)=(1) is still referenced from table \"c\"."
    );
    assert_eq!(
        node.rows("SELECT id FROM p ORDER BY id"),
        [["1".to_owned()], ["2".to_owned()]],
        "and the parent is still there"
    );
}

/// An `UPDATE` that moves the key is the same question asked of the same function.
#[test]
fn an_update_that_moves_the_key_is_refused_across_the_width() {
    let mut node = node("");
    assert_eq!(
        node.answer("UPDATE p SET id = 5 WHERE id = 1").to_string(),
        "!23503 update or delete on table \"p\" violates foreign key constraint \"c_pid_fkey\" \
         on table \"c\" DETAIL: Key (id)=(1) is still referenced from table \"c\"."
    );
}

/// `ON DELETE CASCADE` takes the child with it — and takes **only** the one that pointed at it.
#[test]
fn a_cascading_delete_reaches_the_child_across_the_width() {
    let mut node = node(" ON DELETE CASCADE");
    node.run("DELETE FROM p WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id, pid FROM c ORDER BY id"),
        [["11".to_owned(), "2".to_owned()]],
        "row 10 went with parent 1; row 11 did not"
    );
}

/// `ON DELETE SET NULL` clears the key it can no longer point with.
#[test]
fn set_null_clears_the_child_across_the_width() {
    let mut node = node(" ON DELETE SET NULL");
    node.run("DELETE FROM p WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id, pid FROM c ORDER BY id"),
        [
            ["10".to_owned(), "\\N".to_owned()],
            ["11".to_owned(), "2".to_owned()],
        ]
    );
}

/// `ON UPDATE CASCADE` moves the child with the parent.
#[test]
fn an_update_cascade_moves_the_child_across_the_width() {
    let mut node = node(" ON UPDATE CASCADE");
    node.run("UPDATE p SET id = 5 WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id, pid FROM c ORDER BY id"),
        [
            ["10".to_owned(), "5".to_owned()],
            ["11".to_owned(), "2".to_owned()],
        ]
    );
}

/// **The control.** Same width on both sides was never broken, and has to stay that way — a fix
/// that only widened would be as wrong as the `==` it replaced.
#[test]
fn the_same_width_on_both_sides_still_works() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p (id bigint primary key)",
        "CREATE TABLE c (id int primary key, pid bigint REFERENCES p(id))",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (10, 1)",
    ]);
    assert_eq!(
        node.answer("DELETE FROM p WHERE id = 1").to_string(),
        "!23503 update or delete on table \"p\" violates foreign key constraint \"c_pid_fkey\" \
         on table \"c\" DETAIL: Key (id)=(1) is still referenced from table \"c\"."
    );
}

/// And a value that is genuinely a different one is still different: `smallint` 2 does not answer
/// for `bigint` 1 merely because both fit in an `i64`.
#[test]
fn a_different_value_is_still_a_different_key() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p (id bigserial primary key)",
        "CREATE TABLE c (id int primary key, pid smallint REFERENCES p(id))",
        "INSERT INTO p VALUES (1), (2)",
        "INSERT INTO c VALUES (10, 2)",
    ]);
    node.run("DELETE FROM p WHERE id = 1")
        .expect("nothing points at 1");
    assert_eq!(node.rows("SELECT id FROM p"), [["2".to_owned()]]);
}

/// And the cast is a real one: a parent key that does not fit the child's column is refused, with
/// the sentence a real server uses. Measured — `UPDATE p SET id = 5000000000` under
/// `ON UPDATE CASCADE` is `ERROR: integer out of range`.
#[test]
fn a_cascade_that_does_not_fit_the_child_is_refused() {
    let mut node = node(" ON UPDATE CASCADE");
    assert_eq!(
        node.answer("UPDATE p SET id = 5000000000 WHERE id = 1")
            .to_string(),
        "!22003 integer out of range"
    );
}
