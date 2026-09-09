//! `pg_class` and `pg_namespace` as computed views — the other half of rung 4's blocker.
//!
//! The statement rung 4 stops on needs three things at once, which is why the rung is one unit:
//! an aliased `LEFT JOIN`, the `= ANY` of the previous commit, and these two relations.
//!
//! `pg_class` is the first catalog view whose rows are **not** constants. They come from one scan
//! of the same name records `CREATE TABLE` writes, which is what keeps a view over the records
//! from becoming a second copy of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `relname` and `nspname` are `name` on a real server — the 64-byte identifier type — and
        // `text` here. The values are identical and every comparison against one already treats
        // them as text; `relkind` is `"char"` there and `text` here for the same reason. The same
        // trade `pg_type`'s columns make, declared in `tests/pg_catalog.rs`.
        "SELECT c.relname, c.relkind FROM pg_class c WHERE c.relname IN ('r4a','r4b') ORDER BY \
         c.relname",
        "SELECT c.relkind FROM pg_class c WHERE c.relname = 'r4a_pkey'",
    ],
    answers: &[],
};

#[test]
fn every_pg_class_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_pg_class.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 8,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The statement rung 4 stops on, answered.
///
/// Named on its own rather than left inside the replay, because a future reader should be able to
/// find the ladder's blocker by name — as `tests/regtype.rs` does for rung 2's.
#[test]
fn the_statement_that_stopped_rung_4_answers() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE widgets (id int8 PRIMARY KEY, name text)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = \
             c.relnamespace WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = \
             'widgets' AND c.relkind IN ('r','v','m','p','f')"
        ),
        vec![vec!["widgets"]]
    );
}

/// The rows follow the catalog, because they **are** the catalog.
///
/// The property that makes a computed view worth the choice: nothing writes `pg_class`, so
/// nothing can leave it stale. A table created, then dropped, appears and disappears without any
/// code between the two knowing that `pg_class` exists.
#[test]
fn a_created_table_appears_and_a_dropped_one_leaves() {
    let mut node = parity::Node::new(&[]);
    let count = |node: &mut parity::Node| {
        node.rows("SELECT count(*) FROM pg_class c WHERE c.relname = 'ghost'")
            .pop()
            .and_then(|row| row.first().cloned())
            .unwrap_or_default()
    };

    assert_eq!(count(&mut node), "0");
    node.run("CREATE TABLE ghost (id int8 PRIMARY KEY)")
        .unwrap();
    assert_eq!(count(&mut node), "1");
    node.run("DROP TABLE ghost").unwrap();
    assert_eq!(count(&mut node), "0");
}

/// A primary key is a relation in `pg_class`, and an index is too.
///
/// Measured: `r4a_pkey` is there with `relkind` `i` on a real server, even though here the row key
/// *is* the primary key and there is no separate index behind it. What `relkind` describes is a
/// relation a client can name, and a client can name it.
#[test]
fn an_index_and_a_primary_key_are_relations() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE parts (id int8 PRIMARY KEY, code text)",
        "CREATE INDEX parts_code_idx ON parts (code)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT c.relname, c.relkind FROM pg_class c WHERE c.relname IN ('parts', \
             'parts_pkey', 'parts_code_idx') ORDER BY c.relname"
        ),
        vec![
            vec!["parts", "r"],
            vec!["parts_code_idx", "i"],
            vec!["parts_pkey", "i"],
        ]
    );
}

/// Both new views are read-only, like the two before them.
#[test]
fn writing_either_view_is_refused() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "DROP TABLE pg_class",
        "DROP TABLE pg_namespace",
        "ALTER TABLE pg_class ADD COLUMN x int8",
        "CREATE INDEX i ON pg_namespace (oid)",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42501", "{sql} -> {error}");
    }
}
