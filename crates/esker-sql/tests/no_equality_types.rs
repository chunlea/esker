//! **Four types with no equality, and everything that needs one** — r1's param census, widened.
//!
//! The census reported four rows where this node **answers what PostgreSQL refuses**: `=` over
//! `json`, `xml`, `point` and `polygon`. Four rows is not the mechanism. These types have neither
//! an equality nor an ordering operator, so every shape that needs one is affected — twenty of
//! them, measured, with `jsonb` carried alongside as the contrast.
//!
//! **The cell a reasonable implementation gets wrong is `point <>`.** PostgreSQL has `point_ne`
//! and no `point_eq`: the inequality exists without the equality. Deriving one from the other is
//! wrong on exactly that cell and nowhere else in the table.
//!
//! **Three message families, two SQLSTATEs**, and one shared message would be wrong about two
//! thirds of the table:
//!
//! ```text
//! 42883 operator does not exist: json = json                 a named operator
//! 42883 could not identify an equality operator for type json  IN, IS DISTINCT, DISTINCT,
//!                                                              GROUP BY, UNION
//! 42883 could not identify an ordering operator for type json  ORDER BY — *ordering*, not equality
//! 42704 data type json has no default operator class for access method "btree"
//!                                                              INDEX, PRIMARY KEY, UNIQUE
//! ```
//!
//! **Refusing the whole family for these types would be the same defect facing the other way.**
//! `point` and `polygon` answer `~=`; `polygon` answers `@>`, `<@` and `&&`; and a plain
//! `CREATE TABLE (c json)` is legal — it is the *constraint* that is not. The column may be
//! stored, it may not be ordered.
//!
//! Measured in `tests/captures/pg19_no_equality_types.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// **The whole measured table**, replayed against the corpus.
///
/// It began at 88 disagreements in four shapes facing two directions. **(a) — everything
/// PostgreSQL refuses — is closed**: 64 rows, in seven pieces, each one a place that asked the
/// question differently or not at all.
///
/// ```text
///  8  PRIMARY KEY / UNIQUE never asked `refuse_unindexable` at all
/// 10  `polygon` is the only geometric shape with no `=`, and needed a third predicate
///  2  `IN` is `=` with the operator implied, and asked nothing
///  4  `UNION` deduplicates, and read no list
/// 18  `json` and `jsonb` had to be refused differently — one is PostgreSQL's answer,
///     the other is this node's gap
/// 16  `&&` and `@>` are catalog functions, so the comparison check could not see them
///  6  `IS DISTINCT FROM` names the operator it is missing, `=`
/// ```
///
/// The 24 that were **(b)** — PostgreSQL answers and this node refused — are down to **one**:
/// building an index on a `jsonb` column, which needs an order-preserving key and is its own step.
/// Everything else in that direction now answers, in five pieces: `jsonb`'s six comparisons and
/// its containment over the `Json` the value layer already parses; `point <>`, fuzzy; `polygon`
/// containment and overlap, fuzzy and edge-aware; `~=` for the shapes that have it; and ordering a
/// `jsonb` column, which took the declared type travelling on the `SortKey` rather than a `Datum`
/// of its own.
#[test]
#[ignore = "one row left: an index on a jsonb column, which needs an order-preserving key"]
fn every_no_equality_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_no_equality_types.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 145,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The inequality that exists without its equality.** Its own test because it is the one cell a
/// derived rule gets wrong, and a corpus row is easy to read past.
#[test]
#[ignore = "part of the same unit: the node refuses `point <>`, which a real server answers"]
fn a_point_has_an_inequality_and_no_equality() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '(0,0)'::point <> '(1,1)'::point"),
        vec![vec!["t"]],
        "PostgreSQL has point_ne"
    );
    let refused = node
        .run("SELECT '(0,0)'::point = '(0,0)'::point")
        .unwrap_err();
    assert_eq!(refused.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        refused.to_string(),
        "operator does not exist: point = point",
        "PostgreSQL has no point_eq"
    );
}

/// **The column is legal and the constraint is not** — so this is not a type to refuse storing.
#[test]
fn the_column_is_legal_and_only_the_constraint_is_not() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE jj (c json)").unwrap();
    let refused = node.run("CREATE INDEX jj_c ON jj (c)").unwrap_err();
    assert_eq!(
        refused.to_string(),
        "data type json has no default operator class for access method \"btree\"",
    );
}
