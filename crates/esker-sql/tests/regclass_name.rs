//! `<oid>::regclass` — the inverse cast, and the first piece of boot statement 36.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
/// **None.** The one that stood here said a `regclass` was `text` in this node — "an oid that
/// prints as a name, and `text` here, which is what it prints as" — on the reasoning that
/// `ActiveRecord` writes `::regclass::text` and never reads the bare form. It is 2205 since
/// `regclass[]` arrived (`tests/reg_class.rs`): the inverse direction has to carry the oid beside
/// the name for `array_agg` over one to be a `regclass[]`, and once it does, the declared type is
/// a real server's. The `::text` after it is a cast now rather than the identity.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
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
/// **Asserted as a read count**, which is the property itself rather than its shadow: eight
/// literals against one, over the same catalog, and the instrument counts the keys each statement
/// reads (`stmt_stats`). One read serves all eight, so the two counts are the same; one read per
/// literal makes the second eight times the first.
///
/// It was a ratio over the **wall clock** — `eight < one * 4 + 200ms` — until 2026-09-11, when the
/// sibling assertion in `catalog_read_slope.rs` went red on a gate whose own diff did not touch
/// this crate, at 3.1× under a load of 8 to 11 (#59). A gate cannot carry a wall-clock assertion,
/// and here it never had to: the thing being counted was always a count. The timings are printed.
#[test]
fn one_catalog_read_serves_every_regclass_in_a_statement() {
    let mut node = parity::Node::new(&[]);
    for at in 0..200 {
        node.run(&format!("CREATE TABLE rc{at} (id int8 PRIMARY KEY)"))
            .unwrap();
    }

    // **Turned on here rather than by the environment**, because a test that needs a variable set
    // is one the gate never runs (`stmt_stats::trace_every_read`, the idiom `catalog_read_slope`
    // established).
    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
    );
    // **The second run of each**, because the first statement in a session pays for the catalog
    // itself — 409 keys over these two hundred relations — and that load is not what this test is
    // about. What it is about is the *per-literal* multiplier, which is on every run.
    let measure = |node: &mut parity::Node, sql: &str| {
        esker_sql::stmt_stats::clear_trace();
        node.run(sql).unwrap();
        let cold = esker_sql::stmt_stats::last_trace().len();
        let start = std::time::Instant::now();
        esker_sql::stmt_stats::clear_trace();
        node.run(sql).unwrap();
        let reads = esker_sql::stmt_stats::last_trace().len();
        (cold, reads, start.elapsed())
    };
    let (cold, one, one_took) = measure(&mut node, "SELECT 'rc0'::regclass");
    let (_, eight, eight_took) = measure(
        &mut node,
        "SELECT 'rc0'::regclass, 'rc1'::regclass, 'rc2'::regclass, 'rc3'::regclass, \
         'rc4'::regclass, 'rc5'::regclass, 'rc6'::regclass, 'rc7'::regclass",
    );
    println!(
        "one literal: {cold} reads cold, {one} warm in {one_took:?}; eight: {eight} warm in \
         {eight_took:?}"
    );
    // **The instrument answers to catalog work**, which is what says the warm four are a fact
    // about the statement rather than a trace nobody filled: the very first statement in the
    // session loads these two hundred relations and the counter sees every key of it.
    assert!(
        cold > one,
        "the first statement read {cold} keys and the second {one}: the counter is not seeing the \
         catalog load, so the numbers below say nothing"
    );
    assert_eq!(
        eight, one,
        "eight `::regclass` casts read {eight} keys where one read {one}: the catalog is being \
         read once per literal rather than once per statement"
    );
}
