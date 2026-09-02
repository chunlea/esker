//! The `ActiveRecord` shapes this phase makes runnable — and the honest count of what it did not.
//!
//! `docs/plans/phase-12-subquery.md` unit 5. Three of `ActiveRecord` 8.1.3.1's thirty-six boot
//! statements carry a subquery (`docs/plans/phase-9-rails.md` §5 counted four; one of them was the
//! `= ANY (current_schemas(false))` the array lane has since answered). **This phase moves the
//! `activerecord_surface.rs` counter by zero**, and that is not a hedge — it is what the counter is
//! for. Every one of the three stops on a *catalog function* before it reaches its subquery:
//!
//! ```text
//! line 52  0A000 the function pg_get_indexdef is not supported      -- and `ARRAY(SELECT …)`
//! line 55  0A000 the function pg_get_constraintdef is not supported -- and `array_agg`
//! line 56  0A000 a cast to TEXT is not supported                    -- and `array_agg`
//! ```
//!
//! So what this file does is prove the **shape**, with the pieces that are somebody else's lane
//! replaced by ones this node has. `array_agg` becomes `max`, `generate_subscripts` becomes a
//! table of positions, and what is left is exactly what statements 55 and 56 ask of the query
//! surface: a **correlated scalar subquery whose own `FROM` is a derived table joined to a
//! table**. When the array unit lands, the half measured here is already known to work — and if it
//! does not, this file says so before the harness does.
//!
//! Every row below is what a real PostgreSQL 19beta1 answered for the same statement over the same
//! fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why. Empty, and that is the claim this file makes.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// The same statements, replayed against the capture rather than against a list written here.
#[test]
fn every_activerecord_shape_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_subquery_activerecord.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A miniature of the three relations statement 55 joins — a constraint, the key positions it
/// names, and the attributes those positions point at.
const FIXTURE: &[&str] = &[
    "CREATE TABLE ar_cons (id int8 PRIMARY KEY, name text, rel int8)",
    "INSERT INTO ar_cons VALUES (1, 'fk_a', 10), (2, 'fk_b', 11), (3, 'uq_c', 10)",
    "CREATE TABLE ar_keys (id int8 PRIMARY KEY, cid int8, pos int8)",
    "INSERT INTO ar_keys VALUES (100, 1, 1), (101, 1, 2), (102, 3, 1)",
    "CREATE TABLE ar_attrs (id int8 PRIMARY KEY, rel int8, pos int8, attname text)",
    "INSERT INTO ar_attrs VALUES (200, 10, 1, 'author_id'), (201, 10, 2, 'kind'), \
     (202, 11, 1, 'title')",
];

/// Statement 55's shape: a **correlated scalar subquery whose `FROM` is a derived table joined to
/// a table**.
///
/// `ActiveRecord` writes it as
///
/// ```sql
/// ( SELECT array_agg(a.attname ORDER BY idx)
///   FROM ( SELECT idx, c.conkey[idx] AS conkey_elem FROM generate_subscripts(c.conkey, 1) AS idx )
///        indexed_conkeys
///   JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = indexed_conkeys.conkey_elem )
/// ```
///
/// — and every structural piece of that is here: the correlation on `c`, the derived table with
/// its own alias, the join against it, and an aggregate at the top. What is replaced is
/// `array_agg` (the array lane's) and `generate_subscripts` (likewise); the shape is untouched.
#[test]
fn a_correlated_scalar_over_a_derived_table_joined_to_a_table() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "SELECT c.name, (SELECT max(a.attname) FROM \
             (SELECT k.pos FROM ar_keys k WHERE k.cid = c.id) AS ks \
             JOIN ar_attrs a ON a.pos = ks.pos) AS attname \
             FROM ar_cons c ORDER BY c.name"
        ),
        vec![
            vec!["fk_a", "title"],
            vec!["fk_b", "\\N"],
            vec!["uq_c", "title"],
        ]
    );
}

/// The rest of what those three statements ask of the query surface, one shape at a time.
#[test]
fn the_shapes_the_boot_statements_are_built_from() {
    let mut node = parity::Node::new(FIXTURE);

    // A correlated scalar in a target list, which is how every one of the three carries its
    // per-row count.
    assert_eq!(
        node.rows(
            "SELECT c.name, (SELECT count(*) FROM ar_keys k WHERE k.cid = c.id) AS n \
             FROM ar_cons c ORDER BY c.name"
        ),
        vec![vec!["fk_a", "2"], vec!["fk_b", "0"], vec!["uq_c", "1"],]
    );
    // Statement 52 opens `SELECT distinct …` with a subquery in the target list. Ordered on
    // **both** columns, because `ORDER BY c.rel` alone leaves the pair `(10, 1)`/`(10, 2)` in an
    // order neither server promises — PostgreSQL returned one and this node the other, which is a
    // test over-specifying rather than a divergence.
    assert_eq!(
        node.rows(
            "SELECT DISTINCT c.rel, (SELECT count(*) FROM ar_keys k WHERE k.cid = c.id) \
             FROM ar_cons c ORDER BY 1, 2"
        ),
        vec![vec!["10", "1"], vec!["10", "2"], vec!["11", "0"],]
    );
    // `EXISTS` over the same correlation, which is what an `includes` turns into.
    assert_eq!(
        node.rows("SELECT c.name FROM ar_cons c WHERE EXISTS (SELECT 1 FROM ar_keys k WHERE k.cid = c.id) ORDER BY c.name"),
        vec![vec!["fk_a"], vec!["uq_c"]]
    );
    // `.from(subquery)` with a `count` over a `distinct` relation — the other shape
    // `docs/plans/phase-12-subquery.md` §3 unit 2 names.
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT DISTINCT rel FROM ar_cons) AS t"),
        vec![vec!["2"]]
    );
    // And the CTE form of the same question, which is what a hand-written scope looks like.
    assert_eq!(
        node.rows(
            "WITH ks AS (SELECT cid, count(*) AS n FROM ar_keys GROUP BY cid) \
             SELECT c.name, ks.n FROM ar_cons c JOIN ks ON ks.cid = c.id ORDER BY c.name"
        ),
        vec![vec!["fk_a", "2"], vec!["uq_c", "1"]]
    );
}

/// A subquery over a **`pg_catalog` relation**, which is where every one of the three actually
/// points.
///
/// Worth its own test because a catalog view has no key range and no index: its rows are computed,
/// not scanned. A derived table over one, and a correlated subquery over one, therefore take a
/// path nothing else in this phase exercises — and it is the path `ActiveRecord` will take.
#[test]
fn a_subquery_over_a_catalog_view() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows(
            "SELECT t.typname FROM pg_type t WHERE EXISTS \
             (SELECT 1 FROM pg_type u WHERE u.oid = t.oid AND u.typname = 'int8')"
        ),
        vec![vec!["int8"]]
    );
    assert_eq!(
        node.rows(
            "SELECT t.typname FROM pg_type t WHERE t.oid IN \
             (SELECT u.oid FROM pg_type u WHERE u.typname IN ('int2','int4')) ORDER BY t.typname"
        ),
        vec![vec!["int2"], vec!["int4"]]
    );
    // A derived table over a computed relation: its rows come from the view, through a plan.
    let rows = node.rows("SELECT count(*) FROM (SELECT typname FROM pg_type) AS t");
    assert_eq!(
        rows.len(),
        1,
        "a derived table over pg_type answered {rows:?}"
    );
    // The count itself is this node's type surface and not PostgreSQL's 727, which is why the
    // number is not asserted: `pg_type` here holds the types this server has.
    assert!(
        rows[0][0].parse::<u32>().is_ok_and(|count| count > 0),
        "a derived table over pg_type counted {:?}",
        rows[0][0]
    );
}
