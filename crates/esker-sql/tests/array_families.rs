//! **The three array families r1's wire sweep left**, and two of the three were not what the
//! report said.
//!
//! `rs-wire-107b.txt` §37–48 filed `array_agg(c::regtype)` answering an internal error,
//! `ARRAY[…] || ARRAY[…]` refused on `regtype[]` and `name[]`, and `oid[]` unsupported in every
//! shape. Measured first (`tests/captures/pg19_array_families.txt`):
//!
//! * **The first is not about arrays.** `SELECT t::regtype FROM t` — no constructor in sight —
//!   answered `an oid is an integer, not Text("int4")`, an internal representation in a user's
//!   face. A `regtype` has an input function and an output one and which a cast needs is decided
//!   by **what it casts from**; this node sent both directions to the oid one, so a text column
//!   reached it as a `Datum::Text`. The three array shapes were that defect seen through a
//!   constructor.
//! * **The second is not about `regtype[]` or `name[]`.** `text[] || text[]` was refused too:
//!   there was no array `||` at all, which is a whole operator rather than two element types.
//! * **The third was already closed**, by the `oid` unit
//!   ([ADR 0097](../../../docs/adr/0097-an-oid-is-four-bytes-and-a-derived-one-is-not.md)).
//!   Kept here so it cannot quietly go away again.
//!
//! Neither correction was reachable from the report: each needed the shape the report named to be
//! put to the oracle and to this node side by side.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 'r', 1 || 2",
        "**The refusal is the same and it names a different width.** A real server says \
         `integer || integer` and this node `bigint || bigint`: an integer literal is declared \
         `int4` here too (ADR 0087) but its *datum* stays an `i64`, and `exec::cursor`'s \
         `text_concat` reads the operand's name off the datum — the one place in this crate where \
         a refusal's message is built after the declared type has stopped travelling. The code \
         and the sentence agree; one word does not. Kept in this corpus rather than moved out of \
         it, because the row is what says adding the array operator did not swallow the refusal.",
        "pg19_array_families.txt:73",
    )],
};

#[test]
fn every_array_family_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_families.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Which direction a `::regtype` goes is decided by what it casts from.**
#[test]
fn a_regtype_resolves_a_name_and_prints_an_oid() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rt (t text, i int4)",
        "INSERT INTO rt VALUES ('int4', 23)",
    ]);
    assert_eq!(
        node.rows("SELECT t::regtype, i::regtype FROM rt"),
        vec![vec!["integer", "integer"]]
    );
    // **The canonical name, not the spelling written**: `int4` resolves and prints `integer`.
    assert_eq!(
        node.rows("SELECT t::regtype::oid FROM rt"),
        vec![vec!["23"]]
    );
    // A name nothing has is `42704`, an undefined *type* — the input function resolves rather
    // than parses.
    assert!(node.run("SELECT 'nosuchtype'::text::regtype").is_err());
    // **The wire says 2206**, which is the half only a `Describe` sees: this answered `text`.
    let parsed = esker_sql::parse::parse_statements("SELECT t::regtype FROM rt").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described.fields.expect("a SELECT returns rows");
    assert_eq!(fields[0].type_oid, 2206);
}

/// **An oid no type has prints its digits**, and a corpus with only that row pins the fallback.
#[test]
fn a_real_type_oid_prints_its_name_and_an_unused_one_its_digits() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT ARRAY[23::regtype], ARRAY[1::regtype]"),
        vec![vec!["{integer}", "{1}"]]
    );
}

/// **Array concatenation, three shapes**, and the two NULL rules that are not each other's.
#[test]
fn the_two_null_rules_are_about_the_same_null_on_two_sides() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT ARRAY[1,2] || ARRAY[3], ARRAY[1,2] || 3, 3 || ARRAY[1,2]"),
        vec![vec!["{1,2,3}", "{1,2,3}", "{3,1,2}"]]
    );
    // A NULL **element** is an element; a NULL **array** is empty. Same NULL, two sides.
    assert_eq!(
        node.rows("SELECT ARRAY[1,2] || NULL::int4, NULL::int4[] || ARRAY[1]"),
        vec![vec!["{1,2,NULL}", "{1}"]]
    );
    // The result is the array's type, not `text` — the declared-type half.
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(ARRAY[1,2] || 3), pg_typeof(ARRAY['a'::name] || ARRAY['b'::name])"
        ),
        vec![vec!["integer[]", "name[]"]]
    );
    // **And the refusals that were already right are still right.** `1 || 2` is `42883` because
    // PostgreSQL has no `anynonarray || anynonarray`, and a `"char"` operand is `42725`
    // (ADR 0095) — neither is an array, and adding the array operator must not swallow them.
    assert!(node.run("SELECT 1 || 2").is_err());
    assert!(node.run("SELECT 'r'::\"char\" || 'x'").is_err());
}

/// The family that was already closed, kept so that it cannot go away again.
#[test]
fn an_oid_array_answers_in_every_shape() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT ARRAY[1::oid], array_agg(1::oid), (ARRAY[1::oid])::oid[], '{1,2}'::oid[]"
        ),
        vec![vec!["{1}", "{1}", "{1}", "{1,2}"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(ARRAY[1::oid])"),
        vec![vec!["oid[]"]]
    );
}
