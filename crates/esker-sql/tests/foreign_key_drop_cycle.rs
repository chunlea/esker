//! **`migration/foreign_key_test.rb`'s create/drop cycle**, captured from PostgreSQL 19.
//!
//! The file's `ForeignKeyChangeColumnTest` and its two subclasses run 88 tests, and every one of
//! them creates two tables in `setup` and drops them in `teardown`. On PostgreSQL 19 the whole
//! file is 88 runs / 244 assertions / 0 errors; on this node it was 3 failures and 79 errors, and
//! the first error in every case came out of the *second* `setup`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// One `migrate(:up)`, as ActiveRecord sends it — the statements are copied from the oracle's
/// `log_statement = all` output, prefix and all.
const UP: [&str; 3] = [
    "CREATE UNLOGGED TABLE \"p_rockets\" (\"id\" bigserial primary key, \"name\" character varying)",
    "CREATE UNLOGGED TABLE \"p_astronauts\" (\"id\" bigserial primary key, \"name\" character \
     varying, \"rocket_id\" bigint, CONSTRAINT \"fk_rails_bae32ac863\"\n\tFOREIGN KEY \
     (\"rocket_id\")\n\t  REFERENCES \"p_rockets\" (\"id\")\n\t)",
    "CREATE INDEX \"index_p_astronauts_on_rocket_id\" ON \"p_astronauts\" (\"rocket_id\")",
];

/// And `migrate(:down)`: the child first, because the foreign key points at the parent.
const DOWN: [&str; 2] = ["DROP TABLE \"p_astronauts\"", "DROP TABLE \"p_rockets\""];

/// **A test's teardown must leave nothing for the next test's setup to trip over.**
///
/// `foreign_key_test.rb` runs this cycle 88 times against one connection. The first pass is fine;
/// the second stops at `CREATE INDEX` with `42P07 relation "index_p_astronauts_on_rocket_id"
/// already exists`, because dropping the child left the index's *name* behind. Every later test in
/// the class then fails in `setup`, and the class's `teardown` block never runs — which is how
/// `table_name_prefix` stays set and the suffix subclass goes looking for `p_rockets_s`.
#[test]
fn the_migration_cycle_can_run_twice() {
    let mut node = parity::Node::new(&[]);
    for cycle in 1..=3 {
        for statement in UP {
            node.run(statement)
                .unwrap_or_else(|error| panic!("cycle {cycle}, up: {statement}\n  -> {error}"));
        }
        for statement in DOWN {
            node.run(statement)
                .unwrap_or_else(|error| panic!("cycle {cycle}, down: {statement}\n  -> {error}"));
        }
        // Nothing of either table is left: no table, no index, no sequence.
        assert_eq!(
            node.rows(
                "SELECT count(*) FROM pg_class WHERE relname LIKE 'p\\_%' OR relname LIKE \
                 'index\\_p\\_%'"
            ),
            [["0"]],
            "cycle {cycle} left something behind"
        );
    }
}
