//! A `DESC` index column, against PostgreSQL 19beta1 — statement 189 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_class.relname` is a `name` on a real server and `text` here — the type this node does
    // not have, provided where the client's use of it is text-shaped. The rows agree.
    types: &[
        "SELECT i.relname, x.indkey::text, x.indoption::text FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = 'xdsc'::regclass ORDER BY i.relname",
    ],
    // Two, both **refusals with a different code** — this node refuses everything PostgreSQL
    // refuses, and reaches the refusal by a different road. Neither is an answer where a real
    // server raises, which is the class ADR 0031 ranks worst.
    answers: &[
        (
            "ALTER TABLE xdsc ADD CONSTRAINT xdsc_uc UNIQUE (a DESC)",
            "PostgreSQL stops at the grammar (42601, a constraint's columns take no direction);              this node stops one step earlier at ALTER TABLE ... ADD CONSTRAINT ... UNIQUE, which              it does not have at all (0A000). The valid form is the gap, not the direction",
            "UNMEASURED",
        ),
        (
            "CREATE INDEX xdsc_bad2 ON xdsc (a NULLS)",
            "PostgreSQL reads the bare word as an operator class and answers 42704 operator class              \"nulls\" does not exist; sqlparser cannot read it at all, so this node answers              42601. An index operator class is refused by name here either way",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_desc_index_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_desc_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// All four direction/null-placement combinations, and the shortest spelling of each.
///
/// The rule that cannot be got right by keeping the text: `ASC` never prints, `NULLS FIRST` prints
/// only under ascending, and `NULLS LAST` only under descending — because each direction has its
/// own default and the definition records the difference from it.
#[test]
fn the_definition_prints_only_what_differs_from_the_direction_s_default() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE xd (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for (name, written, printed) in [
        ("xd_plain", "a", "a"),
        ("xd_asc", "a ASC", "a"),
        ("xd_asc_last", "a ASC NULLS LAST", "a"),
        ("xd_asc_first", "a ASC NULLS FIRST", "a NULLS FIRST"),
        ("xd_desc", "a DESC", "a DESC"),
        ("xd_desc_first", "a DESC NULLS FIRST", "a DESC"),
        ("xd_desc_last", "a DESC NULLS LAST", "a DESC NULLS LAST"),
    ] {
        node.run(&format!("CREATE INDEX {name} ON xd ({written})"))
            .unwrap();
        assert_eq!(
            node.rows(&format!("SELECT pg_get_indexdef('{name}'::regclass)")),
            [[format!(
                "CREATE INDEX {name} ON public.xd USING btree ({printed})"
            )]],
            "written as {written}"
        );
    }
}

/// `indoption` is the bitmask, per key part, in key order.
///
/// PostgreSQL's own `INDOPTION_DESC | INDOPTION_NULLS_FIRST`, and the column that says what the
/// definition text spells out. The record round trip under it is
/// `catalog::tests::an_index_key_keeps_its_order_through_a_record`.
#[test]
fn indoption_is_the_bitmask_for_each_key_part() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xd (id int8 PRIMARY KEY, a int8, b text)",
        "CREATE INDEX xd_mix ON xd (b, a DESC NULLS LAST)",
        "CREATE INDEX xd_desc ON xd (a DESC)",
        "CREATE INDEX xd_first ON xd (a NULLS FIRST)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT i.relname, x.indoption FROM pg_index x JOIN pg_class i ON i.oid =              x.indexrelid WHERE x.indrelid = 'xd'::regclass ORDER BY i.relname"
        ),
        [
            vec!["xd_desc", "3"],
            vec!["xd_first", "2"],
            vec!["xd_mix", "0 1"],
            vec!["xd_pkey", "0"],
        ]
    );
}

/// A `DESC` index enforces its `UNIQUE` and answers a lookup exactly as an ascending one does.
///
/// The claim the order does **not** change: uniqueness is a property of the set of keys and not of
/// their order, and this node's only index read is a lookup with the whole key pinned. If a later
/// unit chooses an index to satisfy an `ORDER BY`, this is the test that will need a second half.
#[test]
fn the_direction_changes_nothing_a_read_can_see() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xd (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xd_u ON xd (b DESC)",
        "INSERT INTO xd VALUES (1, 'alpha')",
        "INSERT INTO xd VALUES (2, 'beta')",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("INSERT INTO xd VALUES (3, 'alpha')").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(node.rows("SELECT id FROM xd WHERE b = 'beta'"), [["2"]]);
    assert_eq!(
        node.rows("SELECT id FROM xd ORDER BY b"),
        [["1"], ["2"]],
        "an ORDER BY is the query's, not the index's"
    );
}

/// A constraint's columns take no direction, because a real server's grammar has none.
#[test]
fn a_constraint_column_takes_no_direction() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE xd (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for statement in [
        "ALTER TABLE xd ADD CONSTRAINT xd_uc UNIQUE (a DESC)",
        "CREATE TABLE xe (a int8, UNIQUE (a DESC))",
        "CREATE TABLE xf (a int8, PRIMARY KEY (a NULLS FIRST))",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            "0A000",
            "{statement} — PostgreSQL says 42601; the divergence is the code, not the refusal"
        );
    }
}
