//! **`pk_and_sequence_for` and `reset_pk_sequence!` over a schema-qualified table.**
//!
//! Two `schema_test.rb` tests. The **dependency** half was already passing at HEAD when this was
//! written — failing at `06291ca5`, gone by `47ffc338`, closed by something in between — and this
//! file pins it, because nothing else asserted it and a fix nobody tested is a fix that regresses
//! quietly. The **fallback** half was not passing, and is the unit.
//!
//! # What `ActiveRecord` runs, and which query answers
//!
//! `pk_and_sequence_for` tries a **dependency** query first — the sequence `pg_depend` links to
//! the primary key's column — and falls back to parsing the printed `nextval(…)` default.
//!
//! Both queries earn their place, on a real server as much as here. PostgreSQL answers the
//! dependency query with **0 rows** for `table_with_unmatched_sequence_for_pk` — measured —
//! because a sequence merely named in a `DEFAULT` has no `pg_depend` row pointing at the column.
//! So the fallback is not a workaround for a catalog row this node is missing; it is the path
//! PostgreSQL itself takes for that table, and it was unreachable here while `split_part`,
//! `strpos` and `substr` answered `0A000`.
//!
//! The failure that caused was silent, which is why it is worth a test: `pk_and_sequence_for`
//! returning nothing reads as "no sequence", and `reset_pk_sequence!` treats that as nothing
//! to do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA test_schema",
    "CREATE TABLE test_schema.table_with_pk (id serial primary key)",
    "CREATE SEQUENCE test_schema.unmatched_primary_key_default_value_seq",
    "CREATE TABLE test_schema.table_with_unmatched_sequence_for_pk (id integer NOT NULL \
     DEFAULT nextval('test_schema.unmatched_primary_key_default_value_seq'::regclass), \
     CONSTRAINT unmatched_pkey PRIMARY KEY (id))",
];

/// AR's dependency query, verbatim, for both tables — and the **schema comes back with the
/// sequence**, which is what `Name(schema, seq)` is built from.
#[test]
fn the_sequence_is_found_with_its_schema() {
    let mut node = parity::Node::new(FIXTURE);
    // **Only the owned one.** PostgreSQL answers 0 rows for the unmatched table too — measured,
    // because a sequence merely named in a `DEFAULT` has no `pg_depend` row pointing at the
    // column — which is why `ActiveRecord` has a fallback at all.
    let (table, sequence) = ("\"test_schema\".\"table_with_pk\"", "table_with_pk_id_seq");
    assert_eq!(
        node.rows(&format!(
            "SELECT attr.attname, nsp.nspname, seq.relname \
                 FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, \
                 pg_namespace nsp \
                 WHERE seq.oid = dep.objid AND seq.relkind = 'S' \
                 AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid \
                 AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] \
                 AND seq.relnamespace = nsp.oid AND cons.contype = 'p' \
                 AND dep.classid = 'pg_class'::regclass \
                 AND dep.refobjid = '{table}'::regclass"
        )),
        [[
            "id".to_owned(),
            "test_schema".to_owned(),
            sequence.to_owned(),
        ]],
        "{table}"
    );
}

/// `reset_pk_sequence!`'s own statements, in order, over a qualified sequence — ending where the
/// suite asserts it ends.
#[test]
fn resetting_a_qualified_sequence_starts_it_over() {
    const SEQ: &str = "test_schema.unmatched_primary_key_default_value_seq";
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(&format!("SELECT setval('{SEQ}', 123)")),
        [["123".to_owned()]]
    );
    assert_eq!(
        node.rows(&format!("SELECT nextval('{SEQ}')")),
        [["124".to_owned()]]
    );

    // The table is empty, so `reset_pk_sequence!` takes the sequence's minimum and sets it
    // `is_called = false` — which is the branch the suite exercises.
    assert_eq!(
        node.rows("SELECT MAX(id) FROM test_schema.table_with_unmatched_sequence_for_pk"),
        [["\\N".to_owned()]]
    );
    assert_eq!(
        node.rows(&format!(
            "SELECT seqmin FROM pg_sequence WHERE seqrelid = '{SEQ}'::regclass"
        )),
        [["1".to_owned()]]
    );
    node.run(&format!("SELECT setval('{SEQ}', 1, false)"))
        .unwrap();
    assert_eq!(
        node.rows(&format!("SELECT nextval('{SEQ}')")),
        [["1".to_owned()]],
        "the suite asserts exactly this"
    );
}

/// **The fallback, which is the unit** — and it runs now.
///
/// PostgreSQL answers the dependency query with 0 rows for
/// `table_with_unmatched_sequence_for_pk` as well — measured — because a sequence merely named in
/// a `DEFAULT` has no `pg_depend` row pointing at the column. `pg_depend` there holds two rows and
/// neither is the one the query wants: the sequence depends on its schema, and the *default
/// expression* depends on the sequence.
///
/// So a real server reaches `pk_and_sequence_for`'s second query, which recovers the name out of
/// the printed default. That query needs `split_part`, `strpos` and `substr`; all three answered
/// `0A000` here, and that — not the dependency query — is what stopped these two tests.
#[test]
fn the_fallback_recovers_the_sequence_from_the_printed_default() {
    let mut node = parity::Node::new(FIXTURE);
    // `ActiveRecord`'s `CASE`, in shape: the name sits between the first pair of quotes in the
    // printed default, and the schema is stripped off it when there is one.
    let rows = node.rows(
        "SELECT CASE \
           WHEN pg_get_expr(def.adbin, def.adrelid) !~* 'nextval' THEN NULL \
           WHEN split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2) ~ '.' THEN \
             substr(split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2), \
                    strpos(split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2), '.') + 1) \
           ELSE split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2) \
         END \
         FROM pg_attrdef def \
         WHERE def.adrelid = 'test_schema.table_with_unmatched_sequence_for_pk'::regclass",
    );
    assert_eq!(
        rows,
        [["unmatched_primary_key_default_value_seq".to_owned()]],
        "the sequence a real server recovers here"
    );
}

/// The three functions that query needs, at the edges that decide them — measured on 19beta1.
#[test]
fn the_three_string_functions_answer_as_postgresql_does() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        ("SELECT split_part('a.b.c', '.', 2)", "b"),
        ("SELECT split_part('a.b.c', '.', 9)", ""),
        ("SELECT split_part('abc', '.', 1)", "abc"),
        ("SELECT split_part('a.b.c', '.', -1)", "c"),
        ("SELECT split_part('a.b.c', '.', -3)", "a"),
        ("SELECT split_part('a..b', '.', 2)", ""),
        ("SELECT split_part('abc', '', 1)", "abc"),
        ("SELECT strpos('hello', 'll')", "3"),
        ("SELECT strpos('hello', 'z')", "0"),
        ("SELECT strpos('hello', '')", "1"),
        ("SELECT substr('hello', 2)", "ello"),
        ("SELECT substr('hello', 2, 2)", "el"),
        ("SELECT substr('hello', 0)", "hello"),
        // The clamp: positions -1, 0 and 1, of which only 1 exists.
        ("SELECT substr('hello', -1, 3)", "h"),
        ("SELECT substr('hello', 9)", ""),
    ] {
        assert_eq!(node.rows(sql), [[expected.to_owned()]], "{sql}");
    }
    // And the one argument a real server refuses outright, with its own sentence.
    assert_eq!(
        node.answer("SELECT split_part('a.b', '.', 0)").to_string(),
        "!22023 field position must not be zero"
    );
}
