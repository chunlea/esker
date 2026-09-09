//! `ALTER TABLE … ALTER COLUMN … SET DEFAULT` — statement 755.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** `attname` is a `name` here (ADR 0084) and `attidentity` a `"char"` (ADR 0095) —
    // the two the entry named — so the declared type agrees with the character.
    types: &[],
    answers: &[],
};

#[test]
fn every_set_default_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_set_default.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 24,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The suite's three statements in a row, and the state each one leaves.**
///
/// This is the sequence that clears two refusals at once: pointing the column at the new sequence
/// is what makes the old one droppable, and a node that added a second default instead of
/// replacing the first would answer `2BP01` on the third line — PostgreSQL's own correct answer to
/// a state it should not be in, which is the hardest kind of wrong to notice.
#[test]
fn set_default_replaces_the_bigserials_own_and_frees_its_sequence() {
    let mut node = parity::Node::new(&["CREATE TABLE sd (id bigserial primary key, t text)"]);
    node.run("CREATE SEQUENCE sdseq START 101 OWNED BY sd.id")
        .unwrap();
    // Before: the column's default is its own sequence.
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'sd'::regclass"
        ),
        [["nextval('sd_id_seq'::regclass)"]]
    );
    // While it is, the sequence cannot be dropped.
    assert_eq!(
        node.run("DROP SEQUENCE sd_id_seq").unwrap_err().sqlstate(),
        "2BP01"
    );

    node.run("ALTER TABLE sd ALTER COLUMN id SET DEFAULT nextval('sdseq')")
        .unwrap();

    // After: **one** row, and it is the new sequence.
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'sd'::regclass"
        ),
        [["nextval('sdseq'::regclass)"]],
        "replaced, not added to"
    );
    // The row comes from the new counter, which is what proves the swap rather than the rendering.
    node.run("INSERT INTO sd (t) VALUES ('a')").unwrap();
    assert_eq!(node.rows("SELECT id FROM sd"), [["101"]]);
    // And the original sequence is now droppable without CASCADE.
    node.run("DROP SEQUENCE IF EXISTS sd_id_seq").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sd_id_seq'"),
        [["0"]]
    );
}

/// A constant, an expression, and `DROP DEFAULT` — which is idempotent and leaves NULL behind.
#[test]
fn a_default_can_be_set_to_a_constant_an_expression_or_nothing() {
    let mut node =
        parity::Node::new(&["CREATE TABLE sd (id bigserial primary key, n integer, t text)"]);
    node.run("ALTER TABLE sd ALTER COLUMN n SET DEFAULT 7")
        .unwrap();
    node.run("ALTER TABLE sd ALTER COLUMN t SET DEFAULT concat('x', 'y')")
        .unwrap();
    node.run("INSERT INTO sd (id) VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT n, t FROM sd"), [["7", "xy"]]);

    node.run("ALTER TABLE sd ALTER COLUMN n DROP DEFAULT")
        .unwrap();
    // Idempotent: a column with no default takes it again without complaint.
    node.run("ALTER TABLE sd ALTER COLUMN n DROP DEFAULT")
        .unwrap();
    node.run("INSERT INTO sd (id) VALUES (2)").unwrap();
    assert_eq!(
        node.rows("SELECT n FROM sd WHERE id = 2"),
        [["\\N"]],
        "NULL, not the 7 it used to default to"
    );
}

/// Four refusals, four codes — and the `DEFAULT` rules are the same ones `CREATE TABLE` applies.
#[test]
fn the_refusals_are_postgresqls() {
    let mut node = parity::Node::new(&["CREATE TABLE sd (id bigserial primary key, n integer)"]);
    for (statement, sqlstate, message) in [
        (
            "ALTER TABLE sd ALTER COLUMN nosuchcol SET DEFAULT 1",
            "42703",
            "column \"nosuchcol\" of relation \"sd\" does not exist",
        ),
        (
            "ALTER TABLE nosuchtable ALTER COLUMN n SET DEFAULT 1",
            "42P01",
            "relation \"nosuchtable\" does not exist",
        ),
        (
            "ALTER TABLE sd ALTER COLUMN n SET DEFAULT nextval('nosuchseq')",
            "42P01",
            "relation \"nosuchseq\" does not exist",
        ),
        (
            "ALTER TABLE sd ALTER COLUMN n SET DEFAULT id",
            "0A000",
            "cannot use column reference in DEFAULT expression",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate, "for {statement}");
        assert_eq!(error.to_string(), message, "for {statement}");
    }
    // A literal the column's type will not take is that type's own input error.
    assert_eq!(
        node.run("ALTER TABLE sd ALTER COLUMN n SET DEFAULT 'not a number'")
            .unwrap_err()
            .sqlstate(),
        "22P02"
    );
}
