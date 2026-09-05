//! **A schema-qualified `FROM` item is also referable by its bare relation name.**
//!
//! `FROM test_schema.things … WHERE things.name = …` is what `schema_test.rb` writes and what
//! `corpus/pg19_tsvector.txt` part 3 needs; this node answered
//! `42P01 missing FROM-clause entry for table "things"`. PostgreSQL gives every `FROM` item an
//! implicit alias equal to its **unqualified** relation name.
//!
//! Every row below was put to PostgreSQL 19beta1 in one `BEGIN … ROLLBACK`. The refusals are as
//! much the rule as the successes: an explicit alias *replaces* the implicit one rather than
//! adding to it, and two same-named relations are two entries a query may have — with only the
//! bare *reference* undecidable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Two schemas holding a relation of the same name, which is what makes the ambiguity reachable.
const FIXTURE: &[&str] = &[
    "CREATE SCHEMA s1",
    "CREATE TABLE s1.things (id int8, name text)",
    "CREATE SCHEMA s2",
    "CREATE TABLE s2.things (id int8, name text)",
    "INSERT INTO s1.things VALUES (1, 'a')",
];

/// The bare name resolves, in the target list and in a `WHERE`.
#[test]
fn a_qualified_from_item_answers_to_its_bare_name() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT things.name FROM s1.things"),
        [["a".to_owned()]]
    );
    // The `WHERE` is the shape `schema_test.rb` writes and the one part 3 stops on.
    assert_eq!(
        node.rows("SELECT id FROM s1.things WHERE things.name = 'a'"),
        [["1".to_owned()]]
    );
}

/// **An explicit alias replaces the implicit one.** This is the half that a fix which merely added
/// the bare name as a second alias would get wrong — and it passed before the fix too, for the
/// uninteresting reason that the bare name resolved to nothing at all, so it is a guard now and
/// was vacuous then.
#[test]
fn an_explicit_alias_takes_the_bare_name_away() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT t.name FROM s1.things t"),
        [["a".to_owned()]]
    );
    assert_eq!(
        node.answer("SELECT things.name FROM s1.things t")
            .to_string(),
        "!42P01 missing FROM-clause entry for table \"things\""
    );
}

/// **Two relations of one name are two entries, and only the bare reference is undecidable.**
///
/// Measured, and the pair is the point: a real server takes `FROM s1.things, s2.things` and
/// refuses `things.name` over it with `42P09`, where `FROM s1.things, s1.things` is `42712` on the
/// `FROM` itself. Keying the duplicate check on the referable name rather than on the relation
/// refuses the first of those — which is what this node started doing the moment the bare name
/// resolved, and why `TableRef::identity` is a separate method from `referred_as`.
#[test]
fn the_bare_name_is_only_unambiguous_when_it_is() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("SELECT things.name FROM s1.things, s2.things")
            .to_string(),
        "!42P09 table reference \"things\" is ambiguous"
    );
    // The `FROM` itself is fine, which is the half a duplicate check keyed on the wrong name takes
    // away.
    assert_eq!(node.rows("SELECT 1 FROM s1.things, s2.things").len(), 0);
    // And one relation twice really is `42712`, on the `FROM` and not on a reference.
    assert_eq!(
        node.answer("SELECT 1 FROM s1.things, s1.things")
            .to_string(),
        "!42712 table name \"things\" specified more than once"
    );
    // An alias is the way through, and it is the only one here — see below.
    assert_eq!(
        node.rows("SELECT a.name FROM s1.things a, s2.things b")
            .len(),
        0
    );
}

/// **A three-part column reference is still a gap**, and PostgreSQL's answers for the two shapes
/// it would settle are recorded here so the boundary is a decision rather than an omission.
///
/// `s1.things.name` is what a real server accepts — it is also its escape hatch from the ambiguity
/// above, which is why aliasing is the only way through here. Nothing in the capture needs it:
/// part 3 writes `things.name` over a single qualified `FROM` item.
#[test]
fn a_three_part_reference_is_named_rather_than_answered() {
    let mut node = parity::Node::new(FIXTURE);
    // PostgreSQL answers the row; this node names the construct (contract C2).
    assert_eq!(
        node.answer("SELECT s1.things.name FROM s1.things")
            .to_string(),
        "!0A000 the qualified column s1.things.name is not supported"
    );
    // PostgreSQL: `42P01 invalid reference to FROM-clause entry for table "things"`, with a
    // DETAIL. The construct is refused before the reference can be judged, so this node gives the
    // same sentence as above rather than that one.
    assert_eq!(
        node.answer("SELECT s2.things.name FROM s1.things")
            .to_string(),
        "!0A000 the qualified column s2.things.name is not supported"
    );
}

/// **The implicit alias has to reach every statement that has a `FROM` item**, not only the
/// `SELECT` the census happened to stop on. Each of these is measured on 19beta1 and each keeps
/// working there; a fix that reached one scope constructor and not the others would pass the test
/// above and fail here.
#[test]
fn every_statement_with_a_from_item_gets_the_implicit_alias() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s1",
        "CREATE TABLE s1.things (id int8, name text)",
        "CREATE TABLE s1.other (id int8, tag text)",
        "INSERT INTO s1.things VALUES (1, 'a'), (2, 'b')",
        "INSERT INTO s1.other VALUES (1, 'x')",
    ]);

    node.run("UPDATE s1.things SET name = 'z' WHERE things.id = 1")
        .unwrap();
    node.run("DELETE FROM s1.things WHERE things.id = 2")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name FROM s1.things"),
        [["z".to_owned()]],
        "the UPDATE and the DELETE each acted on the row the bare name named"
    );

    // A join names one side by its alias and the other by its bare name, and then both bare.
    assert_eq!(
        node.rows("SELECT t.id FROM s1.things t JOIN s1.other ON other.id = t.id"),
        [["1".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT things.id FROM s1.things JOIN s1.other ON other.id = things.id"),
        [["1".to_owned()]]
    );

    // A correlated subquery reaches the outer scope by the outer relation's bare name.
    assert_eq!(
        node.rows(
            "SELECT id FROM s1.things WHERE EXISTS (SELECT 1 FROM s1.other WHERE other.id = things.id)"
        ),
        [["1".to_owned()]]
    );

    assert_eq!(
        node.rows("SELECT id FROM s1.things ORDER BY things.id"),
        [["1".to_owned()]]
    );

    // `INSERT … SELECT` would be the seventh shape and is out of reach for a reason of its own:
    // this node refuses the construct entirely (`0A000 INSERT ... SELECT`), so there is no scope
    // for an implicit alias to be missing from. Left out rather than asserted, because a test that
    // stops on a different gap says nothing about this one.
}
