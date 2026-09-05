//! **What `rename_table` does after the table is renamed**, which is three more statements.
//!
//! `rename_table_test.rb` is four tests and all four turn on one thing: PostgreSQL does **not**
//! rename the primary key's index when the table is renamed, so `ActiveRecord` renames it itself —
//! and it only knows to, because `pk_and_sequence_for(new_name)` answered with a `pk`
//! (`postgresql/schema_statements.rb:460`). A nil `pk` skips the whole block silently.
//!
//! Two of the four were `PG::InFailedSqlTransaction` at run 47ffc338, which is the shape worth
//! remembering: the fallback query inside `pk_and_sequence_for` **raised** `0A000` for
//! `split_part`, aborting the transaction, and every later statement in the test reported the
//! abort rather than the cause. The two that passed have a `bigserial` key, where the *dependency*
//! query answers and the fallback never runs. `eb16e3c4` gave the fallback its three functions;
//! this file pins what the sequence then does.
//!
//! Measured on 19beta1, and the first line is the one that surprises:
//!
//! ```text
//! ALTER TABLE "g1_before" RENAME TO "g1_after"     -- the index is STILL g1_before_pkey
//! ALTER INDEX "g1_before_pkey" RENAME TO …         -- now it is g1_after_pkey
//!                                                  -- and pg_constraint.conname followed
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn index_names(node: &mut parity::Node) -> Vec<Vec<String>> {
    node.rows(
        "SELECT c.relname FROM pg_index i JOIN pg_class c ON i.indexrelid = c.oid \
         WHERE c.relname LIKE 'g1\\_%pkey' ORDER BY c.relname",
    )
}

/// **A table rename leaves the index name alone** — which is the whole reason `ActiveRecord` sends a
/// second statement, and the assertion that would silently pass if it did not.
#[test]
fn renaming_the_table_does_not_rename_the_primary_key_index() {
    let mut node = parity::Node::new(&[
        "CREATE EXTENSION \"uuid-ossp\"",
        "CREATE TABLE \"g1_before\" (\"id\" uuid DEFAULT uuid_generate_v4() NOT NULL PRIMARY KEY)",
    ]);
    assert_eq!(index_names(&mut node), [["g1_before_pkey".to_owned()]]);
    node.run("ALTER TABLE \"g1_before\" RENAME TO \"g1_after\"")
        .unwrap();
    assert_eq!(
        index_names(&mut node),
        [["g1_before_pkey".to_owned()]],
        "still the old name: PostgreSQL does not move it and neither does this"
    );
}

/// And the explicit rename moves it — index and constraint together, which is one field here and
/// two catalog rows on a real server.
#[test]
fn the_explicit_index_rename_moves_the_index_and_its_constraint() {
    let mut node = parity::Node::new(&[
        "CREATE EXTENSION \"uuid-ossp\"",
        "CREATE TABLE \"g1_before\" (\"id\" uuid DEFAULT uuid_generate_v4() NOT NULL PRIMARY KEY)",
        "ALTER TABLE \"g1_before\" RENAME TO \"g1_after\"",
    ]);
    node.run("ALTER INDEX \"g1_before_pkey\" RENAME TO \"g1_after_pkey\"")
        .unwrap();
    assert_eq!(index_names(&mut node), [["g1_after_pkey".to_owned()]]);
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = 'g1_after'::regclass AND contype = 'p'"
        ),
        [["g1_after_pkey".to_owned()]]
    );
}

/// The `bigserial` half, where a fourth statement renames the sequence — **and the column's
/// default follows it**, because the default names the sequence that is there now rather than the
/// text it was written with. Measured: `nextval('g1_sa_id_seq'::regclass)`.
#[test]
fn a_renamed_sequence_is_what_the_default_then_names() {
    let mut node = parity::Node::new(&["CREATE TABLE \"g1_sb\" (\"id\" bigserial primary key)"]);
    for statement in [
        "ALTER TABLE \"g1_sb\" RENAME TO \"g1_sa\"",
        "ALTER INDEX \"g1_sb_pkey\" RENAME TO \"g1_sa_pkey\"",
        "ALTER TABLE \"g1_sb_id_seq\" RENAME TO \"g1_sa_id_seq\"",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'g1_sa'::regclass"
        ),
        [["nextval('g1_sa_id_seq'::regclass)".to_owned()]]
    );
    // And the sequence still drives the column after all three renames.
    node.run("INSERT INTO g1_sa DEFAULT VALUES").unwrap();
    assert_eq!(node.rows("SELECT id FROM g1_sa"), [["1".to_owned()]]);
}
