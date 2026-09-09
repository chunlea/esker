//! `ALTER INDEX` — 18 tests over 7 files, and one shape.
//!
//! `ActiveRecord` sends **only `RENAME TO`**, from two places: `rename_index` renames any index
//! (`postgresql/schema_statements.rb:590`), and `rename_table` follows a table rename with the
//! primary key's index (`:467`), because PostgreSQL does not rename that one for you.
//!
//! The fact the unit turns on: **renaming an index renames its constraint.** A real server keeps
//! two catalog rows that share a name and moves both; here they are one field, so the
//! `pg_constraint` row follows by construction rather than by a second write.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_indexes.indexname` is a `name` there and `text` here — the standing catalog trade. Three
    // occurrences of the one statement.
    types: &[
        "SELECT 'r', conname, contype FROM pg_constraint WHERE conrelid = 'ai'::regclass AND contype = 'p'",
    ],
    answers: &[
        // **The four forms `sqlparser` 0.62.0 cannot read.** It parses `RENAME TO` and no other
        // `ALTER INDEX` operation, so these reached `parse`'s refusal table — where they now name
        // themselves rather than answering a parser's expected-token list. A `42601` would claim
        // the SQL was malformed, and three of these four are statements a real server runs; that
        // is contract C1, and naming them is the C2 answer C1 leaves room for.
        //
        // None is sent by `ActiveRecord`, which uses `RENAME TO` alone.
        (
            "ALTER INDEX IF EXISTS \"nosuchindex\" RENAME TO \"x\"",
            "`0A000 ALTER INDEX IF EXISTS is not supported`: `sqlparser` has nowhere to put the \
             clause, so the flag cannot reach the executor — where the plain form's `42P01` is \
             already right, and `IF EXISTS` would only soften it to a notice.",
            "UNMEASURED",
        ),
        (
            "ALTER INDEX \"index_ai_on_name\" ALTER COLUMN 1 SET STATISTICS 100",
            "Both refuse it and both say `0A000`; PostgreSQL's sentence is about the *column* \
             being non-expression and hints at the table's, where this node names the clause it \
             cannot read. The same class, a different sentence.",
            "UNMEASURED",
        ),
        (
            "ALTER INDEX \"index_ai_on_name\" SET (fillfactor = 70)",
            "A real server accepts it and changes nothing this node can observe — there is no \
             `fillfactor` here and no storage parameter on an index. Named rather than accepted \
             and ignored, which is this node's rule for a setting it will not honour.",
            "UNMEASURED",
        ),
        (
            "ALTER INDEX \"index_ai_on_name\" SET TABLESPACE pg_default",
            "The same: one tablespace and no way to have another, so moving an index to one is a \
             word this node cannot mean.",
            "UNMEASURED",
        ),
        // **A real server renames a *table* through `ALTER INDEX`** — the statement is the generic
        // rename wearing another keyword, measured. Refused here rather than quietly doing
        // `ALTER TABLE`'s job from an arm named for indexes; nothing the suite sends does it.
        (
            "ALTER INDEX \"ai\" RENAME TO \"ai_renamed\"",
            "`0A000` naming the relation: `ALTER INDEX` on a table renames the table on a real \
             server. This node refuses rather than letting an arm named for indexes rename \
             something else — `ALTER TABLE … RENAME TO` is the statement that does it here, and \
             it works.",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_alter_index_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_alter_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Renaming an index renames its constraint**, and here that is one write rather than two.
///
/// A real server keeps a `pg_class` row and a `pg_constraint` row that share a name and moves both.
/// This node keeps one field — a `UNIQUE` constraint and the index it owns are one `IndexDef::name`,
/// and a primary key's index is `TableDef::primary_key_name` — so the constraint follows by
/// construction. The corpus proves the catalog agrees; this proves the *index still works*
/// afterwards, which a read of `pg_class` cannot show.
#[test]
fn a_renamed_index_keeps_doing_its_job() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ai (id bigint PRIMARY KEY, name text, qty bigint)")
        .unwrap();
    node.run("CREATE UNIQUE INDEX index_ai_on_qty ON ai (qty)")
        .unwrap();
    node.run("INSERT INTO ai VALUES (1, 'a', 10)").unwrap();

    node.run("ALTER INDEX index_ai_on_qty RENAME TO index_ai_on_amount")
        .unwrap();
    // The old name is gone — a renamed index keeps its id, so a reconciliation keyed on the id
    // would have left it, and two names would resolve to one index.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'index_ai_on_qty'"),
        [["0"]]
    );
    // **And it is still unique.** A rename that quietly detached the index would show up here and
    // nowhere in the catalog.
    assert!(
        node.answer("INSERT INTO ai VALUES (2, 'b', 10)")
            .to_string()
            .starts_with("!23505"),
        "the renamed index still refuses a duplicate"
    );
    node.run("INSERT INTO ai VALUES (2, 'b', 20)").unwrap();

    // The primary key's index renames too, and its constraint goes with it.
    node.run("ALTER INDEX ai_pkey RENAME TO ai2_pkey").unwrap();
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint WHERE conrelid = 'ai'::regclass AND contype = 'p'"
        ),
        [["ai2_pkey"]]
    );
    assert!(
        node.answer("INSERT INTO ai VALUES (1, 'c', 30)")
            .to_string()
            .starts_with("!23505"),
        "the renamed primary key still refuses a duplicate"
    );
    // And the freed name can be taken again, which is what proves the record went with it.
    node.run("CREATE INDEX index_ai_on_qty ON ai (name)")
        .unwrap();
}
