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
