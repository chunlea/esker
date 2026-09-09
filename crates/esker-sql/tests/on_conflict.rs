//! `INSERT … ON CONFLICT` — what `insert_all` and `upsert_all` compile to, and what all of it does
//! through a **partitioned** parent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`pg_typeof` answers a `regtype` on both now** (ADR 0093): it is resolved at plan
    // time from the argument's declared type, so what this list recorded has no difference
    // left in it.
    types: &[],
    answers: &[],
};

#[test]
fn every_on_conflict_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_on_conflict.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`upsert_all` through a partitioned parent**: routed first, arbitrated on the partition's own
/// index, in one statement that both updates and inserts.
#[test]
fn a_conflict_through_a_partitioned_parent_is_routed_then_arbitrated() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL, \
         peaktemp integer, unitsales integer) PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX index_measurements_on_logdate_and_city_id ON measurements (logdate, \
         city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
        "CREATE TABLE measurements_concepcion PARTITION OF measurements FOR VALUES IN (2)",
        "INSERT INTO measurements (city_id, logdate, peaktemp, unitsales) VALUES ('1', \
         '2026-09-01', 30, 10)",
    ]);
    node.run(
        "INSERT INTO measurements (city_id, logdate, peaktemp, unitsales) VALUES \
         ('1','2026-09-01',99,99),('2','2026-09-02',2,2),('2','2026-09-03',0,0) ON CONFLICT \
         (logdate, city_id) DO UPDATE SET peaktemp = excluded.peaktemp, unitsales = \
         excluded.unitsales",
    )
    .unwrap();
    // One updated in place, two inserted where they belong — from one statement.
    assert_eq!(
        node.rows("SELECT count(*) FROM measurements_toronto"),
        [["1"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM measurements_concepcion"),
        [["2"]]
    );
    assert_eq!(
        node.rows("SELECT city_id, peaktemp FROM measurements ORDER BY city_id, logdate"),
        vec![vec!["1", "99"], vec!["2", "2"], vec!["2", "0"],]
    );
    // Straight into the partition, arbitrated on *its* index.
    node.run(
        "INSERT INTO measurements_toronto (city_id, logdate, peaktemp) VALUES ('1','2026-09-01',7) \
         ON CONFLICT (logdate, city_id) DO UPDATE SET peaktemp = excluded.peaktemp",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT peaktemp FROM measurements_toronto"),
        [["7"]]
    );
}

/// **Routing wins over `DO NOTHING`.** The clause never gets a chance: there is no partition whose
/// index could arbitrate.
#[test]
fn a_row_no_partition_takes_is_still_refused_under_do_nothing() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL) \
         PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX m_idx ON measurements (logdate, city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
    ]);
    let error = node
        .run(
            "INSERT INTO measurements (city_id, logdate) VALUES ('99','2026-09-09') ON CONFLICT \
             (logdate, city_id) DO NOTHING",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    assert_eq!(
        error.to_string(),
        "no partition of relation \"measurements\" found for row"
    );
}

/// **`DO UPDATE` cannot move a row between partitions, where a plain `UPDATE` can.**
#[test]
fn a_conflict_update_that_would_move_the_row_is_refused() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL, \
         peaktemp integer) PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX m_idx ON measurements (logdate, city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
        "CREATE TABLE measurements_concepcion PARTITION OF measurements FOR VALUES IN (2)",
        "INSERT INTO measurements (city_id, logdate, peaktemp) VALUES ('1','2026-09-01',30)",
    ]);
    // The key set to what it already holds is fine.
    node.run(
        "INSERT INTO measurements (city_id, logdate, peaktemp) VALUES ('1','2026-09-01',5) ON \
         CONFLICT (logdate, city_id) DO UPDATE SET city_id = excluded.city_id",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM measurements"), [["1"]]);
    // Set to another partition's value, it is `0A000` — and a plain `UPDATE` moves the same row.
    let error = node
        .run(
            "INSERT INTO measurements (city_id, logdate, peaktemp) VALUES ('1','2026-09-01',5) ON \
             CONFLICT (logdate, city_id) DO UPDATE SET city_id = '2'",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(error.to_string(), "invalid ON UPDATE specification");
    assert_eq!(
        error.detail().as_deref(),
        Some("The result tuple would appear in a different partition than the original tuple.")
    );
    node.run("UPDATE measurements SET city_id = '2' WHERE city_id = '1'")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM measurements_concepcion"),
        [["1"]]
    );
}

/// **The sequence is consumed for a row that is never inserted**, and `RETURNING` answers with the
/// row the statement actually wrote.
#[test]
fn the_sequence_advances_for_a_row_that_conflicts() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE books (id bigserial primary key, isbn character varying, name character \
         varying)",
        "CREATE UNIQUE INDEX index_books_on_isbn ON books (isbn)",
        "INSERT INTO books (isbn, name) VALUES ('1', 'Rework')",
    ]);
    // The conflicting insert draws a number, does not use it, and returns the **existing** id.
    assert_eq!(
        node.rows(
            "INSERT INTO books (isbn, name) VALUES ('1', 'Remote') ON CONFLICT (isbn) DO UPDATE \
             SET name = excluded.name RETURNING id"
        ),
        [["1"]]
    );
    // …and the number it drew is gone: the next row is `3`, not `2`.
    assert_eq!(
        node.rows("INSERT INTO books (isbn, name) VALUES ('9', 'Sprint') RETURNING id"),
        [["3"]]
    );
    // `DO NOTHING` on a conflicting row returns **no rows at all**.
    assert_eq!(
        node.rows(
            "INSERT INTO books (isbn, name) VALUES ('1', 'x') ON CONFLICT (isbn) DO NOTHING \
             RETURNING id"
        ),
        Vec::<Vec<String>>::new()
    );
}

/// **Two proposed rows that conflict with each other are two different answers.**
#[test]
fn two_proposed_rows_that_collide_are_21000_only_under_do_update() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE books (id bigserial primary key, isbn character varying, name character \
         varying)",
        "CREATE UNIQUE INDEX index_books_on_isbn ON books (isbn)",
    ]);
    let error = node
        .run(
            "INSERT INTO books (isbn, name) VALUES ('9','a'),('9','b') ON CONFLICT (isbn) DO \
             UPDATE SET name = excluded.name",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "21000");
    assert_eq!(
        error.to_string(),
        "ON CONFLICT DO UPDATE command cannot affect row a second time"
    );
    // `DO NOTHING` takes the pair and the **first** wins.
    node.run(
        "INSERT INTO books (isbn, name) VALUES ('9','a'),('9','b') ON CONFLICT (isbn) DO NOTHING",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT name FROM books WHERE isbn = '9'"),
        [["a"]]
    );
}

/// **The target names columns and the index is inferred from them.**
#[test]
fn a_target_that_matches_no_unique_index_is_42p10() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE books (id bigserial primary key, isbn character varying, name character \
         varying)",
        "CREATE UNIQUE INDEX index_books_on_isbn ON books (isbn)",
    ]);
    let error = node
        .run("INSERT INTO books (isbn, name) VALUES ('1','x') ON CONFLICT (name) DO NOTHING")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P10");
    assert_eq!(
        error.to_string(),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
    // A column that is not there at all is the ordinary `42703`, **quoted**.
    let error = node
        .run("INSERT INTO books (isbn, name) VALUES ('1','x') ON CONFLICT (nosuchcol) DO NOTHING")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42703");
    assert_eq!(error.to_string(), "column \"nosuchcol\" does not exist");
    // …and `excluded.nosuchcol` is **qualified and unquoted**, which is a different sentence.
    let error = node
        .run(
            "INSERT INTO books (isbn, name) VALUES ('1','x') ON CONFLICT (isbn) DO UPDATE SET \
             name = excluded.nosuchcol",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42703");
    assert_eq!(
        error.to_string(),
        "column excluded.nosuchcol does not exist"
    );
}

/// **Both rows are in scope**: `excluded.c` is the proposed row, the table's own name is the row
/// already there.
#[test]
fn the_set_sees_both_the_existing_row_and_the_proposed_one() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE books (id bigserial primary key, isbn character varying, name character \
         varying)",
        "CREATE UNIQUE INDEX index_books_on_isbn ON books (isbn)",
        "INSERT INTO books (isbn, name) VALUES ('1', 'Rework')",
    ]);
    // Assigning the table's own column back is a no-op that proves which row it read.
    node.run(
        "INSERT INTO books (isbn, name) VALUES ('1','x') ON CONFLICT (isbn) DO UPDATE SET name = \
         books.name",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT name FROM books"), [["Rework"]]);
    node.run(
        "INSERT INTO books (isbn, name) VALUES ('1','x') ON CONFLICT (isbn) DO UPDATE SET name = \
         excluded.name",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT name FROM books"), [["x"]]);
}

/// **The shim is one statement's.** The `WHERE` of an `ON CONFLICT (…) WHERE …` comes off the
/// text before the parser sees it and travels on the parsed statement — on *every* statement the
/// message produced, so a second `INSERT … ON CONFLICT` in the same message inherited the first
/// one's predicate and a bare target over a partial index was inferred where a real server says
/// `42P10`. A message carrying two `ON CONFLICT`s with a shim in play is refused by name; one at a
/// time, which is all `ActiveRecord` sends, is unchanged.
#[test]
fn a_message_with_two_on_conflicts_and_a_shim_is_refused_by_name() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE oc2 (isbn text, published_on date)",
        "CREATE UNIQUE INDEX oc2_isbn ON oc2 (isbn) WHERE published_on IS NOT NULL",
    ]);
    node.run(
        "INSERT INTO oc2 VALUES ('a', NULL) ON CONFLICT (isbn) WHERE (published_on IS NOT NULL) \
         DO NOTHING",
    )
    .unwrap();
    let error = node
        .run(
            "INSERT INTO oc2 VALUES ('b', NULL) ON CONFLICT (isbn) WHERE (published_on IS NOT NULL) \
             DO NOTHING; INSERT INTO oc2 VALUES ('c', NULL) ON CONFLICT (isbn) DO NOTHING",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000", "{error}");
    assert!(
        error.to_string().contains("more than one ON CONFLICT"),
        "{error}"
    );
}
