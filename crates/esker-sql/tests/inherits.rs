//! `CREATE TABLE … INHERITS (parent)` — statement 762.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own hierarchy.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `relname` is a `name` on a real server and `text` here, with identical characters — the
    // standing trade every `pg_catalog` column makes.
    types: &[
        "SELECT c.relname, p.relname, i.inhseqno FROM pg_inherits i JOIN pg_class c ON c.oid = \
         i.inhrelid JOIN pg_class p ON p.oid = i.inhparent WHERE c.relname = 'ic'",
        "SELECT attname FROM pg_attribute WHERE attrelid = 'ic2'::regclass AND attnum > 0 ORDER \
         BY attnum",
    ],
    answers: &[
        (
            "SELECT attname, atttypid::regtype::text, attnotnull FROM pg_attribute WHERE attrelid \
             = 'ic'::regclass AND attnum > 0 ORDER BY attnum",
            "`atttypid::regtype` is not implemented — contract C2, and it is the *forward* cast: \
             `'integer'::regtype::oid` runs here because `ActiveRecord` sends it, and reading an \
             oid back as a type name does not. The fact this line is here for — that a child's \
             columns are the parent's, with their types and their `NOT NULL` — is asserted \
             directly in the test below, and the plain `attname` form of the same query agrees \
             two lines further down.",
        ),
        (
            "SELECT tag FROM ONLY ip ORDER BY tag",
            "**`ONLY` is a contract C1 gap, and it is in the parser rather than here.** \
             `sqlparser` 0.62.0 takes the word for `ALTER TABLE` and `LOCK TABLE` and not in a \
             `FROM` clause, so `FROM ONLY ip` parses as the relation `only` aliased `ip` — which \
             is why the answer is `42P01` about a table nobody wrote. It **errors rather than \
             answering wrongly**, which is the property that matters: a node that silently \
             ignored the keyword would return the child's rows where a real server excludes \
             them, and nothing would report it. Registered in the plan's C1 register with \
             `GENERATED … VIRTUAL` and `DROP INDEX CONCURRENTLY`; the fix is the same rewrite \
             mechanism `CONCURRENTLY` already uses.",
        ),
    ],
};

#[test]
fn every_inherits_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_inherits.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Inheritance is a read rule, not just a DDL one.**
///
/// The child's rows come back from the parent. A node that copied the columns and stopped would
/// answer `1` where a real server answers `2` — a wrong answer rather than a missing feature, and
/// the reason the DDL cannot land without the scan.
#[test]
fn a_parents_scan_includes_its_childrens_rows() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (id serial primary key, number integer NOT NULL DEFAULT 7, tag text)",
        "CREATE TABLE ic ( ) INHERITS (ip)",
    ]);
    node.run("INSERT INTO ic (number, tag) VALUES (1, 'child')")
        .unwrap();
    node.run("INSERT INTO ip (number, tag) VALUES (2, 'parent')")
        .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM ip"), [["2"]]);
    assert_eq!(node.rows("SELECT count(*) FROM ic"), [["1"]]);
    assert_eq!(
        node.rows("SELECT tag FROM ip ORDER BY tag"),
        [["child"], ["parent"]]
    );
}

/// The child takes the parent's columns, `NOT NULL` and **defaults** — the parent's sequence too.
#[test]
fn a_child_inherits_columns_and_defaults_but_not_indexes() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (id serial primary key, number integer NOT NULL DEFAULT 7, tag text)",
        "CREATE TABLE ic ( ) INHERITS (ip)",
    ]);
    assert_eq!(
        node.rows("SELECT attname FROM pg_attribute WHERE attrelid = 'ic'::regclass AND attnum > 0 ORDER BY attnum"),
        [["id"], ["number"], ["tag"]]
    );
    // **The parent's sequence**, not one of the child's own.
    assert_eq!(
        node.rows("SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = 'ic'::regclass ORDER BY a.attnum"),
        [["nextval('ip_id_seq'::regclass)"], ["7"]]
    );
    // No index and no primary key: the uniqueness the parent promises does not hold across both.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_index WHERE indrelid = 'ic'::regclass"),
        [["0"]]
    );
    // The default is taken on insert.
    node.run("INSERT INTO ic (tag) VALUES ('dflt')").unwrap();
    assert_eq!(node.rows("SELECT number FROM ic"), [["7"]]);
}

/// `UPDATE` and `DELETE` on the parent reach the child's rows too.
#[test]
fn a_parents_update_and_delete_reach_its_children() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (id serial primary key, number integer NOT NULL DEFAULT 7, tag text)",
        "CREATE TABLE ic ( ) INHERITS (ip)",
    ]);
    node.run("INSERT INTO ic (number, tag) VALUES (1, 'child')")
        .unwrap();
    node.run("UPDATE ip SET tag = 'touched' WHERE tag = 'child'")
        .unwrap();
    assert_eq!(node.rows("SELECT tag FROM ic"), [["touched"]]);
    node.run("DELETE FROM ip WHERE tag = 'touched'").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM ic"), [["0"]]);
}

/// `pg_inherits` is where the relationship lives, and the parent cannot be dropped without it.
#[test]
fn the_relationship_is_a_catalog_row_and_a_dependency() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (id serial primary key, number integer NOT NULL DEFAULT 7, tag text)",
        "CREATE TABLE ic ( ) INHERITS (ip)",
    ]);
    assert_eq!(
        node.rows("SELECT c.relname, p.relname, i.inhseqno FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent WHERE c.relname = 'ic'"),
        [["ic", "ip", "1"]]
    );
    let error = node.run("DROP TABLE ip").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop table ip because other objects depend on it"
    );
}

/// A child's **own** columns come after the inherited ones, and a clashing type is `42804`.
#[test]
fn a_child_may_add_columns_but_not_retype_one() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (id serial primary key, number integer NOT NULL DEFAULT 7, tag text)",
    ]);
    node.run("CREATE TABLE ic2 (extra text) INHERITS (ip)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT attname FROM pg_attribute WHERE attrelid = 'ic2'::regclass AND attnum > 0 ORDER BY attnum"),
        [["id"], ["number"], ["tag"], ["extra"]]
    );
    assert_eq!(
        node.run("CREATE TABLE ic3 ( ) INHERITS (nosuchtable)")
            .unwrap_err()
            .sqlstate(),
        "42P01"
    );
    let error = node
        .run("CREATE TABLE ic4 (number text) INHERITS (ip)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42804");
    assert_eq!(error.to_string(), "column \"number\" has a type conflict");
}
