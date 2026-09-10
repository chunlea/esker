//! **Which types `min`/`max` aggregate** — one list, wrong in both directions.
//!
//! Families **F1b** and **F2** of `esker-coord/b4-wire-v3-families.md`, which are one `match` arm:
//! `exec::aggregate`'s `AggregateFunc::Min | AggregateFunc::Max`. Seventy of the wire v3 census's
//! 233 diverging rows are that arm, and sized by type name they look like eleven separate
//! problems.
//!
//! Measured on 19beta1, `tests/captures/pg19_min_max_greatest.txt`:
//!
//! ```text
//! F2  the arm refuses what PostgreSQL aggregates
//!       min(tsrange[])  min(tstzrange[])  min(int4range[])  min(daterange[])
//!       min(numrange[]) min(int8range[])  min(point[])              -> the array's own type
//!
//! F1b the arm accepts what PostgreSQL has no aggregate for
//!       min(hstore)  min(lquery)  min(void)                         -> 42883
//! ```
//!
//! **F2's mechanism is the one worth keeping.** The arm's own comment records a measurement —
//! *"`min`/`max` over a range does not exist on a real server either, measured:
//! `42883 function min(tsrange) does not exist`"* — which is true, and the **array** entries
//! beside it were never measured. PostgreSQL orders arrays of anything (`array_lt`), so
//! `min(tsrange[])` answers where `min(tsrange)` does not. A rule measured for a scalar was
//! extended to its array, and the comment above the list makes the scalar measurement look like it
//! covers both.
//!
//! **`GREATEST`/`LEAST` is not this list and must not share it** — F1a, a separate unit. Measured
//! beside these: `min(point[])` is answered and `GREATEST(point[], point[])` is refused, because
//! `min` asks whether an *aggregate* exists (`parse_func.c`, at parse time) and `GREATEST` asks
//! whether a *comparison function* does (`execExpr.c`, at executor init). Same `42883`, different
//! sentence, different mechanism.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// A table with one column of each type this file is about, and one row.
fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE EXTENSION IF NOT EXISTS hstore",
        "CREATE EXTENSION IF NOT EXISTS ltree",
        "CREATE TABLE mm ( \
             tsr tsrange[], tstzr tstzrange[], i4r int4range[], dr daterange[], \
             nr numrange[], i8r int8range[], pts point[], \
             h hstore, lq lquery, t text )",
        "INSERT INTO mm VALUES ( \
             ARRAY['[2020-01-01,2020-01-02]'::tsrange], \
             ARRAY['[2020-01-01,2020-01-02]'::tstzrange], \
             ARRAY['[1,2]'::int4range], \
             ARRAY['[2020-01-01,2020-01-02]'::daterange], \
             ARRAY['[1,2]'::numrange], \
             ARRAY['[1,2]'::int8range], \
             ARRAY['(1,2)'::point], \
             'a=>1', 'a.b', 'x' )",
    ])
}

/// **F2: an array is aggregated even when its scalar is not.**
///
/// The seven the arm refuses, each answering **its own type** — `pg_typeof(min(x))` on 19beta1 is
/// the array type itself, not the element's and not `text`.
#[test]
fn min_and_max_aggregate_an_array_of_a_type_they_refuse_alone() {
    let mut node = node();
    for (column, ty) in [
        ("tsr", "tsrange[]"),
        ("tstzr", "tstzrange[]"),
        ("i4r", "int4range[]"),
        ("dr", "daterange[]"),
        ("nr", "numrange[]"),
        ("i8r", "int8range[]"),
        ("pts", "point[]"),
    ] {
        for func in ["min", "max"] {
            assert_eq!(
                node.rows(&format!("SELECT pg_typeof({func}({column})) FROM mm")),
                vec![vec![ty]],
                "{func}({column}) is {ty} on 19beta1"
            );
        }
    }
}

/// **F2's other half, and the reason the arm looked measured**: the *scalar* stays refused.
///
/// `min(tsrange)` really is `42883 function min(tsrange) does not exist` on a real server, which
/// is what the arm's comment measured. A fix that took the whole family out would break this.
#[test]
fn min_and_max_still_refuse_the_scalar_range() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE mms (r tsrange)",
        "INSERT INTO mms VALUES ('[2020-01-01,2020-01-02]')",
    ]);
    assert_eq!(
        node.answer("SELECT min(r) FROM mms").to_string(),
        refusal("min", "tsrange")
    );
}

/// **F1b: the types the arm accepts and PostgreSQL has no aggregate for.**
///
/// `hstore` and `lquery` have a comparison function — `GREATEST(hstore)` is answered on a real
/// server — so refusing them here is about the *aggregate* and not about ordering, which is the
/// distinction F1a turns on.
///
/// **The `DETAIL` and `HINT` are PostgreSQL's own, word for word**, measured beside the primary
/// line: a refusal that matches on its first sentence and not on its explanation is a refusal a
/// client reads differently.
#[test]
fn min_and_max_refuse_the_three_with_no_aggregate() {
    let mut node = node();
    for (column, argument) in [("h", "hstore"), ("lq", "lquery")] {
        for func in ["min", "max"] {
            assert_eq!(
                node.answer(&format!("SELECT {func}({column}) FROM mm"))
                    .to_string(),
                refusal(func, argument),
                "19beta1 has no {func}({argument})"
            );
        }
    }
    // **`void` is the third, and it needs a subquery rather than a column**: a column of it is
    // `42P16 column has pseudo-type void` on both servers, so nothing that probes by column can
    // reach it.
    assert_eq!(
        node.answer("SELECT min(x) FROM (SELECT NULL::void AS x) t")
            .to_string(),
        refusal("min", "void")
    );
}

/// PostgreSQL's whole sentence for an aggregate it does not have, `DETAIL` and `HINT` included.
fn refusal(func: &str, argument: &str) -> String {
    format!(
        "!42883 function {func}({argument}) does not exist \
         DETAIL: No function of that name accepts the given argument types. \
         HINT: You might need to add explicit type casts."
    )
}
