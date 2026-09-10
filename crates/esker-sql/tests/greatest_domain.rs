//! **`GREATEST`/`LEAST` compare, and some types have nothing to compare with** — wire v3 family
//! **F1a**, 63 rows and the largest single family in the census.
//!
//! `exec::cursor`'s `CatalogFunc::Greatest | CatalogFunc::Least` walks its arguments with
//! `Datum::pg_cmp` and **has no type check of any kind**. `pg_cmp` is a total order over every
//! `Datum`, so `GREATEST(json, json)` compares two `Datum::Text` and answers where PostgreSQL
//! raises.
//!
//! **This is not `min`/`max`'s list, and the capture is why.** Measured on 19beta1
//! (`tests/captures/pg19_min_max_greatest.txt`):
//!
//! ```text
//! min(point[])   point[]        GREATEST(point[])   42883 could not identify a comparison function
//! min(hstore)    42883 no min   GREATEST(hstore)    hstore
//! ```
//!
//! `min`/`max` refuse iff PostgreSQL has no **aggregate** for the type — a `pg_proc` lookup that
//! fails in the parser. `GREATEST`/`LEAST` refuse iff the type has no **comparison function** — a
//! `btree` opclass lookup that fails at executor init. Same `42883`, different sentence, different
//! mechanism, different domain: sharing a list between them would close one family and open
//! another.
//!
//! **The list here is measured whole, not extended one type at a time**: `GREATEST` and `LEAST`
//! were asked of all **166** type spellings the probe list has, and exactly **20** refuse — eleven
//! scalars and their arrays. Everything else answers its own type, `int2vector`, `oidvector`,
//! `hstore`, `tsvector`, `tsquery`, `money`, `macaddr`, `bit`, `citext`, `ltree`, `jsonb`, `uuid`
//! and every range included.
//!
//! **No `DETAIL` and no `HINT`**, measured — which is what tells this sentence from its two
//! neighbours in `error.rs`, `could not identify an equality operator` and `… an ordering
//! operator`, both of which carry more.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// PostgreSQL's whole sentence, which has no `DETAIL` and no `HINT`.
fn refusal(ty: &str) -> String {
    format!("!42883 could not identify a comparison function for type {ty}")
}

/// **The eleven scalars with no comparison function, and the nine arrays of them the node has.**
///
/// `lquery[]` refuses on 19beta1 too and is not here because this node has no such type; `void`
/// has no array on either server (`type "void[]" does not exist`).
#[test]
fn greatest_and_least_refuse_a_type_with_no_comparison_function() {
    let mut node = parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS ltree"]);
    for (written, named) in [
        ("'{\"a\":1}'::json", "json"),
        ("'<a/>'::xml", "xml"),
        ("'(1,1)'::point", "point"),
        ("'[(0,0),(1,1)]'::lseg", "lseg"),
        ("'((0,0),(1,1))'::box", "box"),
        ("'((0,0),(1,1))'::path", "path"),
        ("'((0,0),(1,1),(1,0))'::polygon", "polygon"),
        ("'<(0,0),1>'::circle", "circle"),
        ("'{1,-1,0}'::line", "line"),
        ("'a.*'::lquery", "lquery"),
        ("ARRAY['{\"a\":1}'::json]", "json[]"),
        ("ARRAY['<a/>'::xml]", "xml[]"),
        ("ARRAY['(1,1)'::point]", "point[]"),
        ("ARRAY['[(0,0),(1,1)]'::lseg]", "lseg[]"),
        ("ARRAY['((0,0),(1,1))'::box]", "box[]"),
        ("ARRAY['((0,0),(1,1))'::path]", "path[]"),
        ("ARRAY['((0,0),(1,1),(1,0))'::polygon]", "polygon[]"),
        ("ARRAY['<(0,0),1>'::circle]", "circle[]"),
        ("ARRAY['{1,-1,0}'::line]", "line[]"),
    ] {
        for func in ["GREATEST", "LEAST"] {
            assert_eq!(
                node.answer(&format!("SELECT {func}({written}, {written})"))
                    .to_string(),
                refusal(named),
                "{func} over {named}"
            );
        }
    }
}

/// **And the ones `min`/`max` refuses that `GREATEST` does not**, which is the pair that says the
/// two are different domains and must not share a list.
///
/// `hstore` has a comparison function and no aggregate; a range array has both. Both are answered
/// by `GREATEST` on 19beta1, so a gate copied from `min`'s list would refuse two statements a real
/// server answers.
#[test]
fn greatest_answers_what_min_refuses() {
    let mut node = parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS hstore"]);
    for (written, named) in [
        ("'a=>1'::hstore", "hstore"),
        ("ARRAY['[1,2]'::int4range]", "int4range[]"),
        ("'[2020-01-01,2020-01-02]'::tsrange", "tsrange"),
        ("true", "boolean"),
        ("'00000000-0000-0000-0000-000000000001'::uuid", "uuid"),
        ("'{\"a\":1}'::jsonb", "jsonb"),
    ] {
        for func in ["GREATEST", "LEAST"] {
            assert_eq!(
                node.rows(&format!("SELECT pg_typeof({func}({written}, {written}))")),
                vec![vec![named]],
                "{func} over {named} is answered on 19beta1"
            );
        }
    }
}

/// **A refusal that does not need a row**, which is where PostgreSQL's is: `ExecInitExprRec`, at
/// executor init, so an empty table refuses too. A gate that only fired when a value reached
/// `pg_cmp` would leave `WHERE false` answering.
#[test]
fn the_refusal_does_not_wait_for_a_row() {
    let mut node = parity::Node::new(&["CREATE TABLE gd (j json)"]);
    assert_eq!(
        node.answer("SELECT GREATEST(j, j) FROM gd WHERE false")
            .to_string(),
        refusal("json")
    );
}
