//! `<oid>::regclass` — the inverse cast, and the first piece of boot statement 36.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // One: a `regclass` on a real server is an **oid that prints as a name**, and `text` here,
    // which is what it prints as. `ActiveRecord` writes `::regclass::text` and never reads the
    // bare form, so the `::text` after it is the identity on what this already answers.
    types: &["SELECT c.oid::regclass FROM pg_class c WHERE c.relname = 'rc'"],
    answers: &[],
};

#[test]
fn every_regclass_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regclass_name.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 6,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// Oid **0** prints `-`, which is one character and not an empty string.
///
/// It cannot be a corpus row: a lone `-` in the rows column is that format's marker for *no rows*,
/// so the two are indistinguishable there. Measured with `length`, which is how the ambiguity was
/// settled against the oracle in the first place.
#[test]
fn the_invalid_oid_prints_as_one_dash() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 0::regclass::text"), [["-"]]);
}

/// Both directions, and the round trip between them.
#[test]
fn a_name_becomes_an_oid_and_an_oid_becomes_the_name() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8 PRIMARY KEY)"]);
    assert_eq!(
        node.rows("SELECT 'rc'::regclass::oid::regclass::text"),
        [["rc"]]
    );
    // Per row, over a column — which is what the forward form cannot be.
    assert_eq!(
        node.rows("SELECT c.oid::regclass::text FROM pg_class c WHERE c.relname = 'rc_pkey'"),
        [["rc_pkey"]]
    );
    // An oid naming nothing prints itself rather than raising, so a `LEFT JOIN` with no match
    // still answers.
    assert_eq!(
        node.rows("SELECT 2147483647::regclass::text"),
        [["2147483647"]]
    );
    assert_eq!(node.rows("SELECT NULL::oid::regclass::text"), [["\\N"]]);
}

/// The walker that resolves a `::regclass` and substitutes a `$1` visits **every** node that holds
/// an expression — and it did not.
///
/// `'rc'::regclass` nested inside a cast to text reached the row evaluator **unresolved**, an
/// internal error, because a cast to text was not on that walker's list of nodes to descend into.
/// Neither were a scalar function, a `CASE`, an `= ANY` over a value, or an aggregate's arguments
/// — so a `$1` inside any of them was never substituted either, and the statement answered
/// `42P02` for a parameter the client had sent. One omission, two symptoms.
#[test]
fn the_binder_descends_into_every_node_that_holds_an_expression() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8 PRIMARY KEY, b text)"]);
    // Each of these holds a forward `::regclass` one node down. Before the fix every one was
    // `XX000 a ::regclass reached the row evaluator unresolved`.
    assert_eq!(
        node.rows("SELECT CASE WHEN 'rc'::regclass = 'rc'::regclass THEN 'same' END"),
        [["same"]]
    );
    assert_eq!(
        node.rows("SELECT lower(c.relname) FROM pg_class c WHERE c.oid = 'rc'::regclass"),
        [["rc"]]
    );
    assert_eq!(
        node.rows("SELECT max(c.relname) FROM pg_class c WHERE c.oid = 'rc'::regclass"),
        [["rc"]]
    );
    // And through the reverse direction, which is this unit's own.
    assert_eq!(
        node.rows("SELECT lower(c.oid::regclass::text) FROM pg_class c WHERE c.relname = 'rc'"),
        [["rc"]]
    );
}

/// **A divergence this unit found and did not close — and the namespace unit closed it.** Kept as
/// the assertion it should always have been.
///
/// A `regclass` on a real server is an oid whose *output function* prints a name, so
/// `'rc'::regclass::text` is `rc` there and was the oid's digits here: the forward cast resolves
/// to a number before the plan is built, which is what makes `WHERE attrelid = 'x'::regclass` one
/// catalog read per statement rather than one per row, and `::text` of an `int8` prints the
/// number. Both halves of the fix were already in this file — the forward cast resolves the name
/// and [`plan::CatalogFunc::RegClassName`] is the inverse — so `'x'::regclass::text` composes
/// them, and a name nothing answers to still fails in the forward half.
#[test]
fn the_forward_cast_prints_the_name_a_real_server_prints() {
    let mut node = parity::Node::new(&["CREATE TABLE rc (id int8 PRIMARY KEY)"]);
    assert_eq!(node.rows("SELECT 'rc'::regclass::text"), [["rc"]]);
    // **And the oid is still an oid where it is asked for as one**, which is the half that must
    // not move: `::regclass::oid` is the number, and it is what a `WHERE` compares.
    assert_ne!(
        node.rows("SELECT 'rc'::regclass::oid::text"),
        [["rc"]],
        "the oid cast prints what the value is"
    );
}

/// Resolving `::regclass` costs **one** catalog read per statement, not one per literal.
///
/// The debt `08ff6a2` named, and the reason it mattered: a catalog read is a scan of the name
/// records plus a point read per relation, and `ActiveRecord`'s schema dump writes several
/// `::regclass` casts in one statement against a catalog with hundreds of relations. On the node
/// that was serving the Rails suite, one `SELECT 'people'::regclass` took **1.98 s** — the scan
/// itself was walking the whole store, which `95dbd77` fixed; this is the multiplier that sat on
/// top of it.
///
/// **Asserted as a ratio against a control**, the way `a_scan_costs_its_range_and_not_the_store`
/// is: eight literals against one, over the same catalog. One read serves all eight, so the two
/// statements cost the same; one read per literal makes the second eight times the first. Comparing
/// them is what makes this a statement about the shape of the work rather than about this machine.
#[test]
fn one_catalog_read_serves_every_regclass_in_a_statement() {
    let mut node = parity::Node::new(&[]);
    for at in 0..200 {
        node.run(&format!("CREATE TABLE rc{at} (id int8 PRIMARY KEY)"))
            .unwrap();
    }

    let elapsed = |node: &mut parity::Node, sql: &str| {
        let start = std::time::Instant::now();
        for _ in 0..20 {
            node.run(sql).unwrap();
        }
        start.elapsed()
    };
    let one = elapsed(&mut node, "SELECT 'rc0'::regclass");
    let eight = elapsed(
        &mut node,
        "SELECT 'rc0'::regclass, 'rc1'::regclass, 'rc2'::regclass, 'rc3'::regclass, \
         'rc4'::regclass, 'rc5'::regclass, 'rc6'::regclass, 'rc7'::regclass",
    );
    assert!(
        eight < one * 4 + std::time::Duration::from_millis(200),
        "eight `::regclass` casts took {eight:?} where one took {one:?}: the catalog is being \
         read once per literal rather than once per statement"
    );
}
