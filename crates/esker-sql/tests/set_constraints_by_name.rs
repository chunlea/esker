//! **`SET CONSTRAINTS <name> DEFERRED`, which is the half `ALL` does not cover.**
//!
//! `deferred_constraints_test.rb` is five tests and three of them fail here with a message this
//! node raises itself — `constraint "fk_rails_…" is not deferrable`. The one that passes is the
//! one that says `ALL`.
//!
//! Rails declares those foreign keys `deferrable: :immediate`, which is
//! `DEFERRABLE INITIALLY IMMEDIATE`: deferrable, and not deferred until someone asks. Measured on
//! 19beta1, that pair is `condeferrable = t, condeferred = f`, and the whole point of it is that
//! `SET CONSTRAINTS` can then name it.
//!
//! The oracle for every answer below, in one transaction per case:
//!
//! ```text
//! SET CONSTRAINTS fk_auth DEFERRED      then the violating INSERT is accepted
//!                 fk_auth IMMEDIATE     and 23503 arrives here instead
//! SET CONSTRAINTS fk_other DEFERRED     the INSERT still raises: fk_auth was not named
//! SET CONSTRAINTS nosuchname DEFERRED   42704 constraint "nosuchname" does not exist
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE addrs (id bigserial primary key)",
    "CREATE TABLE auth (id bigserial primary key, addr_id bigint, name text)",
    "CREATE TABLE other_p (id bigserial primary key)",
    "CREATE TABLE other_c (id bigserial primary key, p_id bigint)",
    "ALTER TABLE auth ADD CONSTRAINT fk_auth FOREIGN KEY (addr_id) REFERENCES addrs (id) \
     DEFERRABLE INITIALLY IMMEDIATE",
    "ALTER TABLE other_c ADD CONSTRAINT fk_other FOREIGN KEY (p_id) REFERENCES other_p (id) \
     DEFERRABLE INITIALLY IMMEDIATE",
];

const VIOLATION: &str = "INSERT INTO auth (addr_id, name) VALUES (-1, 'John Doe')";
const REFUSED: &str = "!23503 insert or update on table \"auth\" violates foreign key constraint \
                       \"fk_auth\" DETAIL: Key (addr_id)=(-1) is not present in table \"addrs\".";

/// `DEFERRABLE INITIALLY IMMEDIATE` is a *deferrable* constraint, and the catalog says so.
#[test]
fn initially_immediate_is_still_deferrable() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT conname, condeferrable, condeferred FROM pg_constraint \
             WHERE conname IN ('fk_auth','fk_other') ORDER BY conname"
        ),
        [
            ["fk_auth".to_owned(), "t".to_owned(), "f".to_owned()],
            ["fk_other".to_owned(), "t".to_owned(), "f".to_owned()],
        ]
    );
}

/// The test that fails today: name one constraint, and the violation waits for `IMMEDIATE`.
#[test]
fn naming_one_constraint_defers_it() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS fk_auth DEFERRED").unwrap();
    node.run(VIOLATION).expect("deferred, so it is accepted");
    assert_eq!(
        node.answer("SET CONSTRAINTS fk_auth IMMEDIATE").to_string(),
        REFUSED,
        "the check happens where the constraint becomes immediate"
    );
}

/// Two names in one statement, which is what `set_constraints(:deferred, @other_fk, @fk)` sends.
#[test]
fn naming_two_constraints_defers_both() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS fk_other, fk_auth DEFERRED")
        .unwrap();
    node.run(VIOLATION).expect("fk_auth is among the names");
    assert_eq!(
        node.answer("SET CONSTRAINTS fk_other, fk_auth IMMEDIATE")
            .to_string(),
        REFUSED
    );
}

/// **The one that says the name is read**: defer the *other* constraint and the violation is
/// refused where it always was. A `SET CONSTRAINTS` that ignored its argument list would pass
/// every test above and fail this one.
#[test]
fn naming_another_constraint_defers_nothing_here() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS fk_other DEFERRED").unwrap();
    assert_eq!(node.answer(VIOLATION).to_string(), REFUSED);
}

/// `ALL` still means all, which is the case that already worked.
#[test]
fn all_defers_everything() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS ALL DEFERRED").unwrap();
    node.run(VIOLATION).expect("deferred");
    assert_eq!(
        node.answer("SET CONSTRAINTS ALL IMMEDIATE").to_string(),
        REFUSED
    );
}

/// A name that is not a constraint is `42704`, and it quotes the name.
#[test]
fn an_unknown_name_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.answer("SET CONSTRAINTS nosuchname DEFERRED")
            .to_string(),
        "!42704 constraint \"nosuchname\" does not exist"
    );
}

/// **The control**: a foreign key declared without `DEFERRABLE` is still refused by name, in both
/// directions — measured, `IMMEDIATE` too, where it would have changed nothing. A rule that simply
/// started answering `true` for every foreign key would pass all six tests above and fail this.
#[test]
fn a_plain_foreign_key_is_still_not_deferrable() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p2 (id bigint primary key)",
        "CREATE TABLE c2 (id bigint, p_id bigint)",
        "ALTER TABLE c2 ADD CONSTRAINT fk_plain FOREIGN KEY (p_id) REFERENCES p2 (id)",
    ]);
    node.run("BEGIN").unwrap();
    // **`IMMEDIATE` is accepted**, which is the direction this node had wrong. Asking a
    // non-deferrable constraint to be immediate is asking for what is already true. Two comments
    // in this repository said PostgreSQL refuses both and called it measured; the capture they
    // sat beside has only the `DEFERRED` row. Measured on 19beta1, on this one constraint.
    //
    // It goes first because the refusal below aborts the transaction, which is the same reason
    // the oracle script put each direction under its own savepoint.
    node.run("SET CONSTRAINTS fk_plain IMMEDIATE")
        .expect("already immediate is not an error");
    assert_eq!(
        node.answer("SET CONSTRAINTS fk_plain DEFERRED").to_string(),
        "!42809 constraint \"fk_plain\" is not deferrable"
    );
}
