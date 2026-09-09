//! `NULLS NOT DISTINCT`, against PostgreSQL 19beta1 — statement 195 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_class.relname` is a `name` on a real server and `text` here — the standing trade every
    // `pg_catalog` column makes. The rows agree, `indnullsnotdistinct` included.
    types: &[],
    // The one entry here recorded that `ALTER TABLE … ADD CONSTRAINT … UNIQUE` did not exist and
    // so "both refuse" — which stopped being true twice over. The action was built, and then it
    // learned to scan the rows already there, so the line now fails on the *data* the way
    // PostgreSQL does: two NULLs in `a` under `NULLS NOT DISTINCT`. Deleted (ADR 0031 rule 2);
    // `unique_over_existing_rows.rs` is where that scan is measured.
    answers: &[],
};

#[test]
fn every_nulls_not_distinct_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_nulls_not_distinct.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 18,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The default and the clause, over **the same pair of NULLs** in one table.
///
/// This is the whole feature: rows 2 and 4 have `a = NULL` under a plain unique index and are both
/// admitted, and `b = NULL` under a `NULLS NOT DISTINCT` one where the second is `23505`. An
/// implementation that stored the flag and never read it passes every catalog test and fails here.
#[test]
fn the_same_two_nulls_collide_under_one_index_and_not_the_other() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nd (id int8 PRIMARY KEY, a int8, b int8)",
        "CREATE UNIQUE INDEX nd_plain ON nd (a)",
        "CREATE UNIQUE INDEX nd_nnd ON nd (b) NULLS NOT DISTINCT",
        "INSERT INTO nd VALUES (1, NULL, 1)",
    ] {
        node.run(statement).unwrap();
    }
    // A second NULL in `a` is fine; a second NULL in `b` is not.
    node.run("INSERT INTO nd VALUES (2, NULL, 2)").unwrap();
    node.run("INSERT INTO nd VALUES (3, NULL, NULL)").unwrap();
    let error = node
        .run("INSERT INTO nd VALUES (4, NULL, NULL)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.detail().as_deref(),
        Some("Key (b)=(null) already exists."),
        "the NULL is written `null`, unquoted"
    );
}

/// An `UPDATE` into the NULL is checked too — the half that only hooks `INSERT` misses.
#[test]
fn an_update_into_the_null_collides() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nd (id int8 PRIMARY KEY, b int8)",
        "CREATE UNIQUE INDEX nd_nnd ON nd (b) NULLS NOT DISTINCT",
        "INSERT INTO nd VALUES (1, NULL)",
        "INSERT INTO nd VALUES (2, 5)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("UPDATE nd SET b = NULL WHERE id = 2").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    // The row that already held the NULL may still be moved off it, and then the other may take it.
    node.run("UPDATE nd SET b = 9 WHERE id = 1").unwrap();
    node.run("UPDATE nd SET b = NULL WHERE id = 2").unwrap();
    assert_eq!(
        node.rows("SELECT id, b FROM nd ORDER BY id"),
        vec![vec!["1", "9"], vec!["2", "\\N"]]
    );
}

/// The clause survives the catalog record, prints where a real server prints it, and `NULLS
/// DISTINCT` written out prints nothing because it is the default.
#[test]
fn it_is_stored_printed_after_the_key_and_before_the_where() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE nd (id int8 PRIMARY KEY, a int8, b int8, c int8)",
        "CREATE UNIQUE INDEX nd_nnd ON nd (b) NULLS NOT DISTINCT",
        "CREATE UNIQUE INDEX nd_d ON nd (c) NULLS DISTINCT",
        "CREATE UNIQUE INDEX nd_both ON nd (a, b) NULLS NOT DISTINCT WHERE c IS NOT NULL",
        // Accepted on a non-unique index too, where it can refuse nothing.
        "CREATE INDEX nd_plainly ON nd (c) NULLS NOT DISTINCT",
    ] {
        node.run(statement).unwrap();
    }
    for (name, printed) in [
        (
            "nd_nnd",
            "CREATE UNIQUE INDEX nd_nnd ON public.nd USING btree (b) NULLS NOT DISTINCT",
        ),
        (
            "nd_d",
            "CREATE UNIQUE INDEX nd_d ON public.nd USING btree (c)",
        ),
        (
            "nd_both",
            "CREATE UNIQUE INDEX nd_both ON public.nd USING btree (a, b) NULLS NOT DISTINCT WHERE \
             (c IS NOT NULL)",
        ),
        (
            "nd_plainly",
            "CREATE INDEX nd_plainly ON public.nd USING btree (c) NULLS NOT DISTINCT",
        ),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_get_indexdef('{name}'::regclass)")),
            [[printed]],
            "for {name}"
        );
    }
    // A column of `pg_index`, not part of the key.
    assert_eq!(
        node.rows(
            "SELECT i.relname, x.indnullsnotdistinct FROM pg_index x JOIN pg_class i ON i.oid = \
             x.indexrelid WHERE x.indrelid = 'nd'::regclass ORDER BY i.relname"
        ),
        vec![
            vec!["nd_both", "t"],
            vec!["nd_d", "f"],
            vec!["nd_nnd", "t"],
            vec!["nd_pkey", "f"],
            vec!["nd_plainly", "t"],
        ]
    );
}
