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

const DIVERGENCES: &[(&str, &str, &str)] = &[
    // `serial` was here and is gone: ADR 0033 gave this node an `int4`, which was the whole of
    // the argument for refusing it. The entry's removal is the record that the gap closed --
    // the harness fails a divergence that has started agreeing, so this could not have been
    // absorbed silently.
    // `smallserial` was here too, and went with `int2`. Both serial entries are gone: a serial
    // is not a type, and each was refused only while its integer was missing.
];

/// **Moved out of `DIVERGENCES` by parity rule 4**: the rows agree and one typmod does not.
///
/// It was declared because `information_schema` was a later unit than this corpus — "the answer is
/// worth having on file before the unit that serves it" — and that unit landed. What is left is
/// `is_nullable`: it is `yes_or_no` on a real server, a domain over **`character varying(3)`**,
/// and this node's catalog column list carries a type and no typmod, so it declares
/// `character varying`. The base type is right and the length is not there to declare.
const TYPE_DIVERGENCES: &[&str] = &[concat!(
    "SELECT column_name, is_nullable FROM information_schema.columns ",
    "WHERE table_name = 'z2' ORDER BY ordinal_position"
)];

#[test]
fn every_sequence_statement_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_sequence.txt"),
        FIXTURE,
        &parity::Divergences {
            types: TYPE_DIVERGENCES,
            answers: DIVERGENCES,
        },
    );
    assert!(
        checked > 19,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A serial spelling is its integer plus a sequence, and it is refused only while that integer is
/// missing — by **name**, and by the same sentence the type itself gets.
///
/// [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md) withdrew the refusal of
/// `serial` when `int4` arrived: that refusal had one argument — answering it with an `int8` would
/// accept values a real server refuses — and `int4` emptied it. `smallserial` is `int2` and waits
/// for the same reason, which is why this test still has something to assert. A user who wrote
/// `smallserial` and a user who wrote `int2` have made the same mistake and are told the same
/// thing.
#[test]
fn a_serial_is_refused_only_while_its_integer_is_missing() {
    let mut node = parity::Node::new(&[]);

    // All three widths run now: a serial is its integer plus a sequence, and each was refused
    // only while that integer was missing. `tests/int4.rs` and `tests/int2.rs` assert what they
    // build; here it is enough that none of them is an error any more.
    node.run("CREATE TABLE s2 (id smallserial PRIMARY KEY)")
        .unwrap();
    node.run("CREATE TABLE s4 (id serial PRIMARY KEY)").unwrap();
    node.run("CREATE TABLE s8 (id bigserial PRIMARY KEY)")
        .unwrap();
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

/// A sequence **is** a three-column relation, and a name that is nothing is still `42P01`.
///
/// It was `0A000` naming the construct until the counter learned `is_called`: reporting
/// `last_value` needs the flag, because the counter alone cannot tell `setval(s, 5, false)` from
/// `setval(s, 4, true)`. The three columns are PostgreSQL's own, in its order.
#[test]
fn reading_a_sequence_as_a_relation_is_named_rather_than_missing() {
    let mut node = parity::Node::new(&["CREATE TABLE rel (id bigserial PRIMARY KEY)"]);
    assert_eq!(
        node.rows("SELECT * FROM rel_id_seq"),
        vec![vec!["1", "0", "f"]],
        "a fresh sequence: the start value, and nothing handed out"
    );
    // And a name that really is nothing is still `42P01`.
    assert_eq!(
        node.run("SELECT * FROM nothing_at_all")
            .unwrap_err()
            .sqlstate(),
        sqlstate::UNDEFINED_TABLE
    );
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
