//! **`regclass[]` is a type, oid 2210** — and `min` of a `reg*` is an `oid`.
//!
//! Group 5 of r1's wire-108 baseline, with group 7 beside it because the two are one measurement.
//! The probe that found it sends `SELECT array_agg(c::regclass) AS v` over the extended protocol
//! and reads `RowDescription.ftype()`: a real server says 2210 and this node said 25. Nothing
//! about the *characters* was wrong — `{pg_class}` either way — which is why it took a wire sweep
//! to see. `ActiveRecord` decides a value is an array from `pg_type`, and an array described as
//! `text` reaches it as a Ruby String.
//!
//! The cause was one number. `regclass`'s `typarray` was a deliberate `0` — "an array of a
//! regclass is not a type this node offers" — so there was nothing for `array_agg` to answer with.
//! It is **2210** on a real server, and this unit is that type: the variant, the tag, the
//! columnar mapping in both directions, the catalog row, and the two casts that reach it.
//!
//! **`min`/`max` over all three `reg*` types decay to an `oid`, value and all.** ADR 0098 recorded
//! that a `regtype` did not, from one probe rather than from the family; it does. And the value
//! decaying with the type is the half that a declared-type-only fix leaves wrong: `min(typinput)`
//! answered `int4in` under a column described as `oid`, which is the "right bytes, wrong declared
//! type" bug with its halves swapped.
//!
//! Measured in `tests/captures/pg19_reg_class.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why. **All four are one root**: a relation's identity
/// here is `esker-catalog`'s 64-bit id, and the catalog's own relations carry synthetic ids near
/// `i64::MAX` rather than PostgreSQL's small fixed oids (1259 for `pg_class`, 1247 for `pg_type`).
/// [ADR 0097](../../../docs/adr/0097-an-oid-is-a-type-and-a-relation-id-is-not-one.md) decided to
/// keep the width; these are what it costs, and every one of them is about a *catalog* relation.
/// The same statements over a user relation agree, which is why they are in the corpus rather than
/// only in this list.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 1259::regclass",
            "1259 is `pg_class` on a real server and names nothing here: this node's catalog \
             relations carry 64-bit synthetic ids, so the number prints back as itself — which is \
             what a real server does for an oid that names nothing, applied to an oid that names \
             something there. The type is right (2205) and the direction works: \
             `c.oid::regclass` over this node's own `pg_class` row answers `pg_class`.",
            "pg19_reg_class.txt:115",
        ),
        (
            "SELECT 'pg_class'::regclass::oid",
            "`pg_class`'s id here is past four bytes, so the cast that a real server answers 1259 \
             is `22003` — the same refusal `9223372036854775807::oid` gets, and the same one \
             `stored_shape` raises everywhere else. A user relation's id fits and the cast \
             answers: `'rc'::regclass::oid` is in the corpus above.",
            "pg19_reg_class.txt:116",
        ),
        (
            "SELECT 'pg_class'::regclass = 1259",
            "The comparison happens — a `regclass` is in the numbers' family here, which is what \
             makes `WHERE attrelid = 'x'::regclass` work at all — and the two numbers are not \
             equal, because this node's `pg_class` is not 1259.",
            "pg19_reg_class.txt:117",
        ),
        // **Two that are not about the catalog's oids**, and both predate this unit.
        (
            "SELECT ''::regclass",
            "An empty relation name is a name that answers to nothing here (`42P01`) and a name              whose *syntax* a real server rejects before it looks anything up              (`42602 invalid name syntax`, and a name of only blanks is the same). Both refuse,              one class apart; no client writes it, and `to_regclass('')` — which is NULL on a real              server rather than either error — is not written either.",
            "pg19_reg_class.txt:74",
        ),
        (
            "SELECT '{rc}'::regclass[] @> '{rc}'::regclass[]",
            "**Array containment is not built here for any element type.** `@>` is spelled the              same for an hstore, an ltree and an array, and this crate carries the first two; an              array operand reaches the refusal those two share, so `'{1}'::int[] @> '{1}'::int[]`              says the same thing with `integer[]` in it. Named here because a `regclass[]` is the              array that made it visible, not because it is a rule about `regclass`.",
            "pg19_reg_class.txt:102",
        ),
        (
            "SELECT '{pg_class}'::regclass[] = '{1259}'::oid[]",
            "A real server has no `regclass[] = oid[]` operator at all: an array's comparison is \
             its element type's, and two element types are two operators, so this is `42883` \
             there. This node compares two arrays whose elements are one representation and \
             answers false. The same shape as `'{1}'::int[] = '{1}'::int8[]`, which is refused \
             there and answered here, and is a rule about arrays rather than about `regclass`.",
            "pg19_reg_class.txt:118",
        ),
    ],
};

#[test]
fn every_reg_class_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_reg_class.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 55,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The statement the wire probe sends**, described the way the probe reads it.
#[test]
fn an_array_of_regclass_is_described_as_2210() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8)"]);
    for (statement, oid) in [
        (
            "SELECT array_agg(c) FROM (SELECT 'rc'::regclass AS c) s",
            2210,
        ),
        ("SELECT ARRAY['rc'::regclass]", 2210),
        ("SELECT '{rc}'::regclass[]", 2210),
        // The element, which is the type the array is over and not a coincidence of printing.
        ("SELECT unnest('{rc}'::regclass[])", 2205),
        ("SELECT ('{rc}'::regclass[])[1]", 2205),
        // And the scalar it was built from, so the two cannot drift apart.
        ("SELECT 'rc'::regclass", 2205),
        ("SELECT to_regclass('rc')", 2205),
        // **The miss is a `regclass` NULL, not a `text` one.** A `Datum::Null` has no type, so
        // the resolution that replaces `to_regclass` with one has to say what type the NULL is.
        ("SELECT to_regclass('nosuchrel')", 2205),
        // `::text` after it is a real cast, not the identity: the inner half answers 2205 now.
        ("SELECT 'rc'::regclass::text", 25),
    ] {
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        assert_eq!(fields[0].type_oid, oid, "{statement}");
    }
}

/// **`min` and `max` over a `reg*` are an `oid`, and the value decays with the type.**
///
/// The value is the half that a declared-type-only fix leaves wrong, and it is invisible from the
/// type alone: a column described as `oid` whose bytes spell `int4in` reads correctly in `psql`
/// and wrongly in anything that parses the value as the type it was promised.
#[test]
fn a_reg_extreme_is_an_oid_and_so_is_its_value() {
    let mut node = parity::Node::new(&[]);
    for (statement, answer) in [
        // 23 is `int4`, 25 is `text`: the numbers, not the names they print as.
        (
            "SELECT min(c) FROM (VALUES ('int4'::regtype), ('text'::regtype)) s(c)",
            "23",
        ),
        (
            "SELECT max(c) FROM (VALUES ('int4'::regtype), ('text'::regtype)) s(c)",
            "25",
        ),
        // 42 is `int4in`, measured; `textin` is 46, so the minimum is the first.
        (
            "SELECT min(c) FROM (VALUES ('int4in'::regproc), ('textin'::regproc)) s(c)",
            "42",
        ),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        // 26 is `oid`. Described as 2206 or 24 the value above would be a different number to a
        // client that reads it as the type it was told.
        assert_eq!(fields[0].type_oid, 26, "{statement}");
    }
    // **An array of them does not decay**, because an array has a `min` of its own. The rule is
    // about the scalar; a family that swallowed the array would be a rule about the name.
    let outcome = node
        .run("SELECT min(c) FROM (VALUES ('{int4}'::regtype[]), ('{text}'::regtype[])) s(c)")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 2211);
}

/// **A name inside an array literal is resolved by the element's own input function**, so a name
/// that answers to nothing is the element's `42P01` and not the array's `22P02`.
#[test]
fn a_bad_name_in_an_array_literal_is_the_elements_own_error() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8)"]);
    let error = node.run("SELECT '{nosuchrel}'::regclass[]").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        error.to_string(),
        "relation \"nosuchrel\" does not exist",
        "an array literal's bad element must fail as the element, not as the array"
    );
    // The scalar it borrows the rule from, so the two cannot drift apart.
    let error = node.run("SELECT 'nosuchrel'::regclass").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    // **A good name beside a bad one still fails**, which is what says the resolution is per
    // element rather than a check on the first one.
    let error = node.run("SELECT '{rc,nosuchrel}'::regclass[]").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    // And a NULL element is not a name at all.
    assert_eq!(
        node.rows("SELECT '{rc,NULL}'::regclass[]"),
        vec![vec!["{rc,NULL}"]]
    );
}

/// **A string literal is a name and everything else is a value** — the scalar rule, one dimension
/// up, and the one this got wrong first: reading through the cast in `'{1}'::oid[]::regclass[]`
/// handed `1` to the *name* lookup and answered `42P01 relation "1" does not exist`.
#[test]
fn the_two_doors_of_a_regclass_array_are_told_apart_by_the_operand() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8)"]);
    // The name door.
    assert_eq!(node.rows("SELECT '{rc}'::regclass[]"), vec![vec!["{rc}"]]);
    // The value door: the numbers come back as the names they name.
    assert_eq!(
        node.rows(
            "SELECT (SELECT ARRAY[c.oid] FROM pg_class c WHERE c.relname = 'rc')::regclass[]"
        ),
        vec![vec!["{rc}"]]
    );
    // And back, which is the inverse cast rather than a text round trip: through the text the
    // numbers would be handed the *name* `rc` and refused `22P02`.
    assert_eq!(
        node.rows("SELECT '{rc}'::regclass[]::oid[] = (SELECT ARRAY[c.oid::oid] FROM pg_class c WHERE c.relname = 'rc')"),
        vec![vec!["t"]]
    );
}
