//! Contract C3 for `bigserial` and identity columns: replay `tests/corpus/pg19_sequence.txt`.
//!
//! Twenty-one statements put to a real PostgreSQL 19beta1 in order and replayed statefully, which
//! is the only way to see what a sequence actually does: every rule here is about what a *previous*
//! statement left behind. The three worth reading twice are at the top of the corpus file.
//!
//! Two divergences, both `0A000` and both the same argument. `serial` is `integer` under another
//! name and `smallserial` is `smallint`; this crate has neither, and answering with an `int8` would
//! take every value between 2^31 and 2^63 that a real server refuses with `22003` — which is ADR
//! 0031's rule in the direction it cares most about, accepting what the oracle rejects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables, because what they are is the thing under test.
const FIXTURE: &[&str] = &[];

const DIVERGENCES: &[(&str, &str)] = &[
    (
        "CREATE TABLE z4 (id serial PRIMARY KEY)",
        "serial is int4 under another name and this crate has no int4; an int8 would accept every \
         value between 2^31 and 2^63 that a real server answers 22003 for",
    ),
    (
        "CREATE TABLE z5 (id smallserial PRIMARY KEY)",
        "smallserial is int2, and the same argument",
    ),
    (
        concat!(
            "SELECT column_name, is_nullable FROM information_schema.columns ",
            "WHERE table_name = 'z2' ORDER BY ordinal_position"
        ),
        "information_schema is unit 5. Captured here because it is how a client asks whether the \
         identity column came out NOT NULL, and the answer is worth having on file before the \
         unit that serves it",
    ),
];

#[test]
fn every_sequence_statement_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_sequence.txt"),
        FIXTURE,
        &parity::Divergences {
            types: &[],
            answers: DIVERGENCES,
        },
    );
    assert!(
        checked > 19,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The two serial spellings this node will not take are refused by **name**, and by the same
/// sentence the type they stand for already gets. A user who wrote `serial` and a user who wrote
/// `int4` have made the same mistake and should be told the same thing.
#[test]
fn serial_is_refused_the_way_its_type_is() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "CREATE TABLE s (id serial PRIMARY KEY)",
        "CREATE TABLE s (id smallserial PRIMARY KEY)",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{sql} -> {error}"
        );
        assert!(
            error.to_string().to_ascii_lowercase().contains("serial"),
            "{sql} -> `{error}`, which does not name what was written"
        );
    }
}

/// A sequence is dropped with the table that owns it, and its **name** goes with it — so the name
/// is free afterwards. PostgreSQL does the same without being asked: `DROP SEQUENCE` on an owned
/// one is `2BP01` naming the table that depends on it.
#[test]
fn dropping_a_table_frees_its_sequences_name() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE d (id bigserial PRIMARY KEY)")
        .unwrap();
    let taken = node.run("CREATE TABLE d_id_seq (a int8)").unwrap_err();
    assert_eq!(taken.sqlstate(), sqlstate::DUPLICATE_TABLE, "{taken}");

    node.run("DROP TABLE d").unwrap();
    node.run("CREATE TABLE d_id_seq (a int8)")
        .expect("the sequence's name went with the table");
}

/// A failed `INSERT` still consumed its value, here as on a real server.
///
/// That is not a side-effect of how this is built, it is what `nextval` **is**: it runs outside
/// the statement's transaction, so nothing that undoes the statement undoes it. It is also the
/// licence for a sequence to leave gaps at all, and therefore for
/// [`esker_sql::catalog::SEQUENCE_BATCH`] to be larger than one.
///
/// A statement that fails rather than a `ROLLBACK`, because transaction control belongs to the
/// session and this harness drives the executor. The property is the same one: the value is gone
/// and the row is not there.
#[test]
fn a_failed_insert_consumes_its_value() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE g (id bigserial PRIMARY KEY, n text NOT NULL)")
        .unwrap();
    node.run("INSERT INTO g (n) VALUES ('first')").unwrap();
    node.run("INSERT INTO g (n) VALUES (NULL)").unwrap_err();
    node.run("INSERT INTO g (n) VALUES ('third')").unwrap();

    let rows = node.rows("SELECT id, n FROM g ORDER BY id");
    assert_eq!(rows.len(), 2, "the failed row is not there: {rows:?}");
    assert_eq!(rows[0], ["1", "first"]);
    // Whatever the second row's id is, it is **not** 2: the failed statement took that one.
    assert_ne!(rows[1][0], "2", "a failed INSERT gave its value back");
    assert_eq!(rows[1][1], "third");
}
