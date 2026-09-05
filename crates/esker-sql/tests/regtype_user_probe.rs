//! `'<name>'::regtype` over a type the **catalog** made, inside the case-insensitivity probe.
//!
//! Run 97's only backwards movement: `postgresql_adapter_test.rb` traded
//! `the type oidvector is not supported` for `type "example_type" does not exist`. The trade is
//! the point — the probe now *reaches* the user-defined type, which the oidvector work put in its
//! way, and ADR 0077 recorded the catalog-backed rendering as the half it did not build.
//!
//! `test_only_check_for_insensitive_comparison_capability_once` creates
//! `CREATE DOMAIN example_type AS integer` and asks
//! `can_perform_case_insensitive_comparison_for?` about a column of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

#[test]
fn the_probe_answers_for_a_type_the_catalog_made() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE DOMAIN example_type AS integer")
        .unwrap();
    session
        .run("CREATE TABLE ex (id int8, number example_type)")
        .unwrap();

    // Each step of the probe, so a failure says which one moved.
    assert_eq!(
        session.rows("SELECT 'example_type'::regtype"),
        [[Some("example_type".to_owned())]],
        "a regtype over a catalog type prints its name"
    );
    assert_eq!(
        session.rows("SELECT ARRAY['example_type'::regtype]"),
        [[Some("{example_type}".to_owned())]],
        "and survives an ARRAY constructor, which is where run 97 broke"
    );
    // The oid is what `::oidvector` needs, and a catalog type has one like any other.
    let vector = session.rows("SELECT ARRAY['example_type'::regtype]::oidvector");
    assert_eq!(vector.len(), 1, "the oidvector cast must answer");
    assert!(
        vector[0][0]
            .as_deref()
            .is_some_and(|v| v.parse::<u64>().is_ok()),
        "an oidvector is digits, and {vector:?} is not"
    );

    // And the whole statement, which is what the adapter sends.
    let probe = "SELECT exists( \
         SELECT * FROM pg_proc \
         WHERE proname = 'lower' AND proargtypes = ARRAY['example_type'::regtype]::oidvector \
       ) OR exists( \
         SELECT * FROM pg_proc INNER JOIN pg_cast \
           ON ARRAY[casttarget]::oidvector = proargtypes \
         WHERE proname = 'lower' AND castsource = 'example_type'::regtype \
       )";
    assert_eq!(
        session.rows(probe),
        [[Some("f".to_owned())]],
        "a domain over integer has no lower() and no cast to one, so the probe is false"
    );
}
