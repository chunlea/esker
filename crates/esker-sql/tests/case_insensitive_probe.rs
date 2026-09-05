//! `ActiveRecord`'s `can_perform_case_insensitive_comparison_for?`, verbatim.
//!
//! Run 92's six ARRAY rows are all this one statement (`postgresql_adapter.rb:1081`), and closing
//! it took three things: an `ARRAY[…]` built per row, `regtype` modelled as an oid that prints as
//! a name ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)), and
//! `::oidvector`.
//!
//! Measured in `captures/pg19_regtype.txt`: **true** for `character varying` and for `text`, and
//! **false** for `integer`. The third is the one that matters — `lower(integer)` does not exist
//! and `integer` casts to nothing `lower` takes, so a node that answered `true` to everything
//! would pass the two cases anybody would think to check.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// The adapter's own SQL, with only the interpolated type name substituted.
fn probe(ty: &str) -> String {
    format!(
        "SELECT exists( \
           SELECT * FROM pg_proc \
           WHERE proname = 'lower' \
             AND proargtypes = ARRAY['{ty}'::regtype]::oidvector \
         ) OR exists( \
           SELECT * FROM pg_proc \
           INNER JOIN pg_cast \
             ON ARRAY[casttarget]::oidvector = proargtypes \
           WHERE proname = 'lower' \
             AND castsource = '{ty}'::regtype \
         )"
    )
}

#[test]
fn the_case_insensitivity_probe_answers_what_postgresql_answers() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    for (ty, want) in [
        ("character varying", "t"),
        ("text", "t"),
        // The control.
        ("integer", "f"),
    ] {
        let sql = probe(ty);
        assert_eq!(
            session.rows(&sql),
            [[Some(want.to_owned())]],
            "the probe for {ty} must answer {want}, as it does on 19beta1"
        );
    }
}

/// The three pieces on their own, so a failure says which one moved.
#[test]
fn the_three_pieces_the_probe_needs() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    for (sql, want) in [
        // A `regtype` is an oid: it compares with one, and with the catalog column that holds one.
        ("SELECT 'character varying'::regtype = 1043", "t"),
        ("SELECT pg_typeof('integer'::regtype)", "regtype"),
        // Digits, not names — which is what `proargtypes` holds.
        ("SELECT ARRAY['text'::regtype]::oidvector", "25"),
        // An `ARRAY[…]` over a column, built per row.
        (
            "SELECT ARRAY[casttarget]::oidvector FROM pg_cast \
             WHERE castsource = 'character varying'::regtype AND casttarget = 25",
            "25",
        ),
    ] {
        assert_eq!(session.rows(sql), [[Some(want.to_owned())]], "{sql}");
    }
}
