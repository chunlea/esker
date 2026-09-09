//! **`min` and `max` over a `cidr` are an `inet`**, against PostgreSQL 19beta1.
//!
//! r1's wire sweep filed two rows: `min(c::cidr)` and `max(c::cidr)` come back OID 650 (`cidr`)
//! here and 869 (`inet`) on a real server. It is the shape `min(varchar)` and `min(name)` already
//! have in this crate — **the aggregate decays to the category's preferred type** — and `inet` is
//! the preferred type of the network category the way `text` is of the string one.
//!
//! **The values do not change**, which is what makes this a declared type rather than an answer: a
//! `cidr` printed through `inet`'s output function is the same characters, mask included.
//!
//! Three rules, and the third is not derivable from the first two:
//!
//! ```text
//!   min(cidr)      inet      the type decays
//!   min(inet)      inet      itself, unchanged
//!   min(macaddr)   42883 function min(macaddr) does not exist
//! ```
//!
//! `array_agg(cidr)` is a `cidr[]` and `GREATEST(cidr, cidr)` is a `cidr`, so it is `min`/`max`
//! that decay and not the network types generally. ADR 0031's law again: the aggregate set is per
//! type and cannot be derived from whether the type is ordered.
//!
//! Measured in `tests/captures/pg19_cidr_aggregate.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`pg_typeof` answers a `regtype` on both now** (ADR 0093): it is resolved at plan
    // time from the argument's declared type, so what this list recorded has no difference
    // left in it.
    types: &[],
    answers: &[],
};

#[test]
fn every_cidr_aggregate_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_cidr_aggregate.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`** — the path r1's sweep reads.
fn described(node: &mut parity::Node, statement: &str) -> u32 {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")[0]
        .type_oid
}

/// **The two rows r1 filed as group E**, over the wire. 869 is `inet`, 650 is `cidr`.
#[test]
fn min_and_max_of_a_cidr_are_an_inet() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "SELECT min(c::cidr) AS v FROM (VALUES ('127.0.0.0/24')) s(c)",
        "SELECT max(c::cidr) AS v FROM (VALUES ('127.0.0.0/24')) s(c)",
        // Through a column rather than a cast in the target list, which is where a suite meets it.
        "SELECT min(c) AS v FROM (SELECT '127.0.0.0/24'::cidr AS c) s",
    ] {
        assert_eq!(described(&mut node, statement), 869, "{statement}");
    }
    // **An `inet` keeps itself**: the decay is `cidr`'s, not the category's.
    assert_eq!(
        described(
            &mut node,
            "SELECT min(c::inet) AS v FROM (VALUES ('127.0.0.1')) s(c)"
        ),
        869
    );
}

/// **The values do not change**, which is what makes this a declared type and not an answer.
#[test]
fn the_mask_survives_the_decay() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT min(c::cidr), max(c::cidr) FROM (VALUES ('127.0.0.0/24'),('10.0.0.0/8')) s(c)"
        ),
        vec![vec!["10.0.0.0/8", "127.0.0.0/24"]]
    );
    // Through `inet`'s output function, which is what the declared type now says it is.
    assert_eq!(
        node.rows("SELECT min(c::cidr)::text FROM (VALUES ('127.0.0.0/24'),('10.0.0.0/8')) s(c)"),
        vec![vec!["10.0.0.0/8"]]
    );
    assert_eq!(
        node.rows(
            "SELECT min(c::inet), max(c::inet) FROM (VALUES ('127.0.0.1'),('10.0.0.1')) s(c)"
        ),
        vec![vec!["10.0.0.1", "127.0.0.1"]]
    );
}

/// **`min(macaddr)` does not exist at all**, which is the third rule and the one reasoning misses.
#[test]
fn a_macaddr_has_no_min_or_max() {
    let mut node = parity::Node::new(&[]);
    for func in ["min", "max"] {
        let error = node
            .run(&format!(
                "SELECT {func}(c::macaddr) FROM (VALUES ('ff:ff:ff:ff:ff:ff')) s(c)"
            ))
            .unwrap_err();
        assert_eq!(error.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
        assert!(
            error.to_string().contains(&format!("{func}(macaddr)")),
            "the refusal did not name the aggregate: {error}"
        );
    }
    // The ordering itself is there, which is the point of the refusal being about the aggregate:
    // `ORDER BY` over a `macaddr` works on both.
    assert_eq!(
        node.rows(
            "SELECT c::macaddr FROM (VALUES ('ff:ff:ff:ff:ff:ff'),('01:23:45:67:89:0a')) s(c) \
             ORDER BY 1"
        ),
        vec![vec!["01:23:45:67:89:0a"], vec!["ff:ff:ff:ff:ff:ff"]]
    );
}

/// **What does *not* decay**, which is the half that keeps the rule from being "network types".
#[test]
fn an_aggregate_that_is_not_min_keeps_the_cidr() {
    let mut node = parity::Node::new(&[]);
    // `array_agg` answers its argument's array type; 651 is `_cidr`.
    assert_eq!(
        described(
            &mut node,
            "SELECT array_agg(c::cidr) AS v FROM (VALUES ('127.0.0.0/24')) s(c)"
        ),
        651
    );
    // `GREATEST` is not an aggregate and takes the arguments' common type, which is `cidr`.
    assert_eq!(
        described(
            &mut node,
            "SELECT greatest('10.0.0.0/8'::cidr, '127.0.0.0/24'::cidr) AS v"
        ),
        650
    );
    // And a `cidr` beside an `inet` is an `inet` on both, through either constructor.
    for statement in [
        "SELECT coalesce('10.0.0.0/8'::cidr, '127.0.0.1'::inet) AS v",
        "SELECT CASE WHEN true THEN '10.0.0.0/8'::cidr ELSE '127.0.0.1'::inet END AS v",
    ] {
        assert_eq!(described(&mut node, statement), 869, "{statement}");
    }
    assert_eq!(
        node.rows("SELECT '10.0.0.0/8'::cidr = '10.0.0.0/8'::inet"),
        vec![vec!["t"]]
    );
}
